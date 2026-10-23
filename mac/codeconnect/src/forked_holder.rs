//! **A forked child that keeps every descriptor it inherited until it is told
//! to go.** Test-only: the instrument for the tests about a lock a fork
//! duplicates (`codex_launch`'s `LaunchLock`, `update_check`'s throttle claim).
//!
//! **Released by a byte, never by end of file.** Tests run in parallel and fork
//! from one process, so a child forked by one test inherits the write end of
//! another test's release pipe. Closing the parent's writer then ends neither
//! read: each child waits on a writer the other is holding, and both tests wait
//! on their children for ever. One byte written into the child's own pipe
//! reaches it whoever else holds the pipe open.
//!
//! **Reaped within a bound.** A child that does not leave once told is killed
//! and reported, so a regression is a failing test rather than a stuck one; and
//! a child never released — its test panicked first — is killed and reaped when
//! its guard drops.

use std::os::raw::c_int;
use std::time::{Duration, Instant};

/// A release pipe, not yet forked from.
///
/// Split from [`Release::fork_holder`] so a test can create two pipes before
/// either fork — the interleaving in which each child inherits the other's
/// writer.
pub(crate) struct Release {
    read: c_int,
    write: c_int,
}

/// A child holding everything it inherited, waiting for its release byte.
pub(crate) struct HeldChild {
    pid: libc::pid_t,
    write: c_int,
    /// Set once `release` has reaped the child, so the guard has nothing left
    /// to do.
    reaped: bool,
}

impl Release {
    pub(crate) fn new() -> Release {
        let mut fds = [0 as c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        Release {
            read: fds[0],
            write: fds[1],
        }
    }

    /// Fork the child. It holds every descriptor it inherited — the lock under
    /// test among them — until one byte arrives on its pipe, or the pipe ends,
    /// then leaves without running a destructor. Only async-signal-safe calls in
    /// the child: it is the child of a threaded process.
    pub(crate) fn fork_holder(self) -> HeldChild {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork");
        if pid == 0 {
            unsafe {
                libc::close(self.write);
                let mut byte = 0u8;
                loop {
                    let n = libc::read(self.read, std::ptr::addr_of_mut!(byte).cast(), 1);
                    if n >= 0 {
                        libc::_exit(0);
                    }
                    if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                        libc::_exit(1);
                    }
                }
            }
        }
        unsafe { libc::close(self.read) };
        HeldChild {
            pid,
            write: self.write,
            reaped: false,
        }
    }
}

impl HeldChild {
    /// Tell the child to go, and reap it within `bound`.
    ///
    /// `true` when it left by itself; `false` when it was still there at the
    /// bound and had to be killed.
    pub(crate) fn release(mut self, bound: Duration) -> bool {
        let byte = 1u8;
        loop {
            let n = unsafe { libc::write(self.write, std::ptr::addr_of!(byte).cast(), 1) };
            if n == 1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
        unsafe { libc::close(self.write) };
        self.reaped = true;
        let deadline = Instant::now() + bound;
        loop {
            let reaped = unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), libc::WNOHANG) };
            if reaped == self.pid {
                return true;
            }
            if Instant::now() >= deadline {
                kill_and_reap(self.pid);
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// **A child never released goes with its guard.** Killed rather than told,
/// because a guard dropped while a test unwinds has no bound worth waiting out.
impl Drop for HeldChild {
    fn drop(&mut self) {
        if !self.reaped {
            unsafe { libc::close(self.write) };
            kill_and_reap(self.pid);
        }
    }
}

/// `SIGKILL` cannot be caught or ignored, so the `waitpid` after it returns.
fn kill_and_reap(pid: libc::pid_t) {
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Codex's interleaving: two pipes made before either fork.** The first
    /// child inherits both ends of the second pipe, and the second child inherits
    /// the first pipe's writer, which the parent still holds. Released by end of
    /// file, the first child would wait for ever on a writer its sibling holds.
    ///
    /// **Mutation:** make `release` close the writer without writing the byte,
    /// and the first child is still there at the bound.
    #[test]
    fn a_child_leaves_when_released_even_while_another_child_holds_its_pipe() {
        let first = Release::new();
        let second = Release::new();
        let first_child = first.fork_holder();
        let second_child = second.fork_holder();

        let pids = [first_child.pid, second_child.pid];
        // Both released before anything is asserted, so a failure leaves no child.
        let first_left = first_child.release(Duration::from_secs(2));
        let second_left = second_child.release(Duration::from_secs(2));
        let still_running: Vec<libc::pid_t> =
            pids.into_iter().filter(|pid| is_running(*pid)).collect();
        assert!(
            first_left && second_left,
            "each child must leave when released, whoever else holds its pipe \
             (first: {first_left}, second: {second_left}; still running: {still_running:?})"
        );
        assert!(still_running.is_empty(), "left behind: {still_running:?}");
    }

    /// Whether `pid` is still a process (a zombie counts: nothing reaped it).
    fn is_running(pid: libc::pid_t) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// **A child whose test panicked before releasing it is killed and reaped.**
    ///
    /// A failed assertion unwinds past the `release` it never reached; the guard
    /// is what keeps that child from outliving the test, holding everything it
    /// inherited.
    ///
    /// **Mutation:** delete `HeldChild`'s `Drop` and the child is still there.
    #[test]
    fn a_child_never_released_is_killed_and_reaped_when_its_guard_drops() {
        let child = Release::new().fork_holder();
        let pid = child.pid;
        drop(child);
        assert!(!is_running(pid), "the dropped guard left its child running");
    }
}
