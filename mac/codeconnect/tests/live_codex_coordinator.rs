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

impl LiveSandbox {
    fn new(tag: &str) -> LiveSandbox {
        // Keep the private tmux socket under a short /tmp path: it is a unix
        // socket too, and the macOS temp dir is long enough to matter.
        let seq = SANDBOX_SEQ.fetch_add(1, Ordering::SeqCst);
        let base = PathBuf::from(format!("/tmp/ccli.{}.{tag}.{seq}", std::process::id()));
        let home = base.join("home");
        let codex_home = base.join("codexhome");
        std::fs::create_dir_all(&home).expect("mk CODECONNECT_HOME");
        std::fs::create_dir_all(&codex_home).expect("mk CODEX_HOME");
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
            .args(["--approval-policy", "untrusted"])
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

/// The teardown assertions both live gates share: the session is destroyed, the
/// record's cleanup is durably `Complete`, no visible process references the run
/// dir, and the directory is gone.
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
