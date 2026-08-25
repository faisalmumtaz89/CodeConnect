//! GATED live end-to-end integration for the `internal-codex-host` wrapper
//! (Phase 2e-2a), validated against a real `codex` (0.147). This stands up the
//! WHOLE host — a real `codex app-server`, the in-process broker in front of it,
//! and the real interactive TUI (`codex --remote`) — inside a PTY, and proves the
//! things this chunk exists to guarantee:
//!
//!   1. **CRUX** — the real `codex --remote` TUI attaches THROUGH the broker: it
//!      completes the WS-over-UDS handshake on the broker's `tui.sock` and at
//!      least one request off that leg is classified `Forward` and relayed
//!      upstream. Proven from the broker's own event-sink log (`broker.log`),
//!      which records `Tui: forward (...)` only when a real client connected on
//!      the TUI leg and a request was relayed. If `codex --remote` cannot
//!      handshake the broker's `tui.sock`, no such line ever appears — that is a
//!      STOP-AND-AMEND, and the test fails loudly, dumping both logs.
//!   2. **Teardown** — after the host is signalled, it stops both children and
//!      removes the run directory it created. What the harness can actually
//!      observe, and therefore all it claims: **no visible process references the
//!      run-dir tag** (an argv scan of the process table — see
//!      [`processes_referencing`] for what that does and does not prove), and the
//!      directory is gone.
//!   3. **The fatal path, live** — SIGKILL the *real* app-server under a live
//!      session and the host exits 70 with no visible process still referencing
//!      the real TUI's `--remote unix://<run>/tui.sock`. The deterministic
//!      twin of this gate is `codex_host_fatal.rs` (fake codex, normal suite);
//!      this one proves the same contract holds against the real binaries.
//!
//! # What the forward line does and does not prove
//!
//! The broker's `Forward` notes are deliberately generic (`request allowlisted`,
//! `ownership request: fingerprint asserted`) — they carry no method name. So this
//! harness asserts exactly what the log evidences: a real codex client completed
//! the handshake and a request was classified `Forward` and relayed. The stronger
//! statement — that the request was the mandatory `initialize` — follows from the
//! app-server's own protocol (it rejects everything before `initialize`, so a
//! session that goes on to work must have sent it first), but that is *reasoning*,
//! not something the log line evidences, so it is written as a comment and never
//! asserted.
//!
//! # Why `#[ignore]` AND env-gated (mirrors `codex-broker/tests/live_appserver.rs`)
//!
//! It needs `codex` installed and spawns real subprocesses under a PTY, so a
//! normal `cargo test` must never run it. Two independent guards:
//!   * `#[ignore]` — excluded unless `-- --ignored` is passed.
//!   * `CC_CODEX_LIVE=1` — even under `--ignored`, it no-ops (skip message, no
//!     panic) unless the env flag is set.
//!
//! With the flag SET, a missing `codex` is a **failure**, not a skip: an operator
//! who explicitly demanded a live run must never be handed a vacuous green. The
//! same rule extends to the gate's own premise — [`live_gate`] additionally checks
//! the resolved binary before any of it runs, because validating the host against a
//! wrapper or a different codex is a vacuous green of a subtler kind: it reports
//! coverage that executed, against the wrong thing.
//!
//! What that premise check actually establishes, stated at its real strength: the
//! resolved file **is a native Mach-O executable** (a filesystem fact) and it
//! **reports** `0.147.x` when asked for its version (its own claim about itself).
//! It is not a proof that the binary is genuinely upstream codex — nothing short of
//! a signature check would be, and a binary that lies about `--version` would pass.
//! Both halves are still worth having, because the failures they actually catch are
//! the common ones: an `npm` shim or shell wrapper on PATH, and a stale or
//! newer-series install.
//!
//! Run it deliberately:
//! ```text
//! CC_CODEX_LIVE=1 cargo test -p codeconnect --test live_codex_host -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// SUN_LEN: a unix-domain socket PATH must be shorter than `sun_len` (~104 bytes
// on macOS); binding a longer path fails `path must be shorter than SUN_LEN`. A
// deep build path is easily too long, so every socket here lives under a SHORT
// `/tmp` dir.
// ---------------------------------------------------------------------------
const SUN_LEN_LIMIT: usize = 104;

/// The host's `EX_SOFTWARE`: bring-up unproven, or a session-fatal death.
const EX_HOST_FATAL: i32 = 70;

fn assert_sun_len(path: &Path) {
    let len = path.as_os_str().len();
    assert!(
        len < SUN_LEN_LIMIT,
        "socket path {path:?} is {len} bytes; a unix socket path must be < {SUN_LEN_LIMIT} \
         (SUN_LEN) or bind() fails — keep sockets under a short /tmp dir",
    );
}

/// A unique short `/tmp` path. `/tmp` (not the long macOS `/var/folders/...` temp)
/// keeps contained socket paths inside SUN_LEN.
fn short_tmp_path(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/cchost.{}.{tag}.{n}.{nanos}",
        std::process::id()
    ))
}

/// A short-path `/tmp` directory THIS harness creates (0700) and removes on Drop.
/// Used for the isolated `CODEX_HOME`.
struct ShortTmpDir {
    path: PathBuf,
}

impl ShortTmpDir {
    fn new(tag: &str) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let mut last_err: Option<std::io::Error> = None;
        for _ in 0..16 {
            let path = short_tmp_path(tag);
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not create a unique /tmp/cchost.* dir after 16 attempts",
            )
        }))
    }

    fn as_str(&self) -> &str {
        self.path.to_str().expect("short /tmp path is utf-8")
    }
}

impl Drop for ShortTmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A run-dir PATH the HOST must create itself. Nothing is created here — the host
/// owns its run dir and refuses to adopt an existing one, so the harness must not
/// pre-create it. Drop only sweeps up if the host somehow failed to.
struct HostRunDir {
    path: PathBuf,
}

impl HostRunDir {
    fn new(tag: &str) -> HostRunDir {
        HostRunDir {
            path: short_tmp_path(tag),
        }
    }
    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
    fn as_str(&self) -> &str {
        self.path.to_str().expect("short /tmp path is utf-8")
    }
}

impl Drop for HostRunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// True iff `path` is a regular FILE with at least one executable bit set.
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

/// Resolve the NATIVE codex binary: prefer `~/.local/bin/codex` (the standalone
/// native build), fall back to `which codex`. `None` (never a panic) if none is a
/// valid executable file — the caller decides whether that is a skip or a failure.
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
    if is_executable_file(&path) {
        Some(path)
    } else {
        None
    }
}

