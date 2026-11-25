//! **Every child process ccd starts is started here; on macOS, one at a time.**
//!
//! On macOS the standard library makes each pipe it gives a child with `pipe()` and only then
//! marks both ends close-on-exec (`library/std/src/sys/pipe/unix.rs`, the `_` branch of `pipe`,
//! read in Rust 1.97.1). That includes the error pipe a `pre_exec` spawn waits on, since such a
//! spawn forks rather than calling `posix_spawn` (`sys/process/unix/unix.rs`, `posix_spawn` is
//! skipped while closures are set, and the fork path takes `sys::pipe::pipe()` off Linux). A child
//! another thread starts between those two steps inherits the pipe and holds it open for as long
//! as it lives. When it is the error pipe, the first spawn blocks in `read()` until the inheritor
//! exits, which for a run's host can be hours: a start that is never answered, on a worker that
//! never comes back. When it is a captured stdout, the reader waits for an end of file that comes
//! only then.
//!
//! Linux has no such window: `pipe` there is one `pipe2(fds, O_CLOEXEC)` call, and the fork path's
//! error channel is a socket pair made close-on-exec as it is made. So the lock is macOS's alone,
//! and a burst of starts on Linux is not serialised for nothing.
//!
//! The lock is held across `spawn()` alone, never across a wait or the reading of a child's
//! output, so a child that runs for minutes delays nobody's start. `spawn()` does not await, so the
//! lock is never held across an `.await`.
//!
//! `clippy.toml` refuses `spawn`, `output` and `status` on a `Command` anywhere else, on every
//! platform, so a new start cannot forget this module.

use std::io;
use std::process::{Output, Stdio};
#[cfg(target_os = "macos")]
static STARTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_at_a_time<T>(start: impl FnOnce() -> T) -> T {
    // A panic while starting a child leaves nothing half-done here to protect.
    #[cfg(target_os = "macos")]
    let _starting = STARTING.lock().unwrap_or_else(|held| held.into_inner());
    start()
}

/// [`tokio::process::Command::spawn`], one start at a time.
#[allow(clippy::disallowed_methods)]
pub fn spawn(command: &mut tokio::process::Command) -> io::Result<tokio::process::Child> {
    one_at_a_time(|| command.spawn())
}

/// [`tokio::process::Command::output`]: stdout and stderr captured, as it captures them. Only
/// the start is one at a time; the output is read after the lock is let go.
pub async fn output(command: &mut tokio::process::Command) -> io::Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    spawn(command)?.wait_with_output().await
}

/// [`std::process::Command::spawn`], one start at a time.
#[allow(clippy::disallowed_methods)]
pub fn spawn_std(command: &mut std::process::Command) -> io::Result<std::process::Child> {
    one_at_a_time(|| command.spawn())
}

/// [`std::process::Command::output`] for a command whose stdin and stderr the caller set: its
/// stdout captured.
pub fn output_std(command: &mut std::process::Command) -> io::Result<Output> {
    command.stdout(Stdio::piped());
    spawn_std(command)?.wait_with_output()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// **Only the start is one at a time**: while one child's output is still being read, another
    /// child starts at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_child_being_read_holds_up_no_start() {
        let reading = tokio::spawn(async {
            let mut slow = tokio::process::Command::new("/bin/sh");
            slow.args(["-c", "sleep 2; echo done"]);
            output(&mut slow).await.unwrap()
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!reading.is_finished(), "the slow child is still being read");
        let began = Instant::now();
        let mut quick = tokio::process::Command::new("/usr/bin/true");
        let status = spawn(&mut quick).unwrap().wait().await.unwrap();
        assert!(status.success());
        assert!(
            began.elapsed() < Duration::from_millis(500),
            "a start waited {:?} on a child being read",
            began.elapsed()
        );
        assert_eq!(reading.await.unwrap().stdout, b"done\n");
    }
}
