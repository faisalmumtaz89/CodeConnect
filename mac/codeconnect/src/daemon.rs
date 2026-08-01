//! Talking to `ccd` over the unix socket.
//!
//! One request, one reply, one connection. The shim is a short-lived process
//! and the daemon may be down at any moment, so every call here is bounded in
//! time and fails with a sentence the operator can act on rather than an
//! io::Error.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use protocol::ipc::{ClientFrame, DaemonFrame};

/// Generous next to a local socket round-trip, but bounded: `codeconnect pair` must fail
/// with a message rather than hang at a terminal the user is waiting at.
const TIMEOUT: Duration = Duration::from_secs(5);

pub fn request(frame: &ClientFrame) -> Result<DaemonFrame> {
    request_within(frame, TIMEOUT)
}

/// The same, for the one request that legitimately takes longer than a
/// round-trip.
///
/// `codeconnect sessions prune` deletes across seven tables in one transaction, and on a
/// machine with a long history that is real work. Timing it out at five seconds
/// would abandon the *reply* while the daemon carried on committing, leaving the
/// operator with no idea whether their history had been removed — which is the
/// one thing this command must never be ambiguous about.
pub fn request_within(frame: &ClientFrame, timeout: Duration) -> Result<DaemonFrame> {
    let socket = protocol::socket_path();
    let mut stream = UnixStream::connect(&socket).with_context(|| {
        format!(
            "cannot reach ccd at {}; is the daemon running?",
            socket.display()
        )
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    stream.write_all(&line).context("writing to ccd")?;
    stream.flush().context("flushing to ccd")?;

    let mut response = String::new();
    let read = BufReader::new(stream)
        .read_line(&mut response)
        .context("reading ccd's reply")?;
    if read == 0 {
        bail!("ccd closed the connection without replying");
    }

    match serde_json::from_str::<DaemonFrame>(response.trim())? {
        // Surfaced here so no caller has to remember to check for it.
        DaemonFrame::Error { message } => bail!("{message}"),
        frame => Ok(frame),
    }
}
