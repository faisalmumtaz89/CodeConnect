//! What an agent prints before its terminal attaches, kept so an agent that ends
//! first is not lost.
//!
//! `codeconnect claude` starts the agent, may hold the terminal for a note, and
//! only then attaches. An agent that ends in between — `--version`, `--help`, a
//! mistyped flag, a startup error — used to take everything it printed with its
//! session, and the attach then failed on a session that no longer existed.
//!
//! So tmux copies the pane's output into a private file, from the same command
//! that creates the session: tmux runs a command list without reading any pane
//! output between its commands, so the copy starts before the first byte is read.
//! The copy keeps the last [`LIMIT`] bytes — where an error is — and writes them
//! when the pane's output ends. The launcher holds a lock on the copy for as long
//! as it lives; when the session ends, the run's supervisor waits for that lock —
//! the launcher may be in its note, about to show what an ended agent printed —
//! and then removes the copy, so a launcher that dies without cleaning up, its
//! tab closed, leaves nothing behind. When the launcher's attach has painted the
//! pane, tmux's own record of it is on the terminal and the copy is stopped and
//! removed; a copy whose launcher died before that runs until the session ends.
//! When the session ends before the paint, the copy is written to the terminal as
//! the agent drew it — less the questions it asked the terminal, whose asker is
//! gone and whose answers would otherwise be typed into the user's shell.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// The copy's directory in the session's, its bytes, and the mark its writer
/// leaves once it has written them all.
const DIRECTORY: &str = "early-output";
const OUTPUT: &str = "output";
/// Locked by the launcher for as long as it lives; the kernel lets go of it
/// however the launcher ends.
const LAUNCHER: &str = "launcher";
const DONE: &str = "done";

/// The most of an agent's early output the copy keeps.
const LIMIT: usize = 1 << 20;

/// How long a replay waits for the copy to finish. tmux keeps a pane until its
/// copy has been handed every byte, so once the session is gone only the
/// writer's one write is outstanding.
const COMPLETION_WAIT: Duration = Duration::from_secs(2);

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

/// One launch's copy: a private directory under the session's own, removed when
/// this is dropped — which also stops a late writer from leaving its mark behind.
pub struct EarlyOutput {
    dir: PathBuf,
    /// The launcher's lock on the copy, held until this is dropped.
    _lock: Option<std::fs::File>,
}

impl EarlyOutput {
    /// A new, empty copy in `session_dir`: the directory owner-only and new, the
    /// file created here rather than by the writer, so it is owner-only whatever
    /// the tmux server's umask, and never a link someone left in its place.
    pub fn create(session_dir: &Path) -> Result<Self> {
        let dir = session_dir.join(DIRECTORY);
        DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let mut early = EarlyOutput { dir, _lock: None };
        let lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(early.dir.join(LAUNCHER))
            .context("creating the early-output lock")?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error()).context("locking the early-output copy");
        }
        early._lock = Some(lock);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(early.dir.join(OUTPUT))
            .context("creating the early-output file")?;
        Ok(early)
    }

    /// The command `pipe-pane` runs. tmux expands formats and `strftime`
    /// sequences in it before `/bin/sh -c` sees it, so the paths are shell-quoted
    /// first and then have `#` and `%` doubled, which both expansions undo.
    pub fn pipe_command(&self) -> Result<String> {
        let path = |name: &str| -> Result<String> {
            let path = self.dir.join(name);
            let path = path
                .to_str()
                .with_context(|| format!("{} is not UTF-8", path.display()))?;
            Ok(tmux_literal(&crate::settings::shell_quote(path)))
        };
        Ok(format!(
            "/usr/bin/tail -c {LIMIT} >{}; : >{}",
            path(OUTPUT)?,
            path(DONE)?
        ))
    }

    /// Where the copy is, for removing it as soon as the attach has painted.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The session ended before anything painted it: write what it drew to `out`.
    pub fn replay(self, out: &mut impl Write) -> Result<()> {
        let deadline = Instant::now() + COMPLETION_WAIT;
        while !self.dir.join(DONE).exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let bytes =
            std::fs::read(self.dir.join(OUTPUT)).context("reading what the agent printed")?;
        out.write_all(&without_queries(&bytes))?;
        out.flush()?;
        Ok(())
    }
}

