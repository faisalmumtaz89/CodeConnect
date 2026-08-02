//! The private tmux server (`tmux -L codeconnect`).
//!
//! tmux is the session host, not a scraping surface. `capture-pane` is used for
//! two things only — a text snapshot to mirror, and a positive presence check
//! before injecting keys. Nothing here ever infers agent *semantics* from
//! terminal bytes; that is the failure mode that killed agentapi and Omnara.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use protocol::tmux::target_session;
pub use protocol::tmux::{search_path, SessionPresence};

pub fn tmux_bin() -> Result<PathBuf> {
    protocol::tmux::tmux_bin()
        .ok_or_else(|| anyhow::anyhow!("tmux not found; install it or set CODECONNECT_TMUX"))
}

fn base() -> Result<Command> {
    let mut command = Command::new(tmux_bin()?);
    command.arg("-L").arg(protocol::TMUX_SOCKET_NAME);
    Ok(command)
}

/// Run a tmux command and capture stdout. `Ok(None)` when tmux exits non-zero.
///
/// Correct only for subcommands where non-zero genuinely means "nothing to
/// report" — listing sessions when no server is running. Anything that has to
/// distinguish *absent* from *unknown* must use [`session_presence`] instead,
/// which is why this collapses the two and that one does not.
fn run(args: &[&str]) -> Result<Option<String>> {
    let output = base()?
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running tmux")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
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
    // visible difference from plain `claude`, which is the one thing `codeconnect claude`
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
    keep_the_hosts_screen(name);
    let error = base()?
        .args(["attach-session", "-t", &target_session(name)])
        .exec();
    Err(error).context("exec tmux attach-session")
}

/// Stop tmux taking over the terminal's alternate screen when it attaches.
///
/// **The difference this removes.** A tmux *client* sends `smcup` and clears the
/// host terminal on attach, and `rmcup` on detach — tmux's own `tty_start_tty` and
/// `tty_stop_tty`, not anything the session does. So `codeconnect claude` blanked
/// the tab, ran full-window, and took the terminal's scrollback with it, while
/// plain `claude` scrolls in place and leaves history behind. Measured: the pane's
/// `alternate_on` is 0, so Claude Code is not doing this; the client is.
///
/// `smcup@:rmcup@` removes those two capabilities, which is the form tmux's own
/// maintainers recommend for exactly this. It is a **server** option and this is
/// our private `-L codeconnect` server, so the user's own tmux is untouched.
///
/// Written to index 1 rather than appended: `-a` would add a duplicate on every
/// attach, and slot 0 already holds a tmux default (`linux*:AX@`) that must
/// survive. Assigning one stable slot is idempotent across any number of attaches.
///
/// **What this does not claim.** tmux still owns and redraws the viewport while
/// attached, and the pane's history is tmux's rather than the terminal's. What
/// changes is that the host's screen and scrollback are no longer swapped out
/// from under the reader.
fn keep_the_hosts_screen(name: &str) {
    // Best-effort: a session that renders in the alternate screen is a cosmetic
    // regression, and refusing to start over it would be a much worse one.
    if run(&[
        "set-option",
        "-s",
        "terminal-overrides[1]",
        "*:smcup@:rmcup@",
    ])
    .ok()
    .flatten()
    .is_none()
    {
        eprintln!("codeconnect: could not keep the terminal's screen for {name}; attaching anyway");
    }
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

    #[test]
    fn pane_targets_carry_the_colon_as_well_as_the_anchor() {
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
