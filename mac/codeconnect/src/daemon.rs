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
    request_at(&protocol::socket_path(), frame, timeout)
}

/// The same, for a request whose reply waits on a person: the daemon answers
/// `ask_folder_access` once every macOS dialog it raised has been clicked, so
/// the reply is read with no timeout. Writing it stays bounded.
#[cfg(target_os = "macos")]
pub fn request_waiting(frame: &ClientFrame) -> Result<DaemonFrame> {
    request_waiting_at(&protocol::socket_path(), frame)
}

#[cfg(target_os = "macos")]
fn request_waiting_at(socket: &std::path::Path, frame: &ClientFrame) -> Result<DaemonFrame> {
    let mut link = DaemonLink::connect(socket, TIMEOUT)?;
    link.stream.get_ref().set_read_timeout(None)?;
    link.request(frame)
}

/// The same, to the daemon at `socket`.
pub fn request_at(
    socket: &std::path::Path,
    frame: &ClientFrame,
    timeout: Duration,
) -> Result<DaemonFrame> {
    DaemonLink::connect(socket, timeout)?.request(frame)
}

/// One connection to the daemon, for an exchange of more than one frame: the
/// `codeconnect` tool's `read_messages` confirms on the same connection that the
/// agent was given what it read.
/// **ccd took a frame and no reply came back**: it closed, or stopped answering,
/// after the frame was written to it, so what the frame asked may have been done.
#[derive(Debug)]
pub struct NoAnswer(String);

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoAnswer {}

pub struct DaemonLink {
    stream: BufReader<UnixStream>,
}

impl DaemonLink {
    pub fn connect(socket: &std::path::Path, timeout: Duration) -> Result<DaemonLink> {
        let stream = UnixStream::connect(socket).with_context(|| {
            // With no systemd, `daemon install` refuses: name the line that starts it by hand.
            #[cfg(target_os = "linux")]
            let start = crate::service::no_systemd().map_or_else(
                || "codeconnect daemon install".to_string(),
                |_| crate::service::hand_run_line(),
            );
            #[cfg(not(target_os = "linux"))]
            let start = "codeconnect daemon install";
            format!(
                "cannot reach the CodeConnect daemon at {}: start it with \
                 `{start}`, or check it with `codeconnect daemon status`",
                socket.display()
            )
        })?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(DaemonLink {
            stream: BufReader::new(stream),
        })
    }

    /// Send one frame and read the daemon's one reply.
    pub fn request(&mut self, frame: &ClientFrame) -> Result<DaemonFrame> {
        let mut line = serde_json::to_vec(frame)?;
        line.push(b'\n');
        let stream = self.stream.get_mut();
        stream.write_all(&line).context("writing to ccd")?;
        stream.flush().context("flushing to ccd")?;

        let mut response = String::new();
        let read = self
            .stream
            .read_line(&mut response)
            .map_err(|err| NoAnswer(format!("reading ccd's reply: {err}")))?;
        if read == 0 {
            return Err(NoAnswer("ccd closed the connection without replying".into()).into());
        }

        match serde_json::from_str::<DaemonFrame>(response.trim())? {
            // Surfaced here so no caller has to remember to check for it.
            DaemonFrame::Error { message } => bail!("{message}"),
            frame => Ok(frame),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn socket(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cc-preflight-{tag}-{}-{}.sock",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// The folder question is answered once a person has clicked each dialog,
    /// which takes longer than the round trip every other request is held to.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_folder_question_waits_for_the_person_answering_it() {
        const FOLDERS: &str = "{\"type\":\"folders\",\"folders\":[{\"folder\":\"Desktop\",\
                               \"outcome\":{\"status\":\"allowed\"}}]}";
        let slow_daemon = |path: &std::path::Path| {
            let listener = UnixListener::bind(path).expect("bind");
            std::thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept");
                let mut out = stream.try_clone().expect("clone");
                let mut line = String::new();
                let _ = BufReader::new(stream).read_line(&mut line);
                std::thread::sleep(TIMEOUT + Duration::from_secs(2));
                let _ = writeln!(out, "{FOLDERS}");
                line
            })
        };

        let path = socket("folders-bounded");
        let daemon = slow_daemon(&path);
        let bounded = request_at(&path, &ClientFrame::AskFolderAccess, TIMEOUT);
        assert!(
            bounded.is_err(),
            "an ordinary request gives up: {bounded:?}"
        );
        let _ = daemon.join();
        let _ = std::fs::remove_file(&path);

        let path = socket("folders-waiting");
        let daemon = slow_daemon(&path);
        let reply = request_waiting_at(&path, &ClientFrame::AskFolderAccess).expect("the reply");
        assert!(matches!(reply, DaemonFrame::Folders { ref folders } if folders.len() == 1));
        assert!(daemon.join().unwrap().contains("ask_folder_access"));
        let _ = std::fs::remove_file(&path);
    }
}
