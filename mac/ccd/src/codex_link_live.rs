//! GATED live integration for the **ccd control link** (Phase 2e-3), against a
//! real codex 0.147: a real coordinator, a real host, a real broker, a real
//! app-server and a real `codex --remote` TUI in a real tmux pane.
//!
//! `codex_link.rs`'s scripted test proves the state machine deterministically.
//! This proves the thing that actually ships — that the machine speaks the wire
//! the broker's ccd leg really answers with. Three claims, in order:
//!
//!   1. **The ccd leg answers the handshake and its census reads.** A raw client
//!      completes `initialize` over WS-over-UDS on `ccd.sock` and gets back a real
//!      app-server result; `thread/loaded/list` — a ccd-allowlisted read — is
//!      answered rather than refused.
//!   2. **The link binds from the live stream, and its frames become facts.** The
//!      real [`crate::codex_link::run`] task, given no thread id at all, learns one
//!      from the `thread/started` the TUI broadcasts to a merely-initialized
//!      connection (A1/D1/D2), and that frame lands in the store as a normalized
//!      fact through the ordinary [`crate::state::Daemon::ingest`] path.
//!   3. **Reconnect attaches by resume.** The link's connection is dropped; a raw
//!      probe puts the RAW `thread/resume` answer on the record and proves the
//!      broker's session binding **admits** it; a fresh link re-attaches with that
//!      thread id, in the exact ordered sequence `initialize → initialized →
//!      thread/resume`.
//!
//! # What this does NOT prove, stated exactly
//!
//! **No turn runs, and that is the shape of the whole chunk.** The broker refuses
//! the TUI's `turn/start` — measured on this build as a fingerprint refusal
//! (`sandboxPolicy: sandbox value is neither string nor object`), and behind that
//! the deferred D2 head-check would refuse it too. Which layer stops it does not
//! matter here; that it is stopped does. With no turn there is no rollout, no open
//! item, and no `thread/resume` response carrying a populated `turns[]` — which is
//! precisely why [`crate::codex_link`] implements only the contract for that world
//! and fails closed at its edge. This gate proves the paths that world contains.
//!
//! So the resume here answers with the measured not-ready error, and what is
//! asserted is that the broker **admits** the request (it is not a policy refusal)
//! and that the link's own retry predicate accepts the app-server's real error text.
//! The `turns[]`-bearing branch is not skipped — it cannot be reached, and
//! `codex_link::tests::every_answer_but_the_measured_one_fails_closed` proves the
//! link refuses to guess at it.
//!
//! **2e-4 obligation.** When turns can run, this gate must be extended to cover what
//! only then becomes observable, and 2e-4's reconciliation design must be grounded
//! in it rather than assumed:
//!
//!   * whether `turns[]` is **complete** for the turns it reports, or a summary;
//!   * whether item ids **key uniquely** across a resume — D15 says they do not,
//!     inside an interrupted turn;
//!   * what a disconnect-completion actually looks like in the response, so the
//!     durable `…:pre:` call can be closed from real evidence;
//!
//!   and the assertions must reach the **store**: the recovered terminals' dedup
//!   keys and payloads, not merely a returned value.
//!
//! Claim 3 also drops the connection by **ending the link task**, which is the
//! daemon-restart shape (a new `run` starting from a known thread id), not a
//! mid-`run` EOF. The `run` loop's own reconnect — EOF, backoff, re-handshake and
//! the retryable not-ready attach — is proven deterministically in
//! `codex_link::tests::the_link_binds_reconnects_and_retries_the_measured_attach`,
//! because a live broker offers no lever to drop one leg on demand without also
//! killing what is under test.
//!
//! # Why `#[ignore]` AND env-gated (mirrors `live_codex_coordinator.rs`)
//!
//! It needs `codex` and `tmux` and spawns real subprocesses, so a normal
//! `cargo test` must never run it. Two independent guards: `#[ignore]`, and
//! `CC_CODEX_LIVE=1`. With the flag SET, a missing premise is a **failure**, never
//! a skip — an operator who demanded a live run must not be handed a vacuous green.
//!
//! Run it deliberately, after building the launcher this harness drives:
//! ```text
//! cargo build -p codeconnect
//! CC_CODEX_LIVE=1 cargo test -p ccd --bin ccd codex_link_live -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use protocol::event::SessionKey;
use serde_json::Value;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::codex_link::{ControlLink, TempDb};

/// Per-sandbox sequence, so two live sandboxes can never derive the same run dir.
static SANDBOX_SEQ: AtomicU32 = AtomicU32::new(0);

/// The codex series this gate is grounded against — the series `codeconnect`'s own
/// launcher pins.
const LIVE_CODEX_VERSION_PREFIX: &str = "0.147.";

const VERSION_PROBE_BUDGET: Duration = Duration::from_secs(20);
const PROBE_OUTPUT_LIMIT: u64 = 64 * 1024;

// ---------------------------------------------------------------- scaffolding

fn tmux_bin() -> Option<PathBuf> {
    [
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
        "/usr/bin/tmux",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

fn resolve_codex() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        let native = PathBuf::from(home).join(".local/bin/codex");
        if is_executable_file(&native) {
            return Some(native);
        }
    }
    let out = Command::new("which").arg("codex").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    is_executable_file(&path).then_some(path)
}

