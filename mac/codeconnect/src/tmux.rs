//! The private tmux server (`tmux -L codeconnect`).
//!
//! tmux is the session host, not a scraping surface. `capture-pane` is used for
//! two things only — a text snapshot to mirror, and a positive presence check
//! before injecting keys. Nothing here ever infers agent *semantics* from
//! terminal bytes; that is the failure mode that killed agentapi and Omnara.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use protocol::proc::{run_deadlined, RunOutcome};
pub use protocol::tmux::{search_path, SessionPresence};

/// How long a non-interactive tmux client may take before it is killed.
///
/// tmux answers these in single-digit milliseconds; this is two orders of
/// magnitude of headroom. It is a *per-call* bound: the daemon's patience
/// for a whole request is `supervisor_timeout_ms`, whose default is derived
/// from the worst-case sum of these calls plus recovery's observation
/// windows — see its definition in `protocol::config`. What this bound
/// buys: a tmux client the server never services becomes an answer the
/// phone hears, not a supervisor blocked in `wait4` indefinitely.
const OPERATION_DEADLINE: Duration = Duration::from_secs(1);

/// Why a tmux invocation produced no useful answer — three different facts,
/// because callers that just *typed something* need to know which one is true.
#[derive(Debug)]
pub enum TmuxError {
    /// The tmux process could not be started at all. Proof that nothing ran.
    Spawn {
        what: String,
        source: std::io::Error,
    },
    /// tmux ran past its deadline and was killed and reaped. For a command
    /// that mutates, this is indeterminate: the server may have acted before
    /// stalling, and no later evidence can settle it.
    TimedOut { what: String, waited: Duration },
    /// tmux ran and answered no. The server processed and refused it.
    Failed {
        what: String,
        status: std::process::ExitStatus,
        stderr: String,
    },
    /// The first `typed` of `of` bytes were typed and the rest failed with
    /// `source`: text typed in two invocations has partly landed, so a retry
    /// would type that part again.
    Partial {
        typed: usize,
        of: usize,
        source: Box<TmuxError>,
    },
}

impl std::fmt::Display for TmuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TmuxError::Spawn { what, source } => {
                write!(f, "tmux could not be started for {what}: {source}")
            }
            TmuxError::TimedOut { what, waited } => write!(
                f,
                "tmux did not answer {what} within {}ms and was killed",
                waited.as_millis()
            ),
            TmuxError::Failed {
                what,
                status,
                stderr,
            } => {
                let stderr = stderr.trim();
                if stderr.is_empty() {
                    write!(f, "tmux {what} failed with {status}")
                } else {
                    write!(f, "tmux {what} failed with {status}: {stderr}")
                }
            }
            TmuxError::Partial { typed, of, source } => {
                write!(f, "{typed} of {of} bytes were typed, then {source}")
            }
        }
    }
}

impl std::error::Error for TmuxError {}

impl TmuxError {
    /// Whether this failure leaves open the possibility that the command
    /// *acted* before failing. A spawn failure or a refusal proves it did
    /// not; a kill at the deadline proves nothing either way, and text that
    /// partly landed has acted.
    pub fn outcome_is_indeterminate(&self) -> bool {
        matches!(self, TmuxError::TimedOut { .. } | TmuxError::Partial { .. })
    }
}

pub fn tmux_bin() -> Result<PathBuf> {
    protocol::tmux::tmux_bin().ok_or_else(|| {
        anyhow::anyhow!(
            "tmux not found: `codeconnect update` installs CodeConnect's own, or set \
             CODECONNECT_TMUX to a tmux"
        )
    })
}

fn base() -> Result<Command> {
    let mut command = Command::new(tmux_bin()?);
    command.args(server_flag(protocol::TMUX_SOCKET_NAME));
    Ok(command)
}

/// Every non-interactive tmux invocation goes through here: one place where
/// the deadline, the kill, the reap, and the three failure facts live.
fn run_tmux(command: &mut Command, deadline: Duration, what: &str) -> Result<Vec<u8>, TmuxError> {
    match run_deadlined(command, deadline) {
        Err(source) => Err(TmuxError::Spawn {
            what: what.to_string(),
            source,
        }),
        Ok(RunOutcome::TimedOut { waited }) => Err(TmuxError::TimedOut {
            what: what.to_string(),
            waited,
        }),
        Ok(RunOutcome::Completed {
            status,
            stdout,
            stderr,
            ..
        }) => {
            if status.success() {
                Ok(stdout)
            } else {
                Err(TmuxError::Failed {
                    what: what.to_string(),
                    status,
                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                })
            }
        }
    }
}

/// Where macOS keeps the terminal descriptions every program reads, and where
/// `tmux-256color` sits among them: macOS names each folder by the hex code of
/// the first letter.
#[cfg(target_os = "macos")]
const SYSTEM_TERMINFO: &[&str] = &["/usr/share/terminfo"];
#[cfg(target_os = "macos")]
const TMUX_256COLOR: &str = "74/tmux-256color";

/// On Linux, every directory CodeConnect's tmux searches; distributions name
/// each folder by the first letter itself.
#[cfg(target_os = "linux")]
const SYSTEM_TERMINFO: &[&str] = &[
    "/etc/terminfo",
    "/lib/terminfo",
    "/usr/share/terminfo",
    "/usr/lib/terminfo",
];
#[cfg(target_os = "linux")]
const TMUX_256COLOR: &str = "t/tmux-256color";

/// The `TERM` a Codex pane is given, whichever tmux started the server and
/// with whatever options: `tmux-256color` where the system describes it
/// (macOS 14 and later, and the usual Linux distributions), so Codex gets
/// italics and tmux's own features; `screen-256color` where it does not
/// (macOS 13, a minimal Alpine), because programs there could not look the
/// other one up. tmux fixes its own default when it is built, and
/// one build of CodeConnect's tmux serves both; this is what Homebrew's tmux,
/// built separately for each macOS, has always chosen.
pub fn pane_terminal() -> &'static str {
    default_terminal(SYSTEM_TERMINFO.iter().map(std::path::Path::new))
}

fn default_terminal<'a>(terminfo: impl IntoIterator<Item = &'a std::path::Path>) -> &'static str {
    if terminfo
        .into_iter()
        .any(|dir| dir.join(TMUX_256COLOR).is_file())
    {
        "tmux-256color"
    } else {
        "screen-256color"
    }
}

/// Run a tmux command and capture stdout. `Ok(None)` when tmux exits non-zero.
///
/// Correct only for subcommands where non-zero genuinely means "nothing to
/// report" — listing sessions when no server is running. Anything that has to
/// distinguish *absent* from *unknown* must use [`session_presence`] instead,
/// which is why this collapses the two and that one does not. A spawn failure
/// or a deadline kill is neither: those stay errors.
fn run(args: &[&str]) -> Result<Option<String>> {
    run_on(protocol::TMUX_SOCKET_NAME, args)
}

/// [`run`] on the server at `socket`.
fn run_on(socket: &str, args: &[&str]) -> Result<Option<String>> {
    let what = args.first().copied().unwrap_or("tmux");
    let mut command = Command::new(tmux_bin()?);
    command.args(server_flag(socket)).args(args);
    match run_tmux(&mut command, OPERATION_DEADLINE, what) {
        Ok(stdout) => Ok(Some(String::from_utf8_lossy(&stdout).into_owned())),
        Err(TmuxError::Failed { .. }) => Ok(None),
        Err(refusal) => Err(refusal.into()),
    }
}