// ------------------------------------------------- validating the gate's premise
//
// This harness's whole claim is "the host works against a REAL codex 0.147". That
// claim rests on the binary this file resolves actually being one — and
// `resolve_codex` above evidences neither half of it: `~/.local/bin/codex` is a
// symlink chain the user controls, and `which codex` finds whatever is first on
// PATH, which on a dev machine is quite often an `npm` shim or a shell wrapper.
// A live run against the wrong thing does not fail; it validates the host against
// a binary the rest of CodeConnect refuses to launch, and reports it as green. So
// the premise is checked here, and a mismatch FAILS the run with a message that
// names what was found.
//
// The check is deliberately described at its real strength throughout: it
// establishes that the file IS a native executable, and that it REPORTS the pinned
// series. "Is genuinely codex 0.147" is a stronger claim than either check makes,
// and is not asserted anywhere below.

/// The codex series this live gate is grounded against, matching the compiled-in
/// pin `protocol::config::CODEX_PINNED_VERSIONS` that `src/codex.rs` enforces.
const LIVE_CODEX_VERSION_PREFIX: &str = "0.147.";

/// Bounded budget for `codex --version`. A binary that does not answer promptly is
/// not the standalone native CLI, and the gate must not hang on it.
const VERSION_PROBE_BUDGET: Duration = Duration::from_secs(20);

/// Whether the file at `path` is a native Mach-O executable (thin or universal),
/// rather than a `#!`-script or a `.js` wrapper. **Mirrors
/// `codex::is_native_executable`** — the same first-four-bytes magic check the
/// launcher applies — so this gate cannot validate the host against a binary
/// `codeconnect codex` would itself refuse. (A `#!`-script starts `0x23 0x21`,
/// which is in none of these magics.)
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
        // Mach-O 32/64-bit, big- and little-endian (arm64 native is 0xCFFAEDFE).
        0xFEED_FACE | 0xFEED_FACF | 0xCEFA_EDFE | 0xCFFA_EDFE
        // Universal ("fat") binaries, 32- and 64-bit.
        | 0xCAFE_BABE | 0xBEBA_FECA | 0xCAFE_BABF | 0xBFBA_FECA
    )
}

/// Pull the version out of `codex --version` output, in the same strict shape
/// `codex::parse_codex_version` accepts: exactly one non-empty line, either
/// `codex-cli <version>` or a bare `<version>`, version starting with a digit.
/// Anything else is rejected rather than guessed.
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

/// The most a `--version` probe may hand back on one pipe. The answer is a single
/// short line, so this is three orders of magnitude of headroom — its job is to
/// stop a wrapper that streams (a progress bar, a download log, `/dev/zero`) from
/// making the reader thread allocate without limit, which `read_to_end` on its own
/// will happily do long after the caller has given up waiting for it.
const PROBE_OUTPUT_LIMIT: u64 = 64 * 1024;

/// Bounded budget for reaping the probe after its group is SIGKILLed, mirroring the
/// host's own `kill_and_reap`. A blocking `wait()` here would reintroduce the very
/// hazard this helper exists to remove.
const PROBE_REAP_BUDGET: Duration = Duration::from_secs(2);

/// Drain one of the probe's pipes on its OWN thread, handing the result back
/// through a channel.
///
/// The read itself cannot be given a deadline — a `read()` on a pipe blocks until
/// EOF, and EOF only arrives when the last writer closes it, which a *descendant*
/// that inherited the fd can defer forever. So the read is moved off the gate's
/// thread entirely and the caller bounds the *wait* with `recv_timeout`.
///
/// Three properties the obvious version does not have:
///
///   * **The read is bounded** ([`PROBE_OUTPUT_LIMIT`]). A stranded thread outlives
///     the caller's `recv_timeout`, so an unbounded `read_to_end` against a chatty
///     wrapper keeps allocating inside a test process that has already moved on.
///   * **Hitting the limit is a FAILURE, not an end of output.** This is the subtle
///     half. `Read::take(N)` reports EOF once N bytes are consumed, so a plain
///     `take(LIMIT)` hands back a 64 KiB *prefix* that is indistinguishable from a
///     complete answer — and a wrapper whose first line happens to read
///     `codex-cli 0.147.0` would then PASS the premise check while the rest of its
///     output, and whatever forked to produce it, went unexamined. So the reader
///     asks for `LIMIT + 1`: if that extra byte materialises, the output overflowed
///     and the probe is rejected rather than parsed.
///   * **A read error is reported, not discarded.** `let _ = read_to_end(..)` keeps
///     whatever bytes arrived before the failure, and the caller cannot tell that
///     from a complete answer — so a truncated version string could parse and pass.
///     Partial output must never become success, so the error is sent instead.
fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::sync::mpsc::Receiver<Result<Vec<u8>, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = match pipe {
            Some(pipe) => {
                let mut buf = Vec::new();
                // LIMIT + 1: the extra byte exists solely so its arrival can be
                // detected. Reading exactly LIMIT could never tell "the output ended
                // here" from "the ceiling was reached here".
                let mut bounded = std::io::Read::take(pipe, PROBE_OUTPUT_LIMIT + 1);
                match std::io::Read::read_to_end(&mut bounded, &mut buf) {
                    Ok(_) if buf.len() as u64 > PROBE_OUTPUT_LIMIT => Err(format!(
                        "it wrote more than {PROBE_OUTPUT_LIMIT} bytes. A version answer is one \
                         short line, so this is a wrapper streaming something else — and a \
                         valid-looking prefix of a flood is not an answer, it is exactly the \
                         vacuous pass this premise check exists to prevent"
                    )),
                    Ok(_) => Ok(buf),
                    Err(e) => Err(format!(
                        "the read failed part-way ({e}). A partial read is not a shorter \
                         answer — it is no answer, and must not be parsed as one"
                    )),
                }
            }
            // A `piped()` handle is always `Some` straight after spawn; if that ever
            // changed, "no pipe" is no output rather than invented success.
            None => Ok(Vec::new()),
        };
        let _ = tx.send(outcome);
    });
    rx
}

