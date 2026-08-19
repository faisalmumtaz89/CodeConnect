//! The **inert exec gate** (D6) — no real child execs its target before its
//! `{spawn_nonce, role, pid, birth_identity, pgid}` is fsynced.
//!
//! ## The window this closes
//!
//! A launch actor that just `fork`+`exec`s a child has a gap: the child is
//! already running its target (and can mutate, spawn, escape) before the parent
//! has durably recorded *what it spawned*. If the parent then dies, nothing
//! knows that child exists — it is an unrecorded escapee. D6 forbids that
//! ordering: a child is spawned **inert**, its identity is fsynced, and only
//! then is it released to `execve` its real target.
//!
//! ## Mechanism (owner ↔ gate)
//!
//! 1. Owner creates a private `socketpair`; it keeps the only release endpoint.
//! 2. Owner spawns the resolved CodeConnect executable as `internal-exec-gate`,
//!    handing it the other endpoint (dup'd to a fixed fd) and the target argv.
//! 3. The gate puts itself in its **own process group**, sends `READY <pid>
//!    <pgid>` and then **blocks** reading the socket — it mutates nothing and
//!    spawns nothing.
//! 4. Owner reads `READY`, validates the child's kernel birth identity and pgid,
//!    **fsyncs the identity** (the `on_ready` callback — where the caller writes
//!    the `ChildEntry` into the launch record), rechecks, and sends a one-use
//!    `GO <nonce>`.
//! 5. Only `GO` with the matching nonce permits `execvp` (the PID is preserved,
//!    so the recorded identity *is* the target's identity). A malformed token, a
//!    wrong nonce, EOF (**owner death** closes the only writer ⇒ EOF), a read
//!    error, or a timeout all `_exit` the gate **without** exec.
//!
//! The tmux-started `codex-host` cannot inherit this pipe, so it is **its own
//! gate** (handled in the coordinator/host path, D6/D7): before creating
//! anything it validates the launch record and self-records.
//!
//! Owner-side ordering is the load-bearing invariant: `on_ready` (the fsync)
//! runs strictly **before** the GO byte is written, and if it fails the GO is
//! never written — so there is no path where a target-side effect precedes a
//! durable identity.

use anyhow::{bail, Context, Result};
use protocol::proc_identity::{read_birth_identity, read_pgid, BirthIdentity};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The fixed fd the gate reads its release socket from, after the owner dup's
/// the child endpoint onto it. 3 is the first fd past stdio.
const GATE_FD: RawFd = 3;

/// How long the owner waits for the gate to say `READY` before giving up (and
/// never sending GO). Bounded so a wedged gate cannot hang a launch.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the **gate** blocks for `GO` before it `_exit`s without exec
/// (finding 5). A gate whose owner never releases it must not linger forever.
const GO_TIMEOUT: Duration = Duration::from_secs(30);

/// A test-only env override for [`GO_TIMEOUT`], in milliseconds, so the
/// gate-timeout⇒`_exit` path can be exercised in well under a second instead of
/// waiting the 30-second production default. Unset in production ⇒ [`GO_TIMEOUT`]
/// is used unchanged, so real launches behave identically.
const GO_TIMEOUT_ENV: &str = "CC_GATE_GO_TIMEOUT_MS";

/// The effective GO timeout: the env override if present and parseable, else the
/// production [`GO_TIMEOUT`].
fn go_timeout() -> Duration {
    std::env::var(GO_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(GO_TIMEOUT)
}

/// How long the owner waits, **after** `GO`, for the target to confirm it has
/// execed and started (the readiness fence, finding 5). Only after this does
/// `launch_gated` report success — so "custodian armed before tmux" is real, not
/// merely "GO written".
const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// The env var carrying the gate socket fd into the released target, so a
/// CodeConnect child can send its readiness ACK back over it.
pub const ACK_FD_ENV: &str = "CC_GATE_ACK_FD";

/// What the gate reported about itself, validated against the kernel by the
/// owner before it fsyncs and releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateReady {
    pub pid: i32,
    pub pgid: i32,
    pub birth: BirthIdentity,
}