impl Drop for EarlyOutput {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `value` as tmux's format and time expansion will hand it on: `#` and `%`
/// doubled, the only characters either expansion treats specially.
fn tmux_literal(value: &str) -> String {
    value.replace('#', "##").replace('%', "%%")
}

/// `bytes` without the questions they ask the terminal: device attributes,
/// status and cursor reports, version, mode and window reports, colour and
/// clipboard reads, setting and capability reads. Everything else — text, colour,
/// cursor movement, screen modes, titles — is kept byte for byte. A sequence cut
/// off by the end of `bytes` is dropped: a terminal left inside one would read
/// the shell's prompt as part of it.
pub(crate) fn without_queries(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != 0x1b {
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        let rest = &bytes[at + 1..];
        let length = match rest.first() {
            Some(b'[') => match rest[1..].iter().position(|b| (0x40..=0x7e).contains(b)) {
                Some(end) => {
                    let body = &rest[1..end + 2];
                    let length = end + 3;
                    if !csi_query(body) {
                        out.extend_from_slice(&bytes[at..at + length]);
                    }
                    length
                }
                None => break,
            },
            Some(&kind @ (b']' | b'P')) => match string_end(&rest[1..], kind == b']') {
                Some((body_end, end)) => {
                    let body = &rest[1..body_end + 1];
                    let length = end + 2;
                    if kind == b'P' && body.starts_with(b"tmux;") {
                        out.extend(without_queries(&undoubled(&body[5..])));
                    } else if !(kind == b']' && osc_query(body) || kind == b'P' && dcs_query(body))
                    {
                        out.extend_from_slice(&bytes[at..at + length]);
                    }
                    length
                }
                None => break,
            },
            Some(_) => {
                out.push(0x1b);
                1
            }
            None => break,
        };
        at += length;
    }
    out
}

/// Whether a control sequence — its parameters, intermediates and final byte —
/// asks the terminal for an answer.
fn csi_query(body: &[u8]) -> bool {
    let (&last, rest) = body.split_last().expect("a final byte");
    let (private, rest) = match rest.first() {
        Some(&marker @ (b'<' | b'=' | b'>' | b'?')) => (Some(marker), &rest[1..]),
        _ => (None, rest),
    };
    let split = rest
        .iter()
        .position(|b| (0x20..=0x2f).contains(b))
        .unwrap_or(rest.len());
    let (params, intermediates) = rest.split_at(split);
    let first = params.split(|&b| b == b';').next().unwrap_or(b"");
    match (last, private, intermediates) {
        // Device attributes, primary, secondary and tertiary.
        (b'c', _, []) => true,
        // Device status and cursor position reports.
        (b'n', None | Some(b'?'), []) => true,
        // XTVERSION.
        (b'q', Some(b'>'), []) => true,
        // DECRQM, DECRQTSR, DECRQPSR.
        (b'p' | b'u' | b'w', _, b"$") => true,
        // Window reports; 22 and 23 push and pop the title and ask nothing.
        (b't', None, []) => matches!(
            first,
            b"11" | b"13" | b"14" | b"15" | b"16" | b"18" | b"19" | b"20" | b"21"
        ),
        // XTSMGRAPHICS, kitty's keyboard flags, XTQMODKEYS.
        (b'S' | b'u' | b'm', Some(b'?'), []) => true,
        _ => false,
    }
}

/// An OSC that reads rather than sets: a colour, the clipboard, any field that
/// is `?`.
fn osc_query(body: &[u8]) -> bool {
    body.split(|&b| b == b';')
        .skip(1)
        .any(|field| field == b"?")
}

/// A DCS that reads a setting (DECRQSS) or a capability (XTGETTCAP), or the
/// stop marker [`crate::job`] writes for the attached viewer.
fn dcs_query(body: &[u8]) -> bool {
    let marker = &crate::job::STOP_MARKER[2..crate::job::STOP_MARKER.len() - 2];
    body.starts_with(b"$q") || body.starts_with(b"+q") || body == marker
}

/// Where a string sequence's body ends and the whole sequence ends, counted from
/// its body's start: ESC `\`, or BEL for an OSC. A doubled ESC is a tmux
/// passthrough's escaped one and ends nothing.
fn string_end(body: &[u8], bel_ends: bool) -> Option<(usize, usize)> {
    let mut at = 0;
    while at < body.len() {
        match (body[at], body.get(at + 1)) {
            (0x07, _) if bel_ends => return Some((at, at + 1)),
            (0x1b, Some(&b'\\')) => return Some((at, at + 2)),
            (0x1b, Some(&0x1b)) => at += 2,
            _ => at += 1,
        }
    }
    None
}

/// A tmux passthrough's payload, its doubled ESCs single again.
fn undoubled(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut at = 0;
    while at < payload.len() {
        out.push(payload[at]);
        at += if payload[at] == 0x1b && payload.get(at + 1) == Some(&0x1b) {
            2
        } else {
            1
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_colour_movement_and_screen_modes_are_kept_whole() {
        let drawn: &[u8] =
            b"claude 2.1.300\r\n\x1b[1;32mok\x1b[0m\x1b[2J\x1b[H\x1b[?1049h\x1b[?1049l\x1b]0;title\x07\x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\\x1b[3 q";
        assert_eq!(without_queries(drawn), drawn);
    }

    #[test]
    fn every_question_to_the_terminal_is_removed() {
        let asked: &[u8] = b"a\x1b[c\x1b[0c\x1b[>c\x1b[=c\x1b[5n\x1b[6n\x1b[?6n\x1b[?996n\x1b[>q\
\x1b[?2026$p\x1b[4$p\x1b[14t\x1b[18t\x1b[?1;1;0S\x1b[?u\x1b[?4m\
\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]4;1;?\x07\x1b]52;c;?\x07\x1bP$qm\x1b\\\x1bP+q544e\x1b\\b";
        assert_eq!(without_queries(asked), b"ab");
    }

    #[test]
    fn sequences_that_only_look_like_questions_are_kept() {
        // A title push, kitty's keyboard push, a cursor style, XTMODKEYS, an OSC
        // that sets a colour.
        let set: &[u8] = b"\x1b[22;0t\x1b[>1u\x1b[2 q\x1b[>4;1m\x1b]11;#000000\x07";
        assert_eq!(without_queries(set), set);
    }

    #[test]
    fn a_passthrough_is_unwrapped_and_its_questions_removed() {
        assert_eq!(
            without_queries(b"\x1bPtmux;\x1b\x1b]9;done\x07\x1b\x1b[c\x1b\\x"),
            b"\x1b]9;done\x07x"
        );
    }

    #[test]
    fn the_stop_marker_is_not_shown() {
        let mut bytes = b"a".to_vec();
        bytes.extend_from_slice(crate::job::STOP_MARKER);
        bytes.push(b'b');
        assert_eq!(without_queries(&bytes), b"ab");
    }

    #[test]
    fn a_sequence_cut_off_at_the_end_is_dropped() {
        assert_eq!(without_queries(b"done\x1b[1;3"), b"done");
        assert_eq!(without_queries(b"done\x1b]0;tit"), b"done");
        assert_eq!(without_queries(b"done\x1b"), b"done");
    }

    #[test]
    fn hashes_and_percents_survive_tmux_expansion_by_being_doubled() {
        assert_eq!(tmux_literal("'/a b/%d#{pane_id}'"), "'/a b/%%d##{pane_id}'");
    }

    fn scratch_session(tag: &str) -> PathBuf {
        let session = std::env::temp_dir().join(format!(
            "cc-early-{tag}-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir(&session).unwrap();
        session
    }

    /// The supervisor's clean-up at session end never takes a copy from a
    /// launcher still holding it — one in its note, about to show an agent that
    /// ended — and removes it once that launcher is gone, even one that never
    /// cleaned up; one nobody holds goes at once.
    #[test]
    fn a_copy_is_discarded_only_once_its_launcher_is_gone() {
        let session = scratch_session("held");
        let mut early = EarlyOutput::create(&session).unwrap();
        let waiter = {
            let session = session.clone();
            std::thread::spawn(move || discard(&session))
        };
        std::thread::sleep(Duration::from_millis(300));
        assert!(!waiter.is_finished(), "the clean-up waits for the launcher");
        assert!(early.dir().exists(), "a held copy is kept for its launcher");
        // The launcher ends without its own clean-up, as one hung up does.
        drop(early._lock.take());
        std::mem::forget(early);
        waiter.join().unwrap();
        assert!(
            !session.join(DIRECTORY).exists(),
            "the copy goes with its launcher"
        );
        std::fs::remove_dir(&session).unwrap();

        let session = scratch_session("left");
        let dir = session.join(DIRECTORY);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join(LAUNCHER), b"").unwrap();
        std::fs::write(dir.join(OUTPUT), b"left behind").unwrap();
        discard(&session);
        assert!(!dir.exists(), "a copy nobody holds is removed");
        std::fs::remove_dir(&session).unwrap();
    }

    #[test]
    fn the_copy_is_owner_only_new_and_removed_with_its_directory() {
        use std::os::unix::fs::PermissionsExt;
        let session = std::env::temp_dir().join(format!(
            "cc-early-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir(&session).unwrap();
        let early = EarlyOutput::create(&session).unwrap();
        let dir = session.join("early-output");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(OUTPUT)), 0o600);
        assert!(
            EarlyOutput::create(&session).is_err(),
            "a second copy never reuses the first's directory"
        );
        drop(early);
        assert!(!dir.exists(), "dropping the copy removes it");
        std::fs::remove_dir(&session).unwrap();
    }
}
