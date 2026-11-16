//! What an agent prints before its terminal attaches.
//!
//! The copy is this binary's [`SUBCOMMAND`], which keeps the last [`LIMIT`] bytes
//! — where an error is — and writes them when the pane's output ends. It does
//! its own keeping rather than run `tail -c`: uutils' `tail`, Ubuntu's since
//! 26.04, keeps more than it is asked to when its input arrives in short reads.
//! When the session ends, the run's supervisor waits for the launcher's lock on
//! the copy and then removes it, so a launcher that dies without cleaning up, its
//! tab closed, leaves nothing behind.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};

/// The hidden subcommand `pipe-pane` runs to keep the copy.
pub const SUBCOMMAND: &str = "internal-early-output";

/// The copy's directory in the session's.
const DIRECTORY: &str = "early-output";
/// Locked by the launcher for as long as it lives; the kernel lets go of it
/// however the launcher ends.
const LAUNCHER: &str = "launcher";

/// The most of an agent's early output the copy keeps.
const LIMIT: usize = 1 << 20;

/// Remove a run's copy once its launcher is gone. The run's supervisor calls this
/// when the session has ended; a launcher still holding its lock still needs the
/// copy, so this waits for it to finish — however it finishes, since the kernel
/// releases the lock — and removes what it left.
pub fn discard(session_dir: &Path) {
    let dir = session_dir.join(DIRECTORY);
    let Ok(lock) = std::fs::File::open(dir.join(LAUNCHER)) else {
        return;
    };
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `codeconnect internal-early-output`: the last [`LIMIT`] bytes of standard
/// input, written to standard output once it ends.
pub fn run() -> Result<()> {
    keep_last(
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        LIMIT,
    )
}

/// Read `input` to its end through a ring of `limit` bytes, then write what the
/// ring holds, oldest first.
fn keep_last(input: &mut impl Read, output: &mut impl Write, limit: usize) -> Result<()> {
    let mut ring = vec![0u8; limit];
    let (mut end, mut wrapped) = (0, false);
    loop {
        let read = match input.read(&mut ring[end..]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err).context("reading the pane's output"),
        };
        end += read;
        if end == limit {
            (end, wrapped) = (0, true);
        }
    }
    if wrapped {
        output.write_all(&ring[end..])?;
    }
    output.write_all(&ring[..end])?;
    output.flush().context("writing the early output")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads of every size, as a pipe hands over bursts: the case uutils'
    /// `tail -c` keeps too much of.
    #[test]
    fn the_copy_keeps_exactly_the_last_bytes_whatever_the_reads() {
        struct Uneven<'a>(&'a [u8], usize);
        impl Read for Uneven<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let size = [1, 3, 700, 2, 1999, 64][self.1 % 6]
                    .min(buf.len())
                    .min(self.0.len());
                self.1 += 1;
                buf[..size].copy_from_slice(&self.0[..size]);
                self.0 = &self.0[size..];
                Ok(size)
            }
        }
        let input: Vec<u8> = (0..9_000u32).map(|i| (i % 251) as u8).collect();
        for (len, limit) in [(9_000, 1_000), (9_000, 9_000), (500, 1_000), (0, 1_000)] {
            let mut kept = Vec::new();
            keep_last(&mut Uneven(&input[..len], 0), &mut kept, limit).unwrap();
            assert_eq!(
                kept,
                input[len.saturating_sub(limit)..len],
                "{len} bytes, keeping {limit}"
            );
        }
    }
}
