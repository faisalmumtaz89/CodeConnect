//! GATED live end-to-end integration for the **coordinator → pane → host** seam
//! (Phase 2e-2b), validated against a real `codex` (0.147).
//!
//! `live_codex_host.rs` proves the host works when something hands it a charter.
//! This proves the thing that actually ships: the **coordinator** chooses the run
//! dir, writes the charter, puts the real `internal-codex-host` in a real tmux
//! pane, and commits `ready` only after observing the host's own evidence. Three
//! claims, in order:
//!
//!   1. **`ready` means the host's evidence, and the record carries the run
//!      dir.** The coordinator's `tmux new-session` runs the real host, which
//!      execs the real `codex app-server` and the real `codex --remote` TUI
//!      against the pane's tty; the launch record reaches `Ready` only once both
//!      broker legs are bound under the run dir, and its `run_dir` field names
//!      the directory the pane's host was actually given — the field the
//!      custodian later sweeps.
//!   2. **A real codex TUI attaches through the broker, from inside the pane.**
//!      `Tui: forward` in the broker's own log, which appears only when a real
//!      codex client completed the WS-over-UDS handshake on `tui.sock` and a
//!      request off that leg was relayed upstream.
//!   3. **Teardown leaves nothing.** The session is destroyed and no visible
//!      process references the run dir: no host, no `codex app-server`, no
//!      `codex --remote`. The run directory is gone.
//!
//! Claims 1 and 2 are separate tests, and that split is not cosmetic. Committing
//! `ready` is precisely what *starts* the teardown here — the coordinator exits,
//! and a `ready` record whose coordinator is proven gone is session-fatal — so a
//! session that reached `ready` cannot be held still long enough to watch a TUI
//! work through it. The CRUX test therefore holds the coordinator at the
//! bring-up boundary instead, which keeps the launch `pending` and the session
//! alive for as long as the assertions need.
//!
//! # Who tears down here, stated exactly
//!
//! The teardown is the **custodian's**, not a `kill-session` this file issues.
//! After the coordinator commits `ready` it exits, and a `ready` record whose
//! coordinator is proven gone is session-fatal (`codex_custodian`) — so the
//! custodian destroys the session on its own within a poll or two. That is the
//! production path for "the launch's owner went away", and it is the one worth
//! watching.
//!
//! And the run dir is removed by the **host**, not by the custodian's sweep: the
//! session kill hangs the pane up, the host handles SIGHUP, and its teardown
//! removes the directory it created. The sweep is the backstop for a host that
//! never got that chance (SIGKILLed or never started) — which cannot be staged
//! against a live codex without killing the very thing under test, so it is
//! proven deterministically instead, in
//! `codex_lifecycle_integration.rs::a_sigkilled_host_leaves_the_run_dir_for_the_custodian_to_sweep`.
//! What this file asserts is that the directory is gone, and the sentence above
//! is which mechanism the code guarantees will have removed it.
//!
//! # Why `#[ignore]` AND env-gated (mirrors `live_codex_host.rs`)
//!
//! It needs `codex` and `tmux` installed and spawns real subprocesses, so a normal
//! `cargo test` must never run it. Two independent guards: `#[ignore]`, and
//! `CC_CODEX_LIVE=1`. With the flag SET, a missing `codex` is a **failure**, not a
//! skip — an operator who demanded a live run must never be handed a vacuous
//! green — and so is a `codex` that is not the native 0.147 binary the whole
//! chunk is grounded against.
//!
//! Run it deliberately:
//! ```text
//! CC_CODEX_LIVE=1 cargo test -p codeconnect --test live_codex_coordinator -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Per-sandbox sequence, so two live sandboxes can never derive the same run dir.
/// A counter rather than a timestamp: `SystemTime` is not unique across parallel
/// threads here — the collision class already fixed in the lifecycle harness.
static SANDBOX_SEQ: AtomicU32 = AtomicU32::new(0);

/// The codex series this live gate is grounded against, matching the compiled-in
/// pin `protocol::config::CODEX_PINNED_VERSIONS` that `src/codex.rs` enforces.
const LIVE_CODEX_VERSION_PREFIX: &str = "0.147.";

/// Bounded budget for `codex --version`. A binary that does not answer promptly
/// is not the standalone native CLI, and the gate must not hang on it.
const VERSION_PROBE_BUDGET: Duration = Duration::from_secs(20);

/// The most a `--version` probe may hand back. The answer is one short line, so
/// hitting this ceiling means a wrapper is streaming something else — which is
/// rejected rather than parsed, because a valid-looking prefix of a flood is
/// exactly the vacuous pass this premise check exists to prevent.
const PROBE_OUTPUT_LIMIT: u64 = 64 * 1024;

// ---------------------------------------------------------------- scaffolding

