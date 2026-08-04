//! `codeconnect` — the CodeConnect shim.
//!
//! `codeconnect claude [args…]` hosts a real `claude` inside the private tmux server and
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
mod sessions;
mod settings;
mod supervisor;
mod tmux;
mod update_check;

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
        "sessions" => sessions::command(rest),
        "token" => token(),
        "pair" => pair::pair(rest),
        "devices" => pair::devices(rest),
        "revoke" => pair::revoke(rest, false),
        "ssh-revoke" => pair::revoke(rest, true),
        "daemon" => launchd::command(rest),
        // Hidden: spawned by `codeconnect claude`, never typed by a human.
        "supervise" => supervise(rest),
        // Hidden: the detached update checker `codeconnect claude` spawns.
        // Not in --help on purpose — it is machinery, not a command.
        "__update-check" => update_check::run_checker(),
        "--version" | "version" => {
            println!("codeconnect {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "--help" | "-h" => {
            usage();
            Ok(())
        }
        // Anything else is a mistake, and a mistake exits non-zero.
        //
        // This binary was called `cc` until the collision was measured: `cc` is
        // the traditional name of the C compiler, the install prepends its
        // directory to PATH, and so `cc file.c -o out` reached *this* program,
        // printed usage, and **exited 0** — which a Makefile or a configure
        // script reads as a successful compile of nothing. The rename removed
        // the collision; exiting non-zero stays, because a command nobody
        // recognised is not a command that succeeded.
        other => {
            usage();
            anyhow::bail!("unknown command {other:?}");
        }
    }
}

