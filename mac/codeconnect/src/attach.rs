//! The session in the user's own terminal: a tmux control-mode client.
//!
//! A normal tmux client redraws the pane on the terminal's alternate screen, so a
//! CLI that draws inline (Claude Code, Codex) is framed full screen instead of the
//! way it runs natively. This client never redraws. It attaches with `tmux -C`,
//! writes each `%output` payload of the viewed pane to the terminal byte for byte,
//! types the terminal's input into the pane with `send-keys -H` (byte-exact, and
//! never through a prefix or key table), and sizes the window with
//! `refresh-client -C` whenever the terminal is resized.
//!
//! **Terminal queries.** The CLI's queries reach the real terminal inside
//! `%output`, and tmux also answers some of them for the pane itself. Both answers
//! would reach the CLI, the second typed as input. Measured on tmux 3.7b with only
//! a control client attached, tmux answers DA1, DA2, CPR, DSR 5 and 996,
//! XTVERSION, DECRQM, XTWINOPS 14/16/18, XTSMGRAPHICS, DECRQSS and single OSC 10
//! and 11 queries — and not kitty's `CSI ? u`, DA3, OSC 4/12/52, XTGETTCAP. So the
//! queries tmux answers are removed from what reaches the terminal ([`Filter`]),
//! and every other query is the terminal's to answer. Each query gets exactly one
//! answer. tmux's cursor report is in the pane's coordinates, which are the CLI's
//! own. tmux's colour answer is black unless it is told the terminal's, so the
//! client reads the terminal's default colours before attaching and reports them
//! to tmux for the pane (`refresh-client -r`) in the attach itself, before any
//! program in the pane can ask.
//!
//! **Painting on attach.** Control mode streams only what a pane prints next, so
//! the client paints what the pane already shows: its history and screen with
//! colours (only the screen for a program on the alternate screen), the cursor,
//! the modes tmux records for the pane, and its title. Output that arrived before
//! the capture is already in it and is dropped, so no earlier query is replayed;
//! an escape sequence tmux has read only part of is taken from tmux
//! (`capture-pane -P`) and completes when the rest of it streams.
//! The paint leaves the terminal's rows equal to the pane's, so a program that
//! places its cursor by absolute row (Codex) keeps drawing where it drew: it
//! writes the history and the screen's rows in use, then scrolls the terminal up
//! until the pane's first row is the terminal's first. It never walks the cursor
//! over the unused rows below, because Warp keeps any row the cursor has crossed
//! in the block once the program erases it, and the session would then fill the
//! window. A terminal that does not report its cursor gets every row instead.
//!
//! **Ctrl+Z.** The key reaches the agent like any other, and the agent stops itself
//! as it does run directly: it is a job of the pane's own process ([`crate::job`]),
//! which then writes [`crate::job::STOP_MARKER`] and sets `@codeconnect-stopped` on
//! the pane. On the marker, or on a paint that finds the pane stopped, this client
//! gives the terminal back and stops itself, so the user's shell prints its own job
//! line; its tmux client stays attached meanwhile, which is what keeps the agent held.
//! Resumed with `fg`, it takes the terminal again and asks tmux whether the agent is
//! still stopped. If it is and the pane has not moved on, it names the row the shell
//! left the cursor on (`@codeconnect-cursor`) and signals the pane's process, which
//! puts the pane's cursor there and continues the agent, and the agent repaints itself;
//! otherwise the pane is painted afresh first, as on attach.
//!
//! **Focus and held questions.** The client turns focus reporting (DECSET 1004) on
//! in the terminal at attach and on every resume, and tells `ccd` whether the tab
//! is in front ([`crate::viewer_link`]). The terminal's reports are taken out of its
//! input and reach the pane only while the pane has asked for them itself; a pane
//! turning them off never turns them off in the terminal. While `ccd` holds a
//! background agent's question for the phone, what is typed is swallowed and
//! reported instead, so a key meant for the composer never answers a question the
//! user has not seen, and typing goes on being swallowed until it pauses for a
//! second, so the rest of a line that released the question never lands in it.
//! Ctrl+C and Ctrl+Z still reach the pane, in any keyboard mode, so interrupt
//! and suspend keep working.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use protocol::tmux::{reply_block_close, reply_block_open, send_keys_line, ControlLine};

use crate::viewer_link::ViewerLink;

/// How long the terminal has to answer the colour and cursor questions. Every
/// terminal answers DA1, which ends the wait as soon as the others are in.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(1);

/// Asked once, before attaching: default foreground and background, the cursor
/// position, and DA1 as the sentinel that every terminal answers last.
const HANDSHAKE_QUERY: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[6n\x1b[c";

/// The most input bytes one `send-keys` command carries; the phone's carrier
/// measured this size safe on tmux's parser stack.
const SEND_KEYS_CHUNK: usize = 1024;

/// Focus reporting on: the terminal writes [`FOCUS_IN`] and [`FOCUS_OUT`].
const FOCUS_ON: &[u8] = b"\x1b[?1004h";
const FOCUS_IN: &[u8] = b"\x1b[I";
const FOCUS_OUT: &[u8] = b"\x1b[O";
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// How long typing must pause before keys reach the pane again once a hold has
/// ended or a key was swallowed. A question released by coming back to the tab
/// — the tab coming to the front, or the first key typed — is drawn a few
/// milliseconds later: a line typed straight away would land in it, and its
/// Enter would choose. The line is swallowed and discarded instead, and the
/// question is on screen before the next key counts.
const SWALLOW_UNTIL_PAUSE: Duration = Duration::from_secs(1);

/// The pane state an attach paint asks tmux for; see [`View::ask_for_paint`]. The
/// capture keeps
/// trailing spaces (`-N`), because a row erased in a colour — Codex's composer
/// band — is spaces in that colour; rows never written stay empty. The title is last
/// because it may contain spaces; the key mode is rendered as a digit because
/// tmux spells it with one (`Ext 2`).
const PAINT_FORMAT: &str = "#{pane_height} #{cursor_x} #{cursor_y} #{cursor_flag} \
     #{alternate_on} #{bracket_paste_flag} #{keypad_cursor_flag} \
     #{keypad_flag} #{mouse_standard_flag} #{mouse_button_flag} #{mouse_all_flag} \
     #{mouse_sgr_flag} #{mouse_utf8_flag} \
     #{?#{==:#{pane_key_mode},Ext 2},2,#{?#{==:#{pane_key_mode},Ext 1},1,0}} \
     #{?@codeconnect-stopped,1,0} #{cursor_shape} #{cursor_blinking} \
     #{?#{==:#{pane_title},#{host}},,#{pane_title}}";

/// The user's terminal, in raw mode, with what it answered about itself.
pub struct Terminal {
    size: (u16, u16),
    /// `refresh-client -r` reports of the terminal's default colours.
    colours: Vec<String>,
    /// The terminal's cursor row (zero-based), where a paint starts.
    row: Option<usize>,
    /// Keys typed while the terminal was being asked; they go to the pane.
    typed: Vec<u8>,
    /// The pipe a resize writes to, from before the size is read.
    wake: [libc::c_int; 2],
}

/// The attributes to put back, for the exit paths that never reach a destructor.
static SAVED: OnceLock<libc::termios> = OnceLock::new();
/// Whether the terminal is on the alternate screen, so leaving can undo it.
static ALTERNATE: AtomicBool = AtomicBool::new(false);
/// The write end of the pipe the resize handler wakes the client with.
static WAKE: AtomicI32 = AtomicI32::new(-1);
/// The tmux client, once tmux has attached it, so a signal that ends this
/// process detaches it too. Never a refused one: signalling a client whose
/// commands are still finishing takes the server down (measured on tmux 3.7b).
static ATTACHED: AtomicI32 = AtomicI32::new(-1);

impl Terminal {
    /// Put the terminal in raw mode and ask it for its colours and cursor.
    pub fn open() -> Result<Self> {
        let size = crate::tmux::terminal_size().context("reading the terminal size")?;
        let mut original = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return Err(std::io::Error::last_os_error()).context("reading terminal settings");
        }
        SAVED.get_or_init(|| original);
        raw()?;
        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
            unsafe { libc::signal(signal, leave_on_signal as *const () as libc::sighandler_t) };
        }
        let mut wake = [0; 2];
        if unsafe { libc::pipe(wake.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("creating the resize pipe");
        }
        WAKE.store(wake[1], Ordering::SeqCst);
        unsafe {
            libc::signal(
                libc::SIGWINCH,
                wake_on_resize as *const () as libc::sighandler_t,
            )
        };
        // Read again now that a resize is heard: one in between is not lost.
        let size = crate::tmux::terminal_size().unwrap_or(size);
        let mut terminal = Terminal {
            size,
            colours: Vec::new(),
            row: None,
            typed: Vec::new(),
            wake,
        };
        terminal.ask(libc::STDIN_FILENO, libc::STDOUT_FILENO)?;
        Ok(terminal)
    }

    /// Ask the terminal on `output` about itself, read its answers from `input`,
    /// then turn its focus reporting on.
    fn ask(&mut self, input: libc::c_int, output: libc::c_int) -> Result<()> {
        write_all(output, HANDSHAKE_QUERY)?;
        let deadline = Instant::now() + HANDSHAKE_BUDGET;
        let mut replies = Replies::default();
        let mut buffer = [0u8; 512];
        while !replies.done && Instant::now() < deadline {
            let wait = deadline.saturating_duration_since(Instant::now());
            if !readable(input, wait)? {
                break;
            }
            let read = unsafe { libc::read(input, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read <= 0 {
                break;
            }
            replies.feed(&buffer[..read as usize]);
        }
        self.colours = replies.colours;
        self.row = replies.row;
        self.typed = replies.typed;
        self.typed.extend_from_slice(&replies.pending);
        write_all(output, FOCUS_ON)
    }

    /// The commands that run in the attach itself, each as its argument list:
    /// the attach, the terminal's size, its colours for the pane, and
    /// `scroll-on-clear off` for the pane — a screen the program clears leaves no
    /// copy in the history a later attach paints, as in a terminal. Codex clears
    /// its first frame, and tmux's default kept that stale frame above the session.
    pub fn attach_commands(&self, session: &str, pane: &str) -> Vec<Vec<String>> {
        let (cols, rows) = self.size;
        let mut commands = vec![
            vec!["attach-session".into(), "-t".into(), session.into()],
            vec![
                "refresh-client".into(),
                "-C".into(),
                format!("{cols}x{rows}"),
            ],
            ["set-option", "-p", "-t", pane, "scroll-on-clear", "off"]
                .map(str::to_owned)
                .into(),
        ];
        for report in &self.colours {
            commands.push(vec![
                "refresh-client".into(),
                "-r".into(),
                format!("{pane}:{report}"),
            ]);
        }
        commands
    }
}

/// Return once this process is the terminal's foreground job again.
///
/// A job continued in the background (`bg`, or `kill %1`, which continues it so it
/// can die) must not touch the terminal: the kernel would stop it with SIGTTOU at
/// once, and a signal handler ending this process on another thread would be stopped
/// half-way with it. So it waits a moment for such a handler, then stops as a
/// program run directly does when it reaches for a terminal it does not own.
fn wait_for_foreground() {
    loop {
        for _ in 0..20 {
            if unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) == libc::getpgrp() } {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe { libc::kill(libc::getpid(), libc::SIGTTOU) };
    }
}

/// Put the terminal in raw mode, from the attributes it had before this process.
fn raw() -> Result<()> {
    let mut raw = *SAVED.get().expect("saved before raw");
    unsafe { libc::cfmakeraw(&mut raw) };
    // A job resumed in the background is stopped here by SIGTTOU until `fg`, and the
    // call is then interrupted.
    while unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("setting raw mode");
        }
    }
    Ok(())
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe { libc::signal(libc::SIGWINCH, libc::SIG_DFL) };
        WAKE.store(-1, Ordering::SeqCst);
        unsafe {
            libc::close(self.wake[0]);
            libc::close(self.wake[1]);
        }
        leave();
    }
}

