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

/// The A7.1 digest of the codex binary under test: the identity resolution pins,
/// which the host re-verifies immediately before each of its two execs. Computed
/// here rather than written down because these harnesses build (or copy) their
/// codex at run time.
///
/// **Derived locally, deliberately, and now that is a choice rather than the only
/// option.** Until 2e-7d nothing could pin a digest: `codeconnect codex` refused
/// before it would have spawned a coordinator, so every harness composed the
/// charter a launcher would have written. The launcher exists now and its own path
/// is gated end to end by `the_codex_command_launches_a_real_session_end_to_end`
/// (`live_codex_coordinator.rs`). This file still derives its own, because:
///
/// **the coordinator gates in this file inject test-only charter flags** —
/// `--test-bringup hang`, `--test-newsession hang`, hand-picked uid/nonce pairs
/// engineered to collide on a run dir — none of which a shipping launcher can emit,
/// by design. They drive the coordinator directly because the coordinator is their
/// subject. The launcher's path is gated separately, in the same file, against the
/// same real codex.
fn codex_sha256(path: &Path) -> String {
    protocol::hash::sha256_file(path).expect("hash the codex binary under test")
}

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
    /// This sandbox's private `TMUX_TMPDIR`.
    ///
    /// The server is addressed the way production addresses its own — by the
    /// NAME `codeconnect`, not by a path — and isolated by pointing `TMUX_TMPDIR`
    /// at this directory instead. That is what lets `codeconnect ls`, which
    /// hardcodes that name, look at the same server the coordinator created;
    /// a path-addressed sandbox server is one no shipping command can reach.
    tmux_tmpdir: PathBuf,
    tmux: PathBuf,
    uid: String,
    nonce: String,
    run_dir: PathBuf,
    /// Launches this sandbox did not name, and whose guardians `Drop` must still
    /// reach.
    ///
    /// Every gate above spawns the coordinator itself, so `uid` above *is* the
    /// launch and `Drop`'s identity sweep finds its record. The ungate gate does
    /// not: `codeconnect codex` mints its own uid, which is the whole point, so
    /// its record lives at a path this sandbox cannot predict and its coordinator
    /// — which stays on as the session's supervisor for the life of the run —
    /// would survive a failed assertion with nothing to kill it. A launcher-driven
    /// test adopts the uid it discovers, and `Drop` sweeps that record too.
    adopted: std::sync::Mutex<Vec<String>>,
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

/// A fresh `(uid, launch_nonce)` for one sandbox — unique per run, and shaped so
/// that no two sandboxes can derive the same run dir.
///
/// Split out of `LiveSandbox::new` so the collision gate can hand in an identity
/// instead of taking this one: the two constructors then differ in exactly the
/// dimension under test and in nothing else.
fn fresh_identity(seq: u32) -> (String, String) {
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
    (uid, nonce)
}

/// An identity that is **different** from `(uid, nonce)` — and from every other
/// `salt` — yet derives the **same** run dir. The many-to-one derivation, used on
/// purpose.
///
/// `choose_run_dir` names `/tmp/cch.<uid's LAST TEN alphanumerics>.<nonce's FIRST
/// SIXTEEN alphanumerics>`, and it says outright that this mapping is many-to-one.
/// So a colliding identity needs only to vary the characters the derivation throws
/// away:
///
///   * the uid is 26 alphanumerics and the name keeps indices 16..26, so 14 and 15
///     are free. They sit in the random half of the ULID, and hex digits are a
///     subset of Crockford Base32, so writing hex there keeps every property the
///     uid is checked for;
///   * the nonce is 32 hex characters and the name keeps the first sixteen, so the
///     last two are free.
///
/// Each pair gets one character that is unconditionally **different from the
/// original's** — so a contender can never accidentally reproduce the launch it is
/// colliding with — and one that carries `salt`, which is what keeps two
/// contenders on the same directory distinct from each other.
///
/// None of that is trusted on the reasoning: `colliding_with` asserts the derived
/// paths are equal and the identities are not, so a change to either derivation
/// fails the premise rather than quietly producing launches that never contend.
fn collide_on_the_run_dir(uid: &str, nonce: &str, salt: u8) -> (String, String) {
    assert!(
        salt < 16,
        "the salt is written as one hex digit, got {salt}"
    );
    let hex = std::char::from_digit(salt as u32, 16).expect("salt < 16 is a hex digit");
    /// A character guaranteed to differ from `c`, in the same (hex, and therefore
    /// Crockford) alphabet.
    fn other_than(c: char) -> char {
        if c == 'A' {
            'B'
        } else {
            'A'
        }
    }
    let mut u: Vec<char> = uid.chars().collect();
    assert_eq!(
        u.len(),
        protocol::uid::UID_LEN,
        "a colliding uid can only be derived from a well-formed ULID"
    );
    u[14] = other_than(u[14]);
    u[15] = hex.to_ascii_uppercase();
    let mut n: Vec<char> = nonce.chars().collect();
    assert!(
        n.len() >= 18,
        "a colliding nonce needs two characters past the sixteen the name keeps, got {}",
        n.len()
    );
    let last = n.len() - 1;
    n[last - 1] = other_than(n[last - 1].to_ascii_uppercase()).to_ascii_lowercase();
    n[last] = hex;
    (u.into_iter().collect(), n.into_iter().collect())
}

/// The owner marker a run dir carries: `(uid, launch_nonce)`, one per line.
///
/// `None` while the directory or its marker is not there — which for a published
/// run dir is only the window before the host's atomic publish, since A11.4 puts
/// the marker inside the directory *before* the `rename` that names it.
fn marker_owner(run_dir: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(run_dir.join("owner")).ok()?;
    let mut lines = text.lines();
    let uid = lines.next()?.to_string();
    let nonce = lines.next()?.to_string();
    Some((uid, nonce))
}

/// `(device, inode)` — a directory's IDENTITY, which its name is not.
///
/// The whole point of the collision gate is that one name can be derived by two
/// launches, so "the directory is still there" proves nothing on its own: a loser
/// that removed the winner's directory and published its own would leave a
/// perfectly good directory at that path. Only the inode says it is the *same*
/// one.
fn dir_identity(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
    (meta.dev(), meta.ino())
}

impl LiveSandbox {
    fn new(tag: &str) -> LiveSandbox {
        LiveSandbox::with_identity(tag, None)
    }

    /// A fully isolated sandbox that derives the **same run dir** as `other` — the
    /// one contended resource, and nothing else.
    ///
    /// Its `CODECONNECT_HOME`, `CODEX_HOME`, `TMUX_TMPDIR` and base directory are
    /// its own, exactly as any other sandbox's are, so the two launches share no
    /// launch record, no tmux server and no credential. What they share is one
    /// derived path, which is precisely what A11.7 asks two real hosts to race
    /// for.
    ///
    /// `salt` distinguishes CONTENDERS: a name can be derived by any number of
    /// launches, and this gate uses two of them against one directory, so the
    /// second contender must not be the first one over again.
    fn colliding_with(tag: &str, other: &LiveSandbox, salt: u8) -> LiveSandbox {
        let sb = LiveSandbox::with_identity(
            tag,
            Some(collide_on_the_run_dir(&other.uid, &other.nonce, salt)),
        );
        // The premise, CHECKED rather than assumed — see `collide_on_the_run_dir`.
        assert_ne!(
            sb.uid, other.uid,
            "two colliding launches must still be two DIFFERENT launches"
        );
        assert_ne!(
            sb.nonce, other.nonce,
            "two colliding launches must still be two DIFFERENT launches"
        );
        assert_eq!(
            sb.run_dir,
            other.run_dir,
            "the whole gate rests on both launches deriving ONE run-dir name; they \
             derived {} and {}",
            sb.run_dir.display(),
            other.run_dir.display()
        );
        assert_ne!(sb.home, other.home, "the two homes must be separate");
        assert_ne!(
            sb.tmux_tmpdir, other.tmux_tmpdir,
            "the two tmux servers must be separate, or the race is about a session \
             name as well as a directory"
        );
        assert_ne!(
            sb.codex_home, other.codex_home,
            "the two CODEX_HOMEs must be separate"
        );
        sb
    }