/// The durable spawn intent the owner fsyncs **before** it creates the socket or
/// forks the gate (finding 4): a spawn is attributable to a nonce+role+argv-hash
/// even if the owner dies before the child's identity is recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnIntent {
    pub nonce: String,
    pub role: String,
    pub argv_hash: String,
}

/// The owner's description of the child it wants to bring up inertly.
pub struct GateSpec {
    /// A short role tag (`app-server`, `tui`, `custodian`) recorded with the
    /// child. Not interpreted here.
    pub role: String,
    /// The one-use release nonce. The gate execs only on `GO <nonce>`.
    pub nonce: String,
    /// The CodeConnect executable to run as `internal-exec-gate` (normally
    /// `current_exe()`).
    pub gate_program: PathBuf,
    /// The real target the gate will `execvp` on release, plus its argv[0..].
    pub target_argv: Vec<String>,
    /// Environment entries to set on the target (added to the inherited env).
    pub target_envs: Vec<(String, String)>,
}

/// Bring a child up through the gate (D6, completed per Principle E).
///
/// Ordering, each step strictly before the next:
///   1. **`on_intent`** — the owner fsyncs `SpawnIntent{nonce,role,argv_hash}`
///      *before* the socket or the child exists (finding 4).
///   2. spawn the inert gate; read `READY`.
///   3. validate against the kernel: reported pid == spawned pid, reported
///      **pgid == kernel pgid**, reported birth == kernel birth (finding 5).
///   4. **`on_ready`** — the owner fsyncs the child identity (`ChildEntry` with
///      the nonce + argv_hash).
///   5. **re-read** the birth identity after the fsync; abort if it changed
///      (finding 5).
///   6. send one-use `GO`.
///   7. **readiness fence** — wait for the target's `ACK` (finding 5); only then
///      is the child *proven* to have execed and started. If `on_ready` fails,
///      GO is never sent, the gate EOFs and `_exit`s without exec.
pub fn launch_gated<FI, FR>(spec: GateSpec, on_intent: FI, on_ready: FR) -> Result<GateReady>
where
    FI: FnOnce(&SpawnIntent) -> Result<()>,
    FR: FnOnce(&GateReady, &SpawnIntent) -> Result<()>,
{
    if spec.target_argv.is_empty() {
        bail!("exec gate needs a target argv");
    }

    // Step 1: durable spawn intent BEFORE the socket or the child exists.
    let intent = SpawnIntent {
        nonce: spec.nonce.clone(),
        role: spec.role.clone(),
        argv_hash: crate::codex_launch::argv_hash(&spec.target_argv),
    };
    on_intent(&intent).context("fsyncing the spawn intent before spawning")?;

    // A private, connected pair. Owner keeps `owner_fd`; the gate gets `gate_fd`
    // dup'd onto GATE_FD. Neither is CLOEXEC on the gate side (it must survive
    // execve); the owner side is CLOEXEC so it never leaks into other spawns.
    let (owner_fd, gate_fd) = socketpair()?;

    let mut command = Command::new(&spec.gate_program);
    command
        .arg("internal-exec-gate")
        .arg("--gate-fd")
        .arg(GATE_FD.to_string())
        .arg("--nonce")
        .arg(&spec.nonce)
        .arg("--role")
        .arg(&spec.role)
        .arg("--")
        .args(&spec.target_argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in &spec.target_envs {
        command.env(k, v);
    }

    // Move the gate endpoint onto GATE_FD in the forked child, clearing CLOEXEC
    // so it survives the coming execve. dup2/fcntl are async-signal-safe.
    let raw_gate_fd = gate_fd.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(raw_gate_fd, GATE_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Clear CLOEXEC on GATE_FD (dup2 target starts without it, but be
            // explicit) so the gate binary can read it after execve.
            let flags = libc::fcntl(GATE_FD, libc::F_GETFD);
            if flags < 0 || libc::fcntl(GATE_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command
        .spawn()
        .with_context(|| format!("spawning the exec gate for role {}", spec.role))?;
    let child_pid = child.id() as i32;
    // Owner does not keep the gate endpoint.
    drop(gate_fd);

    // Talk to the gate over the owner endpoint.
    let mut stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(owner_fd.into_raw()) };

    // Step 2: bounded wait for READY.
    let ready = read_ready(&mut stream)?;

    // Step 3: validate the gate against the kernel — pid, pgid, and birth.
    if ready.pid != child_pid {
        bail!(
            "exec gate reported pid {} but we spawned {child_pid}",
            ready.pid
        );
    }
    let kernel_pgid = read_pgid(child_pid).context("reading the gate's pgid before release")?;
    if kernel_pgid != ready.pgid {
        bail!(
            "exec gate reported pgid {} but the kernel says {kernel_pgid}",
            ready.pgid
        );
    }
    let birth = read_birth_identity(child_pid)
        .context("reading the gate's birth identity before release")?;
    if birth != ready.birth {
        bail!("exec gate's reported birth identity does not match the kernel");
    }

    // Step 4: fsync the child identity, before a single GO byte is sent.
    on_ready(&ready, &intent).context("recording the child identity before release")?;

    // Step 5: re-read the birth identity AFTER the fsync — the process we
    // durably recorded must still be the same one we are about to release.
    let birth_after = read_birth_identity(child_pid)
        .context("re-reading the gate's birth identity after the identity fsync")?;
    if birth_after != ready.birth {
        bail!("exec gate's birth identity changed around the identity fsync");
    }

    // Step 6: release with one-use GO.
    stream
        .write_all(format!("GO {}\n", spec.nonce).as_bytes())
        .context("sending GO to the exec gate")?;
    stream.flush().ok();

    // Step 7: readiness fence — the target must confirm it execed and started.
    read_ack(&mut stream, child_pid).context("waiting for the released target's readiness ACK")?;

    Ok(ready)
}

/// Read one `READY <pid> <pgid> <sec> <usec>` line within the bounded window.
fn read_ready(stream: &mut std::os::unix::net::UnixStream) -> Result<GateReady> {
    let line =
        read_line_bounded(stream, READY_TIMEOUT).context("reading READY from the exec gate")?;
    parse_ready(line.trim())
}

/// The readiness fence: read one `ACK …` line within [`ACK_TIMEOUT`]. Anything
/// else — EOF (the gate `_exit`ed without exec), a timeout, or a non-ACK line —
/// means the target did not come up, and the caller must not treat the child as
/// armed.
fn read_ack(stream: &mut std::os::unix::net::UnixStream, child_pid: i32) -> Result<()> {
    let line = read_line_bounded(stream, ACK_TIMEOUT)
        .context("the released target never confirmed it started")?;
    if line.trim_start().starts_with("ACK") {
        Ok(())
    } else {
        bail!(
            "expected an ACK from pid {child_pid}, got {:?}",
            line.trim()
        )
    }
}

/// Read a single newline-terminated line off `stream`, giving up after
/// `timeout`. EOF before any byte is an error (the peer closed). Uses a fresh
/// clone so no buffered reader outlives the call and holds the socket open.
fn read_line_bounded(
    stream: &mut std::os::unix::net::UnixStream,
    timeout: Duration,
) -> Result<String> {
    stream
        .set_read_timeout(Some(timeout))
        .context("arming the read timeout")?;
    let deadline = Instant::now() + timeout;
    let mut reader = BufReader::new(stream.try_clone().context("cloning the gate stream")?);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => bail!("peer closed the socket before a line arrived"),
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                bail!("no line within {}ms", timeout.as_millis());
            }
            Err(err) => return Err(err).context("reading a line"),
        }
        if line.trim().is_empty() {
            if Instant::now() >= deadline {
                bail!("no non-empty line within {}ms", timeout.as_millis());
            }
            continue;
        }
        return Ok(line);
    }
}