fn usage() {
    eprintln!(
        "\
cc — CodeConnect shim

  codeconnect claude [args…]      run claude in the private tmux server, attached here
  codeconnect attach <name>       re-attach a session (e.g. after closing the tab)
  codeconnect ls                  list what tmux is running (works with ccd down)
  codeconnect sessions            list what the event log knows, with lifecycle
  codeconnect sessions prune      remove ended sessions and their events (--dry-run first)

  codeconnect daemon install      install and start the ccd LaunchAgent
  codeconnect daemon status       plist, launchd job and live daemon
  codeconnect daemon restart      restart the managed daemon
  codeconnect daemon uninstall    stop it and remove the LaunchAgent

  codeconnect pair [--ssh]        show a QR code that pairs a phone (single use, 5 min)
  codeconnect devices             list paired devices
  codeconnect revoke <device>     revoke a device's token and its SSH key
  codeconnect ssh-revoke <device> remove only that device's SSH key
  codeconnect token               print the static fallback token

`codeconnect pair --ssh` also lets that one pairing install the app's ed25519 public
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
        config.tmux_history_limit,
    )
    .with_context(|| format!("creating tmux session {session_id}"))?;

    spawn_supervisor(&session_id, &session_uid, &cwd, &claude_bin)?;

    // After the session and supervisor exist, before the alternate screen:
    // the hold below delays only the *display*, never the session it is
    // promising is unaffected — Claude is already running while this is
    // read. One hold however many notes apply; reachability first, because
    // a phone that cannot connect at all outranks a version it would fetch.
    let mut notes: Vec<String> = Vec::new();
    if let Some(note) = phone_unreachable_note() {
        notes.push(note);
    }
    if config.update_check {
        if let Some(note) = update_check::cached_notice() {
            notes.push(note);
        }
    }
    if !notes.is_empty() {
        eprintln!();
        for note in &notes {
            eprintln!("{note}");
            eprintln!();
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    // Fire-and-forget, after the notices so its spawn cost cannot delay
    // them. The network belongs entirely to the detached child, so the
    // attach below proceeds without waiting on it.
    if config.update_check {
        update_check::spawn_checker_if_due();
    }

    // **Nothing is printed here.**
    //
    // This line used to name the session and how to detach, and no reader ever saw
    // it: the tmux client takes the terminal's alternate screen microseconds later
    // and the banner is erased with everything else on the tab. Verified by
    // capturing the host pane, which starts at Claude's first frame.
    //
    // Even if it survived, it would be product output on a command whose whole
    // promise is that the session is unchanged. `codeconnect --help` carries
    // `attach`, which is where a durable answer belongs.
    tmux::exec_attach(&session_id)?;
    unreachable!("exec replaces the process")
}

/// The "your phone cannot reach this Mac" note, or `None` while it can —
/// the same fact the app shows as its "Connect Tailscale" banner, told in
/// the same words at the other keyboard. Printed by `start_claude` in the
/// shared pre-attach advisory slot (one 3-second hold however many notes
/// apply).
///
/// Three states earn it, distinguished because their fixes differ:
///
///   * the daemon is bound to an address no phone could ever reach (it
///     started before the tailnet was up) — connect Tailscale, then restart
///     the daemon so it can re-resolve;
///   * the daemon is bound to a tailnet address but Tailscale's backend is
///     stopped (toggled off after the daemon started) — connect Tailscale
///     and nothing else. The standing bind revives with the tunnel: measured
///     on this machine, one daemon held its 100.x listener across a full
///     off/on toggle and accepted connections again with no restart;
///   * Tailscale is not installed at all — set it up.
///
/// A probe that *fails* (tailscale wedged, output unreadable) stays silent:
/// "Tailscale is off" is a categorical claim, and an error is not evidence.
///
/// The session itself is untouched: it is already running when this prints,
/// everything is recorded, and the hold delays only the attach. A warning
/// with no pause is a warning nobody has ever seen — the tmux client erases
/// the terminal microseconds after `exec_attach` (measured; see the note
/// there).
fn phone_unreachable_note() -> Option<String> {
    // No daemon, no claim: a daemon that is not running is a different
    // conversation, and its absence already has its own surfaces. The
    // timeout is advisory-sized — this check must never make a healthy
    // start wait behind a wedged daemon.
    let Ok(protocol::ipc::DaemonFrame::Daemon(info)) = daemon::request_within(
        &protocol::ipc::ClientFrame::DaemonInfo,
        std::time::Duration::from_millis(400),
    ) else {
        return None;
    };
    let signal = match protocol::pairing::tailscale_bin() {
        None => TailscaleSignal::NotInstalled,
        Some(bin) => probe_tailscale(&bin),
    };
    phone_reachability_note(&info.endpoint_host, info.bind_ip.as_deref(), signal)
}

/// What this Mac's Tailscale is doing right now, as far as an advisory may
/// honestly claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailscaleSignal {
    NotInstalled,
    /// The probe failed — wedged daemon, unreadable output. Not evidence.
    Unknown,
    Up,
    /// The backend answered and said it is not running the tunnel.
    Down,
}

/// Ask `tailscale status --json` for its `BackendState`.
///
/// The JSON is the only shape worth parsing: `tailscale ip -4` was measured
/// printing the *assigned* address while `status` said "Tailscale is
/// stopped" — the address is configuration, not liveness. Bounded, because
/// an advisory must never hang a start behind a wedged tailscaled.
fn probe_tailscale(bin: &std::path::Path) -> TailscaleSignal {
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new(bin)
        .args(["status", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
    else {
        return TailscaleSignal::Unknown;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return TailscaleSignal::Unknown;
            }
        }
    }
    let mut stdout = String::new();
    use std::io::Read;
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    parse_backend_state(&stdout)
}

/// `BackendState` → signal. Pure, so the recognised states stay pinned.
///
/// Only the states that *mean* the tunnel is not carrying traffic map to
/// `Down`; anything unrecognised — including a transitional `Starting` — is
/// `Unknown`, because an advisory that guesses is worse than one that stays
/// quiet.
fn parse_backend_state(stdout: &str) -> TailscaleSignal {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return TailscaleSignal::Unknown;
    };
    match value.get("BackendState").and_then(|v| v.as_str()) {
        Some("Running") => TailscaleSignal::Up,
        Some("Stopped") | Some("NeedsLogin") | Some("NeedsMachineAuth") => TailscaleSignal::Down,
        _ => TailscaleSignal::Unknown,
    }
}

/// Whether this address is the tailnet's to explain: Tailscale's own CGNAT
/// v4 range, or its `fd7a:115c:a1e0::/48` v6 range (the /48 matters — all of
/// `fd7a::/16` is ordinary ULA space anyone may use). An operator who
/// explicitly bound the daemon elsewhere gets no Tailscale advice about it.
fn tailnet_shaped_ip(host: &str) -> bool {
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            let octets = v4.octets();
            octets[0] == 100 && (64..128).contains(&octets[1])
        }
        Ok(std::net::IpAddr::V6(v6)) => {
            let segments = v6.segments();
            segments[0] == 0xfd7a && segments[1] == 0x115c && segments[2] == 0xa1e0
        }
        Err(_) => false,
    }
}

