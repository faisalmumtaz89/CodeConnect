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
use protocol::tmux::target_session;
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

/// Server and session startup: the first client may have to fork the server
/// and read its config before the command even begins.
const STARTUP_DEADLINE: Duration = Duration::from_secs(5);

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
        }
    }
}

impl std::error::Error for TmuxError {}

impl TmuxError {
    /// Whether this failure leaves open the possibility that the command
    /// *acted* before failing. A spawn failure or a refusal proves it did
    /// not; a kill at the deadline proves nothing either way.
    pub fn outcome_is_indeterminate(&self) -> bool {
        matches!(self, TmuxError::TimedOut { .. })
    }
}

pub fn tmux_bin() -> Result<PathBuf> {
    protocol::tmux::tmux_bin()
        .ok_or_else(|| anyhow::anyhow!("tmux not found; install it or set CODECONNECT_TMUX"))
}

fn base() -> Result<Command> {
    let mut command = Command::new(tmux_bin()?);
    command.arg("-L").arg(protocol::TMUX_SOCKET_NAME);
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
/// which is why this collapses the two and that one does not. A spawn failure
/// or a deadline kill is neither: those stay errors.
fn run(args: &[&str]) -> Result<Option<String>> {
    let what = args.first().copied().unwrap_or("tmux");
    match run_tmux(base()?.args(args), OPERATION_DEADLINE, what) {
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

    // The startup deadline, not the operational one: this command may be the
    // one that forks the server and reads its config.
    run_tmux(&mut command, STARTUP_DEADLINE, "new-session")
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("starting tmux session")?;

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
///
/// The typed error matters here more than anywhere: a caller that asked for
/// keys to be typed has to know whether a failure proves nothing landed
/// ([`TmuxError::Spawn`], [`TmuxError::Failed`]) or proves nothing at all
/// ([`TmuxError::TimedOut`] — the server may have acted before stalling).
pub fn send_literal(name: &str, text: &str) -> Result<(), TmuxError> {
    run_tmux(
        base()
            .map_err(|err| TmuxError::Spawn {
                what: "send-keys".into(),
                source: std::io::Error::other(err.to_string()),
            })?
            .args(["send-keys", "-t", &target_pane(name), "-l", "--", text]),
        OPERATION_DEADLINE,
        "send-keys",
    )
    .map(|_| ())
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