/// Undo what the session left on the terminal and restore its attributes.
///
/// The program in the pane restores its own modes when it exits; this covers a
/// session that ended under it, as tmux's own client does when it detaches.
/// Only calls that are safe inside a signal handler.
fn leave() {
    // A viewer suspended in the shell already gave the terminal back; one ended while
    // it is not the foreground job must not take it from the shell.
    if unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) } != unsafe { libc::getpgrp() } {
        return;
    }
    if ALTERNATE.swap(false, Ordering::SeqCst) {
        write_raw(libc::STDOUT_FILENO, b"\x1b[?1049l");
    }
    write_raw(libc::STDOUT_FILENO, LEAVE_MODES);
    if let Some(saved) = SAVED.get() {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved) };
    }
}

const LEAVE_MODES: &[u8] = b"\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\
\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?1l\x1b>\x1b[>4m\x1b[<99u";

extern "C" fn leave_on_signal(signal: libc::c_int) {
    let client = ATTACHED.load(Ordering::SeqCst);
    if client > 0 {
        unsafe { libc::kill(client, libc::SIGTERM) };
    }
    leave();
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Keep the signal keys typed while a session is being started from ending this
/// process. With `ISIG` off, Ctrl-C (and Ctrl-\\, Ctrl-Z) stays in the terminal's input
/// as a byte, which [`Terminal::open`] reads and types into the pane, where it means
/// what it means to the program there — instead of killing this process, or a helper it
/// is running, and leaving the session running unseen. The guard puts the terminal
/// back if no [`Terminal`] takes it over, and a signal that ends this process meanwhile
/// puts it back first; `None` when stdin is not a terminal.
pub fn hold_signal_keys() -> Option<SignalKeysHeld> {
    let mut original = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
        return None;
    }
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
        unsafe { libc::signal(signal, leave_on_signal as *const () as libc::sighandler_t) };
    }
    let held = without_signal_keys(*SAVED.get_or_init(|| original));
    (unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &held) } == 0)
        .then_some(SignalKeysHeld)
}

fn without_signal_keys(mut attributes: libc::termios) -> libc::termios {
    attributes.c_lflag &= !libc::ISIG;
    attributes
}

/// Restores the terminal [`hold_signal_keys`] changed.
pub struct SignalKeysHeld;

impl Drop for SignalKeysHeld {
    fn drop(&mut self) {
        if let Some(saved) = SAVED.get() {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved) };
        }
    }
}

extern "C" fn wake_on_resize(_: libc::c_int) {
    let fd = WAKE.load(Ordering::SeqCst);
    if fd >= 0 {
        unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
    }
}

/// The terminal's answers to [`HANDSHAKE_QUERY`], separated from keys typed
/// meanwhile.
#[derive(Default)]
struct Replies {
    colours: Vec<String>,
    row: Option<usize>,
    done: bool,
    typed: Vec<u8>,
    pending: Vec<u8>,
}

impl Replies {
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.done {
                self.typed.push(byte);
                continue;
            }
            // An ESC begins a reply — unless it is the `ESC \` ending an OSC one —
            // so what came before it, an Esc typed meanwhile, was a key.
            if byte == 0x1b && !self.pending.is_empty() && !self.pending.starts_with(b"\x1b]") {
                self.flush();
            }
            self.pending.push(byte);
            match self.pending.as_slice() {
                [0x1b] | [0x1b, b']' | b'['] => {}
                [0x1b, b']', body @ ..] => {
                    let end = if body.ends_with(b"\x1b\\") {
                        body.len() - 2
                    } else if body.ends_with(b"\x07") {
                        body.len() - 1
                    } else {
                        if body.len() > 64 {
                            self.flush();
                        }
                        continue;
                    };
                    if let Some(report) = colour_report(&body[..end]) {
                        self.colours.push(report);
                    }
                    self.pending.clear();
                }
                [0x1b, b'[', body @ .., last] if (0x40..=0x7e).contains(last) => {
                    match (last, cursor_row(body)) {
                        (b'R', Some(row)) => self.row = Some(row),
                        (b'c', _) if body.first() == Some(&b'?') => self.done = true,
                        _ => {
                            self.flush();
                            continue;
                        }
                    }
                    self.pending.clear();
                }
                [0x1b, b'[', ..] => {}
                _ => self.flush(),
            }
        }
    }

    fn flush(&mut self) {
        self.typed.append(&mut self.pending);
    }
}

/// `10;rgb:…` or `11;…` as a report tmux accepts, rebuilt from its validated
/// parts so nothing else the terminal sent can reach a tmux command.
fn colour_report(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let (which, rgb) = text.split_once(";rgb:")?;
    if which != "10" && which != "11" {
        return None;
    }
    let parts: Vec<&str> = rgb.split('/').collect();
    let valid = parts.len() == 3
        && parts.iter().all(|part| {
            (1..=4).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_hexdigit())
        });
    valid.then(|| format!("\x1b]{which};rgb:{rgb}\x1b\\"))
}

/// The zero-based row of a cursor report's `row;column`.
fn cursor_row(body: &[u8]) -> Option<usize> {
    let (row, column) = std::str::from_utf8(body).ok()?.split_once(';')?;
    column.parse::<usize>().ok()?;
    row.parse::<usize>().ok()?.checked_sub(1)
}

/// A running attachment: the tmux control client, and the thread moving bytes
/// between it and the terminal.
pub struct Client {
    pub child: Child,
    viewer: Option<std::thread::JoinHandle<Result<()>>>,
}

impl Client {
    /// Start `tmux argv` and show `pane` of `session` in the terminal.
    pub fn spawn(
        mut terminal: Terminal,
        argv: Vec<String>,
        session: String,
        pane: String,
    ) -> Result<Self> {
        let mut child = Command::new(crate::tmux::tmux_bin()?)
            .args(argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting the tmux control client")?;
        let input = child.stdin.take().context("tmux client input")?;
        let output = child.stdout.take().context("tmux client output")?;
        let client = child.id() as libc::pid_t;
        let viewer = std::thread::spawn(move || {
            let typed = std::mem::take(&mut terminal.typed);
            let wake = terminal.wake[0];
            let mut view = View::new(session, pane, input, typed, terminal.row, client, terminal);
            let result = view.run(output, wake);
            ATTACHED.store(-1, Ordering::SeqCst);
            drop(view);
            result
        });
        Ok(Client {
            child,
            viewer: Some(viewer),
        })
    }

    /// Wait for the attachment to end: `Ok` when tmux ended it — the session
    /// ended or the client was detached — and tmux's reason otherwise.
    pub fn wait(&mut self) -> Result<()> {
        let viewed = self.join();
        let status = self.child.wait().context("waiting for the tmux client")?;
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        match viewed {
            Some(Ok(())) | None if status.success() => Ok(()),
            Some(Err(error)) if !stderr.trim().is_empty() => {
                Err(error.context(stderr.trim().to_string()))
            }
            Some(Err(error)) => Err(error),
            _ => bail!("tmux exited with {status}: {}", stderr.trim()),
        }
    }

    /// End the attachment from this side; returns once the terminal is restored.
    ///
    /// SIGTERM lets tmux detach the client. A wedged client gets a bounded
    /// grace, then is killed and reaped; either way its output closes and the
    /// viewer restores the terminal. Only the client is signalled, never its
    /// process group.
    pub fn stop(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(1);
            while self.child.try_wait()?.is_none() {
                if Instant::now() >= deadline {
                    self.child.kill().context("stopping the terminal client")?;
                    self.child.wait().context("reaping the terminal client")?;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = self.join();
        Ok(())
    }

    fn join(&mut self) -> Option<Result<()>> {
        let viewer = self.viewer.take()?;
        Some(
            viewer
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the terminal client panicked"))),
        )
    }
}

/// Where the attachment is in its life.
enum Phase {
    /// Waiting for tmux to confirm the attach to the expected session.
    Attaching,
    /// The paint was asked for; pane output is already in it.
    Painting { replies: Vec<Vec<Vec<u8>>> },
    /// Pane output goes to the terminal.
    Streaming,
    /// Back from a suspension: asking whether the agent is still stopped, and
    /// whether the pane printed anything since this terminal last showed it.
    Resuming { changed: bool },
}

struct View {
    session: String,
    pane: String,
    input: Option<ChildStdin>,
    phase: Phase,
    filter: Filter,
    /// Keys typed before the paint was done, sent once it is.
    typed: Vec<u8>,
    keys: Keys,
    link: ViewerLink,
    /// The terminal's cursor row before the paint.
    row: Option<usize>,
    /// The tmux client's pid.
    client: libc::pid_t,
    /// What tmux said when it refused the attach. The client is left to end
    /// itself: one whose pipes close first does not exit (measured on tmux
    /// 3.7b), and signalling it while its commands finish takes the server
    /// down with it.
    refused: Option<String>,
    /// The lead bytes of a UTF-8 character that was part way through when the
    /// capture was taken; see [`View::before_capture`].
    partial_char: Vec<u8>,
    /// Whether the stream is still at its start after a paint that carried no
    /// part of an unfinished character or sequence; see [`without_orphans`].
    orphans: bool,
    /// The reply block being read: its `%begin` arguments, whether this
    /// client's own command asked for it, and its lines.
    block: Option<(Vec<u8>, bool, Vec<Vec<u8>>)>,
    resized: bool,
    /// Continue the stopped agent once the paint in progress is on the terminal.
    resume_after_paint: bool,
    /// Stop markers seen whose question to tmux is not answered yet.
    confirming: usize,
    /// Answers still to come to questions asked before this terminal was last stopped.
    stale: usize,
    terminal: Terminal,
}

impl View {
    fn new(
        session: String,
        pane: String,
        input: ChildStdin,
        typed: Vec<u8>,
        row: Option<usize>,
        client: libc::pid_t,
        terminal: Terminal,
    ) -> Self {
        let link = ViewerLink::start(protocol::socket_path(), session.clone(), client);
        let mut view = View {
            session,
            pane,
            input: Some(input),
            phase: Phase::Attaching,
            filter: Filter::default(),
            typed: Vec::new(),
            keys: Keys::default(),
            link,
            row,
            client,
            refused: None,
            partial_char: Vec::new(),
            orphans: false,
            block: None,
            resized: false,
            resume_after_paint: false,
            confirming: 0,
            stale: 0,
            terminal,
        };
        view.typed_in(&typed);
        view
    }

    fn run(&mut self, mut output: ChildStdout, wake: libc::c_int) -> Result<()> {
        let mut line = Vec::new();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut stdin_open = true;
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: output.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if stdin_open { libc::STDIN_FILENO } else { -1 },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("waiting on the terminal");
            }
            if fds[1].revents != 0 {
                let mut drained = [0u8; 64];
                unsafe { libc::read(wake, drained.as_mut_ptr().cast(), drained.len()) };
                self.resized = true;
                self.flush_input()?;
            }
            if fds[2].revents != 0 {
                let read = unsafe {
                    libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len())
                };
                if read <= 0 {
                    // The terminal is gone: detach, and let tmux say so.
                    stdin_open = false;
                    self.input = None;
                } else {
                    self.typed_in(&buffer[..read as usize]);
                    self.flush_input()?;
                }
            }
            if fds[0].revents != 0 {
                let read = output
                    .read(&mut buffer)
                    .context("reading the tmux client")?;
                if read == 0 {
                    if let Some(said) = self.refused.take() {
                        bail!("tmux refused the attach: {said}");
                    }
                    bail!("tmux closed the attachment");
                }
                for &byte in &buffer[..read] {
                    if byte != b'\n' {
                        line.push(byte);
                        continue;
                    }
                    if self.line(&line)? {
                        return Ok(());
                    }
                    line.clear();
                }
            }
        }
    }

