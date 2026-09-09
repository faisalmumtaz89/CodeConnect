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

mod codex;
mod codex_coordinator;
mod codex_custodian;
mod codex_host;
mod codex_launch;
mod daemon;
mod exec_gate;
/// Test-only: the fence that proves the suite does not write into the operator's
/// own `~/.codeconnect`. See the module's own docs for what it watches.
#[cfg(test)]
mod home_guard;
mod launchd;
mod pair;
mod sessions;
mod settings;
mod supervisor;
mod tmux;
mod update_check;
mod update_install;
mod update_release;

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

    // A phone whose app predates the retirement of SSH still tells its owner to
    // run one of two commands here. That instruction ships inside an app already
    // installed on phones, so it cannot be corrected where it is written — it is
    // corrected here, at the moment it is followed. Consulted ahead of the match
    // so both spellings are answered by one explanation rather than by whichever
    // "unknown" refusal they happen to land on.
    if let Some(retired) = retired_command(command, rest) {
        explain_retired(retired);
    }

    match command {
        "claude" => start_claude(rest),
        "codex" => codex::start(rest),
        "attach" => attach(rest),
        "ls" | "list" => list(),
        "sessions" => sessions::command(rest),
        "token" => token(),
        "pair" => pair::pair(rest),
        "devices" => pair::devices(rest),
        "revoke" => pair::revoke(rest),
        "daemon" => launchd::command(rest),
        // Hidden: spawned by `codeconnect claude`, never typed by a human.
        "supervise" => supervise(rest),
        // Hidden: the D6 inert exec gate. Spawned by a launch actor to bring a
        // child up inertly; it either execs its target on GO or _exits without
        // ever touching it. Machinery, never typed by a human.
        "internal-exec-gate" => exec_gate::run_gate(rest),
        // Hidden: a test-only gated target that fires the D6 readiness fence and
        // touches a marker, so the exec-gate tests can fence `execve` on a real
        // target-side effect. Machinery, never typed by a human.
        "internal-gate-ack-probe" => exec_gate::run_ack_probe(rest),
        // Hidden: a test-only stand-in for the launcher's probe freeze. It takes a
        // vnode freeze the way `codex::probe_codex` does and then blocks, so a test
        // can interrupt it and read the flag. Machinery, never typed by a human.
        "internal-freeze-probe" => codex::run_freeze_probe(rest),
        // Hidden: a test-only stand-in for a freezer holding the executable's freeze
        // lock, so a test can prove a second PROCESS is excluded from it. Carries its
        // own deadline. Machinery, never typed by a human.
        "internal-freeze-lock-hold" => codex::run_freeze_lock_hold(rest),
        // Hidden: the D7 launch coordinator (the supervisor in launch mode).
        // Spawned by the `codex` launcher before tmux exists; it owns the launch
        // record and every forward mutation, and stays on as the session's
        // supervisor once it commits `ready`. Machinery.
        "internal-codex-coordinator" => codex_coordinator::run_coordinator(rest),
        // Hidden: the D7 launch custodian. Armed before `tmux new-session` with
        // independent cleanup authority. Machinery.
        "internal-codex-custodian" => codex_custodian::run_custodian(rest),
        // Hidden: one bounded D7 recovery sweep (stale pendings → failed;
        // failed+incomplete+dead-custodian → replacement custodian). Machinery.
        c if c == protocol::CODEX_SWEEP_SUBCOMMAND => codex_custodian::run_sweep(rest),
        // Hidden: the D7 late-host preflight gate — validate the launch record +
        // take a lease, or cleanup-only refuse. Machinery.
        "internal-codex-host-preflight" => codex_custodian::run_host_preflight(rest),
        // Hidden: the Codex session host (Phase 2e). Runs inside a tmux pane;
        // launches the app-server, serves the broker in front of it, and spawns
        // the interactive TUI against the broker. Machinery, never typed by a
        // human — the coordinator spawns it (2e-2b).
        "internal-codex-host" => codex_host::run_host(rest),
        // Hidden: the detached update checker `codeconnect claude` spawns.
        // Not in --help on purpose — it is machinery, not a command.
        "__update-check" => update_check::run_checker(),
        "update" => update_check::run_update(),
        "--version" | "version" => {
            println!(
                "{}",
                protocol::build_identity::version_line("codeconnect", env!("CARGO_PKG_VERSION"))
            );
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
    eprintln!("{}", usage_text());
}

/// The banner, as text rather than as a side effect.
///
/// Split out so it can be held to the same bar as the retirement notice. This
/// is the other place a claim about SSH could plausibly be written — it is the
/// list of what this CLI does, so a line reinstating `pair --ssh`, or
/// reassuring the reader that keys were dealt with, belongs here if it belongs
/// anywhere. A banner is also the text nobody rereads, which is exactly why the
/// guard has to reach it rather than the author having to remember.
fn usage_text() -> &'static str {
    "\
cc — CodeConnect shim

  codeconnect claude [args…]      run claude in the private tmux server, attached here
  codeconnect codex [args…]       run codex in the private tmux server, attached here
  codeconnect attach <name>       re-attach a session (e.g. after closing the tab)
  codeconnect ls                  list what tmux is running (works with ccd down)
  codeconnect sessions            list what the event log knows, with lifecycle
  codeconnect sessions prune      remove ended sessions and their events (--dry-run first)

  codeconnect update              install the latest release (daemon restarts)

  codeconnect daemon install      install and start the ccd LaunchAgent
  codeconnect daemon status       plist, launchd job and live daemon
  codeconnect daemon restart      restart the managed daemon
  codeconnect daemon uninstall    stop it and remove the LaunchAgent

  codeconnect pair                show a QR code that pairs a phone (single use, 5 min)
  codeconnect devices             list paired devices
  codeconnect revoke <device>     revoke a device's token
  codeconnect token               print the static fallback token
"
}

// ------------------------------------------------------- retired commands

/// A command this CLI no longer has, still named on a phone screen.
///
/// CodeConnect's terminal rides the paired connection, so nothing here manages
/// SSH keys. An app from before that change asks its owner to authorise a key
/// with `codeconnect pair --ssh`, and to withdraw one with
/// `codeconnect ssh-revoke`. Both are answered rather than refused: the person
/// typing them is doing exactly what their screen told them to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetiredCommand {
    /// A pairing that also files a key in `~/.ssh/authorized_keys`.
    SshPairing,
    /// The withdrawal of such a key.
    SshRevocation,
}

/// The status a retired command leaves with.
///
/// Non-zero, and deliberately the same `1` an unrecognised command leaves
/// with: a retired command performed no part of what it was asked, so a script
/// that runs `codeconnect pair --ssh` and reads success would be reading a
/// pairing that never happened. One code rather than a second one invented
/// here, because "this did not work" is the whole of what a caller needs and
/// nothing consumes a finer distinction.
const RETIRED_EXIT_CODE: i32 = 1;

/// Which retired command this argv names, or `None` for one that still exists.
///
/// Pure, so the routing and the wording it selects are both pinned by tests
/// with no process to spawn.
///
/// Three spellings reach the same two answers, because all three were reachable
/// when the commands existed and any of them may be typed from memory: the
/// `ssh-revoke` command word, with or without the device it once took; `--ssh`
/// on `pair`, wherever it sits among the arguments; and `--ssh` on `revoke`,
/// which is the half-remembered form of `ssh-revoke`. That last one earns its
/// place by what it does otherwise — `revoke` reads only its first argument, so
/// `codeconnect revoke iPhone --ssh` would revoke the device's token outright
/// while its author believed they were removing a key.
///
/// The flag is looked for only under those two commands. `codeconnect claude`'s
/// arguments belong to `claude` and `codeconnect supervise`'s to the
/// supervisor; a scan that reached either would let this dispatch swallow an
/// argument meant for another program.
fn retired_command(command: &str, args: &[String]) -> Option<RetiredCommand> {
    let ssh_flag = || args.iter().any(|arg| arg == "--ssh");
    match command {
        "ssh-revoke" => Some(RetiredCommand::SshRevocation),
        "pair" if ssh_flag() => Some(RetiredCommand::SshPairing),
        "revoke" if ssh_flag() => Some(RetiredCommand::SshRevocation),
        _ => None,
    }
}

