//! `cc` — the CodeConnect shim.
//!
//! `cc claude [args…]` hosts a real `claude` inside the private tmux server and
//! then **execs** the tmux client, so the terminal tab is showing the session
//! itself rather than a wrapper around it. Nothing is proxied, no PTY is
//! allocated, and every keystroke goes straight to Claude's TTY.
//!
//! What the shim adds happens entirely at spawn time: a generated `--settings`
//! that installs the hooks, the environment fixes that keep transcripts alive,
//! and a detached supervisor that connects out to `ccd`.

mod daemon;
mod launchd;
mod pair;
mod settings;
mod supervisor;
mod tmux;

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use protocol::config::Config;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = args
        .split_first()
        .map(|(head, tail)| (head.as_str(), tail))
        .unwrap_or(("help", &[]));

    match command {
        "claude" => start_claude(rest),
        "attach" => attach(rest),
        "ls" | "list" => list(),
        "token" => token(),
        "pair" => pair::pair(rest),
        "devices" => pair::devices(),
        "revoke" => pair::revoke(rest, false),
        "ssh-revoke" => pair::revoke(rest, true),
        "daemon" => launchd::command(rest),
        // Hidden: spawned by `cc claude`, never typed by a human.
        "supervise" => supervise(rest),
        "--version" | "version" => {
            println!("cc {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            usage();
            Ok(())
        }
    }
}

fn usage() {
    eprintln!(
        "\
cc — CodeConnect shim

  cc claude [args…]      run claude in the private tmux server, attached here
  cc attach <name>       re-attach a session (e.g. after closing the tab)
  cc ls                  list sessions

  cc daemon install      install and start the ccd LaunchAgent
  cc daemon status       plist, launchd job and live daemon
  cc daemon restart      restart the managed daemon
  cc daemon uninstall    stop it and remove the LaunchAgent

  cc pair [--ssh]        show a QR code that pairs a phone (single use, 5 min)
  cc devices             list paired devices
  cc revoke <device>     revoke a device's token and its SSH key
  cc ssh-revoke <device> remove only that device's SSH key
  cc token               print the static fallback token

`cc pair --ssh` also lets that one pairing install the app's ed25519 public
key into ~/.ssh/authorized_keys. Without the flag an offered key is refused.
"
    );
}

fn start_claude(passthrough: &[String]) -> Result<()> {
    let config = Config::load();
    let claude_bin = resolve_claude_bin(&config)?;
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let cwd = cwd.to_string_lossy().to_string();

    let session_id = tmux::next_session_name()?;
    // Minted here, once, before anything else knows the session exists. The
    // tmux name is reused as soon as this session exits; this is not, and it is
    // what the event log, the tail cursor and the answers ledger are keyed by.
    let session_uid = protocol::uid::new().context("minting a session uid")?;
    let plan = settings::write_for_session(&session_id, &session_uid, &config)?;

    let mut argv = vec![
        claude_bin.to_string_lossy().to_string(),
        "--settings".to_string(),
        plan.path.to_string_lossy().to_string(),
    ];
    argv.extend(passthrough.iter().cloned());

    // Spawn hygiene, measured rather than assumed:
    //   * FORCE_SESSION_PERSIST — without it an inherited CHILD_SESSION makes
    //     Claude write no transcript at all, silently.
    //   * CODECONNECT_SESSION / _UID — a fallback identity for hooks; the
    //     generated settings also pass both explicitly.
    // CLAUDE_CODE_CHILD_SESSION is unset inside the session by the `sh -c`
    // wrapper in tmux::new_session, because the tmux *server* may have inherited
    // it before we ever ran.
    let env = vec![
        (
            "CLAUDE_CODE_FORCE_SESSION_PERSIST".to_string(),
            "1".to_string(),
        ),
        (protocol::ENV_SESSION.to_string(), session_id.clone()),
        (protocol::ENV_SESSION_UID.to_string(), session_uid.clone()),
    ];

    tmux::new_session(
        &session_id,
        &cwd,
        &env,
        &argv,
        tmux::terminal_size(),
        config.tmux_status,
    )
    .with_context(|| format!("creating tmux session {session_id}"))?;

    spawn_supervisor(&session_id, &session_uid, &cwd, &claude_bin)?;

    eprintln!("codeconnect: session {session_id} (detach with ctrl-b d, reattach with `cc attach {session_id}`)");
    tmux::exec_attach(&session_id)?;
    unreachable!("exec replaces the process")
}

