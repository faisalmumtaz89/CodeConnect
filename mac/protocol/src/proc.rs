//! Running an external command with a deadline.
//!
//! Every child this workspace spawns for an answer — tmux above all — is a
//! process this one does not control: it can hang without exiting, and a
//! caller that waits unboundedly inherits the hang. tmux makes it worse in a
//! second way: a tmux client passes its stdout to the tmux *server* over
//! `SCM_RIGHTS`, so even a dead client's pipe stays open for as long as the
//! server holds the passed end — reading such a pipe to EOF is itself an
//! unbounded wait (measured: a stopped server held one for over an hour).
//!
//! So everything here happens on the caller's thread under one absolute
//! deadline: the child's pipes are read without blocking, the child is
//! polled for exit, and when the deadline passes the child is killed and
//! reaped. No helper threads exist, so no timeout can accumulate parked
//! threads or descriptors. What a timeout *means* — refusal, or
//! indeterminacy — belongs to the caller, because it depends on whether the
//! command could have mutated anything before it stalled.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How much of a child's stdout or stderr is kept. Far above any answer tmux
/// gives (a full 50,000-line pane capture is under 8 MiB); the cap exists so
/// a child that streams forever costs memory proportional to this constant,
/// not to the deadline.
const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;

/// How long each quiet iteration sleeps. Coarse enough to cost nothing, fine
/// enough that a normal single-digit-millisecond call is not held noticeably
/// past its exit.
const IDLE_POLL: Duration = Duration::from_millis(5);

/// What happened to a deadlined child.
#[derive(Debug)]
pub enum RunOutcome {
    /// The child exited on its own and both pipes reached end-of-file; the
    /// status and the complete drained output.
    Completed {
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// The deadline passed before the child exited — or before its output
    /// finished arriving, which happens when the child handed its pipe to
    /// another process that will not let go. Either way the answer never
    /// arrived in time, the child is killed and reaped, and whether it
    /// *acted* first is unknowable.
    TimedOut { waited: Duration },
}

/// Run `command` to completion or to `deadline`, whichever comes first.
///
/// stdin is closed and stdout/stderr are captured, bounded by
/// [`MAX_STREAM_BYTES`]. The deadline is absolute: exit polling and output
/// draining all happen under it, so the call returns within the deadline
/// plus one poll interval. `Err` is returned **only** when the child could
/// not be spawned — the one failure that proves nothing ran; every failure
/// after a successful spawn kills and reaps the child and reports
/// [`RunOutcome::TimedOut`], because a child that ran and then could not be
/// observed is indeterminate, not absent.
pub fn run_deadlined(command: &mut Command, deadline: Duration) -> std::io::Result<RunOutcome> {
    let started = Instant::now();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // From here on, no `?`: the child exists, so every early return must
    // reap it first, and no post-spawn failure may look like "never ran".
    let mut stdout_pipe = child.stdout.take().map(|p| {
        let fd = p.as_raw_fd();
        set_nonblocking(fd);
        (p, fd)
    });
    let mut stderr_pipe = child.stderr.take().map(|p| {
        let fd = p.as_raw_fd();
        set_nonblocking(fd);
        (p, fd)
    });

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_status: Option<std::process::ExitStatus> = None;
    let until = started + deadline;

    loop {
        // Drain first, exit-check second: a child blocked writing into a
        // full pipe cannot exit, so the read is what lets it finish.
        let read_out = drain_available(&mut stdout_pipe, &mut stdout, until);
        let read_err = drain_available(&mut stderr_pipe, &mut stderr, until);

        if exit_status.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exit_status = Some(status),
                Ok(None) => {}
                // The child ran but can no longer be observed. Kill, reap,
                // and report indeterminate — never "nothing happened".
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(RunOutcome::TimedOut {
                        waited: started.elapsed(),
                    });
                }
            }
        }

        // Done only when the exit *and* both end-of-files have been seen:
        // an exited child whose pipe is still open means another process
        // holds the write end, and its output may still be incomplete.
        if let Some(status) = exit_status {
            if stdout_pipe.is_none() && stderr_pipe.is_none() {
                return Ok(RunOutcome::Completed {
                    status,
                    stdout,
                    stderr,
                });
            }
        }

        if started.elapsed() >= deadline {
            // Kill even an already-exited child is a no-op; the reap is what
            // matters. An exited child whose pipes never closed is reported
            // as a timeout too: its answer did not arrive in time, and
            // pretending an incomplete capture is a completion would hand
            // callers truncated pane text as if it were the pane.
            let _ = child.kill();
            let _ = child.wait();
            return Ok(RunOutcome::TimedOut {
                waited: started.elapsed(),
            });
        }

        if !read_out && !read_err {
            std::thread::sleep(IDLE_POLL);
        }
    }
}