/// The `codeconnect` launcher this harness drives, resolved from this test
/// binary's own target directory (`target/<profile>/deps/ccd-*` → `../codeconnect`).
///
/// `CARGO_BIN_EXE_*` is only handed to integration tests of the crate that owns the
/// binary, and `ccd` is a binary crate with no library, so the live link can only be
/// exercised from a unit test here. A missing launcher is a **failure** with the
/// command that fixes it, never a skip.
fn resolve_codeconnect() -> PathBuf {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps/<test binary>");
    let bin = profile_dir.join("codeconnect");
    assert!(
        is_executable_file(&bin),
        "CC_CODEX_LIVE=1 but the launcher this harness drives is not built at {}. \
         Run `cargo build -p codeconnect` first (same CARGO_TARGET_DIR).",
        bin.display()
    );
    bin
}

fn is_native_executable(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        return false;
    }
    matches!(
        u32::from_be_bytes(magic),
        0xFEED_FACE
            | 0xFEED_FACF
            | 0xCEFA_EDFE
            | 0xCFFA_EDFE
            | 0xCAFE_BABE
            | 0xBEBA_FECA
            | 0xCAFE_BABF
            | 0xBFBA_FECA
    )
}

fn parse_codex_version(text: &str) -> Option<String> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let line = lines.next()?;
    if lines.next().is_some() {
        return None;
    }
    let version = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
        [version] => *version,
        ["codex-cli", version] => *version,
        _ => return None,
    };
    version
        .starts_with(|c: char| c.is_ascii_digit())
        .then(|| version.to_string())
}

/// `codex --version` under a hard budget, in its own process group so a descendant
/// holding the pipes can be reached. Hitting the output ceiling is a failure, not an
/// end of output.
fn codex_version_bounded(bin: &Path, budget: Duration) -> Result<String, String> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("could not run it: {e}"))?;
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = match stdout {
            Some(pipe) => {
                let mut buf = Vec::new();
                let mut bounded = std::io::Read::take(pipe, PROBE_OUTPUT_LIMIT + 1);
                match std::io::Read::read_to_end(&mut bounded, &mut buf) {
                    Ok(_) if buf.len() as u64 > PROBE_OUTPUT_LIMIT => Err(
                        "it wrote more than a version line; that is a wrapper streaming \
                         something else"
                            .to_string(),
                    ),
                    Ok(_) => Ok(buf),
                    Err(e) => Err(format!("the read failed part-way ({e})")),
                }
            }
            None => Ok(Vec::new()),
        };
        let _ = tx.send(outcome);
    });
    let answer = rx.recv_timeout(budget);
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
    let bytes = answer.map_err(|_| format!("it did not answer within {budget:?}"))??;
    parse_codex_version(&String::from_utf8_lossy(&bytes))
        .ok_or_else(|| "its output was not a single parseable version line".to_string())
}

/// The gate. `None` ⇒ skip, and only for the one honest reason: the operator did not
/// ask for a live run.
fn live_gate() -> Option<PathBuf> {
    if std::env::var("CC_CODEX_LIVE").as_deref() != Ok("1") {
        eprintln!("SKIP codex_link_live: set CC_CODEX_LIVE=1 to run it");
        return None;
    }
    let Some(codex) = resolve_codex() else {
        panic!(
            "CC_CODEX_LIVE=1 was set but no codex binary could be found. A demanded live \
             run must FAIL rather than pass vacuously."
        )
    };
    assert!(
        is_native_executable(&codex),
        "CC_CODEX_LIVE=1 resolved {} but it is not a native executable.",
        codex.display()
    );
    let version = match codex_version_bounded(&codex, VERSION_PROBE_BUDGET) {
        Ok(v) => v,
        Err(why) => panic!(
            "CC_CODEX_LIVE=1 resolved {} but its version could not be established: {why}.",
            codex.display()
        ),
    };
    assert!(
        version.starts_with(LIVE_CODEX_VERSION_PREFIX),
        "CC_CODEX_LIVE=1 resolved {} reporting codex {version}, but this gate is grounded \
         against {LIVE_CODEX_VERSION_PREFIX}x.",
        codex.display()
    );
    assert!(
        tmux_bin().is_some(),
        "CC_CODEX_LIVE=1 was set but no tmux was found."
    );
    eprintln!(
        "live gate premise verified: {} is codex {version}",
        codex.display()
    );
    Some(codex)
}

fn tagged_pids(tag: &str) -> Vec<i32> {
    let me = std::process::id() as i32;
    let Ok(out) = Command::new("/bin/ps")
        .args(["-Axww", "-o", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, cmd) = line.trim_start().split_once(char::is_whitespace)?;
            let pid: i32 = pid.trim().parse().ok()?;
            (pid != me && cmd.contains(tag)).then_some(pid)
        })
        .collect()
}

fn read_file(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| format!("<unreadable: {e}>"))
}