/// What a retired command says for itself. Pure, so the facts it has to carry
/// are pinned by tests rather than by reading it.
///
/// The shared advisory grammar: a heading, the explanation, a remedy with its
/// commands isolated on their own lines, the check that settles the rest, and a
/// closing. Bold rather than the impairment colour this file paints an
/// unreachable phone with: nothing here is a fault report. The command was real
/// once, the reader typed what their screen told them to type, and what is out
/// of date is the app that told them.
///
/// **Why this describes the sweep instead of declaring it done.** `ccd`'s
/// `purge_authorized_keys` is best-effort by contract: an unresolvable `$HOME`
/// skips `~/.ssh/authorized_keys` outright, and an `~/.ssh` that cannot be
/// rewritten leaves every entry where it is — both a warning in the daemon log
/// rather than a daemon that refuses to start or a revocation that reports
/// failure. Two callers reach it now, startup and `Daemon::revoke`, and a
/// second best effort is still a best effort: what the notice may say about
/// either is that it runs. This CLI also answers on a Mac whose upgraded daemon
/// has never run, and, being pure, reads neither that file nor that log — nor
/// the count a revocation deliberately does not carry back over IPC. So the
/// notice states what the daemon does and hands over the one-line check, which
/// is worth more to a reader worried about a stale grant than a reassurance
/// nothing here can see.
fn retirement_notice(retired: RetiredCommand, style: update_check::Style) -> String {
    let heading = match retired {
        RetiredCommand::SshPairing => "codeconnect pair --ssh is retired",
        RetiredCommand::SshRevocation => "codeconnect ssh-revoke is retired",
    };

    // Both answers rest on the same facts, so they are stated once. The last of
    // them is the daemon's behaviour, not this Mac's state: the sweep is
    // allowed to fail, so where it reports failure is part of the fact.
    //
    // **Only the install half is denied, because only the install half is
    // true.** This used to read "CodeConnect neither installs an SSH key nor
    // revokes one" — immediately above two sentences describing the daemon
    // removing keys, which is what revoking one is. `ccd`'s
    // `legacy_credentials::purge_authorized_keys` runs at startup and again
    // from `Daemon::revoke`, and takes out every marker-and-key pair an earlier
    // release wrote. That the IPC reply carries `ssh_key_removed: AlwaysFalse`
    // is a fact about what a revocation reports back, not about what it does to
    // the file, and reading it as the latter is how the denial got written.
    //
    // What survives is the claim this CLI can still answer for: `pair.rs` sends
    // `allow_ssh: false`, so nothing in this project installs a key. What
    // replaces the rest is the sweep itself and the hedge it needs — a grant
    // survives silently whenever the key sits outside the single hardcoded
    // `$HOME/.ssh/authorized_keys` or on any line outside the marker-and-key
    // pair, which is what "best-effort" and the check below are carrying.
    let explanation = "CodeConnect does not use SSH. The Terminal tab rides the same paired\n\
                       connection as the rest of the app, so CodeConnect never installs an SSH\n\
                       key — and it does take one back. At startup the daemon removes the\n\
                       entries an earlier release wrote into ~/.ssh/authorized_keys, and it\n\
                       sweeps that file again on every revocation. It is best-effort either\n\
                       way, and warns in its log when it cannot.";
    let out_of_date = "The phone that asked for this is running an older version of the app.";

    // Where the two part company: one reader wanted to grant access, the other
    // to take it back, and each needs the command that now serves that intent.
    //
    // The revocation answer says what that command does to the file as well,
    // because this reader arrived meaning to remove a key and would otherwise
    // read `codeconnect revoke` as touching only the token. It says the sweep
    // runs and stops there: `Daemon::revoke` carries no count back, five of the
    // sweep's outcomes remove nothing, and the check below is what settles
    // which of them this Mac saw.
    let remedy = match retired {
        RetiredCommand::SshPairing => format!(
            "{out_of_date}\n\
             Update the app: its Terminal tab then connects over the paired link\n\
             with nothing to authorise.\n\n\
             Pairing a phone is unchanged:\n\n\
             {}",
            emphasized("codeconnect pair", style)
        ),
        RetiredCommand::SshRevocation => format!(
            "{out_of_date}\n\
             Update the app: its Terminal tab then rides the paired link, and\n\
             nothing in CodeConnect has a use for the key it holds. The check\n\
             below shows what is still in that file.\n\n\
             To stop a phone reaching CodeConnect, revoke its device token:\n\n\
             {}\n\
             {}\n\n\
             Revoking also asks the daemon to sweep ~/.ssh/authorized_keys\n\
             again, on the same best effort it makes at startup.",
            emphasized("codeconnect devices", style),
            emphasized("codeconnect revoke <device>", style)
        ),
    };

    // The reader who arrived worried about a stale grant is served by a check
    // they can run over a reassurance this function is in no position to give.
    // `codeconnect:` is the tag `ccd` matches on, carried by both lines of an
    // installed entry, so the same tag is what finds one still in place.
    //
    // The pair is spelled out because the tag alone is a wider net than the
    // daemon's own rule: `legacy_credentials` removes a key line only as the
    // second half of its marker's pair, and leaves a lone key line alone
    // however its comment field reads. Advice to delete whatever matches would
    // aim the operator at somebody else's key with only their memory to stop
    // them, so what is safe to remove is described by its shape instead.
    let check = format!(
        "To see what is still in that file on this Mac, search for the tag\n\
         those entries carry:\n\n\
         {}\n\n\
         Nothing printed means nothing there carries the tag; grep saying\n\
         there is no such file means the same thing. What it prints is one of\n\
         ours where two adjacent lines carry the same tag: a marker comment\n\
         beginning # codeconnect:<id>, and directly beneath it an ssh-ed25519\n\
         line whose last field is that same tag. Delete that pair by hand.\n\
         A tagged key line with no such marker directly above it is one this\n\
         daemon leaves alone, because nothing in the file identifies whose it\n\
         is — removing it is your call rather than its.",
        emphasized("grep codeconnect: ~/.ssh/authorized_keys", style)
    );

    // The reader's part is observable from here — the spelling they typed is
    // one only an older app hands out. This Mac's part is not, so it is the
    // check above that speaks to it and not this line.
    let closing =
        "Nothing you typed was wrong \u{2014} the app on the phone is what is out of date.";

    let text = format!(
        "{}\n{explanation}\n\n{remedy}\n\n{check}\n\n{closing}",
        emphasized(heading, style)
    );
    match style {
        update_check::Style::Ascii => asciify(&text),
        _ => text,
    }
}

/// Print `retired`'s explanation and leave.
///
/// **Why this leaves through `process::exit` rather than through the `Result`
/// every other command returns.** anyhow prints a failure as `Error: {msg}`,
/// which is the right frame for a mistake — and this is not one. The reader
/// typed what their phone told them to type, and a paragraph explaining that,
/// prefixed `Error:`, reads as a crash they caused. The status must still be
/// non-zero, so returning `Ok(())` is not available either. Writing the notice
/// and choosing the code directly is what satisfies both.
///
/// Safe at this point specifically: nothing has been opened, spawned or
/// written, so the destructors `exit` skips have nothing to release, and
/// stderr is unbuffered — the notice is out before the call.
fn explain_retired(retired: RetiredCommand) -> ! {
    use std::io::IsTerminal;
    // stderr, like every other advisory here: this is a refusal rather than the
    // product of a command, and a reader who redirected stdout expecting a QR
    // still sees why they did not get one.
    let style = update_check::style_for_stream(std::io::stderr().is_terminal());
    eprintln!("{}", retirement_notice(retired, style));
    std::process::exit(RETIRED_EXIT_CODE);
}

/// The agent-varying pieces of a launch: the resolved binary, the exact argv and
/// env the tmux session runs. Built by an agent-specific planner so the pieces
/// that differ between agents live in one place. The `claude` planner reproduces
/// byte-for-byte what shipped — proven by the launcher test and the fixture
/// replay — and it is the only planner today; another agent's launch path lands
/// with that agent, not before.
struct AgentLaunchPlan {
    /// The resolved agent binary, passed on to the supervisor.
    binary: PathBuf,
    /// argv[0] is the binary; the rest is agent flags plus the caller's passthrough.
    argv: Vec<String>,
    env: Vec<(String, String)>,
}

