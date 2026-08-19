//! Real-process integration tests for the D6 inert exec gate.
//!
//! These spawn the actual `codeconnect internal-exec-gate` subcommand (via
//! `CARGO_BIN_EXE_codeconnect`) and drive its release socket by hand, playing
//! the owner. The load-bearing assertion in every scenario is the same one D6
//! makes: **no target-side marker exists unless a valid GO was sent** — owner
//! death, a wrong nonce, or a malformed token all leave the target untouched.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

const GATE_FD: RawFd = 3;

/// A connected AF_UNIX pair; the owner end is CLOEXEC so it does not leak into
/// the spawned gate (otherwise the gate would hold a writer to itself and never
/// EOF when the owner "dies").
fn socketpair() -> (RawFd, RawFd) {
    let mut fds = [0 as libc::c_int; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "socketpair failed");
    unsafe {
        let flags = libc::fcntl(fds[0], libc::F_GETFD);
        libc::fcntl(fds[0], libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
    (fds[0], fds[1])
}

fn unique_marker(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("cc-execgate-{tag}-{}-{nanos}", std::process::id()))
}

struct Owner {
    child: std::process::Child,
    stream: std::os::unix::net::UnixStream,
}

/// Poll `f` until it is true or `within` elapses.
fn wait_until(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

/// Spawn the real gate with `target_argv`, returning the owner half plus the
/// READY line it reported. The gate end is dup'd onto fd 3 in the child.
fn spawn_gate(nonce: &str, target_argv: &[&str]) -> (Owner, String) {
    spawn_gate_env(nonce, target_argv, &[])
}

/// As [`spawn_gate`], but also sets `envs` on the gate process — used to inject
/// the test-only `CC_GATE_GO_TIMEOUT_MS` override so the GO-timeout path can be
/// exercised in milliseconds.
fn spawn_gate_env(nonce: &str, target_argv: &[&str], envs: &[(&str, &str)]) -> (Owner, String) {
    let (owner_fd, gate_fd) = socketpair();
    let bin = env!("CARGO_BIN_EXE_codeconnect");
    let mut cmd = Command::new(bin);
    cmd.arg("internal-exec-gate")
        .arg("--gate-fd")
        .arg(GATE_FD.to_string())
        .arg("--nonce")
        .arg(nonce)
        .arg("--role")
        .arg("test")
        .arg("--")
        .args(target_argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(gate_fd, GATE_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(GATE_FD, libc::F_GETFD);
            libc::fcntl(GATE_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            Ok(())
        });
    }
    let child = cmd.spawn().expect("spawn the exec gate");
    unsafe {
        libc::close(gate_fd);
    }
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(owner_fd) };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut ready = String::new();
    reader.read_line(&mut ready).expect("read READY");
    assert!(ready.starts_with("READY "), "got: {ready:?}");
    (Owner { child, stream }, ready)
}

fn wait_and_marker_absent(mut child: std::process::Child, marker: &std::path::Path) {
    let status = child.wait().expect("gate exits");
    assert!(
        !status.success(),
        "an inert gate must _exit non-zero, not exec the target"
    );
    // Give any (erroneous) target a beat to have created its marker.
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !marker.exists(),
        "the target must never run without a valid GO: {} exists",
        marker.display()
    );
}

#[test]
fn a_valid_go_releases_the_target() {
    let marker = unique_marker("go");
    let (mut owner, _ready) = spawn_gate("secret", &["/usr/bin/touch", marker.to_str().unwrap()]);
    owner.stream.write_all(b"GO secret\n").unwrap();
    owner.stream.flush().ok();
    let status = owner.child.wait().expect("gate exits");
    assert!(status.success(), "touch under a valid GO succeeds");
    assert!(marker.exists(), "the target ran and created its marker");
    let _ = std::fs::remove_file(&marker);
}

#[test]
fn owner_death_before_go_leaves_the_target_untouched() {
    let marker = unique_marker("ownerdeath");
    let (owner, _ready) = spawn_gate("secret", &["/usr/bin/touch", marker.to_str().unwrap()]);
    // "Owner death": drop the only writer without ever sending GO. The gate must
    // EOF and _exit without exec.
    let Owner { child, stream } = owner;
    drop(stream);
    wait_and_marker_absent(child, &marker);
}

#[test]
fn a_wrong_nonce_is_refused() {
    let marker = unique_marker("wrongnonce");
    let (owner, _ready) = spawn_gate("secret", &["/usr/bin/touch", marker.to_str().unwrap()]);
    let Owner { child, mut stream } = owner;
    stream.write_all(b"GO not-the-nonce\n").unwrap();
    stream.flush().ok();
    wait_and_marker_absent(child, &marker);
}

#[test]
fn a_malformed_token_is_refused() {
    let marker = unique_marker("malformed");
    let (owner, _ready) = spawn_gate("secret", &["/usr/bin/touch", marker.to_str().unwrap()]);
    let Owner { child, mut stream } = owner;
    stream.write_all(b"garbage not go\n").unwrap();
    stream.flush().ok();
    wait_and_marker_absent(child, &marker);
}