async fn wait_until(budget: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// -------------------------------------------------------------- the sandbox

/// One live launch's isolated world: a private tmux server, a private
/// `CODECONNECT_HOME`, and a fresh `CODEX_HOME` seeded with the operator's
/// credentials (a turn cannot run without them). Everything is torn down on `Drop`.
struct LiveSandbox {
    base: PathBuf,
    home: PathBuf,
    codex_home: PathBuf,
    sock: PathBuf,
    tmux: PathBuf,
    uid: String,
    nonce: String,
    run_dir: PathBuf,
}

/// Create `path` as a private (0700) directory that must not already exist.
///
/// `create_dir_all` succeeds on a directory somebody else made, at whatever mode
/// they chose — exactly the wrong behaviour for one this harness is about to put
/// credentials in. `DirBuilder::create` with `mode(0700)` is atomic-or-fail: the
/// same idiom as `ShortTmpDir` in `live_codex_host.rs`.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

/// Assert a path is a directory this account owns, readable by nobody else.
fn assert_private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
    assert!(meta.is_dir(), "{} is not a directory", path.display());
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(
        mode,
        0o700,
        "{} is mode {mode:o}; a directory holding credentials must be 0700",
        path.display()
    );
}

impl LiveSandbox {
    fn new(tag: &str) -> LiveSandbox {
        // **A unique, unguessable, atomically created private base.** A path derived
        // from a running process's pid is predictable, and `create_dir_all` would
        // happily adopt a directory planted there in advance — under which the
        // credential written below could be read. Nanos make the name unguessable;
        // `create_private_dir` refuses to adopt anything that already exists.
        let seq = SANDBOX_SEQ.fetch_add(1, Ordering::SeqCst);
        let mut base = PathBuf::new();
        for attempt in 0..16 {
            let candidate = PathBuf::from(format!(
                "/tmp/ccll.{}.{tag}.{seq}.{:x}",
                std::process::id(),
                nanos()
            ));
            match create_private_dir(&candidate) {
                Ok(()) => {
                    base = candidate;
                    break;
                }
                Err(e) if attempt == 15 => panic!("could not create a private sandbox dir: {e}"),
                Err(_) => continue,
            }
        }
        let home = base.join("home");
        let codex_home = base.join("codexhome");
        create_private_dir(&home).expect("mk CODECONNECT_HOME");
        create_private_dir(&codex_home).expect("mk CODEX_HOME");
        assert_private_dir(&base);
        assert_private_dir(&codex_home);

        // **The operator's credentials, written into the sandbox's own CODEX_HOME.**
        //
        // Nothing here runs a turn. This is what gets the TUI past its sign-in
        // screen: measured on an empty `CODEX_HOME`, a real `codex --remote`
        // completes its handshake, issues two bootstrap reads, and then parks on
        // "Sign in with ChatGPT" for ever — never creating a thread, so there is no
        // `thread/started` for a control link to bind to and claim 2 cannot even be
        // attempted.
        //
        // **Read-then-write, never `fs::copy`.** `copy` follows a symlink at the
        // destination and inherits the source's mode; this reads the bytes and
        // creates the destination with `create_new` at 0600, so it cannot be made to
        // write through a planted link and cannot land world-readable. The file goes
        // with the sandbox on `Drop`, which `the_sandbox_credential_...` asserts.
        let real_auth =
            PathBuf::from(std::env::var("HOME").expect("HOME")).join(".codex/auth.json");
        let credential = std::fs::read(&real_auth).unwrap_or_else(|e| {
            panic!(
                "CC_CODEX_LIVE=1 but {} could not be read ({e}). Without it the TUI sits \
                 on its sign-in screen and never starts a thread, and a live run whose \
                 premise is unmet must FAIL rather than pass vacuously; run \
                 `codex login` first.",
                real_auth.display()
            )
        });
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(codex_home.join("auth.json"))
                .expect("create the sandbox credential");
            f.write_all(&credential).expect("write the credential");
        }

