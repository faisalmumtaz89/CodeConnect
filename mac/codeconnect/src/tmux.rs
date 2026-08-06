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

/// The private server's config, written before every spawn and handed to tmux
/// with `-f` so it is read **at server start — before the first pane exists**.
///
/// That ordering is the whole point, and it was measured, not assumed:
/// `history-limit` is fixed into a pane at creation, `set-option -g` cannot
/// start a server, and a `set-option` run after `new-session` leaves the first
/// pane — the only pane — on tmux's stock 2,000 lines. With Claude rendering
/// inline, the pane history *is* the conversation, and 2,000 lines is where
/// "scroll up to see what happened" quietly stopped working.
///
/// `mouse on` is the scroll fix itself. Without it tmux never advertises mouse
/// tracking, the outer terminal falls back to translating the wheel into arrow
/// keys, and Claude receives arrows it neither wanted nor can use — the
/// "scroll wheel is sending arrow keys" warning verbatim. With it, the wheel
/// enters tmux copy-mode over the inline transcript, which is exactly the
/// native-terminal scrollback plain `claude` gets for free. The remaining
/// three lines are Anthropic's own documented tmux configuration for Claude
/// Code (code.claude.com/docs/en/terminal-config): passthrough and extended
/// keys are what keep bindings like shift+enter working under a host.
fn render_server_conf(history_limit: u32) -> String {
    format!(
        "# Written by codeconnect before each spawn; edits here are overwritten.\n\
         # Change history via `tmux_history_limit` in config.json instead.\n\
         set -g mouse on\n\
         set -g history-limit {history_limit}\n\
         set -g allow-passthrough on\n\
         set -s extended-keys on\n\
         set -as terminal-features 'xterm*:extkeys'\n"
    )
}

fn write_server_conf(history_limit: u32) -> Result<PathBuf> {
    let path = protocol::root_dir().join("tmux.conf");
    let body = render_server_conf(history_limit);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {parent:?}"))?;
    }
    std::fs::write(&path, body).with_context(|| format!("writing {path:?}"))?;
    Ok(path)
}

/// Bring an **already-running** server up to the config's options.
///
/// The conf file above only speaks at server start, so a server that predates
/// this build — or this config value — never hears it. `mouse` is a session
/// option applied globally and takes effect immediately, upgrading even the
/// session the user is attached to right now. `history-limit` genuinely cannot
/// reach panes that already exist — tmux fixes capacity at pane creation — so
/// it is set for the panes that come next and the shortfall is accepted rather
/// than papered over.
///
/// Idempotent, and quiet on failure by design: if there is no server yet, the
/// conf file is about to say all of this better.
fn ensure_server_options(history_limit: u32) {
    let limit = history_limit.to_string();
    for args in [
        ["set-option", "-g", "mouse", "on"],
        ["set-option", "-g", "history-limit", limit.as_str()],
        ["set-option", "-s", "extended-keys", "on"],
        ["set-option", "-g", "allow-passthrough", "on"],
    ] {
        let _ = run(&args);
    }
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
    history_limit: u32,
) -> Result<()> {
    // Both halves, deliberately: `-f` speaks when this command *starts* the
    // server (options in place before the first pane), and the runtime pass
    // upgrades a server that was already running from an older build.
    let conf = write_server_conf(history_limit)?;
    ensure_server_options(history_limit);
    let mut command = base()?;
    command.arg("-f").arg(&conf);
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

/// Whether the pane's cursor is visible — tmux's own record of the terminal's
/// DECTCEM state, which is the structural answer to "who has the keyboard".
///
/// **Measured, and better than any string.** Claude's composer keeps a visible
/// cursor while idle, while a turn streams, and after it ends (327 consecutive
/// samples through a live turn, not one of them hidden). Every view it opens
/// hides it — including the one that leaves the composer *drawn* underneath and
/// takes no keys, where the composer-presence needle matches and lies. Reading
/// the pane's text for a view's dismissal hint would work too, until an agent
/// wrote that hint into its own output; the cursor cannot be spelled.
pub fn cursor_is_visible(name: &str) -> Result<bool> {
    let out = run(&["display", "-p", "-t", &target_pane(name), "#{cursor_flag}"])?
        .ok_or_else(|| anyhow::anyhow!("tmux display cursor_flag failed for {name}"))?;
    Ok(out.trim() == "1")
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
    // A session that exists means a server that is running, and it may predate
    // the scroll fix: bring its options up before the user's client connects,
    // so the wheel works in the session they are about to look at.
    ensure_server_options(protocol::config::Config::load().tmux_history_limit);
    let error = base()?
        .args(["attach-session", "-t", &target_session(name)])
        .exec();
    Err(error).context("exec tmux attach-session")
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

    /// The conf is what makes the first pane correct, so its content is pinned:
    /// lose `mouse on` and scrolling regresses to arrow-key noise; lose
    /// `history-limit` and the first pane silently reverts to tmux's 2,000.
    #[test]
    fn the_server_conf_carries_the_scroll_contract() {
        // The rendered string, not the file: the file's path is shared with the
        // live-tmux test running in parallel, and reading it back raced.
        let body = render_server_conf(12_345);
        for line in [
            "set -g mouse on",
            "set -g history-limit 12345",
            "set -g allow-passthrough on",
            "set -s extended-keys on",
            "set -as terminal-features 'xterm*:extkeys'",
        ] {
            assert!(body.contains(line), "conf lost {line:?}:\n{body}");
        }
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
            50_000,
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
