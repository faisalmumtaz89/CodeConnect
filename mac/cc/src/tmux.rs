//! The private tmux server (`tmux -L codeconnect`).
//!
//! tmux is the session host, not a scraping surface. `capture-pane` is used for
//! two things only — a text snapshot to mirror, and a positive presence check
//! before injecting keys. Nothing here ever infers agent *semantics* from
//! terminal bytes; that is the failure mode that killed agentapi and Omnara.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// launchd hands a process a minimal environment with no shell PATH, so the
/// binary is located explicitly rather than through `which`.
const TMUX_CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/tmux",
    "/usr/local/bin/tmux",
    "/usr/bin/tmux",
    "/opt/local/bin/tmux",
];

pub fn tmux_bin() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("CODECONNECT_TMUX") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
    }
    for candidate in TMUX_CANDIDATES {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    if let Some(found) = search_path("tmux") {
        return Ok(found);
    }
    bail!("tmux not found; install it or set CODECONNECT_TMUX")
}

pub fn search_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn base() -> Result<Command> {
    let mut command = Command::new(tmux_bin()?);
    command.arg("-L").arg(protocol::TMUX_SOCKET_NAME);
    Ok(command)
}

/// What a tmux invocation actually did — status *and* stderr, both kept.
///
/// The status alone is not enough. `has-session` exits non-zero for "there is
/// no such session" and for "the server could not be reached", and those are
/// opposite facts: one means the agent is gone, the other means we could not
/// look. Only tmux's own message separates them.
struct TmuxOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn run_raw(args: &[&str]) -> Result<TmuxOutput> {
    let output = base()?
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running tmux")?;
    Ok(TmuxOutput {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Run a tmux command and capture stdout. `Ok(None)` when tmux exits non-zero.
///
/// Correct only for subcommands where non-zero genuinely means "nothing to
/// report" — listing sessions when no server is running. Anything that has to
/// distinguish *absent* from *unknown* must use [`session_presence`] instead.
fn run(args: &[&str]) -> Result<Option<String>> {
    let output = run_raw(args)?;
    if !output.ok {
        return Ok(None);
    }
    Ok(Some(output.stdout))
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

/// Whether a session exists, or whether we could not tell.
///
/// Three states, because the two-state version was a lie. `has-session` exiting
/// non-zero used to mean "gone" unconditionally, and the supervisor turned that
/// straight into a `SessionEnd` event — so a tmux binary that could not be
/// reached, a socket whose permissions had changed, a machine that had run out
/// of file descriptors, or a server briefly restarting all produced a *reported
/// agent exit*. The event log's rule is that the daemon never claims what it
/// does not know, and "tmux returned 1" is not knowledge of an exit.
#[derive(Debug, PartialEq, Eq)]
pub enum SessionPresence {
    Present,
    /// tmux said, in its own words, that there is no such session.
    Gone,
    /// We could not establish either. Never treated as an exit.
    Unknown(String),
}

pub fn session_presence(name: &str) -> SessionPresence {
    match run_raw(&["has-session", "-t", &target_session(name)]) {
        Ok(output) if output.ok => SessionPresence::Present,
        Ok(output) => classify_absence(&output.stderr),
        // tmux could not even be run: the binary moved, or the process is out
        // of descriptors. Certainly not evidence that an agent exited.
        Err(err) => SessionPresence::Unknown(format!("{err:#}")),
    }
}

/// Read tmux's own words for a definite "no such session".
///
/// Matched against the messages tmux emits rather than against the exit code,
/// because the exit code is 1 for every one of them. The list is tmux's actual
/// wording across the versions this ships against; anything unrecognised is
/// `Unknown`, which is the fail-toward-not-claiming direction — an unfamiliar
/// message must not be promoted into a reported exit.
fn classify_absence(stderr: &str) -> SessionPresence {
    let message = stderr.trim().to_ascii_lowercase();
    const GONE: &[&str] = &[
        // `has-session -t cc-1` with the server up and no such session.
        "can't find session",
        "session not found",
        "no such session",
        // The server itself is not running, so no session exists on it. tmux
        // words this several ways depending on version and platform.
        "no server running",
        "failed to connect to server: connection refused",
        "no such file or directory",
    ];
    if GONE.iter().any(|needle| message.contains(needle)) {
        return SessionPresence::Gone;
    }
    if message.is_empty() {
        return SessionPresence::Unknown("tmux failed without saying why".into());
    }
    SessionPresence::Unknown(stderr.trim().to_string())
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

/// Create a detached session running `argv`, with `env` exported into it.
///
/// `argv` is passed as separate arguments through `sh -c '…' "$0" "$@"` so no
/// user argument is ever interpolated into a shell string. The wrapper exists
/// solely to `unset CLAUDE_CODE_CHILD_SESSION`: the tmux *server* may have
/// inherited it at start-up, and a session that inherits it writes no transcript
/// at all (measured) — silently, which is the worst kind of failure.
pub fn new_session(
    name: &str,
    cwd: &str,
    env: &[(String, String)],
    argv: &[String],
    size: Option<(u16, u16)>,
    status_bar: bool,
) -> Result<()> {
    let mut command = base()?;
    command.args(["new-session", "-d", "-s", name, "-c", cwd]);
    if let Some((cols, rows)) = size {
        command.args(["-x", &cols.to_string(), "-y", &rows.to_string()]);
    }
    for (key, value) in env {
        command.arg("-e").arg(format!("{key}={value}"));
    }
    command.arg("--");
    command.arg("/bin/sh");
    command.arg("-c");
    command.arg(r#"unset CLAUDE_CODE_CHILD_SESSION; exec "$0" "$@""#);
    for arg in argv {
        command.arg(arg);
    }

    let status = command
        .stdin(Stdio::null())
        .status()
        .context("starting tmux session")?;
    if !status.success() {
        bail!("tmux new-session failed with {status}");
    }

    // Session-scoped so the user's own tmux config is untouched. Without this
    // the tab shows a status line that plain `claude` never has.
    //
    // `set-option -t` takes a *pane* target on tmux 3.x, so this needs the
    // colon form. Reported rather than swallowed: a silent failure here is a
    // visible difference from plain `claude`, which is the one thing `cc claude`
    // promises not to be.
    if !status_bar && run(&["set-option", "-t", &target_pane(name), "status", "off"])?.is_none() {
        eprintln!("codeconnect: could not hide the tmux status bar for {name}");
    }

    // Claude Code tracks terminal focus and prints a warning inside the session
    // when tmux swallows the events. Enabling it per-session removes the last
    // visible difference from plain `claude` without touching ~/.tmux.conf.
    let _ = run(&["set-option", "-t", &target_pane(name), "focus-events", "on"]);
    Ok(())
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

/// Type text literally. `-l` stops tmux from interpreting the text as key names,
/// which matters the moment a user sends anything containing "C-c" or "Enter".
pub fn send_literal(name: &str, text: &str) -> Result<()> {
    let status = base()?
        .args(["send-keys", "-t", &target_pane(name), "-l", "--", text])
        .stdin(Stdio::null())
        .status()
        .context("tmux send-keys -l")?;
    if !status.success() {
        bail!("tmux send-keys failed with {status}");
    }
    Ok(())
}

/// Send a named key (`Enter`, `Escape`, ...).
pub fn send_key(name: &str, key: &str) -> Result<()> {
    let status = base()?
        .args(["send-keys", "-t", &target_pane(name), key])
        .stdin(Stdio::null())
        .status()
        .context("tmux send-keys")?;
    if !status.success() {
        bail!("tmux send-keys {key} failed with {status}");
    }
    Ok(())
}

/// Replace this process with an attached tmux client, so the terminal tab hosts
/// the session natively and closing the tab leaves the session running.
pub fn exec_attach(name: &str) -> Result<std::convert::Infallible> {
    use std::os::unix::process::CommandExt;
    let error = base()?
        .args(["attach-session", "-t", &target_session(name)])
        .exec();
    Err(error).context("exec tmux attach-session")
}

/// Exact session target. tmux prefix-matches names unless they are anchored
/// with `=`; without this, `cc-1` would happily attach to `cc-12`.
fn target_session(name: &str) -> String {
    format!("={name}")
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

    #[test]
    fn tmux_saying_there_is_no_such_session_is_the_only_thing_read_as_gone() {
        // The defect this replaces: *any* non-zero exit from `has-session` was
        // read as absence, and the supervisor turned absence into a durable
        // `SessionEnd`. tmux exits 1 for "no such session" and also for every
        // way it can fail to look — so a moved binary, an unreachable socket, a
        // process out of descriptors, or a server mid-restart all produced a
        // reported agent exit for a session that was still running.
        for gone in [
            "can't find session: cc-1",
            "no server running on /private/tmp/tmux-501/codeconnect",
            "session not found: cc-1",
            "no such session",
            "failed to connect to server: Connection refused",
            "error connecting to /tmp/tmux-501/codeconnect (No such file or directory)",
        ] {
            assert_eq!(
                classify_absence(gone),
                SessionPresence::Gone,
                "{gone:?} is tmux saying the session is not there"
            );
        }

        for unknown in [
            "permission denied",
            "lost server",
            "server exited unexpectedly",
            "open terminal failed: not a terminal",
            "too many open files",
        ] {
            assert!(
                matches!(classify_absence(unknown), SessionPresence::Unknown(_)),
                "{unknown:?} must never be promoted into a reported exit"
            );
        }
    }

    #[test]
    fn a_silent_failure_is_unknown_rather_than_gone() {
        // The worst case for the old code: tmux exits non-zero and says
        // nothing. There is no evidence of an exit here at all.
        assert!(matches!(classify_absence(""), SessionPresence::Unknown(_)));
        assert!(matches!(
            classify_absence("   \n  "),
            SessionPresence::Unknown(_)
        ));
    }

    #[test]
    fn an_indeterminate_answer_is_an_error_not_a_false() {
        // `has_session` returns a bool, and a bool has no room for "we could
        // not tell". Callers get an error instead of a confident `false`.
        let unknown = SessionPresence::Unknown("permission denied".into());
        assert!(matches!(unknown, SessionPresence::Unknown(ref why) if why.contains("denied")));
    }

    #[test]
    fn targets_are_anchored_and_pane_targets_carry_the_colon() {
        // Both halves matter: without `=`, `cc-1` prefix-matches `cc-12`;
        // without the trailing `:`, a pane target is not found at all.
        assert_eq!(target_session("cc-1"), "=cc-1");
        assert_eq!(target_pane("cc-1"), "=cc-1:");
    }

    #[test]
    fn live_tmux_accepts_both_target_forms() {
        // Guards against a tmux release changing the parse. Uses a throwaway
        // session on the private server so it cannot disturb a real one.
        let name = format!("cctest-{}", std::process::id());
        let created = new_session(
            &name,
            "/tmp",
            &[],
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "sleep 30".to_string(),
            ],
            Some((80, 24)),
            false,
        );
        if created.is_err() {
            return; // no tmux in this environment; the unit test above still holds
        }
        assert!(has_session(&name).unwrap(), "session target form broke");
        assert!(capture_pane(&name, 5).is_ok(), "pane target form broke");
        let _ = run(&["kill-session", "-t", &target_session(&name)]);
    }

    #[test]
    fn tmux_is_locatable_on_this_machine() {
        let bin = tmux_bin().expect("tmux must be installed for CodeConnect to work");
        assert!(bin.is_file());
    }
}