#[test]
fn the_recorded_birth_identity_survives_the_targets_execve() {
    // Fence `execve` on a real **target-side marker**, not a sleep (finding 5):
    // the target's first act is to create the marker, which is proof it truly
    // replaced the gate's image. Only once the marker appears do we read the
    // target's birth identity and assert the one reported at READY (pre-exec)
    // survived — no arbitrary sleep, so no race between our read and the exec.
    let marker = unique_marker("execve");
    let script = format!("touch {}; sleep 3", marker.to_str().unwrap());
    let (mut owner, ready) = spawn_gate("secret", &["/bin/sh", "-c", &script]);
    let mut parts = ready.split_whitespace();
    assert_eq!(parts.next(), Some("READY"));
    let pid: i32 = parts.next().unwrap().parse().unwrap();
    let sec: i64 = parts.nth(1).unwrap().parse().unwrap(); // skip pgid
    let usec: i64 = parts.next().unwrap().parse().unwrap();
    owner.stream.write_all(b"GO secret\n").unwrap();
    owner.stream.flush().ok();
    // Wait for the marker: the exec is proven by the target-side effect.
    assert!(
        wait_until(Duration::from_secs(5), || marker.exists()),
        "the target must exec and create its marker"
    );
    let birth = protocol::proc_identity::read_birth_identity(pid)
        .expect("the exec'd target is alive and readable");
    assert_eq!(birth.start_sec, sec, "start_sec survives execve");
    assert_eq!(birth.start_usec, usec, "start_usec survives execve");
    // Clean up the target.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let _ = owner.child.wait();
    let _ = std::fs::remove_file(&marker);
}

#[test]
fn a_gate_that_never_receives_go_times_out_and_exits_without_exec() {
    // The GO timeout (finding 5): with the owner still alive (writer open, so
    // this is a *timeout*, not an EOF/owner-death) but no GO ever sent, the gate
    // must _exit non-zero and never touch its target. The short env-override
    // timeout keeps the test sub-second.
    let marker = unique_marker("gotimeout");
    let (owner, _ready) = spawn_gate_env(
        "secret",
        &["/usr/bin/touch", marker.to_str().unwrap()],
        &[("CC_GATE_GO_TIMEOUT_MS", "300")],
    );
    // Keep the writer open across the wait so the gate times out rather than EOFs.
    let Owner { mut child, stream } = owner;
    let status = child.wait().expect("the gate exits on GO timeout");
    assert!(
        !status.success(),
        "a timed-out gate must _exit non-zero, not exec the target"
    );
    drop(stream);
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !marker.exists(),
        "the target must never run when the gate times out waiting for GO"
    );
}

#[test]
fn a_trickle_of_bytes_cannot_extend_the_absolute_go_deadline() {
    // Finding 10: the GO wait is an ABSOLUTE deadline, not a per-read timeout. A
    // malicious/wedged owner that trickles bytes slower than the deadline must
    // NOT keep the gate alive past it — the gate _exits without exec. Without the
    // fix, each byte reset a fresh full-window timeout and a late GO would be
    // accepted.
    let marker = unique_marker("trickle");
    let (owner, _ready) = spawn_gate_env(
        "secret",
        &["/usr/bin/touch", marker.to_str().unwrap()],
        &[("CC_GATE_GO_TIMEOUT_MS", "300")],
    );
    let Owner {
        mut child,
        mut stream,
    } = owner;
    // Trickle "GO secret\n" one byte every 100ms → ~1s total, far past the 300ms
    // deadline. Writes fail once the gate exits and closes its end; that is fine.
    for &b in b"GO secret\n" {
        if stream.write_all(&[b]).is_err() {
            break;
        }
        stream.flush().ok();
        std::thread::sleep(Duration::from_millis(100));
    }
    let status = child
        .wait()
        .expect("the gate exits at its absolute deadline");
    assert!(
        !status.success(),
        "a trickle past the absolute GO deadline must _exit, not accept a late GO"
    );
    drop(stream);
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !marker.exists(),
        "the target must never run when GO is trickled past the deadline"
    );
    let _ = std::fs::remove_file(&marker);
}

#[test]
fn the_readiness_fence_delivers_an_ack_from_the_execed_target() {
    // The owner-side readiness fence (finding 5): a gated CodeConnect target, on
    // GO, confirms it has execed and started by writing an ACK back over the gate
    // fd. Drive the gate by hand and prove both that the ACK arrives and that the
    // target really ran — this is what makes "custodian armed before tmux" real
    // rather than merely "GO written".
    let marker = unique_marker("ackfence");
    let bin = env!("CARGO_BIN_EXE_codeconnect");
    let (mut owner, _ready) = spawn_gate(
        "secret",
        &[bin, "internal-gate-ack-probe", marker.to_str().unwrap()],
    );
    owner.stream.write_all(b"GO secret\n").unwrap();
    owner.stream.flush().ok();
    owner
        .stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok();
    let mut reader = BufReader::new(owner.stream.try_clone().unwrap());
    let mut ack = String::new();
    reader.read_line(&mut ack).expect("read the ACK line");
    assert!(
        ack.starts_with("ACK "),
        "the readiness fence must deliver an ACK from the target, got {ack:?}"
    );
    assert!(
        wait_until(Duration::from_secs(5), || marker.exists()),
        "the acked target really execed and ran"
    );
    let _ = owner.child.wait();
    let _ = std::fs::remove_file(&marker);
}