fn parse_ready(line: &str) -> Result<GateReady> {
    // `READY <pid> <pgid> <start_sec> <start_usec>`
    let mut it = line.split_whitespace();
    if it.next() != Some("READY") {
        bail!("exec gate said something that was not READY: {line:?}");
    }
    let pid: i32 = it.next().context("READY missing pid")?.parse()?;
    let pgid: i32 = it.next().context("READY missing pgid")?.parse()?;
    let start_sec: i64 = it.next().context("READY missing start_sec")?.parse()?;
    let start_usec: i64 = it.next().context("READY missing start_usec")?.parse()?;
    Ok(GateReady {
        pid,
        pgid,
        birth: BirthIdentity {
            start_sec,
            start_usec,
        },
    })
}

// ----------------------------------------------------------------------------
// The gate (child) side — the `internal-exec-gate` subcommand.
// ----------------------------------------------------------------------------

/// Parsed args for the gate subcommand.
struct GateArgs {
    gate_fd: RawFd,
    nonce: String,
    #[allow(dead_code)]
    role: String,
    target_argv: Vec<String>,
}

fn parse_gate_args(args: &[String]) -> Result<GateArgs> {
    let mut gate_fd = None;
    let mut nonce = None;
    let mut role = String::new();
    let mut target_argv = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--gate-fd" => gate_fd = it.next().and_then(|v| v.parse::<RawFd>().ok()),
            "--nonce" => nonce = it.next().cloned(),
            "--role" => role = it.next().cloned().unwrap_or_default(),
            "--" => {
                target_argv = it.cloned().collect();
                break;
            }
            _ => {}
        }
    }
    Ok(GateArgs {
        gate_fd: gate_fd.context("--gate-fd is required")?,
        nonce: nonce.context("--nonce is required")?,
        role,
        target_argv,
    })
}