    /// Handle one control-mode line; `true` when the attachment is over.
    fn line(&mut self, line: &[u8]) -> Result<bool> {
        if let Some((args, lines)) = self.block.as_mut().map(|(args, _, lines)| (args, lines)) {
            match reply_block_close(line) {
                Some((close, failed)) if close == args.as_slice() => {
                    let (_, ours, lines) = self.block.take().expect("an open block");
                    self.reply(ours, failed, lines)?;
                }
                _ => lines.push(line.to_vec()),
            }
            return Ok(false);
        }
        if let Some(args) = reply_block_open(line) {
            // The third argument is 1 for a command this client sent on its
            // input, and 0 for the commands it was started with.
            let ours = args.ends_with(b" 1");
            self.block = Some((args.to_vec(), ours, Vec::new()));
            return Ok(false);
        }
        match protocol::tmux::parse_control_line(line) {
            ControlLine::Exit => {
                if let Some(said) = self.refused.take() {
                    bail!("tmux refused the attach: {said}");
                }
                return Ok(true);
            }
            ControlLine::SessionChanged(id) => {
                if !matches!(self.phase, Phase::Attaching) || id != self.session {
                    bail!("tmux attached this terminal to {id}, not {}", self.session);
                }
                ATTACHED.store(self.client, Ordering::SeqCst);
                self.ask_for_paint()?;
            }
            ControlLine::Output { pane, bytes } if pane == self.pane => {
                if matches!(self.phase, Phase::Streaming) {
                    let mut bytes = bytes.as_slice();
                    if self.orphans {
                        bytes = without_orphans(bytes);
                        self.orphans = bytes.is_empty();
                    }
                    let mut out = Vec::with_capacity(bytes.len());
                    self.filter.feed(bytes, &mut out);
                    write_all(libc::STDOUT_FILENO, &out)?;
                    if std::mem::take(&mut self.filter.stopped) {
                        self.confirm_stop()?;
                    }
                } else {
                    if let Phase::Resuming { changed, .. } = &mut self.phase {
                        *changed = true;
                    }
                    self.before_capture(&bytes);
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn reply(&mut self, ours: bool, failed: bool, lines: Vec<Vec<u8>>) -> Result<()> {
        if !ours {
            // The start-up commands answer with nothing; anything else is the
            // attach being refused.
            if failed || !lines.is_empty() {
                let said = lines
                    .iter()
                    .map(|line| String::from_utf8_lossy(line).into_owned())
                    .collect::<Vec<_>>()
                    .join("; ");
                self.refused.get_or_insert(said);
            }
            return Ok(());
        }
        // Outside a paint, the commands this client sends (`send-keys`, `set-option`)
        // answer with nothing, and its questions with one line.
        if lines.is_empty() && !failed && !matches!(self.phase, Phase::Painting { .. }) {
            return Ok(());
        }
        if self.stale > 0 {
            self.stale -= 1;
            return Ok(());
        }
        if self.confirming > 0 {
            self.confirming -= 1;
            if !failed && lines.first().map(Vec::as_slice) == Some(b"1") {
                self.stale = std::mem::take(&mut self.confirming);
                return self.suspend();
            }
            return Ok(());
        }
        if let Phase::Resuming { changed } = self.phase {
            let answer = lines
                .first()
                .map(|line| String::from_utf8_lossy(line).into_owned());
            let answer = answer.unwrap_or_default();
            match answer.split_once(' ') {
                Some((pid, "1")) if !failed && !changed => {
                    self.phase = Phase::Streaming;
                    self.flush_input()?;
                    if let Ok(pid) = pid.parse::<libc::pid_t>() {
                        unsafe { libc::kill(pid, libc::SIGUSR1) };
                    }
                }
                // Stopped still, but the pane moved on while this terminal was away (another
                // viewer resumed the agent): paint it, then continue the agent.
                Some((_, "1")) if !failed => {
                    self.resume_after_paint = true;
                    self.ask_for_paint()?;
                }
                _ => self.ask_for_paint()?,
            }
            return Ok(());
        }
        let Phase::Painting { replies } = &mut self.phase else {
            return Ok(());
        };
        if failed {
            bail!(
                "tmux could not show the pane: {}",
                String::from_utf8_lossy(&lines.concat())
            );
        }
        replies.push(lines);
        if replies.len() < 3 {
            return Ok(());
        }
        let unfinished = replies.pop().expect("three replies").join(&b"\n"[..]);
        let capture = replies.pop().expect("three replies");
        let state = replies.pop().expect("three replies");
        let state = state
            .first()
            .and_then(|line| PaneState::parse(line))
            .context("tmux described the pane in an unexpected shape")?;
        let mut paint = paint(&state, &capture, self.row);
        // What tmux has read of a sequence it has not finished, so the rest of it
        // reaches the terminal whole. A character part way through is kept out
        // of that by tmux, so its lead bytes come from the output seen instead.
        // With neither, a character tmux began reading before this client
        // attached has lead bytes nobody can recover; its remaining bytes are
        // dropped, as a UTF-8 decoder drops continuation bytes with no lead.
        self.orphans = unfinished.is_empty() && self.partial_char.is_empty();
        if unfinished.is_empty() {
            paint.append(&mut self.partial_char);
        } else {
            self.filter.feed(&unfinished, &mut paint);
        }
        ALTERNATE.store(state.alternate, Ordering::SeqCst);
        write_all(libc::STDOUT_FILENO, &paint)?;
        self.phase = Phase::Streaming;
        let resume = std::mem::take(&mut self.resume_after_paint);
        // A marker in the paint is answered by the state read with it.
        self.filter.stopped = false;
        if state.stopped && !resume {
            return self.suspend();
        }
        if state.stopped {
            // The paint lined the terminal's rows up with the pane's: the agent resumes
            // at the pane's own cursor.
            return self.ask_whether_stopped(None);
        }
        self.flush_input()
    }

    /// Ask whether the agent is still stopped, telling the pane's process which row
    /// of the terminal the agent is to resume at (`None`: the pane's own cursor).
    fn ask_whether_stopped(&mut self, row: Option<usize>) -> Result<()> {
        let cursor = crate::job::CURSOR_OPTION;
        let set = match row {
            Some(row) => format!("set-option -p -t {} {cursor} '{}'", self.pane, row + 1),
            None => format!("set-option -p -u -t {} {cursor}", self.pane),
        };
        let ask = format!(
            "{set} ; display-message -p -t {} \"#{{pane_pid}} #{{?@codeconnect-stopped,1,0}}\"\n",
            self.pane
        );
        self.phase = Phase::Resuming { changed: false };
        self.send(ask.as_bytes())
    }

    /// A stop marker came through the pane: suspend only once tmux confirms the pane's
    /// process marked it stopped, since anything the agent prints could spell the
    /// marker. The pane's process marks it before it writes the marker.
    fn confirm_stop(&mut self) -> Result<()> {
        self.confirming += 1;
        let ask = format!(
            "display-message -p -t {} \"#{{?@codeconnect-stopped,1,0}}\"\n",
            self.pane
        );
        self.send(ask.as_bytes())
    }

    /// The agent stopped: give the terminal back to the shell and stop this process
    /// as its job. Once resumed (`fg`), take the terminal again and continue the
    /// agent if it is still stopped, or paint the pane afresh if it is not.
    fn suspend(&mut self) -> Result<()> {
        self.link.focus(false);
        leave();
        unsafe { libc::kill(libc::getpid(), libc::SIGTSTP) };
        wait_for_foreground();
        raw()?;
        self.terminal.ask(libc::STDIN_FILENO, libc::STDOUT_FILENO)?;
        self.row = self.terminal.row;
        self.link.focus(true);
        let typed = std::mem::take(&mut self.terminal.typed);
        self.typed_in(&typed);
        self.resized = true;
        // Where the shell left the cursor, for the agent to resume at: the pane's
        // process moves the pane's cursor there before it continues the agent.
        self.ask_whether_stopped(self.row)
    }

    /// Ask for the pane's state, its capture, and the start of any escape
    /// sequence tmux has read but not finished (`capture-pane -P`), in one line so
    /// the commands run back to back with no pane output processed between them.
    fn ask_for_paint(&mut self) -> Result<()> {
        let paint = format!(
            "display-message -p -t {pane} \"{PAINT_FORMAT}\" ; \
             capture-pane -p -e -N -S - -t {pane} ; capture-pane -p -P -t {pane}\n",
            pane = self.pane
        );
        self.send(paint.as_bytes())?;
        self.phase = Phase::Painting {
            replies: Vec::new(),
        };
        Ok(())
    }

    /// Output the capture already shows. tmux reads the pane in chunks, so the
    /// capture can fall inside a UTF-8 character, which tmux keeps out of the
    /// capture and out of `capture-pane -P` (measured on tmux 3.7b) until it is
    /// whole: its lead bytes, seen here, reach the terminal just before the rest.
    fn before_capture(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match byte {
                0xc0..=0xf7 => self.partial_char = vec![byte],
                0x80..=0xbf if !self.partial_char.is_empty() => {
                    self.partial_char.push(byte);
                    let whole = match self.partial_char[0] {
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        _ => 4,
                    };
                    if self.partial_char.len() == whole {
                        self.partial_char.clear();
                    }
                }
                _ => self.partial_char.clear(),
            }
        }
    }

    /// Take a read of the terminal's input: its focus reports go to `ccd`, and
    /// its keys wait for the pane unless a question is held.
    fn typed_in(&mut self, bytes: &[u8]) {
        let typed = self.keys.feed(
            bytes,
            self.filter.pane_focus,
            self.link.held(),
            self.link.released_at(),
        );
        self.take(typed);
    }

    fn take(&mut self, mut typed: Typed) {
        if let Some(focused) = typed.focus {
            self.link.focus(focused);
        }
        if typed.while_held {
            self.link.key();
        }
        self.typed.append(&mut typed.keys);
    }

    /// Send held keys and a pending resize, once the paint is done.
    fn flush_input(&mut self) -> Result<()> {
        if !matches!(self.phase, Phase::Streaming) {
            return Ok(());
        }
        if std::mem::take(&mut self.resized) {
            if let Some((cols, rows)) = crate::tmux::terminal_size() {
                self.send(protocol::tmux::resize_line(cols, rows).as_bytes())?;
            }
        }
        let typed = std::mem::take(&mut self.typed);
        for chunk in typed.chunks(SEND_KEYS_CHUNK) {
            self.send(send_keys_line(&self.pane, chunk).as_bytes())?;
        }
        Ok(())
    }

    /// Write a command to the tmux client. One that has already ended — its session
    /// ended while this process was stopped in the shell — takes no more input, and
    /// the client's own output then ends the attachment as it does when running.
    fn send(&mut self, line: &[u8]) -> Result<()> {
        if let Some(input) = self.input.as_mut() {
            match input.write_all(line) {
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => self.input = None,
                written => written.context("writing to the tmux client")?,
            }
        }
        Ok(())
    }
}

/// The terminal's input, with its focus reports taken out and what is typed
/// while a question is held swallowed. A terminal writes a key's sequence whole,
/// so one is read whole; inside a bracketed paste every byte is text, and a
/// paste's end mark split across reads is joined. Whether a paste is swallowed
/// is decided at its start, so the pane never gets half of one.
#[derive(Default)]
struct Keys {
    /// The start of a paste's end mark that a read ended in.
    pending: Vec<u8>,
    /// Inside a paste: whether it is swallowed.
    paste: Option<bool>,
    /// When a key was last swallowed: until typing pauses, keys still are.
    swallowed_at: Option<Instant>,
}

/// What a read of the terminal comes to.
#[derive(Default, Debug, PartialEq)]
struct Typed {
    /// The bytes for the pane.
    keys: Vec<u8>,
    /// The terminal's last focus report: `true` in front.
    focus: Option<bool>,
    /// Something was typed while a question was held.
    while_held: bool,
    /// A key, not a mouse report, was swallowed.
    typing: bool,
}

impl Keys {
    /// `pane_focus`: the pane asked for focus reports; `held`: a question is
    /// held; `released_at`: when `ccd` last ended a hold.
    fn feed(
        &mut self,
        bytes: &[u8],
        pane_focus: bool,
        held: bool,
        released_at: Option<Instant>,
    ) -> Typed {
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);
        let lately = |at: Option<Instant>| at.is_some_and(|at| at.elapsed() < SWALLOW_UNTIL_PAUSE);
        let held = held || lately(self.swallowed_at) || lately(released_at);
        let typed = self.scan(&input, pane_focus, held);
        if typed.typing {
            self.swallowed_at = Some(Instant::now());
        }
        typed
    }

    fn scan(&mut self, input: &[u8], pane_focus: bool, held: bool) -> Typed {
        let mut typed = Typed::default();
        let mut at = 0;
        while at < input.len() {
            let rest = &input[at..];
            if self.paste.is_some() && rest.len() < PASTE_END.len() && PASTE_END.starts_with(rest) {
                self.pending = rest.to_vec();
                break;
            }
            if self.paste.is_none() && (rest.starts_with(FOCUS_IN) || rest.starts_with(FOCUS_OUT)) {
                typed.focus = Some(rest.starts_with(FOCUS_IN));
                if pane_focus {
                    typed.keys.extend_from_slice(&rest[..FOCUS_IN.len()]);
                }
                at += FOCUS_IN.len();
                continue;
            }
            let length = match self.paste {
                None if rest.starts_with(PASTE_START) => {
                    self.paste = Some(held);
                    PASTE_START.len()
                }
                None => control_sequence(rest).unwrap_or(1),
                Some(_) if rest.starts_with(PASTE_END) => PASTE_END.len(),
                Some(_) => 1,
            };
            let piece = &rest[..length];
            let swallowed = match self.paste {
                Some(swallowed) => swallowed,
                None => held && !signal_key(piece),
            };
            if piece == PASTE_END {
                self.paste = None;
            }
            typed.while_held |= held || swallowed;
            typed.typing |= swallowed && !mouse_report(piece);
            if !swallowed {
                typed.keys.extend_from_slice(piece);
            }
            at += length;
        }
        typed
    }
}

/// The length of the control sequence (`CSI` … final byte) `input` starts with,
/// when all of it is there.
fn control_sequence(input: &[u8]) -> Option<usize> {
    let body = input.strip_prefix(b"\x1b[")?;
    let end = body.iter().position(|byte| !(0x20..=0x3f).contains(byte))?;
    (0x40..=0x7e).contains(&body[end]).then_some(2 + end + 1)
}

/// Ctrl+C or Ctrl+Z, in any form a terminal types them: the plain byte, or a
/// kitty keyboard protocol (`CSI key[:shifted[:base]];mods u`) or
/// modifyOtherKeys (`CSI 27;mods;key ~`) report whose key, or base-layout key,
/// is `c` or `z` and whose only modifier is Ctrl, lock keys aside.
fn signal_key(piece: &[u8]) -> bool {
    if matches!(piece, [0x03] | [0x1a]) {
        return true;
    }
    let Some(body) = piece
        .strip_prefix(b"\x1b[")
        .and_then(|body| std::str::from_utf8(body).ok())
    else {
        return false;
    };
    let fields: Vec<&str> = body[..body.len() - 1].split(';').collect();
    let (key, modifiers) = match (body.as_bytes().last(), fields.as_slice()) {
        (Some(b'u'), [key, modifiers, ..]) => (*key, *modifiers),
        (Some(b'~'), ["27", modifiers, key]) => (*key, *modifiers),
        _ => return false,
    };
    const CAPS_AND_NUM_LOCK: u32 = 64 | 128;
    let ctrl_only = modifiers
        .split(':')
        .next()
        .and_then(|bits| bits.parse::<u32>().ok())
        .is_some_and(|bits| bits.saturating_sub(1) & !CAPS_AND_NUM_LOCK == 4);
    let mut codes = key.split(':');
    let (code, base) = (codes.next(), codes.nth(1));
    ctrl_only
        && [code, base]
            .iter()
            .any(|key| matches!(key, Some("99" | "122")))
}

/// A mouse report (SGR): the pane's mouse tracking, not typing.
fn mouse_report(piece: &[u8]) -> bool {
    piece.starts_with(b"\x1b[<") && matches!(piece.last(), Some(b'M' | b'm'))
}

/// What tmux records about a pane that a new terminal has to be told.
#[derive(Debug, PartialEq)]
struct PaneState {
    height: usize,
    cursor: (usize, usize),
    cursor_visible: bool,
    alternate: bool,
    bracketed_paste: bool,
    cursor_keys: bool,
    keypad: bool,
    mouse: [bool; 5],
    key_mode: u8,
    stopped: bool,
    cursor_shape: Option<u8>,
    title: String,
}

impl PaneState {
    fn parse(line: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(line).ok()?;
        let mut fields = text.splitn(18, ' ');
        let mut number = || fields.next()?.parse::<usize>().ok();
        let height = number()?;
        let cursor = (number()?, number()?);
        let flags: Vec<bool> = (0..10)
            .map(|_| number().map(|value| value == 1))
            .collect::<Option<_>>()?;
        let key_mode = number()? as u8;
        let stopped = number()? == 1;
        let shape = fields.next()?;
        let blinking = fields.next()? == "1";
        // tmux may trim the separator an empty title leaves at the end.
        let title = fields.next().unwrap_or_default().to_string();
        let cursor_shape = match shape {
            "block" => Some(if blinking { 1 } else { 2 }),
            "underline" => Some(if blinking { 3 } else { 4 }),
            "bar" => Some(if blinking { 5 } else { 6 }),
            _ => None,
        };
        Some(PaneState {
            height,
            cursor,
            cursor_visible: flags[0],
            alternate: flags[1],
            bracketed_paste: flags[2],
            cursor_keys: flags[3],
            keypad: flags[4],
            mouse: [flags[5], flags[6], flags[7], flags[8], flags[9]],
            key_mode,
            stopped,
            cursor_shape,
            title,
        })
    }
}

/// The bytes that show `capture` (the pane's history and screen, one row per
/// line) in a terminal whose cursor is on `row`, and leave it in the pane's state.
fn paint(state: &PaneState, capture: &[Vec<u8>], row: Option<usize>) -> Vec<u8> {
    let mut out = Vec::new();
    let (x, y) = state.cursor;
    let split = capture.len().saturating_sub(state.height);
    let (history, screen) = capture.split_at(split);
    if state.alternate {
        out.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J");
        rows_to(&mut out, screen.iter());
    } else if let Some(start) = row {
        let used = screen
            .iter()
            .rposition(|line| line.iter().any(|&byte| byte != b' '))
            .map_or(0, |last| last + 1)
            .max(y + 1)
            .min(screen.len());
        let lines = history.len() + used;
        rows_to(&mut out, history.iter().chain(&screen[..used]));
        // Where the last row landed, and so how far the pane's first row is
        // below the terminal's. A scroll fills with the current background, so
        // it runs with none.
        let last = (start + lines.saturating_sub(1)).min(state.height.saturating_sub(1));
        let shift = last.saturating_sub(used.saturating_sub(1));
        if shift > 0 {
            out.extend_from_slice(format!("\x1b[0m\x1b[{shift}S").as_bytes());
        }
    } else {
        rows_to(&mut out, capture.iter());
    }
    out.extend_from_slice(b"\x1b[0m");
    out.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
    for (on, mode) in [
        (state.bracketed_paste, "?2004"),
        (state.cursor_keys, "?1"),
        (state.mouse[0], "?1000"),
        (state.mouse[1], "?1002"),
        (state.mouse[2], "?1003"),
        (state.mouse[3], "?1006"),
        (state.mouse[4], "?1005"),
    ] {
        if on {
            out.extend_from_slice(format!("\x1b[{mode}h").as_bytes());
        }
    }
    if state.keypad {
        out.extend_from_slice(b"\x1b=");
    }
    if state.key_mode > 0 {
        out.extend_from_slice(format!("\x1b[>4;{}m", state.key_mode).as_bytes());
    }
    if let Some(shape) = state.cursor_shape {
        out.extend_from_slice(format!("\x1b[{shape} q").as_bytes());
    }
    let title: String = state.title.chars().filter(|c| !c.is_control()).collect();
    if !title.is_empty() {
        out.extend_from_slice(format!("\x1b]0;{title}\x07").as_bytes());
    }
    if !state.cursor_visible {
        out.extend_from_slice(b"\x1b[?25l");
    }
    out
}

/// `bytes` without the UTF-8 continuation bytes it starts with: the rest of a
/// character whose lead bytes tmux read before this client attached.
fn without_orphans(bytes: &[u8]) -> &[u8] {
    let lead = bytes
        .iter()
        .position(|byte| !(0x80..=0xbf).contains(byte))
        .unwrap_or(bytes.len());
    &bytes[lead..]
}

/// Write captured rows, one per line. `capture-pane -e` carries attributes from
/// one row into the next, and so does the terminal across a line break — except
/// that a break which scrolls fills the new row with the current background, so a
/// row erased in a colour (Codex's composer band) would spread down the screen.
/// So each break is written with the default background, and the background in
/// force is set again after it.
fn rows_to<'a>(out: &mut Vec<u8>, rows: impl Iterator<Item = &'a Vec<u8>>) {
    let mut background: Option<Vec<u8>> = None;
    for (index, row) in rows.enumerate() {
        if index > 0 {
            match &background {
                Some(colour) => {
                    out.extend_from_slice(b"\x1b[49m\r\n\x1b[");
                    out.extend_from_slice(colour);
                    out.push(b'm');
                }
                None => out.extend_from_slice(b"\r\n"),
            }
        }
        out.extend_from_slice(row);
        let mut rest = row.as_slice();
        while let Some(at) = rest.windows(2).position(|pair| pair == b"\x1b[") {
            let body = &rest[at + 2..];
            let Some(end) = body.iter().position(|byte| (0x40..=0x7e).contains(byte)) else {
                break;
            };
            if body[end] == b'm' {
                follow_background(&body[..end], &mut background);
            }
            rest = &body[end + 1..];
        }
    }
}

/// Apply one SGR's parameters to the background in force: `None` is the
/// default, `Some` the parameters that set it again.
fn follow_background(params: &[u8], background: &mut Option<Vec<u8>>) {
    if params.is_empty() {
        *background = None;
        return;
    }
    let tokens: Vec<&[u8]> = params.split(|&byte| byte == b';').collect();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        // An extended colour's own parameters: `2;r;g;b` or `5;n`.
        let extended = match tokens.get(index + 1).copied() {
            Some(b"2") => 4,
            Some(b"5") => 2,
            _ => 0,
        };
        let last = (index + extended).min(tokens.len() - 1);
        match token {
            b"" | b"0" | b"49" => *background = None,
            b"48" => *background = Some(tokens[index..=last].join(&b';')),
            b"38" | b"58" => {}
            _ if token.starts_with(b"48:") => *background = Some(token.to_vec()),
            _ => {
                if let Ok(code) = std::str::from_utf8(token).unwrap_or("").parse::<u8>() {
                    if (40..=47).contains(&code) || (100..=107).contains(&code) {
                        *background = Some(token.to_vec());
                    }
                }
            }
        }
        index += if matches!(token, b"38" | b"48" | b"58") {
            extended + 1
        } else {
            1
        };
    }
}