/// The warning text, or `None` while the phone has a route to this daemon.
/// Pure, so the wording and the rules are pinned by tests.
///
/// `bind_ip` is the socket's real address; `endpoint_host` is the name the
/// phone dials, which under TLS is a MagicDNS name regardless of the bind.
/// The toggled-off advice keys on the *bind*: only a daemon genuinely bound
/// to a tailnet address revives with the tunnel, and an older daemon that
/// does not report its bind gets silence, not a guess.
///
/// The wording matches the app's own banner for the same state — the phone
/// says "Connect Tailscale" / "Set up Tailscale"; this says the same thing
/// to the same person at the other keyboard.
fn phone_reachability_note(
    endpoint_host: &str,
    bind_ip: Option<&str>,
    signal: TailscaleSignal,
) -> Option<String> {
    if let Some(problem) = protocol::pairing::unreachable_host(endpoint_host) {
        let fix = match signal {
            TailscaleSignal::NotInstalled => {
                "  set up Tailscale on this Mac — CodeConnect reaches phones only over\n  \
                 your tailnet:\n\n      \
                 https://tailscale.com/download\n\n  \
                 then `codeconnect daemon restart`"
            }
            _ => {
                "  connect Tailscale on this Mac (menu bar, or `tailscale up`), then:\n\n      \
                 codeconnect daemon restart"
            }
        };
        return Some(format!(
            "  note: your phone cannot reach this Mac right now. The daemon is\n  \
             listening on {endpoint_host}, which {problem}.\n\n\
             {fix}\n\n  \
             This session is unaffected — it is already running and recording;\n  \
             the phone catches up when the tailnet is back."
        ));
    }
    if signal == TailscaleSignal::Down && bind_ip.is_some_and(tailnet_shaped_ip) {
        return Some(
            "  note: Tailscale is off on this Mac, so your phone cannot reach it\n  \
             right now.\n\n  \
             connect Tailscale (menu bar, or `tailscale up`) — the daemon keeps\n  \
             its tailnet address and is reachable again the moment the tunnel\n  \
             is back. Nothing needs restarting.\n\n  \
             This session is unaffected — it is already running and recording;\n  \
             the phone catches up when the tailnet is back."
                .to_string(),
        );
    }
    None
}

/// Launch the supervisor so it outlives this process *and* the terminal tab.
///
/// `process_group(0)` puts it in its own process group, so the SIGHUP/SIGINT
/// that reach the tab's foreground group never reach it. When `codeconnect` execs into
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

    let current = std::env::current_exe().context("locating the codeconnect binary")?;
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
        bail!("no session named {name}; `codeconnect ls` shows what is running");
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
    // never be required: `codeconnect ls` has to work when ccd is down.
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