pub fn list_sessions() -> Result<Vec<String>> {
    // No server running is a normal state, not an error.
    let Some(out) = run(&["list-sessions", "-F", "#{session_name}"])? else {
        return Ok(Vec::new());
    };
    Ok(out
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

/// Whether a session exists on the private server, or whether we could not tell.
///
/// Delegated rather than reimplemented. `ccd` asks the same question about the
/// same sessions on its liveness sweep, and two independent readings of tmux's
/// stderr would be two chances for one of them to promote "we could not look"
/// into a reported exit. See [`protocol::tmux`].
pub fn session_presence(name: &str) -> SessionPresence {
    protocol::tmux::session_presence_on(protocol::TMUX_SOCKET_NAME, name)
}

/// Whether a session exists. An indeterminate answer is an error, not a `false`.
pub fn has_session(name: &str) -> Result<bool> {
    match session_presence(name) {
        SessionPresence::Present => Ok(true),
        SessionPresence::Gone => Ok(false),
        SessionPresence::Unknown(why) => {
            bail!("could not determine whether session {name:?} exists: {why}")
        }
    }
}

/// Lowest free `cc-<n>`. Reusing a freed number keeps names short and
/// predictable across a long-running machine.
pub fn next_session_name() -> Result<String> {
    let existing = list_sessions()?;
    for n in 1..=9999u32 {
        let candidate = format!("{}{n}", protocol::SESSION_PREFIX);
        if !existing.iter().any(|name| name == &candidate) {
            return Ok(candidate);
        }
    }
    bail!(
        "no free session name under {}9999",
        protocol::SESSION_PREFIX
    )
}

/// `arg` spelled so the pane's program receives it unchanged. tmux ends a command
/// at an argument that ends in `;` — the program got `x` for `x;` — and reads a
/// trailing `\;` as `;`, so a `\` goes before a final `;` (tmux 3.7b, measured with
/// runs of `;` and of `\` before it; the test below holds the shapes).
pub(crate) fn pane_argument(arg: &str) -> String {
    match arg.strip_suffix(';') {
        Some(rest) => format!("{rest}\\;"),
        None => arg.to_string(),
    }
}

/// What is read about a session in one answer from the server holding it: its
/// ids, that server's own identity, when the session was created and its run's
/// uid. A session's ids are handed out again by the next server once its server
/// exits, so the ids alone never name it.
fn identity_format() -> String {
    format!(
        "#{{session_id}} #{{pane_id}} #{{pid}} #{{start_time}} #{{session_created}} #{{{}}}",
        protocol::ENV_SESSION_UID
    )
}

/// A session and its pane from an [`identity_format`] answer by the server at
/// `socket`, pinned to that server — without its birth, which only proof of the
/// server's death needs.
fn identify(socket: &str, printed: &str) -> Option<(protocol::tmux::OwnedSession, String)> {
    let mut fields = printed.lines().next()?.split(' ');
    let session = fields.next()?;
    let pane = fields.next()?;
    let server_pid = fields.next()?.parse().ok()?;
    let server_start_time = fields.next()?.parse().ok()?;
    let session_created = fields.next()?.parse().ok()?;
    let uid = fields.next()?;
    (fields.next().is_none()
        && internal_id(session, '$')
        && internal_id(pane, '%')
        && protocol::uid::is_well_formed(uid))
    .then(|| {
        (
            protocol::tmux::OwnedSession {
                socket: socket.into(),
                session_id: session.into(),
                uid: uid.into(),
                server_pid,
                server_start_time,
                session_created,
                server_birth: None,
            },
            pane.into(),
        )
    })
}

/// Whether the session `server_a` pins — or, unpinned, the one stamped `uid` —
/// is still on the server at `socket`.
///
/// By uid, so a name another run has since reused is `Gone`, not a stranger's
/// `Present`. An unreachable server is `Unknown`: a live server can drop and
/// recreate its socket (`SIGUSR1`), or another can answer at its address. The pin
/// answers that silence without inventing anything: a session cannot outlive the
/// process hosting it, so its server proven dead by pid **and** birth — a fact
/// about a process, not a socket — is `Gone`.
pub fn owned_presence(
    socket: &str,
    uid: &str,
    server_a: Option<&protocol::tmux::OwnedSession>,
) -> SessionPresence {
    match protocol::tmux::owned_liveness(socket, uid, server_a) {
        protocol::tmux::OwnedLiveness::Live => SessionPresence::Present,
        protocol::tmux::OwnedLiveness::Gone => SessionPresence::Gone,
        protocol::tmux::OwnedLiveness::Unknown(_)
            if server_a.is_some_and(server_is_proven_dead) =>
        {
            SessionPresence::Gone
        }
        protocol::tmux::OwnedLiveness::Unknown(why) => SessionPresence::Unknown(why),
    }
}

/// Whether the tmux server a session was resolved against is **provably** gone.
///
/// The same binding the custodian's own `server_gone_evidence` uses, and the same
/// two refusals: a server whose birth identity was never captured proves nothing
/// (`server_birth: None` ⇒ `false`), and a pid that exists but cannot be described
/// proves nothing either — [`protocol::proc_identity::liveness`] answers `Unknown`
/// for both, and only a positive `Gone` is read as death here.
fn server_is_proven_dead(server_a: &protocol::tmux::OwnedSession) -> bool {
    let Some(birth) = server_a.server_birth else {
        return false;
    };
    protocol::proc_identity::liveness(&protocol::proc_identity::ProcessIdentity {
        pid: server_a.server_pid as i32,
        birth,
    }) == protocol::proc_identity::Liveness::Gone
}

/// Text snapshot of the session's active pane, with `lines` of scrollback.
/// `-J` joins wrapped lines so a needle split across a wrap still matches.
///
/// For anything that *decides* something, use [`capture_visible_pane`]: history
/// is not evidence about what is on screen now.
pub fn capture_pane(name: &str, lines: u32) -> Result<String> {
    let start = format!("-{lines}");
    let out = run(&[
        "capture-pane",
        "-p",
        "-J",
        "-S",
        &start,
        "-t",
        &target_pane(name),
    ])?;
    out.ok_or_else(|| anyhow::anyhow!("capture-pane failed for {name}"))
}

/// Text snapshot of **only what is on screen**.
///
/// Omitting `-S` is what does it: tmux's documented default for `capture-pane`
/// is the visible pane contents, and every other form reaches into history. That
/// distinction is load-bearing — a permission prompt that scrolled away half an
/// hour ago still contains the words "Do you want to proceed", and a presence
/// check reading scrollback would let it authorise typing into whatever is on
/// the screen now.
pub fn capture_visible_pane(name: &str) -> Result<String> {
    let out = run(&["capture-pane", "-p", "-J", "-t", &target_pane(name)])?;
    out.ok_or_else(|| anyhow::anyhow!("capture-pane failed for {name}"))
}

/// The folder the program in session `name`'s pane is in now, on the server at
/// `socket`: where an agent that changed directory has gone (`claude --worktree`
/// moves into its worktree), which the folder it launched in no longer says.
/// `None` when tmux cannot say, as for a session already over.
pub fn pane_current_path(socket: &str, name: &str) -> Option<String> {
    let out = run_on(
        socket,
        &[
            "display",
            "-p",
            "-t",
            &target_pane(name),
            "#{pane_current_path}",
        ],
    )
    .ok()??;
    let path = out.trim_end_matches('\n');
    (!path.is_empty()).then(|| path.to_string())
}

/// Who a pane's keystrokes reach.
///
/// Three states rather than a bool, because the two ways of losing the
/// keyboard need different things from the person at the Mac and the reason
/// travels to the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyboard {
    /// The program in the pane is receiving keys.
    Program,
    /// A view Claude opened has them, and the pane's cursor is hidden.
    View,
    /// tmux itself has them: the pane is in one of tmux's own modes, which an
    /// ordinary tmux client attached by hand can put it in.
    Scrollback(TmuxMode),
}

/// Which of tmux's modes is holding the pane, to the only resolution any
/// decision here needs: a bare scroll position, or something else.
///
/// The distinction exists because they are not the same kind of thing. A
/// scroll position is where the wheel leaves a pane, it is nobody's question,
/// and leaving it loses nothing. Every other mode is a thing the person at the
/// Mac opened and is looking at — and typing into one answers a question they
/// never saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxMode {
    /// Exactly one mode is on the pane and it is `copy-mode`: a scroll
    /// position, and nothing under it.
    CopyMode,
    /// Anything else — a mode that is not copy-mode, or a copy-mode with
    /// another mode underneath it.
    Other,
}