/// Removes, from the pane's output, the terminal queries tmux answers itself,
/// and unwraps tmux passthrough (`ESC P tmux; … ESC \`) the way tmux's own
/// client does. Everything else passes through byte for byte, across `%output`
/// boundaries.
#[derive(Default)]
struct Filter {
    state: Scan,
    held: Vec<u8>,
    /// The pane's process wrote [`crate::job::STOP_MARKER`].
    stopped: bool,
    /// The pane turned focus reporting on (DECSET 1004).
    pane_focus: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Debug)]
enum Scan {
    #[default]
    Ground,
    /// After ESC, collecting intermediates.
    Escape,
    Csi,
    /// A control sequence too long to be a query: passed until its final byte.
    CsiPass,
    /// An OSC that may still be `10;?` or `11;?`.
    Osc,
    /// The ESC that may end a held OSC query.
    OscEnd,
    /// A DCS whose kind is not known yet.
    Dcs,
    /// A DECRQSS, dropped until its terminator.
    DcsDrop,
    DcsDropEnd,
    /// tmux passthrough: its payload, with doubled ESCs undoubled.
    Passthrough,
    PassthroughEscape,
    /// Any other string, passed until an ESC starts its terminator.
    StringPass,
}

impl Filter {
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for &byte in bytes {
            self.byte(byte, out);
        }
    }

    fn byte(&mut self, byte: u8, out: &mut Vec<u8>) {
        match self.state {
            Scan::Ground => {
                if byte == 0x1b {
                    self.held.push(byte);
                    self.state = Scan::Escape;
                } else {
                    out.push(byte);
                }
            }
            Scan::Escape => {
                self.held.push(byte);
                self.state = match (self.held.len(), byte) {
                    (_, 0x1b) => {
                        self.held.pop();
                        self.release(out);
                        self.held.push(0x1b);
                        Scan::Escape
                    }
                    (2, b'[') => Scan::Csi,
                    (2, b']') => Scan::Osc,
                    (2, b'P') => Scan::Dcs,
                    (_, 0x20..=0x2f) => Scan::Escape,
                    _ => {
                        self.release(out);
                        Scan::Ground
                    }
                };
            }
            Scan::Csi => {
                self.held.push(byte);
                if (0x40..=0x7e).contains(&byte) {
                    if !self.control_answered_by_tmux() {
                        self.track_screen();
                        self.pass_control(out);
                    }
                    self.held.clear();
                    self.state = Scan::Ground;
                } else if self.held.len() > 64 {
                    self.release(out);
                    self.state = Scan::CsiPass;
                }
            }
            Scan::CsiPass => {
                out.push(byte);
                if (0x40..=0x7e).contains(&byte) {
                    self.state = Scan::Ground;
                }
            }
            Scan::Osc => {
                let body = &self.held[2..];
                if byte == 0x1b && (body == b"10;?" || body == b"11;?") {
                    self.held.push(byte);
                    self.state = Scan::OscEnd;
                } else if byte == 0x07 && (body == b"10;?" || body == b"11;?") {
                    self.held.clear();
                    self.state = Scan::Ground;
                } else if byte == 0x1b {
                    self.release(out);
                    self.held.push(byte);
                    self.state = Scan::Escape;
                } else {
                    self.held.push(byte);
                    let body = &self.held[2..];
                    if !b"10;?".starts_with(body) && !b"11;?".starts_with(body) {
                        self.release(out);
                        self.state = if byte == 0x07 {
                            Scan::Ground
                        } else {
                            Scan::StringPass
                        };
                    }
                }
            }
            Scan::OscEnd => {
                if byte == b'\\' {
                    self.held.clear();
                    self.state = Scan::Ground;
                } else {
                    // Not a terminator: the ESC begins something new.
                    self.held.truncate(self.held.len() - 1);
                    self.release(out);
                    self.held.push(0x1b);
                    self.state = Scan::Escape;
                    self.byte(byte, out);
                }
            }
            Scan::Dcs => {
                self.held.push(byte);
                let body = &self.held[2..];
                let marker = &crate::job::STOP_MARKER[2..crate::job::STOP_MARKER.len() - 2];
                if body == b"$q" || body == marker {
                    self.stopped |= body == marker;
                    self.held.clear();
                    self.state = Scan::DcsDrop;
                } else if body == b"tmux;" {
                    self.held.clear();
                    self.state = Scan::Passthrough;
                } else if !b"$q".starts_with(body)
                    && !b"tmux;".starts_with(body)
                    && !marker.starts_with(body)
                {
                    let last = self.held.pop().expect("just pushed");
                    self.release(out);
                    self.state = Scan::StringPass;
                    self.byte(last, out);
                }
            }
            Scan::DcsDrop => {
                if byte == 0x1b {
                    self.state = Scan::DcsDropEnd;
                }
            }
            Scan::DcsDropEnd => {
                if byte == b'\\' {
                    self.state = Scan::Ground;
                } else {
                    self.held.push(0x1b);
                    self.state = Scan::Escape;
                    self.byte(byte, out);
                }
            }
            Scan::Passthrough => {
                if byte == 0x1b {
                    self.state = Scan::PassthroughEscape;
                } else {
                    out.push(byte);
                }
            }
            Scan::PassthroughEscape => {
                if byte == b'\\' {
                    self.state = Scan::Ground;
                } else {
                    out.push(0x1b);
                    self.state = Scan::Passthrough;
                    if byte != 0x1b {
                        out.push(byte);
                    }
                }
            }
            Scan::StringPass => {
                if byte == 0x1b {
                    self.held.push(byte);
                    self.state = Scan::Escape;
                } else {
                    out.push(byte);
                    if byte == 0x07 {
                        self.state = Scan::Ground;
                    }
                }
            }
        }
    }

    fn release(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.held);
    }

    /// Pass the held control sequence, following the pane's focus mode. The
    /// terminal's stays on, so a reset that names it reaches the terminal without it.
    fn pass_control(&mut self, out: &mut Vec<u8>) {
        if let Some((&set @ (b'h' | b'l'), [b'?', modes @ ..])) = self.held[2..].split_last() {
            let modes: Vec<&[u8]> = modes.split(|&b| b == b';').collect();
            if modes.contains(&&b"1004"[..]) {
                self.pane_focus = set == b'h';
                if set == b'l' {
                    let rest: Vec<&[u8]> = modes.into_iter().filter(|m| *m != b"1004").collect();
                    if !rest.is_empty() {
                        out.extend_from_slice(b"\x1b[?");
                        out.extend_from_slice(&rest.join(&b';'));
                        out.push(b'l');
                    }
                    return;
                }
            }
        }
        out.extend_from_slice(&self.held);
    }

    /// Whether the held control sequence is a query tmux answers for the pane.
    fn control_answered_by_tmux(&self) -> bool {
        let (final_byte, body) = self.held[2..].split_last().expect("a final byte");
        let (private, rest) = match body.first() {
            Some(&marker @ (b'<' | b'=' | b'>' | b'?')) => (Some(marker), &body[1..]),
            _ => (None, body),
        };
        let split = rest
            .iter()
            .position(|b| (0x20..=0x2f).contains(b))
            .unwrap_or(rest.len());
        let (params, intermediates) = rest.split_at(split);
        match (final_byte, private, intermediates) {
            (b'c', None | Some(b'>'), []) => matches!(params, b"" | b"0"),
            (b'n', None, []) => matches!(params, b"5" | b"6"),
            (b'n', Some(b'?'), []) => params == b"996",
            (b'q', Some(b'>'), []) => matches!(params, b"" | b"0"),
            (b'p', None | Some(b'?'), b"$") => true,
            (b't', None, []) => matches!(params, b"14" | b"16" | b"18"),
            (b'S', Some(b'?'), []) => true,
            _ => false,
        }
    }

    /// Follow the alternate screen, so leaving can put the terminal back.
    fn track_screen(&self) {
        let Some((&set, body)) = self.held[2..].split_last() else {
            return;
        };
        if (set == b'h' || set == b'l') && body.first() == Some(&b'?') {
            let alternate = body[1..]
                .split(|&b| b == b';')
                .any(|mode| matches!(mode, b"1049" | b"1047" | b"47"));
            if alternate {
                ALTERNATE.store(set == b'h', Ordering::SeqCst);
            }
        }
    }
}