/// What is known about the probe's leader process — deliberately three-valued.
///
/// Two states would force `try_wait`'s error case to be filed as one of "reaped" or
/// "unreaped", and both filings are wrong: an `Err` proves only that *that call*
/// collected no status. Calling it "unreaped" would then license a `kill(-pgid)` on
/// the strength of a failed syscall, which is precisely the numeric-pgid-on-
/// uncertain-knowledge hazard the guard exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaderState {
    /// No status has been collected, and every collection attempt so far answered
    /// cleanly. The leader is therefore still in the process table — running or an
    /// unreaped zombie — which is what keeps its pid, and the pgid equal to it,
    /// allocated. **This is the only state in which a group kill is warranted.**
    Unreaped,
    /// Its status was collected. The pid, and the pgid derived from it, may now be
    /// recycled: no numeric group kill from here on.
    Reaped,
    /// A `try_wait` errored, so the leader's state is genuinely unknown. Treated
    /// like `Reaped` for kill purposes — never a numeric pgid on uncertain
    /// knowledge — leaving only cleanup by positively observed identity, where the
    /// holder has such a thing to observe (see [`Probe`], which does not).
    Unknown,
}

/// The `--version` probe child, carrying the same leader-state discipline as the
/// harness `Drop`s — and here it is load-bearing rather than merely careful.
///
/// The probe is spawned into **its own process group**, because the thing that has
/// to be stopped on the interesting failure path is not the probe itself but a
/// *descendant* it forked which inherited the pipes. `child.kill()` cannot reach
/// that descendant; `kill(-pgid, SIGKILL)` can.
///
/// **Where this differs from the harnesses, and it is a real gap:** `Host` and
/// `PtyHost` fall back to a tagged-pid scan when their leader state is not
/// `Unreaped`, because every process in those sessions carries the run-dir path in
/// its argv. A version probe has no such tag — it is `codex --version`, whose argv
/// says nothing session-specific — so for [`LeaderState::Unknown`] there is no
/// fallback at all: the group kill is skipped (correctly, since the pgid is backed
/// by no knowledge) and nothing else is attempted. `Unknown` requires a `try_wait`
/// to have errored, which needs a broken process table rather than a misbehaving
/// probe, so this is accepted rather than solved — but it is a hole, not a covered
/// case, and inventing a tag to scan for would be more machinery than the residual
/// risk justifies.
struct Probe {
    child: Child,
    leader: LeaderState,
}

impl Probe {
    /// SIGKILL the probe's whole process group — best-effort, errors ignored.
    ///
    /// Guarded on [`LeaderState::Unreaped`], which is not a hopeful check but a
    /// provable one: an unreaped leader (live process or zombie) is still in the
    /// process table, so its pid — and therefore this pgid — cannot have been handed
    /// to anything else. [`codex_version_bounded`] is ordered so that every call
    /// site reaches this while the state still holds.
    fn kill_group(&mut self) {
        if self.leader == LeaderState::Unreaped {
            kill_pid(-(self.child.id() as i32), "-KILL");
            let _ = self.child.kill();
        }
    }

    /// Poll for the leader's status until `deadline`. Deliberately not `wait()`,
    /// which is unbounded: a probe that ignores SIGKILL (uninterruptible in a
    /// syscall) must cost the budget, not the suite.
    fn reap_bounded(&mut self, deadline: Instant) -> Option<std::process::ExitStatus> {
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.leader = LeaderState::Reaped;
                    return Some(status);
                }
                Ok(None) => {}
                Err(_) => {
                    // Not "still unreaped" — unknown. Downgrading here is what stops
                    // a later group kill from acting on a failed syscall.
                    self.leader = LeaderState::Unknown;
                    return None;
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The one cleanup shape, used on every path: kill the group, then reap under a
    /// bound. Never the other order — see [`Probe::kill_group`].
    ///
    /// Idempotent, which is what lets [`Probe::drop`] call it unconditionally: once
    /// the leader is `Reaped` the kill is skipped and `try_wait` replays the cached
    /// status.
    fn cleanup(&mut self) -> Option<std::process::ExitStatus> {
        self.kill_group();
        self.reap_bounded(Instant::now() + PROBE_REAP_BUDGET)
    }
}

impl Drop for Probe {
    /// The net beneath the explicit cleanup, so "every path kills the group" is a
    /// structural property rather than a promise about the code as currently
    /// written.
    ///
    /// The explicit calls in [`codex_version_bounded`] cover every `return`, but a
    /// `return` is not the only way out: `thread::spawn` can fail after the child
    /// exists, `format!` can abort on allocation failure, and any future edit can
    /// add an early exit. `std::process::Child` has no `kill_on_drop`, so without
    /// this the probe's group would simply survive. Five lines make the sentence
    /// true instead of aspirational.
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Run `codex --version` under a wall-clock budget and return the parsed version.
///
/// **Every wait in here is bounded and every path — success included — ATTEMPTS to
/// kill the probe's whole process group.** Both halves are stated precisely on
/// purpose. The total time this can take is `budget` (the caller's, spent waiting
/// for output) **plus [`PROBE_REAP_BUDGET`]** (spent reaping after the SIGKILL), not
/// `budget` alone. And the kill is an attempt: `kill(2)` failures are ignored, and
/// [`LeaderState::Unknown`] skips the kill entirely, so what the helper guarantees
/// is that it always tries — never that the group is provably dead afterwards.
///
/// Three hazards, none of which the obvious `output()` survives:
///
///   * **The exit.** A wrapper that stalls (on a network fetch, or on a tty) never
///     exits, so the status is collected by a polled `try_wait` against a deadline.
///   * **The output.** `wait_with_output()` after a successful `try_wait` looks
///     safe — the child is gone, so "its pipes hold at most a buffer's worth" — but
///     that reasoning fails whenever the child forked: a surviving descendant
///     inherits the write ends, EOF never comes, and the read blocks forever *past*
///     the deadline. So the pipes are drained by threads and collected with
///     `recv_timeout` under the same deadline.
///   * **The descendant.** Having read the pipes, the helper must actually stop
///     whatever else the probe left running — which is not the probe itself, since
///     it may well have exited already. Hence the own-process-group spawn and the
///     group kill.
///
/// **The ordering is the correctness argument, not a style choice.** Pipes are
/// collected FIRST, the group is killed SECOND, and the leader is reaped LAST, so
/// that every kill happens while the leader is still unreaped and its pgid is
/// therefore provably not recycled. Reaping first — the natural way to write this —
/// would leave the interesting paths holding a stale group number and unable to
/// kill anything with it safely.
///
/// **The kill is unconditional, not a failure-path measure.** A version probe is a
/// premise check that runs before a live gate; leaving a forked descendant behind on
/// the *success* path would seed the very process-table residue the gates then scan
/// for. Killing on every path also collapses the cleanup to one shape, so a later
/// failure branch cannot be added that forgets it — the kill has already been
/// attempted, and the leader reaped or given up on, before any status or version is
/// validated. [`Probe::drop`] is the net under the explicit calls.
fn codex_version_bounded(bin: &Path, budget: Duration) -> Result<String, String> {
    use std::os::unix::process::CommandExt;
    let child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group, so a descendant that inherits the pipes can be reached.
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawning `{} --version`: {e}", bin.display()))?;
    let mut probe = Probe {
        child,
        leader: LeaderState::Unreaped,
    };

    // Started before any wait, so a chatty probe cannot deadlock by filling a pipe
    // buffer while nobody is draining it.
    let stdout_rx = spawn_pipe_reader(probe.child.stdout.take());
    let stderr_rx = spawn_pipe_reader(probe.child.stderr.take());
    let deadline = Instant::now() + budget;

    let collect = |rx: &std::sync::mpsc::Receiver<Result<Vec<u8>, String>>, which: &str| {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(why)) => Err(format!("`{} --version` on {which}: {why}", bin.display())),
            Err(_) => Err(format!(
                "`{} --version` did not close its {which} within {budget:?}; the standalone \
                 native codex answers and exits immediately, so this is not it (either it \
                 never answered, or something it forked inherited the pipe)",
                bin.display()
            )),
        }
    };