impl Keyboard {
    /// Is the program in the pane the thing that will receive these keys?
    pub fn reaches_the_program(self) -> bool {
        matches!(self, Keyboard::Program)
    }
}

/// Who has the pane's keyboard: the three facts that settle it, read together
/// in one question so they describe one moment.
///
/// **The cursor.** tmux's own record of the terminal's DECTCEM state. Claude's
/// composer keeps a visible cursor while idle, while a turn streams, and after
/// it ends (327 consecutive samples through a live turn, not one of them
/// hidden). Every view it opens hides it — including the one that leaves the
/// composer *drawn* underneath and takes no keys, where the composer-presence
/// check matches and lies. Reading the pane's text for a view's dismissal hint
/// would work too, until an agent wrote that hint into its own output; the
/// cursor cannot be spelled.
///
/// **The mode stack.** A visible cursor is not enough on its own. A pane in
/// one of tmux's modes routes every key to that mode's table instead of to the
/// program, and it does so with `cursor_flag` still at 1: measured on claude
/// 2.1.232 in copy-mode, `send-keys -l` exits 0, the text never reaches the
/// composer, and it is still absent after leaving the mode. CodeConnect's own
/// clients never enter one, but an ordinary tmux client attached by hand can.
///
/// `#{pane_in_mode}` **counts** the modes stacked on the pane rather than
/// flagging one, and several can be on at once (measured on tmux 3.7b: a pane
/// under `choose-tree` answers 1, and 2 with copy-mode above it). Zero is the
/// only value that means the program is receiving keys, so any non-zero count
/// is scrollback: copy-mode, clock-mode and tree-mode each swallow keystrokes.
///
/// **The mode's name**, because a count cannot say *what* is holding the pane
/// and only one of the answers may be left without asking. `#{pane_mode}`
/// names the mode on **top** of the stack (measured on tmux 3.7b: `copy-mode`,
/// `clock-mode`, `tree-mode` for `choose-tree`, `options-mode` for
/// `customize-mode`, `buffer-mode` for `choose-buffer`, `view-mode` for output
/// tmux prints into a pane — not an exhaustive list, and it does not need to
/// be: one name is accepted and everything else is refused) — so the name
/// alone would call a copy-mode stacked over a tree-mode a bare scroll
/// position. The count is what rules that out, which is why both are read and
/// only `1` with `copy-mode` is [`TmuxMode::CopyMode`].
///
/// The three fields are read with separators and split, never concatenated:
/// `1` and `10` cannot be told from `11` and `0` once they are one string. The
/// mode is asked for through `#{?pane_mode,…,none}` because tmux renders it
/// **empty** when no mode is up, and an empty last field is a field that
/// vanishes into the trim — `1 0 ` and `1 0` are the same string.
///
/// Older servers land safely, and it is the *count* that carries them rather
/// than the name: `pane_mode` has existed since tmux 2.5, but `pane_in_mode`
/// was a bool until 2.8 and became a stack count in 2.9. A 2.8 pane could hold
/// only one mode, so its `1` genuinely is a lone mode and the pair still means
/// here what it says.
pub fn who_has_the_keyboard(name: &str) -> Result<Keyboard> {
    let out = run(&["display", "-p", "-t", &target_pane(name), KEYBOARD_FORMAT])?
        .ok_or_else(|| anyhow::anyhow!("tmux display of the keyboard state failed for {name}"))?;
    read_keyboard(&out).with_context(|| format!("reading {name}'s keyboard state"))
}

/// The one question the keyboard state is read with. Shared so a test driving
/// its own tmux server reads the pane the same way the daemon reads the shared
/// one, rather than keeping a second, drifting copy of the format.
pub(crate) const KEYBOARD_FORMAT: &str =
    "#{cursor_flag} #{pane_in_mode} #{?pane_mode,#{pane_mode},none}";

/// The reply, read. Split out from the spawn so every shape tmux can answer
/// with is exercisable without one.
///
/// **A reply that is not exactly three fields is a look that failed, and a look
/// that failed is a refusal** — never a guess about who is holding the
/// keyboard. That is not hypothetical: `display -p` against a pane that does
/// not exist answers the separators with nothing between them and exits 0
/// (measured: `"  none"`), so nothing upstream catches it and the parse is the
/// only thing standing there.
pub(crate) fn read_keyboard(reply: &str) -> Result<Keyboard> {
    let reply = reply.trim();
    let mut fields = reply.split(' ');
    let (Some(cursor), Some(modes), Some(mode), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(anyhow::anyhow!(
            "tmux answered {reply:?}, which is not the three fields that were asked for"
        ));
    };
    let modes: u32 = modes
        .parse()
        .map_err(|_| anyhow::anyhow!("tmux answered {modes:?} as the mode count"))?;
    Ok(match (cursor, modes, mode) {
        // A mode intercepts keys whatever the cursor is doing, and leaving one
        // is a different act from dismissing a view, so it answers first. One
        // mode named `copy-mode` is a scroll position; a deeper stack has
        // something under it, and every other name is a view of its own.
        (_, 1, "copy-mode") => Keyboard::Scrollback(TmuxMode::CopyMode),
        (_, 1.., _) => Keyboard::Scrollback(TmuxMode::Other),
        ("1", 0, _) => Keyboard::Program,
        (_, 0, _) => Keyboard::View,
    })
}