/// The exact argv and env a Claude session runs — a **pure** function, so the
/// byte-identical guarantee is testable without touching the filesystem or tmux.
/// Any change here changes what `claude` itself sees; the launcher test pins it.
fn claude_argv_and_env(
    binary: &std::path::Path,
    settings_path: &std::path::Path,
    session_id: &str,
    session_uid: &str,
    passthrough: &[String],
) -> (Vec<String>, Vec<(String, String)>) {
    let mut argv = vec![
        binary.to_string_lossy().to_string(),
        "--settings".to_string(),
        settings_path.to_string_lossy().to_string(),
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
        (protocol::ENV_SESSION.to_string(), session_id.to_string()),
        (
            protocol::ENV_SESSION_UID.to_string(),
            session_uid.to_string(),
        ),
    ];
    (argv, env)
}

/// Plan a Claude launch around an **already-resolved** binary: write the
/// control-plane settings document and assemble the byte-identical argv/env. The
/// binary is resolved separately and first (see `start_agent`), so a missing
/// binary fails before any session state exists — the pre-seam ordering.
fn plan_claude_launch(
    config: &Config,
    binary: &std::path::Path,
    session_id: &str,
    session_uid: &str,
    passthrough: &[String],
) -> Result<AgentLaunchPlan> {
    let settings = settings::write_for_session(session_id, session_uid, config)?;
    let (argv, env) =
        claude_argv_and_env(binary, &settings.path, session_id, session_uid, passthrough);
    Ok(AgentLaunchPlan {
        binary: binary.to_path_buf(),
        argv,
        env,
    })
}

fn start_claude(passthrough: &[String]) -> Result<()> {
    start_agent(protocol::agent::AgentKind::Claude, passthrough)
}