    let stdout = match collect(&stdout_rx, "stdout") {
        Ok(bytes) => bytes,
        Err(why) => {
            probe.cleanup();
            return Err(why);
        }
    };
    let stderr = match collect(&stderr_rx, "stderr") {
        Ok(bytes) => bytes,
        Err(why) => {
            probe.cleanup();
            return Err(why);
        }
    };

    // The uniform cleanup, reached on the success path too. Everything below this
    // line is validation of bytes already in hand, so no branch under it needs to
    // remember to clean up — the kill has been attempted and the leader reaped or
    // given up on before any of it runs.
    let Some(status) = probe.cleanup() else {
        return Err(format!(
            "`{} --version` could not be reaped within {PROBE_REAP_BUDGET:?} of its process \
             group being SIGKILLed",
            bin.display()
        ));
    };

    if !status.success() {
        return Err(format!(
            "`{} --version` did not exit successfully ({status}): {}. (The probe's process \
             group is SIGKILLed before its status is collected, so a `signal: 9` here means \
             it had not exited on its own by then.)",
            bin.display(),
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&stdout).to_string();
    parse_codex_version(&text).ok_or_else(|| {
        format!(
            "`{} --version` did not print a recognisable version: {text:?}",
            bin.display()
        )
    })
}

/// The version probe's boundedness, proven **deterministically** — no codex, no
/// `CC_CODEX_LIVE`, so this runs in the normal `cargo test` suite and is the one
/// non-live test in this file.
///
/// It exists because the dangerous case is invisible in a live run: against the
/// real codex the probe exits and its pipes close in the same breath, so an
/// unbounded collection *looks* fine forever. The failure only shows up against a
/// binary that forks — exactly the `npm` shim / shell wrapper class this whole gate
/// exists to reject — where a surviving descendant inherits the write ends, EOF
/// never arrives, and a `wait_with_output()` after a successful `try_wait` blocks
/// past the deadline with no timeout above it. The fake below is that shape in four
/// lines: print a plausible version, leave a `sleep` holding stdout, exit 0.
///
/// It gates **both** halves of the fix, which are separable and were separate bugs:
/// that the helper RETURNS under its budget, and that it then actually STOPS the
/// descendant holding the pipes. The second is why the sleeper's death is asserted
/// here rather than swept up by the test — cleaning up by hand would hide a helper
/// that returns promptly and leaks a process every time it is called.
#[test]
fn the_version_probe_is_bounded_even_when_a_descendant_holds_the_pipes() {
    use std::os::unix::fs::PermissionsExt;
    let dir = ShortTmpDir::new("probe").expect("mk probe dir");
    let pidfile = dir.path.join("sleeper.pid");

    // The well-behaved shape first, so a probe that simply refused everything
    // could not pass this test.
    let good = dir.path.join("good-codex");
    std::fs::write(&good, "#!/bin/sh\necho 'codex-cli 0.147.0'\n").expect("write fake");
    std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    assert_eq!(
        codex_version_bounded(&good, Duration::from_secs(10)),
        Ok("0.147.0".to_string()),
        "a well-behaved probe must still be read normally"
    );

    // The OVERFLOW shape, and note carefully where the flood goes. Putting it on
    // stdout would prove nothing: the extra lines make `parse_codex_version` fail
    // anyway, so the test would pass with or without the ceiling check. On stderr
    // the two behaviours separate cleanly — stdout carries exactly one valid line
    // and the exit is 0, so a probe that treats "limit reached" as EOF returns
    // Ok("0.147.0") and the wrapper sails through the premise check with 160 KiB of
    // unexamined output behind it. That is the vacuous pass; hitting the ceiling
    // must be a refusal.
    let flooder = dir.path.join("flooding-codex");
    std::fs::write(
        &flooder,
        // The redirect goes on the HEAD side. `yes ... 1>&2 | head` would send yes's
        // output straight to stderr, leaving head with an immediate EOF — so the
        // flood would be unbounded (yes writing until the reader closes the fd)
        // rather than the ~160 KiB this comment claims. Bounded matters: a fixture
        // that only stops because the code under test stopped reading cannot be
        // evidence about the code under test.
        "#!/bin/sh\necho 'codex-cli 0.147.0'\nyes aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa | \
         head -n 5000 1>&2\nexit 0\n",
    )
    .expect("write flooding fake");
    std::fs::set_permissions(&flooder, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    match codex_version_bounded(&flooder, Duration::from_secs(10)) {
        Err(why) => assert!(
            why.contains("wrote more than") && why.contains("stderr"),
            "the refusal must name the overflow and the pipe it happened on: {why}"
        ),
        Ok(version) => panic!(
            "a probe that flooded past the {PROBE_OUTPUT_LIMIT}-byte ceiling reported \
             {version:?} — a valid-looking prefix of a flood was parsed as the answer"
        ),
    }

    // The forking shape: exits immediately, but a descendant holds both pipes for
    // 30s — vastly longer than the 2s budget, so "did it wait for EOF?" is not a
    // question of timing slack.
    let forker = dir.path.join("forking-codex");
    std::fs::write(
        &forker,
        format!(
            "#!/bin/sh\necho 'codex-cli 0.147.0'\nsleep 30 &\necho $! > {}\nexit 0\n",
            pidfile.display()
        ),
    )
    .expect("write forking fake");
    std::fs::set_permissions(&forker, std::fs::Permissions::from_mode(0o700)).expect("chmod");

    let budget = Duration::from_secs(2);
    let started = Instant::now();
    let result = codex_version_bounded(&forker, budget);
    let elapsed = started.elapsed();

    // The descendant's pid, written by the fake before it exited.
    let sleeper: Option<i32> = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|p| p.trim().parse().ok());

    // Did the helper's own group kill stop it? Polled, because the SIGKILL and the
    // process leaving the table are not the same instant.
    //
    // **No safety-net kill follows this**, deliberately. Once `!is_alive(pid)` has
    // been observed, that pid names nothing — and firing a SIGKILL at a number whose
    // process is gone is the exact stale-pid hazard this file spent two rounds
    // removing from the harness `Drop`s; a test that does it while asserting the
    // rule would be indefensible. The cleanup this test used to perform by hand is
    // now the helper's own unconditional group kill, which is what the assertion
    // below actually gates. If that assertion fails there IS a stray sleeper, and it
    // is 30 seconds from exiting on its own — the correct price for not writing a
    // kill this file argues against.
    let reaped = sleeper.is_some_and(|pid| wait_until(Duration::from_secs(5), || !is_alive(pid)));

    assert!(
        elapsed < Duration::from_secs(10),
        "the probe waited {elapsed:?} on a pipe a descendant held open — the collection \
         is not under the deadline"
    );
    match result {
        Err(why) => assert!(
            why.contains("did not close its stdout"),
            "the failure must name why it gave up: {why}"
        ),
        Ok(version) => panic!(
            "a probe whose output could not be collected within its budget must FAIL, \
             not report {version:?} — the whole point of the bound is that an \
             unaccountable answer is refused"
        ),
    }
    assert!(
        sleeper.is_some(),
        "the fake never recorded its descendant's pid — the test proved nothing about \
         cleanup"
    );
    assert!(
        reaped,
        "the probe returned in time but left the pipe-holding descendant ({sleeper:?}) \
         running: giving up on a wrapper is only half the job, and a gate that leaks a \
         process on every rejection is not a bounded probe"
    );
}

/// Whether `pid` is still in the process table, via signal 0 — no signal sent, just
/// the kernel's existence check.
///
/// **Fails closed**, like [`processes_referencing`] and for a sharper version of the
/// same reason. This function's `false` is the sole evidence for the descendant-
/// cleanup assertion, and the failure mode that would make `/bin/kill` unrunnable —
/// a broken PATH, a exhausted process table, a sandbox that blocks exec — is
/// plausibly the *same* failure that would stop the group kill from working. A
/// fail-open `unwrap_or(false)` would therefore report "the descendant is gone"
/// precisely when it most likely is not. Only a clean exit status decides; anything
/// else panics.
fn is_alive(pid: i32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run /bin/kill -0 to test for a process — a liveness check that cannot RUN must FAIL the test, never silently report 'it is gone'")
        .success()
}

/// The gate every test opens with.
///
/// `None` ⇒ skip, and only for the one honest reason: the operator did not ask for
/// a live run. Once `CC_CODEX_LIVE=1` IS set, everything else **panics** — a
/// deliberate live run that quietly degrades to a green no-op, or that validates
/// the host against the wrong binary, is worse than no gate at all, because it
/// reports coverage that never executed or that proved something else.
///
/// The two premises it establishes, at their real strength: the resolved file is a
/// **native** Mach-O executable, and it **reports** [`LIVE_CODEX_VERSION_PREFIX`]`x`
/// when asked. Neither is a proof that the binary is genuinely upstream codex.
fn live_gate() -> Option<PathBuf> {
    if std::env::var("CC_CODEX_LIVE").as_deref() != Ok("1") {
        eprintln!("SKIP live_codex_host: set CC_CODEX_LIVE=1 to run the live host test");
        return None;
    }
    let Some(codex) = resolve_codex() else {
        panic!(
            "CC_CODEX_LIVE=1 was set but no codex binary could be found \
             (~/.local/bin/codex or `which codex`). A demanded live run must FAIL rather \
             than pass vacuously — install codex or unset CC_CODEX_LIVE."
        )
    };

    // Premise 1: it is the standalone NATIVE executable, not a wrapper. A `#!`
    // script or a `.js` shim would spawn some other codex, so nothing this gate
    // then observes would be evidence about the binary it claims to test.
    assert!(
        is_native_executable(&codex),
        "CC_CODEX_LIVE=1 resolved {} but it is not a native executable (a `#!`-script \
         or `.js` wrapper). CodeConnect supports the standalone native codex only, and \
         a live gate that validated the host through a wrapper would be proving \
         something about a binary nobody can name — failing instead of passing vacuously.",
        codex.display()
    );

    // Premise 2: it REPORTS the pinned series — a self-report, taken at that
    // strength and no higher. Everything this chunk asserts — the reserved argv
    // grammar, the app-server's `--listen unix://` transport, the WS handshake the
    // broker answers on tui.sock — is grounded on 0.147. Against a binary reporting
    // a different series a pass would mean nothing and a failure would be misread
    // as a bug in the host.
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
         against {LIVE_CODEX_VERSION_PREFIX}x (the same series `src/codex.rs` pins). \
         Running it against a different codex would validate the host against the wrong \
         thing — install {LIVE_CODEX_VERSION_PREFIX}x or unset CC_CODEX_LIVE.",
        codex.display()
    );
    eprintln!(
        "live gate premise verified: {} is a native executable reporting codex {version}",
        codex.display()
    );
    Some(codex)
}

fn read_file(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| format!("<unreadable: {e}>"))
}