fn tmux_bin() -> Option<PathBuf> {
    for cand in [
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
        "/usr/bin/tmux",
    ] {
        if Path::new(cand).exists() {
            return Some(PathBuf::from(cand));
        }
    }
    None
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// True iff `path` is a regular FILE with at least one executable bit set.
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

/// Resolve the NATIVE codex binary: prefer `~/.local/bin/codex` (the standalone
/// native build), fall back to `which codex`. Mirrors `live_codex_host.rs`.
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

/// Whether the file at `path` is a native Mach-O executable (thin or universal)
/// rather than a `#!`-script or a `.js` wrapper. Mirrors
/// `codex::is_native_executable` — the same first-four-bytes magic check the
/// launcher applies — so this gate cannot validate against a binary
/// `codeconnect codex` would itself refuse.
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

/// Pull the version out of `codex --version`, in the same strict shape
/// `codex::parse_codex_version` accepts: exactly one non-empty line, either
/// `codex-cli <version>` or a bare `<version>`, starting with a digit.
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

/// `codex --version` under a hard budget.
///
/// The reduced twin of `live_codex_host.rs`'s probe, which carries the full
/// rationale (and a dedicated test for the descendant-holds-the-pipes case). The
/// three properties that matter are kept: the probe runs in its **own process
/// group** so a descendant holding the pipes can be reached; the read happens on
/// its own thread and the *wait* is what gets the deadline, because a `read()` on
/// a pipe cannot be given one; and hitting the output ceiling is a **failure**,
/// not an end of output.
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
                // LIMIT + 1: the extra byte exists so its arrival can be detected.
                let mut bounded = std::io::Read::take(pipe, PROBE_OUTPUT_LIMIT + 1);
                match std::io::Read::read_to_end(&mut bounded, &mut buf) {
                    Ok(_) if buf.len() as u64 > PROBE_OUTPUT_LIMIT => Err(
                        "it wrote more than a version line; that is a wrapper streaming \
                         something else, and a valid-looking prefix of a flood is not an answer"
                            .to_string(),
                    ),
                    Ok(_) => Ok(buf),
                    Err(e) => Err(format!(
                        "the read failed part-way ({e}); a partial read is no answer"
                    )),
                }
            }
            None => Ok(Vec::new()),
        };
        let _ = tx.send(outcome);
    });
    let answer = rx.recv_timeout(budget);
    // Stop the whole group before reaping: the leader is still unreaped here, so
    // its pid — and this pgid — cannot have been handed to anything else.
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

/// The gate every test opens with. `None` ⇒ skip, and only for the one honest
/// reason: the operator did not ask for a live run.
fn live_gate() -> Option<PathBuf> {
    if std::env::var("CC_CODEX_LIVE").as_deref() != Ok("1") {
        eprintln!("SKIP live_codex_coordinator: set CC_CODEX_LIVE=1 to run it");
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
        "CC_CODEX_LIVE=1 resolved {} but it is not a native executable (a `#!`-script or \
         `.js` wrapper). CodeConnect supports the standalone native codex only.",
        codex.display()
    );
    let version = match codex_version_bounded(&codex, VERSION_PROBE_BUDGET) {
        Ok(v) => v,
        Err(why) => panic!(
            "CC_CODEX_LIVE=1 resolved {} but its version could not be established: {why}. \
             A live run whose premise is unverified must FAIL.",
            codex.display()
        ),
    };
    assert!(
        version.starts_with(LIVE_CODEX_VERSION_PREFIX),
        "CC_CODEX_LIVE=1 resolved {} reporting codex {version}, but this gate is grounded \
         against {LIVE_CODEX_VERSION_PREFIX}x (the series `src/codex.rs` pins).",
        codex.display()
    );
    assert!(
        tmux_bin().is_some(),
        "CC_CODEX_LIVE=1 was set but no tmux was found; the coordinator puts the host in a \
         tmux pane, so there is nothing to test without one."
    );
    eprintln!(
        "live gate premise verified: {} is a native executable reporting codex {version}",
        codex.display()
    );
    Some(codex)
}

/// `(pid, command)` for every process whose full command line contains `tag`,
/// excluding this test process.
///
/// What it proves is precisely **no visible process references `tag`** — not
/// universal descendant absence. Fails closed: a `ps` that cannot run panics
/// rather than returning empty, because every leak assertion reads empty as proof
/// that nothing was left behind.
fn processes_referencing(tag: &str) -> Vec<(i32, String)> {
    let me = std::process::id() as i32;
    let out = Command::new("/bin/ps")
        .args(["-Axww", "-o", "pid=,command="])
        .output()
        .expect("run /bin/ps — a scan that cannot run must FAIL, never report 'nothing found'");
    assert!(out.status.success(), "/bin/ps exited {}", out.status);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, cmd) = line.trim_start().split_once(char::is_whitespace)?;
            let pid: i32 = pid.trim().parse().ok()?;
            (pid != me && cmd.contains(tag)).then(|| (pid, cmd.to_string()))
        })
        .collect()
}