fn set_nonblocking(fd: std::os::fd::RawFd) {
    // Best-effort: the read gate below is `poll`, which is what guarantees a
    // read cannot block — this flag just lets the drain loop batch harder.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Whether one read from this descriptor can complete right now — data
/// waiting, or end-of-file. `poll` answers without consuming anything, so a
/// read gated on it cannot block even if `O_NONBLOCK` could not be set.
fn readable_now(fd: std::os::fd::RawFd) -> bool {
    let mut probe = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut probe, 1, 0) };
    ready > 0 && (probe.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0
}

/// Read what the pipe has right now, never past `until`. Each read is gated
/// on [`readable_now`], so it cannot block; the deadline check between reads
/// keeps a fire-hose writer from monopolising the loop past expiry. Returns
/// whether any bytes arrived; sets the slot to `None` at end-of-file (a read
/// error counts — the pipe can yield nothing more either way). Interrupted
/// reads are retried.
fn drain_available(
    pipe: &mut Option<(impl Read, std::os::fd::RawFd)>,
    into: &mut Vec<u8>,
    until: Instant,
) -> bool {
    let Some((stream, fd)) = pipe.as_mut() else {
        return false;
    };
    let fd = *fd;
    let mut any = false;
    let mut chunk = [0u8; 8192];
    loop {
        if !readable_now(fd) {
            return any;
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                *pipe = None;
                return any;
            }
            Ok(n) => {
                any = true;
                let room = MAX_STREAM_BYTES.saturating_sub(into.len());
                into.extend_from_slice(&chunk[..n.min(room)]);
                // Past the cap the stream is still consumed, so the child
                // can keep writing and eventually exit — never block on a
                // full pipe.
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return any,
            Err(_) => {
                *pipe = None;
                return any;
            }
        }
        if Instant::now() >= until {
            return any;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child that never exits must cost the deadline, not forever — and
    /// must be gone afterwards. The child writes its own pid to a file
    /// before parking, so "gone" is checked against that exact pid: after a
    /// kill *and* a reap, `kill(pid, 0)` answers `ESRCH` — a zombie (killed
    /// but never reaped) would still answer it, so this proves both.
    #[test]
    fn a_child_that_never_exits_costs_the_deadline_and_is_reaped() {
        let pid_file = std::env::temp_dir().join(format!("cc-proc-reap-{}", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);
        let started = Instant::now();
        let outcome = run_deadlined(
            Command::new("/bin/sh").args([
                "-c",
                &format!("echo $$ > {}; exec /bin/sleep 30", pid_file.display()),
            ]),
            Duration::from_millis(300),
        )
        .expect("sh spawns");
        assert!(
            matches!(outcome, RunOutcome::TimedOut { .. }),
            "{outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the deadline bounded the wait: {:?}",
            started.elapsed()
        );
        // Gone entirely: `kill(pid, 0)` answers ESRCH only for a pid that
        // was killed AND reaped — a zombie still answers it, so this proves
        // both, against the exact pid the child recorded of itself.
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("the child recorded its pid before parking")
            .trim()
            .parse()
            .expect("a pid");
        let answer = unsafe { libc::kill(pid, 0) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(
            (answer, errno),
            (-1, Some(libc::ESRCH)),
            "pid {pid} must be killed and reaped, not surviving or a zombie"
        );
        let _ = std::fs::remove_file(&pid_file);
    }

    /// A child that floods stdout forever: draining must keep it moving and
    /// the deadline must still hold. This is the pipe-capacity deadlock case.
    #[test]
    fn a_child_that_floods_stdout_cannot_outrun_the_deadline() {
        let started = Instant::now();
        let outcome = run_deadlined(
            &mut Command::new("/usr/bin/yes"),
            Duration::from_millis(300),
        )
        .expect("yes spawns");
        assert!(matches!(outcome, RunOutcome::TimedOut { .. }));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// Output is kept, bounded, and complete for a child that behaves.
    #[test]
    fn a_well_behaved_child_completes_with_its_output() {
        let outcome = run_deadlined(
            Command::new("/bin/echo").arg("answer"),
            Duration::from_secs(5),
        )
        .expect("echo spawns");
        match outcome {
            RunOutcome::Completed {
                status,
                stdout,
                stderr,
            } => {
                assert!(status.success());
                assert_eq!(String::from_utf8_lossy(&stdout).trim(), "answer");
                assert!(stderr.is_empty());
            }
            RunOutcome::TimedOut { .. } => panic!("echo does not time out"),
        }
    }

    /// The tmux shape: the child exits promptly but handed its stdout to a
    /// longer-lived process, so end-of-file never comes. That answer never
    /// arrived — it must be a timeout at the absolute deadline, not a
    /// "completion" carrying truncated output, and not a wait on the holder.
    #[test]
    fn an_exited_child_whose_pipe_is_held_elsewhere_is_a_timeout_not_a_wait() {
        let holder_file =
            std::env::temp_dir().join(format!("cc-proc-holder-{}", std::process::id()));
        let _ = std::fs::remove_file(&holder_file);
        let started = Instant::now();
        let outcome = run_deadlined(
            Command::new("/bin/sh").args([
                "-c",
                &format!(
                    "/bin/sleep 30 & echo $! > {}; echo hi",
                    holder_file.display()
                ),
            ]),
            Duration::from_millis(400),
        )
        .expect("sh spawns");
        assert!(
            matches!(outcome, RunOutcome::TimedOut { .. }),
            "an answer that cannot finish arriving is not a completion: {outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "and the holder is not waited for: {:?}",
            started.elapsed()
        );
        // The orphaned holder is cleaned up by its exact recorded pid, so
        // parallel tests' children are untouched.
        if let Ok(holder) = std::fs::read_to_string(&holder_file) {
            if let Ok(pid) = holder.trim().parse::<i32>() {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        let _ = std::fs::remove_file(&holder_file);
    }

    /// Timeouts must not accumulate resources: after many of them, the
    /// process holds no more descriptors than it started with. This is what
    /// rules out the parked-thread/leaked-pipe design this module replaced.
    #[test]
    fn repeated_timeouts_accumulate_no_descriptors() {
        let open_fds = || std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(0);
        // One warm-up so lazy allocations settle before the baseline.
        let _ = run_deadlined(
            Command::new("/bin/sleep").arg("30"),
            Duration::from_millis(50),
        );
        let baseline = open_fds();
        for _ in 0..20 {
            let _ = run_deadlined(
                Command::new("/bin/sleep").arg("30"),
                Duration::from_millis(50),
            );
        }
        let after = open_fds();
        assert!(
            after <= baseline + 2,
            "descriptors grew from {baseline} to {after} across 20 timeouts"
        );
    }

    /// After a timeout the runner is just a function again — nothing about a
    /// killed child can poison the next call.
    #[test]
    fn the_runner_is_reusable_after_a_timeout() {
        let _ = run_deadlined(
            Command::new("/bin/sleep").arg("30"),
            Duration::from_millis(100),
        )
        .expect("sleep spawns");
        let after = run_deadlined(Command::new("/bin/echo").arg("ok"), Duration::from_secs(5))
            .expect("echo spawns");
        assert!(matches!(
            after,
            RunOutcome::Completed { status, .. } if status.success()
        ));
    }

    /// A command that cannot be spawned is the one case that proves nothing
    /// ran — it must be an `Err`, not a timeout or an empty completion.
    #[test]
    fn an_unspawnable_command_is_an_error_not_an_outcome() {
        assert!(run_deadlined(
            &mut Command::new("/nonexistent/binary"),
            Duration::from_secs(1)
        )
        .is_err());
    }
}