    fn with_identity(tag: &str, identity: Option<(String, String)>) -> LiveSandbox {
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

        let (uid, nonce) = identity.unwrap_or_else(|| fresh_identity(seq));
        assert_eq!(uid.len(), protocol::uid::UID_LEN, "the uid must be a ULID");
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
        let tmux_tmpdir = base.join("tmux");
        create_private_dir(&tmux_tmpdir).expect("mk TMUX_TMPDIR");
        LiveSandbox {
            base,
            home,
            codex_home,
            tmux_tmpdir,
            tmux: tmux_bin().expect("tmux checked by the gate"),
            uid,
            nonce,
            run_dir,
            adopted: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// A tmux command against this sandbox's own server.
    ///
    /// Named `codeconnect` — production's name — and isolated by `TMUX_TMPDIR`,
    /// so every party addresses one server: the coordinator, the supervisor it
    /// becomes, and `codeconnect ls`.
    fn tmux_cmd(&self) -> Command {
        let mut command = Command::new(&self.tmux);
        command
            .args(["-L", protocol::TMUX_SOCKET_NAME])
            .env("TMUX_TMPDIR", &self.tmux_tmpdir)
            .stdin(Stdio::null());
        command
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
            .args(["--tmux-socket", protocol::TMUX_SOCKET_NAME])
            .args(["--deadline-ms", "60000"])
            .args(["--codex", codex.to_str().expect("codex path is utf-8")])
            // A7.1: the identity the launcher pins at resolution and the host
            // re-verifies before each exec. Required — a coordinator with no
            // digest refuses rather than handing the host a bare pathname.
            .args(["--codex-sha256", &codex_sha256(codex)])
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
            .env("TMUX_TMPDIR", &self.tmux_tmpdir)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the real coordinator")
    }

    /// Hold this sandbox's tmux server open with a session no launch owns.
    ///
    /// **A fidelity fix, not a convenience.** A tmux server exits when its last
    /// session goes, and each sandbox here runs a server of its own — so a launch
    /// whose pane command exits immediately takes the whole server with it, and its
    /// coordinator's post-`new-session` bookkeeping then fails with "the tmux
    /// server … is gone or replaced" (measured, on the first run of the collision
    /// gate below). Production cannot produce that: there is ONE `codeconnect`
    /// server per user and a collision means two launches at once, so the loser's
    /// server always still holds at least the winner's session.
    ///
    /// This session restores that property without restoring what the split was
    /// for — the two servers stay separate, so the shared `cc-live` session name
    /// contends for nothing. It carries no uid stamp, so no census, resolve or
    /// `codeconnect ls` can mistake it for a launch.
    ///
    /// No `-f /dev/null`: the server this starts must be the one a coordinator
    /// would have started, user config and all.
    fn hold_the_server_open(&self) {
        let status = self
            .tmux_cmd()
            .args([
                "new-session",
                "-d",
                "-s",
                "cc-keepalive",
                "--",
                "/bin/sleep",
                "86400",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run tmux new-session for the keepalive");
        assert!(
            status.success(),
            "could not hold this sandbox's tmux server open ({status})"
        );
    }

    fn record_text(&self) -> Option<String> {
        self.record_text_for(&self.uid)
    }

    /// The launch record of an arbitrary uid under this sandbox's home — used by
    /// the ungate gate, whose uid is the launcher's rather than this sandbox's.
    fn record_text_for(&self, uid: &str) -> Option<String> {
        std::fs::read_to_string(self.home.join("sessions").join(uid).join("launch.json")).ok()
    }

    fn record_field(&self, key: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(&self.record_text()?).ok()?;
        v.get(key)?.as_str().map(|s| s.to_string())
    }

    fn has_session(&self) -> bool {
        self.tmux_cmd()
            .args(["-f", "/dev/null", "has-session", "-t", "=cc-live"])
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
    /// The launch record as JSON, or `None` while it does not exist / does not parse.
    fn record_json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.record_text()?).ok()
    }

    /// The sanitized reason a **terminally failed** launch carries. `None` for
    /// every other state, so a caller cannot mistake "still pending" for "failed
    /// with no reason".
    fn failure_reason(&self) -> Option<String> {
        self.record_json()?
            .get("state")?
            .get("Failed")?
            .get("reason")?
            .as_str()
            .map(str::to_string)
    }

    /// `NotRequired` / `Pending` / `Complete` — whether disposable-runtime cleanup
    /// is still owed on this launch.
    fn cleanup_state(&self) -> Option<String> {
        self.record_json()?
            .get("cleanup")?
            .as_str()
            .map(str::to_string)
    }

    /// Whether a real `internal-codex-host` reached this launch's D7 gate, and
    /// which process it was.
    ///
    /// **Not the lease.** `host_lease` answers "who holds this launch right now"
    /// and `to_failed` drops it, so a failed launch's lease is always `null`
    /// (measured, on the first run of the collision gate). `host_reached_gate` and
    /// `host_identity` are written on ARRIVAL, before the gate renders a verdict,
    /// and are deliberately never cleared — they are the durable record that a
    /// pane really ran a host, which is the difference between a launch that
    /// contended and a pane that never started.
    fn host_arrived(&self) -> Option<i32> {
        let record = self.record_json()?;
        record
            .get("host_reached_gate")?
            .as_bool()?
            .then(|| record.get("host_identity")?.get("pid")?.as_i64())
            .flatten()
            .map(|pid| pid as i32)
    }

    /// A recorded `(pid, birth)` identity from the launch record.
    fn recorded_identity(&self, field: &str) -> Option<(i32, i64, i64)> {
        self.recorded_identity_of(&self.uid, field)
    }

    fn recorded_identity_of(&self, uid: &str, field: &str) -> Option<(i32, i64, i64)> {
        let v: serde_json::Value = serde_json::from_str(&self.record_text_for(uid)?).ok()?;
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
        let uids: Vec<String> = std::iter::once(self.uid.clone())
            .chain(self.adopted.lock().expect("adopted uids").iter().cloned())
            .collect();
        for (uid, field) in uids
            .iter()
            .flat_map(|u| ["coordinator", "custodian"].map(|f| (u.clone(), f)))
        {
            if let Some((pid, sec, usec)) = self.recorded_identity_of(&uid, field) {
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
        let _ = self
            .tmux_cmd()
            .arg("kill-server")
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

    // --- 3b. …and what Ready CLAIMS is actually written down -----------------
    //
    // Both of these are facts the record asserts about the world, and both were
    // previously inferred rather than recorded. Asserted here, on a real launch
    // against real codex, because they are exactly the kind of claim a unit test
    // can only stage.
    let text = sb.record_text().unwrap_or_default();
    // A11.3: the premise the no-server-A cleanup escape rests on — a pane dies with
    // its command — was established on THIS session, and the coordinator wrote it
    // down between asserting it and recording server A.
    assert!(
        text.contains("\"remain_on_exit_asserted\": true"),
        "a committed Ready must carry the proof that remain-on-exit was cleared: {text}"
    );
    // A11.1, readiness half: both host children are past `execve`. The fence records
    // them BEFORE they can exec, so without this the record would name two processes
    // that had not yet become the programs it claims they are.
    assert_eq!(
        text.matches("\"exec_confirmed\": true").count(),
        2,
        "both host children must be confirmed past execve before Ready: {text}"
    );

    // --- 4. Teardown, by the custodian --------------------------------------
    //
    // **This used to begin by waiting for the coordinator to exit, and that was
    // the defect, not the design.** Committing `ready` ended the coordinator, a
    // `ready` record with no coordinator is session-fatal, and so the custodian
    // destroyed every successfully launched session about a poll later. The
    // coordinator now stays as the session's supervisor — the role the
    // custodian's rule was always written for — so `ready` is followed by a
    // session that is simply UP.
    //
    // Which means teardown has to be asked for. Ending the supervisor is how,
    // and it drives exactly the rule this test always meant to exercise: a ready
    // session whose supervisor is proven gone is torn down by the custodian,
    // with nothing else issuing a kill.
    assert!(
        matches!(coord.try_wait(), Ok(None)),
        "a ready session must outlive its own launch; the coordinator exited"
    );
    assert!(
        sb.has_session(),
        "a ready session must still be running before anything asks for a teardown"
    );
    println!("READY AND UP — the supervisor holds the session; asking for teardown now");
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    println!("supervisor lost; the custodian now owns the ready session");
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
        let out = sb
            .tmux_cmd()
            .args([
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

// ============================ THE REGISTRATION GATE ============================
//
// A9.2: until 2e-7b there was no producer of a Codex registration at all —
// `registration_frame` named Claude as a literal, no Codex launch ran a
// supervisor, and so `ccd` had nothing to admit, `codeconnect ls` rendered a
// Codex pane's uid as `—`, and every Codex row any test ever saw had been staged
// into the store by hand. This is the gate for the producer, driven end to end
// with real binaries: a real `ccd`, a real coordinator, a real codex TUI in a
// real tmux pane, and the real CLI reading it back.

/// Build `ccd` and return the binary this tree just produced.
///
/// Built rather than assumed present, for the reason `resolve_codeconnect` in
/// `ccd`'s own live harness records: `cargo test -p codeconnect` does not rebuild
/// `ccd`, and a live gate that can run against a daemon built before the change
/// under test is not a gate. `cargo build` is a no-op when nothing changed.
fn resolve_ccd() -> PathBuf {
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "-p", "ccd"])
        .current_dir(workspace_root())
        .stdin(Stdio::null())
        .status()
        .expect("run cargo build -p ccd");
    assert!(status.success(), "cargo build -p ccd failed");
    let exe = std::env::current_exe().expect("this test binary's own path");
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps/<test binary>");
    let bin = profile_dir.join("ccd");
    assert!(
        bin.is_file(),
        "ccd is not built at {} (same CARGO_TARGET_DIR?)",
        bin.display()
    );
    bin
}

/// The workspace directory `cargo build` must run in — derived from this file's
/// own location at compile time, so it does not depend on the caller's cwd.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("<workspace>/codeconnect")
        .to_path_buf()
}

impl LiveSandbox {
    /// Start a real `ccd` on this sandbox's home and wait for its IPC socket.
    ///
    /// A private `ws_port` because this daemon shares a machine with whatever
    /// the operator is already running, and two daemons on one port is a
    /// startup failure that would look like an unrelated timeout here.
    fn spawn_daemon(&self, ccd: &Path) -> Child {
        // Port 0: the kernel picks one. This daemon shares a machine with
        // whatever the operator is already running and with a second daemon this
        // test starts after the bounce, and a port collision is a startup failure
        // that arrives here as an unrelated timeout. Nothing in this gate dials
        // the WS listener; only the IPC socket is used.
        std::fs::write(self.home.join("config.json"), "{\"ws_port\": 0}")
            .expect("write the sandbox daemon config");
        let log = std::fs::File::create(self.base.join("ccd.log")).expect("ccd log");
        let child = Command::new(ccd)
            .env("CODECONNECT_HOME", &self.home)
            .env("TMUX_TMPDIR", &self.tmux_tmpdir)
            .env("CODECONNECT_TMUX", &self.tmux)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().expect("clone the log")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn the real ccd");
        // Readiness is a CONNECT, not the presence of the inode: `ccd` creates
        // the socket file before it accepts on it, and a daemon that exits
        // during startup leaves the file behind — so `exists()` reports ready
        // for a daemon that is already dead.
        assert!(
            wait_until(Duration::from_secs(30), || {
                std::os::unix::net::UnixStream::connect(self.home.join("ccd.sock")).is_ok()
            }),
            "the real daemon never accepted on its IPC socket. ccd.log:\n{}",
            read_file(&self.base.join("ccd.log"))
        );
        child
    }

    /// One IPC round trip against this sandbox's daemon.
    fn ipc(&self, frame: &protocol::ipc::ClientFrame) -> protocol::ipc::DaemonFrame {
        use std::io::{BufRead, BufReader, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(self.home.join("ccd.sock"))
            .expect("connect to the sandbox daemon");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("bound the read");
        let mut line = serde_json::to_vec(frame).expect("encode");
        line.push(b'\n');
        stream.write_all(&line).expect("write the frame");
        stream.flush().expect("flush");
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .expect("read the reply");
        serde_json::from_str(response.trim())
            .unwrap_or_else(|e| panic!("undecodable reply {response:?}: {e}"))
    }

    /// What the daemon says its fleet is — its own answer, the one every client
    /// gets, rather than a query this harness composes against the database.
    fn fleet(&self) -> Vec<protocol::event::SessionSummary> {
        match self.ipc(&protocol::ipc::ClientFrame::ListSessions) {
            protocol::ipc::DaemonFrame::Sessions { sessions } => sessions,
            other => panic!("unexpected reply to list_sessions: {other:?}"),
        }
    }

    fn this_run(&self) -> Option<protocol::event::SessionSummary> {
        self.fleet().into_iter().find(|s| s.session_uid == self.uid)
    }

    // ------------------------------------------------- the launcher-driven gate
    //
    // Every helper above is keyed by the uid THIS sandbox minted, because every
    // gate above spawns the coordinator itself. `codeconnect codex` mints its own,
    // so the ungate gate has to discover the launch instead of naming it.

    /// The single launch under this sandbox's home, as `(uid, record)`.
    ///
    /// Discovered rather than named. The assertion that there is at most one is
    /// load-bearing for every caller below: they say "the launch", and a second
    /// record would make that phrase quietly mean "an arbitrary one of them".
    fn the_launch(&self) -> Option<(String, serde_json::Value)> {
        let mut found: Vec<(String, serde_json::Value)> = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.home.join("sessions")) else {
            return None;
        };
        for entry in entries.flatten() {
            let uid = entry.file_name().to_string_lossy().into_owned();
            if let Some(text) = self.record_text_for(&uid) {
                if let Ok(v) = serde_json::from_str(&text) {
                    found.push((uid, v));
                }
            }
        }
        assert!(
            found.len() <= 1,
            "this gate runs exactly one launch; found {}: {:?}",
            found.len(),
            found.iter().map(|(u, _)| u).collect::<Vec<_>>()
        );
        found.pop()
    }

    /// Everything the launcher's detached coordinator wrote to its own log.
    ///
    /// The launcher redirects the coordinator's stdio to `logs/coordinator-<name>-
    /// <uid>.out` precisely so a launch that fails has an account of itself after
    /// the runtime dir is gone; a failure message here that could not reach it
    /// would be reporting the one thing this gate cannot see.
    fn coordinator_log(&self) -> String {
        let dir = self.home.join("logs");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return format!("<no logs at {}>", dir.display());
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("coordinator-"))
            })
            .map(|p| format!("--- {} ---\n{}", p.display(), read_file(&p)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Adopt a launcher-minted uid so `Drop` sweeps its guardians. Idempotent.
    fn adopt(&self, uid: &str) {
        let mut adopted = self.adopted.lock().expect("adopted uids");
        if !adopted.iter().any(|u| u == uid) {
            adopted.push(uid.to_string());
        }
    }

    /// The daemon's row for a discovered uid.
    fn run_of(&self, uid: &str) -> Option<protocol::event::SessionSummary> {
        self.fleet().into_iter().find(|s| s.session_uid == uid)
    }

    /// Type into the pane's tty.
    ///
    /// The only way to drive a session: the broker refuses `turn/start` to the ccd
    /// role by design, so a turn can only ever be started by the real TUI, by
    /// somebody typing. Addressed by NAME under this sandbox's `TMUX_TMPDIR`, like
    /// everything else here.
    fn send_keys(&self, session: &str, keys: &[&str]) {
        let out = self
            .tmux_cmd()
            // `=name:` and not `=name`: the anchor form tmux accepts for a SESSION
            // target is not a valid PANE target, and a pane is what send-keys and
            // capture-pane address. Measured — `-t =cc-1` fails with "can't find
            // pane", `-t =cc-1:` resolves to that session's current pane. The
            // trailing colon keeps the `=`, so a `cc-1` here can still never be
            // prefix-matched onto a `cc-10`.
            .args([
                "-f",
                "/dev/null",
                "send-keys",
                "-t",
                &format!("={session}:"),
            ])
            .args(keys)
            .output()
            .expect("run tmux send-keys");
        assert!(
            out.status.success(),
            "tmux send-keys {keys:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn capture_pane(&self, session: &str) -> String {
        match self
            .tmux_cmd()
            .args([
                "-f",
                "/dev/null",
                "capture-pane",
                "-p",
                "-J",
                "-t",
                &format!("={session}:"),
            ])
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            Ok(o) => format!(
                "<capture exited {}: {}>",
                o.status,
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(e) => format!("<capture failed: {e}>"),
        }
    }

    /// Is the real `codex --remote` TUI up in the pane yet? The host launches it
    /// only after both broker legs bind, so legs alone are earlier than a session
    /// anyone could type into.
    fn tui_running(&self, run_dir: &Path) -> bool {
        !tagged_pids(&format!(
            "--remote unix://{}/tui.sock",
            run_dir.to_str().expect("utf-8 run dir")
        ))
        .is_empty()
    }

    /// Whether a named session exists on this sandbox's tmux server.
    fn has_session_named(&self, name: &str) -> bool {
        self.tmux_cmd()
            .args(["-f", "/dev/null", "has-session", "-t", &format!("={name}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Run the real CLI against this sandbox, exactly as a person would.
    fn cli(&self, args: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
            .args(args)
            .env("CODECONNECT_HOME", &self.home)
            .env("TMUX_TMPDIR", &self.tmux_tmpdir)
            .env("CODECONNECT_TMUX", &self.tmux)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("run codeconnect {args:?}: {e}"));
        assert!(
            out.status.success(),
            "codeconnect {args:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

/// **A9.2, end to end: a real Codex launch registers itself with a real daemon,
/// the fleet shows it, and a daemon bounce neither loses it nor churns it.**
///
/// Every previous live gate here had to hold the coordinator at `--test-bringup
/// hang` to keep a session up, because committing `ready` used to *end* the
/// coordinator and a `ready` record with no coordinator is session-fatal. This
/// one commits `ready` for real and the session stays, because the coordinator
/// is now the supervisor — which is also the only reason there is a registration
/// to observe at all.
///
/// What is deliberately NOT asserted: anything about a phone. No device is
/// paired in this sandbox and none could advertise Codex if it were (`ccd`'s WS
/// capabilities still send no `supported_agents`), so a "no Codex push was
/// delivered" assertion here would pass against any build whatsoever. That
/// narrowing is held where it can fail — the `push_queue` recipient tests.
#[test]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
fn a_real_codex_launch_registers_with_the_real_daemon_and_survives_a_bounce() {
    let Some(codex) = live_gate() else { return };
    let ccd = resolve_ccd();
    let sb = LiveSandbox::new("register");
    let mut daemon = sb.spawn_daemon(&ccd);

    // The fleet is empty before the launch, so every assertion below is about a
    // row this run produced rather than one that was already lying around.
    assert!(
        sb.fleet().is_empty(),
        "a fresh sandbox daemon must start with no sessions: {:?}",
        sb.fleet()
    );

    let mut coord = sb.spawn_coordinator(&codex);
    assert!(
        wait_until(Duration::from_secs(90), || sb
            .record_field("state")
            .as_deref()
            == Some("Ready")),
        "the coordinator never committed ready. record: {:?}",
        sb.record_text()
    );

    // **The registration.** The supervisor the coordinator became introduces the
    // session; the daemon admits it because `supported_agents()` now names Codex.
    assert!(
        wait_until(Duration::from_secs(30), || sb.this_run().is_some()),
        "a ready Codex launch never appeared in the daemon's fleet. ccd.log:\n{}",
        read_file(&sb.base.join("ccd.log"))
    );
    let row = sb.this_run().expect("the row");
    assert_eq!(
        row.agent,
        protocol::agent::AgentKind::Codex,
        "the run must be filed as the agent it is"
    );
    assert_eq!(row.session_id, "cc-live");
    assert_eq!(row.tmux_session, "cc-live");
    assert_eq!(
        row.lifecycle,
        protocol::event::Lifecycle::Live,
        "a running session must be live in the fleet"
    );
    // The CANONICAL cwd, which on this platform is not the one the coordinator
    // was given: `/tmp` is a symlink to `/private/tmp`, and the registration
    // carries the resolved spelling every other record of this run uses.
    assert_eq!(
        row.cwd,
        std::fs::canonicalize("/tmp")
            .expect("canonical /tmp")
            .to_string_lossy(),
        "the fleet must record the canonical launch cwd"
    );

    // **The thread, adopted off the live wire rather than claimed by the
    // launch.** The registration carries no thread id — the launcher never
    // learns one — so a thread appearing here is one the daemon's control link
    // bound by observing the broker's own `thread/started`.
    assert!(
        wait_until(Duration::from_secs(90), || sb
            .this_run()
            .and_then(|s| s.codex_thread_id)
            .is_some()),
        "the daemon never adopted a thread for the registered session. ccd.log:\n{}",
        read_file(&sb.base.join("ccd.log"))
    );
    let thread = sb
        .this_run()
        .and_then(|s| s.codex_thread_id)
        .expect("an adopted thread");
    assert!(
        !thread.trim().is_empty(),
        "an adopted thread id must not be blank"
    );

    // **On the ccd leg, and provably not the TUI's.** The registration names a
    // socket, and which socket it names is what decides the ROLE the broker gives
    // the daemon: the legs are two different allowlists, and the TUI's is the one
    // that may assert ownership. Nothing downstream would notice the difference —
    // a link on the TUI leg still hears `thread/started` and still adopts a
    // thread, so every assertion above passes either way (measured, by pointing
    // the registration at `tui.sock`). The broker's own log is what tells the two
    // apart, because it names the role it opened each leg as.
    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    // `forward`, not `leg opened`: the coordinator's own bring-up probe connects
    // to both legs and hangs up, so a leg being OPENED says only that the broker
    // was listening. A frame FORWARDED under the Ccd role is the daemon's link
    // and can be nothing else. (Measured: with the registration pointed at
    // `tui.sock`, `Ccd: leg opened` is still there — from the probe — and every
    // other assertion in this test still passes.)
    assert!(
        broker_log.contains("Ccd: forward"),
        "the daemon must reach the broker on the CCD leg — the role that cannot assert \
         ownership — and be seen doing it. broker.log:\n{broker_log}"
    );

    // **The day-1 rendering defect.** `codeconnect ls` asks tmux what is running
    // and the daemon who it is; with no row the uid column rendered `—` for
    // every Codex session ever launched. The row exists now, and the same
    // command that showed the defect is what proves it gone.
    let ls = sb.cli(&["ls"]);
    let line = ls
        .lines()
        .find(|l| l.starts_with("cc-live"))
        .unwrap_or_else(|| panic!("`codeconnect ls` did not list the running session:\n{ls}"));
    assert!(
        line.contains(&sb.uid),
        "`codeconnect ls` must show the run's uid, not a dash:\n{ls}"
    );
    assert!(
        !line.contains('—'),
        "`codeconnect ls` still renders an unknown column for a registered session:\n{ls}"
    );
    assert!(
        line.contains("/private/tmp") || line.contains("/tmp"),
        "`codeconnect ls` must show the session's cwd:\n{ls}"
    );

    // And the daemon-only view agrees with it.
    let sessions = sb.cli(&["sessions", "list"]);
    assert!(
        sessions.contains(&sb.uid) && sessions.contains("live"),
        "`codeconnect sessions` must list the registered run as live:\n{sessions}"
    );

    // **The bounce.** The daemon goes away under a live session and comes back.
    // The supervisor is in its reconnect loop the whole time; what must NOT
    // happen is a second identity, a rewritten history, or a session torn down
    // because its daemon blinked.
    let created_at = row.created_at.clone();
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert!(
        sb.has_session(),
        "killing ccd must not touch the session: the daemon never owned it"
    );
    let mut daemon = sb.spawn_daemon(&ccd);
    // **Waited for on NEW-registration evidence, not on `lifecycle == Live`.**
    //
    // `Live` is a durable column: it was `Live` before the bounce and the fresh
    // daemon reads it straight back off disk, so a wait on it is satisfied by the
    // daemon merely having started — with the supervisor's reconnect broken, every
    // identity assertion below would still pass, and so would the thread assertion,
    // for the wrong reason.
    //
    // `link` is the supervisor's own IPC presence (`ccd::state`, `Link::Attached`
    // requires a live supervisor handle *and* a heartbeat since), and it is
    // memory-only: a fresh daemon has no supervisors at all, so nothing but a real
    // re-registration on a real connection can make it `Attached`. That is the
    // edge this gate is about.
    assert!(
        wait_until(Duration::from_secs(60), || sb
            .this_run()
            .map(|s| s.link == protocol::event::Link::Attached)
            .unwrap_or(false)),
        "the supervisor did not re-register the session after the daemon came back. ccd.log:\n{}",
        read_file(&sb.base.join("ccd.log"))
    );
    let after = sb.this_run().expect("the row after the bounce");
    assert_eq!(
        after.lifecycle,
        protocol::event::Lifecycle::Live,
        "and the re-registered run must still be live"
    );
    assert_eq!(
        after.session_uid, sb.uid,
        "a re-registration must not mint a second identity"
    );
    assert_eq!(
        after.created_at, created_at,
        "a re-registration must not rewrite when the run started"
    );
    assert_eq!(
        after.agent,
        protocol::agent::AgentKind::Codex,
        "a re-registration must not change the run's agent"
    );
    // Compared, not printed. The success line below claims cwd survived the
    // bounce, and until now nothing checked it — the only cwd comparison was
    // before the daemon went away.
    assert_eq!(
        after.cwd, row.cwd,
        "a re-registration must not move the run's directory"
    );
    assert_eq!(after.session_id, row.session_id);
    assert_eq!(after.tmux_session, row.tmux_session);
    assert_eq!(
        sb.fleet().len(),
        1,
        "the bounce must leave exactly one row for one session: {:?}",
        sb.fleet()
    );
    // **THE THREAD SURVIVES THE BOUNCE, and this is where the residual closed.**
    //
    // It used to be pinned here as a boundary: the link learned the thread by
    // watching `thread/started`, which the app-server broadcasts once, to the
    // connections it has at that instant — so a fresh link on a fresh daemon
    // attached, initialized, and had nothing to resume. The registration carries
    // no thread id and this side has no honest way to produce one, so nothing
    // downstream could repair it.
    //
    // The repair is in the broker, which is the one process that still holds the
    // fact: it keeps the `thread/started` it forwarded and, when a `ccd` leg
    // subscribes afterwards, replays those exact bytes — but only when the thread
    // they name is the binding this broker itself verified against the launch cwd
    // (`codex_broker::relay::deliver_head`). The reconnecting link is
    // therefore handed the same frame an early one would have received.
    //
    // Note what this is NOT: it is not durable evidence. It lives as long as the
    // broker does, which is as long as the session does, and that is exactly the
    // lifetime the question has — a session whose host is gone has no thread to be
    // on. The alternative that WAS refuted stays refuted and must not be reached
    // for: seeding a replacement from the event log was MEASURED wrong in 2e-4c
    // (the log's newest `SessionStart` can name a thread the link demonstrably
    // could not read, and the log has no representation of the fallback that
    // rescued it).
    //
    // **This is also the strongest re-registration evidence this gate has.** A
    // thread id can only reappear here if a new registration was accepted, built a
    // control link, reached the broker's ccd leg, and bound — four things, none of
    // which survives a broken reconnect.
    assert!(
        wait_until(Duration::from_secs(60), || sb
            .this_run()
            .and_then(|s| s.codex_thread_id)
            .is_some()),
        "the thread did not come back across the daemon restart. ccd.log:\n{}\nbroker.log:\n{}",
        read_file(&sb.base.join("ccd.log")),
        read_file(&sb.run_dir.join("broker.log"))
    );
    assert_eq!(
        sb.this_run().and_then(|s| s.codex_thread_id),
        Some(thread.clone()),
        "the replayed announcement must name the SAME thread the session was on \
         before the bounce, not some other one"
    );
    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    assert!(
        broker_log.contains("replayed thread/started"),
        "and it must have come from the broker's replay rather than from a second \
         live announcement this test cannot tell apart. broker.log:\n{broker_log}"
    );

    println!(
        "REGISTRATION PASS — uid {} filed as codex on thread {thread}, rendered by `ls` and \
         `sessions`, identity/agent/cwd/created_at unchanged across a daemon bounce, the \
         supervisor re-attached, and the thread re-bound from the broker's replayed \
         announcement",
        sb.uid
    );

    // Teardown: the supervisor is what holds the session, so ending it is what
    // ends the run (proven deterministically in `codex_lifecycle_integration`).
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .status();
    let _ = coord.wait();
    let _ = daemon.kill();
    let _ = daemon.wait();
}

// ============================== THE UNGATE GATE ==============================
//
// 2e-7d. Every gate above spawns `internal-codex-coordinator` itself, because for
// the whole of Phase 2 there was no launcher to spawn it: `codeconnect codex`
// resolved, version-pinned, argv-validated, preflighted — and then refused. So the
// charter those gates hand the coordinator is one the harness composed, including
// the `--codex-sha256` each of them derives locally, standing in for a producer
// that did not exist.
//
// This is that producer's gate. It types the command a person types, and asserts
// the launch it produces is the same launch every gate above proved out: one that
// registers, renders, adopts a thread on the ccd leg, runs a turn, survives a
// daemon bounce, and leaves nothing behind when it ends.

/// **THE UNGATE: `codeconnect codex`, run for real, launches a real Codex session.**
///
/// The command is driven under a pty (`script`), because on `Ready` it does what
/// `codeconnect claude` does — `exec`s into `tmux attach-session` — and a launcher
/// that only *claims* to attach would pass a test that never gave it a terminal.
/// The attached client is asserted for the same reason.
///
/// **What this gate adds over the coordinator gate above** is precisely the wiring
/// 2e-7d built, and each of these is a thing the harness used to do FOR the
/// launcher: the uid is minted by the command (so it is discovered here, not
/// named), the session name comes from the same `cc-N` sequence `codeconnect
/// claude` draws from (not the coordinator's `cc-codex` fallback), the cwd is the
/// directory the command was run in, and the digest on the charter is the one
/// resolution pinned rather than one this file hashed.
///
/// **Deliberately NOT asserted here, for the reason the gate above states:**
/// anything about a phone. No device is paired in this sandbox and none could
/// advertise Codex if it were, so a "no Codex push was delivered" assertion would
/// pass against any build whatsoever. That narrowing stays where it can fail — the
/// `push_queue` recipient tests.
#[test]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
fn the_codex_command_launches_a_real_session_end_to_end() {
    let Some(codex) = live_gate() else { return };
    let ccd = resolve_ccd();
    let sb = LiveSandbox::new("ungate");
    let mut daemon = sb.spawn_daemon(&ccd);
    assert!(
        sb.fleet().is_empty(),
        "a fresh sandbox daemon must start with no sessions: {:?}",
        sb.fleet()
    );
    assert!(
        sb.the_launch().is_none(),
        "and with no launch on disk, or the discovery below finds someone else's"
    );

    // **The cwd is a real directory the command is run IN**, not a flag it is
    // handed — which is the whole difference between this and every gate above,
    // where `--cwd /tmp` was written into a charter by hand.
    let cwd = sb.base.join("work");
    create_private_dir(&cwd).expect("mk the launch cwd");
    let canonical_cwd = std::fs::canonicalize(&cwd)
        .expect("canonicalize the launch cwd")
        .to_string_lossy()
        .into_owned();

    // **THE COMMAND.** `script -q /dev/null` gives it a controlling terminal, so
    // the `exec tmux attach-session` it ends with is a real attach. stdin is a
    // pipe this test holds OPEN: an attached tmux client that reads EOF detaches,
    // and the client staying is the evidence.
    let out_path = sb.base.join("codex-command.out");
    let out = std::fs::File::create(&out_path).expect("the command's output file");
    let mut command = Command::new("/usr/bin/script")
        .args([
            "-q",
            "/dev/null",
            env!("CARGO_BIN_EXE_codeconnect"),
            "codex",
        ])
        .current_dir(&cwd)
        .env("CODECONNECT_HOME", &sb.home)
        .env("CODEX_HOME", &sb.codex_home)
        .env("CODECONNECT_CODEX_BIN", &codex)
        .env("TMUX_TMPDIR", &sb.tmux_tmpdir)
        .env("CODECONNECT_TMUX", &sb.tmux)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(out.try_clone().expect("clone the output file")))
        .stderr(Stdio::from(out))
        .spawn()
        .expect("run `codeconnect codex`");
    let _held_stdin = command.stdin.take().expect("hold the pty's stdin open");

    // **The launch the command minted.** Discovered, because the launcher owns the
    // identity now — and adopted immediately, so a failure below still gets its
    // coordinator killed (it stays on as the session's supervisor otherwise).
    assert!(
        wait_until(Duration::from_secs(30), || sb.the_launch().is_some()),
        "`codeconnect codex` never wrote a launch record. output:\n{}",
        read_file(&out_path)
    );
    let (uid, _) = sb.the_launch().expect("the launch");
    sb.adopt(&uid);
    assert_eq!(
        uid.len(),
        protocol::uid::UID_LEN,
        "the launcher must mint a ULID uid, got {uid:?}"
    );

    assert!(
        wait_until(Duration::from_secs(120), || {
            sb.the_launch()
                .and_then(|(_, r)| r.get("state")?.as_str().map(str::to_string))
                .as_deref()
                == Some("Ready")
        }),
        "the launch never reached Ready. record: {:?}\noutput:\n{}\ncoordinator log:\n{}",
        sb.the_launch().map(|(_, r)| r.to_string()),
        read_file(&out_path),
        sb.coordinator_log(),
    );
    let (_, record) = sb.the_launch().expect("the ready launch");
    let session_name = record
        .get("session_name")
        .and_then(|v| v.as_str())
        .expect("the record names the session")
        .to_string();
    let run_dir = PathBuf::from(
        record
            .get("run_dir")
            .and_then(|v| v.as_str())
            .expect("a ready launch has a run dir"),
    );

    // **The name came from the shared `cc-N` sequence**, not the coordinator's own
    // `cc-codex` fallback — so `ls`, `attach` and a second Claude launch all see
    // one namespace on one server.
    let n = session_name
        .strip_prefix(protocol::SESSION_PREFIX)
        .unwrap_or_else(|| panic!("the session must be named cc-N, got {session_name:?}"));
    assert!(
        n.parse::<u32>().is_ok(),
        "the session must be named cc-N, got {session_name:?}"
    );
    assert!(
        sb.has_session_named(&session_name),
        "a Ready launch must have a live tmux session called {session_name}"
    );

    // **It attached.** The command's last act is `exec tmux attach-session`, and a
    // client on this session is the only thing that proves it rather than merely
    // having exited quietly.
    assert!(
        wait_until(Duration::from_secs(30), || {
            let clients = sb
                .tmux_cmd()
                .args([
                    "-f",
                    "/dev/null",
                    "list-clients",
                    "-t",
                    &format!("={session_name}"),
                ])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            !clients.is_empty()
        }),
        "`codeconnect codex` did not attach the terminal to {session_name}. output:\n{}",
        read_file(&out_path)
    );

    // **The fleet.** Same assertions as the coordinator gate, about a launch the
    // command produced.
    assert!(
        wait_until(Duration::from_secs(60), || sb.run_of(&uid).is_some()),
        "the launched session never appeared in the daemon's fleet. ccd.log:\n{}",
        read_file(&sb.base.join("ccd.log"))
    );
    let row = sb.run_of(&uid).expect("the row");
    assert_eq!(
        row.agent,
        protocol::agent::AgentKind::Codex,
        "the run must be filed as the agent it is"
    );
    assert_eq!(row.session_id, session_name);
    assert_eq!(row.tmux_session, session_name);
    assert_eq!(row.lifecycle, protocol::event::Lifecycle::Live);
    // **The cwd the command was run in, canonicalized once by the coordinator.**
    assert_eq!(
        row.cwd, canonical_cwd,
        "the fleet must record the canonical directory the command was run in"
    );

    // The thread, adopted off the wire (the launch never learns one).
    assert!(
        wait_until(Duration::from_secs(120), || sb
            .run_of(&uid)
            .and_then(|s| s.codex_thread_id)
            .is_some()),
        "the daemon never adopted a thread for the launched session. ccd.log:\n{}\nbroker.log:\n{}",
        read_file(&sb.base.join("ccd.log")),
        read_file(&run_dir.join("broker.log"))
    );
    let thread = sb
        .run_of(&uid)
        .and_then(|s| s.codex_thread_id)
        .expect("an adopted thread");

    // On the ccd leg, and provably not the TUI's — see the coordinator gate for
    // why `forward` and not `leg opened`.
    let broker_log = read_file(&run_dir.join("broker.log"));
    assert!(
        broker_log.contains("Ccd: forward"),
        "the daemon must reach the broker on the CCD leg. broker.log:\n{broker_log}"
    );

    // **`codeconnect ls`, which is what a person types next.**
    let ls = sb.cli(&["ls"]);
    let line = ls
        .lines()
        .find(|l| l.starts_with(&session_name))
        .unwrap_or_else(|| panic!("`codeconnect ls` did not list {session_name}:\n{ls}"));
    assert!(
        line.contains(&uid),
        "`ls` must show the launched run's uid:\n{ls}"
    );
    assert!(
        !line.contains('—'),
        "`ls` must not render an unknown column for a registered session:\n{ls}"
    );
    assert!(
        line.contains(&canonical_cwd),
        "`ls` must show the directory the command was run in:\n{ls}"
    );
    let sessions = sb.cli(&["sessions", "list"]);
    assert!(
        sessions.contains(&uid) && sessions.contains("live"),
        "`codeconnect sessions list` must list the launched run as live:\n{sessions}"
    );

    // **The phone's session list carries it, and that is the same answer.**
    //
    // Asserted here rather than over a WebSocket, because it is not a second
    // projection: `ClientMessage::Sessions` (ws_server.rs) and
    // `ClientFrame::ListSessions` (ipc_server.rs) both call one `Daemon::sessions()`
    // and both send the same `Vec<SessionSummary>`; the WS arm adds a serialization
    // and an error code and nothing else. So `fleet()` above IS the list a paired
    // phone would be sent, and dialling a socket to re-read it would prove the
    // serializer, not the fleet. (What IS genuinely WS-only is the event STREAM —
    // `Subscribe` has no IPC counterpart — which is why the turn below is observed
    // at the broker and the pane rather than claimed for the phone.)
    let phone_view = sb.run_of(&uid).expect("the phone-facing row");
    assert_eq!(phone_view.agent, protocol::agent::AgentKind::Codex);
    assert_eq!(phone_view.session_uid, uid);

    // Printed, not just asserted. This gate is the evidence the ungate rests on, so
    // a run of it has to leave behind what a reader would otherwise have to take on
    // trust: what the command produced, what the fleet says, and what `ls` renders.
    println!(
        "--- `codeconnect codex` output (pty) ---\n{}",
        read_file(&out_path)
    );
    println!("--- codeconnect ls ---\n{ls}");
    println!("--- codeconnect sessions list ---\n{sessions}");
    println!("--- the daemon's row ---\n{row:#?}");
    println!(
        "UNGATE PASS (launch) — `codeconnect codex` in {canonical_cwd} produced {session_name} \
         uid {uid}, filed as codex on thread {thread}, attached, and rendered by `ls`"
    );

    // ------------------------------------------------------------------ a turn
    //
    // The session is only worth launching if it can be used. Typed into the pane,
    // because nothing else may start a turn: the broker refuses `turn/start` to the
    // ccd role, so a turn is by construction the real TUI's, driven by a keystroke.
    assert!(
        wait_until(Duration::from_secs(60), || sb.tui_running(&run_dir)),
        "the host never launched the real codex TUI. broker.log:\n{}",
        read_file(&run_dir.join("broker.log"))
    );
    // The TUI is running, which is not the same as ready to accept a prompt; it
    // still has a handshake and a first render to do. Settled the way the ccd live
    // gate settles, then typed in two calls so the text lands before Enter does.
    std::thread::sleep(Duration::from_secs(5));
    let seq_before = sb.run_of(&uid).expect("the row before the turn").last_seq;
    sb.send_keys(
        &session_name,
        &["Reply with the single word ok and nothing else."],
    );
    std::thread::sleep(Duration::from_millis(600));
    sb.send_keys(&session_name, &["Enter"]);

    // **The broker forwarded it.** A turn that the broker refused would leave the
    // pane looking similar and prove the opposite of what this gate claims.
    let forwarded = wait_until(Duration::from_secs(120), || {
        read_file(&run_dir.join("broker.log")).contains("Tui: forward (turn/start")
    });
    let broker_log = read_file(&run_dir.join("broker.log"));
    assert!(
        forwarded,
        "the broker never forwarded the TUI's turn/start. broker.log:\n{broker_log}\npane:\n{}",
        sb.capture_pane(&session_name)
    );
    assert!(
        !broker_log.contains("refuse->synthetic error (turn/start"),
        "the broker refused a turn on a session this launch created. broker.log:\n{broker_log}"
    );

    // **And it completed, in the pane a person is looking at.** The `•` is
    // load-bearing: it marks the agent's reply, and without it the prompt's own
    // echo — which also contains the word — would satisfy this.
    let replied = wait_until(Duration::from_secs(120), || {
        sb.capture_pane(&session_name)
            .to_lowercase()
            .contains("• ok")
    });
    let pane = sb.capture_pane(&session_name);
    assert!(
        replied,
        "the turn was forwarded but never completed in the pane. pane:\n{pane}"
    );

    // **The daemon recorded it.** `last_seq` is the per-session event count on the
    // fleet answer, so a strictly greater one is the observation reaching storage
    // through the ccd link — the leg asserted as `Ccd: forward` above.
    assert!(
        wait_until(Duration::from_secs(60), || sb
            .run_of(&uid)
            .map(|s| s.last_seq > seq_before)
            .unwrap_or(false)),
        "the daemon recorded no event across a real turn (last_seq stuck at {seq_before}). \
         ccd.log:\n{}",
        read_file(&sb.base.join("ccd.log"))
    );
    println!("--- the pane after the turn ---\n{pane}");
    println!(
        "UNGATE PASS (turn) — a turn typed into {session_name}'s real TUI pane was forwarded by \
         the broker, completed in the pane, and reached the daemon (last_seq {} > {seq_before})",
        sb.run_of(&uid).expect("the row after the turn").last_seq
    );

    // ------------------------------------------------------------- the bounce
    //
    // The daemon goes away under a live launched session and comes back. What must
    // NOT happen is a second identity, a rewritten history, or a session torn down
    // because its daemon blinked.
    let created_at = row.created_at.clone();
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert!(
        sb.has_session_named(&session_name),
        "killing ccd must not touch the session: the daemon never owned it"
    );
    let mut daemon = sb.spawn_daemon(&ccd);
    // Waited on `link`, which is memory-only — a fresh daemon has no supervisors,
    // so nothing but a real re-registration can make it `Attached`. (`Live` is a
    // durable column the new daemon would read straight back off disk.)
    assert!(
        wait_until(Duration::from_secs(90), || sb
            .run_of(&uid)
            .map(|s| s.link == protocol::event::Link::Attached)
            .unwrap_or(false)),
        "the supervisor did not re-register the launched session after the daemon came back. \
         ccd.log:\n{}",
        read_file(&sb.base.join("ccd.log"))
    );
    let after = sb.run_of(&uid).expect("the row after the bounce");
    assert_eq!(after.lifecycle, protocol::event::Lifecycle::Live);
    assert_eq!(
        after.session_uid, uid,
        "a re-registration must not mint a second identity"
    );
    assert_eq!(
        after.created_at, created_at,
        "a re-registration must not rewrite when the run started"
    );
    assert_eq!(after.agent, protocol::agent::AgentKind::Codex);
    assert_eq!(after.cwd, row.cwd);
    assert_eq!(after.session_id, session_name);
    assert_eq!(
        sb.fleet().len(),
        1,
        "the bounce must leave exactly one row for one session: {:?}",
        sb.fleet()
    );
    // **The thread comes back from the broker's replay**, and it is waited for
    // rather than sampled: `link == Attached` is the supervisor's reconnect, which
    // happens strictly before the fresh link subscribes, is handed the replayed
    // `thread/started` and binds. Sampling here read the gap and saw `None`.
    assert!(
        wait_until(Duration::from_secs(90), || sb
            .run_of(&uid)
            .and_then(|s| s.codex_thread_id)
            .is_some()),
        "the thread did not come back across the daemon restart. ccd.log:\n{}\nbroker.log:\n{}",
        read_file(&sb.base.join("ccd.log")),
        read_file(&run_dir.join("broker.log"))
    );
    assert_eq!(
        sb.run_of(&uid).and_then(|s| s.codex_thread_id),
        Some(thread.clone()),
        "the thread must come back naming the SAME thread, from the broker's replay"
    );
    assert!(
        read_file(&run_dir.join("broker.log")).contains("replayed thread/started"),
        "and it must have come from the broker's replay rather than a second live \
         announcement this test cannot tell apart"
    );
    println!("UNGATE PASS (bounce) — {session_name} survived a daemon restart unchanged");

    // -------------------------------------------------------------- teardown
    //
    // There is no `codeconnect stop`, for Claude or for Codex: a session ends when
    // the thing in the pane ends. Killing the tmux session is that, deterministically
    // — the TUI dies, the host reaps, and the supervisor the coordinator became
    // reports the exit.
    let coord = sb
        .recorded_identity_of(&uid, "coordinator")
        .map(
            |(pid, sec, usec)| protocol::proc_identity::ProcessIdentity {
                pid,
                birth: protocol::proc_identity::BirthIdentity {
                    start_sec: sec,
                    start_usec: usec,
                },
            },
        )
        .expect("a ready record names its coordinator");
    let _ = sb
        .tmux_cmd()
        .args([
            "-f",
            "/dev/null",
            "kill-session",
            "-t",
            &format!("={session_name}"),
        ])
        .status();
    let _ = command.kill();
    let _ = command.wait();

    assert!(
        wait_until(Duration::from_secs(60), || {
            protocol::proc_identity::liveness(&coord) == protocol::proc_identity::Liveness::Gone
        }),
        "the coordinator/supervisor outlived its session"
    );
    assert!(
        wait_until(Duration::from_secs(60), || !run_dir.exists()),
        "the run dir survived the session at {}",
        run_dir.display()
    );
    assert!(
        !sb.has_session_named(&session_name),
        "the tmux session survived being killed"
    );
    assert!(
        tagged_pids(run_dir.to_str().expect("utf-8 run dir")).is_empty(),
        "processes carrying this launch's run dir survived it"
    );
    assert!(
        wait_until(Duration::from_secs(60), || sb
            .run_of(&uid)
            .map(|s| s.lifecycle == protocol::event::Lifecycle::Exited)
            .unwrap_or(false)),
        "the daemon was never told the session ended. ccd.log:\n{}",
        read_file(&sb.base.join("ccd.log"))
    );

    let _ = daemon.kill();
    let _ = daemon.wait();
    println!(
        "UNGATE PASS (teardown) — {session_name} ended with no leaked process, run dir or pane"
    );
}

// ============================= THE COLLISION GATE =============================
//
// A11.7: "Marker tests build fixtures by hand rather than two real hosts racing
// for one derived name. Closes when: a live two-launch collision harness exists."
//
// Every marker-isolation test before this one staged its competitor inside a
// single process — `codex_host.rs`'s teardown test calls `write_owner_marker`
// with a literal foreign uid; `codex_coordinator.rs`'s
// `only_a_private_directory_this_launch_claimed_counts_as_a_run_dir` hand-writes
// a `good` and a `foreign` marker; `bringup_itself_refuses_a_run_dir_another_
// launch_claimed` gets closest and still hand-writes the foreign marker and
// drives a scripted `CoordinatorDeps`. Nothing anywhere ran two hosts.
//
// This harness had in fact spent its whole life ENGINEERING THE COLLISION AWAY:
// `SANDBOX_SEQ` exists "so two live sandboxes can never derive the same run dir",
// and `codex_lifecycle_integration.rs` records the incident that put it there.
// The gate below turns that around and derives the collision on purpose.

/// Everything a launch that lost a derived run dir must show, from the moment it
/// terminalizes until its last guardian is gone. Returns the recorded reason.
///
/// Shared by the two contenders in the gate below — the one that raced the winner
/// simultaneously and the one that arrived after it was serving — because the
/// property is the same for both: a launch refused a directory it did not create
/// dies without a session, without a supervisor, and without leaving cleanup owed.
///
/// **Two failure sentences, because there are genuinely two orderings.** A refused
/// host dies within milliseconds of its pane's `execve`, and its session dies with
/// it, while its coordinator is still finishing the bookkeeping that follows
/// `new-session` (resolve the session, clear `remain-on-exit`, record that it
/// held). Whether the coordinator notices the loss during that bookkeeping or
/// later in the bring-up wait is a race between two processes on one machine, and
/// the answer is a property of the machine rather than of the code under test.
///
/// **Both were measured.** The simultaneous loser came out as the first shape in
/// every run whose reason was captured — its coordinator is doing the same
/// bookkeeping at the same moment as the winner's, so the refusal lands squarely
/// inside that window. The late contender, whose coordinator has nobody to keep
/// pace with, has been seen both ways, including the second shape:
///
/// ```text
/// wrapper bring-up failed: the pane's tmux session is gone and the wrapper never
/// bound its broker sockets (run dir created but empty) — the host died before it
/// was up
/// ```
///
/// What is invariant either way is the class: a launch whose own pane died before
/// it could be proven up. Enumerated rather than waved at, so a THIRD shape — a
/// codex that would not start, a charter the host rejected — fails this gate
/// loudly instead of passing as "well, it failed".
fn assert_refused_without_adopting(sb: &LiveSandbox, who: &str, coord: &mut Child) -> String {
    const REFUSAL_SHAPES: [&str; 2] = [
        // The post-`new-session` bookkeeping found the session already gone —
        // every captured run of the simultaneous pair.
        "the created tmux session could not be made safe",
        // The bring-up wait saw the pane gone — observed on the late contender.
        "the host died before it was up",
    ];
    let run_dir = sb.run_dir.clone();
    assert!(
        wait_until(Duration::from_secs(90), || sb.failure_reason().is_some()),
        "{who} never terminalized. record: {:?}",
        sb.record_text()
    );
    let reason = sb.failure_reason().expect("the reason just observed");
    let host_pid = sb.host_arrived().unwrap_or_else(|| {
        panic!(
            "no host ever reached {who}'s gate, so it never actually contended for {}. \
             record: {:?}",
            run_dir.display(),
            sb.record_text()
        )
    });
    assert!(
        REFUSAL_SHAPES.iter().any(|shape| reason.contains(shape)),
        "{who} failed in a shape this gate does not recognise, so it is not evidence \
         that its host was refused the directory: {reason:?}"
    );
    // ---- the STAGE, not merely a death before readiness ---------------------
    //
    // The assertions around this one all pass under the reviewer's counterexample
    // (finding 11): a host that treats `EEXIST` as SUCCESS, ADOPTS the winner's
    // directory, and then dies on an already-existing log satisfies the shape
    // check, the inode check, the marker check, the no-`Ready` check and the
    // cleanup check — every one of them, measured. What it CANNOT do is get past
    // the run-dir claim without recording that it did. This bit is written by the
    // host in `orchestrate`, on the one line after
    // `create_run_dir_atomically` returns Ok, so a loser whose record says the
    // claim was made was NOT refused at the fence — it adopted, and this gate would
    // otherwise be green for exactly the wrong reason.
    //
    // Fence-agnostic on purpose. The bit says "past the claim", not "refused by the
    // staging `mkdir`" or "refused by the `RENAME_EXCL` publish", so it holds the
    // same for the simultaneous loser (which meets either fence — see the phase-6
    // note) and the late contender (which always meets the publish). A message
    // match would have to enumerate both sentences and would still say nothing about
    // the adoption case, whose sentence is a LATER stage's, not the claim's.
    let claimed = sb
        .record_json()
        .and_then(|r| r.get("host_claimed_run_dir").and_then(|b| b.as_bool()));
    assert_eq!(
        claimed,
        Some(false),
        "{who}'s host got PAST the run-dir claim ({}), so whatever killed it was a \
         later stage and this gate is green for the wrong reason. record: {:?}",
        run_dir.display(),
        sb.record_text()
    );
    assert!(
        processes_referencing(&format!("--run-dir {}", run_dir.display()))
            .iter()
            .all(|(pid, _)| *pid != host_pid),
        "{who}'s host (pid {host_pid}) is still running against {}",
        run_dir.display()
    );
    // Not "is not Ready now" — the record holds one state, and `Ready` is durable
    // once committed, so its absence from the whole document is the strong form.
    let record = sb.record_text().unwrap_or_default();
    assert!(
        !record.contains("\"Ready\""),
        "{who} committed ready — two launches cannot both own {}: {record}",
        run_dir.display()
    );
    assert!(
        !sb.has_session(),
        "{who} left a session behind on its own tmux server"
    );
    assert!(
        wait_until(Duration::from_secs(30), || matches!(
            coord.try_wait(),
            Ok(Some(_))
        )),
        "{who}'s coordinator is still running; a failed launch has no supervisor to \
         become"
    );
    let _ = coord.wait();

    // The half of "no contamination" that needs this launch's whole life to be
    // over: its custodian is armed with its uid and the run-dir name its own record
    // wrote down, and that name resolves to somebody else's directory. It must
    // settle without deleting it.
    assert!(
        wait_until(Duration::from_secs(90), || sb.cleanup_state().as_deref()
            == Some("Complete")),
        "{who}'s cleanup never settled, so this gate cannot say what its custodian did \
         with {}. record: {:?}",
        run_dir.display(),
        sb.record_text()
    );
    let custodian_gone = wait_until(Duration::from_secs(30), || {
        match sb.recorded_identity("custodian") {
            None => true,
            Some((pid, sec, usec)) => {
                let id = protocol::proc_identity::ProcessIdentity {
                    pid,
                    birth: protocol::proc_identity::BirthIdentity {
                        start_sec: sec,
                        start_usec: usec,
                    },
                };
                protocol::proc_identity::liveness(&id) != protocol::proc_identity::Liveness::Alive
            }
        }
    });
    assert!(
        custodian_gone,
        "{who}'s custodian is still alive after its cleanup reported complete, so any \
         no-contamination assertion would be premature"
    );
    reason
}

/// The winning launch's directory, unchanged in every dimension a contender could
/// have disturbed it in.
fn assert_run_dir_untouched(winner: &LiveSandbox, identity: (u64, u64), marker: &[u8]) {
    let run_dir = &winner.run_dir;
    let staging = PathBuf::from(format!("{}.tmp", run_dir.display()));
    assert_eq!(
        dir_identity(run_dir),
        identity,
        "{} is a DIFFERENT directory than the one the winner published — a contender \
         replaced it rather than being refused it",
        run_dir.display()
    );
    assert_eq!(
        std::fs::read(run_dir.join("owner")).expect("re-read the owner marker"),
        marker,
        "the winner's owner marker was rewritten while a contender was being refused"
    );
    assert_private_dir(run_dir);
    assert!(
        !staging.exists(),
        "a refused launch left its staging directory behind: {}",
        staging.display()
    );
    assert!(
        winner.broker_legs_bound(),
        "the winner's broker legs are gone from {}",
        run_dir.display()
    );
    assert!(
        winner.has_session(),
        "the winner's session did not survive a contender"
    );
    assert_eq!(
        winner.record_field("state").as_deref(),
        Some("Ready"),
        "the winner's record no longer says ready: {:?}",
        winner.record_text()
    );
}

/// **A11.7: two real launches race for ONE derived run dir; exactly one wins,
/// and the loser cannot touch what it lost.**
///
/// # Why a collision is possible at all
///
/// `codex_coordinator::choose_run_dir` names `/tmp/cch.<uid's LAST TEN
/// alphanumerics>.<nonce's FIRST SIXTEEN>`, and its doc comment says outright
/// that the mapping is **many-to-one**: "Uniqueness is therefore NOT a property
/// of this function, and no caller may treat the name as an identity." That is
/// the whole reason the owner marker exists, and it is the "one derived name"
/// A11.7 means. So this test does not simulate a collision — it *derives* one,
/// with [`collide_on_the_run_dir`], and then asserts the two paths are equal so
/// the premise is checked rather than assumed.
///
/// # What is contended, and what deliberately is not
///
/// **Only the run dir.** Every sandbox here — the two that race, and the third
/// that arrives late — has its own `CODECONNECT_HOME` (so its own launch record),
/// its own `CODEX_HOME`, and its own `TMUX_TMPDIR` (so its own tmux server, which
/// is why the shared `cc-live` session name contends for nothing). Piling
/// collisions together would prove that *something* went wrong; isolating the
/// variable is what makes the failure attributable to the directory.
///
/// # What is asserted, symmetrically
///
/// The race is genuinely non-deterministic, so nothing here names a winner in
/// advance: the marker at the shared path is READ, and whichever launch it names
/// is then held to the full property set. Both degenerate outcomes are loud
/// failures — "both won" fails at the loser's record never reaching `Ready`, and
/// "both lost" fails at the winner's. Measured over 20 clean runs with no
/// failures; of the 19 that recorded which side won it was A eleven times and B
/// eight, so the barrier really is releasing two symmetric launches and this gate
/// really cannot be written around a fixed winner.
///
///   1. **Exactly one claim.** A marker exists at the shared name, it names one
///      of the two launches, and its `(uid, nonce)` pair is internally consistent
///      with that launch — not one launch's uid beside the other's nonce.
///   2. **The winner really launched.** Both broker legs bound under the shared
///      directory and the record committed `Ready`.
///   3. **The loser really contended, and was refused.** Its record says
///      `host_reached_gate`, and names the host process that arrived — the
///      difference between a launch that contended and a pane that never started.
///      It terminalized `Failed`, never `Ready`, with no session and no host left.
///   4. **No cross-contamination, across the loser's whole life including its
///      teardown.** The winner's directory keeps its **inode** (a name is not an
///      identity: a loser that removed the winner's dir and published its own
///      would leave a perfectly good directory at that path), its marker bytes,
///      its 0700 mode, its bound legs and its live session — and the loser's
///      custodian, which is armed with the loser's uid and the *recorded* run-dir
///      name, settles its cleanup without deleting a directory whose marker names
///      somebody else. That last clause is the A11.5 fd-bound-delete property
///      observed under a real race instead of a hand-written marker.
///   5. **And again, against a launch that arrives LATE.** See the third
///      contender in phase 6 and the note on the two fences beside it: the
///      simultaneous pair meets the staging `mkdir`, a later launch meets the
///      `RENAME_EXCL` publish, and both are the same derived name.
///
/// # What this gate cannot observe, stated plainly
///
/// **The losing host's own refusal sentence.** It goes to stderr, which in
/// production is the pane's pty, and the pane dies with the command that refused
/// (the coordinator asserts `remain-on-exit off` and records that it held). There
/// is no file to read it back from and no window in which to capture it.
///
/// It was read once, with a temporary trace added to `run_host`, and it is quoted
/// here because it is the fact this gate is otherwise arguing for by signature:
///
/// ```text
/// creating the staging run dir /tmp/cch.D042400000.000043d04a106be9.tmp
/// exclusively — it must NOT already exist: File exists (os error 17)
/// ```
///
/// The same sentence is asserted against a real host process in
/// `codex_host_fatal.rs::an_existing_run_dir_is_refused`, which is a hand-built
/// fixture and openly is one. What A11.7 asked for and what this adds is that the
/// *collision* is real; what stands in for the text here is the refusal's complete
/// filesystem signature — the contender published nothing, left no
/// `<run_dir>.tmp` residue, and the winner's inode never changed.
///
/// **Which line of the coordinator notices.** The refusal lands within
/// milliseconds of the pane's `execve`, inside the window in which that
/// coordinator is still resolving its own session and clearing `remain-on-exit`.
/// Both orderings occur and both were measured; they are enumerated in
/// `assert_refused_without_adopting`, and a third shape fails the gate.
///
/// **The coordinator's own marker check.** `codex_coordinator::run_dir_is_ours`
/// refuses to count a foreign directory's sockets as this launch's readiness, and
/// a live collision does NOT reach it: measured by deleting the marker clause
/// outright, this gate stayed green, because a refused host takes its pane and its
/// session with it and the launch terminalizes on that loss long before the
/// bring-up loop could misread anything. Reaching it needs a host that dies
/// WITHOUT taking the session down, which is a staged interleaving, not a race —
/// so `bringup_itself_refuses_a_run_dir_another_launch_claimed` stays the cover
/// for that half, and this gate covers the half it cannot.
///
/// Measured while writing this, and left as observations rather than changes:
///
///   * the loser's bring-up reason renders the shared directory as "run dir
///     created but empty", because `BringupObservation::run_dir_present` is a
///     bool and the coordinator's marker verdict is not carried into the
///     sentence. It is misleading in exactly this case — the directory is neither
///     this launch's nor empty — so the assertions below pin only the parts that
///     are true of this run;
///   * a terminalized record's `host_lease` is always `null`, because `to_failed`
///     drops it. Arrival evidence has to come from `host_reached_gate` /
///     `host_identity`, which are written before the verdict and never cleared;
///   * `renamex_np` with `RENAME_EXCL` and a plain `rename(2)` are
///     indistinguishable here, because A11.4 puts the marker INSIDE the directory
///     before publishing it, so the target is never empty and a plain rename gets
///     `ENOTEMPTY` (measured directly: onto an empty directory it succeeds and
///     replaces, onto a non-empty one it fails). `RENAME_EXCL`'s distinguishing
///     power is against an empty squatted name, which a collision cannot produce.
///     What this gate does bind is the publish's EXCLUSIVITY: made non-exclusive
///     (remove-then-rename), the late contender adopted the winner's live
///     directory and committed `Ready` on it, and phase 6 went red.
#[test]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
fn two_real_launches_racing_for_one_derived_run_dir_leave_exactly_one_winner() {
    let Some(codex) = live_gate() else { return };
    let a = LiveSandbox::new("racea");
    let b = LiveSandbox::colliding_with("raceb", &a, 1);
    let run_dir = a.run_dir.clone();
    let staging = PathBuf::from(format!("{}.tmp", run_dir.display()));
    let tag = a.tag().to_string();
    println!(
        "COLLISION PREMISE — launch A (uid {}, nonce {}) and launch B (uid {}, nonce {}) \
         both derive {}",
        a.uid,
        a.nonce,
        b.uid,
        b.nonce,
        run_dir.display()
    );
    assert!(
        !run_dir.exists() && !staging.exists(),
        "the derived name must be unclaimed before either launch starts: {}",
        run_dir.display()
    );

    // Both servers up and held open BEFORE either launch — see
    // `hold_the_server_open` for why a per-sandbox server that dies with its only
    // session is a harness artifact production cannot produce.
    a.hold_the_server_open();
    b.hold_the_server_open();

    // **Spawned simultaneously, not one-then-the-other.** A barrier releases both
    // `Command::spawn` calls at once, so the only thing deciding the winner is the
    // work each launch does afterwards — its own record writes, its own pane.
    // Which one gets there first is genuinely indeterminate, and nothing below
    // depends on the answer.
    let gun = std::sync::Barrier::new(2);
    let (coord_a, coord_b) = std::thread::scope(|s| {
        let left = s.spawn(|| {
            gun.wait();
            a.spawn_coordinator(&codex)
        });
        let right = s.spawn(|| {
            gun.wait();
            b.spawn_coordinator(&codex)
        });
        (
            left.join().expect("spawn launch A"),
            right.join().expect("spawn launch B"),
        )
    });

    // --- 1. The name is claimed exactly once, and the marker says by whom -----
    //
    // The marker is inside the directory before the `rename` that publishes it
    // (A11.4), so a directory observed at the final name always has one — which is
    // why "the dir exists" and "who owns it" are one observation here, not two.
    let claimed = wait_until(Duration::from_secs(90), || marker_owner(&run_dir).is_some());
    assert!(
        claimed,
        "neither launch published a run dir at {} — a collision gate in which nobody \
         wins is a failure, not a pass.\nA record: {:?}\nB record: {:?}",
        run_dir.display(),
        a.record_text(),
        b.record_text()
    );
    let (owner_uid, owner_nonce) = marker_owner(&run_dir).expect("the marker just observed");
    let (winner, loser, winner_coord, mut loser_coord) = if owner_uid == a.uid {
        (&a, &b, coord_a, coord_b)
    } else if owner_uid == b.uid {
        (&b, &a, coord_b, coord_a)
    } else {
        panic!(
            "{} is owned by uid {owner_uid:?}, which is neither launch ({} / {})",
            run_dir.display(),
            a.uid,
            b.uid
        )
    };
    assert_eq!(
        owner_nonce, winner.nonce,
        "the marker pairs uid {owner_uid} with nonce {owner_nonce}, which is not that \
         launch's nonce — a marker that mixes two launches identifies neither"
    );
    let claimed_identity = dir_identity(&run_dir);
    let claimed_marker =
        std::fs::read(run_dir.join("owner")).expect("read the marker that was just observed");
    // Which SIDE won is printed, not asserted: the gate must never depend on it,
    // and a run of these that always names the same side is worth seeing.
    println!(
        "CLAIMED — launch {} (uid {}) won {} (inode {:?}); launch {} (uid {}) must now \
         be refused",
        if owner_uid == a.uid { "A" } else { "B" },
        winner.uid,
        run_dir.display(),
        claimed_identity,
        if owner_uid == a.uid { "B" } else { "A" },
        loser.uid
    );

    // --- 2. The winner really launched ---------------------------------------
    assert!(
        wait_until(Duration::from_secs(60), || winner.broker_legs_bound()),
        "the winning launch never bound its broker legs under {}. record: {:?}\n\
         broker.log:\n{}",
        run_dir.display(),
        winner.record_text(),
        read_file(&run_dir.join("broker.log"))
    );
    assert!(
        wait_until(Duration::from_secs(90), || winner
            .record_field("state")
            .as_deref()
            == Some("Ready")),
        "the winning launch never committed ready. record: {:?}",
        winner.record_text()
    );
    println!("WINNER READY — uid {} holds a live session", winner.uid);

    // --- 3+4. The loser really contended, was refused, and settled ----------
    let reason = assert_refused_without_adopting(loser, "the losing launch", &mut loser_coord);
    println!("LOSER REFUSED — uid {} failed: {reason}", loser.uid);

    // --- 5. No cross-contamination -------------------------------------------
    assert_run_dir_untouched(winner, claimed_identity, &claimed_marker);
    let live = processes_referencing(&tag);
    assert!(
        !live.is_empty(),
        "nothing references {} any more, so the 'winner survived' assertions above \
         are about a session that is gone",
        run_dir.display()
    );
    println!(
        "NO CONTAMINATION — {} is still inode {:?}, marker unchanged, legs bound, \
         session live with {} process(es) attached",
        run_dir.display(),
        claimed_identity,
        live.len()
    );

    // --- 6. A LATE contender, against the OTHER fence -------------------------
    //
    // **Why a third launch, when two already raced.** Measured (with a temporary
    // trace in `run_host`, since the pane's stderr is unreadable): the simultaneous
    // loser MOSTLY meets `create_run_dir_atomically`'s FIRST fence — the exclusive
    // `mkdir` of `<run_dir>.tmp`, "creating the staging run dir … exclusively — it
    // must NOT already exist: File exists (os error 17)" — because two barrier-
    // released coordinators usually stay close enough that neither has published
    // when the loser stages. But NOT always: over 20 clean races the loser hit the
    // staging fence 15 times and the publish `renamex_np(RENAME_EXCL)` the other 5
    // — the winner had published in the interval, so the loser's staging name was
    // free and it met the publish instead. An earlier version of this note claimed
    // the simultaneous loser is refused at the staging fence in "every" run; that
    // was over-stated, and the correction is why the stage assertion below keys on
    // a record bit rather than on which sentence the host printed.
    //
    // A third launch is still worth having because it pins the publish fence
    // DETERMINISTICALLY. Two launches SECONDS apart can derive the same name just
    // as easily as two at once, and that one always arrives at a directory already
    // published and serving: its staging name is free, so it stages, writes its
    // marker, and meets the publish fence — 20/20 in the same measurement. Both
    // fences belong to A11.7's "one derived name", and a gate that leaned on the
    // simultaneous pair alone would leave the exclusivity of the publish — the
    // thing `RENAME_EXCL` is there for — exercised only by chance, not on every run.
    //
    // The `host_claimed_run_dir` assertion in `assert_refused_without_adopting` is
    // FENCE-AGNOSTIC across all of this: it asserts the host did not get past the
    // claim, which is equally true whether the staging `mkdir` or the publish
    // `renamex_np` did the refusing, so both contenders are held to it without the
    // gate having to know or predict which fence each met.
    let late = LiveSandbox::colliding_with("racec", &a, 2);
    assert_ne!(
        late.uid, loser.uid,
        "the late contender must be a third launch, not the loser again"
    );
    assert_ne!(late.uid, winner.uid, "and not the winner again");
    assert_eq!(
        late.run_dir, run_dir,
        "the late contender must derive the SAME name"
    );
    late.hold_the_server_open();
    let mut late_coord = late.spawn_coordinator(&codex);
    let late_reason = assert_refused_without_adopting(&late, "the late contender", &mut late_coord);
    println!(
        "LATE CONTENDER REFUSED — uid {} failed: {late_reason}",
        late.uid
    );
    assert_run_dir_untouched(winner, claimed_identity, &claimed_marker);
    assert!(
        !processes_referencing(&tag).is_empty(),
        "the winner's session is gone after the late contender ran"
    );
    println!(
        "STILL NO CONTAMINATION — {} survived a second contender intact",
        run_dir.display()
    );

    // --- 6. And the winner tears down normally afterwards ---------------------
    let mut winner_coord = winner_coord;
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &winner_coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = winner_coord.wait();
    assert_torn_down_clean(winner, &tag);
    println!(
        "A11.7 PASS — two real launches raced for {}; uid {} won it and uid {} was \
         refused without adopting, mutating or deleting it",
        run_dir.display(),
        winner.uid,
        loser.uid
    );
}