        // A well-formed ULID (26 Crockford Base32 symbols): `resolve_owned_session`
        // validates the uid stamp's shape, and a malformed one makes the
        // coordinator's post-`new-session` resolve refuse.
        let uid = format!(
            "01JQXV9K7B{:012X}{seq:04X}",
            (nanos() as u64) & 0xFFFF_FFFF_FFFF
        );
        assert_eq!(uid.len(), protocol::uid::UID_LEN, "the uid must be a ULID");
        // Only the nonce's first sixteen characters reach the run-dir name, so the
        // varying part leads and includes a process-local sequence — `SystemTime` is
        // not unique across parallel threads on this platform.
        let nonce = format!(
            "{seq:04x}{:08x}{:04x}{:016x}",
            nanos() as u32,
            std::process::id() as u16,
            nanos() as u64
        );
        // The path `codex_coordinator::choose_run_dir` derives: the uid's last ten
        // alphanumerics and the nonce's first sixteen. Restated here and asserted
        // against the record, so drift fails the test.
        let kept: Vec<char> = uid.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let uid_slug: String = kept[kept.len().saturating_sub(10)..].iter().collect();
        let nonce_slug: String = nonce
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(16)
            .collect();
        let run_dir = PathBuf::from(format!("/tmp/cch.{uid_slug}.{nonce_slug}"));
        LiveSandbox {
            sock: base.join("t.sock"),
            base,
            home,
            codex_home,
            tmux: tmux_bin().expect("tmux checked by the gate"),
            uid,
            nonce,
            run_dir,
        }
    }

    /// The coordinator, held at the bring-up boundary so the session stays alive for
    /// as long as the assertions need. The hang is a hang, not a fake readiness:
    /// nothing here reports `Ready`.
    fn spawn_coordinator(&self, codex: &Path) -> Child {
        Command::new(resolve_codeconnect())
            .arg("internal-codex-coordinator")
            .args(["--uid", &self.uid])
            .args(["--nonce", &self.nonce])
            .args(["--custodian-nonce", "livelink"])
            .args(["--session-name", "cc-live"])
            .args(["--cwd", "/tmp"])
            .args(["--tmux-socket", self.sock.to_str().unwrap()])
            // The launch deadline is what the custodian enforces on a launch that
            // never reaches `ready`, and `--test-bringup hang` guarantees this one
            // never will. It has to outlast the assertions, or the session is torn
            // down mid-run by the very machinery that is working correctly.
            .args(["--deadline-ms", "900000"])
            .args(["--codex", codex.to_str().expect("codex path is utf-8")])
            .args(["--codex-home", self.codex_home.to_str().unwrap()])
            // `on-request`, not `untrusted`, and that is a **measured** choice: a
            // real 0.147 TUI's `thread/start` carries `approvalPolicy:"on-request"`,
            // so a launch fingerprint of `untrusted` makes the broker refuse it
            //
            //   Tui: refuse->synthetic error (thread/start: fingerprint refused
            //   (Conflict): params.approvalPolicy: "on-request" but fingerprint is
            //   "untrusted")
            //
            // and the TUI then exits fatally, taking the pane and the whole session
            // with it about two seconds in. No thread is ever created, so there is
            // nothing for a control link to observe. Nothing in this chunk changes
            // the coordinator's own defaults; this gate names the value that lets a
            // thread exist, and the mismatch is reported rather than papered over.
            .args(["--approval-policy", "on-request"])
            .args(["--approvals-reviewer", "user"])
            .args(["--sandbox", "read-only"])
            .args(["--hooks-enabled", "true"])
            .args(["--test-bringup", "hang"])
            .env("CODECONNECT_HOME", &self.home)
            .env("CODECONNECT_TMUX", &self.tmux)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the real coordinator")
    }

    fn ccd_sock(&self) -> PathBuf {
        self.run_dir.join("ccd.sock")
    }

    fn broker_legs_bound(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        ["tui.sock", "ccd.sock"].iter().all(|leg| {
            std::fs::metadata(self.run_dir.join(leg))
                .map(|m| m.file_type().is_socket())
                .unwrap_or(false)
        })
    }

    /// Type into the pane's TTY. The only way to drive the session: ccd may not
    /// start turns (the broker refuses `turn/start` to the ccd role by design), so
    /// a turn can only ever be attempted by the real TUI.
    fn send_keys(&self, keys: &[&str]) {
        let out = Command::new(&self.tmux)
            .args([
                "-S",
                self.sock.to_str().unwrap(),
                "-f",
                "/dev/null",
                "send-keys",
                "-t",
                "cc-live",
            ])
            .args(keys)
            .stdin(Stdio::null())
            .output()
            .expect("run tmux send-keys");
        assert!(
            out.status.success(),
            "tmux send-keys {keys:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Is the real `codex --remote` TUI up in the pane yet? The host launches it
    /// *after* both broker legs bind, so a link that connects on the legs alone can
    /// still be earlier than the thread it is there to watch.
    fn tui_running(&self) -> bool {
        !tagged_pids(&format!("--remote unix://{}/tui.sock", self.tag())).is_empty()
    }

    fn capture_pane(&self) -> String {
        let out = Command::new(&self.tmux)
            .args([
                "-S",
                self.sock.to_str().unwrap(),
                "-f",
                "/dev/null",
                "capture-pane",
                "-p",
                "-J",
                "-t",
                "cc-live",
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            Ok(o) => format!(
                "<capture exited {}: {}>",
                o.status,
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(e) => format!("<capture failed: {e}>"),
        }
    }

    /// What the host actually left in the run dir — named when an assertion needs
    /// to say why the log it wanted was not there.
    fn run_dir_listing(&self) -> String {
        match std::fs::read_dir(&self.run_dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(" "),
            Err(e) => format!("<unreadable: {e}>"),
        }
    }

    fn recorded_identity(&self, field: &str) -> Option<(i32, i64, i64)> {
        let text = std::fs::read_to_string(
            self.home
                .join("sessions")
                .join(&self.uid)
                .join("launch.json"),
        )
        .ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let c = v.get(field)?;
        Some((
            c.get("pid")?.as_i64()? as i32,
            c.get("birth")?.get("start_sec")?.as_i64()?,
            c.get("birth")?.get("start_usec")?.as_i64()?,
        ))
    }

    fn tag(&self) -> &str {
        self.run_dir.to_str().expect("short /tmp path is utf-8")
    }

    /// The base and the credential it holds, so a test can prove they are GONE
    /// after `Drop`. A sweep nobody checks is one that can silently stop happening,
    /// and this one is what keeps the operator's token out of `/tmp`.
    fn credential_paths(&self) -> (PathBuf, PathBuf) {
        (self.base.clone(), self.codex_home.join("auth.json"))
    }
}

impl Drop for LiveSandbox {
    fn drop(&mut self) {
        // The guardians, by VERIFIED identity: neither carries the run dir in its
        // argv, so the tag sweep below cannot reach them, and a failed assertion
        // would otherwise leave a custodian polling a deleted record forever.
        for field in ["coordinator", "custodian"] {
            if let Some((pid, sec, usec)) = self.recorded_identity(field) {
                let id = protocol::proc_identity::ProcessIdentity {
                    pid,
                    birth: protocol::proc_identity::BirthIdentity {
                        start_sec: sec,
                        start_usec: usec,
                    },
                };
                if protocol::proc_identity::liveness(&id)
                    == protocol::proc_identity::Liveness::Alive
                {
                    let _ = Command::new("/bin/kill")
                        .args(["-KILL", &pid.to_string()])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
        for pid in tagged_pids(self.tag()) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = Command::new(&self.tmux)
            .args(["-S", self.sock.to_str().unwrap(), "kill-server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.run_dir);
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

// ------------------------------------------------------------- a raw client

/// A raw WS-over-UDS client on the broker's ccd leg, used to read the wire
/// directly: what `initialize` answers, what a ccd-allowlisted census read returns,
/// and — the fact this chunk most needs on the record — the RAW shape of a
/// `thread/resume` response.
struct RawCcd {
    ws: tokio_tungstenite::WebSocketStream<UnixStream>,
    next_id: i64,
}

impl RawCcd {
    async fn connect(sock: &Path) -> RawCcd {
        let stream = UnixStream::connect(sock)
            .await
            .unwrap_or_else(|e| panic!("dial {}: {e}", sock.display()));
        let (ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream)
            .await
            .expect("the ccd leg's WebSocket handshake");
        RawCcd { ws, next_id: 100 }
    }

    /// Send one request and read frames until its response arrives. Every frame is
    /// printed, so `--nocapture` shows the real wire.
    async fn request(&mut self, method: &str, params: Value, budget: Duration) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = serde_json::json!({"id": id, "method": method, "params": params});
        println!("CCD->BROKER {frame}");
        self.ws
            .send(Message::Text(frame.to_string()))
            .await
            .expect("write to the ccd leg");
        let deadline = Instant::now() + budget;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|r| !r.is_zero())
                .unwrap_or_else(|| panic!("timed out waiting for the {method} response"));
            let next = tokio::time::timeout(remaining, self.ws.next())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for the {method} response"));
            let msg = match next {
                Some(Ok(m)) => m,
                Some(Err(e)) => panic!("read error before the {method} response: {e}"),
                None => panic!("the ccd leg closed before the {method} response"),
            };
            let Message::Text(text) = msg else { continue };
            println!(
                "BROKER->CCD {}",
                if text.len() > 4000 {
                    format!("{}… [{} bytes]", &text[..4000], text.len())
                } else {
                    text.clone()
                }
            );
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if v.get("id").and_then(Value::as_i64) == Some(id)
                && v.get("method").is_none()
                && (v.get("result").is_some() ^ v.get("error").is_some())
            {
                return v;
            }
        }
    }

    async fn initialize(&mut self) -> Value {
        self.request(
            "initialize",
            serde_json::json!({"clientInfo": {"name": "cc", "title": "cc", "version": "0.0.0"}}),
            Duration::from_secs(20),
        )
        .await
    }

    /// A second, merely-initialized connection that prints every server→client
    /// frame it is handed, for as long as the test runs.
    ///
    /// It is the wire itself on the record: what the link sees, printed verbatim,
    /// so a claim about a notification is evidence rather than inference. It sends
    /// nothing after its handshake, which is also the point — it proves what a
    /// *merely-initialized* ccd connection is broadcast (A1/D2).
    async fn tap(sock: &Path) -> tokio::task::JoinHandle<()> {
        let mut raw = RawCcd::connect(sock).await;
        let init = raw.initialize().await;
        assert!(init["result"].is_object(), "the tap's initialize: {init}");
        tokio::spawn(async move {
            while let Some(Ok(msg)) = raw.ws.next().await {
                if let Message::Text(text) = msg {
                    println!(
                        "TAP {}",
                        if text.len() > 2000 {
                            format!("{}… [{} bytes]", &text[..2000], text.len())
                        } else {
                            text
                        }
                    );
                }
            }
            println!("TAP closed");
        })
    }
}

// ----------------------------------------------------------- the daemon side

/// A daemon on its own database, with the live run already in the session table.
/// The [`TempDb`] goes with the test, so a live run leaves no database behind.
fn live_daemon(session: &SessionKey) -> (Arc<crate::state::Daemon>, TempDb) {
    let db = TempDb::new(&format!(
        "ccd-link-live-{}-{}",
        std::process::id(),
        protocol::time::now_unix_ms()
    ));
    let store = Arc::new(crate::store::Store::open(db.path()).unwrap());
    let now = protocol::time::now_rfc3339();
    store
        .upsert_session(&crate::store::SessionRow {
            session_uid: session.uid.clone(),
            session_id: session.name.clone(),
            tmux_session: session.name.clone(),
            tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: protocol::event::Lifecycle::Live,
            created_at: now.clone(),
            updated_at: now,
            agent: protocol::agent::AgentKind::Codex,
            codex_thread_id: None,
            codex_socket: None,
        })
        .unwrap()
        .assert_present();
    let (tail_tx, tail_rx) = tokio::sync::mpsc::unbounded_channel();
    Box::leak(Box::new(tail_rx));
    let daemon = crate::state::Daemon::new(
        protocol::config::Config::default(),
        store,
        Arc::new(crate::apns::LoggingPushSender::new()),
        crate::state::Endpoint {
            host: "test.ts.net".into(),
            port: 8787,
            tls: false,
        },
        tail_tx,
    );
    (daemon, db)
}

/// Every fact the daemon holds for this run, as `(source_event_id, seq)`.
fn recorded(daemon: &Arc<crate::state::Daemon>, uid: &str) -> Vec<(String, u64)> {
    daemon
        .store
        .events_after(uid, 0, 10_000)
        .expect("read the event log")
        .into_iter()
        .map(|e| (e.source_event_id.unwrap_or_default(), e.seq))
        .collect()
}

fn print_events(daemon: &Arc<crate::state::Daemon>, uid: &str, header: &str) {
    println!("--- {header} ---");
    for e in daemon.store.events_after(uid, 0, 10_000).unwrap() {
        println!(
            "  seq={} kind={} src={} id={:?} turn={:?} item={:?}",
            e.seq,
            e.kind.as_str(),
            e.source.as_str(),
            e.source_event_id,
            e.turn_id,
            e.item_id
        );
    }
}

// ------------------------------------------------------------------- the gate

/// **The sandbox's copy of the operator's credential is private, and gone once it
/// drops.**
///
/// `Drop` sweeping the base is what keeps a real ChatGPT token out of `/tmp` after
/// a live run, and a sweep nobody checks is one that can silently stop happening —
/// a `remove_dir_all` whose error is discarded looks exactly like a successful one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn the_sandbox_credential_is_written_privately_and_removed_on_drop() {
    use std::os::unix::fs::PermissionsExt;
    if live_gate().is_none() {
        return;
    }
    let (base, credential) = {
        let sb = LiveSandbox::new("cred");
        let paths = sb.credential_paths();
        assert!(paths.1.is_file(), "the credential must have been written");
        let mode = std::fs::metadata(&paths.1)
            .expect("stat the credential")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the sandbox credential is mode {mode:o}; it must be readable by this \
             account alone"
        );
        assert_private_dir(&paths.0);
        println!(
            "credential written 0600 under a 0700 base: {}",
            paths.0.display()
        );
        paths
    };
    assert!(
        !credential.exists(),
        "the sandbox credential survived Drop: {}",
        credential.display()
    );
    assert!(
        !base.exists(),
        "the sandbox base survived Drop: {}",
        base.display()
    );
    println!("CREDENTIAL CLEANUP PASS — {} is gone", base.display());
}

/// The whole control link, live: handshake, bind, observe, reconnect, resume.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn the_control_link_observes_a_real_codex_session_and_reattaches_by_resume() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("link");
    let mut coord = sb.spawn_coordinator(&codex);

    // --- 1. A real host, a real broker, a real app-server, a real TUI ---------
    let legs = wait_until(Duration::from_secs(60), || sb.broker_legs_bound()).await;
    assert!(
        legs,
        "the pane's host should have bound tui.sock + ccd.sock under {}.\nappserver.stderr:\n{}\nbroker.log:\n{}",
        sb.run_dir.display(),
        read_file(&sb.run_dir.join("appserver.stderr.log")),
        read_file(&sb.run_dir.join("broker.log"))
    );
    println!(
        "HOST UP — both broker legs bound under {}",
        sb.run_dir.display()
    );

    // --- 2. The real link, connected BEFORE the TUI ---------------------------
    //
    // Order matters and is the whole design of claim 2: the host binds both legs
    // and only then launches the TUI, so a link that connects the moment the legs
    // appear is a merely-initialized connection when the TUI creates its thread —
    // which is precisely who `thread/started` is broadcast to (A1/D1/D2). Nothing
    // is handed to the link: it must learn the identity off the wire.
    let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-live");
    let (daemon, _db) = live_daemon(&session);
    let uid = session.uid.clone();
    let first = tokio::spawn(crate::codex_link::run(
        Arc::clone(&daemon),
        session.clone(),
        ControlLink {
            socket: sb.ccd_sock(),
            generation: 1,
            thread_id: None,
        },
    ));

    // --- 3. CLAIM 1: the ccd leg answers the handshake and its census reads ---
    let mut raw = RawCcd::connect(&sb.ccd_sock()).await;
    let init = raw.initialize().await;
    assert!(
        init["result"].is_object() && init["result"]["userAgent"].is_string(),
        "a live app-server initialize result on the ccd leg: {init}"
    );
    println!("CLAIM 1a PASS — initialize round-tripped on the ccd leg");

    // `thread/loaded/list` is one of the four reads the ccd role is allowed —
    // proven answered rather than refused, which is what makes the census leg real.
    let loaded = raw
        .request(
            "thread/loaded/list",
            serde_json::json!({}),
            Duration::from_secs(30),
        )
        .await;
    assert!(
        loaded["result"].is_object() || loaded["result"].is_array(),
        "a ccd-allowlisted census read must be answered, not refused: {loaded}"
    );
    println!("CLAIM 1b PASS — thread/loaded/list answered on the ccd leg");

    // --- 4. CLAIM 2: the real link binds from the live stream ----------------
    assert!(
        wait_until(Duration::from_secs(60), || sb.tui_running()).await,
        "the host must launch the real codex TUI. appserver.stderr:\n{}",
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );
    println!("TUI UP — the real `codex --remote` is running in the pane");
    // The wire itself, printed for the rest of the run.
    let tap = RawCcd::tap(&sb.ccd_sock()).await;

    let bound = wait_until(Duration::from_secs(120), || {
        daemon
            .store
            .events_after(&uid, 0, 100)
            .unwrap()
            .iter()
            .any(|e| e.kind == protocol::event::EventKind::SessionStart)
    })
    .await;
    print_events(&daemon, &uid, "after the TUI started its thread");
    assert!(
        bound,
        "the link must bind from a live thread/started. pane:\n{}\nrun dir: {}\nbroker.log:\n{}",
        sb.capture_pane(),
        sb.run_dir_listing(),
        read_file(&sb.run_dir.join("broker.log"))
    );
    let thread_id = daemon
        .store
        .events_after(&uid, 0, 100)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == protocol::event::EventKind::SessionStart)
        .and_then(|e| e.payload["thread_id"].as_str().map(str::to_string))
        .expect("the SessionStart fact names the thread");
    println!("CLAIM 2 PASS — the link bound to thread {thread_id} off the live stream");
    println!("pane:\n{}", sb.capture_pane());
    println!("run dir holds: {}", sb.run_dir_listing());

    // --- 5. THE 2e-4 TRIPWIRE ------------------------------------------------
    //
    // Everything this chunk does rests on one premise: **no turn can run.** It is
    // what makes the strict contract total, and it is a premise about somebody
    // else's code (`codex-broker`'s fail-closed `turn/start` path), so it is
    // asserted rather than assumed — by making a real TUI try. The assertion is on
    // the refusal itself, not on which layer produced it: on this build the
    // fingerprint validator stops it first, and the deferred D2 head-check stands
    // behind that. Either is "no turn can run"; both breaking is the tripwire.
    //
    // A prompt is typed into the pane and submitted. The broker must refuse the
    // `turn/start` it produces. When 2e-4 lands and turns run, THIS ASSERTION
    // BREAKS — loudly, in the gate that says the contract above is still sound —
    // which is exactly the signal that the reconciliation work is now owed.
    tokio::time::sleep(Duration::from_secs(5)).await;
    sb.send_keys(&["say pong"]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);
    let refused = wait_until(Duration::from_secs(60), || {
        read_file(&sb.run_dir.join("broker.log")).contains("refuse->synthetic error (turn/start")
    })
    .await;
    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    assert!(
        refused,
        "TRIPWIRE: the broker did not refuse the TUI's turn/start within 60s. Either \
         no turn was attempted (check the pane), or turns now RUN — in which case \
         this chunk's whole premise has changed: resume responses can carry real \
         turns[], open items can survive a disconnect, and the reconciliation 2e-4 \
         owns is now owed. pane:\n{}\nbroker.log:\n{broker_log}",
        sb.capture_pane()
    );
    println!(
        "TRIPWIRE PASS — a real TUI attempted a turn and the broker refused it, so \
         the pre-2e-4 premise (no turn can run) still holds. The refusal:\n{}",
        broker_log
            .lines()
            .filter(|l| l.contains("turn/start"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // --- 6. CLAIM 3: reconnect attaches by resume ----------------------------
    let before = recorded(&daemon, &uid);
    assert!(
        !before.is_empty(),
        "the link must have recorded the live stream before the drop"
    );
    // The broker's own account of the same events: `Ccd: forward` appears only when
    // a real client completed the WS-over-UDS handshake on `ccd.sock` AND a request
    // off that leg was classified `Forward` and relayed upstream. Independent of
    // anything this test observed from the client side.
    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    assert!(
        broker_log.contains("Ccd: forward"),
        "the broker must record the ccd leg forwarding this link's requests:\n{broker_log}"
    );
    println!("broker.log:\n{broker_log}");

    // Drop the link's connection.
    first.abort();
    let _ = first.await;
    println!("link connection dropped ({} facts recorded)", before.len());

    // The RAW `thread/resume` answer, on the record. This is the wire fact this
    // chunk is built on, so it is read directly rather than inferred from what the
    // link did — and the two things it can be are told apart, because they mean
    // opposite things about the code under test.
    let resume = raw
        .request(
            "thread/resume",
            serde_json::json!({"threadId": thread_id}),
            Duration::from_secs(60),
        )
        .await;
    println!("RAW thread/resume answer: {resume}");
    let broker_refusal = resume["error"]["code"].as_i64() == Some(-32001);
    assert!(
        !broker_refusal,
        "the BROKER refused the resume, so its session binding did not learn this \
         thread from its own stream: {resume}"
    );
    // **The one answer this build accepts, required unconditionally.**
    //
    // Not "an error, or a success of some tolerated shape" — production refuses
    // every success outright (an empty `turns[]` has never been observed on this
    // wire either), so a gate that still tolerated one would be asserting a laxer
    // contract than the code implements, which is how a gate stops being evidence.
    //
    // This is also the second half of the tripwire. The first half proves no turn
    // can run; this proves the resume answer that follows from it. If either
    // changes, the wire has moved and 2e-4's reconciliation is owed.
    assert!(
        crate::codex_link::is_measured_not_ready(&resume, &thread_id),
        "STOP-AND-AMEND: thread/resume answered with something other than the \
         measured not-ready error. Production now fails closed on exactly this, so \
         either the wire has changed or a turn has run — and the reconciliation \
         2e-4 owns is what should handle it, designed against this evidence rather \
         than guessed at. Answer: {resume}"
    );
    println!(
        "CLAIM 3 (retryable path) — the live answer IS the measured not-ready error, \
         and the link retries it rather than reporting an anomaly: {}",
        resume["error"]
    );

    // A fresh link re-attaches with the thread id, exactly as a daemon coming back
    // after a restart would: it resumes, waits out a not-ready attach, and keeps
    // observing either way.
    let lines_before = read_file(&sb.run_dir.join("broker.log")).lines().count();
    let second = tokio::spawn(crate::codex_link::run(
        Arc::clone(&daemon),
        session.clone(),
        ControlLink {
            socket: sb.ccd_sock(),
            generation: 1,
            thread_id: Some(thread_id.clone()),
        },
    ));
    // Long enough for the re-attach and at least one attach retry (the link's floor
    // is 500 ms, doubling).
    tokio::time::sleep(Duration::from_secs(10)).await;
    print_events(&daemon, &uid, "after the reconnect");

    // **The specific sequence the reconnect must produce**, read off the broker's
    // own log — not a bare count, which a repeated `initialize` or three unrelated
    // reads would satisfy just as well. On the ccd leg the three dispositions are
    // distinguishable: `initialize` and the census reads are "request allowlisted",
    // `initialized` is "notification allowlisted", and `thread/resume` is the only
    // thing ccd may send that is an "ownership request", so it is the one line that
    // cannot be mistaken for anything else.
    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    println!("broker.log after the reconnect:\n{broker_log}");
    let tail: Vec<&str> = broker_log
        .lines()
        .skip(lines_before)
        .filter(|l| l.contains("Ccd: "))
        .collect();
    println!("ccd leg, after the drop: {tail:#?}");
    let position = |needle: &str| tail.iter().position(|l| l.contains(needle));
    let init = position("Ccd: forward (request allowlisted)")
        .unwrap_or_else(|| panic!("the re-attached link must send initialize: {tail:#?}"));
    let initialized = position("Ccd: forward (notification allowlisted)")
        .unwrap_or_else(|| panic!("...then initialized: {tail:#?}"));
    let resume = position("Ccd: forward (ownership request: fingerprint asserted)")
        .unwrap_or_else(|| panic!("...then thread/resume: {tail:#?}"));
    assert!(
        init < initialized && initialized < resume,
        "the reconnect must be initialize -> initialized -> thread/resume, in that \
         order; nothing may be pipelined ahead of the attach (A2): {tail:#?}"
    );
    // The disconnect itself PRECEDES this window — it is what opened it — so it is
    // asserted against the whole log rather than the tail. Without it, "the right
    // sequence after the drop" would be satisfied by a link that never dropped.
    assert!(
        broker_log.contains("Ccd: read error") || broker_log.contains("Ccd leg ended"),
        "the dropped connection must appear as a real disconnect on the ccd leg:\n{broker_log}"
    );

    // Nothing recorded before the drop was written again: a re-append would mint a
    // new `seq`, and the dedup key is what stops it.
    //
    // **Labelled honestly: this is a negative, not a positive.** The re-attached
    // link never sees a `thread/started` — a reconnect to a running thread does not
    // get one — so it stays unbound and records nothing at all. What it proves is
    // the thing that matters here: a reconnect did not re-append the facts the
    // FIRST link had already made durable. The positive form — an attach that
    // succeeds, a replay over it, and the pre-drop keys still holding their
    // original `seq` — needs a resume that returns a result, which is the 2e-4
    // obligation recorded above.
    let after = recorded(&daemon, &uid);
    for (key, seq) in &before {
        let now = after.iter().find(|(k, _)| k == key);
        assert_eq!(
            now.map(|(_, s)| *s),
            Some(*seq),
            "{key} was re-appended across the reconnect (seq moved)"
        );
    }
    let mut keys = std::collections::HashSet::new();
    for (key, _) in &after {
        assert!(!key.is_empty(), "every Codex fact carries a dedup key");
        assert!(
            keys.insert(key.clone()),
            "duplicate fact {key} after resume"
        );
    }
    println!(
        "CLAIM 3 PASS — re-attached by resume; {} facts before, {} after, none duplicated",
        before.len(),
        after.len()
    );

    second.abort();
    tap.abort();
    // Lose the coordinator: the retained custodian owns the teardown, which is the
    // same path as a coordinator that died mid-launch.
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    println!("PASS the_control_link_observes_a_real_codex_session_and_reattaches_by_resume");
}