/// Launch an agent session: mint the identity, plan the agent-varying pieces,
/// create the tmux session, and hand ownership to the supervisor. Everything
/// outside the plan — the identity, the tmux session, the advisories, the
/// attach — is agent-agnostic; the plan is where an agent differs. Only Claude
/// has a planner today; any other agent is refused before anything is spawned.
fn start_agent(agent: protocol::agent::AgentKind, passthrough: &[String]) -> Result<()> {
    let config = Config::load();

    // **Binary first.** Resolving the agent's executable is the first thing that
    // can fail, and it must fail before a tmux name is taken or a session
    // identity minted — exactly as the pre-seam launcher did, so a missing
    // binary surfaces the same way it always has. This is the agent-varying
    // binary-resolution step; a non-Claude agent is refused here, before any
    // state exists.
    let binary = match &agent {
        protocol::agent::AgentKind::Claude => resolve_claude_bin(&config)?,
        other => bail!("{} sessions cannot be launched yet", other.as_str()),
    };

    let cwd = std::env::current_dir().context("reading the current directory")?;
    let cwd = cwd.to_string_lossy().to_string();

    let session_id = tmux::next_session_name()?;
    // Minted here, once, before anything else knows the session exists. The
    // tmux name is reused as soon as this session exits; this is not, and it is
    // what the event log, the tail cursor and the answers ledger are keyed by.
    let session_uid = protocol::uid::new().context("minting a session uid")?;

    let plan = match &agent {
        protocol::agent::AgentKind::Claude => {
            plan_claude_launch(&config, &binary, &session_id, &session_uid, passthrough)?
        }
        // Unreachable: a non-Claude agent already bailed at binary resolution.
        other => bail!("{} sessions cannot be launched yet", other.as_str()),
    };

    tmux::new_session(
        &session_id,
        &cwd,
        &plan.env,
        &plan.argv,
        tmux::terminal_size(),
        config.tmux_status,
        config.tmux_history_limit,
    )
    .with_context(|| format!("creating tmux session {session_id}"))?;

    spawn_supervisor(&session_id, &session_uid, &cwd, &plan.binary)?;

    // After the session and supervisor exist, before the alternate screen:
    // the hold below delays only the *display*, never the session it is
    // promising is unaffected — Claude is already running while this is
    // read. One hold however many notes apply; reachability first, because
    // a phone that cannot connect at all outranks a version it would fetch.
    // Styling answers for stderr — this envelope's stream — and the whole
    // group shares one decision. Reachability first: a phone that cannot
    // connect at all outranks a version it would fetch.
    {
        use std::io::IsTerminal;
        let style = update_check::style_for_stream(std::io::stderr().is_terminal());
        let reachability = phone_unreachable_note(style);
        // One update slot: the release advisory, behind the `update_check`
        // switch. `codeconnect update` is what resolves it.
        let update = config
            .update_check
            .then(|| update_check::select_update_note(update_check::cached_advisory(), style))
            .flatten();
        if let Some(group) = advisory_envelope(reachability, update) {
            eprint!("{group}");
            // The hold exists for a reader: only a real person at a real
            // terminal gets one, and it is a countdown — not a spinner,
            // because nothing is working; the session already exists and
            // the pause is for reading. Return skips it.
            if hold_permitted(
                std::io::stderr().is_terminal(),
                std::io::stdin().is_terminal(),
                std::env::var("TERM").ok().as_deref(),
            ) {
                let skip = update_check::spawn_line_listener(std::io::stdin());
                update_check::hold_for_reading(
                    std::time::Duration::from_secs(10),
                    &skip,
                    &mut std::io::stderr(),
                    std::time::Instant::now,
                );
            }
        }
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
/// shared pre-attach advisory slot (one 10-second countdown however many
/// notes apply).
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
fn phone_unreachable_note(style: update_check::Style) -> Option<String> {
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
    phone_reachability_note(&info.endpoint_host, info.bind_ip.as_deref(), signal, style)
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

/// The warning text, or `None` when nothing here can see anything wrong —
/// which is silence rather than a clean bill. `unreachable_host` declines to
/// judge a name it cannot resolve, and a probe that came back `Unknown` is not
/// evidence of a tunnel, so a phone can be off the tailnet with every input
/// this reads looking ordinary. Pure, so the wording and the rules are pinned
/// by tests.
///
/// `bind_ip` is the socket's real address; `endpoint_host` is what the QR sends
/// the phone to, which is a hostname when the daemon has one that reaches its
/// own listener and the bind address as a literal otherwise — `ccd`'s
/// `resolve_transport_with` decides which, and is the authority on the rule.
/// What matters here is only that the two can differ, so neither substitutes
/// for the other: a daemon on the tailnet may be advertised under a name, which
/// is why the toggled-off advice keys on the *bind*. Only a daemon genuinely
/// bound to a tailnet address revives with the tunnel, and an older daemon that
/// does not report its bind gets silence, not a guess.
///
/// The wording matches the app's own banner for the same state — the phone
/// says "Connect Tailscale" / "Set up Tailscale"; this says the same thing
/// to the same person at the other keyboard.
fn phone_reachability_note(
    endpoint_host: &str,
    bind_ip: Option<&str>,
    signal: TailscaleSignal,
    style: update_check::Style,
) -> Option<String> {
    // Shared grammar with the update advisory: a heading, the state's own
    // explanation, a blank line, the state's own remedy (its command or URL
    // isolated on its own line), a blank line, the assurance. Bold yellow on
    // the heading — this one *is* a current impairment, which is exactly
    // what the app's amber means — and bold alone on an isolated command.
    let heading = "Phone unreachable";
    let assurance = "This session is unaffected \u{2014} it is already running and recording.\n\
                     The phone catches up when the tailnet is back.";

    let (explanation, remedy) =
        if let Some(problem) = protocol::pairing::unreachable_host(endpoint_host) {
            // The address is data, not prose: isolated on its own line, the
            // sentence around it stays inside its 76 columns however long a
            // MagicDNS name grows.
            let explanation = format!(
                "The daemon is listening on an address no phone can reach:\n\n\
             {endpoint_host}\n\n\
             It {problem}."
            );
            let remedy = match signal {
                TailscaleSignal::NotInstalled => format!(
                    "Set up Tailscale \u{2014} CodeConnect reaches phones only over your\n\
                 tailnet \u{2014} then restart the daemon:\n\n\
                 {}\n\
                 {}",
                    emphasized("https://tailscale.com/download", style),
                    emphasized("codeconnect daemon restart", style)
                ),
                _ => format!(
                    "Connect Tailscale (menu bar, or `tailscale up`), then:\n\n\
                 {}",
                    emphasized("codeconnect daemon restart", style)
                ),
            };
            (explanation, remedy)
        } else if signal == TailscaleSignal::Down && bind_ip.is_some_and(tailnet_shaped_ip) {
            let explanation = "Tailscale is off on this Mac.".to_string();
            // Scoped to what has been watched happen. The daemon does keep its
            // bind across the toggle — that is a property of the socket, not a
            // guess — but "nothing needs restarting" is a promise about every
            // Mac and every toggle, and the evidence behind it is one machine
            // once. Saying what is known, and naming the fallback, costs the
            // reader one sentence and claims only what was seen.
            let remedy = format!(
                "Connect Tailscale (menu bar), or run:\n\n\
             {}\n\n\
             The daemon holds its tailnet address across the toggle, so it is\n\
             normally reachable again as soon as the tunnel is. If the phone\n\
             still cannot reach it, restart the daemon.",
                emphasized("tailscale up", style)
            );
            (explanation, remedy)
        } else {
            return None;
        };

    let heading = match style {
        update_check::Style::Styled => format!("\u{1b}[1;33m{heading}\u{1b}[0m"),
        update_check::Style::PlainUnicode | update_check::Style::Ascii => heading.to_string(),
    };
    let text = format!("{heading}\n{explanation}\n\n{remedy}\n\n{assurance}");
    Some(match style {
        update_check::Style::Ascii => asciify(&text),
        _ => text,
    })
}

/// The advisory group as one write: a leading blank line, the advisories —
/// reachability always first — separated by exactly two blank lines, and a
/// trailing blank line before whatever follows. `None` when there is
/// nothing to say, so a healthy launch writes nothing at all.
fn advisory_envelope(reachability: Option<String>, update: Option<String>) -> Option<String> {
    let notes: Vec<String> = [reachability, update].into_iter().flatten().collect();
    if notes.is_empty() {
        return None;
    }
    Some(format!("\n{}\n\n", notes.join("\n\n\n")))
}

/// Whether the reading hold may run: a real person at a real terminal on
/// both streams, and not a terminal that has disclaimed control sequences.
fn hold_permitted(stderr_tty: bool, stdin_tty: bool, term: Option<&str>) -> bool {
    stderr_tty && stdin_tty && term != Some("dumb")
}

/// Bold when styling is on; bare otherwise. For an advisory's heading, and for
/// an isolated command or URL line — the thing the reader is meant to act on.
fn emphasized(line: &str, style: update_check::Style) -> String {
    match style {
        update_check::Style::Styled => format!("\u{1b}[1m{line}\u{1b}[0m"),
        update_check::Style::PlainUnicode | update_check::Style::Ascii => line.to_string(),
    }
}

/// The ASCII degradation for logs, pipes and `TERM=dumb`: typography maps
/// to its plain equivalents, so no escape or multi-byte character survives.
fn asciify(text: &str) -> String {
    text.replace('\u{2014}', "--")
        .replace('\u{2192}', "->")
        .replace('\u{b7}', ":")
        .replace('\u{2026}', "...")
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
            // No flag, on purpose. `supervise` is spawned by `codeconnect
            // claude` and by nothing else — the Codex launch supervises itself,
            // in the coordinator process, so the seat and the socket travel as
            // values rather than as argv. Adding `--agent` here would be a
            // user-facing surface with no caller, on a parser that silently
            // ignores what it does not recognise.
            tmux_socket: protocol::TMUX_SOCKET_NAME.to_string(),
            cwd,
            claude_bin,
            codex: None,
            // A Claude run shares the fleet-wide tmux server with every other one,
            // so there is no server whose death is this session's death, and no pin
            // to hand over. `None` keeps the probe exactly as it was.
            server_a: None,
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

    /// **The launcher byte-identical gate.** The agent-parameterised launcher
    /// must leave the `claude` argv and env exactly as they shipped. This pins
    /// the pure builder both planners flow through, so a refactor that reorders a
    /// flag, drops an env var, or slips an agent-specific argument into the Claude
    /// path fails here rather than in a session that behaves subtly differently.
    #[test]
    fn the_claude_argv_and_env_are_byte_identical() {
        let (argv, env) = claude_argv_and_env(
            std::path::Path::new("/usr/local/bin/claude"),
            std::path::Path::new("/home/u/.codeconnect/sessions/cc-1-UID/settings.json"),
            "cc-1",
            "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            &[
                "--resume".to_string(),
                "--permission-mode".to_string(),
                "default".to_string(),
            ],
        );
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/claude",
                "--settings",
                "/home/u/.codeconnect/sessions/cc-1-UID/settings.json",
                "--resume",
                "--permission-mode",
                "default",
            ]
        );
        assert_eq!(
            env,
            vec![
                (
                    "CLAUDE_CODE_FORCE_SESSION_PERSIST".to_string(),
                    "1".to_string()
                ),
                ("CODECONNECT_SESSION".to_string(), "cc-1".to_string()),
                (
                    "CODECONNECT_SESSION_UID".to_string(),
                    "01K1B3XQ8ZC0DE5FGH7JKMNPQR".to_string()
                ),
            ]
        );
        // The passthrough is appended verbatim, in order, after the settings flag
        // — never merged, deduplicated or reordered.
        let (bare, _) = claude_argv_and_env(
            std::path::Path::new("claude"),
            std::path::Path::new("/s.json"),
            "cc-2",
            "UID",
            &[],
        );
        assert_eq!(bare, vec!["claude", "--settings", "/s.json"]);
    }

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

    /// An argv as the dispatch receives it, so a test reads like the command
    /// a person typed.
    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    /// The pairing flag an app from before the retirement still names. It has
    /// to be recognised wherever it sits in the arguments, because the reader
    /// is retyping a command from a phone screen and may add to it.
    #[test]
    fn the_retired_ssh_pairing_flag_explains_itself_instead_of_failing_as_an_unknown_option() {
        for words in [
            argv(&["--ssh"]),
            argv(&["--ssh", "--qr"]),
            argv(&["--qr", "--ssh"]),
        ] {
            assert_eq!(
                retired_command("pair", &words),
                Some(RetiredCommand::SshPairing),
                "{words:?} must reach the explanation, not `unknown option`"
            );
        }
    }

    /// The revocation an app from before the retirement still names — bare, as
    /// that app spells it, and with the device the usage block spelled it with.
    ///
    /// `revoke <device> --ssh` is the same intent typed from memory, and it is
    /// the spelling with teeth: `revoke` reads only its first argument, so
    /// without this it revokes the device's token outright while its author
    /// believes they are removing a key.
    #[test]
    fn the_retired_ssh_revocation_explains_itself_instead_of_failing_as_an_unknown_command() {
        for (command, words) in [
            ("ssh-revoke", argv(&[])),
            ("ssh-revoke", argv(&["iPhone"])),
            ("revoke", argv(&["iPhone", "--ssh"])),
            ("revoke", argv(&["--ssh", "iPhone"])),
        ] {
            assert_eq!(
                retired_command(command, &words),
                Some(RetiredCommand::SshRevocation),
                "`codeconnect {command} {words:?}` must reach the explanation"
            );
        }
    }

    /// The diversion is surgical: every command that still exists runs, and a
    /// passthrough command's arguments belong to the program they are passed
    /// to — `claude` owns its own flags, whatever they are spelled.
    #[test]
    fn the_commands_that_still_exist_are_never_diverted() {
        for (command, words) in [
            ("pair", argv(&[])),
            ("revoke", argv(&["iPhone"])),
            ("devices", argv(&[])),
            ("token", argv(&[])),
            ("update", argv(&[])),
            ("help", argv(&[])),
            ("claude", argv(&["--ssh"])),
            ("supervise", argv(&["--session", "cc1", "--ssh"])),
        ] {
            assert_eq!(
                retired_command(command, &words),
                None,
                "`codeconnect {command} {words:?}` still has work to do"
            );
        }
    }

    /// The rule this diversion must not erode: a command nobody recognises is
    /// not a command that succeeded. Nothing here is retired, so all of it
    /// falls through to the refusals that were already there — including the
    /// `cc file.c` shape that is the reason those refusals exit non-zero.
    #[test]
    fn a_command_nobody_recognises_is_still_nobodys_command() {
        for (command, words) in [
            ("frobnicate", argv(&[])),
            ("ssh", argv(&[])),
            ("ssh-install", argv(&[])),
            ("sshrevoke", argv(&[])),
            ("pair", argv(&["--sshh"])),
            ("pair", argv(&["--nope"])),
            ("file.c", argv(&["-o", "out"])),
        ] {
            assert_eq!(
                retired_command(command, &words),
                None,
                "`codeconnect {command} {words:?}` is not retired, it is unknown"
            );
        }

        // And the refusal it falls through to is intact: a mistyped option on
        // `pair` is still an error naming the usage, never a QR code.
        let err = pair::pair(&argv(&["--nope"])).expect_err("an unknown option must not pair");
        assert!(
            err.to_string().contains("unknown option"),
            "the pre-existing refusal has to survive the diversion: {err}"
        );
    }

    /// Every fact the reader needs, in both answers: that SSH is gone and the
    /// terminal rides the paired connection instead, what the daemon does to
    /// `authorized_keys` — at startup, and again on every revocation — and
    /// where it says so when it cannot, the one-line check that settles what is
    /// actually left there, that the app is what is out of date — and the
    /// command that now serves the intent they arrived with.
    #[test]
    fn the_retirement_notice_names_the_facts_that_matter() {
        use update_check::Style;
        let pairing = retirement_notice(RetiredCommand::SshPairing, Style::Ascii);
        let revocation = retirement_notice(RetiredCommand::SshRevocation, Style::Ascii);

        for notice in [&pairing, &revocation] {
            assert!(notice.contains("does not use SSH"), "{notice}");
            assert!(notice.contains("rides the same paired"), "{notice}");
            assert!(
                notice.contains("At startup the daemon removes"),
                "the sweep is what the daemon does, and it must be stated: {notice}"
            );
            assert!(notice.contains("~/.ssh/authorized_keys"), "{notice}");
            assert!(
                notice.contains("again on every revocation"),
                "the startup sweep is not the only one, and a reader told only \
                 about that one has no reason to run `codeconnect revoke` again \
                 as the retry for a sweep that failed: {notice}"
            );
            assert!(
                notice.contains("best-effort"),
                "a sweep the daemon is allowed to fail at must be described as \
                 one, or `the daemon removes the entries` reads as a \
                 guarantee: {notice}"
            );
            assert!(
                notice.contains("warns in its log"),
                "a sweep that is allowed to fail has to name where it says so, \
                 or the reader cannot tell a quiet success from a quiet one: {notice}"
            );
            assert!(
                notice.contains("\ngrep codeconnect: ~/.ssh/authorized_keys\n"),
                "a reader worried about a stale grant is owed a check they can \
                 run, isolated on its own line: {notice}"
            );
            // A check whose hits the reader cannot classify is a check that
            // aims them at somebody else's key: `legacy_credentials` keeps a
            // lone key line however its comment field reads, so the notice
            // has to describe the pair rather than the tag alone.
            assert!(
                notice.contains("two adjacent lines carry the same tag"),
                "the reader needs the shape that is safe to remove, not just \
                 the tag that finds candidates: {notice}"
            );
            // The hit the reader cannot classify from the file: a tagged key
            // with no marker over it. `legacy_credentials` declines to touch it
            // precisely because nothing there says whose it is, so the notice
            // must hand that uncertainty over rather than resolve it. Calling
            // it somebody else's is the one answer the file cannot support, and
            // the answer that files a live grant as a colleague's.
            assert!(
                notice.contains("nothing in the file identifies whose it"),
                "an unlabelled grant has to be described as unidentified: {notice}"
            );
            assert!(
                !notice.contains("somebody else"),
                "the notice must attribute an unlabelled key to nobody: {notice}"
            );
            assert!(
                notice.contains("older version of the app"),
                "the reader has to learn why their phone asked: {notice}"
            );
            assert!(notice.contains("Update the app"), "{notice}");
        }

        // Each names the command that serves the intent it was reached with,
        // and neither offers the other's.
        assert!(pairing.starts_with("codeconnect pair --ssh is retired\n"));
        assert!(pairing.contains("\ncodeconnect pair\n"));
        assert!(
            !pairing.contains("codeconnect revoke"),
            "a reader who came to grant access is not sent to revoke: {pairing}"
        );

        assert!(revocation.starts_with("codeconnect ssh-revoke is retired\n"));
        assert!(revocation.contains("\ncodeconnect revoke <device>\n"));
        assert!(revocation.contains("\ncodeconnect devices\n"));
        // And the promise attached to that command is the one it can keep.
        // `ccd` does sweep `~/.ssh/authorized_keys` on a revocation now, but it
        // answers with `ssh_key_removed: AlwaysFalse` and carries no count
        // back, because five of that sweep's outcomes remove nothing. So what
        // the token buys back is CodeConnect's own grant, and what happened to
        // the file is a matter for the daemon log and the check.
        assert!(
            revocation.contains("To stop a phone reaching CodeConnect, revoke its device token"),
            "revoking a token takes back what CodeConnect granted, which is \
             less than everything: {revocation}"
        );
    }

    /// The notice obeys the same rendering contract as every other advisory
    /// here: pure ASCII with no escapes when styling is off, inside 80 columns
    /// so it survives a narrow terminal, and bold on the heading and on each
    /// command the reader is meant to act on when styling is on.
    #[test]
    fn the_retirement_notice_renders_within_the_advisory_grammar() {
        use update_check::Style;
        for retired in [RetiredCommand::SshPairing, RetiredCommand::SshRevocation] {
            let plain = retirement_notice(retired, Style::Ascii);
            assert!(plain.is_ascii(), "non-ascii survived: {plain}");
            assert!(!plain.contains('\u{1b}'), "an escape survived: {plain}");
            for line in plain.lines() {
                assert!(line.len() < 80, "over 80 cols: {line}");
                assert_eq!(line, line.trim_end(), "trailing space: {line:?}");
            }

            let styled = retirement_notice(retired, Style::Styled);
            assert!(styled.starts_with("\u{1b}[1m"), "{styled}");
            // Every command line is emphasised, and every emphasis is closed —
            // an unreset attribute bleeds across the rest of the terminal.
            assert_eq!(
                styled.matches("\u{1b}[1m").count(),
                styled.matches("\u{1b}[0m").count(),
                "{styled}"
            );
        }
    }

    /// The pre-attach warning across its states, in the app's own vocabulary
    /// ("connect Tailscale" / "set up Tailscale") so the two screens the same
    /// person is looking at never disagree — and each state names *its* fix,
    /// The envelope, exactly: nothing at all when there is nothing to say;
    /// a leading blank line; reachability before the update; exactly two
    /// blank lines between advisories; one trailing blank line.
    #[test]
    fn the_advisory_envelope_spaces_and_orders_exactly() {
        assert_eq!(
            advisory_envelope(None, None),
            None,
            "healthy launches write nothing"
        );
        assert_eq!(
            advisory_envelope(Some("REACH".into()), None).as_deref(),
            Some("\nREACH\n\n")
        );
        assert_eq!(
            advisory_envelope(None, Some("UPDATE".into())).as_deref(),
            Some("\nUPDATE\n\n")
        );
        assert_eq!(
            advisory_envelope(Some("REACH".into()), Some("UPDATE".into())).as_deref(),
            Some("\nREACH\n\n\nUPDATE\n\n"),
            "reachability first; exactly two blank lines between; one hold-worthy group"
        );
    }

    /// The hold's admission rule: both streams must be a person's terminal,
    /// and a terminal that disclaimed control sequences gets no countdown.
    #[test]
    fn the_hold_needs_two_ttys_and_a_capable_terminal() {
        assert!(hold_permitted(true, true, Some("xterm-256color")));
        assert!(hold_permitted(true, true, None));
        assert!(!hold_permitted(false, true, Some("xterm")));
        assert!(!hold_permitted(true, false, Some("xterm")));
        assert!(!hold_permitted(true, true, Some("dumb")));
    }

    /// Every asciify mapping, and the guarantee that matters: no state's
    /// ASCII rendering carries a single non-ASCII byte, an escape, a
    /// trailing space, or a line at 80 columns or more.
    #[test]
    fn ascii_renderings_are_pure_for_every_reachability_state() {
        assert_eq!(
            asciify("a \u{2014} b \u{2192} c \u{b7} d \u{2026}"),
            "a -- b -> c : d ..."
        );

        use update_check::Style;
        use TailscaleSignal::*;
        let all = [
            phone_reachability_note("127.0.0.1", Some("127.0.0.1"), Up, Style::Ascii),
            phone_reachability_note("127.0.0.1", Some("127.0.0.1"), NotInstalled, Style::Ascii),
            phone_reachability_note("mac.tailnet.ts.net", Some("100.64.0.7"), Down, Style::Ascii),
        ];
        for note in all.into_iter().flatten() {
            assert!(note.is_ascii(), "non-ascii survived: {note}");
            assert!(!note.contains('\u{1b}'));
            for line in note.lines() {
                assert!(line.len() < 80, "over 80 cols: {line}");
                assert_eq!(line, line.trim_end(), "trailing space: {line:?}");
            }
        }
    }

    /// The toggled-off state's ASCII grammar: a heading, the explanation, the
    /// command isolated on its own line, what the daemon does about the toggle,
    /// and the assurance pair.
    ///
    /// Asserted as the parts that have to be there, not as one frozen string.
    /// The sentence this note carries about restarting rests on a single
    /// observation of a single Mac, and a test that pinned it word for word
    /// would make the claim harder to withdraw than to keep — which is how
    /// prose outlives the evidence for it. The structure is what the reader
    /// needs; the wording stays free to become more honest.
    #[test]
    fn the_toggled_off_note_reads_as_the_advisory_grammar() {
        let note = phone_reachability_note(
            "mac.tailnet.ts.net",
            Some("100.64.0.7"),
            TailscaleSignal::Down,
            update_check::Style::Ascii,
        )
        .unwrap();
        let mut lines = note.lines();
        assert_eq!(lines.next(), Some("Phone unreachable"));
        assert_eq!(lines.next(), Some("Tailscale is off on this Mac."));
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some("Connect Tailscale (menu bar), or run:"));
        assert_eq!(lines.next(), Some(""));
        assert_eq!(
            lines.next(),
            Some("tailscale up"),
            "the command has to stand on its own line to be copyable"
        );
        assert!(
            note.contains("This session is unaffected"),
            "the assurance pair closes the note: {note}"
        );

        // The claim itself, held to what has been watched happen. The daemon
        // keeping its bind is a property of the socket; a phone reaching it
        // again on every Mac and every toggle is not something one observation
        // establishes, so the note must not put it as a certainty.
        assert!(
            note.contains("holds its tailnet address"),
            "what the daemon does is the part that is known: {note}"
        );
        assert!(
            !note.contains("Nothing needs restarting"),
            "one measurement of one Mac does not settle every Mac: {note}"
        );
        assert!(
            note.contains("restart the daemon"),
            "the reader needs the way out when the standing bind does not \
             revive: {note}"
        );
    }

    /// The two inputs are not interchangeable, which is the whole reason both
    /// are taken. A daemon is advertised under a name whenever it has one that
    /// reaches its own listener, so the host settles nothing about where the
    /// socket is bound — and the advice that only a tailnet bind can keep is
    /// therefore read off the bind alone.
    #[test]
    fn the_toggled_off_advice_reads_the_bind_and_never_the_advertised_host() {
        use update_check::Style;
        use TailscaleSignal::Down;

        // A tailnet bind behind a hostname: the advice is owed, and the host
        // is no help in working that out.
        assert!(
            phone_reachability_note("mac.tailnet.ts.net", Some("100.64.0.7"), Down, Style::Ascii)
                .is_some(),
            "a daemon on a tailnet address revives with the tunnel however it \
             happens to be advertised"
        );
        // The same name over a bind that is not the tailnet's. Nothing here
        // comes back with `tailscale up`, so nothing is promised.
        assert_eq!(
            phone_reachability_note(
                "mac.tailnet.ts.net",
                Some("192.168.1.10"),
                Down,
                Style::Ascii
            ),
            None,
            "advice that only a tailnet bind can keep must not follow a name"
        );
        // And a daemon too old to report its bind gets silence: an absent
        // bind is not evidence of a tailnet one.
        assert_eq!(
            phone_reachability_note("mac.tailnet.ts.net", None, Down, Style::Ascii),
            None,
            "an unreported bind is not a tailnet bind"
        );
        // The host is still read for the one thing it does settle on its own:
        // an advertised address no phone can reach is unreachable whatever the
        // socket is bound to.
        assert!(
            phone_reachability_note("127.0.0.1", Some("100.64.0.7"), Down, Style::Ascii).is_some(),
            "a loopback advertisement is unreachable on its own terms"
        );
    }

    /// The pre-attach warning across its states, in the shared advisory
    /// grammar: one heading, the state's own explanation, the state's own
    /// remedy with its command isolated, the assurance — and each state
    /// names *its* fix, because they differ.
    #[test]
    fn the_phone_reachability_note_matches_the_apps_vocabulary() {
        use update_check::Style;
        use TailscaleSignal::*;
        let note = |host: &str, bind: Option<&str>, signal| {
            phone_reachability_note(host, bind, signal, Style::Ascii)
        };

        // Bound wrong (daemon started before the tailnet): restart required.
        let bound_wrong = note("127.0.0.1", Some("127.0.0.1"), Up).expect("loopback warns");
        assert!(bound_wrong.starts_with("Phone unreachable\n"));
        assert!(bound_wrong.contains("127.0.0.1"));
        assert!(bound_wrong.contains("Connect Tailscale"));
        assert!(bound_wrong.contains("codeconnect daemon restart"));
        assert!(
            bound_wrong.contains("unaffected"),
            "the note must say the session itself is fine: {bound_wrong}"
        );
        assert!(bound_wrong.is_ascii(), "ascii style stays ascii");

        // Tailscale absent entirely: set-up instruction, URL isolated.
        let missing = note("127.0.0.1", Some("127.0.0.1"), NotInstalled).expect("loopback warns");
        assert!(missing.contains("Set up Tailscale"));
        assert!(missing.contains("\nhttps://tailscale.com/download\n"));

        // Bound to the tailnet, backend stopped afterwards: connecting it back
        // is the first thing to try, because the daemon holds its bind across
        // the toggle. That the phone then reaches it on every Mac is a stronger
        // claim than the one observation behind it, so the note offers the
        // restart as a fallback rather than ruling it out.
        let toggled_off =
            note("mac.tailnet.ts.net", Some("100.64.0.7"), Down).expect("dead tailnet warns");
        assert!(toggled_off.contains("Tailscale is off on this Mac"));
        assert!(toggled_off.contains("holds its tailnet address"));
        assert!(toggled_off.contains("\ntailscale up\n"));
        assert!(
            toggled_off.contains("restart the daemon"),
            "the standing bind is the expectation, not a guarantee: {toggled_off}"
        );

        // Styled: the heading carries the impairment colour; the command is
        // bold; the ascii variant carries neither.
        let styled = phone_reachability_note(
            "mac.tailnet.ts.net",
            Some("100.64.0.7"),
            Down,
            Style::Styled,
        )
        .unwrap();
        assert!(styled.starts_with("\u{1b}[1;33mPhone unreachable\u{1b}[0m\n"));
        assert!(styled.contains("\u{1b}[1mtailscale up\u{1b}[0m"));

        // Silence, each for its own reason.
        assert_eq!(
            note("mac.tailnet.ts.net", Some("100.64.0.7"), Up),
            None,
            "healthy earns silence"
        );
        assert_eq!(
            note("mac.tailnet.ts.net", Some("192.168.1.20"), Down),
            None,
            "an operator's explicit LAN bind gets no Tailscale advice"
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

    // --------------------------------------------- adversarial: the diversion
    //
    // Written against the diversion rather than with it: each of these is an
    // attempt to make it swallow something it has no business swallowing, or
    // to make it let go of something it has to catch.

    /// Every command word `main`'s dispatch still answers. Enumerated so the
    /// scoping test below fails the moment an arm is added without a decision
    /// about whether the flag scan reaches it.
    const LIVE_COMMANDS: &[&str] = &[
        "claude",
        "codex",
        "attach",
        "ls",
        "list",
        "sessions",
        "token",
        "pair",
        "devices",
        "revoke",
        "daemon",
        "supervise",
        "__update-check",
        "update",
        "--version",
        "version",
        "help",
        "--help",
        "-h",
    ];

    /// The scan reaches exactly two command words and no others.
    ///
    /// `--ssh` under `claude` is `claude`'s, and under `supervise` is the
    /// supervisor's. Widening the scan by a single arm would let this dispatch
    /// eat an argument meant for another program — so the scoping is asserted
    /// against the whole dispatch table, not against a sample of it.
    #[test]
    fn only_pair_and_revoke_are_scanned_for_the_flag() {
        for command in LIVE_COMMANDS {
            assert_eq!(
                retired_command(command, &argv(&["--ssh"])).is_some(),
                matches!(*command, "pair" | "revoke"),
                "`codeconnect {command} --ssh` is scoped wrong"
            );
        }
        // And with the flag buried in an argv shaped like real passthrough use,
        // including one that names the flag inside a prompt.
        assert_eq!(
            retired_command(
                "claude",
                &argv(&[
                    "--model",
                    "opus",
                    "--ssh",
                    "-p",
                    "why did --ssh stop working"
                ]),
            ),
            None,
            "claude owns every one of its own arguments"
        );
        assert_eq!(
            retired_command("supervise", &argv(&["--session", "cc1", "--cwd", "--ssh"])),
            None,
            "the supervisor owns its own arguments"
        );
    }

    /// The flag is matched whole and exactly. Everything that merely resembles
    /// it is a mistyped option, and has to keep falling through to the refusal
    /// that was already there rather than collecting an explanation it has not
    /// earned.
    #[test]
    fn only_the_exact_flag_and_the_exact_command_word_divert() {
        for near in [
            "--sshh",
            "-ssh",
            "ssh",
            "--SSH",
            "--Ssh",
            "--ssh=1",
            "--ssh=true",
            "--no-ssh",
            "---ssh",
            "--ssh ",
            " --ssh",
            // A Cyrillic dze where the first `s` belongs: identical on screen,
            // a different string, and not this command.
            "--\u{0455}sh",
        ] {
            assert_eq!(
                retired_command("pair", &argv(&[near])),
                None,
                "`codeconnect pair {near}` is a mistyped option, not a retired one"
            );
            assert_eq!(
                retired_command("revoke", &argv(&["iPhone", near])),
                None,
                "`codeconnect revoke iPhone {near}` is a mistyped option"
            );
        }

        for near in [
            "sshrevoke",
            "ssh_revoke",
            "ssh-revoke-all",
            "SSH-REVOKE",
            "Ssh-Revoke",
            "ssh-revoke ",
            "ssh",
            "ssh-install",
            "ssh-pair",
            "revoke-ssh",
        ] {
            assert_eq!(
                retired_command(near, &argv(&[])),
                None,
                "`codeconnect {near}` is unknown, not retired"
            );
        }
    }

    /// The flag on its own is not a command. `codeconnect --ssh` names nothing,
    /// and must land on the refusal every unknown command word lands on —
    /// which is the rule that exits non-zero.
    #[test]
    fn the_flag_alone_is_not_a_retired_command() {
        assert_eq!(retired_command("--ssh", &argv(&[])), None);
        assert_eq!(retired_command("--ssh", &argv(&["pair"])), None);
        // What `main` substitutes for an empty argv.
        assert_eq!(retired_command("help", &argv(&[])), None);
    }

    /// Position is irrelevant under the two commands that are scanned: the
    /// reader is retyping from a phone screen and may put the flag anywhere,
    /// and may add to the line. `ssh-revoke` is the command word itself, so it
    /// diverts with whatever follows — including the device it once took.
    #[test]
    fn the_flag_is_found_at_every_position_under_the_scanned_commands() {
        for slot in 0..5 {
            let mut words: Vec<String> = (0..5).map(|i| format!("arg{i}")).collect();
            words[slot] = "--ssh".to_string();
            assert_eq!(
                retired_command("pair", &words),
                Some(RetiredCommand::SshPairing),
                "position {slot}: {words:?}"
            );
            assert_eq!(
                retired_command("revoke", &words),
                Some(RetiredCommand::SshRevocation),
                "position {slot}: {words:?}"
            );
        }

        for words in [
            argv(&[]),
            argv(&["iPhone"]),
            argv(&["--help"]),
            argv(&["iPhone", "--ssh"]),
        ] {
            assert_eq!(
                retired_command("ssh-revoke", &words),
                Some(RetiredCommand::SshRevocation),
                "`codeconnect ssh-revoke {words:?}`"
            );
        }
    }

    /// The judgement call, pinned so a later change to it is deliberate:
    /// `codeconnect revoke --ssh` with no device is explained rather than
    /// looked up. Without the diversion `--ssh` *is* the device argument —
    /// `revoke` reads only the first — and the reader gets `no device matches
    /// "--ssh"`, which explains nothing about why their phone asked. No device
    /// is ever named `--ssh`, so nothing legitimate is taken from anyone.
    #[test]
    fn a_deviceless_ssh_revoke_is_explained_rather_than_looked_up() {
        assert_eq!(
            retired_command("revoke", &argv(&["--ssh"])),
            Some(RetiredCommand::SshRevocation)
        );
    }

    /// The status a retired command leaves with is the one an unperformed
    /// command has to leave with: non-zero, and the same `1` the unknown-command
    /// refusal gets from anyhow's `Termination`. A script that runs
    /// `codeconnect pair --ssh` and reads success would be reading a pairing
    /// that never happened.
    #[test]
    fn a_retired_command_never_reports_success() {
        assert_ne!(RETIRED_EXIT_CODE, 0, "nothing was performed");
        assert_eq!(RETIRED_EXIT_CODE, 1);
    }

    /// The third style, which the shipped tests left unpinned. `NO_COLOR` on a
    /// real terminal disables SGR and says nothing about characters, so the
    /// notice must carry no escape at all while keeping its typography.
    #[test]
    fn the_notice_under_no_color_drops_every_escape_and_keeps_its_typography() {
        for retired in [RetiredCommand::SshPairing, RetiredCommand::SshRevocation] {
            let plain = retirement_notice(retired, update_check::Style::PlainUnicode);
            assert!(
                !plain.contains('\u{1b}'),
                "an escape survived NO_COLOR: {plain}"
            );
            assert!(
                plain.contains('\u{2014}'),
                "NO_COLOR disables colour, not characters: {plain}"
            );
            for line in plain.lines() {
                assert_eq!(line, line.trim_end(), "trailing space: {line:?}");
                assert!(line.chars().count() < 80, "over 80 cols: {line}");
            }
        }
    }

    /// **The invariant the notice tests exist to hold: the notice may say the
    /// sweep runs, and may never say the file is now clean.**
    ///
    /// `ccd`'s `legacy_credentials::purge_authorized_keys` is best-effort by
    /// contract. It removes nothing and warns when there is no absolute `$HOME`,
    /// when the file cannot be read, when the replacement cannot be written, when
    /// the file changed underneath the sweep, and when a tagged line is not the
    /// marker-and-key pair earlier releases wrote.
    /// Two callers reach it — startup and `Daemon::revoke` — and a second best
    /// effort is still a best effort. This CLI also answers on a Mac whose
    /// upgraded daemon has never run and, being pure, reads neither that file nor
    /// that log. So the notice states what the daemon *does* and hands over the
    /// `grep`; a reader who is told the removal already happened has been given a
    /// reassurance nothing in this tree can support, and the one they are
    /// likeliest to act on.
    ///
    /// This used to be defended by a few hundred lines that parsed the notice —
    /// and `mac/README.md` — into sentences and swept them for a blocklist of
    /// words. It did not catch the false "CodeConnect neither installs an SSH key
    /// nor revokes one"; it whitelisted that exact sentence as a fix. What
    /// replaces it is a golden snapshot per notice, so any rewording lands in a
    /// diff a human has to look at, plus literal assertions for the facts a
    /// reader is owed. Neither can be argued with, and neither certifies prose it
    /// never understood.
    #[test]
    fn the_notice_never_reports_the_removal_as_done() {
        // Each of these was, or is one keystroke from, a sentence claiming an
        // outcome nothing here observed. Literal and short on purpose: this is
        // an assertion about wording that shipped, not a theory of English.
        const NEVER: &[&str] = &[
            "already",
            "no longer",
            "nothing left",
            "is clean",
            "has been removed",
            "was removed",
            "were removed",
            "removed it",
            "nothing to revoke",
            "no key",
            "cannot log in",
            "will refuse",
        ];
        for retired in [RetiredCommand::SshPairing, RetiredCommand::SshRevocation] {
            for style in [
                update_check::Style::Ascii,
                update_check::Style::PlainUnicode,
                update_check::Style::Styled,
            ] {
                let notice = retirement_notice(retired, style).to_lowercase();
                for claim in NEVER {
                    assert!(
                        !notice.contains(claim),
                        "{claim:?} reports an outcome nothing here observed; the \
                         reader gets the check to run instead: {notice}"
                    );
                }
            }
        }
    }

    /// **The golden each notice is held to, byte for byte.**
    ///
    /// A snapshot rather than a property. Any edit to this wording — a
    /// tightening, a fact added, a hedge dropped — lands in this test's diff
    /// beside the notice's, where a human has to read both and decide the new
    /// sentence is true. That is the whole mechanism, and it is the one thing
    /// the parser this replaced could not be: prose is checked by a reader, and
    /// what a test can do is guarantee a reader is asked.
    ///
    /// `PlainUnicode` because it is the style with neither escapes nor
    /// transliteration, so what is pinned is the wording itself. The other two
    /// styles are pinned as renderings of it by
    /// [`the_notice_under_no_color_drops_every_escape_and_keeps_its_typography`]
    /// and [`the_notice_only_ever_offers_commands_that_still_exist`].
    #[test]
    fn the_pairing_notice_reads_exactly_this() {
        assert_eq!(
            retirement_notice(
                RetiredCommand::SshPairing,
                update_check::Style::PlainUnicode
            ),
            concat!(
                "codeconnect pair --ssh is retired\n",
                "CodeConnect does not use SSH. The Terminal tab rides the same paired\n",
                "connection as the rest of the app, so CodeConnect never installs an SSH\n",
                "key — and it does take one back. At startup the daemon removes the\n",
                "entries an earlier release wrote into ~/.ssh/authorized_keys, and it\n",
                "sweeps that file again on every revocation. It is best-effort either\n",
                "way, and warns in its log when it cannot.\n",
                "\n",
                "The phone that asked for this is running an older version of the app.\n",
                "Update the app: its Terminal tab then connects over the paired link\n",
                "with nothing to authorise.\n",
                "\n",
                "Pairing a phone is unchanged:\n",
                "\n",
                "codeconnect pair\n",
                "\n",
                "To see what is still in that file on this Mac, search for the tag\n",
                "those entries carry:\n",
                "\n",
                "grep codeconnect: ~/.ssh/authorized_keys\n",
                "\n",
                "Nothing printed means nothing there carries the tag; grep saying\n",
                "there is no such file means the same thing. What it prints is one of\n",
                "ours where two adjacent lines carry the same tag: a marker comment\n",
                "beginning # codeconnect:<id>, and directly beneath it an ssh-ed25519\n",
                "line whose last field is that same tag. Delete that pair by hand.\n",
                "A tagged key line with no such marker directly above it is one this\n",
                "daemon leaves alone, because nothing in the file identifies whose it\n",
                "is — removing it is your call rather than its.\n",
                "\n",
                "Nothing you typed was wrong — the app on the phone is what is out of date.",
            )
        );
    }

    /// The golden for the other answer. See
    /// [`the_pairing_notice_reads_exactly_this`] for why this is a snapshot.
    #[test]
    fn the_revocation_notice_reads_exactly_this() {
        assert_eq!(
            retirement_notice(
                RetiredCommand::SshRevocation,
                update_check::Style::PlainUnicode
            ),
            concat!(
                "codeconnect ssh-revoke is retired\n",
                "CodeConnect does not use SSH. The Terminal tab rides the same paired\n",
                "connection as the rest of the app, so CodeConnect never installs an SSH\n",
                "key — and it does take one back. At startup the daemon removes the\n",
                "entries an earlier release wrote into ~/.ssh/authorized_keys, and it\n",
                "sweeps that file again on every revocation. It is best-effort either\n",
                "way, and warns in its log when it cannot.\n",
                "\n",
                "The phone that asked for this is running an older version of the app.\n",
                "Update the app: its Terminal tab then rides the paired link, and\n",
                "nothing in CodeConnect has a use for the key it holds. The check\n",
                "below shows what is still in that file.\n",
                "\n",
                "To stop a phone reaching CodeConnect, revoke its device token:\n",
                "\n",
                "codeconnect devices\n",
                "codeconnect revoke <device>\n",
                "\n",
                "Revoking also asks the daemon to sweep ~/.ssh/authorized_keys\n",
                "again, on the same best effort it makes at startup.\n",
                "\n",
                "To see what is still in that file on this Mac, search for the tag\n",
                "those entries carry:\n",
                "\n",
                "grep codeconnect: ~/.ssh/authorized_keys\n",
                "\n",
                "Nothing printed means nothing there carries the tag; grep saying\n",
                "there is no such file means the same thing. What it prints is one of\n",
                "ours where two adjacent lines carry the same tag: a marker comment\n",
                "beginning # codeconnect:<id>, and directly beneath it an ssh-ed25519\n",
                "line whose last field is that same tag. Delete that pair by hand.\n",
                "A tagged key line with no such marker directly above it is one this\n",
                "daemon leaves alone, because nothing in the file identifies whose it\n",
                "is — removing it is your call rather than its.\n",
                "\n",
                "Nothing you typed was wrong — the app on the phone is what is out of date.",
            )
        );
    }

    /// SSH is retired, so the list of what this CLI does must not offer it.
    ///
    /// A banner is the text nobody rereads, which is how a line reinstating
    /// `pair --ssh` would ship green.
    #[test]
    fn the_usage_banner_offers_no_ssh() {
        assert!(
            !usage_text().to_lowercase().contains("ssh"),
            "SSH is retired, so the list of what this CLI does must not offer \
             it: {}",
            usage_text()
        );
    }

    /// A notice that sent its reader to a command this binary does not have
    /// would replace one dead end with another. Every `codeconnect …` line the
    /// remedy isolates has to name a live dispatch arm, and none of them may be
    /// a retired spelling — the heading is the only place a retired spelling
    /// belongs, because there it is the thing being explained.
    #[test]
    fn the_notice_only_ever_offers_commands_that_still_exist() {
        for retired in [RetiredCommand::SshPairing, RetiredCommand::SshRevocation] {
            let notice = retirement_notice(retired, update_check::Style::Ascii);
            let mut offered = 0;
            for line in notice.lines().skip(1) {
                let Some(rest) = line.strip_prefix("codeconnect ") else {
                    continue;
                };
                offered += 1;
                let word = rest.split_whitespace().next().unwrap_or_default();
                assert!(
                    LIVE_COMMANDS.contains(&word),
                    "the notice offers `codeconnect {word}`, which this binary does not have"
                );
                assert!(
                    !line.contains("--ssh") && !line.contains("ssh-revoke"),
                    "a retired spelling must never be the remedy: {line}"
                );
            }
            assert!(
                offered > 0,
                "a remedy with no command to run is not a remedy: {notice}"
            );
        }
    }
}