/// Launch the supervisor so it outlives this process *and* the terminal tab.
///
/// `process_group(0)` puts it in its own process group, so the SIGHUP/SIGINT
/// that reach the tab's foreground group never reach it. When `cc` execs into
/// the tmux client and that client later dies, the supervisor is reparented to
/// launchd and keeps running. This is the standard daemonisation shape, and it
/// is reachable from safe Rust.
fn spawn_supervisor(
    session_id: &str,
    session_uid: &str,
    cwd: &str,
    claude_bin: &std::path::Path,
) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let current = std::env::current_exe().context("locating the cc binary")?;
    // Named by uid, not by tmux name: the name is reused, and two runs sharing
    // one supervisor log makes the file useless exactly when it is needed.
    let log_path = protocol::logs_dir().join(format!("supervisor-{session_id}-{session_uid}.out"));
    std::fs::create_dir_all(protocol::logs_dir())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;

    let mut command = Command::new(current);
    command
        .arg("supervise")
        .arg("--session")
        .arg(session_id)
        .arg("--session-uid")
        .arg(session_uid)
        .arg("--tmux-session")
        .arg(session_id)
        .arg("--cwd")
        .arg(cwd)
        .arg("--claude-bin")
        .arg(claude_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .process_group(0);

    command
        .spawn()
        .with_context(|| format!("spawning the supervisor for {session_id}"))?;
    Ok(())
}

fn supervise(args: &[String]) -> Result<()> {
    let mut session_id = None;
    let mut session_uid = None;
    let mut tmux_session = None;
    let mut cwd = None;
    let mut claude_bin = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--session" => session_id = it.next().cloned(),
            "--session-uid" => session_uid = it.next().cloned(),
            "--tmux-session" => tmux_session = it.next().cloned(),
            "--cwd" => cwd = it.next().cloned(),
            "--claude-bin" => claude_bin = it.next().cloned(),
            _ => {}
        }
    }
    let session_id = session_id.context("--session is required")?;
    let tmux_session = tmux_session.unwrap_or_else(|| session_id.clone());
    let cwd = cwd.unwrap_or_else(|| "/".to_string());

    supervisor::run(
        supervisor::SupervisorArgs {
            session_id,
            // A malformed value is dropped rather than forwarded: the daemon
            // resolves the name instead, which is right, whereas a bad uid
            // would mint a second identity for a session that has one.
            session_uid: session_uid.filter(|uid| protocol::uid::is_well_formed(uid)),
            tmux_session,
            cwd,
            claude_bin,
        },
        &Config::load(),
    )
}

fn attach(args: &[String]) -> Result<()> {
    let Some(name) = args.first() else {
        let sessions = tmux::list_sessions()?;
        match sessions.len() {
            0 => bail!("no CodeConnect sessions are running"),
            // With exactly one session, asking which one would be theatre.
            1 => return attach(&[sessions[0].clone()]),
            _ => bail!("which session? {}", sessions.join(", ")),
        }
    };
    if !tmux::has_session(name)? {
        bail!("no session named {name}; `cc ls` shows what is running");
    }
    tmux::exec_attach(name)?;
    unreachable!("exec replaces the process")
}