/// `(pid, command)` for every process whose full command line contains `tag`,
/// excluding this test process. `-ww` disables ps's column truncation so a tag
/// deep in a long argv is still matched.
///
/// This is a scan of the process table by argv, so what it proves is precisely:
/// **no visible process references `tag`**. It is not a proof of universal
/// descendant absence — a descendant that re-execs without the tag in its argv
/// would not be seen, and nothing here would catch it.
///
/// The group kill in [`PtyHost::drop`] is **not** the belt-and-braces for that, as
/// this comment used to claim. `script` `setsid`s the command it runs, so the
/// session — host, app-server, TUI — is in a different process group from the
/// wrapper; that kill reaches the wrapper side only, and the tag sweep is what
/// reaches the session side. A tagless session descendant is therefore covered by
/// neither, and is a genuine blind spot of this harness rather than a case handled
/// elsewhere. (See [`PtyHost::drop`], which describes the split correctly.)
///
/// **Fails closed.** Every leak assertion here reads an empty result as proof that
/// nothing was left behind, so a `ps` that could not be run, or that ran and
/// failed, panics rather than returning `Vec::new()` — a broken scan must never
/// green a leak assertion.
fn processes_referencing(tag: &str) -> Vec<(i32, String)> {
    let me = std::process::id() as i32;
    let out = Command::new("/bin/ps")
        .args(["-Axww", "-o", "pid=,command="])
        .output()
        .expect("run /bin/ps to scan the process table — a scan that cannot run must FAIL the live gate, never silently report 'nothing found'");
    assert!(
        out.status.success(),
        "/bin/ps exited {} — the process scan is the evidence for every leak \
         assertion here, so a failed scan fails the test: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, cmd) = line.split_once(char::is_whitespace)?;
            let pid: i32 = pid.trim().parse().ok()?;
            if pid != me && cmd.contains(tag) {
                Some((pid, cmd.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// Poll `path` until it exists and contains `needle`, up to `timeout`.
fn wait_for_log_contains(path: &Path, needle: &str, timeout: Duration) -> bool {
    wait_until(timeout, || {
        std::fs::read_to_string(path)
            .map(|s| s.contains(needle))
            .unwrap_or(false)
    })
}

/// Poll `cond` every 100ms until it returns true or `timeout` elapses.
fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Send `signal` to `pid` (or, for a negative `pid`, to that process GROUP).
/// Silent: on the cleanup paths the target is usually already gone, and "no such
/// process" is the expected, uninteresting case — not something to print over a
/// passing test's output.
fn kill_pid(pid: i32, signal: &str) {
    let _ = Command::new("/bin/kill")
        .args([signal, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ------------------------------------------------- the D7 admission the host runs
//
// Since 2e-2b the host presents itself to the D7 launch gate before it creates
// anything: it takes an exclusive `host_lease` on a `pending` launch record, or
// refuses and exits 75 having made nothing. That is a real gate, not a formality,
// so a harness that drives the host directly has to give it a launch to belong to.
//
// This writes the minimum admissible record by hand rather than through
// `codex_launch` (an integration test links the binary, not a library). The
// coordinator and custodian slots are set to **this test process**, which is
// genuinely alive, because admission requires both to be proven live — pointing
// them at a fabricated pid would be refused, correctly.

/// The launch identity a host harness presents to the gate.
struct Launch {
    home: PathBuf,
    uid: String,
    nonce: String,
}

impl Launch {
    /// Write a fresh admissible `pending` record under a private
    /// `CODECONNECT_HOME`, and return the identity to pass to the host.
    fn admissible(tag: &str) -> Launch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let home = short_tmp_path(&format!("{tag}home"));
        let uid = format!("01JQXV9K7B{:016X}", nanos as u64);
        let nonce = format!("{:016x}{:016x}", nanos as u64, std::process::id());
        let me = protocol::proc_identity::current_identity().expect("read our own identity");
        let boot = protocol::proc_identity::boot_identity().expect("read the boot identity");
        let now = protocol::proc_identity::monotonic_now_nanos().expect("read the monotonic clock");
        let identity = serde_json::json!({
            "pid": me.pid,
            "birth": { "start_sec": me.birth.start_sec, "start_usec": me.birth.start_usec },
        });
        let record = serde_json::json!({
            "schema": 1,
            "launch_nonce": nonce,
            "uid": uid,
            "session_name": "cc-host-harness",
            // Both guardians are THIS process: alive, so admission's
            // proven-live requirements are satisfied honestly.
            "coordinator": identity,
            "custodian": identity,
            "boot": { "boot_sec": boot.boot_sec, "boot_usec": boot.boot_usec },
            // Far enough out that a slow test machine cannot expire the launch
            // mid-run, which would surface as a confusing admission refusal.
            "deadline_monotonic_nanos": now + 600_000_000_000u64,
            "state": "Pending",
            "cleanup": "Pending",
            "new_session_indeterminate": false,
            "host_lease": serde_json::Value::Null,
            "pending_spawn": serde_json::Value::Null,
            "server_a": serde_json::Value::Null,
            "run_dir": serde_json::Value::Null,
            "children": [],
            "created_ms": 0,
        });
        let dir = home.join("sessions").join(&uid);
        std::fs::create_dir_all(&dir).expect("create the session dir");
        std::fs::write(
            dir.join("launch.json"),
            serde_json::to_vec_pretty(&record).expect("serialize the record"),
        )
        .expect("write the launch record");
        Launch { home, uid, nonce }
    }
}

impl Drop for Launch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// The host, spawned under a PTY via `script(1)` so the real codex TUI sees a tty.
///
/// Its stdin is held open (never dropped) so an immediate stdin-EOF cannot kill
/// the TUI before the handshake is observed.
///
/// **`script` is put in its own process group** (`process_group(0)`, so its pgid is
/// its pid) and Drop SIGKILLs the whole group `-pgid` before reaping. `script`
/// itself `setsid`s the command it runs, so the group kill catches the wrapper and
/// anything that stayed in its group; the run-dir tag sweep that follows catches
/// the session side. Between them a failing assertion cannot leave the PTY, the
/// host, or either codex child running.
struct PtyHost {
    child: Child,
    /// The run dir path — the argv tag every process in this session carries.
    tag: String,
    /// What is known about the leader (`script`) — three-valued, and gating the
    /// group kill in [`PtyHost::drop`].
    ///
    /// `kill(-pgid)` names a group by NUMBER, which is only meaningfully ours while
    /// the leader is [`LeaderState::Unreaped`]; after a reap it is a stale token
    /// that could address an unrelated group, and after a failed `try_wait` it is a
    /// number backed by no knowledge at all. Both live gates call
    /// [`PtyHost::wait_code`], so `Reaped` is the normal case — after it, cleanup
    /// goes only through the tagged-pid scan, whose warrant is narrower but not
    /// race-free either.
    leader: LeaderState,
    _stdin: std::process::ChildStdin,
}

impl PtyHost {
    fn spawn(codeconnect: &str, codex: &Path, run: &str, home: &str, launch: &Launch) -> PtyHost {
        use std::os::unix::process::CommandExt;
        // BSD `script`: `script [-q] file [command ...]` runs the command directly
        // (no shell) with a PTY as its stdio. `/dev/null` discards the typescript.
        //
        // Round-2 P4: the host is spawned WITHOUT a coordinator, so it inherits this
        // process's cwd — and that is the cwd the app-server will resolve and report. The
        // launch cwd must therefore be this directory, canonicalized here exactly as the
        // real coordinator canonicalizes it before writing the host argv.
        let launch_cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
            .expect("canonical cwd")
            .to_str()
            .expect("utf-8 cwd")
            .to_string();
        let mut child = Command::new("/usr/bin/script")
            .args([
                "-q",
                "/dev/null",
                codeconnect,
                "internal-codex-host",
                // The D7 launch identity the host presents to the gate before it
                // creates anything (2e-2b).
                "--uid",
                &launch.uid,
                "--nonce",
                &launch.nonce,
                "--tmux-socket",
                "/tmp/cc-host-harness-no-server.sock",
                "--codex",
                codex.to_str().expect("codex path is utf-8"),
                "--run-dir",
                run,
                "--codex-home",
                home,
                // The host applies no policy default: every fingerprint dimension
                // is required and must be passed explicitly.
                //
                // **`on-request`, because that is what the real TUI asserts.** A
                // codex 0.147 `codex --remote` sends `approvalPolicy:"on-request"`
                // on its `thread/start`, and the broker's fingerprint validator
                // refuses any present ownership value that disagrees with the
                // launch fingerprint. A harness that launches with `untrusted`
                // therefore gets
                //
                //   Tui: refuse->synthetic error (thread/start: fingerprint refused
                //   (Conflict): params.approvalPolicy: "on-request" but fingerprint
                //   is "untrusted")
                //
                // the TUI exits fatally, and the session dies about two seconds in
                // with no thread ever created. The broker is behaving correctly;
                // the fingerprint it was handed was the wrong one.
                "--approval-policy",
                "on-request",
                "--approvals-reviewer",
                "user",
                "--sandbox",
                "read-only",
                "--hooks-enabled",
                "true",
                // Round-2 P4: the canonical launch cwd (the workspace anchor).
                "--launch-cwd",
                launch_cwd.as_str(),
            ])
            .env("TERM", "xterm-256color")
            .env("CODECONNECT_HOME", &launch.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Own process group, so Drop can kill the group rather than one pid.
            .process_group(0)
            .spawn()
            .expect("spawn `script` host under a PTY");
        let stdin = child.stdin.take().expect("piped stdin");
        PtyHost {
            child,
            tag: run.to_string(),
            leader: LeaderState::Unreaped,
            _stdin: stdin,
        }
    }

    /// The pid of the `internal-codex-host` process itself (not the `script`
    /// wrapper that also names it in its argv).
    fn host_pids(&self) -> Vec<i32> {
        processes_referencing(&self.tag)
            .into_iter()
            .filter(|(_, cmd)| {
                cmd.contains("internal-codex-host") && !cmd.contains("/usr/bin/script")
            })
            .map(|(pid, _)| pid)
            .collect()
    }

    /// Wait (bounded) for `script` to exit and return its code. BSD `script`
    /// propagates the child's exit status (probed: `script -q /dev/null sh -c
    /// 'exit 70'` ⇒ 70), so this IS the host's exit code.
    ///
    /// Collecting a status here IS the reap, so it records that fact: the leader's
    /// pgid becomes reusable from this point and [`PtyHost::drop`] must stop
    /// group-killing by that number.
    fn wait_code(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.leader = LeaderState::Reaped;
                    return status.code();
                }
                Ok(None) => {}
                // An Err proves only that THIS call collected no status — not that
                // the leader is still unreaped. `Unknown`, so Drop falls back to
                // tagged-pid cleanup rather than acting on a failed syscall.
                Err(_) => {
                    self.leader = LeaderState::Unknown;
                    return None;
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for PtyHost {
    fn drop(&mut self) {
        // The whole group `script` leads — but ONLY while that group number is
        // provably still ours, which is exactly [`LeaderState::Unreaped`]. Once the
        // leader has been reaped the kernel may hand its pid (and hence this pgid)
        // to something else; and after a failed `try_wait` nothing is known at all,
        // which is no better a warrant than a guess. Either way a numeric group kill
        // could land on an unrelated group, so both non-`Unreaped` states fall
        // through to the tagged-pid sweep below. The group kill therefore covers
        // exactly the case it exists for — a failed assertion before `wait_code`,
        // with the PTY wrapper still alive.
        if self.leader == LeaderState::Unreaped {
            let pgid = self.child.id() as i32;
            kill_pid(-pgid, "-KILL");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // `script` setsids the command it runs, so the session side lives in a
        // different group anyway: sweep it by the run-dir tag every process carries.
        // This stays unconditional because it kills only pids observed carrying this
        // run dir in their argv moments earlier — a much stronger warrant than a
        // bare pgid, though still not a guarantee: a tagged process can exit between
        // the `ps` and the `/bin/kill` and have its pid recycled in that window.
        // That race is inherent to ps-based cleanup and is accepted rather than
        // pretended away. Uses the tolerant scan, not
        // `processes_referencing`, which now panics on a failed scan — panicking
        // inside a Drop that runs during an assertion's unwind would abort the
        // binary and hide the real failure.
        for pid in tagged_pids_best_effort(&self.tag) {
            kill_pid(pid, "-KILL");
        }
    }
}

/// The tagged pids, or an empty list if `ps` could not be run.
///
/// The deliberately tolerant twin of [`processes_referencing`]: used only by
/// [`PtyHost::drop`] for cleanup, where an empty answer costs nothing and a panic
/// would abort a test binary mid-unwind. Nothing asserts on its result.
fn tagged_pids_best_effort(tag: &str) -> Vec<i32> {
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

/// Bring a live session up and return the PTY host once the broker has proven a
/// real codex client attached through it. Shared by both live gates.
fn live_session_up(codex: &Path, run: &HostRunDir, home: &ShortTmpDir, launch: &Launch) -> PtyHost {
    let broker_log = run.join("broker.log");
    let as_stderr = run.join("appserver.stderr.log");
    for s in [
        &run.join("as.sock"),
        &run.join("tui.sock"),
        &run.join("ccd.sock"),
    ] {
        assert_sun_len(s);
    }

    // The freshly built `codeconnect` binary this test crate was compiled against.
    let codeconnect = env!("CARGO_BIN_EXE_codeconnect");
    let host = PtyHost::spawn(codeconnect, codex, run.as_str(), home.as_str(), launch);

    // The host launches `codex --remote unix://<tui.sock>`. `Tui: forward (...)`
    // appears in broker.log ONLY when a real client completed the WS-over-UDS
    // handshake on tui.sock AND a request off that leg was classified Forward and
    // relayed upstream — exactly the handshake compatibility this chunk must prove
    // live. (The forwarded request is necessarily `initialize`, since the
    // app-server rejects everything before it — but the broker's Forward note is
    // generic, so that inference is reasoning, not evidence, and is not asserted.)
    let forwarded = wait_for_log_contains(&broker_log, "Tui: forward", Duration::from_secs(25));
    if !forwarded {
        let broker = read_file(&broker_log);
        let appserver = read_file(&as_stderr);
        // Kill before panicking so the PTY child does not outlive the test.
        drop(host);
        panic!(
            "STOP-AND-AMEND: `codex --remote` did not forward through the broker's tui.sock \
             within 25s — a real codex client may not handshake the broker (WS headers/path/\
             subprotocol mismatch).\n--- broker.log ---\n{broker}\n--- appserver.stderr ---\n{appserver}"
        );
    }
    host
}

/// The CRUX gate: a real codex TUI attaches through the broker, and a signalled
/// host leaves nothing behind.
#[test]
#[ignore = "live: needs a real codex; run with CC_CODEX_LIVE=1 -- --ignored"]
fn tui_attaches_through_broker_and_teardown_leaves_nothing() {
    let Some(codex) = live_gate() else { return };

    let run = HostRunDir::new("run");
    let home = ShortTmpDir::new("home").expect("mk codex home");
    let launch = Launch::admissible("live");
    let mut host = live_session_up(&codex, &run, &home, &launch);

    println!(
        "CRUX PASS — a real codex client handshaked tui.sock and a request was forwarded \
         through the broker. broker.log:\n{}",
        read_file(&run.join("broker.log"))
    );

    // --- Teardown: signal the host, expect no leaks --------------------------
    let host_pids = host.host_pids();
    assert!(
        !host_pids.is_empty(),
        "the internal-codex-host process should be running before teardown"
    );
    println!("signalling host pids {host_pids:?} with SIGTERM");
    for pid in &host_pids {
        kill_pid(*pid, "-TERM");
    }

    // Everything referencing the run dir (host, app-server, codex TUI, and the
    // `script` wrapper) must be gone once teardown completes.
    let tag = run.as_str();
    let clean = wait_until(Duration::from_secs(15), || {
        processes_referencing(tag).is_empty()
    });
    let leftover = processes_referencing(tag);
    assert!(
        clean && leftover.is_empty(),
        "processes still reference the run dir after teardown (leak): {leftover:?}"
    );
    // The host creates AND removes its whole run dir, sockets and logs together.
    assert!(
        !run.path.exists(),
        "the host did not remove the run dir it created: {}",
        run.path.display()
    );
    let _ = host.wait_code(Duration::from_secs(5));
    println!("TEARDOWN PASS — no leaked processes, run dir removed");
    println!("PASS tui_attaches_through_broker_and_teardown_leaves_nothing");
}

/// The fatal path against the REAL binaries: kill the live `codex app-server`
/// under an attached TUI. The host must call the session over — exit 70 — and stop
/// the real TUI rather than leave it talking to a dead upstream, which the harness
/// observes as: no visible process references the TUI's `--remote` marker any more.
#[test]
#[ignore = "live: needs a real codex; run with CC_CODEX_LIVE=1 -- --ignored"]
fn live_app_server_death_is_session_fatal() {
    let Some(codex) = live_gate() else { return };

    let run = HostRunDir::new("fatal");
    let home = ShortTmpDir::new("fatalhome").expect("mk codex home");
    let launch = Launch::admissible("live");
    let mut host = live_session_up(&codex, &run, &home, &launch);
    let tag = run.as_str().to_string();

    // The real app-server, identified by the `--listen unix://<run>/as.sock` it
    // was given, and the real TUI by its `--remote unix://<run>/tui.sock`.
    let as_marker = format!("app-server --listen unix://{tag}/as.sock");
    let tui_marker = format!("--remote unix://{tag}/tui.sock");
    let as_pids: Vec<i32> = processes_referencing(&as_marker)
        .into_iter()
        .map(|(pid, _)| pid)
        .collect();
    let tui_pids: Vec<i32> = processes_referencing(&tui_marker)
        .into_iter()
        .map(|(pid, _)| pid)
        .collect();
    assert!(
        !as_pids.is_empty(),
        "the real app-server should be running under a live session"
    );
    assert!(
        !tui_pids.is_empty(),
        "the real codex TUI should be running under a live session"
    );
    println!("live session up: app-server={as_pids:?} tui={tui_pids:?}; SIGKILLing the app-server");
    for pid in &as_pids {
        kill_pid(*pid, "-KILL");
    }

    // 1. The session is fatal, not a clean TUI exit.
    let code = host.wait_code(Duration::from_secs(30));
    assert_eq!(
        code,
        Some(EX_HOST_FATAL),
        "a dead app-server must end the session with {EX_HOST_FATAL}, got {code:?}. \
         broker.log:\n{}",
        read_file(&run.join("broker.log"))
    );

    // 2. No visible process references the TUI marker any more. An orphaned TUI
    // would still carry `--remote unix://<run>/tui.sock` in its argv and be seen.
    let tui_gone = wait_until(Duration::from_secs(15), || {
        processes_referencing(&tui_marker).is_empty()
    });
    assert!(
        tui_gone,
        "a process still references the TUI marker — the real TUI outlived its dead \
         upstream (leak): {:?}",
        processes_referencing(&tui_marker)
    );

    // 3. Nothing visible references the run dir, and the dir itself is gone.
    let clean = wait_until(Duration::from_secs(10), || {
        processes_referencing(&tag).is_empty()
    });
    assert!(
        clean,
        "processes still reference the run dir after the fatal path: {:?}",
        processes_referencing(&tag)
    );
    assert!(
        !run.path.exists(),
        "the host did not remove the run dir on the fatal path: {}",
        run.path.display()
    );
    println!(
        "PASS live_app_server_death_is_session_fatal (exit 70, no visible process \
         references the TUI marker, run dir removed)"
    );
}
