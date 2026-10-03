//! A viewer's line to `ccd`: whether its terminal tab is in front, and whether a
//! background agent's question in its session is held for the phone.
//!
//! The viewer registers once per connection ([`ClientFrame::Viewer`]), then reports
//! focus changes and keys typed while a question is held; the daemon answers with
//! [`DaemonFrame::HiddenHold`]. Nothing here may stall the viewer's byte pipe, so the
//! daemon is read on a thread of its own and every write is non-blocking: a daemon
//! that stops reading loses the connection. A connection that fails or ends, or a
//! daemon that answers anything else (an older `ccd` says `error`), means no hold,
//! at once, so a dead daemon never eats a key; the link is tried again every
//! [`RETRY`] until the viewer ends.

use std::io::{BufRead, BufReader};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use protocol::ipc::{ClientFrame, DaemonFrame};

/// How long a viewer waits before it tries the daemon again.
const RETRY: Duration = Duration::from_secs(2);

/// The link, for as long as the viewer shows its session.
pub struct ViewerLink {
    link: Arc<Mutex<Link>>,
}

struct Link {
    stream: Option<UnixStream>,
    focused: bool,
    held: bool,
    /// When the daemon last ended a hold. Not when the link was lost.
    released_at: Option<Instant>,
    ended: bool,
}

impl ViewerLink {
    /// Link the viewer of `tmux_session` (tmux's `$N`), whose tmux client is
    /// `client_pid`, to the daemon at `socket`. The terminal starts in front.
    pub fn start(socket: PathBuf, tmux_session: String, client_pid: i32) -> Self {
        let link = Arc::new(Mutex::new(Link {
            stream: None,
            focused: true,
            held: false,
            released_at: None,
            ended: false,
        }));
        let hello = ClientFrame::Viewer {
            tmux_session,
            client_pid,
        };
        let shared = Arc::clone(&link);
        std::thread::spawn(move || keep_linked(&shared, &socket, &hello));
        ViewerLink { link }
    }

    /// Whether what is typed is to be swallowed.
    pub fn held(&self) -> bool {
        lock(&self.link).held
    }

    /// When the daemon last let a held question go to the Mac, or the phone
    /// answered it.
    pub fn released_at(&self) -> Option<Instant> {
        lock(&self.link).released_at
    }

    pub fn focus(&self, focused: bool) {
        let mut link = lock(&self.link);
        if link.focused != focused {
            link.focused = focused;
            link.send(&ClientFrame::ViewerFocus { focused });
        }
    }

    /// Something was typed while a question was held.
    pub fn key(&self) {
        lock(&self.link).send(&ClientFrame::ViewerKey);
    }
}

impl Drop for ViewerLink {
    fn drop(&mut self) {
        let mut link = lock(&self.link);
        link.ended = true;
        link.close();
    }
}

