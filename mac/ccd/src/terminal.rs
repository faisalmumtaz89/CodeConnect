//! **A run's shell** (`run_terminal`): one interactive login shell per run, in the run's folder on this computer,
//! its bytes carried over the paired WebSocket to the one viewer that holds it.
//!
//! The shell is the run's, never a connection's. The first open starts it; a viewer folding away, a link dropping or
//! the app quitting only lets it go, and it runs on. It ends with the run (a run an account switch moved is not
//! ended and keeps it), when it exits by itself, and when the daemon stops.
//!
//! One task per shell owns everything that touches the terminal: the child, the master side of the terminal, and the
//! order its bytes are read and written in. A viewer is an [`Attachment`]: the connection that holds one pulls its
//! output from the shell's ring as its credit allows and writes it from its own loop, so nothing here writes to a
//! socket. While a viewer holds the shell, the task reads no further than [`READ_AHEAD`] past what that viewer was
//! sent, so a program printing faster than the viewer takes it blocks, as on any terminal; with no viewer the task
//! reads on, and the ring keeps the last [`RING_BYTES`] for the next one.

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use protocol::agent::AgentKind;
use protocol::event::Lifecycle;
use protocol::ws::{
    terminal_closed, MAX_TERMINAL_CHUNK_BYTES, TERMINAL_COLS, TERMINAL_INPUT_CREDIT,
    TERMINAL_MAX_OUTSTANDING_CREDIT, TERMINAL_ROWS,
};
use tokio::io::unix::AsyncFd;
use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use crate::state::Daemon;

/// The most shells one daemon keeps at once.
pub(crate) const MAX_SHELLS: usize = 16;

/// The output a shell keeps for its next viewer.
const RING_BYTES: usize = 256 * 1024;

/// How far the task reads past what the holding viewer was sent. Below [`RING_BYTES`], so nothing a viewer has not
/// been sent ever leaves the ring while it holds the shell.
const READ_AHEAD: u64 = 64 * 1024;

/// How long output may wait on a viewer's spent credit before the viewer is closed `slow_consumer`.
pub(crate) const STALL_DEADLINE: Duration = Duration::from_secs(30);

/// How long an ended shell and its foreground job have after their hang-up before they are killed.
const HANGUP_GRACE: Duration = Duration::from_secs(5);

/// How long a shell that exited by itself has for its last output to be read.
const LAST_OUTPUT: Duration = Duration::from_millis(200);

/// How long a killed program's new parent has to reap it.
const KILLED_REAPED: Duration = Duration::from_secs(2);

const POLL: Duration = Duration::from_millis(50);

/// The daemon's shells, by run uid.
#[derive(Default)]
pub(crate) struct Terminals {
    shells: Mutex<HashMap<String, Arc<Shell>>>,
    /// Opens one at a time, so the count of shells is checked and a new one registered as one step.
    opening: tokio::sync::Mutex<()>,
    generation: AtomicU64,
}

/// One run's shell, as its task and its viewers share it.
pub(crate) struct Shell {
    folder: String,
    state: Mutex<State>,
    /// Bumped on every change a viewer waits for.
    version: watch::Sender<u64>,
    /// Wakes the task: a viewer changed something it acts on.
    wake: Notify,
}

#[derive(Default)]
struct State {
    /// The last output, `start` being the offset of its first byte.
    ring: VecDeque<u8>,
    start: u64,
    holder: Option<Holder>,
    /// Input waiting to be written, each chunk tagged with the generation of the viewer that sent it.
    input: VecDeque<(u64, Vec<u8>)>,
    resize: Option<(u16, u16)>,
    /// A fresh viewer was given the ring: the foreground program is asked to redraw.
    redraw: bool,
    /// The shell is to end, and why viewers are told it did.
    end: Option<&'static str>,
    /// The shell is gone, and why.
    closed: Option<&'static str>,
}

/// The viewer holding the shell.
struct Holder {
    generation: u64,
    /// The output offset sent to it so far.
    sent: u64,
    /// Output it is ready for.
    credit: u64,
    /// Its first output clears its screen.
    reset: bool,
    /// Its input written to the shell and not yet given back as credit.
    written: u64,
    /// Input it may still send before credit comes back.
    input_left: u64,
    /// Since when output has waited on its spent credit.
    stalled: Option<Instant>,
}

impl State {
    fn end_offset(&self) -> u64 {
        self.start + self.ring.len() as u64
    }

    fn holds(&self, generation: u64) -> Option<&Holder> {
        self.holder
            .as_ref()
            .filter(|holder| holder.generation == generation)
    }

    fn want_read(&self) -> bool {
        let end = self.end_offset();
        self.holder
            .as_ref()
            .is_none_or(|holder| end - holder.sent < READ_AHEAD)
    }

    fn push(&mut self, bytes: &[u8]) {
        self.ring.extend(bytes);
        let excess = self.ring.len().saturating_sub(RING_BYTES);
        self.ring.drain(..excess);
        self.start += excess as u64;
        if let Some(holder) = self
            .holder
            .as_mut()
            .filter(|holder| holder.sent < self.start)
        {
            holder.sent = self.start;
            holder.reset = true;
        }
    }

    fn note_stall(&mut self, now: Instant) {
        let end = self.end_offset();
        if let Some(holder) = self.holder.as_mut() {
            if holder.credit == 0 && end > holder.sent {
                holder.stalled.get_or_insert(now);
            } else {
                holder.stalled = None;
            }
        }
    }
}

impl Shell {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|held| held.into_inner())
    }

    fn changed(&self) {
        self.version.send_modify(|version| *version += 1);
    }
}

/// What an open asks for.
pub(crate) struct OpenRequest {
    pub(crate) attachment_id: String,
    pub(crate) session_uid: String,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) output_credit: u32,
    pub(crate) resume_after: Option<u64>,
    pub(crate) takeover: bool,
    /// The paired device asking, for the log.
    pub(crate) device_id: String,
}

/// An open that took the run's shell.
pub(crate) struct Opened {
    pub(crate) attachment: Attachment,
    pub(crate) folder: String,
    pub(crate) replay_end: u64,
}

/// A viewer's hold on a run's shell. Dropping it lets the shell go, with any of its input not yet written.
pub(crate) struct Attachment {
    pub(crate) id: String,
    shell: Arc<Shell>,
    generation: u64,
    version: watch::Receiver<u64>,
    done: bool,
}