fn readable(fd: libc::c_int, wait: Duration) -> Result<bool> {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let polled = unsafe { libc::poll(&mut poll, 1, wait.as_millis() as i32) };
        if polled >= 0 {
            return Ok(polled > 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("waiting for the terminal");
        }
    }
}

fn write_all(fd: libc::c_int, bytes: &[u8]) -> Result<()> {
    if write_raw(fd, bytes) {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("writing to the terminal")
    }
}

/// `write(2)` until done; no allocation, so a signal handler may use it.
fn write_raw(fd: libc::c_int, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        bytes = &bytes[written as usize..];
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Ctrl-C typed while the signal keys are held is a byte waiting in the input,
    /// and it is still there, unchanged, once the terminal is made raw.
    #[test]
    fn a_ctrl_c_typed_while_signal_keys_are_held_reaches_the_raw_terminal_as_a_byte() {
        let (mut main, mut side) = (-1, -1);
        let opened = unsafe {
            libc::openpty(
                &mut main,
                &mut side,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "openpty");
        let mut attributes: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(side, &mut attributes) }, 0);
        assert_ne!(
            attributes.c_lflag & libc::ISIG,
            0,
            "a fresh pty sends signals"
        );
        let held = without_signal_keys(attributes);
        assert_eq!(unsafe { libc::tcsetattr(side, libc::TCSANOW, &held) }, 0);

        assert_eq!(unsafe { libc::write(main, [0x03u8].as_ptr().cast(), 1) }, 1);
        let mut raw = held;
        unsafe { libc::cfmakeraw(&mut raw) };
        assert_eq!(unsafe { libc::tcsetattr(side, libc::TCSANOW, &raw) }, 0);
        assert!(
            readable(side, Duration::from_secs(2)).unwrap(),
            "the byte is waiting"
        );
        let mut byte = [0u8; 8];
        let read = unsafe { libc::read(side, byte.as_mut_ptr().cast(), byte.len()) };
        assert_eq!(&byte[..read as usize], [0x03u8], "exactly the Ctrl-C");
        unsafe {
            libc::close(main);
            libc::close(side);
        }
    }

    #[test]
    fn early_terminal_cleanup_reaps_a_stopped_client_without_signalling_its_group() {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let pid = child.id() as i32;
        let mut terminal = Client {
            child,
            viewer: None,
        };
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let mut status = 0;
            let observed =
                unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
            if observed == pid && libc::WIFSTOPPED(status) {
                break;
            }
            assert!(Instant::now() < deadline, "client did not stop");
            std::thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        terminal.stop().unwrap();
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(terminal.child.try_wait().unwrap().is_some());
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        terminal.stop().unwrap();
    }

    #[test]
    fn early_terminal_cleanup_preserves_an_already_reaped_exit_status() {
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .spawn()
            .unwrap();
        let mut terminal = Client {
            child,
            viewer: None,
        };
        assert_eq!(terminal.child.wait().unwrap().code(), Some(7));
        terminal.stop().unwrap();
        assert_eq!(terminal.child.try_wait().unwrap().unwrap().code(), Some(7));
    }

    fn filtered(chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = Filter::default();
        let mut out = Vec::new();
        for chunk in chunks {
            filter.feed(chunk, &mut out);
        }
        out
    }

    /// Every query tmux 3.7b answered for a pane with only a control client
    /// attached is removed; each is measured, not assumed (see the module doc).
    #[test]
    fn the_queries_tmux_answers_never_reach_the_terminal() {
        for query in [
            &b"\x1b[c"[..],
            b"\x1b[0c",
            b"\x1b[>c",
            b"\x1b[>0c",
            b"\x1b[6n",
            b"\x1b[5n",
            b"\x1b[?996n",
            b"\x1b[>q",
            b"\x1b[>0q",
            b"\x1b[?2026$p",
            b"\x1b[4$p",
            b"\x1b[14t",
            b"\x1b[16t",
            b"\x1b[18t",
            b"\x1b[?1;1;0S",
            b"\x1bP$qm\x1b\\",
            b"\x1bP$q q\x1b\\",
            b"\x1b]10;?\x1b\\",
            b"\x1b]11;?\x1b\\",
            b"\x1b]10;?\x07",
            b"\x1b]11;?\x07",
        ] {
            assert_eq!(filtered(&[b"a", query, b"b"]), b"ab", "{query:?}");
            // Split at every byte: a query that straddles `%output` lines too.
            let pieces: Vec<&[u8]> = query.chunks(1).collect();
            let mut all = vec![&b"a"[..]];
            all.extend(pieces);
            all.push(b"b");
            assert_eq!(filtered(&all), b"ab", "{query:?} split");
        }
    }

    /// Queries tmux leaves unanswered are the terminal's, and everything that is
    /// not a query passes byte for byte.
    #[test]
    fn everything_else_passes_byte_for_byte() {
        for bytes in [
            &b"\x1b[?u"[..],
            b"\x1b[=c",
            b"\x1b[?6n",
            b"\x1b[21t",
            b"\x1b]12;?\x1b\\",
            b"\x1b]4;1;?\x07",
            b"\x1b]52;c;?\x1b\\",
            b"\x1b]10;?;?\x1b\\",
            b"\x1bP+q544e\x1b\\",
            b"\x1b]0;\xe2\x9c\xb3 Claude Code\x07",
            b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
            b"\x1b[?2026h\x1b[1;1H\x1b[J\x1b[38;2;215;119;87m\xe2\x96\x90\x1b[?2026l",
            b"\x1b[>7u\x1b[<u\x1b[>4;2m\x1b[1;40r\x1bM\x1b(B\x1b7\x1b8\x1b=\x1b>",
            b"\x1b\x1b[A",
            b"plain \r\n\xf0\x9f\x98\x80 \x07",
        ] {
            assert_eq!(filtered(&[bytes]), bytes, "{bytes:?}");
            let pieces: Vec<&[u8]> = bytes.chunks(1).collect();
            assert_eq!(filtered(&pieces), bytes, "{bytes:?} split");
        }
    }

    /// The pane's process marks a stopped agent in the stream; the mark never reaches
    /// the terminal, whole or split across `%output` lines, and nothing else sets it.
    #[test]
    fn the_stop_marker_is_taken_out_of_the_stream_and_noted() {
        let marked = [&b"a"[..], crate::job::STOP_MARKER, b"b"].concat();
        for chunk in [marked.len(), 1] {
            let mut filter = Filter::default();
            let mut out = Vec::new();
            for piece in marked.chunks(chunk) {
                filter.feed(piece, &mut out);
            }
            assert_eq!(out, b"ab", "chunks of {chunk}");
            assert!(filter.stopped, "chunks of {chunk}");
        }
        let other = b"\x1bP=codeconnect\x1b\\";
        let mut filter = Filter::default();
        let mut out = Vec::new();
        filter.feed(other, &mut out);
        assert!(!filter.stopped);
        assert_eq!(out, other);
    }

    #[test]
    fn tmux_passthrough_is_unwrapped_as_tmux_does() {
        let wrapped = b"\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\after";
        assert_eq!(filtered(&[wrapped]), b"\x1b]52;c;aGk=\x07after");
        let pieces: Vec<&[u8]> = wrapped.chunks(1).collect();
        assert_eq!(filtered(&pieces), b"\x1b]52;c;aGk=\x07after");
    }

    #[test]
    fn a_stream_drops_only_the_continuation_bytes_it_starts_with() {
        assert_eq!(without_orphans(b"\xb3XYZ"), b"XYZ");
        assert_eq!(
            without_orphans(b"\x98\x80\xf0\x9f\x98\x80"),
            b"\xf0\x9f\x98\x80"
        );
        assert_eq!(without_orphans(b"\xe2\x9c\xb3"), b"\xe2\x9c\xb3");
        assert_eq!(without_orphans(b"\x1b[0m\xb3"), b"\x1b[0m\xb3");
        assert_eq!(without_orphans(b"\x80\xbf"), b"");
        assert_eq!(without_orphans(b""), b"");
    }

    #[test]
    fn the_alternate_screen_is_followed() {
        let mut filter = Filter::default();
        let mut out = Vec::new();
        filter.feed(b"\x1b[?1049h", &mut out);
        assert!(ALTERNATE.load(Ordering::SeqCst));
        filter.feed(b"\x1b[?25;1049l", &mut out);
        assert!(!ALTERNATE.load(Ordering::SeqCst));
    }

    #[test]
    fn the_handshake_keeps_answers_apart_from_typed_keys() {
        let mut replies = Replies::default();
        replies.feed(b"x\x1b]10;rgb:e4e4/eeee/f5f5\x1b\\\x1b]11;rgb:1d/20/22\x07");
        replies.feed(b"\x1b[3;1R\x1b[Ay\x1b[?62c\x03z");
        assert_eq!(
            replies.colours,
            [
                "\x1b]10;rgb:e4e4/eeee/f5f5\x1b\\",
                "\x1b]11;rgb:1d/20/22\x1b\\"
            ]
        );
        assert_eq!(replies.row, Some(2));
        assert!(replies.done);
        assert_eq!(replies.typed, b"x\x1b[Ay\x03z");
        // A colour answer that is not strictly hex never reaches a tmux command.
        let mut replies = Replies::default();
        replies.feed(b"\x1b]11;rgb:1d/20/2' ; kill-server\x1b\\");
        assert!(replies.colours.is_empty());
        // An Esc typed before the answers is a key, and the answer after it is
        // still an answer.
        let mut replies = Replies::default();
        replies.feed(b"\x1b");
        replies.feed(b"\x1b]10;rgb:e4e4/eeee/f5f5\x1b\\\x1b[?62c");
        assert_eq!(replies.typed, b"\x1b");
        assert_eq!(replies.colours, ["\x1b]10;rgb:e4e4/eeee/f5f5\x1b\\"]);
    }

    /// The terminal is asked about itself, and only then told to report focus.
    #[test]
    fn focus_reporting_is_turned_on_once_the_terminal_has_answered() {
        let (mut main, mut side) = (-1, -1);
        let opened = unsafe {
            libc::openpty(
                &mut main,
                &mut side,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "openpty");
        let mut attributes: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(side, &mut attributes) }, 0);
        unsafe { libc::cfmakeraw(&mut attributes) };
        assert_eq!(
            unsafe { libc::tcsetattr(side, libc::TCSANOW, &attributes) },
            0
        );
        let answer = b"\x1b[?62c";
        assert_eq!(
            unsafe { libc::write(main, answer.as_ptr().cast(), answer.len()) },
            answer.len() as isize
        );
        let mut terminal = std::mem::ManuallyDrop::new(Terminal {
            size: (80, 24),
            colours: Vec::new(),
            row: None,
            typed: Vec::new(),
            wake: [-1, -1],
        });
        terminal.ask(side, side).unwrap();
        let mut written = Vec::new();
        let mut buffer = [0u8; 256];
        while readable(main, Duration::from_millis(200)).unwrap() {
            let read = unsafe { libc::read(main, buffer.as_mut_ptr().cast(), buffer.len()) };
            assert!(read > 0);
            written.extend_from_slice(&buffer[..read as usize]);
        }
        assert_eq!(written, [HANDSHAKE_QUERY, b"\x1b[?1004h"].concat());
        unsafe {
            libc::close(main);
            libc::close(side);
        }
    }

    fn typed(keys: &mut Keys, bytes: &[u8], pane_focus: bool, held: bool) -> Typed {
        keys.feed(bytes, pane_focus, held, None)
    }

    #[test]
    fn a_focus_report_never_reaches_a_pane_that_did_not_ask() {
        let typed = typed(&mut Keys::default(), b"a\x1b[Ib\x1b[Oc", false, false);
        assert_eq!(typed.keys, b"abc");
    }

    #[test]
    fn the_viewer_learns_focus_in_and_out() {
        let mut keys = Keys::default();
        assert_eq!(typed(&mut keys, b"\x1b[O", false, false).focus, Some(false));
        assert_eq!(typed(&mut keys, b"\x1b[I", false, false).focus, Some(true));
        assert_eq!(typed(&mut keys, b"x", false, false).focus, None);
        assert_eq!(
            typed(&mut keys, b"\x1b[I\x1b[O", false, false).focus,
            Some(false),
            "the last report counts"
        );
    }

    /// The pane's focus reports, on and then off: it gets the terminal's reports
    /// byte for byte while on, and its reset never turns them off in the terminal.
    #[test]
    fn the_pane_s_focus_mode_is_followed_and_kept_off_the_terminal() {
        let mut filter = Filter::default();
        let mut keys = Keys::default();
        let mut out = Vec::new();
        filter.feed(b"\x1b[?1004h", &mut out);
        assert_eq!(out, b"\x1b[?1004h");
        let report = typed(&mut keys, b"\x1b[Ix\x1b[O", filter.pane_focus, false);
        assert_eq!(report.keys, b"\x1b[Ix\x1b[O");
        assert_eq!(report.focus, Some(false));
        out.clear();
        filter.feed(b"a\x1b[?1004lb", &mut out);
        assert_eq!(out, b"ab");
        let report = typed(&mut keys, b"\x1b[Ix", filter.pane_focus, false);
        assert_eq!(report.keys, b"x");
        assert_eq!(report.focus, Some(true));
    }

    #[test]
    fn a_reset_that_names_focus_reaches_the_terminal_without_it() {
        for (pane, terminal) in [
            (&b"\x1b[?1004;2004l"[..], &b"\x1b[?2004l"[..]),
            (b"\x1b[?2004;1004;1l", b"\x1b[?2004;1l"),
            (b"\x1b[?1004l", b""),
            (b"\x1b[?10041l", b"\x1b[?10041l"),
            (b"\x1b[?1004;2004h", b"\x1b[?1004;2004h"),
        ] {
            assert_eq!(filtered(&[pane]), terminal, "{pane:?}");
            let pieces: Vec<&[u8]> = pane.chunks(1).collect();
            assert_eq!(filtered(&pieces), terminal, "{pane:?} split");
        }
    }

    /// An Esc key reaches the pane as it is read, never kept waiting for what
    /// might follow it.
    #[test]
    fn an_esc_is_sent_at_once() {
        let mut keys = Keys::default();
        assert_eq!(typed(&mut keys, b"\x1b", false, false).keys, b"\x1b");
        assert!(keys.pending.is_empty());
    }

    /// A paste's end mark split across reads is joined: a paste's bytes always
    /// follow, so waiting for them costs nothing, and a missed end would leave
    /// every later key inside the paste.
    #[test]
    fn a_paste_s_end_split_across_reads_is_joined() {
        for split in 1..PASTE_END.len() {
            let mut keys = Keys::default();
            let start = [PASTE_START, b"p", &PASTE_END[..split]].concat();
            let mut sent = typed(&mut keys, &start, false, false).keys;
            assert_eq!(
                sent,
                [PASTE_START, b"p"].concat(),
                "{split}: the mark waits"
            );
            let after = typed(
                &mut keys,
                &[&PASTE_END[split..], b"x"].concat(),
                false,
                true,
            );
            assert!(keys.paste.is_none(), "{split}: the paste ended");
            sent.extend(after.keys);
            assert_eq!(sent, [PASTE_START, b"p", PASTE_END].concat(), "{split}");
        }
    }

    #[test]
    fn a_report_inside_a_paste_is_text() {
        let paste = b"\x1b[200~x\x1b[Iy\x1b[O\x1b[201~\x1b[I";
        let typed_whole = typed(&mut Keys::default(), paste, false, false);
        assert_eq!(typed_whole.keys, &paste[..paste.len() - 3]);
        assert_eq!(
            typed_whole.focus,
            Some(true),
            "only the report after the paste"
        );
    }

    /// A paste that starts while a question is held is swallowed to its end, even
    /// once the hold is gone; one that starts unheld reaches the pane whole.
    #[test]
    fn a_paste_is_swallowed_whole_or_not_at_all() {
        let mut keys = Keys::default();
        let start = typed(&mut keys, b"\x1b[200~ab", false, true);
        assert!(start.keys.is_empty() && start.while_held);
        assert!(typed(&mut keys, b"c\x03d\x1b[20", false, false)
            .keys
            .is_empty());
        let end = typed(&mut keys, b"1~", false, false);
        assert!(end.keys.is_empty());
        assert!(end.while_held, "the swallowed end is reported");
        let mut keys = Keys::default();
        let start = typed(&mut keys, b"\x1b[200~ab", false, false);
        assert_eq!(start.keys, b"\x1b[200~ab");
        let end = typed(&mut keys, b"c\x1b[201~d", false, true);
        assert_eq!(end.keys, b"c\x1b[201~");
        assert!(end.while_held);
    }

    /// The first key of a line releases a held question and is swallowed; the
    /// rest of the line, typed while Claude draws the question, is swallowed
    /// too. Keys reach the pane again once typing has paused, and mouse reports
    /// do not keep the pause going.
    #[test]
    fn a_line_that_releases_a_question_is_swallowed_to_its_pause() {
        let mut keys = Keys::default();
        let first = typed(&mut keys, b"g", false, true);
        assert!(first.keys.is_empty() && first.while_held);
        let rest = typed(&mut keys, b"o\r", false, false);
        assert!(
            rest.keys.is_empty(),
            "the hold is gone, the line is not over"
        );
        assert_eq!(typed(&mut keys, b"\x03", false, false).keys, b"\x03");
        let paused = Instant::now() - SWALLOW_UNTIL_PAUSE - Duration::from_millis(1);
        keys.swallowed_at = Some(paused);
        let moved = typed(&mut keys, b"\x1b[<35;10;5M", false, true);
        assert!(moved.keys.is_empty() && moved.while_held);
        assert_eq!(
            keys.swallowed_at,
            Some(paused),
            "a mouse report is not typing"
        );
        assert_eq!(typed(&mut keys, b"1", false, false).keys, b"1");
        assert_eq!(
            typed(&mut Keys::default(), b"go\r", false, false).keys,
            b"go\r",
            "nothing held, nothing swallowed"
        );
    }

    /// A question released by anything but a swallowed key — the tab coming to
    /// the front, the phone, a client that attached — is drawn a few
    /// milliseconds after the release: a line typed then is swallowed to its
    /// pause just the same.
    #[test]
    fn a_line_typed_just_after_a_release_is_swallowed_to_its_pause() {
        let mut keys = Keys::default();
        let released = Some(Instant::now());
        assert!(keys.feed(b"go", false, false, released).keys.is_empty());
        assert!(keys.feed(b"\r", false, false, released).keys.is_empty());
        let long_ago = Some(Instant::now() - SWALLOW_UNTIL_PAUSE - Duration::from_millis(1));
        let mut keys = Keys::default();
        assert_eq!(keys.feed(b"go\r", false, false, long_ago).keys, b"go\r");
    }

    /// Ctrl+C and Ctrl+Z in the kitty keyboard protocol and modifyOtherKeys, as
    /// Claude asks Warp and other terminals for, pass while a question is held:
    /// with Caps or Num Lock on, and on a keyboard layout whose key is not `c`
    /// or `z` but whose base-layout key is. Other modified keys do not.
    #[test]
    fn encoded_ctrl_c_and_ctrl_z_pass_while_a_question_is_held() {
        for key in [
            &b"\x1b[99;5u"[..],
            b"\x1b[122;5u",
            b"\x1b[99;69u",
            b"\x1b[99;133u",
            b"\x1b[122;197u",
            b"\x1b[1089::99;5u",
            b"\x1b[1103::122;5u",
            b"\x1b[27;5;99~",
            b"\x1b[27;5;122~",
        ] {
            let held = typed(
                &mut Keys::default(),
                &[b"a", key, b"b"].concat(),
                false,
                true,
            );
            assert_eq!(held.keys, key, "{key:?}");
        }
        for key in [
            &b"\x1b[99;6u"[..],
            b"\x1b[99;3u",
            b"\x1b[100;5u",
            b"\x1b[1089;5u",
            b"\x1b[27;6;99~",
            b"\x1b[A",
        ] {
            assert!(
                typed(&mut Keys::default(), key, false, true)
                    .keys
                    .is_empty(),
                "{key:?}"
            );
        }
    }

    #[test]
    fn ctrl_c_and_ctrl_z_pass_while_a_question_is_held() {
        let held = typed(&mut Keys::default(), b"a\x03b\x1a\r\x1b", false, true);
        assert_eq!(held.keys, b"\x03\x1a");
        assert!(held.while_held);
        assert!(
            typed(&mut Keys::default(), b"\x1b", false, true)
                .keys
                .is_empty(),
            "an Esc is swallowed"
        );
    }

    /// With no question held every key reaches the pane byte for byte, in one read
    /// or a byte at a time.
    #[test]
    fn keys_pass_byte_for_byte_when_nothing_is_held() {
        let input: &[u8] =
            b"plain\r\x7f\x1b[A\x1bOP\x1b\x1b[1;5D\xe2\x9c\xb3\x03\x1a\x1b[200~p\x03\x1b[201~\x1b[2~\x1b";
        let whole = typed(&mut Keys::default(), input, false, false);
        assert_eq!(whole.keys, input);
        assert!(!whole.while_held);
        let mut keys = Keys::default();
        let mut bytewise = Vec::new();
        for piece in input.chunks(1) {
            bytewise.append(&mut typed(&mut keys, piece, false, false).keys);
        }
        assert_eq!(bytewise, input);
    }

    fn state(line: &str) -> PaneState {
        PaneState::parse(line.as_bytes()).expect("a pane state")
    }

    #[test]
    fn the_pane_state_is_read_as_tmux_writes_it() {
        let read = state("30 2 10 1 0 1 1 1 0 1 0 1 0 2 0 bar 1 \u{2733} Claude Code");
        assert_eq!(read.height, 30);
        assert_eq!(read.cursor, (2, 10));
        assert!(read.cursor_visible && !read.alternate && read.bracketed_paste);
        assert_eq!(read.mouse, [false, true, false, true, false]);
        assert_eq!(read.key_mode, 2);
        assert_eq!(read.cursor_shape, Some(5));
        assert_eq!(read.title, "\u{2733} Claude Code");
        assert!(!read.stopped);
        assert!(state("24 0 0 1 0 0 0 0 0 0 0 0 0 0 1 default 0 ").stopped);
        assert_eq!(state("24 0 0 1 0 0 0 0 0 0 0 0 0 0 0 default 0").title, "");
        assert!(PaneState::parse(b"24 0 0").is_none());
    }

    #[test]
    fn a_paint_puts_the_pane_s_rows_on_the_terminal_s_rows() {
        let capture: Vec<Vec<u8>> = ["old", "row0", "row1", "", ""]
            .iter()
            .map(|row| row.as_bytes().to_vec())
            .collect();
        let modes = state("4 3 1 0 0 1 0 0 0 0 0 0 0 2 0 default 0 title");
        // A fresh terminal: the history and the rows in use, then one row up so
        // the pane's first row is the terminal's first; never the unused rows.
        assert_eq!(
            paint(&modes, &capture, Some(0)),
            b"old\r\nrow0\r\nrow1\x1b[0m\x1b[1S\x1b[0m\x1b[2;4H\x1b[?2004h\x1b[>4;2m\x1b]0;title\x07\x1b[?25l"
        );
        // Text above the cursor scrolls away with the history.
        let plain = state("4 0 1 1 0 0 0 0 0 0 0 0 0 0 0 default 0 ");
        assert_eq!(
            paint(&plain, &capture, Some(1)),
            b"old\r\nrow0\r\nrow1\x1b[0m\x1b[2S\x1b[0m\x1b[2;1H"
        );
        // More rows than the terminal has: its own scrolling does part of it.
        assert_eq!(
            paint(&plain, &capture, Some(3)),
            b"old\r\nrow0\r\nrow1\x1b[0m\x1b[2S\x1b[0m\x1b[2;1H"
        );
        // An empty pane in a fresh terminal writes nothing at all; a row of
        // plain spaces is as empty as a row of nothing.
        let empty = vec![Vec::new(), b"   ".to_vec(), Vec::new(), Vec::new()];
        let blank = state("4 0 0 1 0 0 0 0 0 0 0 0 0 0 0 default 0 ");
        assert_eq!(paint(&blank, &empty, Some(0)), b"\x1b[0m\x1b[1;1H");
        // The cursor below the last text keeps its row; here the terminal's
        // own scrolling already lines the rows up.
        let low = state("4 0 3 1 0 0 0 0 0 0 0 0 0 0 0 default 0 ");
        assert_eq!(
            paint(&low, &capture, Some(0)),
            b"old\r\nrow0\r\nrow1\r\n\r\n\x1b[0m\x1b[4;1H"
        );
        // A terminal that never said where its cursor is gets every row.
        assert_eq!(
            paint(&plain, &capture, None),
            b"old\r\nrow0\r\nrow1\r\n\r\n\x1b[0m\x1b[2;1H"
        );
        // The alternate screen: its screen only, never the history under it.
        let alternate = state("4 0 0 1 1 0 0 0 0 0 1 1 0 0 0 default 0 ");
        assert_eq!(
            paint(&alternate, &capture, Some(0)),
            b"\x1b[?1049h\x1b[H\x1b[2Jrow0\r\nrow1\r\n\r\n\x1b[0m\x1b[1;1H\x1b[?1003h\x1b[?1006h"
        );
    }

    /// A row erased in a colour carries it into the next row of the capture;
    /// the line break between them is written with the default background and
    /// the colour is set again after it. Nothing else is repeated, and a paint
    /// grows with the capture, never with its square.
    #[test]
    fn a_line_break_never_carries_a_background_into_a_scroll() {
        let rows: Vec<Vec<u8>> = [
            &b"\x1b[48;2;56;58;60m    "[..],
            b"\x1b[1mask\x1b[0;2mdim",
            b"\x1b[38;5;3;48;5;61mcolour\x1b[39m",
            b"\x1b[49mplain",
            b"end",
        ]
        .iter()
        .map(|row| row.to_vec())
        .collect();
        let mut out = Vec::new();
        rows_to(&mut out, rows.iter());
        assert_eq!(
            out,
            b"\x1b[48;2;56;58;60m    \x1b[49m\r\n\x1b[48;2;56;58;60m\x1b[1mask\x1b[0;2mdim\r\n\
              \x1b[38;5;3;48;5;61mcolour\x1b[39m\x1b[49m\r\n\x1b[48;5;61m\x1b[49mplain\r\nend"
        );
        let many: Vec<Vec<u8>> = (0..2000)
            .map(|i| format!("\x1b[38;5;{}mdef\x1b[39m f{i:05}", 1 + i % 200).into_bytes())
            .collect();
        let size: usize = many.iter().map(Vec::len).sum::<usize>() + 2 * many.len();
        let mut out = Vec::new();
        rows_to(&mut out, many.iter());
        assert!(
            out.len() <= size,
            "{} bytes for a {size}-byte capture",
            out.len()
        );
    }

    #[test]
    fn the_background_in_force_is_followed_through_every_sgr_spelling() {
        for (params, expected) in [
            (&b"41"[..], Some(&b"41"[..])),
            (b"1;104", Some(b"104")),
            (b"48;5;61", Some(b"48;5;61")),
            (b"48;2;1;2;3", Some(b"48;2;1;2;3")),
            (b"48:2::1:2:3", Some(b"48:2::1:2:3")),
            (b"38;5;41", None),
            (b"38;2;40;41;42", None),
            (b"48;5;61;0", None),
            (b"48;5;61;49", None),
            (b"", None),
        ] {
            let mut background = None;
            follow_background(params, &mut background);
            assert_eq!(background.as_deref(), expected, "{params:?}");
        }
        let mut background = Some(b"41".to_vec());
        follow_background(b"1;38;5;2", &mut background);
        assert_eq!(
            background.as_deref(),
            Some(&b"41"[..]),
            "a foreground keeps it"
        );
    }
}