fn list() -> Result<()> {
    let sessions = tmux::list_sessions()?;
    if sessions.is_empty() {
        println!("no CodeConnect sessions");
        return Ok(());
    }
    // The daemon knows more (identity, link state, blocked approvals) but must
    // never be required: `cc ls` has to work when ccd is down.
    let known = daemon_sessions().unwrap_or_default();
    println!("{:<10} {:<28} {:<10} CWD", "SESSION", "UID", "LINK");
    for name in sessions {
        // The *live* run under this name. A dead one with the same name may
        // still be listed by the daemon; tmux is what says which is current.
        let current = known
            .iter()
            .filter(|s| s.session_id == name)
            .max_by(|a, b| a.session_uid.cmp(&b.session_uid));
        let (uid, link, cwd) = current
            .map(|s| {
                (
                    s.session_uid.clone(),
                    format!("{:?}", s.link).to_lowercase(),
                    s.cwd.clone(),
                )
            })
            .unwrap_or_else(|| ("—".to_string(), "unknown".to_string(), String::new()));
        println!("{name:<10} {uid:<28} {link:<10} {cwd}");
    }
    Ok(())
}

fn daemon_sessions() -> Result<Vec<protocol::event::SessionSummary>> {
    match daemon::request(&protocol::ipc::ClientFrame::ListSessions)? {
        protocol::ipc::DaemonFrame::Sessions { sessions } => Ok(sessions),
        other => bail!("unexpected reply: {other:?}"),
    }
}

fn token() -> Result<()> {
    let path = protocol::token_path();
    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}; is ccd running?", path.display()))?;
    println!("{}", token.trim());
    Ok(())
}

/// Find the real `claude`, never `cc` itself.
///
/// launchd-safe: an explicit candidate list first, `PATH` only as a fallback,
/// and a guard against resolving to this binary (which a shell alias like
/// `alias claude=cc claude` would otherwise cause, producing an infinite spawn
/// loop that is very confusing from the inside).
fn resolve_claude_bin(config: &Config) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(configured) = &config.claude_bin {
        candidates.push(PathBuf::from(configured));
    }
    if let Some(env) = std::env::var_os("CODECONNECT_CLAUDE_BIN") {
        candidates.push(PathBuf::from(env));
    }
    let home = protocol::home_dir();
    candidates.push(home.join(".local/bin/claude"));
    candidates.push(home.join(".claude/local/claude"));
    candidates.push(PathBuf::from("/opt/homebrew/bin/claude"));
    candidates.push(PathBuf::from("/usr/local/bin/claude"));
    if let Some(found) = tmux::search_path("claude") {
        candidates.push(found);
    }

    let current = std::env::current_exe().ok();
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        if is_same_file(&candidate, current.as_deref()) {
            continue;
        }
        return Ok(candidate);
    }
    bail!("could not find the claude binary; set claude_bin in ~/.codeconnect/config.json")
}

fn is_same_file(candidate: &std::path::Path, current: Option<&std::path::Path>) -> bool {
    let Some(current) = current else { return false };
    match (candidate.canonicalize(), current.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_real_claude_on_this_machine() {
        let path = resolve_claude_bin(&Config::default()).expect("claude must be installed");
        assert!(path.is_file());
        assert!(
            !path.ends_with("cc"),
            "must never resolve to the shim itself: {}",
            path.display()
        );
    }

    #[test]
    fn config_override_wins() {
        let config = Config {
            claude_bin: Some("/bin/echo".into()),
            ..Config::default()
        };
        assert_eq!(
            resolve_claude_bin(&config).unwrap(),
            PathBuf::from("/bin/echo")
        );
    }

    #[test]
    fn a_missing_override_falls_through_rather_than_failing() {
        let config = Config {
            claude_bin: Some("/nonexistent/claude".into()),
            ..Config::default()
        };
        let path = resolve_claude_bin(&config).expect("must fall back");
        assert!(path.is_file());
    }

    #[test]
    fn same_file_detection_handles_symlinks_and_absence() {
        assert!(is_same_file(
            std::path::Path::new("/bin/sh"),
            Some(std::path::Path::new("/bin/sh"))
        ));
        assert!(!is_same_file(std::path::Path::new("/bin/sh"), None));
        assert!(!is_same_file(
            std::path::Path::new("/bin/sh"),
            Some(std::path::Path::new("/bin/echo"))
        ));
    }
}