/// What a viewer is sent next.
#[derive(Debug, PartialEq)]
pub(crate) enum Step {
    Output {
        data: Vec<u8>,
        reset: bool,
        offset: u64,
    },
    InputCredit(u32),
    Closed(&'static str),
}

impl Attachment {
    /// Bytes typed into the terminal, for the task to write. `Err` when they are more than its rules allow.
    pub(crate) fn input(&self, data: Vec<u8>) -> Result<(), ()> {
        let mut state = self.shell.lock();
        let Some(holder) = state
            .holder
            .as_mut()
            .filter(|holder| holder.generation == self.generation)
        else {
            // Taken by another viewer: this one is told so by its next step, and its input goes nowhere.
            return Ok(());
        };
        let len = data.len() as u64;
        if len > u64::from(MAX_TERMINAL_CHUNK_BYTES) || len > holder.input_left {
            return Err(());
        }
        holder.input_left -= len;
        state.input.push_back((self.generation, data));
        drop(state);
        self.shell.wake.notify_one();
        Ok(())
    }

    /// The viewer is ready for `bytes` more output. `Err` past [`TERMINAL_MAX_OUTSTANDING_CREDIT`].
    pub(crate) fn credit(&self, bytes: u32) -> Result<(), ()> {
        let mut state = self.shell.lock();
        let Some(holder) = state
            .holder
            .as_mut()
            .filter(|holder| holder.generation == self.generation)
        else {
            return Ok(());
        };
        let credit = holder.credit + u64::from(bytes);
        if credit > u64::from(TERMINAL_MAX_OUTSTANDING_CREDIT) {
            return Err(());
        }
        holder.credit = credit;
        state.note_stall(Instant::now());
        Ok(())
    }

    /// The viewer's size changed.
    pub(crate) fn resize(&self, cols: u16, rows: u16) {
        let mut state = self.shell.lock();
        if state.holds(self.generation).is_some() {
            state.resize = Some((cols, rows));
            drop(state);
            self.shell.wake.notify_one();
        }
    }

    /// Wait until [`Attachment::next`] has something. Cancel-safe.
    pub(crate) async fn ready(&mut self) {
        loop {
            let deadline = {
                let state = self.shell.lock();
                if self.waiting(&state) {
                    return;
                }
                state
                    .holds(self.generation)
                    .and_then(|holder| holder.stalled)
                    .map(|since| since + STALL_DEADLINE)
            };
            tokio::select! {
                changed = self.version.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return,
            }
        }
    }

    fn waiting(&self, state: &State) -> bool {
        if self.done {
            return false;
        }
        let Some(holder) = state.holds(self.generation) else {
            return true;
        };
        let pending = state.end_offset() > holder.sent;
        holder.written > 0
            || holder.reset
            || (holder.credit > 0 && pending)
            || (state.closed.is_some() && !pending)
            || holder
                .stalled
                .is_some_and(|since| since + STALL_DEADLINE <= Instant::now())
    }

    /// The next thing to send this viewer, one bounded chunk at a time; `None` when there is nothing yet. A shell
    /// that is gone still sends what it printed last, as credit allows, before its [`Step::Closed`]; after that there
    /// is never anything more.
    pub(crate) fn next(&mut self) -> Option<Step> {
        if self.done {
            return None;
        }
        let now = Instant::now();
        let mut state = self.shell.lock();
        let closed = state.closed;
        let (start, end) = (state.start, state.end_offset());
        let Some(holder) = state
            .holder
            .as_mut()
            .filter(|holder| holder.generation == self.generation)
        else {
            self.done = true;
            return Some(Step::Closed(closed.unwrap_or(terminal_closed::SUPERSEDED)));
        };
        if holder.written > 0 {
            let bytes = holder.written.min(u64::from(u32::MAX)) as u32;
            holder.written -= u64::from(bytes);
            holder.input_left += u64::from(bytes);
            return Some(Step::InputCredit(bytes));
        }
        if holder.reset || (holder.credit > 0 && end > holder.sent) {
            let len = (end - holder.sent)
                .min(holder.credit)
                .min(u64::from(MAX_TERMINAL_CHUNK_BYTES));
            let from = (holder.sent - start) as usize;
            holder.sent += len;
            holder.credit -= len;
            let reset = std::mem::take(&mut holder.reset);
            let offset = holder.sent;
            let data = state
                .ring
                .range(from..from + len as usize)
                .copied()
                .collect();
            state.note_stall(now);
            drop(state);
            // The task may be waiting for room to read into.
            self.shell.wake.notify_one();
            return Some(Step::Output {
                data,
                reset,
                offset,
            });
        }
        let stalled_out = holder
            .stalled
            .is_some_and(|since| since + STALL_DEADLINE <= now);
        if let Some(reason) = closed {
            if end > holder.sent && !stalled_out {
                return None;
            }
            self.done = true;
            return Some(Step::Closed(reason));
        }
        if stalled_out {
            state.holder = None;
            drop(state);
            self.shell.wake.notify_one();
            self.done = true;
            return Some(Step::Closed(terminal_closed::SLOW_CONSUMER));
        }
        None
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let mut state = self.shell.lock();
        if state.holds(self.generation).is_some() {
            state.holder = None;
            let generation = self.generation;
            state.input.retain(|(sent_by, _)| *sent_by != generation);
            drop(state);
            self.shell.wake.notify_one();
        }
    }
}

impl Terminals {
    /// End the run `session_uid`'s shell, if it has one, viewers told `reason`. The hang-up runs on the shell's
    /// own task.
    pub(crate) fn end(&self, session_uid: &str, reason: &'static str) {
        let shell = self.lock().get(session_uid).cloned();
        if let Some(shell) = shell {
            shell.lock().end.get_or_insert(reason);
            shell.wake.notify_one();
        }
    }

