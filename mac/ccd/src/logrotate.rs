//! Size-capped rotation of the daemon's own launchd logs.
//!
//! ## Why the daemon does this and not `newsyslog`
//!
//! launchd opens `StandardOutPath` / `StandardErrorPath` itself and holds those
//! descriptors for the life of the process. Rotating by rename — what every log
//! rotator does — moves the *name*, not the descriptor, so launchd carries on
//! appending to an unlinked inode: the visible log stops growing, the disk keeps
//! filling, and nothing anywhere says so. The only rotation that works against a
//! held descriptor is **copy-and-truncate**, and the only process guaranteed to
//! be around to do it is the one being logged.
//!
//! That is why the paths live in `protocol` rather than in this file: the plist
//! writer and this rotator have to name the same two files, and there is no
//! runtime symptom if they drift.
//!
//! ## Why truncation is safe here
//!
//! launchd opens both files `O_APPEND`. An appending write is positioned by the
//! kernel at the current end of file, so after `set_len(0)` the next line lands
//! at offset 0 rather than leaving an 8MB hole. Truncating a file another
//! process is appending to costs at most a torn line at the boundary, which is a
//! price worth one bounded log.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

/// What was done to one file, so the caller can report it honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// Below the cap, or not present (a daemon in the foreground has no files).
    Untouched,
    /// Copied to `<name>.1` and truncated. Carries the size that was archived.
    Rotated { bytes: u64 },
}

/// Watch the daemon's own logs and keep each one under `cap_bytes`.
pub async fn run(cap_bytes: u64, interval: Duration) {
    let paths = [protocol::daemon_stdout_log(), protocol::daemon_stderr_log()];
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        for path in &paths {
            let path = path.clone();
            // Copying up to the cap is blocking IO; a busy daemon must not stall
            // its runtime on it.
            let result = tokio::task::spawn_blocking(move || rotate(&path, cap_bytes)).await;
            match result {
                Ok(Ok(Rotation::Rotated { bytes })) => {
                    // Written *after* the truncation, so it becomes the first
                    // line of the new file and the reader knows what came
                    // before it rather than finding an inexplicably short log.
                    crate::log_info!(
                        "rotated the daemon log at {bytes} bytes; the previous contents are in \
                         the .1 file next to it"
                    );
                }
                Ok(Ok(Rotation::Untouched)) => {}
                Ok(Err(err)) => crate::log_warn!("log rotation failed: {err:#}"),
                Err(err) => crate::log_warn!("log rotation task failed: {err}"),
            }
        }
    }
}

/// Archive and truncate `path` if it is over `cap_bytes`.
///
/// One generation is kept. Two would double the ceiling for a file nobody reads
/// twice; the interesting content is always the tail, and that is the half this
/// keeps live.
pub fn rotate(path: &Path, cap_bytes: u64) -> Result<Rotation> {
    let size = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        // Not an error: running `ccd` in a terminal writes to the terminal, and
        // these files simply do not exist.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Rotation::Untouched),
        Err(err) => return Err(err).context(format!("stat {}", path.display())),
    };
    if size <= cap_bytes {
        return Ok(Rotation::Untouched);
    }

    let archive = archive_path(path);
    std::fs::copy(path, &archive)
        .with_context(|| format!("copying {} to {}", path.display(), archive.display()))?;

    // Opened for writing without `truncate(true)`: `set_len` is the explicit
    // operation, and `File::create` would race the copy above by emptying the
    // file before it is safely archived.
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} to truncate it", path.display()))?
        .set_len(0)
        .with_context(|| format!("truncating {}", path.display()))?;

    Ok(Rotation::Rotated { bytes: size })
}

fn archive_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_log(contents: &[u8]) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ccd-rotate-{}-{}-{}.log",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            protocol::time::now_unix_ms()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn a_small_log_is_left_alone() {
        let path = temp_log(b"one line\n");
        assert_eq!(rotate(&path, 1024).unwrap(), Rotation::Untouched);
        assert_eq!(std::fs::read(&path).unwrap(), b"one line\n");
        assert!(!archive_path(&path).exists(), "nothing to archive yet");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_oversized_log_is_archived_and_emptied() {
        let path = temp_log(&b"x".repeat(2048));
        assert_eq!(
            rotate(&path, 1024).unwrap(),
            Rotation::Rotated { bytes: 2048 }
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        // Nothing is lost: the previous contents are one file away.
        assert_eq!(std::fs::read(archive_path(&path)).unwrap().len(), 2048);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(archive_path(&path));
    }

    #[test]
    fn an_appending_writer_keeps_working_across_a_rotation() {
        // The property the whole design rests on: launchd holds this descriptor
        // open across the truncation, and an O_APPEND write must land at offset
        // 0 afterwards rather than leaving a hole the size of the old file.
        let path = temp_log(b"");
        let mut held = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        held.write_all(&b"o".repeat(2048)).unwrap();
        held.flush().unwrap();

        assert!(matches!(
            rotate(&path, 1024).unwrap(),
            Rotation::Rotated { .. }
        ));

        held.write_all(b"after\n").unwrap();
        held.flush().unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"after\n",
            "an appending writer must resume at the start of the emptied file"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(archive_path(&path));
    }

    #[test]
    fn rotating_twice_keeps_one_generation_and_stays_bounded() {
        let path = temp_log(&b"a".repeat(2048));
        rotate(&path, 1024).unwrap();
        std::fs::write(&path, b"b".repeat(2048)).unwrap();
        rotate(&path, 1024).unwrap();

        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        // The older generation is replaced rather than accumulated, so total
        // usage is capped at two files however long the daemon runs.
        assert_eq!(
            std::fs::read(archive_path(&path)).unwrap(),
            b"b".repeat(2048)
        );
        assert!(!archive_path(&archive_path(&path)).exists());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(archive_path(&path));
    }

    #[test]
    fn a_missing_log_is_not_an_error() {
        // The foreground case: `ccd` in a terminal has no log files at all, and
        // that must not produce a warning every tick.
        let path = std::env::temp_dir().join("ccd-rotate-does-not-exist.log");
        let _ = std::fs::remove_file(&path);
        assert_eq!(rotate(&path, 1024).unwrap(), Rotation::Untouched);
    }

    #[test]
    fn the_archive_sits_next_to_the_original() {
        assert_eq!(
            archive_path(Path::new("/tmp/ccd.err.log")),
            PathBuf::from("/tmp/ccd.err.log.1")
        );
    }
}