/// The `internal-exec-gate` entry point. This function **never returns on the
/// success path**: it either `execvp`s the target (replacing this process) or
/// `_exit`s without ever touching it. It is deliberately hidden (an `internal-`
/// subcommand), not part of the public CLI.
pub fn run_gate(args: &[String]) -> ! {
    match gate_body(args) {
        // gate_body only returns on refusal; the exec path diverges inside it.
        Ok(()) => inert_exit("gate returned without releasing"),
        Err(_) => inert_exit("gate refused"),
    }
}

fn gate_body(args: &[String]) -> Result<()> {
    let parsed = parse_gate_args(args)?;
    if parsed.target_argv.is_empty() {
        bail!("no target argv");
    }

    // Our own process group: the gate contains itself and the coming target,
    // so cleanup can act on the group without touching our parent. A `setpgid`
    // failure is **fatal** (finding 5) — a gate that could not contain itself
    // must not exec — and the pgid is **read from the kernel**, never invented
    // as `pid`: a `getpgid`/read failure is fatal too.
    if unsafe { libc::setpgid(0, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).context("setpgid on the gate");
    }
    let pid = std::process::id() as i32;
    let pgid = read_pgid(pid).context("reading the gate's own pgid")?;
    let birth = read_birth_identity(pid).context("reading own birth identity")?;

    // Attach to the release socket the owner dup'd onto GATE_FD.
    let mut stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(parsed.gate_fd) };

    // Report READY and then block for GO. Any write failure ⇒ inert exit.
    stream
        .write_all(
            format!(
                "READY {pid} {pgid} {} {}\n",
                birth.start_sec, birth.start_usec
            )
            .as_bytes(),
        )
        .context("sending READY")?;
    stream.flush().ok();

    // Block reading the single release token, but only until an **absolute
    // deadline** (findings 5 & 10): owner death closes the only writer ⇒ read
    // returns 0 ⇒ exit; a wedged or malicious owner that trickles bytes cannot
    // extend the wait, because each read's timeout is the *remaining* time to the
    // one deadline, not a fresh full window per read. Deadline exceeded ⇒ exit
    // without exec.
    let go_timeout = go_timeout();
    let deadline = Instant::now() + go_timeout;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("no GO within {}ms", go_timeout.as_millis());
        }
        // A zero Duration would mean "block forever" to the socket, so the
        // is_zero() guard above ensures we only ever arm a strictly positive
        // remaining window.
        stream
            .set_read_timeout(Some(remaining))
            .context("arming the GO read timeout")?;
        match stream.read(&mut byte) {
            Ok(0) => bail!("owner closed the release socket before GO"),
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
                if buf.len() > 256 {
                    bail!("release line too long");
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                bail!("no GO within {}ms", go_timeout.as_millis());
            }
            Err(err) => return Err(err).context("reading the release token"),
        }
    }
    let line = String::from_utf8_lossy(&buf);
    let mut it = line.split_whitespace();
    if it.next() != Some("GO") {
        bail!("release token was not GO");
    }
    let got_nonce = it.next().unwrap_or("");
    if got_nonce != parsed.nonce {
        bail!("release nonce mismatch");
    }

    // Released. Hand the gate socket to the target as the readiness-ACK fd
    // (finding 5): keep GATE_FD open across execve (clear CLOEXEC) and name it
    // in the environment so a CodeConnect child confirms it started. Then
    // replace this process with the target — PID (and the recorded birth
    // identity) preserved. execvp does not return on success.
    unsafe {
        let flags = libc::fcntl(GATE_FD, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(GATE_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
    std::env::set_var(ACK_FD_ENV, GATE_FD.to_string());
    // Do not let the Rust wrapper close GATE_FD when it drops.
    std::mem::forget(stream);
    exec_target(&parsed.target_argv)
}

/// If launched through the exec gate, confirm to the owner that this process has
/// really execed and started (the readiness fence, finding 5), then stop using
/// the ack fd. A no-op when not gated. Call once at the top of a gated
/// subcommand's `main` body.
pub fn ack_started_if_gated() {
    let Some(raw) = std::env::var(ACK_FD_ENV)
        .ok()
        .and_then(|v| v.parse::<RawFd>().ok())
    else {
        return;
    };
    // Write directly to the raw fd; do not wrap-and-drop (that would close the
    // owner-shared socket in a way the owner might misread). One line, then we
    // clear the env so a grandchild does not inherit the duty.
    let msg = format!("ACK {}\n", std::process::id());
    unsafe {
        libc::write(raw, msg.as_ptr() as *const libc::c_void, msg.len());
        libc::close(raw);
    }
    std::env::remove_var(ACK_FD_ENV);
}

/// A tiny **test-only gated target** (`internal-gate-ack-probe <marker>`): it
/// fires the readiness fence ([`ack_started_if_gated`]) so the owner's ACK wait
/// resolves, then touches `<marker>` to prove it genuinely execed the target,
/// and exits. Used by the exec-gate integration tests to fence `execve` on a
/// real target-side marker rather than a sleep (finding 5). Hidden machinery,
/// never a human command.
pub fn run_ack_probe(args: &[String]) -> ! {
    ack_started_if_gated();
    if let Some(marker) = args.first() {
        // Best-effort: the test polls for this file to confirm the exec landed.
        let _ = std::fs::File::create(marker);
    }
    std::process::exit(0)
}

/// `execvp` the target. Only returns (as an error) if exec fails.
fn exec_target(argv: &[String]) -> Result<()> {
    use std::ffi::CString;
    let program = CString::new(argv[0].as_bytes()).context("target path has a NUL")?;
    let c_args: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(a.as_bytes()))
        .collect::<std::result::Result<_, _>>()
        .context("a target arg has a NUL")?;
    let mut ptrs: Vec<*const libc::c_char> = c_args.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe {
        libc::execvp(program.as_ptr(), ptrs.as_ptr());
    }
    // Only reached if execvp failed.
    Err(std::io::Error::last_os_error()).context("execvp of the gate target failed")
}

/// Exit **without** touching the target. `_exit` (not `exit`) so no atexit
/// handler or buffered write can produce a target-side effect.
fn inert_exit(_why: &str) -> ! {
    unsafe { libc::_exit(70) }
}

// ----------------------------------------------------------------------------
// socketpair plumbing.
// ----------------------------------------------------------------------------

/// A connected `AF_UNIX` `SOCK_STREAM` pair. The owner end is CLOEXEC (never
/// leaks into unrelated spawns); the gate end is not (it must survive the gate's
/// own execve of the target — though in practice the gate keeps it open only
/// until GO).
fn socketpair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("socketpair");
    }
    let owner = OwnedFd(fds[0]);
    let gate = OwnedFd(fds[1]);
    // Owner end CLOEXEC: it must not be inherited by the gate or any later child.
    unsafe {
        let flags = libc::fcntl(owner.0, libc::F_GETFD);
        libc::fcntl(owner.0, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
    Ok((owner, gate))
}

/// A minimal owned fd so the pair is closed on drop without pulling in a crate.
struct OwnedFd(RawFd);
impl OwnedFd {
    fn into_raw(self) -> RawFd {
        let fd = self.0;
        std::mem::forget(self);
        fd
    }
}
impl AsRawFd for OwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}
impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ready_reads_all_five_fields() {
        let r = parse_ready("READY 100 200 12345 678").unwrap();
        assert_eq!(r.pid, 100);
        assert_eq!(r.pgid, 200);
        assert_eq!(r.birth.start_sec, 12345);
        assert_eq!(r.birth.start_usec, 678);
    }

    #[test]
    fn parse_ready_rejects_a_non_ready_line() {
        assert!(parse_ready("HELLO 1 2 3 4").is_err());
        assert!(parse_ready("READY 1 2").is_err());
    }

    #[test]
    fn gate_args_require_fd_and_nonce_and_split_the_target() {
        let args: Vec<String> = [
            "--gate-fd",
            "7",
            "--nonce",
            "abc",
            "--role",
            "tui",
            "--",
            "/bin/echo",
            "hi",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let parsed = parse_gate_args(&args).unwrap();
        assert_eq!(parsed.gate_fd, 7);
        assert_eq!(parsed.nonce, "abc");
        assert_eq!(parsed.target_argv, vec!["/bin/echo", "hi"]);
    }

    /// The owner-side ordering invariant, exercised with an in-process fake
    /// gate: `on_ready` (the fsync) must run before the GO byte appears on the
    /// wire, and if `on_ready` fails, GO must never be sent.
    #[test]
    fn on_ready_runs_before_go_and_a_failing_on_ready_withholds_go() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        // We can't easily fake the spawned gate binary here, so this test drives
        // the *ordering* contract directly against a socketpair with a thread
        // acting as the gate. It mirrors launch_gated's send order.
        for on_ready_fails in [false, true] {
            let (owner, gate) = socketpair().unwrap();
            let owner_stream =
                unsafe { std::os::unix::net::UnixStream::from_raw_fd(owner.into_raw()) };
            let gate_stream =
                unsafe { std::os::unix::net::UnixStream::from_raw_fd(gate.into_raw()) };

            let go_seen = Arc::new(AtomicBool::new(false));
            let ready_ran = Arc::new(AtomicBool::new(false));
            let go_seen_g = go_seen.clone();

            // The fake gate: send READY, then read a line; record if it is GO.
            let gate_thread = std::thread::spawn(move || {
                let mut s = gate_stream;
                s.write_all(b"READY 1 1 1 1\n").unwrap();
                let mut r = BufReader::new(s);
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() > 0 && line.starts_with("GO ") {
                    go_seen_g.store(true, Ordering::SeqCst);
                }
            });

            // Drive the owner half by hand (launch_gated's post-spawn logic).
            let mut s = owner_stream;
            let mut line = String::new();
            {
                // Read READY through a scoped BufReader on a clone, then DROP the
                // clone: otherwise the owner still holds a second fd to the pair
                // and dropping `s` alone would never EOF the gate.
                let mut reader = BufReader::new(s.try_clone().unwrap());
                reader.read_line(&mut line).unwrap();
            }
            assert!(line.starts_with("READY"));

            // on_ready — must be observed strictly before GO.
            let ready_ran_c = ready_ran.clone();
            let go_before = go_seen.load(Ordering::SeqCst);
            let on_ready = || {
                ready_ran_c.store(true, Ordering::SeqCst);
                assert!(!go_before, "GO must not precede on_ready");
                if on_ready_fails {
                    Err(anyhow::anyhow!("fsync failed"))
                } else {
                    Ok(())
                }
            };
            let result = on_ready();

            if result.is_ok() {
                s.write_all(b"GO nonce\n").unwrap();
                s.flush().ok();
            }
            drop(s);
            gate_thread.join().unwrap();

            assert!(ready_ran.load(Ordering::SeqCst), "on_ready must run");
            if on_ready_fails {
                assert!(
                    !go_seen.load(Ordering::SeqCst),
                    "a failing on_ready must withhold GO"
                );
            } else {
                assert!(
                    go_seen.load(Ordering::SeqCst),
                    "a good on_ready releases GO"
                );
            }
        }
    }
}