    /// End every shell, as the daemon stops, and wait for their hang-ups to finish.
    pub(crate) async fn end_all(&self) {
        let shells: Vec<Arc<Shell>> = self.lock().values().cloned().collect();
        for shell in &shells {
            shell.lock().end.get_or_insert(terminal_closed::STOPPING);
            shell.wake.notify_one();
        }
        let deadline = Instant::now() + HANGUP_GRACE + Duration::from_secs(1);
        while Instant::now() < deadline && shells.iter().any(|shell| shell.lock().closed.is_none())
        {
            tokio::time::sleep(POLL).await;
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<Shell>>> {
        self.shells.lock().unwrap_or_else(|held| held.into_inner())
    }
}

/// Open, or attach to, the run's shell, as [`protocol::ws::ClientMessage::TerminalOpen`] asks. `Err` is the reason
/// the viewer is closed with.
pub(crate) async fn open(
    daemon: &Arc<Daemon>,
    request: OpenRequest,
) -> Result<Opened, &'static str> {
    if !TERMINAL_COLS.contains(&request.cols)
        || !TERMINAL_ROWS.contains(&request.rows)
        || request.output_credit > TERMINAL_MAX_OUTSTANDING_CREDIT
    {
        return Err(terminal_closed::PROTOCOL_ERROR);
    }
    let terminals = &daemon.terminals;
    let _opening = terminals.opening.lock().await;
    // Under the gate the run's end is recorded under, so a shell is never registered for a run that just ended.
    let gate = daemon.registration_gate(&request.session_uid).await;
    let _registering = gate.lock().await;
    let uid = request.session_uid.clone();
    let row = daemon
        .db
        .get_session(uid.clone())
        .await
        .ok()
        .flatten()
        .ok_or(terminal_closed::NOT_LIVE)?;
    let moved = daemon.db.switch_state(uid.clone()).await.ok().flatten()
        == Some(crate::store::Store::SWITCH_MOVED);
    if row.lifecycle != Lifecycle::Live && !moved {
        return Err(terminal_closed::NOT_LIVE);
    }
    // A Claude run started in a new worktree is in the folder its first report names, which claude makes itself;
    // until then the row holds the folder it was started from.
    if row.agent == AgentKind::Claude
        && row.claude_session_id.is_none()
        && starts_in_a_new_worktree(daemon, &uid).await
    {
        return Err(terminal_closed::NOT_READY);
    }
    let existing = terminals.lock().get(&uid).cloned().filter(|shell| {
        let state = shell.lock();
        state.closed.is_none() && state.end.is_none()
    });
    let shell = match existing {
        Some(shell) => shell,
        None => {
            if terminals.lock().len() >= MAX_SHELLS {
                return Err(terminal_closed::UNAVAILABLE);
            }
            let shell =
                start(daemon, &uid, &row.cwd, request.cols, request.rows).map_err(|err| {
                    crate::log_error!("{uid}: could not start its shell: {err:#}");
                    terminal_closed::UNAVAILABLE
                })?;
            terminals.lock().insert(uid.clone(), Arc::clone(&shell));
            shell
        }
    };
    let generation = terminals.generation.fetch_add(1, Ordering::Relaxed) + 1;
    let replay_end = {
        let mut state = shell.lock();
        if state.holder.is_some() && !request.takeover {
            return Err(terminal_closed::SUPERSEDED);
        }
        let end = state.end_offset();
        let (sent, reset) = match request.resume_after {
            Some(offset) if (state.start..=end).contains(&offset) => (offset, false),
            _ => (state.start, true),
        };
        state.holder = Some(Holder {
            generation,
            sent,
            credit: u64::from(request.output_credit),
            reset,
            written: 0,
            input_left: u64::from(TERMINAL_INPUT_CREDIT),
            stalled: None,
        });
        state.input.clear();
        state.resize = Some((request.cols, request.rows));
        state.redraw |= reset;
        state.note_stall(Instant::now());
        end
    };
    // A viewer this open took the shell from learns it at its next step.
    shell.changed();
    shell.wake.notify_one();
    crate::log_info!("{uid}: device {} took its shell", request.device_id);
    Ok(Opened {
        attachment: Attachment {
            id: request.attachment_id,
            version: shell.version.subscribe(),
            shell: Arc::clone(&shell),
            generation,
            done: false,
        },
        folder: shell.folder.clone(),
        replay_end,
    })
}

/// Whether the hosted run `uid` was started in a worktree claude makes, as its kept start line says.
async fn starts_in_a_new_worktree(daemon: &Daemon, uid: &str) -> bool {
    let start = daemon
        .db
        .hosted_link(uid.to_string())
        .await
        .ok()
        .flatten()
        .and_then(|link| link.start);
    matches!(
        start.and_then(|line| serde_json::from_str(&line).ok()),
        Some(protocol::host::ToHost::Start { options, .. }) if options.worktree.is_some()
    )
}

/// Start the run's shell in `folder`, and its task.
fn start(
    daemon: &Arc<Daemon>,
    uid: &str,
    folder: &str,
    cols: u16,
    rows: u16,
) -> io::Result<Arc<Shell>> {
    let environment = crate::start_session::phone_environment();
    let program = login_shell(&environment);
    let (master, slave) = open_terminal(cols, rows)?;
    let mut command = tokio::process::Command::new(&program);
    command
        .arg("-l")
        .current_dir(folder)
        .env_clear()
        .envs(environment.iter())
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor");
    let child = crate::spawn::spawn_in_terminal(command, slave)?;
    let master = AsyncFd::new(master)?;
    // Each shell counts its output from a base of its own, so an offset a viewer kept from an earlier shell never
    // names a place in this one.
    let base = daemon.terminals.generation.fetch_add(1, Ordering::Relaxed) << 40;
    let shell = Arc::new(Shell {
        folder: folder.to_string(),
        state: Mutex::new(State {
            start: base,
            ..State::default()
        }),
        version: watch::channel(0).0,
        wake: Notify::new(),
    });
    crate::log_info!("{uid}: started its shell {} in {folder}", program.display());
    tokio::spawn(run(
        Arc::clone(daemon),
        uid.to_string(),
        Arc::clone(&shell),
        child,
        master,
    ));
    Ok(shell)
}

/// The account's login shell: `$SHELL` as its session has it, else its entry in the password database, else
/// `/bin/sh`.
fn login_shell(environment: &HashMap<OsString, OsString>) -> std::path::PathBuf {
    if let Some(shell) = environment
        .get(OsStr::new("SHELL"))
        .map(Path::new)
        .filter(|shell| shell.is_absolute() && shell.is_file())
    {
        return shell.to_path_buf();
    }
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: getpwuid_r writes into `entry` and `buffer`, which outlive the read of `pw_shell` below.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status == 0 && !found.is_null() && !entry.pw_shell.is_null() {
        // SAFETY: a non-null `pw_shell` from getpwuid_r is a NUL-terminated string inside `buffer`.
        let shell = unsafe { CStr::from_ptr(entry.pw_shell) };
        let shell = Path::new(OsStr::from_bytes(shell.to_bytes()));
        if shell.is_absolute() && shell.is_file() {
            return shell.to_path_buf();
        }
    }
    "/bin/sh".into()
}

/// A new terminal of `cols` by `rows`: its master, non-blocking, and its slave, both close-on-exec from the moment
/// they exist, so no child another thread starts meanwhile inherits either.
fn open_terminal(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let master = open_fd(
        c"/dev/ptmx",
        libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
    )?;
    // SAFETY: plain calls on a descriptor this function owns.
    if unsafe { libc::grantpt(master.as_raw_fd()) } != 0
        || unsafe { libc::unlockpt(master.as_raw_fd()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let name = slave_name(master.as_raw_fd())?;
    let slave = open_fd(&name, libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC)?;
    set_size(master.as_raw_fd(), cols, rows);
    // SAFETY: as above.
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok((master, slave))
}

fn open_fd(path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: `path` is NUL-terminated; the descriptor returned is owned from here on.
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The slave's path. `ptsname` keeps it in a buffer of its own, so its callers take turns.
fn slave_name(master: RawFd) -> io::Result<CString> {
    static NAMING: Mutex<()> = Mutex::new(());
    let _turn = NAMING.lock().unwrap_or_else(|held| held.into_inner());
    // SAFETY: the returned pointer is read before anyone else may call ptsname.
    let name = unsafe { libc::ptsname(master) };
    if name.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a non-null ptsname result is a NUL-terminated string.
    Ok(unsafe { CStr::from_ptr(name) }.to_owned())
}

fn set_size(master: RawFd, cols: u16, rows: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads one `winsize` from the pointer.
    unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &size) };
}

/// The terminal's foreground process group, if it has one.
fn foreground(master: RawFd) -> Option<i32> {
    // SAFETY: tcgetpgrp only reads the terminal's state.
    let group = unsafe { libc::tcgetpgrp(master) };
    (group > 0).then_some(group)
}

/// The shell's task: everything that touches its terminal and its child, until it is gone.
async fn run(
    daemon: Arc<Daemon>,
    uid: String,
    shell: Arc<Shell>,
    mut child: tokio::process::Child,
    master: AsyncFd<OwnedFd>,
) {
    let mut buffer = vec![0u8; MAX_TERMINAL_CHUNK_BYTES as usize];
    let mut hung_up = false;
    let reason = loop {
        let (want_read, want_write, end, resize, redraw) = {
            let mut state = shell.lock();
            let redraw = std::mem::take(&mut state.redraw);
            (
                state.want_read(),
                !state.input.is_empty(),
                state.end,
                state.resize.take(),
                redraw,
            )
        };
        if let Some(reason) = end {
            hang_up(&mut child, master).await;
            break reason;
        }
        if let Some((cols, rows)) = resize {
            set_size(master.as_raw_fd(), cols, rows);
        }
        if redraw {
            if let Some(group) = foreground(master.as_raw_fd()) {
                // SAFETY: a signal to the terminal's own foreground group, asking it to redraw.
                unsafe { libc::kill(-group, libc::SIGWINCH) };
            }
        }
        tokio::select! {
            _ = child.wait() => {
                read_last_output(&shell, &master, &mut buffer).await;
                break terminal_closed::EXITED;
            }
            ready = master.readable(), if want_read && !hung_up => {
                let Ok(mut guard) = ready else {
                    hung_up = true;
                    continue;
                };
                match guard.try_io(|fd| read(fd.as_raw_fd(), &mut buffer)) {
                    Ok(Ok(0)) | Ok(Err(_)) => hung_up = true,
                    Ok(Ok(len)) => {
                        let mut state = shell.lock();
                        state.push(&buffer[..len]);
                        state.note_stall(Instant::now());
                        drop(state);
                        shell.changed();
                    }
                    Err(_would_block) => {}
                }
            }
            ready = master.writable(), if want_write => {
                let Ok(mut guard) = ready else { continue };
                let chunk = {
                    let mut state = shell.lock();
                    let current = state.holder.as_ref().map(|holder| holder.generation);
                    state.input.retain(|(sent_by, _)| Some(*sent_by) == current);
                    state.input.front().map(|(sent_by, bytes)| (*sent_by, bytes.clone()))
                };
                let Some((sent_by, bytes)) = chunk else { continue };
                let written = match guard.try_io(|fd| write(fd.as_raw_fd(), &bytes)) {
                    Ok(Ok(written)) => written,
                    // The terminal takes no more input (it hung up): what waits for it goes nowhere.
                    Ok(Err(_)) => {
                        shell.lock().input.clear();
                        continue;
                    }
                    Err(_would_block) => continue,
                };
                {
                    let mut state = shell.lock();
                    if state.input.front().is_some_and(|(by, _)| *by == sent_by) {
                        let front = &mut state.input.front_mut().expect("checked above").1;
                        front.drain(..written);
                        if front.is_empty() {
                            state.input.pop_front();
                        }
                    }
                    if let Some(holder) = state.holder.as_mut().filter(|holder| holder.generation == sent_by) {
                        holder.written += written as u64;
                    }
                    drop(state);
                    shell.changed();
                }
            }
            () = shell.wake.notified() => {}
        }
    };
    // Out of the registry before its viewers hear it is gone, so whatever they do next finds it gone.
    let mut shells = daemon.terminals.lock();
    if shells
        .get(&uid)
        .is_some_and(|registered| Arc::ptr_eq(registered, &shell))
    {
        shells.remove(&uid);
    }
    drop(shells);
    shell.lock().closed = Some(reason);
    shell.changed();
    crate::log_info!("{uid}: its shell is gone ({reason})");
}

/// After the shell exited by itself: what it and its last programs printed, read until the terminal has no more.
async fn read_last_output(shell: &Shell, master: &AsyncFd<OwnedFd>, buffer: &mut [u8]) {
    let _ = tokio::time::timeout(LAST_OUTPUT, async {
        loop {
            let Ok(mut guard) = master.readable().await else {
                return;
            };
            match guard.try_io(|fd| read(fd.as_raw_fd(), buffer)) {
                Ok(Ok(0)) | Ok(Err(_)) => return,
                Ok(Ok(len)) => {
                    shell.lock().push(&buffer[..len]);
                    shell.changed();
                }
                Err(_would_block) => {}
            }
        }
    })
    .await;
}

/// End the shell: hang up its foreground job and itself, close the terminal, and kill whichever of the two groups
/// is left after [`HANGUP_GRACE`]. The shell is reaped before this returns.
async fn hang_up(child: &mut tokio::process::Child, master: AsyncFd<OwnedFd>) {
    // While unreaped, the shell's pid, which `setsid` made its session's and its group's id, cannot be reused.
    let shell_group = child.id().map(|pid| pid as i32);
    let groups: Vec<i32> = [shell_group, foreground(master.as_raw_fd())]
        .into_iter()
        .flatten()
        .fold(Vec::new(), |mut groups, group| {
            if !groups.contains(&group) {
                groups.push(group);
            }
            groups
        });
    for group in &groups {
        // SAFETY: the shell's own group, and the foreground group of the terminal it leads.
        unsafe { libc::kill(-group, libc::SIGHUP) };
    }
    drop(master);
    let deadline = Instant::now() + HANGUP_GRACE;
    loop {
        let exited = matches!(child.try_wait(), Ok(Some(_)));
        let left = groups
            .iter()
            .any(|group| protocol::proc_identity::group_has_members(*group));
        if (exited && !left) || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    for group in &groups {
        // A group with members keeps its id, so this is still the group hung up above.
        if protocol::proc_identity::group_has_members(*group) {
            // SAFETY: as above.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
    }
    let _ = child.wait().await;
    // A killed program is gone once its new parent has reaped it: the shell is said to have ended only then.
    let reaped = Instant::now() + KILLED_REAPED;
    while groups
        .iter()
        .any(|group| protocol::proc_identity::group_has_members(*group))
        && Instant::now() < reaped
    {
        tokio::time::sleep(POLL).await;
    }
}

fn read(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    // SAFETY: reads at most `buffer.len()` bytes into `buffer`.
    let len = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    if len < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(len as usize)
}

fn write(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: writes at most `bytes.len()` bytes from `bytes`.
    let len = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if len < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(len as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SessionRow, Store};
    use protocol::config::Config;

    /// A daemon with a private store, and one run in it: live, a Codex run's row (whose folder is known from its
    /// start), in a temporary folder of its own.
    struct Rig {
        daemon: Arc<Daemon>,
        uid: String,
        folder: std::path::PathBuf,
        _env: crate::ProcessEnvRead,
    }

    impl Rig {
        fn new(tag: &str) -> Rig {
            let env = crate::ProcessEnvRead::take();
            // A login zsh with no start-up file of its own in a fresh home asks to make one, and waits.
            let home = std::env::temp_dir().join("ccd-test-home");
            std::fs::create_dir_all(&home).unwrap();
            let zshrc = home.join(".zshrc");
            if !zshrc.exists() {
                std::fs::write(&zshrc, "").unwrap();
            }
            let folder =
                std::env::temp_dir().join(format!("ccd-terminal-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&folder).unwrap();
            let folder = folder.canonicalize().unwrap();
            let daemon = Daemon::new(
                Config::default(),
                Arc::new(Store::open_temp("ccd-terminal")),
                Arc::new(crate::apns::LoggingPushSender::new()),
                crate::state::Endpoint {
                    host: "test.ts.net".into(),
                    port: 8787,
                    tls: false,
                },
            );
            let uid = format!("01TERM{}", tag.to_uppercase());
            let rig = Rig {
                daemon,
                uid,
                folder,
                _env: env,
            };
            rig.begin(&rig.uid, AgentKind::Codex, Lifecycle::Live, None);
            rig
        }

        fn begin(
            &self,
            uid: &str,
            agent: AgentKind,
            lifecycle: Lifecycle,
            claude_session_id: Option<&str>,
        ) {
            self.begin_started(uid, agent, lifecycle, claude_session_id, None);
        }

        /// A run begun with `worktree` as its kept start line's new worktree, when it is a Claude start.
        fn begin_started(
            &self,
            uid: &str,
            agent: AgentKind,
            lifecycle: Lifecycle,
            claude_session_id: Option<&str>,
            worktree: Option<&str>,
        ) {
            let now = protocol::time::now_rfc3339();
            let row = SessionRow {
                session_uid: uid.into(),
                session_id: uid.into(),
                cwd: self.folder.to_str().unwrap().into(),
                claude_session_id: claude_session_id.map(str::to_string),
                transcript_path: None,
                lifecycle,
                created_at: now.clone(),
                updated_at: now,
                agent,
                codex_thread_id: None,
            };
            let start = (row.agent == AgentKind::Claude).then(|| {
                serde_json::to_string(&protocol::host::ToHost::Start {
                    uuid: "u".into(),
                    options: protocol::host::StartOptions {
                        agent: AgentKind::Claude,
                        cwd: row.cwd.clone(),
                        claude_path: "/nonexistent/claude".into(),
                        permission_mode: None,
                        trust: protocol::host::Trust::Trusted,
                        resume: None,
                        model: None,
                        effort: None,
                        worktree: worktree.map(str::to_string),
                        additional_directories: Vec::new(),
                        mail: protocol::host::McpCommand {
                            command: "/nonexistent/mail".into(),
                            args: Vec::new(),
                        },
                    },
                    prompt: String::new(),
                })
                .unwrap()
            });
            self.daemon
                .store
                .begin_run(&row, uid, start.as_deref(), None)
                .unwrap();
        }

        fn request(&self, attachment_id: &str) -> OpenRequest {
            OpenRequest {
                attachment_id: attachment_id.into(),
                session_uid: self.uid.clone(),
                cols: 80,
                rows: 24,
                output_credit: TERMINAL_MAX_OUTSTANDING_CREDIT,
                resume_after: None,
                takeover: true,
                device_id: "test-device".into(),
            }
        }

        async fn open(&self, request: OpenRequest) -> Attachment {
            open(&self.daemon, request)
                .await
                .map(|opened| opened.attachment)
                .expect("the shell opens")
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.folder);
        }
    }

    /// Everything the viewer is sent until its output holds `needle`, its credit given back as it reads: the
    /// output, the offset it reached, and whether the first output reset the screen.
    async fn read_until(attachment: &mut Attachment, needle: &str) -> (String, u64, bool) {
        let mut output = Vec::new();
        let mut offset = 0;
        let mut first_reset = None;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                attachment.ready().await;
                match attachment.next() {
                    Some(Step::Output {
                        data,
                        reset,
                        offset: reached,
                    }) => {
                        first_reset.get_or_insert(reset);
                        attachment.credit(data.len() as u32).unwrap();
                        output.extend_from_slice(&data);
                        offset = reached;
                        if String::from_utf8_lossy(&output).contains(needle) {
                            return;
                        }
                    }
                    Some(Step::Closed(reason)) => panic!(
                        "closed {reason} before {needle:?}: {}",
                        String::from_utf8_lossy(&output)
                    ),
                    Some(Step::InputCredit(_)) | None => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {needle:?} in: {}", String::from_utf8_lossy(&output)));
        (
            String::from_utf8_lossy(&output).into_owned(),
            offset,
            first_reset.unwrap_or(false),
        )
    }

    async fn closed(attachment: &mut Attachment) -> &'static str {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                attachment.ready().await;
                match attachment.next() {
                    Some(Step::Closed(reason)) => return reason,
                    Some(Step::Output { data, .. }) => {
                        attachment.credit(data.len() as u32).unwrap()
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("the viewer is closed")
    }

    fn type_line(attachment: &Attachment, line: &str) {
        attachment.input(format!("{line}\n").into_bytes()).unwrap();
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only asks whether the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// The number printed after `name=` in `output`.
    fn printed(output: &str, name: &str) -> i32 {
        let prefix = format!("{name}=");
        // Anywhere in a line: a shell may write its own escape codes and a carriage return before a command's output.
        output
            .lines()
            .filter_map(|line| line.split(prefix.as_str()).nth(1))
            .find_map(|rest| {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().ok()
            })
            .unwrap_or_else(|| panic!("no {prefix} in: {output}"))
    }

    /// **The shell is a login shell on a terminal of its own, in the run's folder**, sized as the viewer asked.
    #[tokio::test]
    async fn the_shell_runs_on_a_terminal_in_the_runs_folder() {
        let rig = Rig::new("folder");
        let mut viewer = rig.open(rig.request("t1")).await;
        type_line(
            &viewer,
            r#"printf 'cwd=%s\n' "$PWD"; [ -t 0 ] && echo on-a-terminal; stty size; printf 'term=%s\n' "$TERM""#,
        );
        let (output, _, reset) = read_until(&mut viewer, "term=xterm-256color").await;
        assert!(
            reset,
            "a first viewer's screen is cleared before the shell's output"
        );
        assert!(
            output.contains(&format!("cwd={}", rig.folder.display())),
            "{output}"
        );
        assert!(output.contains("on-a-terminal"), "{output}");
        assert!(output.contains("24 80"), "{output}");
        rig.daemon
            .terminals
            .end(&rig.uid, terminal_closed::RUN_ENDED);
        assert_eq!(closed(&mut viewer).await, terminal_closed::RUN_ENDED);
    }

    /// **Ending the run leaves nothing of its shell running**: the shell, a background job, and a foreground
    /// program that ignores the hang-up, which only the kill after the grace ends.
    #[tokio::test]
    async fn ending_the_run_ends_the_shell_and_its_jobs() {
        let rig = Rig::new("end");
        let mut viewer = rig.open(rig.request("t1")).await;
        type_line(
            &viewer,
            r#"echo shell=$$; sleep 1000 & echo background=$!; echo mark-$((1+1))"#,
        );
        let (output, _, _) = read_until(&mut viewer, "mark-2").await;
        let (shell, background) = (printed(&output, "shell"), printed(&output, "background"));
        type_line(
            &viewer,
            r#"sh -c 'trap "" HUP; echo foreground=$$; echo mark-$((2+1)); exec sleep 1001'"#,
        );
        let (output, _, _) = read_until(&mut viewer, "mark-3").await;
        let foreground = printed(&output, "foreground");
        assert!(alive(shell) && alive(background) && alive(foreground));
        rig.daemon
            .terminals
            .end(&rig.uid, terminal_closed::RUN_ENDED);
        assert_eq!(closed(&mut viewer).await, terminal_closed::RUN_ENDED);
        for (name, pid) in [
            ("shell", shell),
            ("background", background),
            ("foreground", foreground),
        ] {
            assert!(
                !alive(pid),
                "the {name} process {pid} outlived the run's end"
            );
        }
        assert!(
            rig.daemon.terminals.lock().is_empty(),
            "an ended shell leaves the registry"
        );
    }

    /// **The shell ends with its run's recorded end, but not with the end an account switch files**: a moved run is
    /// not ended and keeps its shell, which ends when that run is ended in its turn.
    #[tokio::test]
    async fn the_runs_end_ends_its_shell_and_a_switch_does_not() {
        const HOST: protocol::proc_identity::ProcessIdentity =
            protocol::proc_identity::ProcessIdentity {
                pid: 4242,
                birth: protocol::proc_identity::BirthIdentity {
                    start_sec: 1,
                    start_usec: 2,
                },
            };
        let rig = Rig::new("runend");
        rig.daemon.store.record_host(&rig.uid, &HOST, None).unwrap();
        let mut viewer = rig.open(rig.request("t1")).await;
        rig.daemon
            .store
            .set_switch_state(&rig.uid, Some(Store::SWITCH_MOVED))
            .unwrap();
        let end = |from| {
            rig.daemon.end_hosted_run(
                &rig.uid,
                HOST,
                None,
                from,
                Lifecycle::Exited,
                serde_json::json!({"exit_code": 0}),
            )
        };
        assert!(end(Lifecycle::Live).await.unwrap());
        type_line(&viewer, "echo still-$((1+1))");
        read_until(&mut viewer, "still-2").await;
        crate::switch::ended(&rig.daemon, &rig.uid).await;
        assert_eq!(closed(&mut viewer).await, terminal_closed::RUN_ENDED);

        // A second run, in the same rig: its plain end ends its shell, and an ended run opens none.
        let ended = "01TERMRUNENDTWO";
        rig.begin(ended, AgentKind::Codex, Lifecycle::Live, None);
        rig.daemon.store.record_host(ended, &HOST, None).unwrap();
        let other = |attachment_id: &str| OpenRequest {
            session_uid: ended.into(),
            ..rig.request(attachment_id)
        };
        let mut viewer = rig.open(other("t2")).await;
        assert!(rig
            .daemon
            .end_hosted_run(
                ended,
                HOST,
                None,
                Lifecycle::Live,
                Lifecycle::Exited,
                serde_json::json!({"exit_code": 0})
            )
            .await
            .unwrap());
        assert_eq!(closed(&mut viewer).await, terminal_closed::RUN_ENDED);
        let after = open(&rig.daemon, other("t3")).await;
        assert_eq!(
            after.err(),
            Some(terminal_closed::NOT_LIVE),
            "no shell for an ended run"
        );
    }

    /// **A switch whose relaunch fails ends the run, and its shell with it.**
    #[tokio::test]
    async fn a_failed_switch_ends_the_shell() {
        const HOST: protocol::proc_identity::ProcessIdentity =
            protocol::proc_identity::ProcessIdentity {
                pid: 4242,
                birth: protocol::proc_identity::BirthIdentity {
                    start_sec: 1,
                    start_usec: 2,
                },
            };
        let rig = Rig::new("switchfail");
        rig.daemon.store.record_host(&rig.uid, &HOST, None).unwrap();
        rig.daemon
            .store
            .set_switch_state(&rig.uid, Some(Store::SWITCH_MOVED))
            .unwrap();
        assert!(rig
            .daemon
            .end_hosted_run(
                &rig.uid,
                HOST,
                None,
                Lifecycle::Live,
                Lifecycle::Exited,
                serde_json::json!({"exit_code": 0})
            )
            .await
            .unwrap());
        let mut failed = rig.open(rig.request("t1")).await;
        crate::switch::relaunch_failed(&rig.daemon, &rig.uid, "signed out".into()).await;
        assert_eq!(closed(&mut failed).await, terminal_closed::RUN_ENDED);
    }

    /// **Typing never runs out**: input credit comes back as the shell takes it, so a viewer types far past the
    /// credit it was first given.
    #[tokio::test]
    async fn a_viewer_types_past_its_first_credit() {
        let rig = Rig::new("typing");
        let mut viewer = rig.open(rig.request("t1")).await;
        // Into a program that takes it all without echoing it, so the test is about credit, not about a shell's editor.
        type_line(
            &viewer,
            "stty -echo; cat > /dev/null; stty echo; echo typed-$((1+1))",
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        let line = format!("{}\n", "a".repeat(127));
        // Sends `bytes`, waiting for input credit to come back whenever it has run out.
        async fn send(viewer: &mut Attachment, bytes: &[u8]) {
            while viewer.input(bytes.to_vec()).is_err() {
                loop {
                    viewer.ready().await;
                    match viewer.next() {
                        Some(Step::InputCredit(_)) => break,
                        Some(Step::Output { data, .. }) => {
                            viewer.credit(data.len() as u32).unwrap()
                        }
                        Some(Step::Closed(reason)) => panic!("closed {reason} while typing"),
                        None => {}
                    }
                }
            }
        }
        let mut typed = 0;
        let typing = tokio::time::timeout(Duration::from_secs(20), async {
            while typed < 3 * TERMINAL_INPUT_CREDIT as usize {
                send(&mut viewer, line.as_bytes()).await;
                typed += line.len();
            }
            // End of input for `cat`: the shell goes on to the marker.
            send(&mut viewer, &[4]).await;
        })
        .await;
        assert!(typing.is_ok(), "typing stopped after {typed} bytes");
        read_until(&mut viewer, "typed-2").await;
    }

    /// **A shell that exits leaves its last output readable**: what it printed last still reaches a viewer that had
    /// no credit for it when it exited, before the viewer is closed `exited`.
    #[tokio::test]
    async fn a_shell_that_exits_still_sends_what_it_printed_last() {
        let rig = Rig::new("lastwords");
        let mut viewer = rig
            .open(OpenRequest {
                output_credit: 0,
                ..rig.request("t1")
            })
            .await;
        viewer.ready().await;
        assert!(matches!(
            viewer.next(),
            Some(Step::Output { reset: true, .. })
        ));
        type_line(&viewer, "printf 'last-%s\\n' $((1+1)); exit");
        let shell = rig.daemon.terminals.lock().get(&rig.uid).cloned().unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            while shell.lock().closed.is_none() {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the shell exits");
        while let Some(step) = viewer.next() {
            assert!(
                matches!(step, Step::InputCredit(_)),
                "nothing but credit before the output is read: {step:?}"
            );
        }
        viewer.credit(TERMINAL_MAX_OUTSTANDING_CREDIT).unwrap();
        read_until(&mut viewer, "last-2").await;
        assert_eq!(closed(&mut viewer).await, terminal_closed::EXITED);
    }

    /// **An offset kept from a shell that exited names nothing in the next one**, so the viewer is reset, not
    /// resumed mid-stream.
    #[tokio::test]
    async fn an_offset_from_an_earlier_shell_resets_the_viewer() {
        let rig = Rig::new("oldoffset");
        let mut first = rig.open(rig.request("t1")).await;
        type_line(&first, "echo first-$((1+1))");
        let (_, offset, _) = read_until(&mut first, "first-2").await;
        type_line(&first, "exit");
        assert_eq!(closed(&mut first).await, terminal_closed::EXITED);
        // The fresh shell prints more than the first did, so the old offset would land inside its output.
        let mut fresh = rig.open(rig.request("t2")).await;
        type_line(
            &fresh,
            "printf 'x%.0s' $(seq 1 4000); echo; echo second-$((1+1))",
        );
        let (_, reached, _) = read_until(&mut fresh, "second-2").await;
        drop(fresh);
        let shell = rig.daemon.terminals.lock().get(&rig.uid).cloned().unwrap();
        assert!(
            reached - shell.lock().start > offset,
            "the fresh shell printed past the old offset"
        );
        let mut stale = rig
            .open(OpenRequest {
                resume_after: Some(offset),
                ..rig.request("t3")
            })
            .await;
        type_line(&stale, "echo third-$((1+1))");
        let (_, _, reset) = read_until(&mut stale, "third-2").await;
        assert!(reset, "an offset from the earlier shell resets the viewer");
    }

    /// **A viewer that folds away resumes where it left off; a fresh one gets a reset and what the shell kept.**
    /// The same shell all along.
    #[tokio::test]
    async fn a_viewer_resumes_by_offset_and_a_fresh_one_gets_the_ring() {
        let rig = Rig::new("resume");
        let mut first = rig.open(rig.request("t1")).await;
        type_line(&first, "echo shell=$$; echo before-the-$((1+1))");
        let (output, offset, _) = read_until(&mut first, "before-the-2").await;
        let shell = printed(&output, "shell");
        drop(first);
        let mut resumed = rig
            .open(OpenRequest {
                resume_after: Some(offset),
                ..rig.request("t2")
            })
            .await;
        type_line(&resumed, "echo shell=$$; echo after-the-$((1+1))");
        let (output, _, reset) = read_until(&mut resumed, "after-the-2").await;
        assert!(!reset, "a resumed viewer keeps its screen");
        assert!(
            !output.contains("before-the-2"),
            "nothing it already showed is sent again: {output}"
        );
        assert_eq!(printed(&output, "shell"), shell, "the same shell");
        drop(resumed);
        let mut fresh = rig.open(rig.request("t3")).await;
        let (output, _, reset) = read_until(&mut fresh, "after-the-2").await;
        assert!(reset, "a fresh viewer's screen is cleared first");
        assert!(
            output.contains("before-the-2"),
            "the ring replays what the shell printed: {output}"
        );
    }

    /// **One viewer at a time**: an open that does not take over finds the shell held and is turned away, and one
    /// that takes over closes the other `superseded`.
    #[tokio::test]
    async fn only_a_takeover_takes_the_shell_from_another_viewer() {
        let rig = Rig::new("takeover");
        let mut first = rig.open(rig.request("t1")).await;
        let refused = open(
            &rig.daemon,
            OpenRequest {
                takeover: false,
                ..rig.request("t2")
            },
        )
        .await;
        assert_eq!(refused.err(), Some(terminal_closed::SUPERSEDED));
        let _second = rig.open(rig.request("t3")).await;
        assert_eq!(closed(&mut first).await, terminal_closed::SUPERSEDED);
    }

    /// **Only a live run's shell, and a Claude run's only once it named its folder.**
    #[tokio::test]
    async fn an_ended_or_unready_run_is_refused() {
        let rig = Rig::new("refused");
        rig.begin("01ENDED", AgentKind::Codex, Lifecycle::Exited, None);
        rig.begin_started(
            "01CLAUDENEW",
            AgentKind::Claude,
            Lifecycle::Live,
            None,
            Some("feature-x"),
        );
        rig.begin("01CLAUDEHERE", AgentKind::Claude, Lifecycle::Live, None);
        for (uid, reason) in [
            ("01ENDED", terminal_closed::NOT_LIVE),
            ("01NOSUCHRUN", terminal_closed::NOT_LIVE),
            ("01CLAUDENEW", terminal_closed::NOT_READY),
        ] {
            let refused = open(
                &rig.daemon,
                OpenRequest {
                    session_uid: uid.into(),
                    ..rig.request("t1")
                },
            )
            .await;
            assert_eq!(refused.err(), Some(reason), "{uid}");
        }
        // A Claude run started in its own folder with no first message has no report yet, and its folder is known.
        let mut here = rig
            .open(OpenRequest {
                session_uid: "01CLAUDEHERE".into(),
                ..rig.request("t2")
            })
            .await;
        rig.daemon
            .terminals
            .end("01CLAUDEHERE", terminal_closed::RUN_ENDED);
        assert_eq!(closed(&mut here).await, terminal_closed::RUN_ENDED);
        let bad_size = open(
            &rig.daemon,
            OpenRequest {
                cols: 1,
                ..rig.request("t1")
            },
        )
        .await;
        assert_eq!(bad_size.err(), Some(terminal_closed::PROTOCOL_ERROR));
        assert!(
            rig.daemon.terminals.lock().is_empty(),
            "no refused open started a shell"
        );
    }

    /// **Output waits on the viewer's credit, and the shell reads no further ahead than [`READ_AHEAD`]**, so a
    /// program printing faster than the viewer takes it waits.
    #[tokio::test]
    async fn output_waits_on_credit_and_reads_only_so_far_ahead() {
        let rig = Rig::new("credit");
        let mut viewer = rig
            .open(OpenRequest {
                output_credit: 0,
                ..rig.request("t1")
            })
            .await;
        // The reset comes first, carrying nothing: there is no credit for anything.
        viewer.ready().await;
        assert!(
            matches!(viewer.next(), Some(Step::Output { ref data, reset: true, .. }) if data.is_empty())
        );
        type_line(
            &viewer,
            "head -c 1000000 /dev/zero | tr '\\0' x; echo; echo done-$((1+1))",
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        let shell = rig.daemon.terminals.lock().get(&rig.uid).cloned().unwrap();
        let held = shell.lock().end_offset();
        assert!(
            held <= READ_AHEAD + u64::from(MAX_TERMINAL_CHUNK_BYTES),
            "read {held} bytes ahead of a viewer with no credit"
        );
        while let Some(step) = viewer.next() {
            assert!(
                matches!(step, Step::InputCredit(_)),
                "nothing is sent without credit: {step:?}"
            );
        }
        viewer.credit(TERMINAL_MAX_OUTSTANDING_CREDIT).unwrap();
        read_until(&mut viewer, "done-2").await;
        assert!(
            viewer.credit(TERMINAL_MAX_OUTSTANDING_CREDIT + 1).is_err(),
            "credit past the most a viewer may hold"
        );
    }

    /// **Input is bounded by its credit**, which comes back as it is written.
    #[tokio::test]
    async fn input_is_bounded_and_its_credit_comes_back() {
        let rig = Rig::new("input");
        let mut viewer = rig.open(rig.request("t1")).await;
        assert!(
            viewer
                .input(vec![b'a'; MAX_TERMINAL_CHUNK_BYTES as usize + 1])
                .is_err(),
            "a chunk past the most one carries"
        );
        let line = b"echo typed\n".to_vec();
        viewer.input(line.clone()).unwrap();
        let mut given_back = 0;
        tokio::time::timeout(Duration::from_secs(10), async {
            while given_back < line.len() as u32 {
                viewer.ready().await;
                match viewer.next() {
                    Some(Step::InputCredit(bytes)) => given_back += bytes,
                    Some(Step::Output { data, .. }) => viewer.credit(data.len() as u32).unwrap(),
                    _ => {}
                }
            }
        })
        .await
        .expect("the written input's credit comes back");
        assert_eq!(given_back, line.len() as u32);
        let shell = rig.daemon.terminals.lock().get(&rig.uid).cloned().unwrap();
        shell.lock().holder.as_mut().unwrap().input_left = 3;
        assert!(
            viewer.input(b"four".to_vec()).is_err(),
            "input past the credit left"
        );
    }

    /// **A shell that exits by itself closes its viewer `exited`, and the next open starts a fresh one.**
    #[tokio::test]
    async fn a_shell_that_exits_is_replaced_by_the_next_open() {
        let rig = Rig::new("exit");
        let mut viewer = rig.open(rig.request("t1")).await;
        type_line(&viewer, "echo shell=$$; echo mark-$((1+1))");
        let (output, _, _) = read_until(&mut viewer, "mark-2").await;
        let first = printed(&output, "shell");
        type_line(&viewer, "exit");
        assert_eq!(closed(&mut viewer).await, terminal_closed::EXITED);
        assert!(!alive(first), "the exited shell was reaped");
        let mut again = rig.open(rig.request("t2")).await;
        type_line(&again, "echo shell=$$; echo mark-$((1+1))");
        let (output, _, _) = read_until(&mut again, "mark-2").await;
        assert_ne!(printed(&output, "shell"), first, "a fresh shell");
        rig.daemon
            .terminals
            .end(&rig.uid, terminal_closed::RUN_ENDED);
        closed(&mut again).await;
    }

    /// **A viewer whose output waits on spent credit past [`STALL_DEADLINE`] is closed `slow_consumer`**, and the
    /// shell is free for the next.
    #[test]
    fn a_viewer_stalled_past_the_deadline_is_closed() {
        let shell = Arc::new(Shell {
            folder: "/".into(),
            state: Mutex::new(State::default()),
            version: watch::channel(0).0,
            wake: Notify::new(),
        });
        let mut viewer = Attachment {
            id: "t1".into(),
            version: shell.version.subscribe(),
            shell: Arc::clone(&shell),
            generation: 1,
            done: false,
        };
        let mut state = shell.lock();
        state.push(b"waiting");
        state.holder = Some(Holder {
            generation: 1,
            sent: 0,
            credit: 0,
            reset: false,
            written: 0,
            input_left: 0,
            stalled: Some(Instant::now()),
        });
        drop(state);
        assert_eq!(viewer.next(), None, "not yet past the deadline");
        shell.lock().holder.as_mut().unwrap().stalled = Instant::now().checked_sub(STALL_DEADLINE);
        assert_eq!(
            viewer.next(),
            Some(Step::Closed(terminal_closed::SLOW_CONSUMER))
        );
        assert!(shell.lock().holder.is_none(), "the shell is let go");
    }
}