/// Find the real `claude`, never `codeconnect` itself.
///
/// launchd-safe: an explicit candidate list first, `PATH` only as a fallback,
/// and a guard against resolving to this binary (which a shell alias like
/// `alias claude=codeconnect claude` would otherwise cause, producing an infinite spawn
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

    /// Whether a bare command name resolves on this machine's `PATH`.
    ///
    /// Used to skip the two environment assertions below on a machine that has
    /// no `claude` — a CI runner. They are not weakened: on any machine that
    /// *has* it, both still run and still assert. Skipping on the strict
    /// condition "the tool is absent" is the difference between a test that
    /// cannot run here and a test that quietly stopped running everywhere.
    fn on_path(name: &str) -> bool {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
            .unwrap_or(false)
    }

    #[test]
    fn resolves_the_real_claude_on_this_machine() {
        if !on_path("claude") {
            eprintln!("skipped: no `claude` on PATH — nothing to resolve");
            return;
        }
        let path = resolve_claude_bin(&Config::default()).expect("claude must be installed");
        assert!(path.is_file());
        assert!(
            !path.ends_with("codeconnect"),
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
        if !on_path("claude") {
            eprintln!("skipped: no `claude` on PATH — nothing to fall through to");
            return;
        }
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

    /// The pre-attach warning across its states, in the app's own vocabulary
    /// ("connect Tailscale" / "set up Tailscale") so the two screens the same
    /// person is looking at never disagree — and each state names *its* fix,
    /// because they differ.
    #[test]
    fn the_phone_reachability_note_matches_the_apps_vocabulary() {
        use TailscaleSignal::*;
        let note = phone_reachability_note;

        // Bound wrong (daemon started before the tailnet): restart required.
        let bound_wrong = note("127.0.0.1", Some("127.0.0.1"), Up).expect("loopback warns");
        assert!(bound_wrong.contains("127.0.0.1"));
        assert!(bound_wrong.contains("connect Tailscale"));
        assert!(bound_wrong.contains("codeconnect daemon restart"));
        assert!(
            bound_wrong.contains("unaffected"),
            "the note must say the session itself is fine: {bound_wrong}"
        );

        // Tailscale absent entirely: set-up instruction.
        let missing = note("127.0.0.1", Some("127.0.0.1"), NotInstalled).expect("loopback warns");
        assert!(missing.contains("set up Tailscale"));
        assert!(missing.contains("https://tailscale.com/download"));

        // Bound to the tailnet, backend stopped afterwards: connect it back
        // and nothing else — the standing bind revives with the tunnel
        // (measured: one daemon held its 100.x listener across a full off/on
        // toggle and accepted connections again, same pid). Keyed on the
        // *bind*: under TLS the endpoint is a MagicDNS name whatever the
        // operator bound.
        let toggled_off =
            note("mac.tailnet.ts.net", Some("100.64.0.7"), Down).expect("dead tailnet warns");
        assert!(toggled_off.contains("Tailscale is off on this Mac"));
        assert!(toggled_off.contains("Nothing needs restarting"));
        assert!(
            !toggled_off.contains("codeconnect daemon restart"),
            "no restart instruction when the bind is fine: {toggled_off}"
        );

        // Silence, each for its own reason.
        assert_eq!(
            note("mac.tailnet.ts.net", Some("100.64.0.7"), Up),
            None,
            "healthy earns silence"
        );
        assert_eq!(
            note("mac.tailnet.ts.net", Some("192.168.1.20"), Down),
            None,
            "an operator's explicit LAN bind gets no Tailscale advice, even \
             though TLS put a MagicDNS name on the endpoint"
        );
        assert_eq!(
            note("mac.tailnet.ts.net", None, Down),
            None,
            "an older daemon that cannot report its bind gets silence, not a guess"
        );
        assert_eq!(
            note("mac.tailnet.ts.net", Some("100.64.0.7"), Unknown),
            None,
            "a failed probe is not evidence; stay quiet"
        );
        assert_eq!(
            note("mac.tailnet.ts.net", Some("100.64.0.7"), NotInstalled),
            None,
            "a tailnet bind with no tailscale binary is a state the daemon \
             could not have produced; stay silent rather than guess"
        );
    }

    /// `BackendState` is the only liveness signal worth believing:
    /// `tailscale ip -4` was measured printing the assigned address while
    /// `status` said stopped. Unrecognised states — including transitional
    /// ones — are Unknown, never Down.
    #[test]
    fn the_tailscale_probe_believes_backend_state_only() {
        use TailscaleSignal::*;
        assert_eq!(parse_backend_state(r#"{"BackendState":"Running"}"#), Up);
        assert_eq!(parse_backend_state(r#"{"BackendState":"Stopped"}"#), Down);
        assert_eq!(
            parse_backend_state(r#"{"BackendState":"NeedsLogin"}"#),
            Down
        );
        assert_eq!(
            parse_backend_state(r#"{"BackendState":"NeedsMachineAuth"}"#),
            Down
        );
        assert_eq!(
            parse_backend_state(r#"{"BackendState":"Starting"}"#),
            Unknown,
            "transitional is not off"
        );
        assert_eq!(parse_backend_state("not json"), Unknown);
        assert_eq!(parse_backend_state(""), Unknown);
        assert_eq!(parse_backend_state(r#"{"Version":"1.94"}"#), Unknown);
    }

    /// Which addresses are the tailnet's to explain: the CGNAT v4 range and
    /// Tailscale's own `fd7a:115c:a1e0::/48` — not all of `fd7a::/16`, which
    /// is ordinary ULA space anyone may use.
    #[test]
    fn tailnet_shaped_ips_are_cgnat_and_the_tailscale_48_only() {
        assert!(tailnet_shaped_ip("100.64.0.1"));
        assert!(tailnet_shaped_ip("100.101.102.103"));
        assert!(tailnet_shaped_ip("100.127.255.254"));
        assert!(
            !tailnet_shaped_ip("100.128.0.1"),
            "the CGNAT range ends at 100.127"
        );
        assert!(!tailnet_shaped_ip("100.63.255.255"));
        assert!(!tailnet_shaped_ip("192.168.1.20"));
        assert!(tailnet_shaped_ip("fd7a:115c:a1e0::6001:6740"));
        assert!(
            !tailnet_shaped_ip("fd7a:2222::1"),
            "a stranger's ULA in fd7a::/16 is not Tailscale's"
        );
        assert!(
            !tailnet_shaped_ip("mac.tailnet.ts.net"),
            "names are not binds"
        );
    }
}