/// Leave the pane's copy-mode, so the next keystroke reaches the program again.
///
/// `send-keys -X cancel` rather than `copy-mode -q`, and the difference is the
/// whole safety of doing this unasked. Measured on tmux 3.7b against a live
/// pane:
///
///   * `-X cancel` pops **exactly one** mode — a copy-mode over a `choose-tree`
///     goes from 2 to 1, leaving the tree the person at the Mac opened. The
///     same pane under `copy-mode -q` goes to **0**: it flattens the stack.
///   * `-X cancel` refuses the modes that are not copy-mode's own — clock-mode,
///     tree-mode, options-mode and buffer-mode each answer `not in a mode` and
///     exit 1, **untouched**. `copy-mode -q` clears a clock-mode as readily as
///     a scroll position.
///   * against a pane in no mode at all it exits 1 with `not in a mode` and
///     types nothing: the literal word `cancel` never reaches the program.
///     That is what makes it safe on the losing side of the race below —
///     `copy-mode -q` would instead succeed silently.
///   * it does not depend on `mode-keys`, which the `Escape` sent elsewhere in
///     this program does: under `mode-keys vi` — which tmux picks by itself
///     from `$EDITOR` — Escape is `clear-selection` and leaves the pane in the
///     mode, where `-X cancel` names the operation instead of a key for it.
///
/// So the guard in front of this decides *whether* to leave a mode, and this
/// command narrows that decision without replacing it — **with one exception**.
/// `view-mode`, which is what tmux pushes to show output in a pane (the stock
/// `prefix ?` binding does), shares copy-mode's command table, so `-X cancel`
/// pops that too. Nothing ever points this at one: [`read_keyboard`] calls
/// view-mode `Other` and the send refuses. What is left is the few
/// milliseconds between that look and this call, and the most that can be
/// taken in them is one view-mode pushed inside the window — after which the
/// look that follows finds the copy-mode still there and refuses, so nothing
/// is typed.
///
/// The result is reported but nothing is concluded from it: what matters is
/// the state of the pane afterwards, which the caller re-reads. A tmux that
/// exits 0 without leaving the mode and one that exits 1 having left it are
/// both answered by looking.
pub fn leave_copy_mode(name: &str) -> Result<(), TmuxError> {
    let target = target_pane(name);
    run_tmux(
        base()
            .map_err(|err| TmuxError::Spawn {
                what: "send-keys -X cancel".into(),
                source: std::io::Error::other(err.to_string()),
            })?
            .args(leave_copy_mode_args(&target)),
        OPERATION_DEADLINE,
        "send-keys -X cancel",
    )
    .map(|_| ())
}

/// That command as arguments, so a test driving its own tmux server runs the
/// one that ships rather than one that resembles it.
///
/// The order is part of the command, not a style: `cancel` is where tmux stops
/// reading flags, so a `-t` written after it is not the target. Measured on
/// tmux 3.7b, `send-keys -X cancel -t <pane>` against a pane in copy-mode
/// **exits 0 and leaves the mode standing**. Nothing unsafe follows from that
/// — the look after the exit sees the mode and refuses — but it would switch
/// this off for every send while reporting success, which is why the argv is
/// written once, here, and run against a real pane by the tests.
pub(crate) fn leave_copy_mode_args(target: &str) -> [&str; 5] {
    ["send-keys", "-t", target, "-X", "cancel"]
}

/// Type text literally. `-l` stops tmux from interpreting the text as key names,
/// which matters the moment a user sends anything containing "C-c" or "Enter".
///
/// It does not stop tmux's parser from reading an argument that ends in `;` as
/// the end of a command: `-l -- 'end;'` typed `end`, and `x\;` typed `x;` (tmux
/// 3.7b, measured). So a trailing run of `;` goes after the rest, as the byte it
/// is (`-H 3b`), which tmux reads as nothing but data, inside the same deadline.
/// The rest goes through `-l` in one invocation, exactly as before.
///
/// The typed error matters here more than anywhere: a caller that asked for
/// keys to be typed has to know whether a failure proves nothing landed
/// ([`TmuxError::Spawn`], [`TmuxError::Failed`]) or proves nothing at all
/// ([`TmuxError::TimedOut`] — the server may have acted before stalling — and
/// [`TmuxError::Partial`], when the text was typed and its `;` was not).
pub fn send_literal(socket: &str, name: &str, text: &str) -> Result<(), TmuxError> {
    let started = std::time::Instant::now();
    let send_keys = |keys: &[&str]| {
        let mut command = Command::new(tmux_bin().map_err(|err| TmuxError::Spawn {
            what: "send-keys".into(),
            source: std::io::Error::other(err.to_string()),
        })?);
        command
            .args(server_flag(socket))
            .args(["send-keys", "-t", &target_pane(name)])
            .args(keys);
        let left = OPERATION_DEADLINE.saturating_sub(started.elapsed());
        run_tmux(&mut command, left, "send-keys").map(|_| ())
    };
    let body = text.trim_end_matches(';');
    if !body.is_empty() {
        send_keys(&["-l", "--", body])?;
    }
    let semicolons = vec!["3b"; text.len() - body.len()];
    if semicolons.is_empty() {
        return Ok(());
    }
    send_keys(&[&["-H"][..], &semicolons].concat()).map_err(|source| {
        if body.is_empty() {
            source
        } else {
            TmuxError::Partial {
                typed: body.len(),
                of: text.len(),
                source: Box::new(source),
            }
        }
    })
}

/// Send a named key (`Enter`, `Escape`, ...). Same error contract as
/// [`send_literal`], for the same reason.
pub fn send_key(name: &str, key: &str) -> Result<(), TmuxError> {
    run_tmux(
        base()
            .map_err(|err| TmuxError::Spawn {
                what: format!("send-keys {key}"),
                source: std::io::Error::other(err.to_string()),
            })?
            .args(["send-keys", "-t", &target_pane(name), key]),
        OPERATION_DEADLINE,
        &format!("send-keys {key}"),
    )
    .map(|_| ())
}

/// Show a session in this terminal until it ends, through the control-mode
/// client in [`crate::attach`]: the agent draws in the terminal exactly as it does
/// run directly, and closing the tab leaves the session running.
pub fn attach(name: &str) -> Result<()> {
    let reply = run(&[
        "display-message",
        "-p",
        "-t",
        &target_pane(name),
        &identity_format(),
    ])?
    .unwrap_or_default();
    let Some((pin, pane)) = identify(protocol::TMUX_SOCKET_NAME, &reply) else {
        bail!("no session named {name}");
    };
    attach_pinned(&pin, &pane, None)
}

/// Attach to the session `pin` names, on its own server only.
fn attach_pinned(
    pin: &protocol::tmux::OwnedSession,
    pane: &str,
    on_paint: Option<Box<dyn FnOnce() + Send>>,
) -> Result<()> {
    let terminal = crate::attach::Terminal::open()?;
    let commands = terminal.attach_commands(&pin.session_id, pane);
    let argv = owned_attach_argv(
        pin,
        pane,
        &commands,
        "the session is gone from its tmux server",
    )?;
    crate::attach::Client::spawn(
        terminal,
        argv,
        pin.session_id.clone(),
        pane.into(),
        on_paint,
    )?
    .wait()
}