impl Link {
    fn send(&mut self, frame: &ClientFrame) {
        let Some(stream) = &self.stream else {
            return;
        };
        let mut line = serde_json::to_vec(frame).expect("a frame serialises");
        line.push(b'\n');
        let sent = unsafe {
            libc::send(
                stream.as_raw_fd(),
                line.as_ptr().cast(),
                line.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if sent != line.len() as isize {
            self.close();
        }
    }

    fn close(&mut self) {
        self.held = false;
        if let Some(stream) = self.stream.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn lock(link: &Mutex<Link>) -> MutexGuard<'_, Link> {
    link.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn keep_linked(link: &Mutex<Link>, socket: &std::path::Path, hello: &ClientFrame) {
    loop {
        if let Some(reader) = connect(link, socket, hello) {
            for line in BufReader::new(reader).lines() {
                let Ok(Ok(DaemonFrame::HiddenHold { held })) =
                    line.map(|line| serde_json::from_str(&line))
                else {
                    break;
                };
                let mut link = lock(link);
                if link.stream.is_none() {
                    break;
                }
                if link.held && !held {
                    link.released_at = Some(Instant::now());
                }
                link.held = held;
            }
            lock(link).close();
        }
        std::thread::sleep(RETRY);
        if lock(link).ended {
            return;
        }
    }
}

/// Connect and register; the half to read the daemon from, or `None`.
fn connect(
    link: &Mutex<Link>,
    socket: &std::path::Path,
    hello: &ClientFrame,
) -> Option<UnixStream> {
    let stream = UnixStream::connect(socket).ok()?;
    let reader = stream.try_clone().ok()?;
    let mut link = lock(link);
    if link.ended {
        return None;
    }
    link.stream = Some(stream);
    let focused = link.focused;
    link.send(hello);
    link.send(&ClientFrame::ViewerFocus { focused });
    Some(reader)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    fn socket(name: &str) -> (PathBuf, UnixListener) {
        let dir = std::env::temp_dir().join(format!("ccv-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        (path, listener)
    }

    fn accept(listener: &UnixListener) -> UnixStream {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    return stream;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "the viewer never connected");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }

    fn frames(stream: &UnixStream, count: usize) -> Vec<serde_json::Value> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        (0..count)
            .map(|_| {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                serde_json::from_str(&line).unwrap()
            })
            .collect()
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_viewer_registers_reports_focus_and_follows_the_hold() {
        let (path, listener) = socket("hold");
        let link = ViewerLink::start(path, "$3".into(), 4242);
        let mut daemon = accept(&listener);
        assert_eq!(
            frames(&daemon, 2),
            [
                serde_json::json!({"type": "viewer", "tmux_session": "$3", "client_pid": 4242}),
                serde_json::json!({"type": "viewer_focus", "focused": true}),
            ]
        );
        assert!(!link.held());
        daemon
            .write_all(b"{\"type\":\"hidden_hold\",\"held\":true}\n")
            .unwrap();
        until("held", || link.held());
        link.focus(false);
        link.focus(false);
        link.key();
        assert_eq!(
            frames(&daemon, 2),
            [
                serde_json::json!({"type": "viewer_focus", "focused": false}),
                serde_json::json!({"type": "viewer_key"}),
            ]
        );
        assert!(link.released_at().is_none());
        daemon
            .write_all(b"{\"type\":\"hidden_hold\",\"held\":false}\n")
            .unwrap();
        until("released", || !link.held());
        assert!(link.released_at().is_some(), "the daemon ended the hold");
    }

    /// A daemon that goes away while a question is held must not leave the
    /// viewer swallowing keys; the viewer links again, with its focus as it is.
    #[test]
    fn the_hold_ends_with_the_connection_and_the_link_comes_back() {
        let (path, listener) = socket("drop");
        let link = ViewerLink::start(path, "$1".into(), 7);
        let mut daemon = accept(&listener);
        frames(&daemon, 2);
        daemon
            .write_all(b"{\"type\":\"hidden_hold\",\"held\":true}\n")
            .unwrap();
        until("held", || link.held());
        link.focus(false);
        drop(daemon);
        until("released when the daemon goes", || !link.held());
        assert!(
            link.released_at().is_none(),
            "a lost daemon releases nothing: keys pass at once"
        );
        let again = accept(&listener);
        assert_eq!(
            frames(&again, 2),
            [
                serde_json::json!({"type": "viewer", "tmux_session": "$1", "client_pid": 7}),
                serde_json::json!({"type": "viewer_focus", "focused": false}),
            ]
        );
    }

    /// An older daemon cannot read the frames and says so; that is no link, and
    /// it is tried again no sooner than the retry.
    #[test]
    fn an_older_daemon_is_no_link_and_is_not_hammered() {
        let (path, listener) = socket("old");
        let link = ViewerLink::start(path, "$1".into(), 7);
        let mut daemon = accept(&listener);
        frames(&daemon, 1);
        let refused = Instant::now();
        daemon
            .write_all(b"{\"type\":\"error\",\"message\":\"undecodable frame\"}\n")
            .unwrap();
        daemon
            .write_all(b"{\"type\":\"hidden_hold\",\"held\":true}\n")
            .unwrap();
        accept(&listener);
        let waited = refused.elapsed();
        assert!(waited >= Duration::from_secs(2), "{waited:?}");
        assert!(!link.held());
    }

    /// Once the viewer ends, the daemon sees the connection close.
    #[test]
    fn ending_the_viewer_closes_its_connection() {
        let (path, listener) = socket("end");
        let link = ViewerLink::start(path, "$1".into(), 7);
        let daemon = accept(&listener);
        frames(&daemon, 2);
        drop(link);
        daemon
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut rest = String::new();
        let read = BufReader::new(daemon).read_line(&mut rest).unwrap();
        assert_eq!(read, 0, "{rest:?}");
    }
}