/// The tolerant twin, for `Drop`, where a panic would abort the binary mid-unwind.
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

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_file(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| format!("<unreadable: {e}>"))
}

/// The isolated world one live launch runs in: a private tmux server, a private
/// `CODECONNECT_HOME`, and a fresh `CODEX_HOME`. Everything it made is torn down
/// on `Drop`, including any process still carrying the run dir in its argv — so a
/// failed assertion cannot leave a real codex session running.
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
/// they chose — which is exactly the wrong behaviour for a directory this harness
/// is about to put credentials in. `DirBuilder::create` with `mode(0700)` is
/// atomic-or-fail: the same idiom as `ShortTmpDir` in `live_codex_host.rs`.
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
        // **A unique, unguessable, atomically created private base.** The old
        // `/tmp/ccli.<pid>.<tag>.<seq>` was fully predictable from a running
        // process's pid, and `create_dir_all` would have happily adopted a
        // directory planted there in advance — under which the credential written
        // below could be read. Nanos plus the sequence make the name unguessable;
        // `create_private_dir` refuses to adopt anything that already exists.
        let seq = SANDBOX_SEQ.fetch_add(1, Ordering::SeqCst);
        let mut base = PathBuf::new();
        for attempt in 0..16 {
            let candidate = PathBuf::from(format!(
                "/tmp/ccli.{}.{tag}.{seq}.{:x}",
                std::process::id(),
                nanos()
            ));
            match create_private_dir(&candidate) {
                Ok(()) => {
                    base = candidate;
                    break;
                }
                Err(e) if attempt == 15 => {
                    panic!("could not create a private sandbox dir: {e}")
                }
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
        // Nothing here runs a turn. This is what gets the TUI past its **sign-in
        // screen**, and without it these gates prove far less than they read:
        // measured on an empty `CODEX_HOME`, a real `codex --remote` completes its
        // handshake, issues two bootstrap reads, and then parks on
        //
        //   Sign in with ChatGPT to use Codex as part of your paid plan
        //
        // for ever. `Tui: forward` is satisfied by that, and so was this file's
        // CRUX assertion — against a client that never asks for a thread. Seeded so
        // the TUI reaches `thread/start`, which is what
        // `assert_session_survives_the_thread_start` is there to watch.
        //
        // **Read-then-write, never `fs::copy`.** `copy` follows a symlink at the
        // destination and inherits the source's mode; this reads the bytes and
        // creates the destination with `create_new` at 0600, so it cannot be made
        // to write through a planted link and cannot land world-readable. The file
        // goes with the sandbox on `Drop`, which the test asserts.
        let real_auth =
            PathBuf::from(std::env::var("HOME").expect("HOME")).join(".codex/auth.json");
        let credential = std::fs::read(&real_auth).unwrap_or_else(|e| {
            panic!(
                "CC_CODEX_LIVE=1 but {} could not be read ({e}). Without it the TUI \
                 sits on its sign-in screen and never starts a thread, and a live run \
                 whose premise is unmet must FAIL rather than pass vacuously; run \
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

        // A ULID-shaped uid, unique per run.
        // Exactly 26 Crockford Base32 symbols — a WELL-FORMED ULID.
        //
        // Not cosmetic: `resolve_owned_session` validates the uid stamp's shape, so
        // a 30-character uid makes the coordinator's post-`new-session` resolve
        // refuse, which surfaces as an *indeterminate* new-session and a failed
        // launch. (Measured, after an earlier version of this line grew the uid by
        // four characters.) Hex digits are a subset of Crockford Base32, so the
        // low 48 bits of the clock plus the sequence keep it both valid and unique.
        let uid = format!(
            "01JQXV9K7B{:012X}{seq:04X}",
            (nanos() as u64) & 0xFFFF_FFFF_FFFF
        );
        assert_eq!(uid.len(), protocol::uid::UID_LEN, "the uid must be a ULID");
        // Only the nonce's FIRST sixteen characters reach the run-dir name, so the
        // varying part must lead — and it must include a process-local SEQUENCE, not
        // just a timestamp. `SystemTime` is not unique across parallel threads on
        // this platform: the lifecycle harness had two sandboxes derive identical
        // values and collide on one tmux server. A counter cannot.
        let nonce = format!(
            "{seq:04x}{:08x}{:04x}{:016x}",
            nanos() as u32,
            std::process::id() as u16,
            nanos() as u64
        );
        // The path `codex_coordinator::choose_run_dir` will derive: the uid's LAST
        // ten alphanumerics (a ULID's random half) and the nonce's first sixteen.
        // Restated here rather than imported (an integration test links the binary,
        // not a library); the record is asserted to agree, so drift fails the test.
        let kept: Vec<char> = uid.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let uid_slug: String = kept[kept.len().saturating_sub(10)..].iter().collect();
        let nonce_slug: String = nonce
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(16)
            .collect();
        let run_dir = PathBuf::from(format!("/tmp/cch.{uid_slug}.{nonce_slug}"));
        let sock = base.join("t.sock");
        LiveSandbox {
            base,
            home,
            codex_home,
            sock,
            tmux: tmux_bin().expect("tmux checked by the gate"),
            uid,
            nonce,
            run_dir,
        }
    }

    fn spawn_coordinator(&self, codex: &Path) -> Child {
        self.spawn_coordinator_opts(codex, &[])
    }

    fn spawn_coordinator_opts(&self, codex: &Path, extra: &[&str]) -> Child {
        Command::new(env!("CARGO_BIN_EXE_codeconnect"))
            .arg("internal-codex-coordinator")
            .args(["--uid", &self.uid])
            .args(["--nonce", &self.nonce])
            .args(["--custodian-nonce", "livecust"])
            .args(["--session-name", "cc-live"])
            .args(["--cwd", "/tmp"])
            .args(["--tmux-socket", self.sock.to_str().unwrap()])
            .args(["--deadline-ms", "60000"])
            .args(["--codex", codex.to_str().expect("codex path is utf-8")])
            .args(["--codex-home", self.codex_home.to_str().unwrap()])
            // The host applies no policy default; the coordinator carries these
            // four dimensions verbatim into the pane command.
            //
            // **`on-request`, because that is what the real TUI asserts.** A codex
            // 0.147 `codex --remote` sends `approvalPolicy:"on-request"` on its
            // `thread/start`, and the broker's fingerprint validator refuses any
            // present ownership value that disagrees with the launch fingerprint.
            // Launching with `untrusted` therefore produces
            //
            //   Tui: refuse->synthetic error (thread/start: fingerprint refused
            //   (Conflict): params.approvalPolicy: "on-request" but fingerprint is
            //   "untrusted")
            //
            // whereupon the TUI exits fatally and the session dies about two
            // seconds in, having never created a thread. The broker is behaving
            // exactly as designed; the fingerprint it was handed was the wrong one.
            // `assert_session_survives_the_thread_start` below is what keeps this
            // value honest — see the note there on what a passing gate used to hide.
            .args(["--approval-policy", "on-request"])
            .args(["--approvals-reviewer", "user"])
            .args(["--sandbox", "read-only"])
            .args(["--hooks-enabled", "true"])
            .args(extra)
            .env("CODECONNECT_HOME", &self.home)
            .env("CODECONNECT_TMUX", &self.tmux)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the real coordinator")
    }

    fn record_text(&self) -> Option<String> {
        std::fs::read_to_string(
            self.home
                .join("sessions")
                .join(&self.uid)
                .join("launch.json"),
        )
        .ok()
    }

    fn record_field(&self, key: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(&self.record_text()?).ok()?;
        v.get(key)?.as_str().map(|s| s.to_string())
    }

    fn has_session(&self) -> bool {
        Command::new(&self.tmux)
            .args([
                "-S",
                self.sock.to_str().unwrap(),
                "-f",
                "/dev/null",
                "has-session",
                "-t",
                "=cc-live",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Both broker legs bound under the run dir — the host's step 2 complete, and
    /// exactly the evidence the coordinator's bring-up waits for.
    fn broker_legs_bound(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        ["tui.sock", "ccd.sock"].iter().all(|leg| {
            std::fs::metadata(self.run_dir.join(leg))
                .map(|m| m.file_type().is_socket())
                .unwrap_or(false)
        })
    }

    fn tag(&self) -> &str {
        self.run_dir.to_str().expect("short /tmp path is utf-8")
    }

    /// The credential this sandbox wrote, and the base that holds it. Handed out so
    /// a test can prove they are GONE after `Drop` — a sweep nobody checks is a
    /// sweep that can silently stop happening, and this one is what keeps the
    /// operator's token out of `/tmp`.
    fn credential_paths(&self) -> (PathBuf, PathBuf) {
        (self.base.clone(), self.codex_home.join("auth.json"))
    }
}

impl LiveSandbox {
    /// A recorded `(pid, birth)` identity from the launch record.
    fn recorded_identity(&self, field: &str) -> Option<(i32, i64, i64)> {
        let v: serde_json::Value = serde_json::from_str(&self.record_text()?).ok()?;
        let c = v.get(field)?;
        Some((
            c.get("pid")?.as_i64()? as i32,
            c.get("birth")?.get("start_sec")?.as_i64()?,
            c.get("birth")?.get("start_usec")?.as_i64()?,
        ))
    }
}

impl Drop for LiveSandbox {
    fn drop(&mut self) {
        // The guardians, by VERIFIED identity. Neither carries the run dir in its
        // argv — the coordinator derives it and the custodian is told only the uid
        // — so the tag sweep below cannot reach either, and a failed assertion
        // used to leave a custodian polling a deleted record forever (observed
        // once, after a live run failed). The birth check is what keeps this from
        // signalling a recycled pid.
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
        // Kill anything still carrying this launch's run dir in its argv. The
        // warrant is narrow but real: these pids were observed with this exact
        // path moments ago. It is not race-free — a tagged process can exit and
        // have its pid recycled in the window — which is inherent to ps-based
        // cleanup and accepted rather than pretended away.
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

/// The launch arc: a real coordinator puts a real codex session in a real pane,
/// commits `ready` on the host's own evidence, and the session tears down
/// leaving nothing behind.
///
/// It deliberately asserts nothing about what the TUI *did*, because it cannot
/// hold still long enough to watch: the coordinator exits the instant `ready` is
/// committed, and the custodian tears the session down a poll later. Watching a
/// real TUI work through the broker needs a session that stays up, which is the
/// next test.
#[test]
#[ignore = "live: needs a real codex; run with CC_CODEX_LIVE=1 -- --ignored"]
fn the_coordinator_commits_ready_on_the_hosts_evidence_and_teardown_leaves_nothing() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("ready");
    let tag = sb.tag().to_string();
    let mut coord = sb.spawn_coordinator(&codex);

    // --- 1. The run dir is durable BEFORE the mutation ----------------------
    let recorded = wait_until(Duration::from_secs(20), || {
        sb.record_field("run_dir").is_some()
    });
    assert!(
        recorded,
        "the coordinator must record its run dir before new-session: {:?}",
        sb.record_text()
    );
    assert_eq!(
        sb.record_field("run_dir").as_deref(),
        sb.run_dir.to_str(),
        "the record must name the run dir the pane's host was given"
    );

    // --- 2. The REAL host comes up in the pane ------------------------------
    let legs = wait_until(Duration::from_secs(45), || sb.broker_legs_bound());
    assert!(
        legs,
        "the pane's host should have bound tui.sock + ccd.sock under {}. \
         record:\n{:?}\nappserver.stderr:\n{}\nbroker.log:\n{}",
        sb.run_dir.display(),
        sb.record_text(),
        read_file(&sb.run_dir.join("appserver.stderr.log")),
        read_file(&sb.run_dir.join("broker.log"))
    );
    println!(
        "HOST UP — both broker legs bound under {}",
        sb.run_dir.display()
    );
    println!("live session processes:");
    for (pid, cmd) in processes_referencing(&tag) {
        println!("  {pid} {cmd}");
    }

    // --- 3. `ready` is committed, and it means that evidence -----------------
    let ready = wait_until(Duration::from_secs(45), || {
        sb.record_text()
            .map(|t| t.contains("\"Ready\""))
            .unwrap_or(false)
    });
    assert!(
        ready,
        "the coordinator must commit ready once the host proved itself: {:?}",
        sb.record_text()
    );
    println!("READY — bring_up_wrapper reported Ready and the record committed it");

    // --- 4. Teardown, by the custodian --------------------------------------
    // The coordinator exits once ready is committed; a `ready` record whose
    // coordinator is proven gone is session-fatal, so the custodian destroys the
    // session itself. Nothing here issues a kill.
    // Bounded: a coordinator that does not exit must cost this budget, not the
    // suite. Its exit is expected within moments of committing `ready` — that IS
    // the coordinator's whole life — so a timeout here is a real finding.
    let exited = wait_until(Duration::from_secs(30), || {
        matches!(coord.try_wait(), Ok(Some(_)))
    });
    assert!(
        exited,
        "the coordinator must exit once it has committed ready; it is still running"
    );
    println!("coordinator exited; the custodian now owns the ready session");
    assert_torn_down_clean(&sb, &tag);
    println!(
        "PASS the_coordinator_commits_ready_on_the_hosts_evidence_and_teardown_leaves_nothing"
    );
}

/// The CRUX, with the session held still: a **real** `codex --remote` TUI,
/// running in a tmux pane the coordinator created, attaches through the broker.
///
/// `--test-bringup hang` holds the coordinator at the bring-up boundary, which
/// keeps the launch `pending` and the session alive for as long as the assertions
/// need — the previous test's `ready` path cannot do that, because committing
/// `ready` is exactly what starts the teardown. The hang is a hang, not a fake
/// readiness: nothing here reports `Ready`.
///
/// `Tui: forward` appears in the broker's own log ONLY when a real client
/// completed the WS-over-UDS handshake on `tui.sock` AND a request off that leg
/// was classified `Forward` and relayed upstream. (The forwarded request is
/// necessarily `initialize`, since the app-server rejects everything before it —
/// but the broker's Forward note is generic, so that inference is reasoning, not
/// evidence, and is not asserted.)
#[test]
#[ignore = "live: needs a real codex; run with CC_CODEX_LIVE=1 -- --ignored"]
fn a_real_codex_tui_attaches_through_the_broker_from_inside_the_pane() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("crux");
    let tag = sb.tag().to_string();
    let mut coord = sb.spawn_coordinator_opts(&codex, &["--test-bringup", "hang"]);

    let legs = wait_until(Duration::from_secs(45), || sb.broker_legs_bound());
    assert!(
        legs,
        "the pane's host should have bound both broker legs under {}. \
         appserver.stderr:\n{}\nbroker.log:\n{}",
        sb.run_dir.display(),
        read_file(&sb.run_dir.join("appserver.stderr.log")),
        read_file(&sb.run_dir.join("broker.log"))
    );

    let broker_log = sb.run_dir.join("broker.log");
    let forwarded = wait_until(Duration::from_secs(45), || {
        read_file(&broker_log).contains("Tui: forward")
    });
    assert!(
        forwarded,
        "a real codex TUI should have handshaked the broker's tui.sock from the pane. \
         broker.log:\n{}\nappserver.stderr:\n{}",
        read_file(&broker_log),
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );
    println!(
        "CRUX PASS — a real codex client forwarded through the broker from inside the pane. \
         broker.log:\n{}",
        read_file(&broker_log)
    );
    assert_session_survives_the_thread_start(&sb);
    println!("live session processes:");
    for (pid, cmd) in processes_referencing(&tag) {
        println!("  {pid} {cmd}");
    }
    let named = |marker: &str| -> Vec<i32> {
        processes_referencing(marker)
            .into_iter()
            .map(|(pid, _)| pid)
            .collect()
    };

    // "From inside the pane", proven by ANCESTRY rather than by a shared argv tag.
    //
    // Every assertion above rests on `processes_referencing`, which matches the run
    // dir anywhere in a command line — and the run dir appears in the coordinator's
    // `tmux new-session` argv too. So the tag alone cannot distinguish "a codex the
    // pane is running" from "a codex started some other way that happens to name
    // the same directory". tmux is asked who its pane's process is, and the chain
    // is walked from there: pane pid → host → the codex children.
    let pane_pid: i32 = {
        let out = Command::new(&sb.tmux)
            .args([
                "-S",
                sb.sock.to_str().unwrap(),
                "-f",
                "/dev/null",
                "list-panes",
                "-t",
                "=cc-live",
                "-F",
                "#{pane_pid}",
            ])
            .output()
            .expect("ask tmux for its pane pid");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("tmux gave no usable pane pid ({e}): {out:?}"))
    };
    let ppid_of = |pid: i32| -> Option<i32> {
        let out = Command::new("/bin/ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    };
    let descends_from_pane = |mut pid: i32| -> bool {
        for _ in 0..8 {
            if pid == pane_pid {
                return true;
            }
            match ppid_of(pid) {
                Some(parent) if parent > 1 => pid = parent,
                _ => return false,
            }
        }
        false
    };
    println!("  tmux says the pane's process is {pane_pid}");
    let host_in_pane: Vec<i32> = named("internal-codex-host")
        .into_iter()
        .filter(|pid| descends_from_pane(*pid))
        .collect();
    assert!(
        !host_in_pane.is_empty(),
        "no internal-codex-host descends from tmux's own pane pid {pane_pid} — the \
         session under test is not actually running inside the pane"
    );

    // The two real codex children are there, addressed through the run dir the
    // coordinator chose — the app-server on `as.sock`, the TUI on `tui.sock`.
    let as_pids = named(&format!("app-server --listen unix://{tag}/as.sock"));
    assert!(
        !as_pids.is_empty(),
        "the real app-server should be running under the live session"
    );
    assert!(
        as_pids.iter().all(|pid| descends_from_pane(*pid)),
        "the real app-server must descend from the pane's process: {as_pids:?}"
    );
    let tui_pids = named(&format!("--remote unix://{tag}/tui.sock"));
    assert!(
        !tui_pids.is_empty(),
        "the real codex TUI should be running under the live session"
    );
    assert!(
        tui_pids.iter().all(|pid| descends_from_pane(*pid)),
        "the real codex TUI must descend from the pane's process: {tui_pids:?}"
    );

    // The TUI must own the pane's terminal — `tpgid == pgid`.
    //
    // This is the sharpest gate in this file, and it is here because the obvious
    // change breaks it silently. The host puts the app-server in its own process
    // group so cleanup can address a recorded pgid; doing the same to the TUI
    // measurably leaves it with `pgid == its own pid` while the tty keeps
    // `tpgid == the host's group`, i.e. a BACKGROUND process group on the terminal
    // it is supposed to own. Nothing visible fails — it is not even stopped, since
    // it blocks SIGTTIN — the pane renders, the broker handshake succeeds, and
    // every other assertion in this test passes. The only casualty is the user's
    // keyboard, which keeps going to the foreground group.
    //
    // So `tpgid == pgid` is asserted directly, rather than the weaker "is it
    // stopped", which that regression sails straight through.
    std::thread::sleep(Duration::from_secs(2));
    for pid in &tui_pids {
        let out = Command::new("/bin/ps")
            .args(["-o", "pgid=,tpgid=,stat=", "-p", &pid.to_string()])
            .output()
            .expect("run /bin/ps for the TUI's terminal state");
        let text = String::from_utf8_lossy(&out.stdout);
        let fields: Vec<&str> = text.split_whitespace().collect();
        assert!(
            fields.len() >= 3,
            "could not read the TUI's process state (pid {pid}): {text:?}"
        );
        let (pgid, tpgid, stat) = (fields[0], fields[1], fields[2]);
        println!("  TUI {pid}: pgid={pgid} tpgid={tpgid} stat={stat}");
        assert_eq!(
            pgid, tpgid,
            "the real codex TUI (pid {pid}) is in a BACKGROUND process group on the pane's \
             tty (pgid {pgid}, terminal foreground {tpgid}), so the user's keystrokes go \
             somewhere else. It renders and talks to the broker either way — that is what \
             makes this worth asserting."
        );
        assert!(
            !stat.starts_with('T'),
            "the real codex TUI (pid {pid}) is STOPPED (stat {stat}) — SIGTTIN/SIGTTOU on \
             the pane's tty"
        );
    }

    // Lose the coordinator: the retained custodian owns the outcome and performs
    // the teardown — the same path as a coordinator that died mid-launch.
    unsafe {
        libc::kill(coord.id() as i32, libc::SIGKILL);
    }
    let _ = coord.wait();
    println!("coordinator SIGKILLed; the custodian now owns the launch");
    assert_torn_down_clean(&sb, &tag);
    println!("PASS a_real_codex_tui_attaches_through_the_broker_from_inside_the_pane");
}

/// The session is **still there** after the TUI has asked for its thread.
///
/// # What a passing gate used to hide
///
/// `Tui: forward` is the first thing a codex client does, and every assertion above
/// it lands within a second or two of the pane coming up. `thread/start` comes
/// later — and when the launch fingerprint disagrees with what the TUI asserts, the
/// broker refuses it, the TUI exits fatally, and the pane, the tmux server and the
/// run dir all go with it about two seconds in. Every assertion in this test would
/// still have passed, because every one of them had already run. The gate was green
/// against a session that no longer existed.
///
/// So this waits past that window and asks two questions the CRUX cannot:
///
///   1. **Is the session still alive?** A dead tmux session is the observable end
///      state of a fatal TUI exit, whatever caused it.
///   2. **Did the broker refuse a `thread/start`?** The cause, named. Asserted
///      separately from (1) because a refusal that somehow did *not* kill the
///      session is still a launch whose thread never existed — and because a bare
///      "the session died" would send the next reader hunting.
///
/// # Why it waits for the decision rather than for a clock
///
/// `thread/start` is not the first thing the TUI sends — a bootstrap census of
/// reads comes first, and how long that takes depends on the machine and on what
/// else the suite is running. So this waits for the broker to **decide** the
/// request, either way, and only then sleeps out the window in which a refusal
/// takes the session down. Sleeping a fixed interval from the CRUX instead would
/// be a guess about someone else's timing, and a green one would prove nothing.
fn assert_session_survives_the_thread_start(sb: &LiveSandbox) {
    /// How long the TUI may take to get around to asking for its thread.
    const THREAD_START_BUDGET: Duration = Duration::from_secs(45);
    /// How long after that request the session must still be standing. The failure
    /// this guards against is measured at ~2 s from the refusal; holding a real
    /// codex session open longer buys the suite no further evidence.
    const VIABILITY_WAIT: Duration = Duration::from_secs(3);

    let broker_log = || read_file(&sb.run_dir.join("broker.log"));
    let decided = wait_until(THREAD_START_BUDGET, || {
        let log = broker_log();
        log.contains("Tui: forward (ownership request: fingerprint asserted)")
            || log.contains("refuse->synthetic error (thread/start")
    });
    // Captured while the run dir still exists. A refused thread/start kills the
    // TUI, the pane, the tmux server and the run dir together, so a log read
    // *after* the wait below reports `<unreadable>` — the one moment the evidence
    // matters is the one moment it is gone.
    let at_decision = broker_log();
    assert!(
        decided,
        "the TUI never asked for a thread within {THREAD_START_BUDGET:?}, so this gate \
         cannot say whether the launch is viable. It parks like this when its \
         CODEX_HOME has no credentials. broker.log:\n{at_decision}"
    );
    std::thread::sleep(VIABILITY_WAIT);

    // The cause first: it is the sentence that explains the symptom after it.
    assert!(
        !at_decision.contains("refuse->synthetic error (thread/start"),
        "the broker refused the TUI's thread/start, so this launch's fingerprint \
         disagrees with what a real codex client asserts and no thread was ever \
         created. broker.log at the refusal:\n{at_decision}"
    );
    assert!(
        sb.has_session(),
        "the session was gone {VIABILITY_WAIT:?} after the TUI attached — it did not \
         survive its own thread/start. broker.log at the request:\n{at_decision}"
    );
    // The session lived, so the log is still readable and can be re-read — and it
    // is re-checked for a refusal, not just for the positive signal. A refusal that
    // lands DURING the survival window is exactly as fatal as one that lands before
    // it, and `at_decision` is by definition blind to it.
    let broker_log = broker_log();
    assert!(
        !broker_log.contains("refuse->synthetic error (thread/start"),
        "the broker refused a thread/start during the survival window. \
         broker.log:\n{broker_log}"
    );
    assert!(
        broker_log.contains("Tui: forward (ownership request: fingerprint asserted)"),
        "the TUI's thread/start must be FORWARDED, not merely un-refused: a launch \
         whose ownership request never reached the app-server has no thread. \
         broker.log:\n{broker_log}"
    );
    println!(
        "VIABILITY PASS — thread/start forwarded and the session is still alive \
         {VIABILITY_WAIT:?} after the TUI attached. broker.log:\n{broker_log}"
    );
}

/// The teardown assertions both live gates share: the session is destroyed, the
/// record's cleanup is durably `Complete`, no visible process references the run
/// dir, and the directory is gone.
/// **The sandbox's copy of the operator's credential is gone once it drops.**
///
/// `Drop` sweeping the base directory is what keeps a real ChatGPT token out of
/// `/tmp` after a live run, and a sweep nobody checks is one that can silently stop
/// happening — a `remove_dir_all` whose error is discarded looks identical to a
/// successful one. So the paths are captured, the sandbox is dropped, and their
/// absence is asserted. It is deliberately a test of the harness rather than of the
/// product: the harness is what handles the credential.
#[test]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
fn the_sandbox_credential_is_written_privately_and_removed_on_drop() {
    use std::os::unix::fs::PermissionsExt;
    if live_gate().is_none() {
        return;
    }
    let (base, credential) = {
        let sb = LiveSandbox::new("cred");
        let paths = sb.credential_paths();
        let (base, credential) = (&paths.0, &paths.1);
        assert!(
            credential.is_file(),
            "the credential must have been written"
        );
        let mode = std::fs::metadata(credential)
            .expect("stat the credential")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the sandbox credential is mode {mode:o}; it must be readable by this \
             account alone"
        );
        assert_private_dir(base);
        println!(
            "credential written 0600 under a 0700 base: {}",
            base.display()
        );
        paths.clone()
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

fn assert_torn_down_clean(sb: &LiveSandbox, tag: &str) {
    let torn = wait_until(Duration::from_secs(45), || {
        !sb.has_session()
            && sb
                .record_text()
                .map(|t| t.contains("\"Complete\""))
                .unwrap_or(false)
    });
    assert!(
        torn,
        "the custodian must destroy the session and mark cleanup complete: {:?}",
        sb.record_text()
    );

    // No visible process references the run dir: not the host, not the real
    // `codex app-server`, not the real `codex --remote` TUI.
    let clean = wait_until(Duration::from_secs(20), || {
        processes_referencing(tag).is_empty()
    });
    let leftover = processes_referencing(tag);
    assert!(
        clean && leftover.is_empty(),
        "processes still reference the run dir after teardown (leak): {leftover:?}"
    );
    // Removed by the HOST's own teardown on the pane's SIGHUP (see the module
    // doc); the custodian's sweep is the backstop for a host that never got that
    // chance, and is proven deterministically elsewhere.
    assert!(
        wait_until(Duration::from_secs(20), || !sb.run_dir.exists()),
        "the run dir must be gone after teardown: {}",
        sb.run_dir.display()
    );
    println!(
        "TEARDOWN PASS — session destroyed by the custodian, no leaked processes, run dir removed"
    );
}