/// tmux's own id for a session (`$N`), window (`@N`) or pane (`%N`).
fn internal_id(value: &str, sigil: char) -> bool {
    value
        .strip_prefix(sigil)
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Attach before the TUI starts, so the terminal's colours are reported for the
/// pane before Codex asks for them. The caller owns the returned client and keeps
/// watching launch readiness while it runs.
pub fn spawn_owned_attach(
    expected: &protocol::tmux::OwnedSession,
) -> Result<crate::attach::Client> {
    let current = protocol::tmux::resolve_owned_session(&expected.socket, &expected.uid)
        .map_err(|why| anyhow::anyhow!("resolving the codex terminal: {why:?}"))?;
    if current != *expected || expected.server_birth.is_none() {
        bail!("the codex terminal changed before attachment");
    }
    let pane = run_tmux(
        Command::new(tmux_bin()?)
            .args(server_flag(&expected.socket))
            .args([
                "display-message",
                "-p",
                "-t",
                &expected.session_id,
                "#{pane_id}",
            ]),
        OPERATION_DEADLINE,
        "reading the codex pane",
    )?;
    let pane = String::from_utf8_lossy(&pane).trim().to_string();
    let terminal = crate::attach::Terminal::open()?;
    let commands = terminal.attach_commands(&expected.session_id, &pane);
    let argv = owned_attach_argv(
        expected,
        &pane,
        &commands,
        "the codex terminal changed before attachment",
    )?;
    crate::attach::Client::spawn(terminal, argv, expected.session_id.clone(), pane, None)
}

/// The server at `socket`, as the flags a tmux client opens with:
/// [`protocol::tmux::CLIENT_FLAGS`] first, so every client declares UTF-8, then the
/// address.
pub(crate) fn server_flag(socket: &str) -> [&str; 3] {
    let [utf8] = protocol::tmux::CLIENT_FLAGS;
    [utf8, if socket.contains('/') { "-S" } else { "-L" }, socket]
}

/// The pinned attach: `commands` run only if the server, the session and its pane
/// are still the ones resolved, and the check and the attach run on the same
/// server connection — an internal session id alone could otherwise bind to a
/// replacement server. Otherwise tmux prints `refusal` and fails.
fn owned_attach_argv(
    expected: &protocol::tmux::OwnedSession,
    pane: &str,
    commands: &[Vec<String>],
    refusal: &str,
) -> Result<Vec<String>> {
    let id = &expected.session_id;
    if !internal_id(id, '$')
        || !internal_id(pane, '%')
        || !protocol::uid::is_well_formed(&expected.uid)
        || expected.server_pid <= 0
        || expected.server_start_time <= 0
        || expected.session_created <= 0
    {
        bail!("{id} has an invalid attachment identity");
    }
    // Every argument is single-quoted for tmux's parser; none contains a quote
    // (ids and the uid are validated, and colour reports are rebuilt from hex).
    let mut list = Vec::new();
    for command in commands {
        if command.iter().any(|arg| arg.contains('\'')) {
            bail!("an attach command cannot be quoted");
        }
        list.push(
            command
                .iter()
                .map(|arg| format!("'{arg}'"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    let [utf8, flag, socket] = server_flag(&expected.socket);
    Ok(vec![
        "-N".into(),
        "-C".into(),
        utf8.into(),
        flag.into(),
        socket.into(),
        "if-shell".into(),
        "-F".into(),
        "-t".into(),
        id.clone(),
        pin_format(expected, pane),
        list.join(" ; "),
        format!("display-message -p '{refusal}' ; run-shell 'exit 1'"),
    ])
}

/// A format that is `1` only on the server `expected` was resolved against, in its
/// session, at `pane`. For `if-shell -F`, which expands formats alone: commands
/// that also expand `strftime` sequences would rewrite the `%` of a pane id.
fn pin_format(expected: &protocol::tmux::OwnedSession, pane: &str) -> String {
    let server = format!(
        "#{{&&:#{{==:#{{pid}},{}}},#{{==:#{{start_time}},{}}}}}",
        expected.server_pid, expected.server_start_time
    );
    let session = format!(
        "#{{&&:#{{==:#{{session_id}},{}}},#{{&&:#{{==:#{{session_created}},{}}},#{{==:#{{{}}},{}}}}}}}",
        expected.session_id, expected.session_created, protocol::ENV_SESSION_UID, expected.uid
    );
    let pane = format!("#{{==:#{{pane_id}},{pane}}}");
    format!("#{{&&:{server},#{{&&:{session},{pane}}}}}")
}

/// Exact *pane* target — the session's current pane.
///
/// The trailing colon is required and is not a stylistic choice. Measured on
/// tmux 3.7b: `capture-pane -t =cc-1` fails with "can't find pane" and
/// `set-option -t =cc-1` with "no such session", because both parse `-t` as a
/// **pane** target where a bare `=name` means a pane named `name`. `=cc-1:`
/// parses as "exact session cc-1, current pane" and works everywhere.
/// Dropping the `=` instead would silently reintroduce prefix matching, which
/// is worse: keys meant for `cc-1` would land in `cc-12`.
///
/// Only `has-session`, `attach-session` and `kill-session` take a real
/// target-session and accept the bare `=name` form.
fn target_pane(name: &str) -> String {
    format!("={name}:")
}

/// Current terminal size, so the session is created at the right dimensions
/// instead of starting at 80x24 and reflowing on attach.
pub fn terminal_size() -> Option<(u16, u16)> {
    let output = Command::new("/bin/stty")
        .arg("size")
        .stdin(std::fs::File::open("/dev/tty").ok()?)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let rows: u16 = parts.next()?.parse().ok()?;
    let cols: u16 = parts.next()?.parse().ok()?;
    (rows > 0 && cols > 0).then_some((cols, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::tmux::target_session;

    /// Server and session startup: the first client may have to fork the server
    /// and read its config before the command even begins.
    const STARTUP_DEADLINE: Duration = Duration::from_secs(5);

    #[test]
    fn the_shim_and_the_daemon_read_tmux_through_the_same_classifier() {
        // The wording table itself is tested in `protocol::tmux`, which is the
        // point: there is one of it now. What this asserts is that the shim did
        // not keep a private copy — a second reading of tmux's stderr is a
        // second chance for one side to promote "we could not look" into a
        // reported exit, and the two sides ask about the *same sessions*.
        assert_eq!(
            session_presence("cc-nonexistent-probe"),
            protocol::tmux::session_presence_on(protocol::TMUX_SOCKET_NAME, "cc-nonexistent-probe"),
        );
    }

    #[test]
    fn an_indeterminate_answer_is_an_error_not_a_false() {
        // `has_session` returns a bool, and a bool has no room for "we could
        // not tell". Callers get an error instead of a confident `false`.
        let unknown = SessionPresence::Unknown("permission denied".into());
        assert!(matches!(unknown, SessionPresence::Unknown(ref why) if why.contains("denied")));
    }

    /// Every reply tmux really gives, and what each one means. Measured on
    /// tmux 3.7b against a live pane: no mode answers `0` with an empty name,
    /// copy-mode, clock-mode, `choose-tree` and `customize-mode` each answer
    /// `1` — named `copy-mode`, `clock-mode`, `tree-mode` and `options-mode` —
    /// and stacking copy-mode on `choose-tree` answers `2`, still named
    /// `copy-mode`. The count is a count and the name is the **top** of the
    /// stack, so only the two together say "a scroll position and nothing
    /// under it".
    #[test]
    fn the_keyboard_reply_is_read_by_what_tmux_actually_answers() {
        for (reply, expected) in [
            ("1 0 none", Keyboard::Program),
            // A trailing newline is what a real reply carries.
            ("1 0 none\n", Keyboard::Program),
            ("0 0 none", Keyboard::View),
            // The wheel's own state, and the only one that may be left unasked.
            ("1 1 copy-mode", Keyboard::Scrollback(TmuxMode::CopyMode)),
            // A hidden cursor changes nothing: a mode holds the keyboard
            // whatever the program under it is drawing.
            ("0 1 copy-mode", Keyboard::Scrollback(TmuxMode::CopyMode)),
            // Modes the person at the Mac opened, which are never popped for
            // them. `clock-mode` answers `1` exactly as copy-mode does — the
            // name is the only thing that separates them.
            ("1 1 clock-mode", Keyboard::Scrollback(TmuxMode::Other)),
            ("1 1 tree-mode", Keyboard::Scrollback(TmuxMode::Other)),
            ("1 1 options-mode", Keyboard::Scrollback(TmuxMode::Other)),
            ("1 1 buffer-mode", Keyboard::Scrollback(TmuxMode::Other)),
            // `view-mode` is the one that shares copy-mode's command table, so
            // `send-keys -X cancel` would pop it. It is refused here instead:
            // tmux pushes it to show output — the stock `prefix ?` does — and
            // that is a thing on the screen to read, not a scroll position.
            ("1 1 view-mode", Keyboard::Scrollback(TmuxMode::Other)),
            // copy-mode over choose-tree: the name says `copy-mode` and the
            // count is what refuses it. This is the stack that a name-only
            // reading would have flattened.
            ("1 2 copy-mode", Keyboard::Scrollback(TmuxMode::Other)),
            ("1 42 copy-mode", Keyboard::Scrollback(TmuxMode::Other)),
            (
                "1 4294967295 copy-mode",
                Keyboard::Scrollback(TmuxMode::Other),
            ),
            // A tmux too old to know `#{pane_mode}` renders it empty, so the
            // format's substitution answers `none` and the mode is left to the
            // person at the Mac, exactly as before this could be left at all.
            ("1 1 none", Keyboard::Scrollback(TmuxMode::Other)),
        ] {
            assert_eq!(
                read_keyboard(reply).unwrap(),
                expected,
                "tmux answering {reply:?}"
            );
        }
    }

    /// A malformed reply is an error, and never a guess. The first two are the
    /// ones that matter: `display -p` against a pane that does not exist
    /// answers the separators with nothing between them and **exits 0**
    /// (measured on tmux 3.7b: `"  none"`, which trims to a bare `"none"`), so
    /// this parse is the only thing between a vanished session and a confident
    /// answer about its keyboard.
    #[test]
    fn a_reply_that_is_not_three_fields_is_an_error_and_never_a_guess() {
        for reply in [
            "  none",
            "none",
            " ",
            "",
            "1",
            "1 0",
            " 0 none",
            // Two spaces are an empty field, and which of them was asked for
            // is not something to assume.
            "1  0 copy-mode",
            "1 0 none extra",
            "1 x none",
            "1 -1 none",
        ] {
            assert!(
                read_keyboard(reply).is_err(),
                "tmux answering {reply:?} says nothing about who has the keyboard"
            );
        }
    }

    #[test]
    fn pane_targets_carry_the_colon_as_well_as_the_anchor() {
        // Both halves matter: without `=`, `cc-1` prefix-matches `cc-12`;
        // without the trailing `:`, a pane target is not found at all.
        assert_eq!(target_session("cc-1"), "=cc-1");
        assert_eq!(target_pane("cc-1"), "=cc-1:");
    }

    /// The pinned attach runs its commands only against the session, server and
    /// pane it resolved. Its `refresh-client -C` sizes the window only once the
    /// control client is attached, so the window's size shows which branch ran.
    #[test]
    fn the_pinned_attach_runs_only_against_the_resolved_session_and_pane() {
        let dir = std::env::temp_dir().join(format!("cc-attach-{}", protocol::uid::new().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        let server = TestServer {
            dir,
            pid: std::cell::Cell::new(None),
        };
        let uid = protocol::uid::new().unwrap();
        let stamp = format!("{}={uid}", protocol::ENV_SESSION_UID);
        let started = run_tmux(
            &mut server.command(&[
                "new-session",
                "-d",
                "-s",
                "attachment",
                "-x",
                "80",
                "-y",
                "24",
                "-e",
                &stamp,
                "--",
                "/bin/sleep",
                "60",
            ]),
            STARTUP_DEADLINE,
            "creating the attachment test session",
        );
        if let Ok(bytes) = run_tmux(
            &mut server.command(&["display-message", "-p", "#{pid}"]),
            OPERATION_DEADLINE,
            "reading the test server",
        ) {
            server
                .pid
                .set(String::from_utf8_lossy(&bytes).trim().parse().ok());
        }
        started.unwrap();
        let pin = protocol::tmux::resolve_owned_session(&server.socket(), &uid).unwrap();
        let pane = String::from_utf8(
            run_tmux(
                &mut server.command(&[
                    "display-message",
                    "-p",
                    "-t",
                    &pin.session_id,
                    "#{pane_id}",
                ]),
                OPERATION_DEADLINE,
                "reading the test pane",
            )
            .unwrap(),
        )
        .unwrap()
        .trim()
        .to_string();
        let commands = |size: &str| {
            vec![
                vec![
                    "attach-session".to_string(),
                    "-t".into(),
                    pin.session_id.clone(),
                ],
                vec!["refresh-client".to_string(), "-C".into(), size.into()],
            ]
        };
        let size = || {
            String::from_utf8(
                run_tmux(
                    &mut server.command(&[
                        "display-message",
                        "-p",
                        "-t",
                        &pin.session_id,
                        "#{window_width}x#{window_height}",
                    ]),
                    OPERATION_DEADLINE,
                    "reading the window size",
                )
                .unwrap(),
            )
            .unwrap()
            .trim()
            .to_string()
        };
        let mut variants = Vec::new();
        let mut wrong = pin.clone();
        wrong.uid = protocol::uid::new().unwrap();
        variants.push((wrong, pane.clone()));
        let mut wrong = pin.clone();
        wrong.server_pid += 1;
        variants.push((wrong, pane.clone()));
        let mut wrong = pin.clone();
        wrong.server_start_time += 1;
        variants.push((wrong, pane.clone()));
        let mut wrong = pin.clone();
        wrong.session_created += 1;
        variants.push((wrong, pane.clone()));
        variants.push((pin.clone(), "%999".into()));
        for (wrong, wrong_pane) in variants {
            let result = run_tmux(
                Command::new(tmux_bin().unwrap()).args(
                    owned_attach_argv(&wrong, &wrong_pane, &commands("91x17"), "refused").unwrap(),
                ),
                OPERATION_DEADLINE,
                "refusing a changed attachment",
            );
            assert!(
                matches!(result, Err(TmuxError::Failed { .. })),
                "{result:?}"
            );
            assert_eq!(size(), "80x24", "a refused attach changed the window");
        }
        // With its input closed the control client attaches, runs the rest of
        // the list and detaches.
        run_tmux(
            Command::new(tmux_bin().unwrap())
                .args(owned_attach_argv(&pin, &pane, &commands("91x17"), "refused").unwrap()),
            OPERATION_DEADLINE,
            "running the matching attachment",
        )
        .unwrap();
        assert_eq!(size(), "91x17");
        for bad_id in ["", "$", "$0 ; kill-server", "$-1"] {
            let mut wrong = pin.clone();
            wrong.session_id = bad_id.into();
            assert!(owned_attach_argv(&wrong, &pane, &commands("91x17"), "refused").is_err());
        }
        for bad_pane in ["", "%", "%1 ; kill-server", "$1"] {
            assert!(owned_attach_argv(&pin, bad_pane, &commands("91x17"), "refused").is_err());
        }
        let quoted = vec![vec!["display-message".to_string(), "it's".into()]];
        assert!(owned_attach_argv(&pin, &pane, &quoted, "refused").is_err());
        run_tmux(
            &mut server.command(&["kill-server"]),
            OPERATION_DEADLINE,
            "stopping the attachment test server",
        )
        .unwrap();
        assert!(proven_gone(
            server.pid.get().expect("test server pid recorded")
        ));
    }

    /// A pane's `TERM` is `tmux-256color` only where programs can look it up.
    #[test]
    fn panes_get_tmux_256color_only_where_the_system_describes_it() {
        let dir = std::env::temp_dir().join(format!("cc-terminfo-{}", std::process::id()));
        let entry = dir.join(TMUX_256COLOR);
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        assert_eq!(default_terminal([dir.as_path()]), "screen-256color");
        std::fs::write(&entry, b"").unwrap();
        assert_eq!(default_terminal([dir.as_path()]), "tmux-256color");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Every argument reaches the pane's program as it was given, those ending in
    /// `;` included.
    #[test]
    fn a_pane_s_arguments_reach_its_program_unchanged() {
        let Ok(tmux) = tmux_bin() else {
            return;
        };
        let dir = PathBuf::from(format!("/tmp/cc-pane-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s");
        let out = dir.join("out");
        let args = [
            "fix it;", ";", "a\\;", "a\\\\;", "e\\;;", "f;;", "x\\", "a\\;b", "plain",
        ];
        let mut command = Command::new(&tmux);
        command
            .arg("-S")
            .arg(&socket)
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "--",
                "/bin/sh",
                "-c",
            ])
            .arg(format!(
                r#"printf '%s\000' "$@" > {}; sleep 5"#,
                out.display()
            ))
            .arg("sh");
        for arg in args {
            command.arg(pane_argument(arg));
        }
        let status = command.status().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !out.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));
        let printed = std::fs::read(&out).unwrap_or_default();
        let _ = Command::new(&tmux)
            .arg("-S")
            .arg(&socket)
            .arg("kill-server")
            .output();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(status.success());
        let expected: Vec<u8> = args
            .iter()
            .flat_map(|arg| [arg.as_bytes(), b"\0"].concat())
            .collect();
        assert_eq!(
            String::from_utf8_lossy(&printed),
            String::from_utf8_lossy(&expected)
        );
    }

    /// A session on `server` running `script`, its server's pid noted from tmux's answer.
    fn launch_on(server: &TestServer, name: &str, script: &str) {
        let stamp = format!(
            "{}={}",
            protocol::ENV_SESSION_UID,
            protocol::uid::new().unwrap()
        );
        let printed = run_tmux(
            &mut server.command(&[
                "new-session",
                "-d",
                "-P",
                "-F",
                &identity_format(),
                "-s",
                name,
                "-e",
                &stamp,
                "--",
                "/bin/sh",
                "-c",
                script,
            ]),
            STARTUP_DEADLINE,
            "creating a test launch",
        )
        .unwrap();
        let (pin, _) = identify(&server.socket(), &String::from_utf8_lossy(&printed)).unwrap();
        server.pid.set(Some(pin.server_pid as i32));
    }

    /// **A folder reads back as itself from a client with no UTF-8 locale**, as
    /// `ccd` under launchd and the launcher in a container or a bare systemd unit
    /// are: without `-u`, tmux writes every byte outside printable ASCII as `_`.
    #[test]
    fn a_folder_reads_back_exactly_from_a_client_with_no_locale() {
        let server = TestServer::new("utf8");
        let folder = server.dir.join("unicode-ünï-☕-分支");
        std::fs::create_dir_all(&folder).unwrap();
        let start = folder.display().to_string();
        let status = server
            .command(&["new-session", "-d", "-s", "u", "-c", &start, "sleep", "60"])
            .status()
            .unwrap();
        assert!(status.success());
        let socket = server.socket();
        let read = Command::new(tmux_bin().unwrap())
            .env_clear()
            .args(server_flag(&socket))
            .args([
                "display",
                "-p",
                "-t",
                &target_pane("u"),
                "#{pane_current_path}",
            ])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&read.stdout).trim_end(),
            folder.canonicalize().unwrap().display().to_string()
        );
    }

    /// **A pane's folder is where its program is now**: a program that changed
    /// directory after it started reads at its new folder, and a session that is
    /// over reads as nothing.
    #[test]
    fn a_pane_reads_at_the_folder_its_program_moved_to() {
        let server = TestServer::new("folder");
        let moved = server.dir.join("moved here");
        std::fs::create_dir_all(&moved).unwrap();
        let start = server.dir.display().to_string();
        let script = format!("cd '{}' && exec sleep 60", moved.display());
        let status = server
            .command(&[
                "new-session",
                "-d",
                "-s",
                "pp",
                "-c",
                &start,
                "sh",
                "-c",
                &script,
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let socket = server.socket();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let canonical = moved.canonicalize().unwrap().display().to_string();
        while pane_current_path(&socket, "pp").as_deref() != Some(canonical.as_str()) {
            assert!(
                std::time::Instant::now() < deadline,
                "the pane never read at {canonical}: {:?}",
                pane_current_path(&socket, "pp")
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(pane_current_path(&socket, "no-such-session"), None);
    }

    /// Whether `pid` is gone — killed and cleaned up — within a short bound.
    /// `kill(pid, 0)` answering `ESRCH` is the only accepted proof; sending
    /// `SIGKILL` is a request, not a fact.
    fn proven_gone(pid: i32) -> bool {
        for _ in 0..50 {
            let answer = unsafe { libc::kill(pid, 0) };
            if answer == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// What a pane's program read, once at least `len` bytes have arrived: the
    /// pane runs `stty raw -echo; exec dd bs=1 of=FILE`, so every byte the
    /// terminal hands over lands in the file unchanged.
    fn recorded(file: &std::path::Path, len: usize) -> Vec<u8> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got = std::fs::read(file).unwrap_or_default();
            if got.len() >= len || std::time::Instant::now() >= deadline {
                std::thread::sleep(Duration::from_millis(100));
                return std::fs::read(file).unwrap_or_default();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A pane on `server` that writes down every byte it is typed.
    fn recording_pane(server: &TestServer, name: &str) -> std::path::PathBuf {
        let file = server.dir.join(name);
        launch_on(
            server,
            name,
            &format!(
                "stty raw -echo; exec dd bs=1 of='{}' 2>/dev/null",
                file.display()
            ),
        );
        std::thread::sleep(Duration::from_millis(300));
        file
    }

    /// Every class of text tmux's parser has an opinion about reaches the
    /// program byte for byte — an argument ending in `;` (alone, after a
    /// space, doubled, escaped), a trailing `\`, quotes, a format, `~` and `$`,
    /// a leading `-` — and so do the bytes that are keys rather than text, a
    /// newline, a tab, Ctrl+C and Escape, unicode, and the 8 KiB one message may
    /// carry.
    #[test]
    fn text_is_typed_byte_for_byte_whatever_tmux_would_make_of_it() {
        let server = TestServer::new("typed");
        let long = "0123456789abcdef".repeat(512);
        let mut wrong = Vec::new();
        for (i, text) in [
            "end;",
            ";",
            "a ;",
            "x\\;",
            "two;;",
            "end\\",
            r#"it's "so""#,
            "#{pane_id} #S",
            "~/x $HOME",
            "-x --y",
            "one\ntwo\tthree\u{3}four\u{1b}five",
            "café ☕ 日本 🚀;",
            long.as_str(),
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("typed-{i}");
            let file = recording_pane(&server, &name);
            send_literal(&server.socket(), &name, text).unwrap();
            let got = recorded(&file, text.len());
            if got != text.as_bytes() {
                wrong.push(format!(
                    "{:?} arrived as {:?}",
                    &text[..text.len().min(40)],
                    String::from_utf8_lossy(&got[..got.len().min(40)])
                ));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("; "));
    }

    /// The text and its trailing `;` go in two invocations. When the first is
    /// refused nothing was typed; when the second is (tmux refuses 2,000 bytes
    /// in one invocation, measured on 3.7b), the text is on the screen and the
    /// failure says so.
    #[test]
    fn a_failure_after_the_text_landed_is_partial() {
        let server = TestServer::new("partial");
        let file = recording_pane(&server, "partial");
        let text = format!("end{}", ";".repeat(2000));
        let failed = send_literal(&server.socket(), "partial", &text).unwrap_err();
        assert!(
            matches!(&failed, TmuxError::Partial { typed: 3, of: 2003, source }
                if matches!(**source, TmuxError::Failed { .. })),
            "{failed}"
        );
        assert!(failed.outcome_is_indeterminate());
        assert_eq!(recorded(&file, 3), b"end");

        let refused = send_literal(&server.socket(), "no-such-pane", "end;").unwrap_err();
        assert!(matches!(refused, TmuxError::Failed { .. }), "{refused}");
        assert!(!refused.outcome_is_indeterminate());
    }

    /// Owns a private tmux server for one test — and owns it from *before*
    /// the spawn: the guard exists whatever the spawn does, teardown is
    /// bounded (an unbounded `kill-server` against a wedged server would
    /// recreate the hang inside the cleanup), and the exact server pid is
    /// the fallback when asking nicely fails. The socket directory is only
    /// removed after the server is dead — deleting a live server's socket
    /// strands it unaddressable.
    struct TestServer {
        dir: std::path::PathBuf,
        pid: std::cell::Cell<Option<i32>>,
    }

    impl TestServer {
        fn new(label: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("cc-{label}-{}", protocol::uid::new().unwrap()));
            std::fs::create_dir_all(&dir).unwrap();
            TestServer {
                dir,
                pid: std::cell::Cell::new(None),
            }
        }

        fn socket(&self) -> String {
            self.dir.join("sock").to_string_lossy().into_owned()
        }

        fn command(&self, args: &[&str]) -> Command {
            let mut command = Command::new(tmux_bin().expect("tmux present"));
            command.arg("-S").arg(self.socket());
            command.args(args);
            command
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let asked = run_deadlined(&mut self.command(&["kill-server"]), Duration::from_secs(2));
            // "no server" is as dead as a successful kill; anything else —
            // a timeout, a spawn failure, an unexpected refusal — means the
            // server may still be alive.
            let mut dead = match &asked {
                Ok(RunOutcome::Completed { status, stderr, .. }) => {
                    status.success() || String::from_utf8_lossy(stderr).contains("no server")
                }
                _ => false,
            };
            if !dead {
                if let Some(pid) = self.pid.get() {
                    // Asking nicely failed and the exact pid is known: end
                    // it directly. CONT first, so a stopped server takes
                    // the KILL — and death is then *verified*, because a
                    // sent signal is a request, not a fact.
                    unsafe {
                        libc::kill(pid, libc::SIGCONT);
                        libc::kill(pid, libc::SIGKILL);
                    }
                    dead = proven_gone(pid);
                }
            }
            // The directory goes only once the server cannot still be using
            // it: deleting a live server's socket strands it unaddressable,
            // which is worse than a leftover directory.
            if dead {
                let _ = std::fs::remove_dir_all(&self.dir);
            } else {
                eprintln!(
                    "test server at {} could not be proven dead; leaving its directory",
                    self.dir.display()
                );
            }
        }
    }

    /// The wedge that motivated the bound, reproduced: a tmux server that
    /// stops servicing clients while holding their connections. `SIGSTOP`
    /// is the stand-in — a wedged server sits in `select` never replying;
    /// a stopped one is frozen mid-loop; from the client's side both are a
    /// command that will never be answered. The bound turns that into an
    /// error in about a second — and the server, once resumed, must still
    /// be servable, because killing the *client* at the deadline must not
    /// damage the *server*.
    #[test]
    fn a_frozen_tmux_server_costs_the_deadline_not_forever() {
        if tmux_bin().is_err() {
            eprintln!("skipped: no tmux on this machine");
            return;
        }
        let dir = std::env::temp_dir().join(format!("cc-wedge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Guard first: from here on, however this test ends, the server dies.
        let server = TestServer {
            dir,
            pid: std::cell::Cell::new(None),
        };

        // Plain `new-session`, not `-P -F '#{pid}'`: with `-P`, tmux writes
        // the pid to the client's stdout and the server retains that piped
        // fd, so the bounded runner would correctly wait for an EOF that
        // never comes and time the *creation* out. The pid is read by a
        // separate `display` instead — and read unconditionally, so a server
        // that started even though `new-session` timed out is still owned.
        let started = run_deadlined(
            &mut server.command(&[
                "new-session",
                "-d",
                "-s",
                "wedge",
                "-x",
                "20",
                "-y",
                "5",
                "--",
                "/bin/sleep",
                "60",
            ]),
            Duration::from_secs(5),
        )
        .expect("tmux spawns");
        // The pid via a separate `display`, not `new-session -P`: with `-P`
        // tmux prints through the client stdout the server retains, so the
        // both-EOFs runner would time the creation itself out (verified on
        // tmux 3.7b through a pipe). Read BEFORE the success assertion, so
        // a server that started under a timed-out create is still owned by
        // the guard when the panic unwinds.
        if let Ok(RunOutcome::Completed { stdout, .. }) = run_deadlined(
            &mut server.command(&["display", "-p", "-t", "wedge", "#{pid}"]),
            Duration::from_secs(2),
        ) {
            if let Ok(pid) = String::from_utf8_lossy(&stdout).trim().parse::<i32>() {
                server.pid.set(Some(pid));
            }
        }
        assert!(
            matches!(started, RunOutcome::Completed { status, .. } if status.success()),
            "the test server starts: {started:?}"
        );
        let pid = server.pid.get().expect("a fresh server names its pid");

        // Freeze the server; the next client is accepted and never served.
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        let asked = std::time::Instant::now();
        let outcome = run_deadlined(
            &mut server.command(&["send-keys", "-t", "wedge", "-l", "--", "x"]),
            Duration::from_secs(1),
        )
        .expect("the client spawns even against a frozen server");
        assert!(
            matches!(outcome, RunOutcome::TimedOut { .. }),
            "a never-answered client is a timeout, not a wait: {outcome:?}"
        );
        assert!(
            asked.elapsed() < Duration::from_secs(4),
            "the deadline bounded it: {:?}",
            asked.elapsed()
        );

        // Thaw. The server must be undamaged by its client's death: the
        // next command answers, which is what lets a live session survive a
        // wedge instead of joining it.
        unsafe { libc::kill(pid, libc::SIGCONT) };
        let after = run_deadlined(
            &mut server.command(&["list-sessions", "-F", "#{session_name}"]),
            Duration::from_secs(2),
        )
        .expect("list runs");
        match after {
            RunOutcome::Completed { status, stdout, .. } => {
                assert!(status.success());
                assert!(String::from_utf8_lossy(&stdout).contains("wedge"));
            }
            RunOutcome::TimedOut { .. } => panic!("a resumed server answers"),
        }
    }

    #[test]
    fn tmux_is_locatable_on_this_machine() {
        // Skipped only where tmux is genuinely absent — a CI runner. Every
        // machine that can actually run a session still asserts this.
        if tmux_bin().is_err() {
            eprintln!("skipped: no `tmux` on this machine — nothing to locate");
            return;
        }
        let bin = tmux_bin().expect("tmux must be installed for CodeConnect to work");
        assert!(bin.is_file());
    }
}
