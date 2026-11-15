//! A Claude run's agent host, from ccd's side (P0 §2.1 H1-H3).
//!
//! ccd writes the run's row first, with the app's start request id, so a repeated
//! request finds the run instead of starting another. Then it makes the run's folder
//! `<root>/runs/<uid>/` (0700) and spawns `cc-agent-host <run dir>` through the login
//! shell, in a session of its own so it outlives ccd. The host writes `host.json`; ccd
//! admits the pid in it only if it is the child ccd spawned, and records it with the
//! birth time the kernel gives it. ccd then attaches over `host.sock`, commits the
//! host's first `state` before acknowledging it, and sends `start`. The connection
//! stays with a link task that commits and acknowledges each message in sequence
//! order and stops at the first one it does not apply, which the host keeps, so
//! nothing is acknowledged that was not stored. The start's outcome, committed by the
//! link, makes the run `live` or `failed`, and the app's answer waits for it. A restarted
//! ccd attaches each host still running again from its run's cursor ([`attach_run`]). An
//! app's message to a live run goes to its host as `say` ([`send_text`]), and the
//! person's words are filed once the host has taken them; its command menu is what its
//! claude last reported ([`catalog`]), and an app changes its model, effort and mode
//! ([`set_setting`]). An
//! app ends a live run ([`stop_run`]), a Codex run on the engine as well: it is `stopping`,
//! its link takes the graceful step if it is attached (a Claude host is sent `stop`), and a
//! host that has not exited 10 s later is stopped by its group ([`escalate_stop`]).
//!
//! The lines are [`protocol::host`]'s; the host is mac/agent-host.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::agent::AgentKind;
use protocol::config::Config;
use protocol::event::{EventKind, Lifecycle, PendingEvent, SessionKey, Source};
use protocol::event::{MenuMode, MenuModel, RunMenus};
use protocol::host::{
    FromHost, HostFile, McpCommand, OutcomeStatus, StartOptions, ToHost, Trust,
    HOST_PROTOCOL_VERSION,
};
use protocol::proc_identity::{
    group_has_members, group_warrant, is_zombie, liveness, read_birth_identity, BootIdentity,
    GroupWarrant, Liveness, ProcessIdentity,
};
use protocol::sdk_map::Signal;
use protocol::ws::{
    AnswerDecision, AnswerOutcome, AnswerPath, AnswerResult, AnsweredOn, Applies,
    CommandCatalogResult, CommandEntry, ResolvedBy, SendTextResult, SettingResult, StartFailure,
    StartSessionResult, StopRunResult,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::codex_link::InterruptReport;
use crate::state::Daemon;
use crate::store::{
    AnswerClaim, HostedCommands, HostedRun, MutationClaimRow, RunNotes, SessionRow, TextClaim,
};

/// The agent host's file name, beside ccd's own executable.
const HOST_BIN: &str = "cc-agent-host";

/// How often the start looks for `host.json` and the socket.
const POLL: Duration = Duration::from_millis(50);

/// How long a dead host may stay an unreaped zombie while ccd waits to signal its group. On
/// macOS a zombie's birth cannot be read, so its group is warranted only once its pid is free;
/// Linux reads a zombie's birth, and the warrant passes at once.
const ZOMBIE_WAIT: Duration = Duration::from_secs(5);

/// How long a dead host's group has after SIGTERM before SIGKILL (decision 179).
const TERM_GRACE: Duration = Duration::from_secs(5);

/// How long a run sent `stop` has to exit before its host's group is stopped (H5).
const STOP_GRACE: Duration = Duration::from_secs(10);

/// The end of a run that was `stopping`, however it ended. It carries no exit code: the
/// person asked for the stop, so claude's code (1 when it was mid-turn) says nothing.
fn stopped_end() -> serde_json::Value {
    serde_json::json!({ "exit_code": null, "reason": "Stopped" })
}

/// One Claude start, as the app asked for it and the checks before the row resolved it.
pub(crate) struct Run<'a> {
    /// This daemon's state folder, which holds `runs/`.
    pub root: &'a Path,
    pub request_id: &'a str,
    /// The folder the run starts in.
    pub cwd: &'a str,
    pub prompt: &'a str,
    /// The app's model. Claude Code's own `default` is sent as none, so the run starts
    /// with the model its settings name.
    pub model: Option<&'a str>,
    /// A new worktree for claude to make and start in, when the app asked for one.
    pub worktree: Option<&'a str>,
    /// The answer to "trust this folder?" the run starts with: the one the start resolved
    /// for the folder it names, lowered to not trusted when the folder the run runs in has
    /// its own `not_trusted` answer. Trusted, claude loads the project it runs in; not
    /// trusted, only the person's own settings and CodeConnect's MCP server (decision
    /// 157, the host's `queryOptions`).
    pub trust: Trust,
    /// The claude the start's check found.
    pub claude_path: &'a str,
    /// The agent host to spawn.
    pub host_bin: &'a Path,
    /// The `codeconnect` binary, which runs the run's mail server.
    pub codeconnect: &'a Path,
    /// The start's environment, the one its claude check read: the host and claude
    /// get it as it is, with only `CODECONNECT_SESSION_UID` added.
    pub environment: &'a HashMap<OsString, OsString>,
}

/// The run's folder: `<root>/runs/<uid>/`.
pub(crate) fn run_dir(root: &Path, uid: &str) -> PathBuf {
    root.join("runs").join(uid)
}

/// The agent host ccd spawns: `agent_host_bin` when the config names one, else the one
/// beside ccd's own executable, where the Mac app installs it.
pub(crate) fn host_binary(config: &Config) -> Result<PathBuf, String> {
    if let Some(bin) = &config.agent_host_bin {
        return Ok(PathBuf::from(bin));
    }
    std::env::current_exe()
        .map(|exe| exe.with_file_name(HOST_BIN))
        .map_err(|err| format!("could not find this daemon's own executable: {err}"))
}

/// The answer for a start whose request id already has a run (H1): that run.
pub(crate) fn answer_for(run: HostedRun) -> StartSessionResult {
    match run.lifecycle {
        Lifecycle::Failed => failed("did not start".into()),
        _ => StartSessionResult::Started {
            session_uid: run.session_uid,
            cwd: Some(run.cwd),
        },
    }
}

/// What became of a run's `start`, as its link committed it: `Ok` once the start is
/// done, or why it did not start (the host's refusal, or the link ending first).
type Begun = Result<(), String>;

/// Start `run`: its row, its folder, its host, the first attach and `start`, then the
/// answer from the start's outcome.
pub(crate) async fn start(daemon: &Arc<Daemon>, run: Run<'_>) -> StartSessionResult {
    start_within(daemon, run, crate::start_session::START_TIMEOUT).await
}

/// [`start`], with `timeout` for all of it, from the row to the outcome: the tests'
/// way to a deadline shorter than START_TIMEOUT.
async fn start_within(daemon: &Arc<Daemon>, run: Run<'_>, timeout: Duration) -> StartSessionResult {
    // The run and its first message are named together, before the row: the start's
    // uuid is the one claude stamps on that message.
    let (uid, start_uuid) =
        match protocol::uid::new().and_then(|uid| Ok((uid, protocol::uid::uuid_v4()?))) {
            Ok(names) => names,
            Err(err) => return failed(format!("could not name the run: {err}")),
        };
    let start = ToHost::Start {
        uuid: start_uuid.clone(),
        options: start_options(&uid, &run),
        prompt: run.prompt.to_string(),
    };
    let line = match serde_json::to_string(&start) {
        Ok(line) => line,
        Err(err) => return failed(format!("could not write the run's start: {err}")),
    };
    let now = protocol::time::now_rfc3339();
    let row = SessionRow {
        session_uid: uid.clone(),
        session_id: uid.clone(),
        tmux_session: String::new(),
        tmux_socket: String::new(),
        cwd: run.cwd.to_string(),
        claude_session_id: None,
        transcript_path: None,
        lifecycle: Lifecycle::Starting,
        created_at: now.clone(),
        updated_at: now,
        agent: AgentKind::Claude,
        codex_thread_id: None,
        codex_socket: None,
    };
    match daemon
        .db
        .begin_run(row, run.request_id.to_string(), Some(line))
        .await
    {
        Ok(Some(existing)) => return answer_for(existing),
        Ok(None) => {}
        Err(err) => return failed(format!("could not record the run: {err:#}")),
    }
    let deadline = Instant::now() + timeout;
    let begun = match bring_up(daemon, &uid, &start_uuid, &start, &run, deadline).await {
        Ok(begun) => begun,
        // Before `start` no claude can exist (the host exits by itself at 30 s), so the
        // run has failed.
        Err(detail) => {
            if let Err(err) = daemon
                .db
                .set_lifecycle(uid.clone(), Lifecycle::Failed)
                .await
            {
                crate::log_error!("could not mark {uid} failed: {err:#}");
            }
            return failed(detail);
        }
    };
    // From `start` on, claude may be running, so only the outcome the link commits, or the
    // host's end, ends the run: a link that ends first, its host still running, leaves it
    // `starting`, protected from deletion.
    let started = StartSessionResult::Started {
        session_uid: uid,
        // A start in a new worktree does not know its folder until claude reports it.
        cwd: run.worktree.is_none().then(|| run.cwd.to_string()),
    };
    match tokio::time::timeout_at(deadline, begun).await {
        Ok(Ok(Ok(()))) => started,
        Ok(Ok(Err(detail))) => failed(detail),
        Ok(Err(_)) => failed("the agent host's link ended".into()),
        // No outcome yet. The run exists and is starting, and its link keeps applying
        // what comes, so a late outcome still makes it `live` or `failed`.
        Err(_) => started,
    }
}

fn failed(detail: String) -> StartSessionResult {
    StartSessionResult::Failed {
        reason: StartFailure::LaunchFailed,
        detail,
    }
}

/// The run's folder, its host spawned and admitted, attached, and `start` sent, by
/// `deadline`: then what the link will commit of the start's outcome.
async fn bring_up(
    daemon: &Arc<Daemon>,
    uid: &str,
    start_uuid: &str,
    start: &ToHost,
    run: &Run<'_>,
    deadline: Instant,
) -> Result<oneshot::Receiver<Begun>, String> {
    let dir = run_dir(run.root, uid);
    let log = crate::start_session::make_run_dir(run.root, &dir)
        .map_err(|err| format!("could not make the run's folder {}: {err}", dir.display()))?;
    let mut child = crate::start_session::spawn_detached(
        run.environment,
        &[run.host_bin.as_os_str(), dir.as_os_str()],
        Path::new(run.cwd),
        uid,
        log,
    )
    .map_err(|err| format!("could not run the login shell: {err}"))?;
    let spawned = child.id();
    let attached = attach(
        daemon, uid, start_uuid, run.prompt, start, &dir, spawned, &mut child, deadline,
    )
    .await;
    crate::start_session::reap(child);
    attached
}

#[allow(clippy::too_many_arguments)]
async fn attach(
    daemon: &Arc<Daemon>,
    uid: &str,
    start_uuid: &str,
    start_prompt: &str,
    start: &ToHost,
    dir: &Path,
    spawned: Option<u32>,
    child: &mut tokio::process::Child,
    deadline: Instant,
) -> Result<oneshot::Receiver<Begun>, String> {
    let record = loop {
        if let Some(record) = read_host_file(&dir.join("host.json")) {
            break record;
        }
        crate::start_session::give_up_if_ended(
            dir,
            child,
            deadline,
            "the agent host did not write host.json",
        )?;
        tokio::time::sleep(POLL).await;
    };
    if record.protocol_version != HOST_PROTOCOL_VERSION {
        return Err(format!(
            "the agent host speaks version {} of its protocol, and this CodeConnect speaks {}",
            record.protocol_version, HOST_PROTOCOL_VERSION
        ));
    }
    if u32::try_from(record.host_pid).ok() != spawned {
        return Err("host.json names a process this daemon did not start".into());
    }
    let host = ProcessIdentity {
        pid: record.host_pid,
        birth: read_birth_identity(record.host_pid)
            .ok_or("could not read the agent host's start time")?,
    };
    daemon
        .db
        .record_host(
            uid.to_string(),
            host,
            protocol::proc_identity::boot_identity(),
        )
        .await
        .map_err(|err| format!("could not record the agent host: {err:#}"))?;

    let stream = connect(&dir.join("host.sock"), || {
        crate::start_session::give_up_if_ended(
            dir,
            child,
            deadline,
            "the agent host did not listen",
        )
    })
    .await?;
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    send(&mut write, &ToHost::Attach { after: 0 }).await?;
    // A new host keeps nothing yet, so the first line it sends is its state.
    let first = tokio::time::timeout_at(deadline, lines.next_line())
        .await
        .map_err(|_| "the agent host sent nothing after the attach".to_string())?
        .map_err(|err| format!("could not read the agent host: {err}"))?
        .ok_or("the agent host closed the connection")?;
    match serde_json::from_str::<FromHost>(&first) {
        Ok(FromHost::State { seq, .. }) => commit_and_ack(daemon, uid, host, seq, &mut write)
            .await
            .map_err(|why| format!("could not commit the agent host's state: {why}"))?,
        _ => return Err("the agent host's first message was not its state".into()),
    }
    send(&mut write, start).await?;
    // The link is a task of its own, so the start's answer timing out never ends it.
    let (begun, outcome) = oneshot::channel();
    tokio::spawn(link(
        Arc::clone(daemon),
        uid.to_string(),
        host,
        Linked {
            start_uuid: start_uuid.to_string(),
            start_prompt: start_prompt.to_string(),
            lifecycle: Lifecycle::Starting,
            turn: None,
            resend: None,
        },
        Some(begun),
        lines,
        write,
    ));
    Ok(outcome)
}

/// A restarted ccd's link to the run `uid`'s recorded host `host`, alive when recovery
/// found it (H8): its socket, waited for while the host is alive and not yet listening,
/// as a new host's is; then `attach` from the run's cursor and the link, which reads from
/// the store what the connection that started the run knew. A run still starting gets
/// its stored `start` at the first `state`, once the replay is over, and a run stopping
/// gets `stop` again there (H8). When it cannot connect, the run's host is checked as at
/// the link's end ([`end_if_host_gone`]).
pub(crate) async fn attach_run(
    daemon: Arc<Daemon>,
    root: PathBuf,
    uid: String,
    host: ProcessIdentity,
) {
    let linked = match daemon.db.hosted_link(uid.clone()).await {
        Ok(Some(linked)) => linked,
        Ok(None) => return,
        Err(err) => {
            crate::log_error!("{uid}: could not read its run to attach its host: {err:#}");
            return;
        }
    };
    let deadline = Instant::now() + crate::start_session::START_TIMEOUT;
    let connected = connect(&run_dir(&root, &uid).join("host.sock"), || {
        if liveness(&host) != Liveness::Alive {
            return Err("the agent host is no longer running".into());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the agent host did not listen within {} s",
                crate::start_session::START_TIMEOUT.as_secs()
            ));
        }
        Ok(())
    })
    .await;
    let attached = match connected {
        Ok(stream) => {
            let (read, mut write) = stream.into_split();
            send(
                &mut write,
                &ToHost::Attach {
                    after: linked.cursor,
                },
            )
            .await
            .map(|()| (read, write))
        }
        Err(why) => Err(why),
    };
    let (read, write) = match attached {
        Ok(halves) => halves,
        Err(why) => {
            crate::log_info!("{uid}: its agent host was not attached again: {why}");
            end_if_host_gone(&daemon, &uid).await;
            return;
        }
    };
    let start = linked
        .start
        .as_deref()
        .and_then(|line| serde_json::from_str::<ToHost>(line).ok());
    let (start_uuid, start_prompt) = match &start {
        Some(ToHost::Start { uuid, prompt, .. }) => (uuid.clone(), prompt.clone()),
        _ => (String::new(), String::new()),
    };
    let resend = match linked.lifecycle {
        Lifecycle::Starting => start,
        Lifecycle::Stopping => match protocol::uid::uuid_v4() {
            Ok(uuid) => Some(ToHost::Stop { uuid }),
            Err(err) => {
                crate::log_error!("{uid}: could not name its stop: {err}");
                None
            }
        },
        _ => None,
    };
    link(
        daemon,
        uid,
        host,
        Linked {
            start_uuid,
            start_prompt,
            lifecycle: linked.lifecycle,
            turn: linked.turn,
            resend,
        },
        None,
        BufReader::new(read).lines(),
        write,
    )
    .await;
}

/// Connect to a host's socket, waiting while it is not yet there or not yet listening,
/// until `give_up` says why not to wait any longer.
async fn connect(
    socket: &Path,
    mut give_up: impl FnMut() -> Result<(), String>,
) -> Result<UnixStream, String> {
    loop {
        match UnixStream::connect(socket).await {
            Ok(stream) => return Ok(stream),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                give_up()?;
                tokio::time::sleep(POLL).await;
            }
            Err(err) => return Err(format!("could not reach the agent host: {err}")),
        }
    }
}

/// The run's `query()` options.
fn start_options(uid: &str, run: &Run<'_>) -> StartOptions {
    StartOptions {
        cwd: run.cwd.to_string(),
        claude_path: run.claude_path.to_string(),
        // The app chooses no mode at a start, so claude picks its own: the person's
        // `defaultMode`, else its default.
        permission_mode: None,
        trust: run.trust,
        resume: None,
        model: run
            .model
            .filter(|model| *model != "default")
            .map(str::to_string),
        worktree: run.worktree.map(str::to_string),
        mail: McpCommand {
            command: run.codeconnect.to_string_lossy().into_owned(),
            args: protocol::ipc::mail_server_args(uid, &protocol::socket_path()),
        },
    }
}

/// `host.json` once it is whole: the host creates it before it writes it.
fn read_host_file(path: &Path) -> Option<HostFile> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

async fn send(write: &mut OwnedWriteHalf, message: &ToHost) -> Result<(), String> {
    let mut line = serde_json::to_string(message).map_err(|err| err.to_string())?;
    line.push('\n');
    write
        .write_all(line.as_bytes())
        .await
        .map_err(|err| format!("could not write to the agent host: {err}"))
}

/// Commit `seq` as the run's cursor, then acknowledge it: never the other way round,
/// so the host drops only what is stored (H3).
async fn commit_and_ack(
    daemon: &Daemon,
    uid: &str,
    host: ProcessIdentity,
    seq: u64,
    write: &mut OwnedWriteHalf,
) -> Result<(), String> {
    match daemon.db.commit_cursor(uid.to_string(), host, seq).await {
        Ok(true) => send(write, &ToHost::Ack { seq }).await,
        Ok(false) => Err(format!(
            "message {seq} is not past the run's cursor for this host"
        )),
        Err(err) => Err(format!("{err:#}")),
    }
}

/// Map one SDK message of a live run and file it with its cursor, then acknowledge it.
/// `turn` is the uuid of the run's latest `system/init`: an init sets it before it is
/// mapped, so the init's own events carry their turn. The run's turns are marked for the
/// mail wake rule as a terminal run's transcript marks them: a start at the main thread's
/// init, an end at its `result`, which is also the end the rule hears of
/// ([`crate::mail::turn_ended`]); ccd's clock, as a prompt's mark.
async fn apply_sdk(
    daemon: &Arc<Daemon>,
    uid: &str,
    host: ProcessIdentity,
    seq: u64,
    message: &serde_json::Value,
    turn: &mut Option<String>,
    write: &mut OwnedWriteHalf,
) -> Result<(), String> {
    let text = |key: &str| message.get(key).and_then(serde_json::Value::as_str);
    if text("type") == Some("system") && text("subtype") == Some("init") {
        if let Some(uuid) = text("uuid") {
            *turn = Some(uuid.to_string());
        }
    }
    let mapped = protocol::sdk_map::map(&SessionKey::new(uid, uid), turn.as_deref(), message);
    let mut notes = RunNotes::default();
    for signal in mapped.signals {
        match signal {
            Signal::SessionId { id } => notes.session_id = Some(id),
            Signal::Cwd { path } => notes.cwd = Some(path),
            Signal::ScreenOnly { names } => notes.screen_only = Some(names),
            Signal::Commands { commands } => notes.commands = Some(commands),
            // A send is settled by its host's outcome ([`apply_send_outcome`]), not by
            // claude's deliveries or command lifecycles; `/clear` keeps the session id,
            // which the next init names.
            Signal::Delivered { .. } | Signal::Lifecycle { .. } | Signal::Reset { .. } => {}
        }
    }
    let filed = |kind: EventKind| mapped.events.iter().any(|event| event.kind == kind);
    let (started, ended) = (
        filed(EventKind::SessionStart),
        filed(EventKind::TurnComplete),
    );
    match daemon
        .ingest_host_message(uid, host, seq, mapped.events, notes)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "message {seq} is not past the run's cursor for this host"
            ))
        }
        Err(err) => return Err(format!("{err:#}")),
    }
    if started || ended {
        let now = protocol::time::now_unix_ms();
        if let Err(err) = daemon
            .db
            .mail_turn_marks(
                uid.to_string(),
                started.then_some(now),
                ended.then_some(now),
            )
            .await
        {
            crate::log_error!("mail for {uid}: could not mark its turn: {err:#}");
        }
        if ended {
            crate::mail::turn_ended(daemon, uid);
        }
    }
    send(write, &ToHost::Ack { seq }).await
}

/// What a link knows of its run when it begins: from the start that spawned the host, or,
/// after a restart, from the store.
struct Linked {
    /// The uuid of the run's `start`, whose outcome makes the run `live` or `failed`.
    start_uuid: String,
    /// The start's prompt, the person's first words, filed when the run goes live.
    start_prompt: String,
    lifecycle: Lifecycle,
    /// The uuid of the run's last SDK init, which its turn's later events carry.
    turn: Option<String>,
    /// Sent at the link's first `state`: the run's `start` if no outcome has come by then,
    /// or `stop` for a run stopping.
    resend: Option<ToHost>,
}

/// The run's connection to its host: each message committed, then acknowledged, in
/// order. It applies the run's `state` and the outcome of the run's start, which makes
/// the run `live` or `failed` and is told to `begun` once committed; `begun` hears instead
/// why the link ended if it ends first. A `live` run's SDK messages are its events
/// ([`apply_sdk`]); the host sends none before the outcome. A `live` run's `exited` ends the
/// run (H6, `Daemon::end_hosted_run`). After a refusal claude's `exited` changes nothing
/// in the `failed` run and is applied so its host can end. After a re-attach, the host
/// replays what it kept and then sends its `state`, so the first `state` ends the replay
/// (decision 1): a run still starting then gets its start again, which a host that
/// already has one ignores (decision 2), and a run stopping gets `stop` again (H8).
///
/// While it runs, the daemon sends the host commands through it
/// ([`Daemon::send_to_host`]). A `stopping` run is still going, so its messages are
/// filed as a live run's, and its `exited` ends it "Stopped" ([`stopped_end`]). It ends at
/// the first message this daemon does not apply, which stays in the host with everything
/// after it, or when the connection or a commit fails, and then checks the run's host
/// ([`end_if_host_gone`]).
async fn link(
    daemon: Arc<Daemon>,
    uid: String,
    host: ProcessIdentity,
    linked: Linked,
    mut begun: Option<oneshot::Sender<Begun>>,
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    mut write: OwnedWriteHalf,
) {
    let (to_link, mut commands) = tokio::sync::mpsc::unbounded_channel();
    daemon
        .host_link_attached(&uid, crate::state::HostLink::Claude(to_link))
        .await;
    let Linked {
        start_uuid,
        start_prompt,
        lifecycle,
        mut turn,
        mut resend,
    } = linked;
    let (mut live, mut refused) = (
        matches!(lifecycle, Lifecycle::Live | Lifecycle::Stopping),
        lifecycle == Lifecycle::Failed,
    );
    let mut stopping = lifecycle == Lifecycle::Stopping;
    let ended = loop {
        let line = tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => line,
                Ok(None) => break "the agent host closed the connection".to_string(),
                Err(err) => break format!("could not read the agent host: {err}"),
            },
            Some(command) = commands.recv() => {
                stopping |= matches!(command, ToHost::Stop { .. });
                if let Err(why) = send(&mut write, &command).await {
                    break why;
                }
                continue;
            }
        };
        match serde_json::from_str::<FromHost>(&line) {
            Ok(FromHost::State { seq, pending, .. }) => {
                if let Err(why) = commit_and_ack(&daemon, &uid, host, seq, &mut write).await {
                    break why;
                }
                // The end of a replay: the run's cards are put right against the
                // requests the host still holds, and a live run can be woken for its
                // mail again, its replayed turns marked.
                daemon.host_cards_reconcile(&uid, &pending).await;
                if live {
                    crate::mail::waiter_registered(&daemon, &uid);
                }
                if let Some(command) = resend.take() {
                    // A start goes again only while no outcome has come.
                    let due = !matches!(command, ToHost::Start { .. }) || (!live && !refused);
                    if due {
                        if let Err(why) = send(&mut write, &command).await {
                            break why;
                        }
                    }
                }
            }
            Ok(FromHost::Outcome {
                seq,
                uuid,
                status,
                detail,
                initialize,
                ..
            }) if uuid == start_uuid => {
                let (lifecycle, said) = match status {
                    OutcomeStatus::Done => (Lifecycle::Live, Ok(())),
                    OutcomeStatus::Refused => (
                        Lifecycle::Failed,
                        Err(detail.unwrap_or_else(|| "the agent host refused the start".into())),
                    ),
                };
                let initialize = match initialize.as_ref().map(serde_json::to_string).transpose() {
                    Ok(initialize) => initialize,
                    Err(err) => {
                        break format!("could not keep the start's initialize answer: {err}")
                    }
                };
                // The person's first words, filed as the run goes live.
                let words = vec![person_row(&uid, &start_uuid, &start_prompt, None)];
                match daemon
                    .commit_start_outcome(&uid, host, seq, lifecycle, initialize, words)
                    .await
                {
                    Ok(true) => {
                        live = lifecycle == Lifecycle::Live;
                        refused = lifecycle == Lifecycle::Failed;
                        if live {
                            daemon.mail_run_live(&uid).await;
                        }
                    }
                    Ok(false) => {
                        break format!("message {seq} is not past the run's cursor for this host")
                    }
                    Err(err) => break format!("{err:#}"),
                }
                if let Err(detail) = &said {
                    crate::log_info!("{uid}: the agent host refused the start: {detail}");
                }
                // Told once committed, before the ack: a failed ack keeps what is stored.
                if let Some(begun) = begun.take() {
                    let _ = begun.send(said);
                }
                if let Err(why) = send(&mut write, &ToHost::Ack { seq }).await {
                    break why;
                }
            }
            Ok(FromHost::Outcome {
                seq,
                uuid,
                status,
                detail,
                cancelled,
                terminal_reason,
                ..
            }) if live => {
                let told = StopOutcome {
                    seq,
                    uuid: &uuid,
                    status,
                    cancelled: cancelled.as_deref(),
                    terminal_reason: terminal_reason.as_deref(),
                };
                let applied = match apply_interrupt_outcome(&daemon, &uid, host, &told, &mut write)
                    .await
                {
                    Ok(true) => Ok(()),
                    Ok(false) => match apply_setting_outcome(
                        &daemon, &uid, host, seq, &uuid, status, detail, &mut write,
                    )
                    .await
                    {
                        Ok(true) => Ok(()),
                        Ok(false) => {
                            apply_send_outcome(&daemon, &uid, host, seq, &uuid, status, &mut write)
                                .await
                        }
                        Err(why) => Err(why),
                    },
                    Err(why) => Err(why),
                };
                if let Err(why) = applied {
                    break why;
                }
            }
            Ok(FromHost::Sdk { seq, message }) if live => {
                if let Err(why) =
                    apply_sdk(&daemon, &uid, host, seq, &message, &mut turn, &mut write).await
                {
                    break why;
                }
            }
            Ok(FromHost::Exited { seq, code, signal }) if live => {
                let (from, payload) = if stopping {
                    (Lifecycle::Stopping, stopped_end())
                } else {
                    let mut payload = serde_json::json!({ "exit_code": code });
                    if let Some(signal) = signal {
                        payload["signal"] = signal.into();
                    }
                    (Lifecycle::Live, payload)
                };
                match daemon
                    .end_hosted_run(&uid, Some((host, seq)), from, Lifecycle::Exited, payload)
                    .await
                {
                    Ok(true) => {
                        if let Err(why) = send(&mut write, &ToHost::Ack { seq }).await {
                            break why;
                        }
                    }
                    Ok(false) => {
                        break format!("message {seq} is not past the run's cursor for this host")
                    }
                    Err(err) => break format!("{err:#}"),
                }
            }
            Ok(FromHost::Exited { seq, .. }) if refused => {
                if let Err(why) = commit_and_ack(&daemon, &uid, host, seq, &mut write).await {
                    break why;
                }
            }
            Ok(FromHost::Permission { seq, request }) if live => {
                match daemon.host_card_asked(&uid, host, seq, request).await {
                    Ok(true) => {
                        if let Err(why) = send(&mut write, &ToHost::Ack { seq }).await {
                            break why;
                        }
                    }
                    Ok(false) => {
                        break format!("message {seq} is not past the run's cursor for this host")
                    }
                    Err(err) => break format!("{err:#}"),
                }
            }
            Ok(FromHost::Withdrawn { seq, tool_use_id }) if live => {
                match daemon
                    .host_card_withdrawn(&uid, host, seq, &tool_use_id)
                    .await
                {
                    Ok(true) => {
                        if let Err(why) = send(&mut write, &ToHost::Ack { seq }).await {
                            break why;
                        }
                    }
                    Ok(false) => {
                        break format!("message {seq} is not past the run's cursor for this host")
                    }
                    Err(err) => break format!("{err:#}"),
                }
            }
            Ok(_) => break "a message this daemon does not apply yet; the host keeps it".into(),
            Err(err) => break format!("a line that is not a host message: {err}"),
        }
    };
    crate::log_info!("{uid}: the agent host link ended: {ended}");
    if let Some(begun) = begun.take() {
        let _ = begun.send(Err(ended));
    }
    daemon.host_link_detached(&uid).await;
    end_if_host_gone(&daemon, &uid).await;
}

/// What became of a dead host's group.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ladder {
    /// Signalled as the ladder says, or no member was left to signal.
    Done,
    /// The warrant was refused, so nothing was signalled.
    Refused(String),
    /// The host was still an unreaped zombie after [`ZOMBIE_WAIT`]: nothing was signalled yet.
    Deferred,
}

/// The ladder (decision 179) on a dead host's process group, whose id is the host's pid,
/// under the warrant (`group_warrant`): SIGTERM, then SIGKILL after [`TERM_GRACE`] only
/// while members remain and the warrant still holds. Never SIGKILL first: claude ends its
/// Bash tool's own session on SIGTERM. A host that is a zombie whose birth cannot be read (on
/// macOS) is waited out first: its group is warranted only once it is reaped and its pid free.
/// A Codex start that fails runs it on its app-server's group, whose leader may still be alive.
pub(crate) async fn stop_group(host: ProcessIdentity, boot: BootIdentity) -> Ladder {
    let waited = Instant::now() + ZOMBIE_WAIT;
    loop {
        match group_warrant(&host, boot) {
            GroupWarrant::Warranted => break,
            GroupWarrant::Refused(_) if is_zombie(host.pid) == Some(true) => {
                if Instant::now() >= waited {
                    return Ladder::Deferred;
                }
                tokio::time::sleep(POLL).await;
            }
            GroupWarrant::Refused(why) => return Ladder::Refused(why),
        }
    }
    let group = host.pid;
    if !group_has_members(group) {
        return Ladder::Done;
    }
    // SAFETY: kill(2) of the host's process group, which the warrant ties to the host.
    unsafe { libc::kill(-group, libc::SIGTERM) };
    let grace = Instant::now() + TERM_GRACE;
    while Instant::now() < grace {
        if !group_has_members(group) {
            return Ladder::Done;
        }
        tokio::time::sleep(POLL).await;
    }
    if group_has_members(group) && group_warrant(&host, boot) == GroupWarrant::Warranted {
        // SAFETY: as above, the warrant asked again just before.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    }
    Ladder::Done
}

/// One SIGTERM to the process group `host` leads, under the warrant, if it has members:
/// the graceful first step of ending a Codex run, whose app-server drains and exits.
/// Whether it was sent.
pub(crate) fn term_group(host: &ProcessIdentity, boot: BootIdentity) -> bool {
    if group_warrant(host, boot) != GroupWarrant::Warranted || !group_has_members(host.pid) {
        return false;
    }
    // SAFETY: kill(2) of the run's process group, which the warrant ties to its leader.
    unsafe { libc::kill(-host.pid, libc::SIGTERM) == 0 }
}

/// H7: when the run `uid` has not ended and its recorded host is gone, the ladder on the
/// host's group, then the run ends. `starting` → `failed`, as a refused start, with no end
/// in its timeline, which the apps would read as a session that ran and finished; `live` →
/// `exited` ("Stopped: its process ended"); `stopping` → `exited` ("Stopped"). A host
/// alive, or one the kernel cannot be asked about, leaves the run as it is; so does one
/// still a zombie after [`ZOMBIE_WAIT`], for the next sweep.
pub(crate) async fn end_if_host_gone(daemon: &Daemon, uid: &str) {
    let watch = match daemon.db.watched_host(uid.to_string()).await {
        Ok(Some(watch)) => watch,
        Ok(None) => return,
        Err(err) => {
            crate::log_error!("{uid}: could not read its host: {err:#}");
            return;
        }
    };
    if !matches!(
        watch.lifecycle,
        Lifecycle::Starting | Lifecycle::Live | Lifecycle::Stopping
    ) {
        return;
    }
    if liveness(&watch.host) != Liveness::Gone {
        return;
    }
    match watch.boot {
        Some(boot) => match stop_group(watch.host, boot).await {
            Ladder::Done => {}
            Ladder::Refused(why) => {
                crate::log_info!("{uid}: its host is gone; its group was not signalled: {why}")
            }
            Ladder::Deferred => {
                crate::log_info!(
                    "{uid}: its host is gone and not yet reaped; the next sweep tries again"
                );
                return;
            }
        },
        None => crate::log_info!(
            "{uid}: its host is gone; no boot was recorded with it, so its group was not signalled"
        ),
    }
    if watch.lifecycle == Lifecycle::Starting {
        if let Err(err) = daemon
            .db
            .set_lifecycle(uid.to_string(), Lifecycle::Failed)
            .await
        {
            crate::log_error!("{uid}: could not mark the run failed: {err:#}");
        }
        return;
    }
    let payload = if watch.lifecycle == Lifecycle::Stopping {
        stopped_end()
    } else {
        serde_json::json!({ "exit_code": null, "reason": "Stopped: its process ended" })
    };
    if let Err(err) = daemon
        .end_hosted_run(uid, None, watch.lifecycle, Lifecycle::Exited, payload)
        .await
    {
        crate::log_error!("{uid}: could not end the run: {err:#}");
    }
}

/// End the hosted run `uid` ([`protocol::ws::ClientMessage::StopRun`], H5), Claude's or
/// Codex's: a `live` run becomes `stopping`, its link takes the graceful step
/// ([`Daemon::stop_through_link`]), and [`escalate_stop`] follows. One that is stopping is
/// told so again and nothing more is sent; one that has ended is left as it is. A run still
/// starting is refused: its start's own outcome decides it. A run CodeConnect did not start
/// (no hosted row) is refused, whatever its agent.
pub(crate) async fn stop_run(daemon: &Arc<Daemon>, uid: &str) -> StopRunResult {
    let refuse = |reason: &str| StopRunResult::Refused {
        reason: reason.to_string(),
    };
    let lifecycle = match daemon.db.hosted_link(uid.to_string()).await {
        Ok(Some(linked)) => linked.lifecycle,
        Ok(None) => return refuse("CodeConnect did not start this run, so it cannot end it."),
        Err(err) => {
            crate::log_error!("{uid}: could not read its run to stop it: {err:#}");
            return refuse("This computer could not read the run.");
        }
    };
    match lifecycle {
        Lifecycle::Live => {}
        Lifecycle::Stopping => return StopRunResult::Stopping,
        Lifecycle::Exited | Lifecycle::Failed => return StopRunResult::AlreadyEnded,
        Lifecycle::Starting => return refuse("It is still starting; end it once it has started."),
        Lifecycle::Unknown => return refuse("This computer does not know whether it is running."),
    }
    match daemon
        .db
        .advance_lifecycle(uid.to_string(), Lifecycle::Live, Lifecycle::Stopping)
        .await
    {
        Ok(true) => {}
        // It moved meanwhile: another stop, or its end.
        Ok(false) => return Box::pin(stop_run(daemon, uid)).await,
        Err(err) => {
            crate::log_error!("{uid}: could not mark the run stopping: {err:#}");
            return refuse("This computer could not record the stop.");
        }
    }
    // A run whose link is not attached is stopped by the escalation.
    daemon.stop_through_link(uid).await;
    tokio::spawn(escalate_stop(Arc::clone(daemon), uid.to_string()));
    StopRunResult::Stopping
}

/// How long a send waits for the run's host to take the message into claude's input.
const SEND_WAIT: Duration = Duration::from_secs(10);

/// How long a settings change waits for claude to take each of its parts.
const SETTING_WAIT: Duration = Duration::from_secs(10);

/// Why a run other than a Claude run CodeConnect started takes no settings from an app.
pub(crate) const NOT_FROM_THE_APP: &str = "This session's settings can't be changed from the app.";

/// The permission modes an app may choose for such a run, with the names it shows. Never
/// `bypassPermissions`, which the host's run cannot take, nor claude's other modes.
const MODES: [(&str, &str); 3] = [
    ("default", "Ask before edits"),
    ("acceptEdits", "Accept edits"),
    ("plan", "Plan"),
];

/// An app's change of a hosted run's settings.
pub(crate) enum Setting {
    /// A model, an effort, or both.
    Model {
        model: Option<String>,
        effort: Option<String>,
    },
    /// A permission mode.
    Mode(String),
}

/// A hosted run's menus, from its start's answer `models` (each as claude gives it): each
/// model's `value`, name, description and efforts, and [`MODES`].
pub(crate) fn menus(models: &[serde_json::Value]) -> RunMenus {
    let text = |model: &serde_json::Value, key: &str| {
        model
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    RunMenus {
        models: models
            .iter()
            .filter_map(|model| {
                let value = text(model, "value")?;
                Some(MenuModel {
                    label: text(model, "displayName").unwrap_or_else(|| value.clone()),
                    detail: text(model, "description"),
                    efforts: model
                        .get("supportedEffortLevels")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    value,
                })
            })
            .collect(),
        modes: MODES
            .iter()
            .map(|(value, label)| MenuMode {
                value: value.to_string(),
                label: label.to_string(),
            })
            .collect(),
    }
}

/// What became of one settings line sent to the host.
enum Told {
    Done,
    Refused(String),
    /// Nothing was sent: the run's link was down.
    NotSent,
    /// Sent, and no outcome came.
    NoAnswer,
}

/// An app's change of the hosted Claude run `uid`'s settings. Checked first, with nothing
/// sent when it fails: a change naming nothing, a mode not in [`MODES`] (so
/// `bypassPermissions` never reaches the host), a run not hosted here or not live, a model
/// not in the run's menus, an effort none of its models takes. Then each part goes to the
/// host as its own line, the model before the effort, and waits for claude to take it: a
/// part refused ends the change with claude's reason, saying so when the model was changed
/// and the effort was not. `applied` reads `next_turn` while a turn runs. An effort taken
/// is filed as the run's ([`apply_setting_outcome`]). Nothing is kept per request: a retry
/// applies the change again.
pub(crate) async fn set_setting(daemon: &Daemon, uid: &str, setting: Setting) -> SettingResult {
    let refused = |reason: &str| SettingResult::Refused {
        reason: reason.to_string(),
    };
    let parts: Vec<(Option<String>, Option<String>, Option<String>)> = match setting {
        Setting::Model {
            model: None,
            effort: None,
        } => return refused("Choose a model or an effort; nothing was changed."),
        Setting::Model { model, effort } => model
            .map(|model| (Some(model), None, None))
            .into_iter()
            .chain(effort.map(|effort| (None, Some(effort), None)))
            .collect(),
        Setting::Mode(mode) => {
            if !MODES.iter().any(|(value, _)| *value == mode) {
                return refused(
                    "That permission mode isn't offered in this session; nothing was changed.",
                );
            }
            vec![(None, None, Some(mode))]
        }
    };
    match daemon.db.hosted_link(uid.to_string()).await {
        Ok(Some(linked)) if linked.lifecycle == Lifecycle::Live => {}
        Ok(Some(_)) => return refused("This session isn't running, so nothing was changed."),
        Ok(None) => return refused(NOT_FROM_THE_APP),
        Err(err) => {
            crate::log_error!("{uid}: could not read its host: {err:#}");
            return refused("This computer could not read the session.");
        }
    }
    let menus = match daemon.db.hosted_models(uid.to_string()).await {
        Ok(Some(models)) => menus(&models),
        Ok(None) => return refused("This session isn't running, so nothing was changed."),
        Err(err) => {
            crate::log_error!("{uid}: could not read its models: {err:#}");
            return refused("This computer could not read the session.");
        }
    };
    for (model, effort, _) in &parts {
        if model
            .as_ref()
            .is_some_and(|model| !menus.models.iter().any(|offered| &offered.value == model))
        {
            return refused(
                "Claude doesn't offer that model in this session; nothing was changed.",
            );
        }
        if effort.as_ref().is_some_and(|effort| {
            !menus
                .models
                .iter()
                .any(|offered| offered.efforts.contains(effort))
        }) {
            return refused(
                "Claude doesn't offer that effort in this session; nothing was changed.",
            );
        }
    }
    let mut model_changed = false;
    for (model, effort, mode) in parts {
        let is_model = model.is_some();
        let told = apply_setting(daemon, uid, model, effort, mode).await;
        let before = |reason: &str| {
            if model_changed {
                format!("The model was changed; the effort was not: {reason}")
            } else {
                reason.to_string()
            }
        };
        match told {
            Told::Done => model_changed |= is_model,
            Told::Refused(reason) => return refused(&before(&reason)),
            Told::NotSent => {
                return refused(&before(
                    "The computer lost its connection to this session, so nothing was changed.",
                ))
            }
            Told::NoAnswer => {
                return SettingResult::Indeterminate {
                    reason: before(
                        "Claude didn't answer, so the change may or may not have been made.",
                    ),
                }
            }
        }
    }
    let applies = match daemon.db.turn_running(uid.to_string()).await {
        Ok(false) => Applies::Now,
        Ok(true) => Applies::NextTurn,
        Err(err) => {
            crate::log_error!("{uid}: could not read whether a turn runs: {err:#}");
            Applies::NextTurn
        }
    };
    SettingResult::Applied { applies }
}

/// One settings line to the run `uid`'s host, with a uuid of its own, and its outcome.
async fn apply_setting(
    daemon: &Daemon,
    uid: &str,
    model: Option<String>,
    effort: Option<String>,
    mode: Option<String>,
) -> Told {
    let uuid = match protocol::uid::new() {
        Ok(uuid) => uuid,
        Err(err) => {
            crate::log_error!("{uid}: could not name a settings line: {err:#}");
            return Told::NotSent;
        }
    };
    let told = daemon.await_host_setting(uid, &uuid, effort.clone()).await;
    let line = match mode {
        Some(mode) => ToHost::SetMode {
            uuid: uuid.clone(),
            mode,
        },
        None => ToHost::SetModel {
            uuid: uuid.clone(),
            model,
            effort,
        },
    };
    if !daemon.send_to_host(uid, line).await {
        daemon.take_host_setting(&uuid).await;
        return Told::NotSent;
    }
    match tokio::time::timeout(SETTING_WAIT, told).await {
        Ok(Ok((true, _))) => Told::Done,
        Ok(Ok((false, detail))) => {
            Told::Refused(detail.unwrap_or_else(|| "Claude didn't take the change.".into()))
        }
        Ok(Err(_)) | Err(_) => Told::NoAnswer,
    }
}

/// The host's outcome for a settings line this daemon sent ([`apply_setting`]): committed
/// with its cursor, with a `model_set {effort}` notice when it set an effort and claude took
/// it, then acknowledged, and whoever waits is told. It files even when nobody waits any
/// more, so an effort taken late is still the run's. `false` for an outcome that names no
/// settings line.
#[allow(clippy::too_many_arguments)]
async fn apply_setting_outcome(
    daemon: &Daemon,
    uid: &str,
    host: ProcessIdentity,
    seq: u64,
    uuid: &str,
    status: OutcomeStatus,
    detail: Option<String>,
    write: &mut OwnedWriteHalf,
) -> Result<bool, String> {
    let Some(pending) = daemon.take_host_setting(uuid).await else {
        return Ok(false);
    };
    let taken = matches!(status, OutcomeStatus::Done);
    let notices = match pending.effort.as_deref().filter(|_| taken) {
        Some(effort) => vec![PendingEvent::new(
            &SessionKey::new(uid, uid),
            EventKind::Notification,
            serde_json::json!({
                "notification_type": protocol::event::notice::MODEL_SET,
                "effort": effort,
                "message": format!("Effort set to {effort}"),
            }),
            Source::Daemon,
        )
        .with_source_event_id(format!("setting:{uuid}"))],
        None => Vec::new(),
    };
    match daemon
        .ingest_host_message(uid, host, seq, notices, RunNotes::default())
        .await
    {
        Ok(true) => send(write, &ToHost::Ack { seq }).await?,
        Ok(false) => {
            return Err(format!(
                "message {seq} is not past the run's cursor for this host"
            ))
        }
        Err(err) => return Err(format!("{err:#}")),
    }
    let _ = pending.tell.send((taken, detail));
    Ok(true)
}

/// The commands an app opens a screen of its own for that this ccd refuses on a hosted
/// run, each with the reason the app shows. One list, read twice: [`send_text`] refuses
/// each from its screen, and the run's command menu ([`catalog`]) leaves out exactly
/// these, because the daemon is the one that knows what it refuses, so an app offers
/// nothing it would be refused and keeps no list of its own in step with this one.
const REFUSED_SCREENS: [(&str, &str); 3] = [
    ("clear", "This isn't available for this session yet."),
    (
        "model",
        "Changing the model isn't available for this session yet.",
    ),
    (
        "effort",
        "Changing the effort isn't available for this session yet.",
    ),
];

/// Why ccd refuses `text`, sent from an app, as the screen of a command in
/// [`REFUSED_SCREENS`]: the Model and Effort sheets send theirs with the native
/// confirmation (`confirmed`), and the Clear dialog sends `/clear` alone. Typed with an
/// argument (`/effort low`), such a command is a message, as at claude's keyboard.
fn refused_screen(text: &str, confirmed: bool) -> Option<&'static str> {
    let mut words = text.split_whitespace();
    let command = words.next()?.strip_prefix('/')?;
    let (_, reason) = REFUSED_SCREENS.iter().find(|(name, _)| *name == command)?;
    (confirmed || words.next().is_none()).then_some(*reason)
}

/// A hosted run's command menu, from what the store keeps of it: `unavailable` until
/// the run's first init has named the commands claude keeps to its own screen, then
/// claude's list as it last reported it, without those and without
/// [`REFUSED_SCREENS`], each with what claude says of it.
pub(crate) fn catalog(kept: HostedCommands) -> CommandCatalogResult {
    let (Some(commands), Some(screen_only)) = (kept.commands, kept.screen_only) else {
        return CommandCatalogResult::Unavailable {
            reason: "Claude hasn't listed its commands for this session yet.".into(),
        };
    };
    let text = |command: &serde_json::Value, key: &str| {
        command
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let entries: Vec<CommandEntry> = commands
        .iter()
        .filter_map(|command| {
            let name = text(command, "name")?;
            let offered = !screen_only.contains(&name)
                && !REFUSED_SCREENS.iter().any(|(refused, _)| *refused == name);
            offered.then(|| CommandEntry {
                description: text(command, "description").unwrap_or_default(),
                argument_hint: text(command, "argumentHint").filter(|hint| !hint.is_empty()),
                name,
            })
        })
        .collect();
    CommandCatalogResult::Available {
        commands: entries.iter().map(|entry| entry.name.clone()).collect(),
        entries,
        claude_version: kept.claude_version,
        probed_at: kept.commands_at.unwrap_or_default(),
    }
}

/// One `send_text` for a run started in an agent host, as the app sent it and ccd
/// checked it.
pub(crate) struct Message {
    pub text: String,
    /// The app's request id with the hash it was checked against, when it gave them.
    pub identity: Option<(String, String)>,
    pub submit: bool,
    pub complete_native_confirmation: bool,
}

/// `send_text` to the hosted Claude run `uid`, whose lifecycle was last read as
/// `lifecycle`: the message goes into claude's input through the run's host (`say`),
/// and the person's words are filed once the host has taken it ([`apply_send_outcome`]).
/// The send's claim keeps its text, so a restarted ccd still files them from the host's
/// replay. A retry is answered from that claim first, as a typed send's is; only a new
/// message meets the run's state. The screens of [`REFUSED_SCREENS`] (the model and effort
/// confirmations, `/clear`) are refused, as are `/status`, which claude does not offer
/// such a run, and a message staged without being sent. `/usage` and `/cost` are
/// messages, whose output the run's events carry.
pub(crate) async fn send_text(
    daemon: &Daemon,
    uid: &str,
    lifecycle: Lifecycle,
    message: Message,
) -> SendTextResult {
    let refuse = |reason: &str| SendTextResult::Refused {
        reason: reason.to_string(),
    };
    let Message {
        text,
        identity,
        submit,
        complete_native_confirmation,
    } = message;
    if let Some(reason) = refused_screen(&text, complete_native_confirmation) {
        return refuse(reason);
    }
    if text.split_whitespace().next() == Some("/status") {
        return refuse("Claude doesn't offer /status in this session.");
    }
    if !submit {
        return refuse("A message to this session is sent whole; nothing was sent.");
    }
    if text.trim().is_empty() {
        return refuse("There is nothing to send.");
    }
    let Some((request_id, hash)) = identity else {
        return refuse("A message to this session needs its request id; nothing was sent.");
    };
    match daemon
        .db
        .claim_text_mutation(
            uid.to_string(),
            request_id.clone(),
            hash,
            Some(text.clone()),
            protocol::time::now_rfc3339(),
        )
        .await
    {
        Ok(TextClaim::Claimed) => {}
        Ok(TextClaim::Applied {
            matched,
            settled_at,
        }) => {
            return SendTextResult::Duplicate {
                matched,
                applied_at: settled_at,
            }
        }
        Ok(TextClaim::Indeterminate { started_at }) => {
            return SendTextResult::Indeterminate {
                reason: format!(
                    "this message (first sent {started_at}) was never confirmed; it may or may not \
                     have reached Claude, and it will not be sent again"
                ),
            }
        }
        Ok(TextClaim::Conflict) => {
            return refuse(
                "this request_id was already used for different text in this session; use a new \
                 one",
            )
        }
        Err(err) => {
            crate::log_error!("{uid}: could not claim send {request_id}: {err:#}");
            return refuse("could not record that this message is being sent; nothing was sent");
        }
    }
    if lifecycle != Lifecycle::Live {
        release_send(daemon, uid, &request_id).await;
        return refuse("This session isn't running, so nothing was sent.");
    }
    let uuid = send_uuid(uid, &request_id);
    let taken = daemon.await_host_send(uid, &uuid).await;
    let say = ToHost::Say {
        uuid: uuid.clone(),
        text: text.clone(),
    };
    if !daemon.send_to_host(uid, say).await {
        daemon.host_send_taken(&uuid, false).await;
        release_send(daemon, uid, &request_id).await;
        return refuse(
            "The computer is reconnecting to this session; nothing was sent. Try again in a \
             moment.",
        );
    }
    match tokio::time::timeout(SEND_WAIT, taken).await {
        Ok(Ok(true)) => SendTextResult::Sent { matched: text },
        Ok(Ok(false)) => refuse("This session stopped taking messages; nothing was sent."),
        Ok(Err(_)) => SendTextResult::Indeterminate {
            reason: "The computer lost the session before the message was confirmed; it may or \
                     may not have reached Claude."
                .into(),
        },
        // Settled nothing: the host's outcome, whenever it comes, still files the words.
        Err(_) => {
            daemon.host_send_taken(&uuid, false).await;
            SendTextResult::Indeterminate {
                reason: "The session did not confirm the message in time; it may or may not \
                         have reached Claude."
                    .into(),
            }
        }
    }
}

/// Drop a send's claim when nothing was sent, so a retry is a fresh attempt.
async fn release_send(daemon: &Daemon, uid: &str, request_id: &str) {
    if let Err(err) = daemon
        .db
        .release_text_mutation(uid.to_string(), request_id.to_string())
        .await
    {
        crate::log_error!("{uid}: could not release send {request_id}: {err:#}");
    }
}

/// The uuid of the message ccd sends a run's host for the app's request `request_id`:
/// the same for a retry, and found again by a restarted ccd. A Codex run's message is named
/// the same way ([`crate::codex_host`]).
pub(crate) fn send_uuid(uid: &str, request_id: &str) -> String {
    protocol::uid::uuid_named(&format!("{uid}\0{request_id}"))
}

/// The uuid of the answer ccd sends a run's host for its card `request_id`: the same
/// for a retry, and found again by a restarted ccd.
pub(crate) fn answer_uuid(uid: &str, request_id: &str) -> String {
    protocol::uid::uuid_named(&format!("{uid}\0answer\0{request_id}"))
}

/// The uuid of the interrupt ccd sends a run's host for the app's Stop `request_id`: the
/// same for a retry, and found again among the run's claims when its outcome comes.
fn interrupt_uuid(uid: &str, request_id: &str) -> String {
    protocol::uid::uuid_named(&format!("{uid}\0interrupt\0{request_id}"))
}

/// How long an app's Stop waits for its host's outcome, the stopped turn's end, as a Codex
/// Stop waits for its own.
const STOP_WAIT: Duration = Duration::from_secs(15);

/// An app's Stop of the turn `turn_id` of the hosted run `uid`, whose lifecycle is
/// `lifecycle`, once the ledger holds no settled record of it ([`Daemon::interrupt`]). Only
/// a live run's turn is stopped. A Stop of that turn already waiting (its claim `applying`)
/// answers so at once, and a run whose link is not attached is refused, nothing sent, or
/// unknown if this very ask was sent before its link went. Otherwise the ask is claimed and
/// sent as `interrupt`, and the host's outcome answers it ([`apply_interrupt_outcome`]):
/// aborted, ended on its own, or not the turn the host runs. With no outcome within
/// [`STOP_WAIT`] it is unknown, and the outcome settles it when it comes.
pub(crate) async fn interrupt(
    daemon: &Daemon,
    uid: &str,
    request_id: &str,
    turn_id: &str,
    lifecycle: Lifecycle,
) -> InterruptReport {
    use crate::codex_refusals as says;
    use crate::store::{ClaimedMaterial, MutationClaim, INTERRUPT_ROUTE, OPERATION_INTERRUPT};
    match lifecycle {
        Lifecycle::Live => {}
        Lifecycle::Stopping => {
            return InterruptReport::NotApplied(says::INTERRUPT_ALREADY_WAITING.into())
        }
        Lifecycle::Starting => {
            return InterruptReport::NotApplied(says::CLAUDE_TURN_NOT_RUNNING.into())
        }
        _ => return InterruptReport::NotApplied(says::INTERRUPT_RUN_IS_GONE.into()),
    }
    let waiting: Vec<MutationClaimRow> = match daemon
        .db
        .unsettled_claims_for(OPERATION_INTERRUPT, uid.to_string())
        .await
    {
        Ok(claims) => claims
            .into_iter()
            .filter(|claim| claim.claimed.target_turn_id.as_deref() == Some(turn_id))
            .collect(),
        Err(err) => {
            crate::log_error!("{uid}: could not read its Stops: {err:#}");
            return InterruptReport::NotApplied(says::INTERRUPT_LEDGER_UNREADABLE.into());
        }
    };
    if !daemon.host_attached(uid).await {
        return if waiting
            .iter()
            .any(|claim| claim.client_request_id == request_id)
        {
            InterruptReport::Unknown(says::INTERRUPT_ALREADY_SENT_UNKNOWN.into())
        } else {
            InterruptReport::NotApplied(says::ENGINE_NO_CONNECTION.into())
        };
    }
    if !waiting.is_empty() {
        return InterruptReport::NotApplied(says::INTERRUPT_ALREADY_WAITING.into());
    }
    let claimed = ClaimedMaterial {
        thread_id: uid.to_string(),
        generation: 0,
        route: INTERRUPT_ROUTE.to_string(),
        target_turn_id: Some(turn_id.to_string()),
        claimed_hash: protocol::hash::interrupt_hash(uid, turn_id),
    };
    let claim = daemon
        .db
        .claim_mutation(
            OPERATION_INTERRUPT,
            uid.to_string(),
            request_id.to_string(),
            claimed,
            protocol::time::now_rfc3339(),
        )
        .await;
    match claim {
        Ok(MutationClaim::Claimed) => {}
        Ok(MutationClaim::Applied { outcome, .. }) => {
            return crate::codex_link::replayed_interrupt_report(&outcome, turn_id)
        }
        Ok(MutationClaim::Indeterminate { .. }) => {
            return InterruptReport::Unknown(says::INTERRUPT_ALREADY_SENT_UNKNOWN.into())
        }
        Ok(MutationClaim::Conflict) => {
            return InterruptReport::NotApplied(says::INTERRUPT_ID_REUSED_ON_LINK.into())
        }
        Ok(MutationClaim::NoSession) => {
            return InterruptReport::NotApplied(says::INTERRUPT_RUN_IS_GONE.into())
        }
        Err(err) => {
            crate::log_error!("{uid}: could not claim the stop of turn {turn_id}: {err:#}");
            return InterruptReport::NotApplied(says::INTERRUPT_CLAIM_UNRECORDED.into());
        }
    }
    let uuid = interrupt_uuid(uid, request_id);
    let told = daemon.await_host_stop(uid, &uuid).await;
    let command = ToHost::Interrupt {
        uuid: uuid.clone(),
        turn_id: turn_id.to_string(),
    };
    if !daemon.send_to_host(uid, command).await {
        daemon.host_stop_told(&uuid, None).await;
        if let Err(err) = daemon
            .db
            .release_mutation(OPERATION_INTERRUPT, uid.to_string(), request_id.to_string())
            .await
        {
            crate::log_error!("{uid}: could not give back the stop that was not sent: {err:#}");
        }
        return InterruptReport::NotApplied(says::ENGINE_NO_CONNECTION.into());
    }
    match tokio::time::timeout(STOP_WAIT, told).await {
        Ok(Ok(report)) => report,
        Ok(Err(_)) => InterruptReport::Unknown(says::interrupt_settled_unknown(
            says::CLAUDE_CONNECTION_ENDED,
        )),
        Err(_) => {
            daemon.host_stop_told(&uuid, None).await;
            InterruptReport::Unknown(says::INTERRUPT_ALREADY_SENT_UNKNOWN.into())
        }
    }
}

/// An app's answer to the hosted run `uid`'s card `request_id`, after the ledger found
/// none recorded: it goes to the run's host (`answer`), and is `Applied` once the host
/// says it reached claude ([`apply_answer_outcome`] records it). Its claim is settled
/// only by the host: an answer whose outcome has not come is never sent again, and is
/// left for the host's replay or its `state` to settle.
pub(crate) async fn answer(
    daemon: &Daemon,
    uid: &str,
    request_id: &str,
    payload_hash: &str,
    decision: AnswerDecision,
    answered_on: AnsweredOn,
) -> AnswerResult {
    let rejected = |reason: &str| AnswerResult::Rejected {
        reason: reason.to_string(),
    };
    match daemon
        .db
        .answer_claim(uid.to_string(), request_id.to_string())
        .await
    {
        Ok(None) => {}
        Ok(Some(_)) => {
            return rejected(
                "an answer to this card is still on its way to Claude; it will not be sent again",
            )
        }
        Err(err) => {
            crate::log_error!("{uid}: claim read failed: {err:#}");
            return rejected("ledger unavailable");
        }
    }
    let (session, host_decision) = match daemon
        .claim_host_card(uid, request_id, payload_hash, &decision, answered_on)
        .await
    {
        Ok(claimed) => claimed,
        Err(reason) => return AnswerResult::Rejected { reason },
    };
    let claim = AnswerClaim {
        session_uid: uid.to_string(),
        session_id: session.name.clone(),
        request_id: request_id.to_string(),
        payload_hash: payload_hash.to_string(),
        decision: serde_json::to_string(&decision).unwrap_or_default(),
        started_at: protocol::time::now_rfc3339(),
    };
    if let Err(err) = daemon.db.claim_answer(claim).await {
        crate::log_error!("{uid}: could not claim the answer to {request_id}: {err:#}");
        daemon.unclaim_host_card(uid, request_id).await;
        return rejected("could not record that this answer is being applied; nothing was sent");
    }
    let uuid = answer_uuid(uid, request_id);
    let outcome = AnswerOutcome {
        request_id: request_id.to_string(),
        session_id: session.name.clone(),
        decision,
        resolved_by: ResolvedBy::Phone,
        answered_on: Some(answered_on),
        applied_via: AnswerPath::HookReturn,
        resolved_at: protocol::time::now_rfc3339(),
        detail: None,
        inferred: false,
        indeterminate: false,
    };
    daemon.hold_host_answer(&uuid, outcome).await;
    let taken = daemon.await_host_send(uid, &uuid).await;
    let command = ToHost::Answer {
        uuid: uuid.clone(),
        tool_use_id: request_id.to_string(),
        decision: host_decision,
    };
    if !daemon.send_to_host(uid, command).await {
        daemon.host_send_taken(&uuid, false).await;
        daemon.take_host_answer(&uuid).await;
        if let Err(err) = daemon
            .db
            .release_answer_claim(uid.to_string(), request_id.to_string())
            .await
        {
            crate::log_error!("{uid}: could not release the answer to {request_id}: {err:#}");
        }
        daemon.unclaim_host_card(uid, request_id).await;
        return rejected(
            "The computer is reconnecting to this session; nothing was sent. Try again in a \
             moment.",
        );
    }
    match tokio::time::timeout(SEND_WAIT, taken).await {
        Ok(Ok(true)) => match daemon
            .db
            .get_answer(uid.to_string(), request_id.to_string())
            .await
        {
            Ok(Some((_, outcome))) => AnswerResult::Applied { outcome },
            _ => rejected("Claude has the answer, but this computer could not read its record"),
        },
        Ok(Ok(false)) => rejected("Claude was no longer asking, so the answer was not used."),
        Ok(Err(_)) => rejected(
            "The computer lost the session before the answer was confirmed; it may or may not \
             have reached Claude, and it will not be sent again.",
        ),
        Err(_) => {
            daemon.host_send_taken(&uuid, false).await;
            rejected(
                "The session did not confirm the answer in time; it may or may not have reached \
                 Claude, and it will not be sent again.",
            )
        }
    }
}

/// **Wake the hosted run `uid` for its mail** with `text`, the fixed nudge: the line goes to
/// its host through its attached link, and ccd files a `mail_nudge` notice for it, since a
/// replayed user line files nothing ([`protocol::sdk_map`]). Whether a link took it; a run
/// with no link attached (a terminal run, or a host not reached) is not woken here.
pub(crate) async fn wake(daemon: &Daemon, uid: &str, text: &str) -> bool {
    let uuid = match protocol::uid::uuid_v4() {
        Ok(uuid) => uuid,
        Err(err) => {
            crate::log_error!("{uid}: could not name a mail wake: {err}");
            return false;
        }
    };
    let line = ToHost::Wake {
        uuid: uuid.clone(),
        text: text.to_string(),
    };
    if !daemon.send_to_host(uid, line).await {
        return false;
    }
    let notice = PendingEvent::new(
        &SessionKey::new(uid, uid),
        EventKind::Notification,
        serde_json::json!({
            "notification_type": protocol::event::notice::MAIL_NUDGE,
            "message": text,
        }),
        Source::Daemon,
    )
    .with_source_event_id(format!("wake:{uuid}"));
    if let Err(err) = daemon.ingest(notice).await {
        crate::log_error!("{uid}: could not record its mail wake: {err:#}");
    }
    true
}

/// The person's words `text`, sent as the message `uuid` (the start's or a later
/// send's), as the run's event. A Codex run's later sends file the same row.
pub(crate) fn person_row(
    uid: &str,
    uuid: &str,
    text: &str,
    request_id: Option<&str>,
) -> PendingEvent {
    let mut payload = serde_json::json!({ "text": text });
    if let Some(request_id) = request_id {
        payload["request_id"] = request_id.into();
    }
    PendingEvent::new(
        &SessionKey::new(uid, uid),
        EventKind::UserMessage,
        payload,
        Source::Daemon,
    )
    .with_source_event_id(format!("say:{uuid}"))
}

/// The host's outcome for a message ccd sent it (`say`): the send it names, found by its
/// uuid among the run's claimed sends, is settled with the person's words filed when the
/// host took the message, or released when it refused it, in one transaction with the
/// cursor; then acknowledged, and whoever waits for it is told. A message taken is the
/// person's prompt to the mail wake rule, as a terminal run's prompt hook tells it: with no
/// turn running and unread mail waiting, the nudge follows it as a wake line ([`wake`]). An
/// outcome naming no claimed send is an answer's ([`apply_answer_outcome`]); a wake's own
/// outcome is one of those.
async fn apply_send_outcome(
    daemon: &Arc<Daemon>,
    uid: &str,
    host: ProcessIdentity,
    seq: u64,
    uuid: &str,
    status: OutcomeStatus,
    write: &mut OwnedWriteHalf,
) -> Result<(), String> {
    let taken = matches!(status, OutcomeStatus::Done);
    let sends = daemon
        .db
        .host_sends(uid.to_string())
        .await
        .map_err(|err| format!("{err:#}"))?;
    match sends
        .into_iter()
        .find(|(request_id, _)| send_uuid(uid, request_id) == uuid)
    {
        Some((request_id, text)) => {
            let row = taken.then(|| person_row(uid, uuid, &text, Some(&request_id)));
            match daemon
                .commit_host_send(uid, host, seq, &request_id, row)
                .await
            {
                Ok(true) => send(write, &ToHost::Ack { seq }).await?,
                Ok(false) => {
                    return Err(format!(
                        "message {seq} is not past the run's cursor for this host"
                    ))
                }
                Err(err) => return Err(format!("{err:#}")),
            }
            if taken {
                if let Some(nudge) = crate::mail::prompt_submitted(daemon, uid).await {
                    wake(daemon, uid, nudge).await;
                }
            }
        }
        None => apply_answer_outcome(daemon, uid, host, seq, uuid, taken, write).await?,
    }
    daemon.host_send_taken(uuid, taken).await;
    Ok(())
}

/// The host's outcome for a command, as an app's Stop reads it.
struct StopOutcome<'a> {
    seq: u64,
    uuid: &'a str,
    status: OutcomeStatus,
    /// The interrupt's receipt: the uuids of the person's messages claude cancelled.
    cancelled: Option<&'a [String]>,
    /// The stopped turn's `result.terminal_reason`.
    terminal_reason: Option<&'a str>,
}

/// The host's outcome for an app's Stop ccd sent it (`interrupt`), found by its uuid among
/// the run's `applying` interrupt claims: settled from the stopped turn's end ([`settle_stop`]),
/// and whoever waits for it is told. Each of the person's messages the receipt lists as
/// cancelled is marked not sent ([`not_sent`]), with the cursor, then the outcome is
/// acknowledged. A restart's replay finds its claim settled already by the startup sweep,
/// and marks the messages alone. `false` when the outcome is not a Stop's.
async fn apply_interrupt_outcome(
    daemon: &Daemon,
    uid: &str,
    host: ProcessIdentity,
    told: &StopOutcome<'_>,
    write: &mut OwnedWriteHalf,
) -> Result<bool, String> {
    let claims = daemon
        .db
        .unsettled_claims_for(crate::store::OPERATION_INTERRUPT, uid.to_string())
        .await
        .map_err(|err| format!("{err:#}"))?;
    let claim = claims
        .into_iter()
        .find(|claim| interrupt_uuid(uid, &claim.client_request_id) == told.uuid);
    if claim.is_none() && told.cancelled.is_none() && told.terminal_reason.is_none() {
        return Ok(false);
    }
    let report = match &claim {
        Some(claim) => Some(settle_stop(daemon, uid, claim, told).await),
        None => None,
    };
    let notices = not_sent(daemon, uid, told.cancelled.unwrap_or_default()).await?;
    match daemon
        .ingest_host_message(uid, host, told.seq, notices, RunNotes::default())
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "message {} is not past the run's cursor for this host",
                told.seq
            ))
        }
        Err(err) => return Err(format!("{err:#}")),
    }
    send(write, &ToHost::Ack { seq: told.seq }).await?;
    daemon.host_stop_told(told.uuid, report).await;
    Ok(true)
}

/// What the host's outcome `told` makes of the app's Stop `claim`, as Codex's settle reads
/// its turn's end: a turn that ended aborted was stopped by it, any other end was the turn's
/// own; both are settled in the ledger. A refusal (the host was not running that turn) gives
/// the claim back, so the same ask is a first attempt again.
async fn settle_stop(
    daemon: &Daemon,
    uid: &str,
    claim: &MutationClaimRow,
    told: &StopOutcome<'_>,
) -> InterruptReport {
    use crate::codex_refusals as says;
    use crate::store::OPERATION_INTERRUPT;
    let request_id = claim.client_request_id.clone();
    if told.status == OutcomeStatus::Refused {
        if let Err(err) = daemon
            .db
            .release_mutation(OPERATION_INTERRUPT, uid.to_string(), request_id)
            .await
        {
            crate::log_error!("{uid}: could not give back the refused stop: {err:#}");
        }
        return InterruptReport::NotApplied(says::CLAUDE_TURN_NOT_RUNNING.into());
    }
    let reason = told.terminal_reason.unwrap_or("completed");
    let (outcome, report) = if reason.starts_with("aborted") {
        (
            crate::store::INTERRUPT_ABORTED,
            InterruptReport::Aborted {
                turn_id: claim.claimed.target_turn_id.clone().unwrap_or_default(),
            },
        )
    } else {
        (
            crate::store::INTERRUPT_TURN_ENDED,
            InterruptReport::NotApplied(says::interrupt_turn_ended_itself(reason)),
        )
    };
    let settled = daemon
        .db
        .settle_mutation(
            OPERATION_INTERRUPT,
            uid.to_string(),
            request_id,
            outcome.to_string(),
            protocol::time::now_rfc3339(),
        )
        .await;
    match settled {
        Ok(true) => report,
        Ok(false) => InterruptReport::Unknown(says::INTERRUPT_SETTLED_ELSEWHERE.into()),
        Err(err) => {
            crate::log_error!("{uid}: could not record what became of a stop: {err:#}");
            InterruptReport::Unknown(says::INTERRUPT_OUTCOME_UNRECORDED.into())
        }
    }
}

/// A `send_not_sent` notice for each of the run's messages whose uuid the interrupt's receipt
/// lists as `cancelled`: claude never ran them. A uuid that names none is ignored. Each
/// notice's source event id is its message's, so a replay files it once.
async fn not_sent(
    daemon: &Daemon,
    uid: &str,
    cancelled: &[String],
) -> Result<Vec<PendingEvent>, String> {
    if cancelled.is_empty() {
        return Ok(Vec::new());
    }
    let sends = daemon
        .db
        .send_request_ids(uid.to_string())
        .await
        .map_err(|err| format!("{err:#}"))?;
    Ok(sends
        .into_iter()
        .filter_map(|request_id| {
            let uuid = send_uuid(uid, &request_id);
            cancelled
                .contains(&uuid)
                .then(|| not_sent_notice(uid, &uuid, &request_id))
        })
        .collect())
}

/// The `send_not_sent` notice for the app's send `request_id`, the message `uuid`, which the
/// run's agent dropped when its turn was stopped. A Codex run's dropped message gets the same.
pub(crate) fn not_sent_notice(uid: &str, uuid: &str, request_id: &str) -> PendingEvent {
    PendingEvent::new(
        &SessionKey::new(uid, uid),
        EventKind::Notification,
        serde_json::json!({
            "notification_type": protocol::event::notice::SEND_NOT_SENT,
            "message": "Not sent: stopped",
            "request_id": request_id,
            "reason": "stopped",
        }),
        Source::Daemon,
    )
    .with_source_event_id(format!("not_sent:{uuid}"))
}

/// The host's outcome for an answer ccd sent it: the answer it names, found by its uuid
/// among the run's claimed answers, is recorded with its card closed when the host says
/// it reached claude (`taken`), or released, its card answerable again, when the host
/// refused it; in one transaction with the cursor, then acknowledged. The outcome this
/// daemon held for it is recorded; a restarted daemon records one from the claim, which
/// keeps no device. An outcome naming no claimed answer moves only the cursor.
async fn apply_answer_outcome(
    daemon: &Daemon,
    uid: &str,
    host: ProcessIdentity,
    seq: u64,
    uuid: &str,
    taken: bool,
    write: &mut OwnedWriteHalf,
) -> Result<(), String> {
    let claims = daemon
        .db
        .answer_claims_of(uid.to_string())
        .await
        .map_err(|err| format!("{err:#}"))?;
    let Some(claim) = claims
        .into_iter()
        .find(|claim| answer_uuid(uid, &claim.request_id) == uuid)
    else {
        return commit_and_ack(daemon, uid, host, seq, write).await;
    };
    let held = daemon.take_host_answer(uuid).await;
    let outcome = taken.then(|| {
        let outcome = held.unwrap_or_else(|| AnswerOutcome {
            request_id: claim.request_id.clone(),
            session_id: claim.session_id.clone(),
            decision: serde_json::from_str(&claim.decision).unwrap_or(AnswerDecision::Deny),
            resolved_by: ResolvedBy::Phone,
            answered_on: None,
            applied_via: AnswerPath::HookReturn,
            resolved_at: protocol::time::now_rfc3339(),
            detail: None,
            inferred: false,
            indeterminate: false,
        });
        (claim.payload_hash.clone(), outcome)
    });
    match daemon
        .commit_host_answer(uid, host, seq, &claim.request_id, outcome)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "message {seq} is not past the run's cursor for this host"
            ))
        }
        Err(err) => return Err(format!("{err:#}")),
    }
    if taken {
        daemon.answered_host_card(uid, &claim.request_id).await;
    } else {
        daemon.unclaim_host_card(uid, &claim.request_id).await;
    }
    send(write, &ToHost::Ack { seq }).await
}

/// H5's escalation for the run `uid`, sent `stop`: [`STOP_GRACE`] later, while it is still
/// `stopping`, its host's group is stopped by the ladder if the host still runs, and the
/// run then ends as a run whose host is gone ([`end_if_host_gone`]).
pub(crate) async fn escalate_stop(daemon: Arc<Daemon>, uid: String) {
    tokio::time::sleep(STOP_GRACE).await;
    let watch = match daemon.db.watched_host(uid.clone()).await {
        Ok(Some(watch)) if watch.lifecycle == Lifecycle::Stopping => watch,
        Ok(_) => return,
        Err(err) => {
            crate::log_error!("{uid}: could not read its host to stop it: {err:#}");
            return;
        }
    };
    if liveness(&watch.host) == Liveness::Alive {
        match watch.boot {
            Some(boot) => {
                if let Ladder::Refused(why) = stop_group(watch.host, boot).await {
                    crate::log_info!("{uid}: its host did not stop; its group was not signalled: {why}");
                }
            }
            None => crate::log_info!(
                "{uid}: its host did not stop; no boot was recorded with it, so its group was not signalled"
            ),
        }
    }
    end_if_host_gone(&daemon, &uid).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Endpoint;
    use crate::store::Store;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    /// A state folder of its own, short and under `/tmp` so `runs/<uid>/host.sock` fits
    /// `sun_path`, with a daemon on a store in it. Removed when dropped.
    struct Rig {
        root: PathBuf,
        daemon: Arc<Daemon>,
        /// What a start gives its host: a home of the rig's own and the system shell.
        environment: HashMap<OsString, OsString>,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn rig() -> Rig {
        let uid = protocol::uid::new().unwrap();
        let root = PathBuf::from(format!("/tmp/cch2.{}", &uid[16..]));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("home")).unwrap();
        let daemon = daemon_on(&root);
        let environment = HashMap::from([
            (OsString::from("HOME"), root.join("home").into_os_string()),
            (OsString::from("SHELL"), OsString::from("/bin/sh")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        Rig {
            root,
            daemon,
            environment,
        }
    }

    /// A daemon on the store in `root`, as a restarted ccd opens it again.
    fn daemon_on(root: &Path) -> Arc<Daemon> {
        let store = Store::open(&root.join("events.db")).unwrap();
        let (transcript_tx, transcript_rx) = tokio::sync::mpsc::unbounded_channel();
        Box::leak(Box::new(transcript_rx));
        Daemon::new(
            Config::default(),
            Arc::new(store),
            Arc::new(crate::apns::LoggingPushSender::new()),
            Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
            transcript_tx,
        )
    }

    /// A stand-in host: it writes `host.json` with `version` and `pid` (its own when
    /// `None`) into the run's folder, then waits, so the test can play the host's side
    /// of the socket.
    fn stub_host(rig: &Rig, version: u32, pid: Option<i32>) -> PathBuf {
        let path = rig.root.join("stub-host");
        let pid = pid.map_or("$$".to_string(), |pid| pid.to_string());
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho $$ > \"$1/stub.pid\"\nprintf '{{\"protocol_version\":{version},\"host_pid\":%s}}' \
                 \"{pid}\" > \"$1/host.json\"\nexec sleep 20\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// [`stub_host`] whose process group also holds a child it started, its pid in
    /// `child.pid` in the run's folder, as claude is held in the real host's group. The child
    /// ignores SIGTERM, so only the ladder's SIGKILL ends it.
    fn stub_host_with_child(rig: &Rig) -> PathBuf {
        let path = rig.root.join("stub-host-with-child");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho $$ > \"$1/stub.pid\"\n(trap '' TERM; exec sleep 60) &\necho $! > \"$1/child.pid\"\n\
                 printf '{{\"protocol_version\":{HOST_PROTOCOL_VERSION},\"host_pid\":%s}}' \"$$\" > \
                 \"$1/host.json\"\nexec sleep 20\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The pid a stand-in wrote into `dir/name`.
    fn pid_in(dir: &Path, name: &str) -> i32 {
        std::fs::read_to_string(dir.join(name))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// Whether `pid` is a running process: neither gone nor a zombie.
    fn running(pid: i32) -> bool {
        // SAFETY: kill(2) with signal 0 only asks whether the process exists.
        let exists = unsafe { libc::kill(pid, 0) } == 0;
        exists && protocol::proc_identity::is_zombie(pid) != Some(true)
    }

    fn run<'a>(rig: &'a Rig, request_id: &'a str, cwd: &'a str, host_bin: &'a Path) -> Run<'a> {
        Run {
            root: &rig.root,
            request_id,
            cwd,
            prompt: "fix the tests",
            model: Some("default"),
            worktree: None,
            trust: Trust::NotTrusted,
            claude_path: "/nonexistent/claude",
            host_bin,
            codeconnect: Path::new("/nonexistent/codeconnect"),
            environment: &rig.environment,
        }
    }

    fn runs(rig: &Rig) -> Vec<PathBuf> {
        match std::fs::read_dir(rig.root.join("runs")) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The test's side of the run's socket: it waits for the start to write host.json,
    /// listens where the host would, and takes ccd's connection.
    async fn play_host(dir: &Path) -> (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf, HostFile) {
        let record = loop {
            if let Some(record) = read_host_file(&dir.join("host.json")) {
                break record;
            }
            tokio::time::sleep(POLL).await;
        };
        let listener = UnixListener::bind(dir.join("host.sock")).unwrap();
        let (stream, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
            .await
            .expect("ccd connected")
            .unwrap();
        let (read, write) = stream.into_split();
        (BufReader::new(read).lines(), write, record)
    }

    async fn next(lines: &mut Lines<BufReader<OwnedReadHalf>>) -> Option<ToHost> {
        tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .ok()?
            .ok()?
            .map(|line| serde_json::from_str(&line).unwrap())
    }

    async fn say(write: &mut OwnedWriteHalf, message: serde_json::Value) {
        write
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    /// Stop the stand-in host in `dir` by the pid it recorded for itself.
    fn stop(dir: &Path) {
        let pid: i32 = std::fs::read_to_string(dir.join("stub.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: a plain kill(2) of the stand-in host this test started, by its pid.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }

    fn cursor(rig: &Rig, uid: &str) -> i64 {
        rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .query_row(
                "SELECT cursor FROM run_hosts WHERE session_uid = ?1",
                [uid],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// The run's folder once the start has spawned its stand-in host, which has written
    /// host.json there.
    async fn spawned_dir(rig: &Rig) -> PathBuf {
        loop {
            if let Some(dir) = runs(rig)
                .into_iter()
                .find(|dir| dir.join("host.json").exists())
            {
                return dir;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// The host's side of a start, up to and including `start`: ccd's attach, the first
    /// state and its ack, then the `start` line.
    struct Played {
        dir: PathBuf,
        uid: String,
        lines: Lines<BufReader<OwnedReadHalf>>,
        write: OwnedWriteHalf,
        record: HostFile,
        start: ToHost,
    }

    impl Played {
        fn start_uuid(&self) -> String {
            match &self.start {
                ToHost::Start { uuid, .. } => uuid.clone(),
                other => panic!("not a start: {other:?}"),
            }
        }
    }

    async fn play_to_start(rig: &Rig) -> Played {
        let dir = spawned_dir(rig).await;
        let uid = dir.file_name().unwrap().to_str().unwrap().to_string();
        let (mut lines, mut write, record) = play_host(&dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 0 }));
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 1, "pending": []}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 1 }));
        let start = next(&mut lines).await.expect("ccd sent start");
        Played {
            dir,
            uid,
            lines,
            write,
            record,
            start,
        }
    }

    /// An initialize answer as the host sends it in a done outcome.
    fn initialize_answer() -> serde_json::Value {
        serde_json::json!({
            "commands": [{"name": "compact"}],
            "models": [
                {"value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default (recommended)",
                 "description": "Use the default model (currently Opus 5.5) · $4/$20 per Mtok", "supportsEffort": true,
                 "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]},
                {"value": "sonnet", "resolvedModel": "claude-sonnet-5-5", "displayName": "Sonnet",
                 "description": "Sonnet 5.5 · Efficient for routine tasks · $2/$10 per Mtok", "supportsEffort": true,
                 "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]}
            ],
            "account": {"api_provider": "firstParty"},
            "claude_code_version": "2.1.322"
        })
    }

    /// What the store holds for run `uid`: its lifecycle and the initialize answer kept.
    fn kept(rig: &Rig, uid: &str) -> (String, Option<serde_json::Value>) {
        rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .query_row(
                "SELECT s.lifecycle, h.initialize
                   FROM run_hosts AS h JOIN all_sessions AS s USING(session_uid)
                  WHERE session_uid = ?1",
                [uid],
                |row| {
                    let initialize: Option<String> = row.get(1)?;
                    Ok((
                        row.get(0)?,
                        initialize.map(|json| serde_json::from_str(&json).unwrap()),
                    ))
                },
            )
            .unwrap()
    }

    fn started_uid(result: &StartSessionResult) -> String {
        match result {
            StartSessionResult::Started { session_uid, .. } => session_uid.clone(),
            other => panic!("not started: {other:?}"),
        }
    }

    /// H1: the row is written before the host is spawned, so a ccd that died while its
    /// host starts finds the run when it starts again (H8) instead of a host nobody
    /// knows about. A write lock held from a second connection makes the row's insert
    /// wait (the store's busy timeout): while it is held, the host must not start.
    #[tokio::test]
    async fn the_row_is_there_while_the_host_starts() {
        let rig = rig();
        // A host that says it is up, then ends without writing host.json.
        let host = rig.root.join("slow-host");
        std::fs::write(&host, "#!/bin/sh\ntouch \"$1/started\"\nexec sleep 1\n").unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cwd = rig.root.to_str().unwrap().to_string();
        let lock = rusqlite::Connection::open(rig.root.join("events.db")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = || runs(&rig).iter().any(|dir| dir.join("started").exists());
        let (result, (started_under_lock, seen)) =
            tokio::join!(start(&rig.daemon, run(&rig, "req-1", &cwd, &host)), async {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let started_under_lock = started();
                lock.execute_batch("ROLLBACK").unwrap();
                while !started() {
                    tokio::time::sleep(POLL).await;
                }
                let seen = rig
                    .daemon
                    .db
                    .hosted_run_for_request("req-1".into())
                    .await
                    .unwrap()
                    .map(|run| run.lifecycle);
                (started_under_lock, seen)
            });
        assert!(
            !started_under_lock,
            "the host started before its row could be written"
        );
        assert_eq!(seen, Some(Lifecycle::Starting));
        assert!(
            matches!(result, StartSessionResult::Failed { .. }),
            "{result:?}"
        );
    }

    /// A start whose spawn cannot even begin (its folder is gone) leaves its row, failed,
    /// with its request id, and its folder private: 0700, with a 0600 log.
    #[tokio::test]
    async fn a_start_whose_spawn_fails_leaves_its_row_failed_and_its_folder_private() {
        let rig = rig();
        let missing = rig.root.join("gone");
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let result = start(
            &rig.daemon,
            run(&rig, "req-1", missing.to_str().unwrap(), &host),
        )
        .await;
        assert!(
            matches!(&result, StartSessionResult::Failed { detail, .. } if detail.contains("login shell")),
            "{result:?}"
        );
        let found = rig
            .daemon
            .db
            .hosted_run_for_request("req-1".into())
            .await
            .unwrap()
            .expect("the row is there");
        assert_eq!(found.lifecycle, Lifecycle::Failed);
        let dirs = runs(&rig);
        assert_eq!(dirs.len(), 1, "{dirs:?}");
        assert_eq!(dirs[0].file_name().unwrap(), found.session_uid.as_str());
        assert_eq!(mode(&rig.root.join("runs")), 0o700);
        assert_eq!(mode(&dirs[0]), 0o700);
        assert_eq!(mode(&dirs[0].join("host.log")), 0o600);
    }

    /// H1: a repeated request id, after the first finished and after a restart, gets
    /// the first run and starts nothing; so does one that overlaps the first.
    #[tokio::test]
    async fn a_repeated_start_gets_the_first_run_and_starts_nothing() {
        let rig = rig();
        let missing = rig.root.join("gone");
        let missing = missing.to_str().unwrap();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let first = start(&rig.daemon, run(&rig, "req-1", missing, &host)).await;
        let again = start(&rig.daemon, run(&rig, "req-1", missing, &host)).await;
        let restarted = daemon_on(&rig.root);
        let after_restart = start(&restarted, run(&rig, "req-1", missing, &host)).await;
        let did_not_start = failed("did not start".into());
        assert!(
            matches!(first, StartSessionResult::Failed { .. }),
            "{first:?}"
        );
        assert_eq!(again, did_not_start);
        assert_eq!(after_restart, did_not_start);
        assert_eq!(runs(&rig).len(), 1);

        // Overlapping: the second asks while the first is still bringing its host up, and
        // gets the first start's own answer when it comes, here a refusal, rather than
        // "started" for a run that is about to fail.
        let cwd = rig.root.to_str().unwrap();
        let refused = stub_host(&rig, 999, None);
        let (one, two) = tokio::join!(
            rig.daemon.starts.once(
                "req-2",
                start(&rig.daemon, run(&rig, "req-2", cwd, &refused))
            ),
            rig.daemon.starts.once(
                "req-2",
                start(&rig.daemon, run(&rig, "req-2", cwd, &refused))
            ),
        );
        assert!(
            matches!(&one, StartSessionResult::Failed { detail, .. } if detail.contains("version 999")),
            "{one:?}"
        );
        assert_eq!(
            one, two,
            "the repeat waited for the first start's own answer"
        );
        let dirs = runs(&rig);
        assert_eq!(dirs.len(), 2);
        for dir in dirs.iter().filter(|dir| dir.join("stub.pid").exists()) {
            stop(dir);
        }
    }

    /// The handshake against a stand-in host: the pid ccd spawned, its kernel birth,
    /// the first state committed before its ack, `start` with the run's options, its
    /// done outcome committed (the run `live`, its initialize answer kept) before its
    /// ack and before the answer, and the run's link attached until the connection ends.
    #[tokio::test]
    async fn a_new_host_is_admitted_attached_and_started() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let daemon = Arc::clone(&rig.daemon);
        let (result, played) = tokio::join!(
            start(
                &daemon,
                Run {
                    worktree: Some("feature-x"),
                    trust: Trust::Trusted,
                    ..run(&rig, "req-1", &cwd, &host)
                }
            ),
            async {
                let mut played = play_to_start(&rig).await;
                let uuid = played.start_uuid();
                say(
                    &mut played.write,
                    serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                       "status": "done", "initialize": initialize_answer()}),
                )
                .await;
                assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 2 }));
                // By the ack, the run is live with its answer kept. (That nothing is
                // acknowledged before its commit is a_start_with_no_committed_outcome's.)
                assert_eq!(
                    kept(&rig, &played.uid),
                    ("live".to_string(), Some(initialize_answer()))
                );
                played
            }
        );
        let Played {
            dir,
            uid,
            mut lines,
            mut write,
            record,
            start,
        } = played;
        assert_eq!(started_uid(&result), uid);
        // A start in a new worktree does not know its folder until claude reports it.
        assert!(
            matches!(&result, StartSessionResult::Started { cwd: None, .. }),
            "{result:?}"
        );
        assert_eq!(cursor(&rig, &uid), 2);
        match start {
            ToHost::Start {
                uuid,
                options,
                prompt,
            } => {
                assert_eq!(uuid.len(), 36);
                assert_eq!(prompt, "fix the tests");
                assert_eq!(options.cwd, cwd);
                assert_eq!(options.claude_path, "/nonexistent/claude");
                assert_eq!(options.permission_mode, None, "the app chose no mode");
                assert_eq!(options.worktree.as_deref(), Some("feature-x"));
                assert_eq!(options.trust, Trust::Trusted, "the start's answer");
                assert_eq!(options.model, None, "Claude Code's own default is no model");
                assert_eq!(
                    options.mail.args[..3],
                    ["mcp", "--session-uid", uid.as_str()]
                );
            }
            other => panic!("no start: {other:?}"),
        }
        // The host is a session leader of its own, so it outlives this ccd.
        // SAFETY: getsid(2) reads one process's session id.
        assert_eq!(unsafe { libc::getsid(record.host_pid) }, record.host_pid);
        let link = || async {
            daemon
                .sessions()
                .await
                .unwrap()
                .into_iter()
                .find(|session| session.session_uid == uid)
                .map(|session| session.link)
        };
        assert_eq!(link().await, Some(protocol::event::Link::Attached));

        // The host sends a line this daemon cannot read, then a state: neither is
        // acknowledged and the cursor stays, so the host keeps both.
        say(
            &mut write,
            serde_json::json!({"type": "unknown_to_this_daemon", "seq": 3}),
        )
        .await;
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 4, "pending": []}),
        )
        .await;
        assert_eq!(next(&mut lines).await, None);
        assert_eq!(cursor(&rig, &uid), 2);
        let detached = async {
            while link().await != Some(protocol::event::Link::Detached) {
                tokio::time::sleep(POLL).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), detached)
            .await
            .expect("the link reads detached once its connection ends");
        stop(&dir);
    }

    /// A start the host refuses: its outcome makes the run `failed`, and the app is
    /// answered with the host's reason. Claude's end after it is acknowledged, so the host
    /// can end.
    #[tokio::test]
    async fn a_refused_start_fails_with_the_hosts_reason() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (result, played) =
            tokio::join!(start(&rig.daemon, run(&rig, "req-1", &cwd, &host)), async {
                let mut played = play_to_start(&rig).await;
                let uuid = played.start_uuid();
                say(
                    &mut played.write,
                    serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                       "status": "refused", "detail": "claude would not start"}),
                )
                .await;
                assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 2 }));
                say(
                    &mut played.write,
                    serde_json::json!({"type": "exited", "seq": 3, "code": 1}),
                )
                .await;
                assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 3 }));
                played
            });
        assert_eq!(result, failed("claude would not start".into()));
        assert_eq!(kept(&rig, &played.uid), ("failed".to_string(), None));
        assert_eq!(cursor(&rig, &played.uid), 3);
        stop(&played.dir);
    }

    /// Once `start` is sent, claude may be running, so a run whose outcome is not
    /// committed stays `starting` while its host runs, and a delete is refused: the app is
    /// answered with the reason, but the run is not marked failed. Three ways: the outcome is
    /// for another command, the connection ends first, and the outcome's commit changes
    /// nothing because the run's recorded host is another. None is acknowledged, and nothing
    /// of the outcome is kept. In the last, the recorded host is gone by its own identity, so
    /// once the link ends the run is `failed` (H7), with no end in its timeline.
    #[tokio::test]
    async fn a_start_with_no_committed_outcome_leaves_its_run_starting() {
        for (case, why, after) in [
            ("another host", "not past the run's cursor", "failed"),
            ("another command", "does not apply", "starting"),
            ("closed", "closed the connection", "starting"),
        ] {
            let rig = rig();
            let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
            let cwd = rig.root.to_str().unwrap().to_string();
            let (result, (dir, uid)) =
                tokio::join!(start(&rig.daemon, run(&rig, "req-1", &cwd, &host)), async {
                    let Played {
                        dir,
                        uid,
                        mut lines,
                        mut write,
                        start,
                        ..
                    } = play_to_start(&rig).await;
                    let uuid = match &start {
                        ToHost::Start { uuid, .. } => uuid.clone(),
                        other => panic!("{other:?}"),
                    };
                    let outcome = |uuid: &str| {
                        serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                           "status": "done", "initialize": initialize_answer()})
                    };
                    match case {
                        "another host" => {
                            rusqlite::Connection::open(rig.root.join("events.db"))
                                .unwrap()
                                .execute(
                                    "UPDATE run_hosts SET host_start_usec = host_start_usec + 1
                                      WHERE session_uid = ?1",
                                    [&uid],
                                )
                                .unwrap();
                            say(&mut write, outcome(&uuid)).await;
                        }
                        "another command" => say(&mut write, outcome("not-the-start")).await,
                        _ => drop(write),
                    }
                    assert_eq!(next(&mut lines).await, None, "{case}: acknowledged");
                    (dir, uid)
                });
            assert!(
                matches!(&result, StartSessionResult::Failed { detail, .. } if detail.contains(why)),
                "{case}: {result:?}"
            );
            until_ended(&rig, &uid, after, None).await;
            assert_eq!(kept(&rig, &uid), (after.to_string(), None), "{case}");
            assert_eq!(cursor(&rig, &uid), 1, "{case}");
            assert_eq!(
                end_reason(&rig, &uid),
                None,
                "{case}: an end in a run that never started"
            );
            if after == "starting" {
                let deleted = rig.daemon.db.delete_exited_session(uid).await.unwrap();
                assert!(
                    matches!(deleted, crate::store::DeleteOutcome::NotExited { .. }),
                    "{case}: {deleted:?}"
                );
            }
            stop(&dir);
        }
    }

    /// 97's condition on the start's deadline: the app is answered `Started` once it
    /// passes with no outcome, and the run's link keeps applying what comes, so a late
    /// outcome still makes the run `live` (with its answer kept) or `failed`.
    #[tokio::test]
    async fn a_late_outcome_still_ends_the_run() {
        for (status, lifecycle) in [("done", "live"), ("refused", "failed")] {
            let rig = rig();
            let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
            let cwd = rig.root.to_str().unwrap().to_string();
            let (result, played) = tokio::join!(
                start_within(
                    &rig.daemon,
                    run(&rig, "req-1", &cwd, &host),
                    Duration::from_secs(3)
                ),
                async {
                    let mut played = play_to_start(&rig).await;
                    let uuid = played.start_uuid();
                    // Past the start's deadline: the answer has gone.
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    assert_eq!(kept(&rig, &played.uid).0, "starting");
                    say(
                        &mut played.write,
                        serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                           "status": status, "detail": "late",
                                           "initialize": (status == "done").then(initialize_answer)}),
                    )
                    .await;
                    assert_eq!(
                        next(&mut played.lines).await,
                        Some(ToHost::Ack { seq: 2 }),
                        "{status}"
                    );
                    played
                }
            );
            assert_eq!(started_uid(&result), played.uid, "{status}");
            assert_eq!(
                kept(&rig, &played.uid),
                (
                    lifecycle.to_string(),
                    (status == "done").then(initialize_answer)
                ),
                "{status}"
            );
            stop(&played.dir);
        }
    }

    /// The host gets the run's environment as it was given, with only the run's uid
    /// added: a sign-in variable arrives unchanged, and nothing sets a Claude config
    /// folder. (That the claude check read this same map is the arm's test,
    /// `start_session::tests::the_check_and_the_host_see_one_environment`.)
    #[tokio::test]
    async fn the_host_gets_its_runs_environment_as_given() {
        let mut rig = rig();
        rig.environment.insert(
            OsString::from("ANTHROPIC_API_KEY"),
            OsString::from("sk-test-marker"),
        );
        let host = rig.root.join("env-host");
        std::fs::write(&host, "#!/bin/sh\nenv > \"$1/env.txt\"\n").unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cwd = rig.root.to_str().unwrap().to_string();
        let result = start(&rig.daemon, run(&rig, "req-1", &cwd, &host)).await;
        assert!(
            matches!(result, StartSessionResult::Failed { .. }),
            "{result:?}"
        );
        let dir = runs(&rig).pop().unwrap();
        let uid = dir.file_name().unwrap().to_str().unwrap().to_string();
        let seen = std::fs::read_to_string(dir.join("env.txt")).unwrap();
        let lines: Vec<&str> = seen.lines().collect();
        assert!(
            lines.contains(&"ANTHROPIC_API_KEY=sk-test-marker"),
            "{seen}"
        );
        assert!(
            lines.contains(&format!("CODECONNECT_SESSION_UID={uid}").as_str()),
            "{seen}"
        );
        assert!(!seen.contains("CLAUDE_CONFIG_DIR"), "{seen}");
        let home = format!("HOME={}", rig.root.join("home").display());
        assert!(lines.contains(&home.as_str()), "{seen}");
    }

    /// A state whose commit changes nothing (the run's recorded host is another) is
    /// not acknowledged.
    #[tokio::test]
    async fn a_message_whose_commit_changes_nothing_is_not_acknowledged() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (result, ()) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-1", &cwd, &host)),
            async {
                let dir = loop {
                    if let Some(dir) = runs(&rig)
                        .into_iter()
                        .find(|dir| dir.join("host.json").exists())
                    {
                        break dir;
                    }
                    tokio::time::sleep(POLL).await;
                };
                let (mut lines, mut write, _) = play_host(&dir).await;
                assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 0 }));
                let uid = dir.file_name().unwrap().to_str().unwrap().to_string();
                rusqlite::Connection::open(rig.root.join("events.db"))
                .unwrap()
                .execute(
                    "UPDATE run_hosts SET host_start_usec = host_start_usec + 1 WHERE session_uid = ?1",
                    [&uid],
                )
                .unwrap();
                say(
                    &mut write,
                    serde_json::json!({"type": "state", "seq": 1, "pending": []}),
                )
                .await;
                assert_eq!(
                    next(&mut lines).await,
                    None,
                    "nothing after an uncommitted state"
                );
                stop(&dir);
            }
        );
        assert!(
            matches!(&result, StartSessionResult::Failed { detail, .. } if detail.contains("commit")),
            "{result:?}"
        );
    }

    /// A host.json naming a process ccd did not spawn, or a protocol version it does
    /// not speak, is refused before ccd attaches: nothing is sent and the run fails.
    #[tokio::test]
    async fn a_host_file_ccd_cannot_admit_gets_no_start() {
        for (version, pid, why) in [
            (
                HOST_PROTOCOL_VERSION,
                Some(std::process::id() as i32),
                "did not start",
            ),
            (999, None, "version 999"),
        ] {
            let rig = rig();
            let host = stub_host(&rig, version, pid);
            let cwd = rig.root.to_str().unwrap().to_string();
            let (result, connected) =
                tokio::join!(start(&rig.daemon, run(&rig, "req-1", &cwd, &host)), async {
                    let dir = loop {
                        if let Some(dir) = runs(&rig)
                            .into_iter()
                            .find(|dir| dir.join("host.json").exists())
                        {
                            break dir;
                        }
                        tokio::time::sleep(POLL).await;
                    };
                    tokio::time::sleep(POLL).await;
                    let listener = UnixListener::bind(dir.join("host.sock")).unwrap();
                    let connected = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                        .await
                        .is_ok();
                    stop(&dir);
                    connected
                });
            assert!(
                matches!(&result, StartSessionResult::Failed { detail, .. } if detail.contains(why)),
                "{result:?}"
            );
            assert!(!connected, "ccd attached to a host it should have refused");
        }
    }

    /// One real session's SDK messages (SDK 0.3.322, claude 2.1.322), as a host relays
    /// them, and the events the mapper makes of them.
    const CAPTURE: &str = include_str!("../../../fixtures/claude/sdk-session-0.3.322.jsonl");
    const WIRE: &str = include_str!("../../../fixtures/claude/sdk-session-wire.json");

    fn capture() -> Vec<serde_json::Value> {
        CAPTURE
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// A run a stand-in host has made `live` (its done outcome is seq 2), with the
    /// host's side of the socket open.
    async fn live_run(rig: &Rig) -> Played {
        live_run_on(rig, &stub_host(rig, HOST_PROTOCOL_VERSION, None)).await
    }

    /// [`live_run`] with the stand-in host `host`.
    async fn live_run_on(rig: &Rig, host: &Path) -> Played {
        let host = host.to_path_buf();
        let cwd = rig.root.to_str().unwrap().to_string();
        let daemon = Arc::clone(&rig.daemon);
        let (result, played) =
            tokio::join!(start(&daemon, run(rig, "req-live", &cwd, &host)), async {
                let mut played = play_to_start(rig).await;
                let uuid = played.start_uuid();
                say(
                    &mut played.write,
                    serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                   "status": "done", "initialize": initialize_answer()}),
                )
                .await;
                assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 2 }));
                played
            });
        assert_eq!(started_uid(&result), played.uid);
        played
    }

    async fn sdk(write: &mut OwnedWriteHalf, seq: u64, message: serde_json::Value) {
        say(
            write,
            serde_json::json!({"type": "sdk", "seq": seq, "message": message}),
        )
        .await;
    }

    /// The run's stored events, oldest first, as an app reads each (the mapper's
    /// fixture's shape).
    fn stored(rig: &Rig, uid: &str) -> Vec<serde_json::Value> {
        let conn = rusqlite::Connection::open(rig.root.join("events.db")).unwrap();
        let mut statement = conn
            .prepare(
                "SELECT kind, payload, source, source_event_id, turn_id, item_id FROM events
                  WHERE session_uid = ?1 ORDER BY seq",
            )
            .unwrap();
        let rows = statement
            .query_map([uid], |row| {
                let payload: String = row.get(1)?;
                Ok(serde_json::json!({
                    "kind": row.get::<_, String>(0)?,
                    "payload": serde_json::from_str::<serde_json::Value>(&payload).unwrap(),
                    "source": row.get::<_, String>(2)?,
                    "source_event_id": row.get::<_, Option<String>>(3)?,
                    "turn_id": row.get::<_, Option<String>>(4)?,
                    "item_id": row.get::<_, Option<String>>(5)?,
                }))
            })
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    /// The `link` of each of the run's [`stored`] `link_state` facts, oldest first.
    fn link_facts(rig: &Rig, uid: &str) -> Vec<String> {
        stored(rig, uid)
            .iter()
            .filter(|event| event["kind"] == "link_state")
            .map(|event| event["payload"]["link"].as_str().unwrap().to_string())
            .collect()
    }

    /// A live run's [`stored`] events after the person's first words, which the start's
    /// outcome filed first.
    fn after_words(rig: &Rig, uid: &str) -> Vec<serde_json::Value> {
        let mut stored = stored(rig, uid);
        // The link's own facts (`link_state`) are not the run's messages.
        stored.retain(|event| event["kind"] != "link_state");
        assert_eq!(
            stored[0]["kind"], "user_message",
            "the person's words first"
        );
        assert_eq!(
            stored[0]["payload"],
            serde_json::json!({"text": "fix the tests"})
        );
        stored.split_off(1)
    }

    /// The run row's Claude session id and folder.
    fn row_of(rig: &Rig, uid: &str) -> (Option<String>, String) {
        rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .query_row(
                "SELECT claude_session_id, cwd FROM sessions WHERE session_uid = ?1",
                [uid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    /// A test-only trigger on the run's store, so a write fails where a test needs it.
    fn trigger(rig: &Rig, sql: &str) {
        rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    async fn summary(daemon: &Daemon, uid: &str) -> protocol::event::SessionSummary {
        daemon
            .sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|session| session.session_uid == uid)
            .unwrap()
    }

    /// **A live run's SDK messages are its events (G1, G2, G3).** The recorded session's
    /// 150 messages, each filed with the run's cursor in one transaction, then
    /// acknowledged: by each ack the cursor is that message's seq. The first, a
    /// command's `queued`, files nothing. After the link's attach and the person's first
    /// words, the run's events are the mapper's, each turn's carrying its init's uuid, each
    /// broadcast once and in order. The session id stays
    /// through the `/clear` (line 133) and is the next init's from line 135. A message
    /// sent again with its seq files nothing and is not acknowledged: the link ends.
    #[tokio::test]
    async fn a_live_run_files_its_sdk_messages_with_their_cursor_then_acknowledges_them() {
        let rig = rig();
        let mut broadcast = rig.daemon.events_tx.subscribe();
        let mut played = live_run(&rig).await;
        let messages = capture();
        let mut ids = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let seq = index as u64 + 3;
            sdk(&mut played.write, seq, message.clone()).await;
            assert_eq!(
                next(&mut played.lines).await,
                Some(ToHost::Ack { seq }),
                "line {}",
                index + 1
            );
            assert_eq!(cursor(&rig, &played.uid), seq as i64, "line {}", index + 1);
            if index == 0 {
                assert!(after_words(&rig, &played.uid).is_empty(), "the queued line");
            }
            ids.push(row_of(&rig, &played.uid).0);
        }
        let first_id = messages[2]["session_id"].as_str().map(str::to_string);
        assert_eq!(ids[2], first_id, "after the first init");
        assert_eq!(ids[132], first_id, "after the /clear");
        assert_eq!(
            ids[134],
            messages[134]["session_id"].as_str().map(str::to_string),
            "after the next init"
        );
        assert_ne!(ids[134], first_id);
        let committed: serde_json::Value = serde_json::from_str(WIRE).unwrap();
        assert_eq!(
            serde_json::Value::Array(after_words(&rig, &played.uid)),
            committed
        );
        let mut seqs = Vec::new();
        while let Ok(event) = broadcast.try_recv() {
            if event.session_uid == played.uid {
                seqs.push(event.seq);
            }
        }
        assert_eq!(seqs, (1..=113).collect::<Vec<u64>>(), "each once, in order");

        let last = messages.len() as u64 + 2;
        sdk(&mut played.write, last, messages.last().unwrap().clone()).await;
        assert_eq!(next(&mut played.lines).await, None);
        assert_eq!(after_words(&rig, &played.uid).len(), 111);
        assert_eq!(cursor(&rig, &played.uid), last as i64);
        stop(&played.dir);
    }

    /// **G1's injected failure:** a result's second event cannot be stored (a test
    /// trigger aborts its insert). Its first event, already inserted, goes too; the
    /// cursor stays, none of its events is broadcast, and the message is not acknowledged:
    /// the link ends there while the run is live, so its detach is all that is broadcast.
    #[tokio::test]
    async fn a_message_whose_events_cannot_all_be_stored_files_none() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        trigger(
            &rig,
            "CREATE TRIGGER no_usage BEFORE INSERT ON events WHEN NEW.kind = 'usage'
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        );
        let mut broadcast = rig.daemon.events_tx.subscribe();
        sdk(
            &mut played.write,
            3,
            serde_json::json!({"type": "result", "subtype": "success", "uuid": "r1",
                               "is_error": false, "num_turns": 1, "usage": {}}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, None);
        assert!(after_words(&rig, &played.uid).is_empty());
        assert_eq!(cursor(&rig, &played.uid), 2);
        let event = broadcast.try_recv().unwrap();
        assert_eq!(
            (event.kind, event.payload["link"].as_str()),
            (EventKind::LinkState, Some("detached"))
        );
        assert!(broadcast.try_recv().is_err());
        stop(&played.dir);
    }

    /// **G3's injected failure:** an init whose session id cannot be kept (a test
    /// trigger aborts that update) is kept back whole: no event, the cursor and the
    /// row's id as they were, and no acknowledgement.
    #[tokio::test]
    async fn an_init_whose_session_id_cannot_be_kept_is_kept_back_whole() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        trigger(
            &rig,
            "CREATE TRIGGER no_id BEFORE UPDATE OF claude_session_id ON sessions
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        );
        sdk(&mut played.write, 3, capture()[2].clone()).await;
        assert_eq!(next(&mut played.lines).await, None);
        assert!(after_words(&rig, &played.uid).is_empty());
        assert_eq!(cursor(&rig, &played.uid), 2);
        assert_eq!(row_of(&rig, &played.uid).0, None);
        stop(&played.dir);
    }

    /// **G2b:** an SDK message is applied only in a live run. Before the start's outcome,
    /// and after a refusal, the host sends none (the outcome first; nothing but claude's
    /// end after a refusal), and one that came would file nothing and not be
    /// acknowledged: the link ends there (G5b), so the host keeps it.
    #[tokio::test]
    async fn an_sdk_message_outside_a_live_run_is_not_applied() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let daemon = Arc::clone(&rig.daemon);
        let (_, uid) = tokio::join!(start(&daemon, run(&rig, "req-early", &cwd, &host)), async {
            let mut played = play_to_start(&rig).await;
            sdk(&mut played.write, 2, capture()[2].clone()).await;
            assert_eq!(next(&mut played.lines).await, None);
            let mut ended = false;
            for _ in 0..100 {
                if summary(&rig.daemon, &played.uid).await.link != protocol::event::Link::Attached {
                    ended = true;
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
            assert!(ended, "the link stayed up past a message it does not apply");
            stop(&played.dir);
            played.uid
        });
        // Nothing but the link's attach: no message was applied.
        let filed: Vec<(serde_json::Value, serde_json::Value)> = stored(&rig, &uid)
            .iter()
            .map(|event| (event["kind"].clone(), event["payload"]["link"].clone()))
            .collect();
        assert_eq!(
            filed,
            [(
                serde_json::json!("link_state"),
                serde_json::json!("attached")
            )]
        );
        assert_eq!(cursor(&rig, &uid), 1);

        let rig = self::rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let daemon = Arc::clone(&rig.daemon);
        let (_, uid) = tokio::join!(
            start(&daemon, run(&rig, "req-refused", &cwd, &host)),
            async {
                let mut played = play_to_start(&rig).await;
                let uuid = played.start_uuid();
                say(
                    &mut played.write,
                    serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                   "status": "refused", "detail": "claude refused it"}),
                )
                .await;
                assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 2 }));
                sdk(&mut played.write, 3, capture()[2].clone()).await;
                assert_eq!(next(&mut played.lines).await, None);
                stop(&played.dir);
                played.uid
            }
        );
        // Only the link's attach again: a failed run's link ends with no detach.
        assert_eq!(link_facts(&rig, &uid), ["attached"]);
        assert_eq!(stored(&rig, &uid).len(), 1);
        assert_eq!(cursor(&rig, &uid), 2);
    }

    /// **G3b:** a run is in the folder its init names: a new worktree's, which claude
    /// made, rather than the folder it was started from. Once that message is
    /// acknowledged the row and the fleet say so; a later init naming it again changes
    /// nothing.
    #[tokio::test]
    async fn a_run_is_in_the_folder_its_init_names() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let started = rig.root.to_str().unwrap().to_string();
        assert_eq!(row_of(&rig, &played.uid).1, started);
        let worktree = rig.root.join(".claude/worktrees/feature-x");
        let mut init = capture()[2].clone();
        init["cwd"] = serde_json::json!(worktree.to_str().unwrap());
        sdk(&mut played.write, 3, init.clone()).await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 3 }));
        assert_eq!(row_of(&rig, &played.uid).1, worktree.to_str().unwrap());
        assert_eq!(
            summary(&rig.daemon, &played.uid).await.cwd,
            worktree.to_str().unwrap()
        );
        init["uuid"] = serde_json::json!("a-later-init");
        sdk(&mut played.write, 4, init).await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 4 }));
        assert_eq!(row_of(&rig, &played.uid).1, worktree.to_str().unwrap());
        stop(&played.dir);
    }

    /// **G4:** the init's permission mode is the run's, with that init's event's seq,
    /// live and as a restarted daemon rebuilds it from the store.
    #[tokio::test]
    async fn the_inits_permission_mode_is_the_runs_live_and_after_a_restart() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        for (index, message) in capture().into_iter().take(3).enumerate() {
            let seq = index as u64 + 3;
            sdk(&mut played.write, seq, message).await;
            assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
        }
        // The link's attach is the run's first fact, and the person's words its second.
        let mode = protocol::event::Fact {
            value: capture()[2]["permissionMode"].as_str().unwrap().to_string(),
            seq: 3,
        };
        assert_eq!(
            summary(&rig.daemon, &played.uid).await.permission_mode,
            Some(mode.clone())
        );
        stop(&played.dir);
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        assert_eq!(
            summary(&restarted, &played.uid).await.permission_mode,
            Some(mode)
        );
    }

    /// A restarted ccd on the rig's store: the connection that started the run is gone,
    /// its host (the test) listens again at the run's socket, and a new daemon starts as
    /// ccd's start does: `recover`, then the hosted runs. The host's side of the connection
    /// the new daemon attached with.
    async fn restart(rig: &Rig, dir: &Path) -> (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf) {
        let (_, lines, write) = restarted(rig, dir).await;
        (lines, write)
    }

    /// [`restart`], with the restarted daemon.
    async fn restarted(
        rig: &Rig,
        dir: &Path,
    ) -> (Arc<Daemon>, Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf) {
        let _ = std::fs::remove_file(dir.join("host.sock"));
        let listener = UnixListener::bind(dir.join("host.sock")).unwrap();
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        restarted.recover_hosted_runs(&rig.root).await;
        let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("the restarted daemon attached the run's host")
            .unwrap();
        let (read, write) = stream.into_split();
        (restarted, BufReader::new(read).lines(), write)
    }

    /// A run's row and its stored start, as a start answered `trust` writes them before
    /// anything is spawned: what a ccd that died then leaves.
    async fn begun(rig: &Rig, cwd: &str, host: &Path, trust: Trust) -> (String, ToHost) {
        let uid = protocol::uid::new().unwrap();
        let start = ToHost::Start {
            uuid: protocol::uid::uuid_v4().unwrap(),
            options: start_options(
                &uid,
                &Run {
                    trust,
                    ..run(rig, "req-1", cwd, host)
                },
            ),
            prompt: "fix the tests".into(),
        };
        let now = protocol::time::now_rfc3339();
        let row = SessionRow {
            session_uid: uid.clone(),
            session_id: uid.clone(),
            tmux_session: String::new(),
            tmux_socket: String::new(),
            cwd: cwd.to_string(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: Lifecycle::Starting,
            created_at: now.clone(),
            updated_at: now,
            agent: AgentKind::Claude,
            codex_thread_id: None,
            codex_socket: None,
        };
        let line = serde_json::to_string(&start).unwrap();
        assert_eq!(
            rig.daemon
                .db
                .begin_run(row, "req-1".into(), Some(line))
                .await
                .unwrap(),
            None
        );
        (uid, start)
    }

    /// **G1: a live run across a restart.** Its host outlives ccd. The restarted ccd
    /// attaches it again from the run's cursor and files what the host replays, each
    /// message once and in order, then claude's end, acknowledging through the attach's
    /// new `state`. It sends no start.
    #[tokio::test]
    async fn a_restarted_daemon_attaches_a_live_runs_host_again() {
        let rig = rig();
        let played = live_run(&rig).await;
        let (uid, dir) = (played.uid.clone(), played.dir.clone());
        drop(played);
        // The first daemon stays up in this test, so it sees its connection drop while the
        // run is live and files a detach; a ccd that restarts is gone and files none. Let
        // that detach land before the restarted daemon attaches.
        for _ in 0..100 {
            if link_facts(&rig, &uid).len() == 2 {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        let (mut lines, mut write) = restart(&rig, &dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 2 }));
        for (index, message) in capture().into_iter().take(3).enumerate() {
            let seq = index as u64 + 3;
            sdk(&mut write, seq, message).await;
            assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq }));
        }
        say(
            &mut write,
            serde_json::json!({"type": "exited", "seq": 6, "code": 0}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 6 }));
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 7, "pending": []}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 7 }));
        drop(write);
        assert_eq!(next(&mut lines).await, None, "no start for a live run");
        stop(&dir);
        assert_eq!(kept(&rig, &uid).0, "exited");
        assert_eq!(cursor(&rig, &uid), 7);
        let key = SessionKey::new(&uid, &uid);
        let mut expected = vec!["user_message".to_string()];
        expected.extend(
            capture()
                .iter()
                .take(3)
                .flat_map(|message| protocol::sdk_map::map(&key, None, message).events)
                .map(|event| event.kind.as_str().to_string()),
        );
        expected.push("session_end".into());
        let filed: Vec<String> = stored(&rig, &uid)
            .iter()
            .filter(|event| event["kind"] != "link_state")
            .map(|event| event["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(filed, expected);
        // The run's link facts: attached by the first daemon, detached when this test drops
        // that daemon's connection while the run is live, and attached again by the restarted
        // daemon. The run's end, after its host said it exited, adds none.
        assert_eq!(link_facts(&rig, &uid), ["attached", "detached", "attached"]);
    }

    /// **G1, a run still starting across a restart:** its start was sent and its outcome
    /// not committed. The host replays the outcome before its new `state`, so by the end of
    /// the replay the run is `live`, and it is not sent its start again.
    #[tokio::test]
    async fn a_starting_runs_replayed_outcome_is_applied_and_no_start_is_sent() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (result, played) = tokio::join!(
            start_within(
                &rig.daemon,
                run(&rig, "req-1", &cwd, &host),
                Duration::from_secs(2)
            ),
            play_to_start(&rig)
        );
        let uid = started_uid(&result);
        let (dir, uuid) = (played.dir.clone(), played.start_uuid());
        drop(played);
        assert_eq!(kept(&rig, &uid).0, "starting");
        let (mut lines, mut write) = restart(&rig, &dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 1 }));
        say(
            &mut write,
            serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                               "status": "done", "initialize": initialize_answer()}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 2 }));
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 3, "pending": []}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 3 }));
        drop(write);
        assert_eq!(
            next(&mut lines).await,
            None,
            "no start once the outcome is in"
        );
        stop(&dir);
        assert_eq!(kept(&rig, &uid).0, "live");
    }

    /// **G2: a start its host never had.** A ccd died after it recorded the run's host and
    /// before it sent `start`, while the host was not yet listening. The restarted ccd waits
    /// for the socket, attaches, and at the host's first `state` sends the stored start. It
    /// carries the answer the run started with: the folder's answer changed since (B10's
    /// G2) is the next start's, not this run's.
    #[tokio::test]
    async fn a_restarted_daemon_sends_a_start_its_host_never_had() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (uid, start) = begun(&rig, &cwd, &host, Trust::Trusted).await;
        let folder = std::fs::canonicalize(&cwd).unwrap();
        rig.daemon
            .db
            .set_folder_trust(folder.to_string_lossy().into_owned(), Trust::NotTrusted)
            .await
            .unwrap();
        let dir = run_dir(&rig.root, &uid);
        std::fs::create_dir_all(&dir).unwrap();
        let mut stub = std::process::Command::new(&host).arg(&dir).spawn().unwrap();
        let pid = loop {
            if let Some(record) = read_host_file(&dir.join("host.json")) {
                break record.host_pid;
            }
            tokio::time::sleep(POLL).await;
        };
        rig.daemon
            .db
            .record_host(
                uid.clone(),
                ProcessIdentity {
                    pid,
                    birth: read_birth_identity(pid).unwrap(),
                },
                protocol::proc_identity::boot_identity(),
            )
            .await
            .unwrap();
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        restarted.recover_hosted_runs(&rig.root).await;
        // The host listens only now, after the restarted ccd first tried its socket.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let listener = UnixListener::bind(dir.join("host.sock")).unwrap();
        let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
        stop(&dir);
        let _ = stub.wait();
        let (stream, _) = accepted
            .expect("the restarted daemon attached the run's host")
            .unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 0 }));
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 1, "pending": []}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 1 }));
        let sent = next(&mut lines).await;
        assert!(
            matches!(&sent, Some(ToHost::Start { options, .. }) if options.trust == Trust::Trusted),
            "the run's own answer, not the folder's answer now: {sent:?}"
        );
        assert_eq!(sent, Some(start));
    }

    /// **G3: a run with no host.** A ccd died after the run's row and before it recorded the
    /// run's host, so no start was sent: the restarted ccd fails the run.
    #[tokio::test]
    async fn a_starting_run_with_no_recorded_host_fails_at_a_restart() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (uid, _) = begun(&rig, &cwd, &host, Trust::NotTrusted).await;
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        restarted.recover_hosted_runs(&rig.root).await;
        assert_eq!(kept(&rig, &uid).0, "failed");
    }

    /// **G9: an end not acknowledged.** Claude's end is committed; the ack is lost with ccd.
    /// The restarted ccd attaches the host again from the end's cursor, which the host takes
    /// as the end's acknowledgement, so it can exit.
    #[tokio::test]
    async fn an_ended_runs_host_is_attached_again_to_drain_it() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        say(
            &mut played.write,
            serde_json::json!({"type": "exited", "seq": 3, "code": 0}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 3 }));
        let (uid, dir) = (played.uid.clone(), played.dir.clone());
        drop(played);
        let (mut lines, write) = restart(&rig, &dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 3 }));
        // The host, drained, exits with nothing more to say.
        drop(write);
        assert_eq!(next(&mut lines).await, None);
        stop(&dir);
        assert_eq!(kept(&rig, &uid).0, "exited");
        assert_eq!(end_reason(&rig, &uid), Some(serde_json::Value::Null));
    }

    /// **G9, a refused start's end still to come:** the refusal is committed and claude's
    /// `exited` is not. The restarted ccd's link knows from the store that the run failed,
    /// so it applies the replayed `exited` and the new `state`, and the host can exit.
    #[tokio::test]
    async fn a_refused_runs_later_end_is_applied_after_a_restart() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (result, played) =
            tokio::join!(start(&rig.daemon, run(&rig, "req-1", &cwd, &host)), async {
                let mut played = play_to_start(&rig).await;
                let uuid = played.start_uuid();
                say(
                    &mut played.write,
                    serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                       "status": "refused", "detail": "claude would not start"}),
                )
                .await;
                assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 2 }));
                played
            });
        assert_eq!(result, failed("claude would not start".into()));
        let (uid, dir) = (played.uid.clone(), played.dir.clone());
        drop(played);
        let (mut lines, mut write) = restart(&rig, &dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 2 }));
        say(
            &mut write,
            serde_json::json!({"type": "exited", "seq": 3, "code": 1}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 3 }));
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 4, "pending": []}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 4 }));
        drop(write);
        assert_eq!(next(&mut lines).await, None, "no start for a refused run");
        stop(&dir);
        assert_eq!(kept(&rig, &uid).0, "failed");
        assert_eq!(cursor(&rig, &uid), 4);
        assert_eq!(
            end_reason(&rig, &uid),
            None,
            "a run that never started has no end"
        );
    }

    /// **G12: the turn across a restart.** A live run's first turn and the start of its
    /// second, with the second's init, are filed, then ccd restarts. The rest of the second
    /// turn, with no init of its own, is filed with the second init's uuid as its turn.
    #[tokio::test]
    async fn a_turn_begun_before_a_restart_keeps_its_id_after_it() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let capture = capture();
        assert_eq!(capture[2]["subtype"], "init");
        assert_eq!(capture[12]["subtype"], "init");
        for (index, message) in capture.iter().take(17).enumerate() {
            let seq = index as u64 + 3;
            sdk(&mut played.write, seq, message.clone()).await;
            assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
        }
        let (uid, dir) = (played.uid.clone(), played.dir.clone());
        let before = stored(&rig, &uid).len();
        drop(played);
        let (mut lines, mut write) = restart(&rig, &dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 19 }));
        for (index, message) in capture.iter().enumerate().take(22).skip(17) {
            assert_ne!(message["subtype"], "init");
            let seq = index as u64 + 3;
            sdk(&mut write, seq, message.clone()).await;
            assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq }));
        }
        stop(&dir);
        let continued: Vec<serde_json::Value> = stored(&rig, &uid)
            .split_off(before)
            .into_iter()
            .filter(|event| event["kind"] != "link_state")
            .collect();
        assert!(!continued.is_empty(), "the rest of the turn is filed");
        for event in continued {
            assert_eq!(event["turn_id"], capture[12]["uuid"], "{event}");
        }
    }

    /// **G5:** nothing tied to the person's account reaches the store through the link:
    /// an init carrying claude.ai connectors, a result with its login's expiry, and a
    /// rate-limit message, each acknowledged, the last filing nothing at all.
    #[tokio::test]
    async fn nothing_tied_to_the_account_reaches_the_store() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let account = [
            serde_json::json!({"type": "system", "subtype": "init", "uuid": "i1",
                "session_id": "s1", "cwd": "/tmp", "model": "claude-opus-5-5",
                "permissionMode": "default", "apiKeySource": "none",
                "mcp_servers": [{"name": "claude.ai Example", "source": "claudeai",
                                 "status": "connected"}],
                "tools": ["Bash", "mcp__claude_ai_Example__search"],
                "slash_commands": ["compact", "mcp__claude_ai_Example__prompt"]}),
            serde_json::json!({"type": "result", "subtype": "success", "uuid": "r1",
                "is_error": false, "usage": {"input_tokens": 1}, "total_cost_usd": 0.01,
                "credential_expires_at": 1_800_000_000_000_u64}),
            serde_json::json!({"type": "rate_limit_event", "uuid": "l1",
                "rate_limit_info": {"status": "allowed"}}),
        ];
        let mut filed = Vec::new();
        for (index, message) in account.into_iter().enumerate() {
            let seq = index as u64 + 3;
            sdk(&mut played.write, seq, message).await;
            assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
            filed.push(after_words(&rig, &played.uid).len());
        }
        assert_eq!(filed, [1, 3, 3], "the rate-limit message files nothing");
        let text = serde_json::to_string(&stored(&rig, &played.uid)).unwrap();
        for field in [
            "mcp_servers",
            "\"tools\"",
            "slash_commands",
            "apiKeySource",
            "rate_limit_info",
            "credential_expires_at",
            "claude.ai Example",
            "mcp__claude_ai_Example",
        ] {
            assert!(!text.contains(field), "{field} reached the store");
        }
        stop(&played.dir);
    }

    /// **G13: claude's own end (H6).** A live run's `exited` is filed with the run's cursor
    /// in one transaction: the row `exited` and one `SessionEnd`, which a receiver already
    /// subscribed gets once. Then it is acknowledged, so the host can end.
    #[tokio::test]
    async fn a_live_runs_own_end_is_filed_with_its_cursor_then_acknowledged() {
        let rig = rig();
        let mut broadcast = rig.daemon.events_tx.subscribe();
        let mut played = live_run(&rig).await;
        say(
            &mut played.write,
            serde_json::json!({"type": "exited", "seq": 3, "code": 0}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 3 }));
        assert_eq!(kept(&rig, &played.uid).0, "exited");
        assert_eq!(cursor(&rig, &played.uid), 3);
        let ends: Vec<serde_json::Value> = stored(&rig, &played.uid)
            .into_iter()
            .filter(|event| event["kind"] == "session_end")
            .collect();
        assert_eq!(ends.len(), 1, "{ends:?}");
        assert_eq!(ends[0]["payload"], serde_json::json!({"exit_code": 0}));
        let mut seen = 0;
        while let Ok(event) = broadcast.try_recv() {
            if event.session_uid == played.uid
                && event.kind == protocol::event::EventKind::SessionEnd
            {
                seen += 1;
            }
        }
        assert_eq!(seen, 1, "the subscribed receiver gets the end once");
        stop(&played.dir);
    }

    /// **G13's injected failure:** an end that cannot be stored (a test trigger aborts its
    /// event's insert) changes nothing: the row stays `live`, the cursor where it was, and
    /// the end is not acknowledged, so the host keeps it.
    #[tokio::test]
    async fn an_end_that_cannot_be_stored_changes_nothing() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        trigger(
            &rig,
            "CREATE TRIGGER no_end BEFORE INSERT ON events WHEN NEW.kind = 'session_end'
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        );
        say(
            &mut played.write,
            serde_json::json!({"type": "exited", "seq": 3, "code": 0}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, None);
        assert_eq!(kept(&rig, &played.uid).0, "live");
        assert_eq!(cursor(&rig, &played.uid), 2);
        stop(&played.dir);
    }

    /// **G4: the host dies (H7).** The host is SIGKILLed while its group holds a child, a
    /// decoy runs outside the group, and the run's connection ends. ccd finds the host
    /// gone: the group is signalled, its child, which ignores SIGTERM, is gone by the
    /// ladder's SIGKILL, the decoy lives, and the run is `exited` with H7's reason.
    #[tokio::test]
    async fn a_dead_hosts_group_is_stopped_and_its_run_ends() {
        let rig = rig();
        let host = stub_host_with_child(&rig);
        let mut decoy = std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let Played {
            dir,
            uid,
            lines,
            write,
            ..
        } = live_run_on(&rig, &host).await;
        let child = pid_in(&dir, "child.pid");
        // SAFETY: kill(2) of the stand-in host this test started, by its recorded pid.
        unsafe { libc::kill(pid_in(&dir, "stub.pid"), libc::SIGKILL) };
        drop((lines, write));
        for _ in 0..150 {
            if kept(&rig, &uid).0 == "exited" && !running(child) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let child_ran = running(child);
        let decoy_ran = running(decoy.id() as i32);
        let _ = decoy.kill();
        let _ = decoy.wait();
        if child_ran {
            // SAFETY: kill(2) of the child this test's stand-in started, by its pid.
            unsafe { libc::kill(child, libc::SIGKILL) };
        }
        assert!(!child_ran, "the host's group outlived it");
        assert!(decoy_ran, "a process outside the group was signalled");
        assert_eq!(kept(&rig, &uid).0, "exited");
        let ends: Vec<serde_json::Value> = stored(&rig, &uid)
            .into_iter()
            .filter(|event| event["kind"] == "session_end")
            .collect();
        assert_eq!(ends.len(), 1, "{ends:?}");
        assert_eq!(
            ends[0]["payload"]["reason"],
            serde_json::json!("Stopped: its process ended")
        );
    }

    /// A live run whose link has ended with its host still running: the host sent a
    /// line this daemon cannot read. Its folder and uid.
    async fn detached_run(rig: &Rig, host: &Path) -> (PathBuf, String) {
        let Played {
            dir,
            uid,
            mut lines,
            mut write,
            ..
        } = live_run_on(rig, host).await;
        say(
            &mut write,
            serde_json::json!({"type": "unknown_to_this_daemon", "seq": 3}),
        )
        .await;
        assert_eq!(next(&mut lines).await, None);
        for _ in 0..100 {
            let session = summary(&rig.daemon, &uid).await;
            if session.link == protocol::event::Link::Detached {
                return (dir, uid);
            }
            tokio::time::sleep(POLL).await;
        }
        panic!("the run's link never read detached");
    }

    /// A process leading a group of its own (`setsid`) with a child in it: a host this
    /// test controls, which it reaps itself. The child's pid is written to `child`.
    fn leader(child: &Path) -> (std::process::Child, i32) {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 60 & echo $! > \"$0\"; exec sleep 60")
            .arg(child);
        // SAFETY: setsid(2) is async-signal-safe and touches nothing of the parent's.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut leader = command.spawn().unwrap();
        for _ in 0..100 {
            if let Ok(pid) = std::fs::read_to_string(child) {
                if let Ok(pid) = pid.trim().parse() {
                    return (leader, pid);
                }
            }
            std::thread::sleep(POLL);
        }
        let _ = leader.kill();
        let _ = leader.wait();
        panic!("the leader's child never wrote its pid");
    }

    /// Record `host`, in `boot`, as the run's host, in place of the one ccd recorded.
    fn record_as_host(rig: &Rig, uid: &str, host: ProcessIdentity, boot: BootIdentity) {
        rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .execute(
                "UPDATE run_hosts SET host_pid = ?2, host_start_sec = ?3, host_start_usec = ?4,
                                      boot_sec = ?5, boot_usec = ?6
                  WHERE session_uid = ?1",
                rusqlite::params![
                    uid,
                    host.pid,
                    host.birth.start_sec,
                    host.birth.start_usec,
                    boot.boot_sec,
                    boot.boot_usec
                ],
            )
            .unwrap();
    }

    /// Wait, with a bound, until the run's row reads `lifecycle` and `pid`, if any, is not
    /// running.
    async fn until_ended(rig: &Rig, uid: &str, lifecycle: &str, pid: Option<i32>) {
        for _ in 0..150 {
            if kept(rig, uid).0 == lifecycle && !pid.is_some_and(running) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The reason the run's one `SessionEnd` gives, if it has one.
    fn end_reason(rig: &Rig, uid: &str) -> Option<serde_json::Value> {
        let ends: Vec<serde_json::Value> = stored(rig, uid)
            .into_iter()
            .filter(|event| event["kind"] == "session_end")
            .collect();
        assert!(ends.len() <= 1, "{ends:?}");
        ends.first().map(|end| end["payload"]["reason"].clone())
    }

    /// **G6: a detached host's death (H7).** The run's link has ended with its host alive;
    /// then the host is killed while ccd runs. Once its pid is free, one pass of the
    /// hosted-run watch, which the liveness tick runs beside the tmux sweep, finds it gone:
    /// its group is signalled and the run ends.
    #[tokio::test]
    async fn a_detached_hosts_death_ends_its_run_on_the_next_watch() {
        let rig = rig();
        let host = stub_host_with_child(&rig);
        let (dir, uid) = detached_run(&rig, &host).await;
        let (stub, child) = (pid_in(&dir, "stub.pid"), pid_in(&dir, "child.pid"));
        assert_eq!(kept(&rig, &uid).0, "live");
        // SAFETY: kill(2) of the stand-in host this test started, by its recorded pid.
        unsafe { libc::kill(stub, libc::SIGKILL) };
        for _ in 0..100 {
            // SAFETY: kill(2) with signal 0 only asks whether the pid is in use.
            if unsafe { libc::kill(stub, 0) } != 0 {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        rig.daemon.watch_hosted_runs().await;
        until_ended(&rig, &uid, "exited", Some(child)).await;
        let child_ran = running(child);
        if child_ran {
            // SAFETY: kill(2) of the child this test's stand-in started, by its pid.
            unsafe { libc::kill(child, libc::SIGKILL) };
        }
        assert!(!child_ran, "the host's group outlived it");
        assert_eq!(kept(&rig, &uid).0, "exited");
        assert_eq!(
            end_reason(&rig, &uid),
            Some(serde_json::json!("Stopped: its process ended"))
        );
    }

    /// **G1d (X7a): a linked Claude run is skipped by the hosted-run watch.** Its link is the
    /// host's own connection, so its end is the link's to act on, not the sweep's: with the
    /// link attached and the host gone, a pass of the watch leaves the run `live`.
    #[tokio::test]
    async fn a_linked_claude_run_is_skipped_by_the_watch() {
        let rig = rig();
        let host = stub_host_with_child(&rig);
        let (dir, uid) = detached_run(&rig, &host).await;
        let (stub, child) = (pid_in(&dir, "stub.pid"), pid_in(&dir, "child.pid"));
        let (to_link, _commands) = tokio::sync::mpsc::unbounded_channel();
        rig.daemon
            .host_link_attached(&uid, crate::state::HostLink::Claude(to_link))
            .await;
        // SAFETY: kill(2) of the stand-in host this test started, by its recorded pid.
        unsafe { libc::kill(stub, libc::SIGKILL) };
        for _ in 0..100 {
            // SAFETY: kill(2) with signal 0 only asks whether the pid is in use.
            if unsafe { libc::kill(stub, 0) } != 0 {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        rig.daemon.watch_hosted_runs().await;
        // The watch ends a run by signalling its group and waiting it out, so a run it acted
        // on leaves `live` within the ladder's 5 s; this one stays, its host's child running.
        let mut lifecycle = kept(&rig, &uid).0;
        for _ in 0..120 {
            if lifecycle != "live" {
                break;
            }
            tokio::time::sleep(POLL).await;
            lifecycle = kept(&rig, &uid).0;
        }
        let child_ran = running(child);
        if child_ran {
            // SAFETY: kill(2) of the child this test's stand-in started, by its pid.
            unsafe { libc::kill(child, libc::SIGKILL) };
        }
        assert_eq!(lifecycle, "live", "the watch acted on a run its link holds");
        assert!(
            child_ran,
            "the watch signalled the group of a run its link holds"
        );
    }

    /// The one `SessionEnd` the run has, whole.
    fn the_end(rig: &Rig, uid: &str) -> serde_json::Value {
        let ends: Vec<serde_json::Value> = stored(rig, uid)
            .into_iter()
            .filter(|event| event["kind"] == "session_end")
            .collect();
        assert_eq!(ends.len(), 1, "{ends:?}");
        ends[0]["payload"].clone()
    }

    /// Put the run in `lifecycle` as only the store knows it: no stop sent, no escalation.
    fn force_lifecycle(rig: &Rig, uid: &str, lifecycle: &str) {
        rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .execute(
                "UPDATE sessions SET lifecycle = ?2 WHERE session_uid = ?1",
                rusqlite::params![uid, lifecycle],
            )
            .unwrap();
    }

    /// **B7 G1: End run.** A live run asked to end is `stopping` and its host is sent
    /// `stop`; claude's end, mid-turn code 1, ends it "Stopped", with no exit code for an app
    /// to read as a failure. Asked again, it has already ended, and nothing more is sent.
    #[tokio::test]
    async fn a_run_asked_to_end_is_sent_stop_and_ends_stopped() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        assert_eq!(
            rig.daemon.stop_run(&played.uid).await,
            StopRunResult::Stopping
        );
        assert_eq!(kept(&rig, &played.uid).0, "stopping");
        assert!(matches!(
            next(&mut played.lines).await,
            Some(ToHost::Stop { .. })
        ));
        say(
            &mut played.write,
            serde_json::json!({"type": "exited", "seq": 3, "code": 1}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 3 }));
        assert_eq!(kept(&rig, &played.uid).0, "exited");
        assert_eq!(
            the_end(&rig, &played.uid),
            serde_json::json!({"exit_code": null, "reason": "Stopped"})
        );
        assert_eq!(
            rig.daemon.stop_run(&played.uid).await,
            StopRunResult::AlreadyEnded
        );
        assert_eq!(next(&mut played.lines).await, None, "nothing more is sent");
        stop(&played.dir);
    }

    /// **B7 G4, the other answers.** Two asks at once send one `stop`, and a third reads
    /// `stopping`; a run still starting and a session there is not are refused, and the
    /// starting run stays as it was.
    #[tokio::test]
    async fn a_run_is_sent_one_stop_however_often_it_is_asked() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let (first, second) = tokio::join!(
            rig.daemon.stop_run(&played.uid),
            rig.daemon.stop_run(&played.uid)
        );
        assert_eq!(
            (first, second),
            (StopRunResult::Stopping, StopRunResult::Stopping)
        );
        assert_eq!(
            rig.daemon.stop_run(&played.uid).await,
            StopRunResult::Stopping
        );
        assert!(matches!(
            next(&mut played.lines).await,
            Some(ToHost::Stop { .. })
        ));
        assert_eq!(next(&mut played.lines).await, None, "one stop only");
        stop(&played.dir);

        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (starting, _) = begun(&rig, &cwd, &host, Trust::NotTrusted).await;
        assert!(matches!(
            rig.daemon.stop_run(&starting).await,
            StopRunResult::Refused { .. }
        ));
        assert_eq!(kept(&rig, &starting).0, "starting");
        assert!(matches!(
            rig.daemon.stop_run("no-such-session").await,
            StopRunResult::Refused { .. }
        ));
    }

    /// **B7 G5: the watch alone ends a stopping run whose host is gone.** The run is
    /// `stopping` in the store only, so no escalation runs; its host dies, and one pass of
    /// the watch ends it "Stopped".
    #[tokio::test]
    async fn a_stopping_runs_dead_host_is_ended_by_the_watch() {
        let rig = rig();
        let host = stub_host_with_child(&rig);
        let (dir, uid) = detached_run(&rig, &host).await;
        let (stub, child) = (pid_in(&dir, "stub.pid"), pid_in(&dir, "child.pid"));
        force_lifecycle(&rig, &uid, "stopping");
        // SAFETY: kill(2) of the stand-in host this test started, by its recorded pid.
        unsafe { libc::kill(stub, libc::SIGKILL) };
        for _ in 0..100 {
            // SAFETY: kill(2) with signal 0 only asks whether the pid is in use.
            if unsafe { libc::kill(stub, 0) } != 0 {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        rig.daemon.watch_hosted_runs().await;
        until_ended(&rig, &uid, "exited", Some(child)).await;
        let child_ran = running(child);
        if child_ran {
            // SAFETY: kill(2) of the child this test's stand-in started, by its pid.
            unsafe { libc::kill(child, libc::SIGKILL) };
        }
        assert!(!child_ran, "the host's group outlived it");
        assert_eq!(kept(&rig, &uid).0, "exited");
        assert_eq!(
            the_end(&rig, &uid),
            serde_json::json!({"exit_code": null, "reason": "Stopped"})
        );
    }

    /// **B7 G5b: claude's own end racing the stop.** The run turns `stopping` before its
    /// link has the stop, and claude's `exited` comes: the link's end from `live` changes
    /// nothing and is acknowledged; the host gone, the watch ends the run once, "Stopped".
    #[tokio::test]
    async fn claudes_own_end_racing_the_stop_ends_the_run_once() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        force_lifecycle(&rig, &played.uid, "stopping");
        say(
            &mut played.write,
            serde_json::json!({"type": "exited", "seq": 3, "code": 0}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 3 }));
        assert_eq!(kept(&rig, &played.uid).0, "stopping");
        let (uid, dir) = (played.uid.clone(), played.dir.clone());
        let host = pid_in(&dir, "stub.pid");
        drop(played);
        stop(&dir);
        for _ in 0..100 {
            // SAFETY: kill(2) with signal 0 only asks whether the pid is in use.
            if unsafe { libc::kill(host, 0) } != 0 {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        rig.daemon.watch_hosted_runs().await;
        until_ended(&rig, &uid, "exited", None).await;
        assert_eq!(
            the_end(&rig, &uid),
            serde_json::json!({"exit_code": null, "reason": "Stopped"})
        );
    }

    /// **B7 G6: a restart while stopping.** ccd stopped a working run and died before
    /// filing the host's last messages. The restarted ccd files each replayed message once,
    /// at its cursor, sends `stop` again after the first `state`, and claude's end ends the
    /// run "Stopped".
    #[tokio::test]
    async fn a_run_stopping_across_a_restart_is_sent_stop_again_and_ends() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        assert_eq!(
            rig.daemon.stop_run(&played.uid).await,
            StopRunResult::Stopping
        );
        assert!(matches!(
            next(&mut played.lines).await,
            Some(ToHost::Stop { .. })
        ));
        let (uid, dir) = (played.uid.clone(), played.dir.clone());
        let before = stored(&rig, &uid).len();
        drop(played);
        let (mut lines, mut write) = restart(&rig, &dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 2 }));
        let capture = capture();
        for (index, message) in capture.iter().enumerate().take(5) {
            let seq = index as u64 + 3;
            sdk(&mut write, seq, message.clone()).await;
            assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq }));
        }
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 8, "pending": []}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 8 }));
        assert!(matches!(next(&mut lines).await, Some(ToHost::Stop { .. })));
        say(
            &mut write,
            serde_json::json!({"type": "exited", "seq": 9, "code": 1}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 9 }));
        stop(&dir);
        assert_eq!(kept(&rig, &uid).0, "exited");
        assert_eq!(cursor(&rig, &uid), 9);
        let filed: Vec<serde_json::Value> = stored(&rig, &uid)
            .split_off(before)
            .into_iter()
            .filter(|event| event["kind"] != "session_end" && event["kind"] != "link_state")
            .collect();
        let key = SessionKey::new(&uid, &uid);
        let expected: usize = capture
            .iter()
            .take(5)
            .map(|message| protocol::sdk_map::map(&key, None, message).events.len())
            .sum();
        assert_eq!(filed.len(), expected, "each replayed message filed once");
        assert_eq!(
            the_end(&rig, &uid),
            serde_json::json!({"exit_code": null, "reason": "Stopped"})
        );
    }

    /// **B7 G6, a host that cannot answer across a restart.** A run is `stopping` and its
    /// host SIGSTOPped when ccd restarts, with no escalation of the old ccd's running. The
    /// recovered ccd's own escalation stops the host's group 10 s later, and the run ends
    /// "Stopped".
    #[tokio::test]
    async fn a_stopping_runs_frozen_host_is_stopped_by_the_recovered_daemon() {
        let rig = rig();
        let host = stub_host_with_child(&rig);
        let played = live_run_on(&rig, &host).await;
        let (uid, dir) = (played.uid.clone(), played.dir.clone());
        let (stub, child) = (pid_in(&dir, "stub.pid"), pid_in(&dir, "child.pid"));
        force_lifecycle(&rig, &uid, "stopping");
        drop(played);
        // SAFETY: kill(2) of the stand-in host this test started, by its recorded pid.
        unsafe { libc::kill(stub, libc::SIGSTOP) };
        let (_lines, _write) = restart(&rig, &dir).await;
        let mut ended = false;
        for _ in 0..300 {
            if kept(&rig, &uid).0 == "exited" {
                ended = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let child_ran = running(child);
        if !ended || child_ran {
            // SAFETY: kill(2) of the group of the stand-in host this test started.
            unsafe { libc::kill(-stub, libc::SIGKILL) };
        }
        assert!(ended, "the recovered daemon never ended the run");
        assert!(!child_ran, "the host's group outlived it");
        assert_eq!(
            the_end(&rig, &uid),
            serde_json::json!({"exit_code": null, "reason": "Stopped"})
        );
    }

    /// The app's `send_text` of `text` to the run `uid` as request `request_id`, as the
    /// Kit sends it.
    async fn send_as_app(
        daemon: &Daemon,
        uid: &str,
        request_id: &str,
        text: &str,
    ) -> SendTextResult {
        let hash = protocol::hash::send_text_hash(uid, text, true);
        daemon
            .send_text(
                uid,
                text.to_string(),
                Some(request_id),
                Some(&hash),
                true,
                false,
            )
            .await
    }

    /// The person's words the run has filed, in order.
    fn words(rig: &Rig, uid: &str) -> Vec<String> {
        stored(rig, uid)
            .into_iter()
            .filter(|event| event["kind"] == "user_message" && event["source"] == "daemon")
            .map(|event| event["payload"]["text"].as_str().unwrap().to_string())
            .collect()
    }

    /// The host's side of one `say`: it reads it, then answers `status` as message `seq`
    /// and reads ccd's ack. The say's uuid and text.
    async fn host_takes(played: &mut Played, seq: u64, status: &str) -> (String, String) {
        let Some(ToHost::Say { uuid, text }) = next(&mut played.lines).await else {
            panic!("no say reached the host");
        };
        say(
            &mut played.write,
            serde_json::json!({"type": "outcome", "seq": seq, "uuid": uuid, "status": status}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
        (uuid, text)
    }

    /// **B5 G1 and G9: a second message, and the first.** The start's prompt is filed as
    /// the person's words when the run goes live. A message to the live run goes to its
    /// host as `say`; once the host has taken it, the words are filed with the outcome's
    /// cursor, reach a subscribed connection, and the app is told `sent`.
    #[tokio::test]
    async fn a_message_to_a_live_run_reaches_its_host_and_the_persons_words_are_filed() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        assert_eq!(words(&rig, &played.uid), ["fix the tests"]);
        let uid = played.uid.clone();
        let mut broadcast = rig.daemon.events_tx.subscribe();
        let (result, (_, text)) = tokio::join!(
            send_as_app(&rig.daemon, &uid, "r-1", "and the docs"),
            host_takes(&mut played, 3, "done")
        );
        assert_eq!(text, "and the docs");
        assert_eq!(
            result,
            SendTextResult::Sent {
                matched: "and the docs".into()
            }
        );
        assert_eq!(words(&rig, &uid), ["fix the tests", "and the docs"]);
        assert_eq!(cursor(&rig, &uid), 3);
        let mut live = Vec::new();
        while let Ok(event) = broadcast.try_recv() {
            if event.session_uid == uid && event.kind == EventKind::UserMessage {
                live.push(event.payload["text"].clone());
            }
        }
        assert_eq!(live, [serde_json::json!("and the docs")], "broadcast live");
        stop(&played.dir);
    }

    /// **B5 G9: a refused start files no words.**
    #[tokio::test]
    async fn a_refused_start_files_no_words() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (_, played) =
            tokio::join!(start(&rig.daemon, run(&rig, "req-1", &cwd, &host)), async {
                let mut played = play_to_start(&rig).await;
                let uuid = played.start_uuid();
                say(
                    &mut played.write,
                    serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                   "status": "refused", "detail": "no"}),
                )
                .await;
                assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 2 }));
                played
            });
        assert!(words(&rig, &played.uid).is_empty());
        stop(&played.dir);
    }

    /// What an agent's message to the run `uid`, from another run on this computer (`studio`,
    /// as the recorded tailnet names it), is told.
    async fn mail_from_another_run(daemon: &Arc<Daemon>, uid: &str) -> protocol::ipc::MailState {
        let tailnet = protocol::tailnet::Tailnet::parse(include_str!(
            "../../../fixtures/tailscale/status-six-nodes.json"
        ))
        .unwrap();
        let sender = protocol::uid::new().unwrap();
        let row = crate::store::SessionRow {
            session_uid: sender.clone(),
            session_id: sender.clone(),
            claude_session_id: None,
            codex_thread_id: None,
            ..daemon
                .db
                .get_session(uid.to_string())
                .await
                .unwrap()
                .unwrap()
        };
        daemon.store.upsert_session(&row).unwrap().assert_present();
        daemon
            .send_agent_message_on(
                Some(&tailnet),
                &sender,
                &format!("{uid}@studio"),
                "hi",
                None,
            )
            .await
            .state
    }

    /// **The mail item's D1 and D2 on Claude: a hosted run takes mail from the moment its start
    /// is answered `done`.** It is kept with this build's minor, as a run of this build's
    /// supervisor is, so it is listed and mail from another run on this computer waits for it.
    /// A start the host refuses is never kept, and mail for it is refused.
    #[tokio::test]
    async fn a_hosted_claude_run_takes_mail_once_live_and_a_refused_start_never_does() {
        for (status, kept, told) in [
            (
                "done",
                Some(protocol::PROTOCOL_MINOR),
                protocol::ipc::MailState::Waiting,
            ),
            ("refused", None, protocol::ipc::MailState::Refused),
        ] {
            let rig = rig();
            let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
            let cwd = rig.root.to_str().unwrap().to_string();
            let (_, played) = tokio::join!(
                start(&rig.daemon, run(&rig, "req-mail", &cwd, &host)),
                async {
                    let mut played = play_to_start(&rig).await;
                    let uuid = played.start_uuid();
                    say(
                        &mut played.write,
                        serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                           "status": status,
                                           "initialize": (status == "done").then(initialize_answer)}),
                    )
                    .await;
                    assert_eq!(
                        next(&mut played.lines).await,
                        Some(ToHost::Ack { seq: 2 }),
                        "{status}"
                    );
                    played
                }
            );
            let minors: Vec<(String, u32)> = rig
                .daemon
                .db
                .mail_runs()
                .await
                .unwrap()
                .into_iter()
                .map(|(row, minor)| (row.session_uid, minor))
                .collect();
            assert_eq!(
                minors,
                kept.map(|minor| (played.uid.clone(), minor))
                    .into_iter()
                    .collect::<Vec<_>>(),
                "{status}"
            );
            assert_eq!(
                mail_from_another_run(&rig.daemon, &played.uid).await,
                told,
                "{status}"
            );
            stop(&played.dir);
        }
    }

    /// The stand-in host ends the running turn with claude's `result`, as message `seq`.
    async fn turn_ends(played: &mut Played, seq: u64) {
        sdk(
            &mut played.write,
            seq,
            serde_json::json!({"type": "result", "subtype": "success", "is_error": false,
                "num_turns": 1, "uuid": format!("9d1f0000-0000-4000-8000-0000000003{seq:02}"),
                "session_id": "5f0c1e2a-0000-4000-8000-000000000001"}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
    }

    /// The text of the mail wake ccd sends the host next.
    async fn woken(lines: &mut Lines<BufReader<OwnedReadHalf>>) -> String {
        match next(lines).await {
            Some(ToHost::Wake { text, .. }) => text,
            other => panic!("no mail wake reached the host: {other:?}"),
        }
    }

    /// The line ccd sends the host within a second, if any.
    async fn sent_soon(played: &mut Played) -> Option<String> {
        tokio::time::timeout(Duration::from_secs(1), played.lines.next_line())
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }

    /// How many `mail_nudge` notices the run has filed.
    fn nudges(rig: &Rig, uid: &str) -> usize {
        stored(rig, uid)
            .iter()
            .filter(|event| {
                event["kind"] == "notification"
                    && event["payload"]["notification_type"] == protocol::event::notice::MAIL_NUDGE
            })
            .count()
    }

    /// **B11 T1: mail that reaches a hosted run while its turn runs wakes it once that turn
    /// has ended, never during it.** The turn starts at claude's init and ends at its
    /// `result`, as a terminal run's transcript marks them.
    #[tokio::test]
    async fn mail_to_a_hosted_run_mid_turn_wakes_it_once_the_turn_ends() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        turn_begins(&mut played, 3, "9d1f0000-0000-4000-8000-000000000311").await;
        assert_eq!(
            mail_from_another_run(&rig.daemon, &uid).await,
            protocol::ipc::MailState::Waiting
        );
        assert_eq!(
            sent_soon(&mut played).await,
            None,
            "a wake while the turn runs"
        );
        turn_ends(&mut played, 4).await;
        assert_eq!(woken(&mut played.lines).await, crate::mail::nudge());
        stop(&played.dir);
    }

    /// **B11 T2: after a person's turn, mail wakes the idle hosted run once, until the woken
    /// turn ends.** The wake is the fixed nudge, with ccd's own `mail_nudge` notice beside
    /// it; a second message while the woken turn runs sends nothing more; that turn's end,
    /// its mail still unread, lets the rule wake it again.
    #[tokio::test]
    async fn mail_wakes_an_idle_hosted_run_once_until_its_woken_turn_ends() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        turn_begins(&mut played, 3, "9d1f0000-0000-4000-8000-000000000321").await;
        turn_ends(&mut played, 4).await;
        tokio::join!(
            send_as_app(&rig.daemon, &uid, "r-1", "and the docs"),
            host_takes(&mut played, 5, "done")
        );
        turn_begins(&mut played, 6, "9d1f0000-0000-4000-8000-000000000322").await;
        turn_ends(&mut played, 7).await;
        assert_eq!(
            mail_from_another_run(&rig.daemon, &uid).await,
            protocol::ipc::MailState::Waiting
        );
        assert_eq!(woken(&mut played.lines).await, crate::mail::nudge());
        assert_eq!(nudges(&rig, &uid), 1);
        turn_begins(&mut played, 8, "9d1f0000-0000-4000-8000-000000000323").await;
        assert_eq!(
            mail_from_another_run(&rig.daemon, &uid).await,
            protocol::ipc::MailState::Waiting
        );
        assert_eq!(
            sent_soon(&mut played).await,
            None,
            "a second wake before the woken turn ended"
        );
        turn_ends(&mut played, 9).await;
        assert_eq!(woken(&mut played.lines).await, crate::mail::nudge());
        assert_eq!(nudges(&rig, &uid), 2);
        stop(&played.dir);
    }

    /// **B11 T3: a person's message to an idle hosted run while unread mail waits is followed
    /// by the nudge**, which claude reads in that turn, as a terminal run's prompt is told of
    /// its mail. With no mail waiting, nothing follows. Mail waits unwoken once three woken
    /// turns have left it unread.
    #[tokio::test]
    async fn a_message_to_an_idle_hosted_run_with_mail_waiting_is_followed_by_the_nudge() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        turn_begins(&mut played, 3, "9d1f0000-0000-4000-8000-000000000331").await;
        turn_ends(&mut played, 4).await;
        tokio::join!(
            send_as_app(&rig.daemon, &uid, "r-1", "and the docs"),
            host_takes(&mut played, 5, "done")
        );
        assert_eq!(sent_soon(&mut played).await, None, "a nudge with no mail");
        turn_begins(&mut played, 6, "9d1f0000-0000-4000-8000-000000000332").await;
        turn_ends(&mut played, 7).await;
        assert_eq!(
            mail_from_another_run(&rig.daemon, &uid).await,
            protocol::ipc::MailState::Waiting
        );
        for (n, seq) in [8, 10, 12].into_iter().enumerate() {
            assert_eq!(
                woken(&mut played.lines).await,
                crate::mail::nudge(),
                "wake {n}"
            );
            turn_begins(
                &mut played,
                seq,
                &format!("9d1f0000-0000-4000-8000-0000000003{seq:02}"),
            )
            .await;
            turn_ends(&mut played, seq + 1).await;
        }
        assert_eq!(sent_soon(&mut played).await, None, "a fourth wake");
        tokio::join!(
            send_as_app(&rig.daemon, &uid, "r-2", "and the tests"),
            host_takes(&mut played, 14, "done")
        );
        assert_eq!(woken(&mut played.lines).await, crate::mail::nudge());
        stop(&played.dir);
    }

    /// **B11 T4: a hosted run whose link is gone is not woken, and is woken once its host is
    /// attached again.** The wake that found no link is released; the restarted daemon's
    /// attach decides again at the end of the host's replay.
    #[tokio::test]
    async fn a_hosted_run_is_woken_for_mail_that_came_while_its_link_was_gone_once_attached_again()
    {
        let rig = rig();
        let played = live_run(&rig).await;
        let (uid, dir) = (played.uid.clone(), played.dir.clone());
        drop(played);
        for _ in 0..100 {
            if link_facts(&rig, &uid).len() == 2 {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        assert_eq!(
            mail_from_another_run(&rig.daemon, &uid).await,
            protocol::ipc::MailState::Waiting
        );
        let outstanding = || {
            rusqlite::Connection::open(rig.root.join("events.db"))
                .unwrap()
                .query_row(
                    "SELECT outstanding FROM mail_wake WHERE session_uid = ?1",
                    [&uid],
                    |row| row.get::<_, bool>(0),
                )
                .ok()
        };
        // The claim makes the row; its release, once no link took the wake, clears it.
        for _ in 0..100 {
            if outstanding() == Some(false) {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        assert_eq!(outstanding(), Some(false), "the wake that found no link");
        let (mut lines, mut write) = restart(&rig, &dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 2 }));
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 3, "pending": []}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 3 }));
        assert_eq!(woken(&mut lines).await, crate::mail::nudge());
        stop(&dir);
    }

    /// **B5 G2: retries.** The same request asked again while its first send waits for
    /// the host is not sent twice; once the host has taken it, it is answered `duplicate`,
    /// with nothing sent and no second row; the same id with other words is refused;
    /// another id with the same words is a second message.
    #[tokio::test]
    async fn a_retried_message_is_answered_from_its_first_send() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let (first, ()) = tokio::join!(send_as_app(&rig.daemon, &uid, "r-1", "again"), async {
            let Some(ToHost::Say { uuid, .. }) = next(&mut played.lines).await else {
                panic!("no say reached the host");
            };
            assert!(
                matches!(
                    send_as_app(&rig.daemon, &uid, "r-1", "again").await,
                    SendTextResult::Indeterminate { .. }
                ),
                "asked again at once"
            );
            say(
                &mut played.write,
                serde_json::json!({"type": "outcome", "seq": 3, "uuid": uuid, "status": "done"}),
            )
            .await;
            assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 3 }));
        });
        assert!(matches!(first, SendTextResult::Sent { .. }));
        assert!(matches!(
            send_as_app(&rig.daemon, &uid, "r-1", "again").await,
            SendTextResult::Duplicate { .. }
        ));
        assert!(matches!(
            send_as_app(&rig.daemon, &uid, "r-1", "other words").await,
            SendTextResult::Refused { .. }
        ));
        assert_eq!(next(&mut played.lines).await, None, "nothing sent again");
        let (second, _) = tokio::join!(
            send_as_app(&rig.daemon, &uid, "r-2", "again"),
            host_takes(&mut played, 4, "done")
        );
        assert!(matches!(second, SendTextResult::Sent { .. }));
        assert_eq!(words(&rig, &uid), ["fix the tests", "again", "again"]);
        stop(&played.dir);
    }

    /// **B5 G3, B8 G4: what a run started in a host does not take.** The Model and Effort
    /// sheets' confirmations, the Clear dialog's `/clear` and `/status` are refused, and
    /// nothing is sent, claimed or filed; a message that only mentions a command, `/usage`
    /// and `/cost` are messages.
    #[tokio::test]
    async fn what_a_hosted_run_does_not_take_is_refused_and_nothing_is_sent() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let refused = |result: SendTextResult| match result {
            SendTextResult::Refused { reason } => reason,
            other => panic!("not refused: {other:?}"),
        };
        let model_hash = protocol::hash::send_text_hash(&uid, "/model opus", true);
        assert_eq!(
            refused(
                rig.daemon
                    .send_text(
                        &uid,
                        "/model opus".into(),
                        Some("m"),
                        Some(&model_hash),
                        true,
                        true
                    )
                    .await
            ),
            "Changing the model isn't available for this session yet."
        );
        let effort_hash = protocol::hash::send_text_hash(&uid, "/effort high", true);
        assert_eq!(
            refused(
                rig.daemon
                    .send_text(
                        &uid,
                        "/effort high".into(),
                        Some("f"),
                        Some(&effort_hash),
                        true,
                        true
                    )
                    .await
            ),
            "Changing the effort isn't available for this session yet."
        );
        assert_eq!(
            refused(send_as_app(&rig.daemon, &uid, "c", "/clear").await),
            "This isn't available for this session yet."
        );
        assert_eq!(
            refused(send_as_app(&rig.daemon, &uid, "st", "/status").await),
            "Claude doesn't offer /status in this session."
        );
        let staged = protocol::hash::send_text_hash(&uid, "half", false);
        refused(
            rig.daemon
                .send_text(&uid, "half".into(), Some("s"), Some(&staged), false, false)
                .await,
        );
        refused(send_as_app(&rig.daemon, &uid, "e", "  ").await);
        refused(
            rig.daemon
                .send_text(&uid, "no id".into(), None, None, true, false)
                .await,
        );
        assert_eq!(next(&mut played.lines).await, None, "nothing sent");
        let (result, (_, text)) = tokio::join!(
            send_as_app(&rig.daemon, &uid, "r-1", "why did /clear not run?"),
            host_takes(&mut played, 3, "done")
        );
        assert!(matches!(result, SendTextResult::Sent { .. }));
        assert_eq!(text, "why did /clear not run?");
        for (seq, command) in [(4, "/usage"), (5, "/cost")] {
            let (result, (_, text)) = tokio::join!(
                send_as_app(&rig.daemon, &uid, command, command),
                host_takes(&mut played, seq, "done")
            );
            assert!(matches!(result, SendTextResult::Sent { .. }), "{result:?}");
            assert_eq!(text, command);
        }
        for (lifecycle, request_id) in [("stopping", "r-2"), ("exited", "r-3")] {
            force_lifecycle(&rig, &uid, lifecycle);
            assert_eq!(
                refused(send_as_app(&rig.daemon, &uid, request_id, "late").await),
                "This session isn't running, so nothing was sent."
            );
            assert!(
                matches!(
                    send_as_app(&rig.daemon, &uid, "r-1", "why did /clear not run?").await,
                    SendTextResult::Duplicate { .. }
                ),
                "a retry once {lifecycle} is answered from its send"
            );
        }
        assert_eq!(next(&mut played.lines).await, None, "nothing sent");
        assert_eq!(
            words(&rig, &uid),
            [
                "fix the tests",
                "why did /clear not run?",
                "/usage",
                "/cost"
            ]
        );
        assert!(
            rig.daemon
                .db
                .host_sends(uid.clone())
                .await
                .unwrap()
                .is_empty(),
            "nothing left claimed"
        );
        stop(&played.dir);
    }

    /// **B5 G3: a run whose link is down** refuses a message, and keeps no claim of it.
    #[tokio::test]
    async fn a_message_to_a_run_whose_link_is_down_is_refused() {
        let rig = rig();
        let (dir, uid) = detached_run(&rig, &stub_host(&rig, HOST_PROTOCOL_VERSION, None)).await;
        assert!(matches!(
            send_as_app(&rig.daemon, &uid, "r-1", "hello").await,
            SendTextResult::Refused { .. }
        ));
        assert!(rig
            .daemon
            .db
            .host_sends(uid.clone())
            .await
            .unwrap()
            .is_empty());
        stop(&dir);
    }

    /// **B5 G4: the link ends before the host's outcome:** the app is told the message is
    /// not confirmed, and no words are filed. A retry is answered the same, from its send,
    /// though the link is down.
    #[tokio::test]
    async fn a_message_whose_link_ends_before_its_outcome_is_not_confirmed() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let dir = played.dir.clone();
        let (result, ()) = tokio::join!(send_as_app(&rig.daemon, &uid, "r-1", "lost"), async {
            assert!(matches!(
                next(&mut played.lines).await,
                Some(ToHost::Say { .. })
            ));
            drop(played);
        });
        assert_eq!(
            result,
            SendTextResult::Indeterminate {
                reason: "The computer lost the session before the message was confirmed; it \
                         may or may not have reached Claude."
                    .into()
            }
        );
        assert!(matches!(
            send_as_app(&rig.daemon, &uid, "r-1", "lost").await,
            SendTextResult::Indeterminate { .. }
        ));
        assert_eq!(words(&rig, &uid), ["fix the tests"]);
        stop(&dir);
    }

    /// **B5 G3 and G7: a message that crosses End run.** The host no longer takes it once
    /// its run is stopping, and refuses it: the app is told nothing was sent, no words are
    /// filed, the claim is let go, and the run stays `stopping`.
    #[tokio::test]
    async fn a_message_crossing_a_stop_is_refused_and_files_nothing() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let (result, ()) = tokio::join!(send_as_app(&rig.daemon, &uid, "r-1", "late"), async {
            let Some(ToHost::Say { uuid, .. }) = next(&mut played.lines).await else {
                panic!("no say reached the host");
            };
            assert_eq!(rig.daemon.stop_run(&uid).await, StopRunResult::Stopping);
            assert!(matches!(
                next(&mut played.lines).await,
                Some(ToHost::Stop { .. })
            ));
            say(
                &mut played.write,
                serde_json::json!({"type": "outcome", "seq": 3, "uuid": uuid,
                                   "status": "refused"}),
            )
            .await;
            assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 3 }));
        });
        assert_eq!(
            result,
            SendTextResult::Refused {
                reason: "This session stopped taking messages; nothing was sent.".into()
            }
        );
        assert_eq!(words(&rig, &uid), ["fix the tests"]);
        assert!(rig
            .daemon
            .db
            .host_sends(uid.clone())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(kept(&rig, &uid).0, "stopping");
        assert_eq!(cursor(&rig, &uid), 3);
        stop(&played.dir);
    }

    /// **B5 G4: an outcome later than the wait.** The app is told the message is not
    /// confirmed; the host's outcome, when it comes, still files the words once, and the
    /// retry is then answered `duplicate`.
    #[tokio::test]
    async fn a_late_outcome_still_files_the_words_once() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let (result, uuid) = tokio::join!(send_as_app(&rig.daemon, &uid, "r-1", "slow"), async {
            let Some(ToHost::Say { uuid, .. }) = next(&mut played.lines).await else {
                panic!("no say");
            };
            uuid
        });
        assert!(matches!(result, SendTextResult::Indeterminate { .. }));
        say(
            &mut played.write,
            serde_json::json!({"type": "outcome", "seq": 3, "uuid": uuid, "status": "done"}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 3 }));
        assert_eq!(words(&rig, &uid), ["fix the tests", "slow"]);
        assert!(matches!(
            send_as_app(&rig.daemon, &uid, "r-1", "slow").await,
            SendTextResult::Duplicate { .. }
        ));
        stop(&played.dir);
    }

    /// **B5 G5: a restart.** ccd died after the host took a message and before its outcome
    /// was committed: the restarted ccd, holding nothing of it in memory, files the words
    /// from the replayed outcome, and the retry is answered `duplicate`. A message sent
    /// while the host replays goes to it as any other. A message claimed and never sent is
    /// not confirmed, and is not sent again.
    #[tokio::test]
    async fn a_message_taken_before_a_restart_is_filed_from_the_replay() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let dir = played.dir.clone();
        let (lost, uuid) = tokio::join!(send_as_app(&rig.daemon, &uid, "r-1", "kept"), async {
            let Some(ToHost::Say { uuid, .. }) = next(&mut played.lines).await else {
                panic!("no say");
            };
            drop(played);
            uuid
        });
        assert!(matches!(lost, SendTextResult::Indeterminate { .. }));
        let (restarted, mut lines, mut write) = restarted(&rig, &dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 2 }));
        let (meanwhile, ()) =
            tokio::join!(send_as_app(&restarted, &uid, "r-3", "meanwhile"), async {
                let Some(ToHost::Say { uuid: new, .. }) = next(&mut lines).await else {
                    panic!("no say during the replay");
                };
                for (seq, uuid) in [(3, &uuid), (4, &new)] {
                    say(
                        &mut write,
                        serde_json::json!({"type": "outcome", "seq": seq, "uuid": uuid,
                                           "status": "done"}),
                    )
                    .await;
                    assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq }));
                }
            });
        assert_eq!(
            meanwhile,
            SendTextResult::Sent {
                matched: "meanwhile".into()
            }
        );
        assert!(matches!(
            send_as_app(&restarted, &uid, "r-1", "kept").await,
            SendTextResult::Duplicate { .. }
        ));
        assert_eq!(words(&rig, &uid), ["fix the tests", "kept", "meanwhile"]);
        let hash = protocol::hash::send_text_hash(&uid, "never sent", true);
        rig.daemon
            .db
            .claim_text_mutation(
                uid.clone(),
                "r-2".into(),
                hash,
                Some("never sent".into()),
                protocol::time::now_rfc3339(),
            )
            .await
            .unwrap();
        assert!(matches!(
            send_as_app(&restarted, &uid, "r-2", "never sent").await,
            SendTextResult::Indeterminate { .. }
        ));
        stop(&dir);
    }

    /// claude's permission request for one Bash call, as its host relays it.
    fn bash_request(tool_use_id: &str, options: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"tool_use_id": tool_use_id, "tool_name": "Bash",
                           "input": {"command": "ls"}, "options": options})
    }

    /// The hash a card for that call carries, a hook card's and a host card's alike.
    fn bash_hash() -> String {
        protocol::hash::approval_payload_hash("Bash", &serde_json::json!({"command": "ls"}))
    }

    /// The stand-in host relays the request `tool_use_id` as message `seq` and reads the ack.
    async fn host_asks(
        played: &mut Played,
        seq: u64,
        tool_use_id: &str,
        options: serde_json::Value,
    ) {
        say(
            &mut played.write,
            serde_json::json!({"type": "permission", "seq": seq,
                               "request": bash_request(tool_use_id, options)}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
    }

    /// The app's answer to the card `request_id` of the run `uid`, from an iPhone.
    async fn app_answers(
        daemon: &Daemon,
        uid: &str,
        request_id: &str,
        decision: AnswerDecision,
    ) -> AnswerResult {
        daemon
            .answer(
                request_id,
                &bash_hash(),
                decision,
                Some(uid),
                AnsweredOn::Iphone,
            )
            .await
    }

    /// The host's side of one answer, on `lines` and `write`: it reads the answer, answers
    /// `status` as message `seq` and reads the ack. The answer's uuid and decision.
    async fn host_settles(
        lines: &mut Lines<BufReader<OwnedReadHalf>>,
        write: &mut OwnedWriteHalf,
        seq: u64,
        status: &str,
    ) -> (String, protocol::host::Decision) {
        let Some(ToHost::Answer { uuid, decision, .. }) = next(lines).await else {
            panic!("no answer reached the host");
        };
        say(
            write,
            serde_json::json!({"type": "outcome", "seq": seq, "uuid": uuid, "status": status}),
        )
        .await;
        assert_eq!(next(lines).await, Some(ToHost::Ack { seq }));
        (uuid, decision)
    }

    /// The run's open cards, as the fleet lists them.
    async fn cards(daemon: &Daemon, uid: &str) -> (Vec<String>, Vec<String>) {
        let listed = summary(daemon, uid).await;
        let pending = listed
            .pending
            .into_iter()
            .map(|card| card.request_id)
            .collect();
        (listed.blocked_on, pending)
    }

    /// How many cards of the run `uid` are kept for a restart.
    fn kept_cards(rig: &Rig, uid: &str) -> i64 {
        rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM pending_approvals WHERE session_uid = ?1",
                [uid],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// **B6 G1: a host's permission request is a card.** It is filed by the daemon with
    /// the host's cursor before its ack, listed as pending and as what the run is blocked
    /// on, with the hash a hook card of the same call carries, held for the apps, and kept
    /// for a restart; and the link stays up for the next request.
    #[tokio::test]
    async fn a_hosts_permission_request_is_a_card_the_apps_can_answer() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        host_asks(&mut played, 3, "toolu_1", serde_json::json!({})).await;
        assert_eq!(cursor(&rig, &uid), 3);
        assert_eq!(
            cards(&rig.daemon, &uid).await,
            (vec!["toolu_1".to_string()], vec!["toolu_1".to_string()])
        );
        let filed = stored(&rig, &uid);
        let card = filed
            .iter()
            .find(|event| event["kind"] == "approval_request")
            .expect("the card's event");
        assert_eq!(card["source"], "daemon");
        assert_eq!(card["source_event_id"], "perm:toolu_1");
        assert_eq!(card["payload"]["card"]["payload_hash"], bash_hash());
        assert_eq!(card["payload"]["card"]["question_hold"], "held");
        assert_eq!(kept_cards(&rig, &uid), 1);
        host_asks(&mut played, 4, "toolu_2", serde_json::json!({})).await;
        assert_eq!(kept_cards(&rig, &uid), 2);
        stop(&played.dir);
    }

    /// **B6 G2 and G6: allow.** The app's Allow goes to the host as `answer`; once the host
    /// says it reached claude, the answer is recorded and its card closes in one
    /// transaction with the cursor, and the app is told it applied, confirmed rather than
    /// only sent. The same answer again is a duplicate.
    #[tokio::test]
    async fn an_allow_applies_once_the_host_says_it_reached_claude() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        host_asks(&mut played, 3, "toolu_1", serde_json::json!({})).await;
        let Played {
            lines, write, dir, ..
        } = &mut played;
        let (result, (_, decision)) = tokio::join!(
            app_answers(&rig.daemon, &uid, "toolu_1", AnswerDecision::Allow),
            host_settles(lines, write, 4, "done")
        );
        assert_eq!(
            decision,
            protocol::host::Decision::Allow {
                updated_input: None,
                updated_permissions: None
            }
        );
        let AnswerResult::Applied { outcome } = result else {
            panic!("not applied: {result:?}");
        };
        assert!(!outcome.indeterminate);
        assert_eq!(outcome.applied_via, AnswerPath::HookReturn);
        assert_eq!(outcome.answered_on, Some(AnsweredOn::Iphone));
        assert_eq!(cards(&rig.daemon, &uid).await, (vec![], vec![]));
        assert_eq!(kept_cards(&rig, &uid), 0);
        assert_eq!(cursor(&rig, &uid), 4);
        let resolved = stored(&rig, &uid)
            .into_iter()
            .find(|event| event["kind"] == "approval_resolved")
            .expect("the card's resolution");
        assert_eq!(resolved["source_event_id"], "resolved:toolu_1");
        assert!(matches!(
            app_answers(&rig.daemon, &uid, "toolu_1", AnswerDecision::Allow).await,
            AnswerResult::Duplicate { .. }
        ));
        assert_eq!(next(lines).await, None, "nothing sent again");
        stop(dir);
    }

    /// **B6 G3: deny.** A deny of the main conversation's call also ends its turn; a
    /// subagent's (`agentID`) does not; a deny with a reason carries it to claude and does
    /// not end the turn.
    #[tokio::test]
    async fn a_deny_ends_the_main_turn_but_not_a_subagents_and_a_reason_reaches_claude() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let cases = [
            (
                "toolu_main",
                serde_json::json!({}),
                AnswerDecision::Deny,
                true,
                None,
            ),
            (
                "toolu_sub",
                serde_json::json!({"agentID": "agent-1"}),
                AnswerDecision::Deny,
                false,
                None,
            ),
            (
                "toolu_why",
                serde_json::json!({}),
                AnswerDecision::Text {
                    text: "not that file".into(),
                },
                false,
                Some("not that file"),
            ),
        ];
        let mut seq = 3;
        for (request_id, options, answer, interrupts, reason) in cases {
            host_asks(&mut played, seq, request_id, options).await;
            let Played { lines, write, .. } = &mut played;
            let (result, (_, decision)) = tokio::join!(
                app_answers(&rig.daemon, &uid, request_id, answer),
                host_settles(lines, write, seq + 1, "done")
            );
            assert!(
                matches!(result, AnswerResult::Applied { .. }),
                "{request_id}"
            );
            let protocol::host::Decision::Deny { message, interrupt } = decision else {
                panic!("{request_id}: not a deny");
            };
            assert_eq!(interrupt, interrupts, "{request_id}");
            if let Some(reason) = reason {
                assert!(message.ends_with(reason), "{request_id}: {message}");
            }
            seq += 2;
        }
        stop(&played.dir);
    }

    /// **B6 G4: withdrawn.** claude stops waiting on a request (End run, or its own end):
    /// its card closes, nobody having answered, with no answer recorded, so a late answer
    /// finds no card.
    #[tokio::test]
    async fn a_withdrawn_request_closes_its_card_with_nobody_answering() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        host_asks(&mut played, 3, "toolu_1", serde_json::json!({})).await;
        say(
            &mut played.write,
            serde_json::json!({"type": "withdrawn", "seq": 4, "tool_use_id": "toolu_1"}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 4 }));
        assert_eq!(cards(&rig.daemon, &uid).await, (vec![], vec![]));
        assert_eq!(kept_cards(&rig, &uid), 0);
        let resolved = stored(&rig, &uid)
            .into_iter()
            .find(|event| event["kind"] == "approval_resolved")
            .expect("the card's closing");
        assert_eq!(resolved["payload"]["resolved_by"], "timeout");
        assert_eq!(
            resolved["payload"]["detail"],
            "the session ended before anyone answered"
        );
        assert_eq!(
            rig.daemon
                .db
                .get_answer(uid.clone(), "toolu_1".into())
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            app_answers(&rig.daemon, &uid, "toolu_1", AnswerDecision::Allow).await,
            AnswerResult::Rejected {
                reason: "unknown or already-resolved request".into()
            }
        );
        assert_eq!(next(&mut played.lines).await, None, "nothing sent");
        stop(&played.dir);
    }

    /// **B6 G4: the run ends with a card open.** claude's end closes its cards the same
    /// way, nobody having answered, with no answer recorded and nothing labelled typed.
    #[tokio::test]
    async fn a_hosted_runs_end_closes_its_cards_with_nobody_answering() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        host_asks(&mut played, 3, "toolu_1", serde_json::json!({})).await;
        say(
            &mut played.write,
            serde_json::json!({"type": "exited", "seq": 4, "code": 0}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 4 }));
        let mut closed = false;
        for _ in 0..100 {
            if cards(&rig.daemon, &uid).await == (vec![], vec![]) {
                closed = true;
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        assert!(closed, "the card outlived its run");
        let resolved = stored(&rig, &uid)
            .into_iter()
            .find(|event| event["kind"] == "approval_resolved")
            .expect("the card's closing");
        assert_eq!(resolved["payload"]["resolved_by"], "timeout");
        assert_ne!(resolved["payload"]["applied_via"], "send_keys");
        assert_eq!(
            rig.daemon
                .db
                .get_answer(uid.clone(), "toolu_1".into())
                .await
                .unwrap(),
            None
        );
        stop(&played.dir);
    }

    /// **B6: an answer the host refuses** (claude stopped asking first) is not recorded,
    /// its claim is let go, and the app is told why.
    #[tokio::test]
    async fn an_answer_the_host_refuses_is_not_recorded() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        host_asks(&mut played, 3, "toolu_1", serde_json::json!({})).await;
        let Played {
            lines, write, dir, ..
        } = &mut played;
        let (result, _) = tokio::join!(
            app_answers(&rig.daemon, &uid, "toolu_1", AnswerDecision::Allow),
            host_settles(lines, write, 4, "refused")
        );
        assert_eq!(
            result,
            AnswerResult::Rejected {
                reason: "Claude was no longer asking, so the answer was not used.".into()
            }
        );
        assert!(rig
            .daemon
            .db
            .answer_claim(uid.clone(), "toolu_1".into())
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            rig.daemon
                .db
                .get_answer(uid.clone(), "toolu_1".into())
                .await
                .unwrap(),
            None
        );
        stop(dir);
    }

    /// A question claude asks a hosted run: one single choice whose text holds a tab and a
    /// variation selector (⚠️), as does its first option's label, and whose preview is
    /// indented with a tab, text the terminal's dialog would rewrite.
    fn question_input() -> serde_json::Value {
        serde_json::json!({"questions": [{"question": "Which\tstart ⚠️?", "header": "Start",
            "multiSelect": false, "options": [
                {"label": "⚠️ Now", "description": "", "preview": "if ready {\n\tstart()\n}"},
                {"label": "Later", "description": "", "preview": "wait()"}]}]})
    }

    /// The stand-in host relays the question `tool_use_id` as message `seq` and reads the
    /// ack.
    async fn host_asks_question(
        played: &mut Played,
        seq: u64,
        tool_use_id: &str,
        options: serde_json::Value,
    ) {
        say(
            &mut played.write,
            serde_json::json!({"type": "permission", "seq": seq, "request": {
                "tool_use_id": tool_use_id, "tool_name": "AskUserQuestion",
                "input": question_input(), "options": options}}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
    }

    /// An app's `decision` on the question `request_id` of the run `uid`, from `on`.
    async fn app_answers_question(
        daemon: &Daemon,
        uid: &str,
        request_id: &str,
        decision: AnswerDecision,
        on: AnsweredOn,
    ) -> AnswerResult {
        let hash = protocol::hash::approval_payload_hash("AskUserQuestion", &question_input());
        daemon
            .answer(request_id, &hash, decision, Some(uid), on)
            .await
    }

    /// **B6 part 2a, G8a: a hosted run's question.** It is a question card at birth, held
    /// and never bound to an allow; born as anything else, its answers would be refused.
    /// The app's answers reach the host as the call's input: the chosen label and the
    /// preview with the notes as its annotation, though the terminal would rewrite that
    /// text and the question's own. The answer is then `Applied` and the card closes. An
    /// incomplete answer is refused and reaches nothing: the first answer the host reads
    /// is the whole one.
    #[tokio::test]
    async fn a_hosted_question_is_answered_with_its_answers_as_its_input() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        host_asks_question(&mut played, 3, "toolu_q", serde_json::json!({})).await;
        let asked = stored(&rig, &uid)
            .into_iter()
            .find(|event| event["kind"] == "approval_request")
            .unwrap();
        assert_ne!(asked["payload"]["card"]["identity_bound"], true);
        assert_eq!(asked["payload"]["card"]["question_hold"], "held");
        let incomplete = app_answers_question(
            &rig.daemon,
            &uid,
            "toolu_q",
            AnswerDecision::Answers { answers: vec![] },
            AnsweredOn::Iphone,
        )
        .await;
        assert!(
            matches!(&incomplete, AnswerResult::Rejected { reason }
                if reason.contains("every question needs exactly one answer")),
            "{incomplete:?}"
        );
        let answers = vec![protocol::ws::QuestionAnswer {
            selected: vec![0],
            other: None,
            notes: Some("go".into()),
        }];
        let (result, (_, decision)) = tokio::join!(
            app_answers_question(
                &rig.daemon,
                &uid,
                "toolu_q",
                AnswerDecision::Answers {
                    answers: answers.clone()
                },
                AnsweredOn::Iphone
            ),
            host_settles(&mut played.lines, &mut played.write, 4, "done")
        );
        let protocol::host::Decision::Allow {
            updated_input: Some(input),
            updated_permissions: None,
        } = decision
        else {
            panic!("{decision:?}");
        };
        assert_eq!(
            input["answers"],
            serde_json::json!({"Which\tstart ⚠️?": "⚠️ Now"})
        );
        assert_eq!(
            input["annotations"],
            serde_json::json!({"Which\tstart ⚠️?": {"preview": "if ready {\n\tstart()\n}", "notes": "go"}})
        );
        assert_eq!(input["questions"], question_input()["questions"]);
        let AnswerResult::Applied { outcome } = result else {
            panic!("{result:?}");
        };
        assert_eq!(outcome.decision, AnswerDecision::Answers { answers });
        assert!(!outcome.indeterminate);
        assert_eq!(cards(&rig.daemon, &uid).await, (vec![], vec![]));
        stop(&played.dir);
    }

    /// **B6 part 2a, G8b: a hosted question declined.** The main conversation's question,
    /// declined from the iPhone, is a deny that ends its turn, saying who declined it. A
    /// subagent's, answered Deny from the Mac, is a decline without the interrupt. Both
    /// are `Applied`, so the card reads declined.
    #[tokio::test]
    async fn a_hosted_questions_decline_ends_the_main_turn_but_not_a_subagents() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        host_asks_question(&mut played, 3, "toolu_main", serde_json::json!({})).await;
        host_asks_question(
            &mut played,
            4,
            "toolu_sub",
            serde_json::json!({"agentID": "agent-1"}),
        )
        .await;
        let (result, (_, decision)) = tokio::join!(
            app_answers_question(
                &rig.daemon,
                &uid,
                "toolu_main",
                AnswerDecision::Decline,
                AnsweredOn::Iphone
            ),
            host_settles(&mut played.lines, &mut played.write, 5, "done")
        );
        assert_eq!(
            decision,
            protocol::host::Decision::Deny {
                message: "Declined from iPhone".into(),
                interrupt: true,
            }
        );
        assert!(matches!(result, AnswerResult::Applied { .. }), "{result:?}");
        let (result, (_, decision)) = tokio::join!(
            app_answers_question(
                &rig.daemon,
                &uid,
                "toolu_sub",
                AnswerDecision::Deny,
                AnsweredOn::Mac
            ),
            host_settles(&mut played.lines, &mut played.write, 6, "done")
        );
        assert_eq!(
            decision,
            protocol::host::Decision::Deny {
                message: "Declined from Mac".into(),
                interrupt: false,
            }
        );
        assert!(matches!(result, AnswerResult::Applied { .. }), "{result:?}");
        stop(&played.dir);
    }

    /// **B6 part 2a, G8c: a hosted question takes only its answers or a decline.** Allow,
    /// any option, a reason and an option id are refused on it; answers or a decline on an
    /// ordinary card are refused too. None reaches the host: the first answer it reads is
    /// the question's own decline.
    #[tokio::test]
    async fn only_answers_or_a_decline_answer_a_hosted_question() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        host_asks_question(&mut played, 3, "toolu_q", serde_json::json!({})).await;
        host_asks(&mut played, 4, "toolu_bash", serde_json::json!({})).await;
        for decision in [
            AnswerDecision::Allow,
            AnswerDecision::Option { index: 1 },
            AnswerDecision::Option { index: 2 },
            AnswerDecision::Text { text: "no".into() },
            AnswerDecision::OptionId {
                option_id: "acceptForSession".into(),
            },
        ] {
            let result = app_answers_question(
                &rig.daemon,
                &uid,
                "toolu_q",
                decision.clone(),
                AnsweredOn::Iphone,
            )
            .await;
            assert!(
                matches!(result, AnswerResult::Rejected { .. }),
                "{decision:?}: {result:?}"
            );
        }
        for decision in [
            AnswerDecision::Answers {
                answers: vec![protocol::ws::QuestionAnswer {
                    selected: vec![0],
                    ..Default::default()
                }],
            },
            AnswerDecision::Decline,
        ] {
            let result = app_answers(&rig.daemon, &uid, "toolu_bash", decision.clone()).await;
            assert!(
                matches!(&result, AnswerResult::Rejected { reason }
                    if reason.contains("this card is not one")),
                "{decision:?}: {result:?}"
            );
        }
        let (result, (_, decision)) = tokio::join!(
            app_answers_question(
                &rig.daemon,
                &uid,
                "toolu_q",
                AnswerDecision::Decline,
                AnsweredOn::Iphone
            ),
            host_settles(&mut played.lines, &mut played.write, 5, "done")
        );
        assert!(
            matches!(decision, protocol::host::Decision::Deny { .. }),
            "{decision:?}"
        );
        assert!(matches!(result, AnswerResult::Applied { .. }), "{result:?}");
        stop(&played.dir);
    }

    /// Claude's allow rule for the `ls` call, as it suggests one: for the folder's own
    /// settings file.
    fn allow_rule() -> serde_json::Value {
        serde_json::json!({"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "ls"}],
                           "behavior": "allow", "destination": "localSettings"})
    }

    /// `update` with the session as its destination.
    fn for_the_session_only(mut update: serde_json::Value) -> serde_json::Value {
        update["destination"] = "session".into();
        update
    }

    /// The answer an app sends for "Allow for this session".
    fn allow_for_this_session() -> AnswerDecision {
        AnswerDecision::OptionId {
            option_id: "acceptForSession".into(),
        }
    }

    /// The run `uid`'s card `request_id` as the apps read it.
    fn filed_card(rig: &Rig, uid: &str, request_id: &str) -> serde_json::Value {
        stored(rig, uid)
            .into_iter()
            .find(|event| {
                event["kind"] == "approval_request"
                    && event["payload"]["card"]["request_id"] == request_id
            })
            .expect("the card's event")["payload"]["card"]
            .clone()
    }

    /// **B6 part 2b, G15: who is offered "Allow for this session".** A card whose request
    /// carries claude's own allow rule offers it; one whose request has no allow rule, only
    /// a deny rule or other suggestions, or says `suppressAlwaysAllowRule`, does not, and
    /// the field is left out; nor does a question, whatever its suggestions.
    #[tokio::test]
    async fn only_a_card_with_claudes_allow_rule_offers_allow_for_this_session() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let mut deny = allow_rule();
        deny["behavior"] = "deny".into();
        let cases = [
            (
                "toolu_rule",
                serde_json::json!({"suggestions": [allow_rule()]}),
                true,
            ),
            (
                "toolu_suppressed",
                serde_json::json!({"suggestions": [allow_rule()], "suppressAlwaysAllowRule": true}),
                false,
            ),
            (
                "toolu_others",
                serde_json::json!({"suggestions": [
                    {"type": "addDirectories", "directories": ["/tmp/x"], "destination": "session"},
                    {"type": "setMode", "mode": "acceptEdits", "destination": "session"}]}),
                false,
            ),
            (
                "toolu_deny",
                serde_json::json!({"suggestions": [deny]}),
                false,
            ),
            ("toolu_none", serde_json::json!({}), false),
        ];
        let mut seq = 3;
        for (request_id, options, offered) in cases {
            host_asks(&mut played, seq, request_id, options).await;
            seq += 1;
            let expected = offered.then_some(serde_json::Value::Bool(true));
            assert_eq!(
                filed_card(&rig, &uid, request_id).get("allow_for_session"),
                expected.as_ref(),
                "{request_id}"
            );
        }
        host_asks_question(
            &mut played,
            seq,
            "toolu_q",
            serde_json::json!({"suggestions": [allow_rule()]}),
        )
        .await;
        assert_eq!(
            filed_card(&rig, &uid, "toolu_q").get("allow_for_session"),
            None
        );
        stop(&played.dir);
    }

    /// **B6 part 2b, G16: what claude is sent for "Allow for this session".** Every allow
    /// rule claude suggested, its rules as claude gave them and the session as its
    /// destination, and nothing else: no deny rule, no rule replacement, no folder and no
    /// mode. The answer is recorded as the app chose it, and a retry is a duplicate that
    /// sends nothing.
    #[tokio::test]
    async fn allow_for_this_session_sends_claudes_allow_rules_for_the_session_only() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let project = serde_json::json!({"type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "ls"}, {"toolName": "Read"}],
            "behavior": "allow", "destination": "projectSettings"});
        let mut deny = allow_rule();
        deny["behavior"] = "deny".into();
        let options = serde_json::json!({"suggestions": [
            allow_rule(),
            {"type": "addDirectories", "directories": ["/tmp/x"], "destination": "session"},
            deny,
            project.clone(),
            {"type": "replaceRules", "rules": [{"toolName": "Bash"}], "behavior": "allow",
             "destination": "localSettings"},
            {"type": "setMode", "mode": "acceptEdits", "destination": "session"}]});
        host_asks(&mut played, 3, "toolu_1", options).await;
        let Played {
            lines, write, dir, ..
        } = &mut played;
        let (result, (_, decision)) = tokio::join!(
            app_answers(&rig.daemon, &uid, "toolu_1", allow_for_this_session()),
            host_settles(lines, write, 4, "done")
        );
        assert_eq!(
            decision,
            protocol::host::Decision::Allow {
                updated_input: None,
                updated_permissions: Some(serde_json::json!([
                    for_the_session_only(allow_rule()),
                    for_the_session_only(project)
                ])),
            }
        );
        let AnswerResult::Applied { outcome } = result else {
            panic!("not applied: {result:?}");
        };
        assert_eq!(outcome.decision, allow_for_this_session());
        let Some((_, recorded)) = rig
            .daemon
            .db
            .get_answer(uid.clone(), "toolu_1".into())
            .await
            .unwrap()
        else {
            panic!("no answer recorded");
        };
        assert_eq!(recorded.decision, allow_for_this_session());
        assert!(matches!(
            app_answers(&rig.daemon, &uid, "toolu_1", allow_for_this_session()).await,
            AnswerResult::Duplicate { .. }
        ));
        assert_eq!(next(lines).await, None, "nothing sent again");
        stop(dir);
    }

    /// **B6 part 2b: "Allow for this session" only where the card offers it.** On a card
    /// that does not offer it, and any other option id on one that does, the answer is
    /// refused before any claim: no claim is recorded, nothing reaches the host (the first
    /// answer it reads is the Allow that follows), and the cards stay open.
    #[tokio::test]
    async fn allow_for_this_session_is_refused_where_the_card_does_not_offer_it() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        host_asks(
            &mut played,
            3,
            "toolu_plain",
            serde_json::json!({"suggestions": [
                {"type": "setMode", "mode": "acceptEdits", "destination": "session"}]}),
        )
        .await;
        host_asks(
            &mut played,
            4,
            "toolu_rule",
            serde_json::json!({"suggestions": [allow_rule()]}),
        )
        .await;
        let accept = AnswerDecision::OptionId {
            option_id: "accept".into(),
        };
        for (request_id, decision) in [
            ("toolu_plain", allow_for_this_session()),
            ("toolu_rule", accept),
        ] {
            let result = app_answers(&rig.daemon, &uid, request_id, decision).await;
            assert!(
                matches!(&result, AnswerResult::Rejected { reason }
                    if reason.contains("Nothing was sent")),
                "{request_id}: {result:?}"
            );
            assert!(
                rig.daemon
                    .db
                    .answer_claim(uid.clone(), request_id.into())
                    .await
                    .unwrap()
                    .is_none(),
                "{request_id}: claimed"
            );
        }
        let (_, mut pending) = cards(&rig.daemon, &uid).await;
        pending.sort();
        assert_eq!(pending, ["toolu_plain", "toolu_rule"]);
        let Played { lines, write, .. } = &mut played;
        let (result, (_, decision)) = tokio::join!(
            app_answers(&rig.daemon, &uid, "toolu_plain", AnswerDecision::Allow),
            host_settles(lines, write, 5, "done")
        );
        assert!(matches!(result, AnswerResult::Applied { .. }), "{result:?}");
        assert_eq!(
            decision,
            protocol::host::Decision::Allow {
                updated_input: None,
                updated_permissions: None
            }
        );
        stop(&played.dir);
    }

    /// **"Allow for this session", as a minor-36 daemon sends it, emitted by this build.**
    /// A copy of it under `ios/` is kept byte-identical by
    /// `fixtures/check-duplicated-fixtures.sh`.
    #[tokio::test]
    async fn the_minor_36_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/claude/minor-36-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_minor_36_wire().await,
            committed,
            "the committed minor-36 wire contract no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_minor_36_wire_fixture, then copy it over any copy under ios/ \
             (fixtures/check-duplicated-fixtures.sh names them)"
        );
    }

    /// Rewrite `fixtures/claude/minor-36-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_minor_36_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_minor_36_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/minor-36-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// claude 2.1.322's request for `touch m1.txt` (probe M1) with its three suggestions,
    /// and the same call with no allow rule among them: the card each is filed as; the
    /// answer an app sends for "Allow for this session"; and the daemon's refusal of that
    /// answer on the card that does not offer it.
    async fn derived_minor_36_wire() -> serde_json::Value {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let input = serde_json::json!({"command": "touch m1.txt"});
        let rule = serde_json::json!({"type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "touch m1.txt"}],
            "behavior": "allow", "destination": "localSettings"});
        let others = [
            serde_json::json!({"type": "addDirectories", "directories": ["/tmp/ccd-minor-36/app"],
                               "destination": "session"}),
            serde_json::json!({"type": "setMode", "mode": "acceptEdits", "destination": "session"}),
        ];
        let offered = [vec![rule], others.to_vec()].concat();
        for (seq, tool_use_id, suggestions) in [
            (3, "toolu_offered", offered),
            (4, "toolu_not_offered", others.to_vec()),
        ] {
            say(
                &mut played.write,
                serde_json::json!({"type": "permission", "seq": seq, "request": {
                    "tool_use_id": tool_use_id, "tool_name": "Bash", "input": input,
                    "options": {"suggestions": suggestions}}}),
            )
            .await;
            assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
        }
        let hash = protocol::hash::approval_payload_hash("Bash", &input);
        let refused = rig
            .daemon
            .answer(
                "toolu_not_offered",
                &hash,
                allow_for_this_session(),
                Some(&uid),
                AnsweredOn::Iphone,
            )
            .await;
        let wire = serde_json::json!({
            "card_offering": filed_card(&rig, &uid, "toolu_offered"),
            "card_not_offering": filed_card(&rig, &uid, "toolu_not_offered"),
            "answer": serde_json::to_value(protocol::ws::ClientMessage::Answer {
                request_id: "toolu_offered".into(),
                payload_hash: hash,
                decision: allow_for_this_session(),
                session_id: Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR".into()),
            })
            .unwrap(),
            "refused": serde_json::to_value(protocol::ws::ServerMessage::AnswerResult {
                request_id: "toolu_not_offered".into(),
                result: refused,
            })
            .unwrap(),
        });
        stop(&played.dir);
        wire
    }

    /// The app's Stop of the turn `turn` of the run `uid`, as request `request_id`, as the Kit
    /// sends it: its hash is over the run's uid.
    async fn stop_as_app(
        daemon: &Daemon,
        uid: &str,
        request_id: &str,
        turn: &str,
    ) -> protocol::ws::InterruptResult {
        let hash = protocol::hash::interrupt_hash(uid, turn);
        daemon.interrupt(uid, request_id, turn, &hash).await
    }

    /// The stand-in host begins the turn `turn` with claude's `system/init`, as message `seq`.
    async fn turn_begins(played: &mut Played, seq: u64, turn: &str) {
        sdk(
            &mut played.write,
            seq,
            serde_json::json!({"type": "system", "subtype": "init", "uuid": turn,
                               "session_id": "5f0c1e2a-0000-4000-8000-000000000001"}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
    }

    /// The interrupt the stand-in host reads next: its uuid, and the turn it names.
    async fn host_reads_stop(played: &mut Played) -> (String, String) {
        let Some(ToHost::Interrupt { uuid, turn_id }) = next(&mut played.lines).await else {
            panic!("no interrupt reached the host");
        };
        (uuid, turn_id)
    }

    /// The stand-in host ends the stopped turn: claude's `result` ending `reason` as message
    /// `seq`, then the interrupt `uuid`'s outcome `done` as `seq + 1`, with the receipt's
    /// `cancelled`.
    async fn host_ends_turn(
        played: &mut Played,
        seq: u64,
        uuid: &str,
        reason: &str,
        cancelled: &[String],
    ) {
        let aborted = reason.starts_with("aborted");
        sdk(
            &mut played.write,
            seq,
            serde_json::json!({"type": "result",
                "subtype": if aborted { "error_during_execution" } else { "success" },
                "is_error": aborted, "terminal_reason": reason, "num_turns": 1,
                "uuid": format!("9d1f0000-0000-4000-8000-0000000001{seq:02}"),
                "session_id": "5f0c1e2a-0000-4000-8000-000000000001"}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
        say(
            &mut played.write,
            serde_json::json!({"type": "outcome", "seq": seq + 1, "uuid": uuid, "status": "done",
                               "terminal_reason": reason, "cancelled": cancelled}),
        )
        .await;
        assert_eq!(
            next(&mut played.lines).await,
            Some(ToHost::Ack { seq: seq + 1 })
        );
    }

    /// The stand-in host refuses the interrupt `uuid` as message `seq`: it is not running
    /// that turn.
    async fn host_refuses_stop(played: &mut Played, seq: u64, uuid: &str) {
        say(
            &mut played.write,
            serde_json::json!({"type": "outcome", "seq": seq, "uuid": uuid, "status": "refused",
                               "detail": "that turn is not running"}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
    }

    /// Where the run `uid`'s Stop `request_id` stands in the interrupt ledger: its status
    /// word, or `None` when there is no claim.
    async fn stop_record(daemon: &Daemon, uid: &str, request_id: &str) -> Option<String> {
        let state = daemon
            .db
            .mutation_status(
                crate::store::OPERATION_INTERRUPT,
                uid.to_string(),
                request_id.to_string(),
            )
            .await
            .unwrap()?;
        Some(match state.status {
            crate::store::AnswerStatus::Settled(outcome) => outcome,
            crate::store::AnswerStatus::Applying => "applying".into(),
            crate::store::AnswerStatus::Indeterminate => "indeterminate".into(),
        })
    }

    /// The `send_not_sent` notices the run has filed, by request id.
    fn not_sent_notices(rig: &Rig, uid: &str) -> Vec<serde_json::Value> {
        stored(rig, uid)
            .into_iter()
            .filter(|event| event["payload"]["notification_type"] == "send_not_sent")
            .map(|event| event["payload"].clone())
            .collect()
    }

    /// Until the run `uid`'s link has ended, 5 s at most.
    async fn until_detached(daemon: &Daemon, uid: &str) {
        for _ in 0..100 {
            if !daemon.host_attached(uid).await {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
        panic!("the link to {uid}'s host did not end");
    }

    /// **B7 part 2 S1, S4 and S7: a Stop aborts the running turn, a second one waits, and
    /// the message it cancelled is marked.** The app's Stop reaches the host as
    /// `interrupt` naming the turn. A second Stop of the turn while the first waits is
    /// refused at once, and nothing more reaches the host. The host files the turn's end
    /// and then its outcome: the Stop is `aborted`, after the turn's `turn_complete`, and
    /// recorded so; a retry is a `duplicate` and sends nothing. Of the person's two
    /// messages, both sent and filed before the Stop, the one the receipt lists gets one
    /// `send_not_sent` with its request id, as its own row carries, and the other none; a
    /// uuid that names no message files nothing.
    #[tokio::test]
    async fn a_stop_aborts_the_running_turn_and_marks_the_message_it_cancelled() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        turn_begins(&mut played, 3, "turn-1").await;
        let (sent, _) = tokio::join!(
            send_as_app(&rig.daemon, &uid, "r-1", "and the docs"),
            host_takes(&mut played, 4, "done")
        );
        assert!(matches!(sent, SendTextResult::Sent { .. }), "{sent:?}");
        let (ran, _) = tokio::join!(
            send_as_app(&rig.daemon, &uid, "r-2", "and the tests"),
            host_takes(&mut played, 5, "done")
        );
        assert!(matches!(ran, SendTextResult::Sent { .. }), "{ran:?}");
        let cancelled = [send_uuid(&uid, "r-1"), "a-uuid-of-no-send".to_string()];
        let (first, second) =
            tokio::join!(stop_as_app(&rig.daemon, &uid, "stop-1", "turn-1"), async {
                let (uuid, turn) = host_reads_stop(&mut played).await;
                assert_eq!(turn, "turn-1");
                let second = stop_as_app(&rig.daemon, &uid, "stop-2", "turn-1").await;
                host_ends_turn(&mut played, 6, &uuid, "aborted_tools", &cancelled).await;
                second
            });
        assert_eq!(
            first,
            protocol::ws::InterruptResult::Aborted {
                turn_id: "turn-1".into()
            }
        );
        assert_eq!(
            second,
            protocol::ws::InterruptResult::Rejected {
                reason: crate::codex_refusals::INTERRUPT_ALREADY_WAITING.into()
            }
        );
        assert!(stored(&rig, &uid)
            .iter()
            .any(|event| event["kind"] == "turn_complete" && event["turn_id"] == "turn-1"));
        assert_eq!(
            stop_record(&rig.daemon, &uid, "stop-1").await.as_deref(),
            Some("aborted")
        );
        assert_eq!(stop_record(&rig.daemon, &uid, "stop-2").await, None);
        assert_eq!(
            not_sent_notices(&rig, &uid),
            [serde_json::json!({"notification_type": "send_not_sent",
                "message": "Not sent: stopped", "request_id": "r-1", "reason": "stopped"})]
        );
        let row = stored(&rig, &uid)
            .into_iter()
            .find(|event| {
                event["kind"] == "user_message" && event["payload"]["text"] == "and the docs"
            })
            .expect("the person's row");
        assert_eq!(row["payload"]["request_id"], "r-1");
        assert_eq!(
            stop_as_app(&rig.daemon, &uid, "stop-1", "turn-1").await,
            protocol::ws::InterruptResult::Duplicate {
                turn_id: "turn-1".into()
            }
        );
        assert_eq!(next(&mut played.lines).await, None, "nothing sent again");
        stop(&played.dir);
    }

    /// **B7 part 2 S2 and S3: a turn that ended on its own, and one the host is not
    /// running.** A Stop whose turn ended `completed` before the interrupt took is refused
    /// with that fact and recorded so. One the host refuses (not the turn it runs) is
    /// refused as such and its claim given back: the same request id is a first attempt
    /// again, and reaches the host.
    #[tokio::test]
    async fn a_stop_of_a_turn_that_ended_or_is_not_running_says_so() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        turn_begins(&mut played, 3, "turn-2").await;
        let (ended, _) = tokio::join!(stop_as_app(&rig.daemon, &uid, "stop-3", "turn-2"), async {
            let (uuid, _) = host_reads_stop(&mut played).await;
            host_ends_turn(&mut played, 4, &uuid, "completed", &[]).await;
        });
        assert_eq!(
            ended,
            protocol::ws::InterruptResult::Rejected {
                reason: crate::codex_refusals::interrupt_turn_ended_itself("completed")
            }
        );
        assert_eq!(
            stop_record(&rig.daemon, &uid, "stop-3").await.as_deref(),
            Some("turn_ended")
        );
        turn_begins(&mut played, 6, "turn-3").await;
        let (refused, _) =
            tokio::join!(stop_as_app(&rig.daemon, &uid, "stop-4", "turn-3"), async {
                let (uuid, _) = host_reads_stop(&mut played).await;
                host_refuses_stop(&mut played, 7, &uuid).await;
            });
        assert_eq!(
            refused,
            protocol::ws::InterruptResult::Rejected {
                reason: crate::codex_refusals::CLAUDE_TURN_NOT_RUNNING.into()
            }
        );
        assert_eq!(stop_record(&rig.daemon, &uid, "stop-4").await, None);
        let (again, _) = tokio::join!(stop_as_app(&rig.daemon, &uid, "stop-4", "turn-3"), async {
            let (uuid, _) = host_reads_stop(&mut played).await;
            host_ends_turn(&mut played, 8, &uuid, "aborted_streaming", &[]).await;
        });
        assert_eq!(
            again,
            protocol::ws::InterruptResult::Aborted {
                turn_id: "turn-3".into()
            }
        );
        stop(&played.dir);
    }

    /// **B7 part 2 S5: a Stop that cannot reach a running turn is refused, with no
    /// claim.** A stale hash and an empty turn; a run ending (`stopping`); a run whose
    /// link is not attached; one that has ended; one still starting. None claims anything
    /// or reaches a host.
    #[tokio::test]
    async fn a_stop_that_cannot_reach_a_running_turn_is_refused_with_no_claim() {
        let says = |reason: &str| protocol::ws::InterruptResult::Rejected {
            reason: reason.to_string(),
        };
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        turn_begins(&mut played, 3, "turn-1").await;
        assert_eq!(
            rig.daemon
                .interrupt(&uid, "stop-1", "turn-1", "not-the-hash")
                .await,
            says(crate::codex_refusals::INTERRUPT_STALE_HASH)
        );
        assert_eq!(
            stop_as_app(&rig.daemon, &uid, "stop-1", "").await,
            says(crate::codex_refusals::INTERRUPT_NAMES_NO_TURN)
        );
        assert_eq!(rig.daemon.stop_run(&uid).await, StopRunResult::Stopping);
        assert!(matches!(
            next(&mut played.lines).await,
            Some(ToHost::Stop { .. })
        ));
        assert_eq!(
            stop_as_app(&rig.daemon, &uid, "stop-1", "turn-1").await,
            says(crate::codex_refusals::INTERRUPT_ALREADY_WAITING)
        );
        assert_eq!(
            next(&mut played.lines).await,
            None,
            "nothing reached the host"
        );
        assert_eq!(stop_record(&rig.daemon, &uid, "stop-1").await, None);
        stop(&played.dir);

        let rig = self::rig();
        let played = live_run(&rig).await;
        let uid = played.uid.clone();
        let dir = played.dir.clone();
        drop(played);
        until_detached(&rig.daemon, &uid).await;
        assert_eq!(
            stop_as_app(&rig.daemon, &uid, "stop-1", "turn-1").await,
            says(crate::codex_refusals::ENGINE_NO_CONNECTION)
        );
        assert_eq!(stop_record(&rig.daemon, &uid, "stop-1").await, None);
        stop(&dir);

        let rig = self::rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        say(
            &mut played.write,
            serde_json::json!({"type": "exited", "seq": 3, "code": 0}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 3 }));
        assert_eq!(
            stop_as_app(&rig.daemon, &uid, "stop-1", "turn-1").await,
            says(crate::codex_refusals::INTERRUPT_RUN_IS_GONE)
        );
        assert_eq!(stop_record(&rig.daemon, &uid, "stop-1").await, None);
        stop(&played.dir);

        let rig = self::rig();
        let cwd = rig.root.to_str().unwrap().to_string();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let (starting, _) = begun(&rig, &cwd, &host, Trust::Trusted).await;
        assert_eq!(
            stop_as_app(&rig.daemon, &starting, "stop-1", "turn-1").await,
            says(crate::codex_refusals::CLAUDE_TURN_NOT_RUNNING)
        );
        assert_eq!(stop_record(&rig.daemon, &starting, "stop-1").await, None);
    }

    /// **B7 part 2 S6: a Stop whose link ends before its outcome.** It is unknown, never
    /// "nothing was sent", and its claim stays: a retry before any restart is unknown too.
    /// A restarted ccd's sweep settles it unknown; the host's replayed outcome then marks
    /// the message it cancelled, once across a second replay, and a retry stays unknown.
    #[tokio::test]
    async fn a_stop_whose_link_ends_before_its_outcome_stays_unknown() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let dir = played.dir.clone();
        turn_begins(&mut played, 3, "turn-1").await;
        let (sent, _) = tokio::join!(
            send_as_app(&rig.daemon, &uid, "r-1", "and the docs"),
            host_takes(&mut played, 4, "done")
        );
        assert!(matches!(sent, SendTextResult::Sent { .. }), "{sent:?}");
        let (lost, uuid) =
            tokio::join!(stop_as_app(&rig.daemon, &uid, "stop-1", "turn-1"), async {
                let (uuid, _) = host_reads_stop(&mut played).await;
                drop(played);
                uuid
            });
        assert_eq!(
            lost,
            protocol::ws::InterruptResult::Indeterminate {
                reason: crate::codex_refusals::interrupt_settled_unknown(
                    crate::codex_refusals::CLAUDE_CONNECTION_ENDED
                )
            }
        );
        assert_eq!(
            stop_record(&rig.daemon, &uid, "stop-1").await.as_deref(),
            Some("applying")
        );
        until_detached(&rig.daemon, &uid).await;
        let unknown = protocol::ws::InterruptResult::Indeterminate {
            reason: crate::codex_refusals::INTERRUPT_ALREADY_SENT_UNKNOWN.into(),
        };
        assert_eq!(
            stop_as_app(&rig.daemon, &uid, "stop-1", "turn-1").await,
            unknown
        );
        let (restarted, mut lines, mut write) = restarted(&rig, &dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 4 }));
        let outcome = |seq: u64| {
            serde_json::json!({"type": "outcome", "seq": seq, "uuid": uuid, "status": "done",
                "terminal_reason": "aborted_tools", "cancelled": [send_uuid(&uid, "r-1")]})
        };
        say(&mut write, outcome(5)).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 5 }));
        say(&mut write, outcome(6)).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 6 }));
        assert_eq!(
            not_sent_notices(&rig, &uid)
                .iter()
                .map(|notice| notice["request_id"].clone())
                .collect::<Vec<_>>(),
            [serde_json::json!("r-1")]
        );
        assert_eq!(
            stop_record(&restarted, &uid, "stop-1").await.as_deref(),
            Some("indeterminate")
        );
        assert_eq!(
            stop_as_app(&restarted, &uid, "stop-1", "turn-1").await,
            unknown
        );
        stop(&dir);
    }

    /// **B7 part 2 S8: a card open at a Stop reads stopped.** The host withdraws the open
    /// request while the Stop waits, and its card closes saying the turn was stopped; a
    /// card withdrawn with no Stop waiting keeps saying the session ended.
    #[tokio::test]
    async fn a_card_open_at_a_stop_reads_stopped() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        turn_begins(&mut played, 3, "turn-1").await;
        host_asks(&mut played, 4, "toolu_1", serde_json::json!({})).await;
        let withdrawn = |seq: u64, tool_use_id: &str| serde_json::json!({"type": "withdrawn", "seq": seq, "tool_use_id": tool_use_id});
        let (stopped, _) =
            tokio::join!(stop_as_app(&rig.daemon, &uid, "stop-1", "turn-1"), async {
                let (uuid, _) = host_reads_stop(&mut played).await;
                say(&mut played.write, withdrawn(5, "toolu_1")).await;
                assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 5 }));
                host_ends_turn(&mut played, 6, &uuid, "aborted_tools", &[]).await;
            });
        assert!(
            matches!(stopped, protocol::ws::InterruptResult::Aborted { .. }),
            "{stopped:?}"
        );
        turn_begins(&mut played, 8, "turn-2").await;
        host_asks(&mut played, 9, "toolu_2", serde_json::json!({})).await;
        say(&mut played.write, withdrawn(10, "toolu_2")).await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 10 }));
        let detail = |request_id: &str| {
            stored(&rig, &uid)
                .into_iter()
                .find(|event| event["source_event_id"] == format!("resolved:{request_id}"))
                .map(|event| event["payload"]["detail"].clone())
        };
        assert_eq!(
            detail("toolu_1"),
            Some(serde_json::json!(
                "The turn was stopped before anyone answered."
            ))
        );
        assert_eq!(
            detail("toolu_2"),
            Some(serde_json::json!(
                "the session ended before anyone answered"
            ))
        );
        assert_eq!(cards(&rig.daemon, &uid).await, (vec![], vec![]));
        stop(&played.dir);
    }

    /// **Stop on a Claude run, as a minor-37 daemon sends it, emitted by this build.** A copy
    /// of it under `ios/` is kept byte-identical by `fixtures/check-duplicated-fixtures.sh`.
    #[tokio::test]
    async fn the_minor_37_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/claude/minor-37-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_minor_37_wire().await,
            committed,
            "the committed minor-37 wire contract no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_minor_37_wire_fixture, then copy it over any copy under ios/ \
             (fixtures/check-duplicated-fixtures.sh names them)"
        );
    }

    /// Rewrite `fixtures/claude/minor-37-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_minor_37_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_minor_37_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/minor-37-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// The run `uid`'s `events`, as the wire delivers them, with what differs between runs
    /// pinned, each checked first: the run's uid, which is also its name (to `pinned`), each
    /// event's clock read (to `ts`), and the start's uuid, which names the person's first
    /// words in the daemon's own source id (`say:<start_uuid>`).
    fn pinned_events(
        events: &[protocol::event::Event],
        uid: &str,
        pinned: &str,
        ts: &str,
        start_uuid: &str,
    ) -> serde_json::Value {
        let first_words = format!("say:{start_uuid}");
        let mut wire = serde_json::to_value(events).unwrap();
        for event in wire.as_array_mut().unwrap() {
            assert_eq!(event["session_uid"], uid);
            assert_eq!(event["session_id"], uid);
            assert!(
                protocol::time::unix_ms_from_rfc3339(event["ts"].as_str().unwrap()).is_some(),
                "ts is a clock read: {event}"
            );
            event["session_uid"] = pinned.into();
            event["session_id"] = pinned.into();
            event["ts"] = ts.into();
            if event["source"] == "daemon" && event["kind"] != "link_state" {
                assert_eq!(event["source_event_id"], first_words.as_str(), "{event}");
                event["source_event_id"] = "say:6f1c2a5e-8b3d-4e7f-9a10-2c4d6e8f0a1b".into();
            }
        }
        wire
    }

    /// `summary` as an app reads it in the fleet, with what differs between runs pinned,
    /// each checked first: the run's uid (`uid` to `pinned`), and its name when that is the
    /// uid; the rig's folder (`root`) in `cwd`, and the label made from it; and the clock reads
    /// (the two times, and the model fact's). Nothing else of the rig or the run's uid is left
    /// in it.
    fn pinned_summary(
        summary: &protocol::event::SessionSummary,
        uid: &str,
        pinned: &str,
        root: &Path,
    ) -> serde_json::Value {
        let mut wire = serde_json::to_value(summary).unwrap();
        assert_eq!(wire["session_uid"], uid);
        wire["session_uid"] = pinned.into();
        if wire["session_id"] == uid {
            wire["session_id"] = pinned.into();
        }
        let root = root.to_str().unwrap();
        if wire["cwd"] == root {
            let label = Path::new(root).file_name().unwrap().to_str().unwrap();
            assert_eq!(wire["project_label"], label);
            wire["cwd"] = "/tmp/cc-rig".into();
            wire["project_label"] = "cc-rig".into();
        }
        for key in ["created_at", "updated_at"] {
            assert!(
                protocol::time::unix_ms_from_rfc3339(wire[key].as_str().unwrap()).is_some(),
                "{key} is a clock read: {wire}"
            );
            wire[key] = "2026-11-15T09:00:00.000Z".into();
        }
        if let Some(at) = wire.get_mut("model").and_then(|model| model.get_mut("at")) {
            assert!(
                protocol::time::unix_ms_from_rfc3339(at.as_str().unwrap()).is_some(),
                "the model's at is a clock read: {at}"
            );
            *at = "2026-11-15T09:00:00.000Z".into();
        }
        let text = wire.to_string();
        assert!(!text.contains(root) && !text.contains(uid), "{text}");
        wire
    }

    /// The capabilities a paired phone is offered, with `claude_interrupt`; the Interrupt an
    /// app sends for a Claude run's turn; each answer ccd gives a hosted run's Stop, from a
    /// run on a stand-in host (aborted, a second Stop while one waits, ended on its own, not
    /// the turn the host runs, no connection); the person's row of a message, with its
    /// request id; and the notice for that message, which the Stop cancelled. And the
    /// fleet's session summaries an app decides from: the hosted run live, with the turn the
    /// Interrupt names running, and its events then; a tmux Claude run on the same daemon;
    /// and the hosted run once its host is gone and the run has ended.
    async fn derived_minor_37_wire() -> serde_json::Value {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let fixed = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
        let answer = |request_id: &str, result: protocol::ws::InterruptResult| {
            serde_json::to_value(protocol::ws::ServerMessage::InterruptResult {
                session_id: fixed.into(),
                request_id: request_id.into(),
                result,
            })
            .unwrap()
        };
        let turns = [
            "9d1f0000-0000-4000-8000-000000000011",
            "9d1f0000-0000-4000-8000-000000000012",
            "9d1f0000-0000-4000-8000-000000000013",
        ];
        turn_begins(&mut played, 3, turns[0]).await;
        let hosted_live = pinned_summary(&summary(&rig.daemon, &uid).await, &uid, fixed, &rig.root);
        let hosted_live_events = pinned_events(
            &rig.daemon
                .db
                .events_after(uid.clone(), 0, 100)
                .await
                .unwrap(),
            &uid,
            fixed,
            "2026-11-15T09:00:00.000Z",
            &played.start_uuid(),
        );
        let (_, _) = tokio::join!(
            send_as_app(&rig.daemon, &uid, "send-1", "and the docs"),
            host_takes(&mut played, 4, "done")
        );
        let cancelled = [send_uuid(&uid, "send-1")];
        let (aborted, waiting) =
            tokio::join!(stop_as_app(&rig.daemon, &uid, "stop-1", turns[0]), async {
                let (uuid, _) = host_reads_stop(&mut played).await;
                let waiting = stop_as_app(&rig.daemon, &uid, "stop-2", turns[0]).await;
                host_ends_turn(&mut played, 5, &uuid, "aborted_tools", &cancelled).await;
                waiting
            });
        turn_begins(&mut played, 7, turns[1]).await;
        let (ended, _) = tokio::join!(stop_as_app(&rig.daemon, &uid, "stop-3", turns[1]), async {
            let (uuid, _) = host_reads_stop(&mut played).await;
            host_ends_turn(&mut played, 8, &uuid, "completed", &[]).await;
        });
        turn_begins(&mut played, 10, turns[2]).await;
        let (not_running, _) =
            tokio::join!(stop_as_app(&rig.daemon, &uid, "stop-4", turns[2]), async {
                let (uuid, _) = host_reads_stop(&mut played).await;
                host_refuses_stop(&mut played, 11, &uuid).await;
            });
        let dir = played.dir.clone();
        let events = stored(&rig, &uid);
        drop(played);
        until_detached(&rig.daemon, &uid).await;
        let no_connection = stop_as_app(&rig.daemon, &uid, "stop-5", turns[2]).await;
        let host = pid_in(&dir, "stub.pid");
        stop(&dir);
        for _ in 0..100 {
            // SAFETY: kill(2) with signal 0 only asks whether the pid is in use.
            if unsafe { libc::kill(host, 0) } != 0 {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        rig.daemon.watch_hosted_runs().await;
        until_ended(&rig, &uid, "exited", None).await;
        let hosted_ended =
            pinned_summary(&summary(&rig.daemon, &uid).await, &uid, fixed, &rig.root);
        let (tmux, _supervisor) = register_tmux_run(&rig).await;
        let tmux_live = pinned_summary(
            &summary(&rig.daemon, &tmux).await,
            &tmux,
            "01K1B3XQ8ZC0DE5FGH7JKMNTMX",
            &rig.root,
        );
        let payload = |find: &dyn Fn(&serde_json::Value) -> bool| {
            events
                .iter()
                .find(|event| find(event))
                .map(|event| event["payload"].clone())
                .expect("the event")
        };
        serde_json::json!({
            "hello_ack_capabilities": crate::ws_server::capabilities(&rig.daemon, false, false, true),
            "interrupt_request": serde_json::to_value(protocol::ws::ClientMessage::Interrupt {
                session_id: fixed.into(),
                request_id: "stop-1".into(),
                turn_id: turns[0].into(),
                payload_hash: protocol::hash::interrupt_hash(fixed, turns[0]),
            })
            .unwrap(),
            "interrupt_aborted": answer("stop-1", aborted),
            "interrupt_already_waiting": answer("stop-2", waiting),
            "interrupt_ended_on_its_own": answer("stop-3", ended),
            "interrupt_not_running": answer("stop-4", not_running),
            "interrupt_no_connection": answer("stop-5", no_connection),
            "person_row": payload(&|event| event["kind"] == "user_message"
                && event["payload"]["text"] == "and the docs"),
            "send_not_sent": payload(&|event| event["payload"]["notification_type"] == "send_not_sent"),
            "session_hosted_live": hosted_live,
            "session_hosted_live_events": hosted_live_events,
            "session_tmux_live": tmux_live,
            "session_hosted_ended": hosted_ended,
        })
    }

    /// The commands of a `commands_changed` as claude gives them: a refused screen's
    /// (`clear`, `model`, `effort`), a screen-only one (`doctor`), and two it offers
    /// (`usage` with no argument hint, `compact` with one).
    fn commands_changed() -> serde_json::Value {
        serde_json::json!({"type": "system", "subtype": "commands_changed",
        "uuid": "9d1f0000-0000-4000-8000-000000000021", "session_id": "5f0c1e2a-0000-4000-8000-000000000001",
        "commands": [
            {"name": "clear", "description": "Clear conversation history and free up context", "argumentHint": "", "builtin": true, "aliases": ["reset", "new"]},
            {"name": "model", "description": "Set the AI model for Claude Code", "argumentHint": "[model]", "builtin": true},
            {"name": "effort", "description": "Set effort level for model usage", "argumentHint": "<low|medium|high|xhigh|max>", "builtin": true},
            {"name": "doctor", "description": "Diagnose and verify your Claude Code installation and settings", "argumentHint": "", "builtin": true},
            {"name": "usage", "description": "Show plan usage limits", "argumentHint": "", "builtin": true, "aliases": ["cost", "stats"]},
            {"name": "compact", "description": "Clear conversation history but keep a summary in context", "argumentHint": "<optional custom summarization instructions>", "builtin": true}
        ]})
    }

    /// A turn's init, as claude's carries the commands it keeps to its own screen.
    async fn init_with_screen_only(played: &mut Played, seq: u64, turn: &str) {
        sdk(
            &mut played.write,
            seq,
            serde_json::json!({"type": "system", "subtype": "init", "uuid": turn,
                               "session_id": "5f0c1e2a-0000-4000-8000-000000000001",
                               "terminal_slash_commands": ["doctor", "send-to-project"]}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
    }

    /// The catalog's names, with each entry, when it is available.
    fn offered(result: &CommandCatalogResult) -> Vec<CommandEntry> {
        match result {
            CommandCatalogResult::Available {
                commands, entries, ..
            } => {
                assert_eq!(
                    commands,
                    &entries
                        .iter()
                        .map(|entry| entry.name.clone())
                        .collect::<Vec<_>>()
                );
                entries.clone()
            }
            other => panic!("not available: {other:?}"),
        }
    }

    /// **B8 G2: a hosted run's menu is what its claude last reported.** `unavailable`
    /// until the first init; then the answer's list, then each `commands_changed`'s,
    /// without the init's screen-only commands and without [`REFUSED_SCREENS`], each
    /// with claude's description and argument hint, the answer's claude version and when
    /// the list was taken; a restarted ccd answers the same. And the other way: every
    /// command claude lists is either offered or one ccd refuses from its app screen (or
    /// claude's screen-only), never both, from the one list.
    #[tokio::test]
    async fn a_hosted_runs_menu_is_what_its_claude_last_reported() {
        let rig = rig();
        let before = protocol::time::now_rfc3339();
        let mut played = live_run(&rig).await;
        let after = protocol::time::now_rfc3339();
        let uid = played.uid.clone();
        assert_eq!(
            rig.daemon.command_catalog(&uid).await,
            CommandCatalogResult::Unavailable {
                reason: "Claude hasn't listed its commands for this session yet.".into()
            }
        );
        init_with_screen_only(&mut played, 3, "9d1f0000-0000-4000-8000-000000000020").await;
        let first = rig.daemon.command_catalog(&uid).await;
        assert_eq!(
            offered(&first),
            [CommandEntry {
                name: "compact".into(),
                description: String::new(),
                argument_hint: None
            }]
        );
        let CommandCatalogResult::Available {
            claude_version,
            probed_at,
            ..
        } = &first
        else {
            unreachable!()
        };
        assert_eq!(claude_version.as_deref(), Some("2.1.322"));
        assert!(
            before <= *probed_at && *probed_at <= after,
            "{before} {probed_at} {after}"
        );
        sdk(&mut played.write, 4, commands_changed()).await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 4 }));
        let changed = rig.daemon.command_catalog(&uid).await;
        let expected = [
            CommandEntry {
                name: "usage".into(),
                description: "Show plan usage limits".into(),
                argument_hint: None,
            },
            CommandEntry {
                name: "compact".into(),
                description: "Clear conversation history but keep a summary in context".into(),
                argument_hint: Some("<optional custom summarization instructions>".into()),
            },
        ];
        assert_eq!(offered(&changed), expected);
        let names: Vec<String> = offered(&changed)
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        for command in commands_changed()["commands"].as_array().unwrap() {
            let name = command["name"].as_str().unwrap();
            let refused = refused_screen(&format!("/{name}"), true).is_some();
            let screen_only = name == "doctor";
            assert_eq!(
                names.iter().any(|offered| offered == name),
                !refused && !screen_only,
                "{name}"
            );
        }
        for (name, reason) in REFUSED_SCREENS {
            assert_eq!(refused_screen(&format!("/{name} x"), true), Some(reason));
            assert_eq!(refused_screen(&format!("/{name}"), false), Some(reason));
            assert_eq!(refused_screen(&format!("/{name} x"), false), None, "typed");
        }
        stop(&played.dir);
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        assert_eq!(restarted.command_catalog(&uid).await, changed);
    }

    #[tokio::test]
    async fn the_minor_38_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/claude/minor-38-wire.json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(FIXTURE).unwrap(),
            derived_minor_38_wire().await,
            "the committed minor 38 wire fixture no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_minor_38_wire_fixture, then copy it over any copy under ios/ \
             or the Kit"
        );
    }

    /// Rewrite `fixtures/claude/minor-38-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_minor_38_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_minor_38_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/minor-38-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// The capabilities a paired phone is offered; the catalog request an app sends for a
    /// hosted run; the answers before the run's first init and after a `commands_changed`,
    /// from a run on a stand-in host (`probed_at` is when the list was taken, so it is fixed
    /// here); the run's session summary as the fleet gives it once its menu is there, and
    /// again with ccd's connection to its host down, the host still running, with the run's
    /// events as the wire delivers them, its turn still running (claude's init, no result);
    /// a command's
    /// output from each of the SDK's three ways, as the run's events file it; and the
    /// `/status` refusal.
    async fn derived_minor_38_wire() -> serde_json::Value {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let fixed = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
        let answer = |result: CommandCatalogResult| {
            let mut frame = serde_json::to_value(protocol::ws::ServerMessage::CommandCatalog {
                session_id: fixed.into(),
                result,
            })
            .unwrap();
            if frame["result"]["status"] == "available" {
                frame["result"]["probed_at"] = "2026-11-15T09:00:00Z".into();
            }
            frame
        };
        let before_init = rig.daemon.command_catalog(&uid).await;
        init_with_screen_only(&mut played, 3, "9d1f0000-0000-4000-8000-000000000020").await;
        sdk(&mut played.write, 4, commands_changed()).await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 4 }));
        let available = rig.daemon.command_catalog(&uid).await;
        let hosted_live = pinned_summary(&summary(&rig.daemon, &uid).await, &uid, fixed, &rig.root);
        let outputs = [
            serde_json::json!({"type": "user", "uuid": "9d1f0000-0000-4000-8000-000000000031", "isReplay": true,
                "session_id": "5f0c1e2a-0000-4000-8000-000000000001",
                "message": {"role": "user", "content": "<local-command-stdout>Compacted </local-command-stdout>"}}),
            serde_json::json!({"type": "assistant", "uuid": "9d1f0000-0000-4000-8000-000000000032",
                "session_id": "5f0c1e2a-0000-4000-8000-000000000001",
                "message": {"model": "<synthetic>", "role": "assistant", "content": [{"type": "text", "text": "Set effort level to low (this session only): Quick, straightforward implementation with minimal overhead"}]},
                "local_command_run": {"command": "effort", "args": "low"},
                "local_command_source": "<local-command-stdout>Set effort level to low (this session only): Quick, straightforward implementation with minimal overhead</local-command-stdout>"}),
            serde_json::json!({"type": "system", "subtype": "local_command_output", "uuid": "9d1f0000-0000-4000-8000-000000000033",
                "session_id": "5f0c1e2a-0000-4000-8000-000000000001",
                "content": "Current session: 3% used"}),
        ];
        for (seq, message) in (5..).zip(outputs) {
            sdk(&mut played.write, seq, message).await;
            assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
        }
        let status = send_as_app(&rig.daemon, &uid, "status-1", "/status").await;
        let events = stored(&rig, &uid);
        let dir = played.dir.clone();
        let start_uuid = played.start_uuid();
        drop(played);
        until_detached(&rig.daemon, &uid).await;
        let hosted_detached =
            pinned_summary(&summary(&rig.daemon, &uid).await, &uid, fixed, &rig.root);
        assert!(rig.daemon.db.turn_running(uid.clone()).await.unwrap());
        let hosted_detached_events = pinned_events(
            &rig.daemon
                .db
                .events_after(uid.clone(), 0, 100)
                .await
                .unwrap(),
            &uid,
            fixed,
            "2026-11-15T09:00:00.000Z",
            &start_uuid,
        );
        stop(&dir);
        let row = |item: &str| {
            events
                .iter()
                .find(|event| event["item_id"] == item)
                .map(
                    |event| serde_json::json!({"kind": event["kind"], "payload": event["payload"]}),
                )
                .expect("the event")
        };
        serde_json::json!({
            "hello_ack_capabilities": crate::ws_server::capabilities(&rig.daemon, false, false, true),
            "session_hosted_live": hosted_live,
            "session_hosted_detached": hosted_detached,
            "session_hosted_detached_events": hosted_detached_events,
            "catalog_request": serde_json::to_value(protocol::ws::ClientMessage::GetCommandCatalog {
                session_id: fixed.into(),
            })
            .unwrap(),
            "catalog_before_first_init": answer(before_init),
            "catalog": answer(available),
            "output_replayed": row("9d1f0000-0000-4000-8000-000000000031"),
            "output_effort": row("9d1f0000-0000-4000-8000-000000000032"),
            "output_local_command": row("9d1f0000-0000-4000-8000-000000000033"),
            "status_refused": serde_json::to_value(protocol::ws::ServerMessage::SendTextResult {
                session_id: fixed.into(),
                result: status,
            })
            .unwrap(),
        })
    }

    /// A tmux Claude run `cc-1` on the rig's daemon, registered as its supervisor registers
    /// it: its uid, and the supervisor's end of its channel, held to keep it attached.
    async fn register_tmux_run(
        rig: &Rig,
    ) -> (
        String,
        tokio::sync::mpsc::Receiver<protocol::ipc::DaemonFrame>,
    ) {
        let (tx, supervisor) = tokio::sync::mpsc::channel(Config::default().ipc_write_queue);
        let uid = rig
            .daemon
            .register_supervisor(
                protocol::ipc::RegisterSession {
                    session_id: "cc-1".into(),
                    session_uid: None,
                    tmux_session: "cc-1".into(),
                    tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
                    cwd: "/tmp".into(),
                    supervisor_pid: 4242,
                    claude_bin: None,
                    agent: AgentKind::Claude,
                    agent_bin: None,
                    codex_thread_id: None,
                    codex_socket: None,
                    codex_generation: None,
                    started_at: protocol::time::now_rfc3339(),
                    protocol_minor: protocol::PROTOCOL_MINOR,
                    exit_replay: false,
                },
                tx,
                Arc::new(std::sync::Mutex::new(HashMap::new())),
            )
            .await
            .unwrap()
            .session
            .uid;
        (uid, supervisor)
    }

    /// The host's side of one settings line: it reads it, then answers `status` (with
    /// `detail`) as message `seq` and reads ccd's ack. The line, its uuid cleared.
    async fn host_answers_setting(
        played: &mut Played,
        seq: u64,
        status: &str,
        detail: Option<&str>,
    ) -> ToHost {
        let line = next(&mut played.lines)
            .await
            .expect("a settings line reached the host");
        let (uuid, cleared) = match line {
            ToHost::SetModel {
                uuid,
                model,
                effort,
            } => (
                uuid,
                ToHost::SetModel {
                    uuid: String::new(),
                    model,
                    effort,
                },
            ),
            ToHost::SetMode { uuid, mode } => (
                uuid,
                ToHost::SetMode {
                    uuid: String::new(),
                    mode,
                },
            ),
            other => panic!("not a settings line: {other:?}"),
        };
        let mut outcome =
            serde_json::json!({"type": "outcome", "seq": seq, "uuid": uuid, "status": status});
        if let Some(detail) = detail {
            outcome["detail"] = detail.into();
        }
        say(&mut played.write, outcome).await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
        cleared
    }

    fn set_model(model: Option<&str>, effort: Option<&str>) -> Setting {
        Setting::Model {
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
        }
    }

    fn model_line(model: Option<&str>, effort: Option<&str>) -> ToHost {
        ToHost::SetModel {
            uuid: String::new(),
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
        }
    }

    fn refusal(result: SettingResult) -> String {
        match result {
            SettingResult::Refused { reason } => reason,
            other => panic!("not refused: {other:?}"),
        }
    }

    /// **B8 part 2 S2: a setting is checked before anything reaches the host.** A change
    /// naming nothing, a model or an effort the run's menus do not list, and a mode not among
    /// the three, `bypassPermissions` above all, are refused with nothing sent; so is any
    /// change to a run not running, and to a Claude run in a terminal.
    #[tokio::test]
    async fn a_setting_is_checked_before_anything_reaches_the_host() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        for (setting, reason) in [
            (
                set_model(None, None),
                "Choose a model or an effort; nothing was changed.",
            ),
            (
                set_model(Some("gpt-9"), None),
                "Claude doesn't offer that model in this session; nothing was changed.",
            ),
            (
                set_model(Some("sonnet"), Some("turbo")),
                "Claude doesn't offer that effort in this session; nothing was changed.",
            ),
            (
                Setting::Mode("bypassPermissions".into()),
                "That permission mode isn't offered in this session; nothing was changed.",
            ),
            (
                Setting::Mode("dontAsk".into()),
                "That permission mode isn't offered in this session; nothing was changed.",
            ),
        ] {
            assert_eq!(refusal(rig.daemon.set_setting(&uid, setting).await), reason);
        }
        assert_eq!(next(&mut played.lines).await, None, "nothing sent");
        force_lifecycle(&rig, &uid, "stopping");
        assert_eq!(
            refusal(
                rig.daemon
                    .set_setting(&uid, Setting::Mode("plan".into()))
                    .await
            ),
            "This session isn't running, so nothing was changed."
        );
        let (tmux, _supervisor) = register_tmux_run(&rig).await;
        assert_eq!(
            refusal(
                rig.daemon
                    .set_setting(&tmux, Setting::Mode("plan".into()))
                    .await
            ),
            NOT_FROM_THE_APP
        );
        stop(&played.dir);
    }

    /// **B8 part 2 S3, S4: a setting reaches the host one part per line, and is answered as
    /// claude answered.** A model while idle applies now; a mode while a turn runs, from the
    /// next turn. A model taken and an effort refused reads refused, saying so, and files
    /// nothing. An effort taken is the run's effort, filed as a `model_set` notice. Then the
    /// person types `/effort low`: claude's recorded receipt for it is the run's effort, and
    /// still after a restart.
    #[tokio::test]
    async fn a_setting_reaches_the_host_part_by_part_and_is_answered_as_claude_answered() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let (idle, line) = tokio::join!(
            rig.daemon
                .set_setting(&uid, set_model(Some("sonnet"), None)),
            host_answers_setting(&mut played, 3, "done", None)
        );
        assert_eq!(line, model_line(Some("sonnet"), None));
        assert_eq!(
            idle,
            SettingResult::Applied {
                applies: Applies::Now
            }
        );
        turn_begins(&mut played, 4, "9d1f0000-0000-4000-8000-000000000041").await;
        let (running, line) = tokio::join!(
            rig.daemon.set_setting(&uid, Setting::Mode("plan".into())),
            host_answers_setting(&mut played, 5, "done", None)
        );
        assert_eq!(
            line,
            ToHost::SetMode {
                uuid: String::new(),
                mode: "plan".into()
            }
        );
        assert_eq!(
            running,
            SettingResult::Applied {
                applies: Applies::NextTurn
            }
        );
        let (partial, lines) = tokio::join!(
            rig.daemon
                .set_setting(&uid, set_model(Some("default"), Some("low"))),
            async {
                let first = host_answers_setting(&mut played, 6, "done", None).await;
                let second =
                    host_answers_setting(&mut played, 7, "refused", Some("Invalid effort level"))
                        .await;
                (first, second)
            }
        );
        assert_eq!(
            lines,
            (
                model_line(Some("default"), None),
                model_line(None, Some("low"))
            )
        );
        assert_eq!(
            refusal(partial),
            "The model was changed; the effort was not: Invalid effort level"
        );
        assert_eq!(summary(&rig.daemon, &uid).await.effort, None);
        let (effort, line) = tokio::join!(
            rig.daemon.set_setting(&uid, set_model(None, Some("high"))),
            host_answers_setting(&mut played, 8, "done", None)
        );
        assert_eq!(line, model_line(None, Some("high")));
        assert_eq!(
            effort,
            SettingResult::Applied {
                applies: Applies::NextTurn
            }
        );
        let fact = summary(&rig.daemon, &uid)
            .await
            .effort
            .expect("the run's effort");
        assert_eq!(fact.value, "high");
        let notices: Vec<serde_json::Value> = stored(&rig, &uid)
            .into_iter()
            .filter(|event| event["payload"]["notification_type"] == "model_set")
            .map(|event| event["payload"].clone())
            .collect();
        assert_eq!(
            notices,
            [
                serde_json::json!({"notification_type": "model_set", "effort": "high", "message": "Effort set to high"})
            ]
        );
        sdk(&mut played.write, 9, capture()[130].clone()).await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 9 }));
        let typed = summary(&rig.daemon, &uid)
            .await
            .effort
            .expect("the run's effort");
        assert_eq!(typed.value, "low");
        assert!(typed.seq > fact.seq);
        stop(&played.dir);
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        assert_eq!(summary(&restarted, &uid).await.effort, Some(typed));
    }

    /// **B8 part 2 S3: an answer that does not come in time, and a link that is down.** No
    /// outcome within the wait reads indeterminate, and the outcome that then comes still
    /// files the effort it set. A run whose link is down sends nothing and reads refused.
    #[tokio::test]
    async fn a_late_answer_reads_indeterminate_and_still_files_and_a_down_link_refuses() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let (late, uuid) = tokio::join!(
            rig.daemon.set_setting(&uid, set_model(None, Some("low"))),
            async {
                let Some(ToHost::SetModel { uuid, .. }) = next(&mut played.lines).await else {
                    panic!("no settings line reached the host");
                };
                uuid
            }
        );
        assert!(
            matches!(late, SettingResult::Indeterminate { .. }),
            "{late:?}"
        );
        assert_eq!(summary(&rig.daemon, &uid).await.effort, None);
        say(
            &mut played.write,
            serde_json::json!({"type": "outcome", "seq": 3, "uuid": uuid, "status": "done"}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 3 }));
        assert_eq!(
            summary(&rig.daemon, &uid)
                .await
                .effort
                .map(|fact| fact.value),
            Some("low".to_string())
        );
        stop(&played.dir);
        let down = self::rig();
        let (dir, detached) =
            detached_run(&down, &stub_host(&down, HOST_PROTOCOL_VERSION, None)).await;
        assert_eq!(
            refusal(
                down.daemon
                    .set_setting(&detached, Setting::Mode("plan".into()))
                    .await
            ),
            "The computer lost its connection to this session, so nothing was changed."
        );
        stop(&dir);
    }

    /// **B8 part 2 S5: a hosted run's menus are its start's answer's.** Each model's value,
    /// name, description and efforts, and the three modes with their names, never
    /// `bypassPermissions`; a tmux run has none.
    #[tokio::test]
    async fn a_hosted_runs_menus_are_its_starts_answers() {
        let rig = rig();
        let played = live_run(&rig).await;
        let efforts = ["low", "medium", "high", "xhigh", "max"]
            .map(str::to_string)
            .to_vec();
        assert_eq!(
            summary(&rig.daemon, &played.uid).await.menus,
            Some(RunMenus {
                models: vec![
                    MenuModel {
                        value: "default".into(),
                        label: "Default (recommended)".into(),
                        detail: Some(
                            "Use the default model (currently Opus 5.5) · $4/$20 per Mtok".into()
                        ),
                        efforts: efforts.clone(),
                    },
                    MenuModel {
                        value: "sonnet".into(),
                        label: "Sonnet".into(),
                        detail: Some(
                            "Sonnet 5.5 · Efficient for routine tasks · $2/$10 per Mtok".into()
                        ),
                        efforts,
                    },
                ],
                modes: [
                    ("default", "Ask before edits"),
                    ("acceptEdits", "Accept edits"),
                    ("plan", "Plan"),
                ]
                .map(|(value, label)| MenuMode {
                    value: value.into(),
                    label: label.into(),
                })
                .to_vec(),
            })
        );
        let (tmux, _supervisor) = register_tmux_run(&rig).await;
        assert_eq!(summary(&rig.daemon, &tmux).await.menus, None);
        stop(&played.dir);
    }

    #[tokio::test]
    async fn the_minor_39_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/claude/minor-39-wire.json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(FIXTURE).unwrap(),
            derived_minor_39_wire().await,
            "the committed minor 39 wire fixture no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_minor_39_wire_fixture, then copy it over any copy under ios/ \
             or the Kit"
        );
    }

    /// Rewrite `fixtures/claude/minor-39-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_minor_39_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_minor_39_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/minor-39-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// The frames of a hosted run's settings and each answer ccd gives, from a run on a
    /// stand-in host: a model applied while idle, a mode while a turn runs, an effort, a mode
    /// refused (`bypassPermissions`), a model taken with its effort refused, and a change
    /// whose link ended before its answer; the effort's `model_set` notice; the capabilities
    /// a paired phone is offered; and the fleet's summaries an app decides from, pinned as
    /// minor 37's are: the run still starting (no menus yet), the run with its menus, the
    /// effort set and claude's echoes of the model and the mode filed, that run as an older
    /// daemon sends it (minor 38's summary has neither `effort` nor `menus`), and a tmux
    /// Claude run (none).
    async fn derived_minor_39_wire() -> serde_json::Value {
        let rig = rig();
        let fixed = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let daemon = Arc::clone(&rig.daemon);
        let (started, (mut played, starting)) =
            tokio::join!(start(&daemon, run(&rig, "req-live", &cwd, &host)), async {
                let mut played = play_to_start(&rig).await;
                let starting = summary(&rig.daemon, &played.uid).await;
                let uuid = played.start_uuid();
                say(
                    &mut played.write,
                    serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                       "status": "done", "initialize": initialize_answer()}),
                )
                .await;
                assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 2 }));
                (played, starting)
            });
        let uid = played.uid.clone();
        assert_eq!(started_uid(&started), uid);
        let starting = pinned_summary(&starting, &uid, fixed, &rig.root);
        let (now, _) = tokio::join!(
            rig.daemon
                .set_setting(&uid, set_model(Some("sonnet"), None)),
            host_answers_setting(&mut played, 3, "done", None)
        );
        // claude's own echoes of a change: its receipt of the app's `sonnet`, as claude's
        // transcript of a real run kept it (B8 part 2's S7, claude 2.1.322), replayed as the
        // recorded session's line 108 replays a receipt; and its status of a mode (line 29).
        sdk(
            &mut played.write,
            4,
            serde_json::json!({"type": "user", "isReplay": true, "uuid": "9d1f0000-0000-4000-8000-000000000061",
                "session_id": "5f0c1e2a-0000-4000-8000-000000000001",
                "message": {"role": "user", "content": "<local-command-stdout>Set model to `sonnet (claude-sonnet-5-5)`</local-command-stdout>"}}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 4 }));
        turn_begins(&mut played, 5, "9d1f0000-0000-4000-8000-000000000051").await;
        let (next_turn, _) = tokio::join!(
            rig.daemon.set_setting(&uid, Setting::Mode("plan".into())),
            host_answers_setting(&mut played, 6, "done", None)
        );
        sdk(
            &mut played.write,
            7,
            serde_json::json!({"type": "system", "subtype": "status", "permissionMode": "plan",
                "uuid": "9d1f0000-0000-4000-8000-000000000062",
                "session_id": "5f0c1e2a-0000-4000-8000-000000000001"}),
        )
        .await;
        assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 7 }));
        let (effort, _) = tokio::join!(
            rig.daemon.set_setting(&uid, set_model(None, Some("high"))),
            host_answers_setting(&mut played, 8, "done", None)
        );
        let bypass = rig
            .daemon
            .set_setting(&uid, Setting::Mode("bypassPermissions".into()))
            .await;
        let (partial, _) = tokio::join!(
            rig.daemon
                .set_setting(&uid, set_model(Some("default"), Some("low"))),
            async {
                host_answers_setting(&mut played, 9, "done", None).await;
                host_answers_setting(&mut played, 10, "refused", Some("Invalid effort level"))
                    .await;
            }
        );
        let hosted = pinned_summary(&summary(&rig.daemon, &uid).await, &uid, fixed, &rig.root);
        let mut older = hosted.clone();
        for field in ["effort", "menus"] {
            assert!(
                older.as_object_mut().unwrap().remove(field).is_some(),
                "{field}"
            );
        }
        let notice = stored(&rig, &uid)
            .into_iter()
            .find(|event| event["payload"]["notification_type"] == "model_set")
            .map(|event| event["payload"].clone())
            .expect("the effort's notice");
        let (tmux, _supervisor) = register_tmux_run(&rig).await;
        let tmux_live = pinned_summary(
            &summary(&rig.daemon, &tmux).await,
            &tmux,
            "01K1B3XQ8ZC0DE5FGH7JKMNTMX",
            &rig.root,
        );
        let dir = played.dir.clone();
        let (lost, _) = tokio::join!(
            rig.daemon
                .set_setting(&uid, Setting::Mode("acceptEdits".into())),
            async move {
                let mut played = played;
                assert!(matches!(
                    next(&mut played.lines).await,
                    Some(ToHost::SetMode { .. })
                ));
                drop(played);
            }
        );
        stop(&dir);
        let answer = |request_id: &str, result: SettingResult| {
            serde_json::to_value(protocol::ws::ServerMessage::SettingResult {
                session_id: fixed.into(),
                request_id: request_id.into(),
                result,
            })
            .unwrap()
        };
        let request = |message: protocol::ws::ClientMessage| serde_json::to_value(message).unwrap();
        serde_json::json!({
            "hello_ack_capabilities": crate::ws_server::capabilities(&rig.daemon, false, false, true),
            "set_model_request": request(protocol::ws::ClientMessage::SetModel {
                session_id: fixed.into(),
                request_id: "model-1".into(),
                model: Some("sonnet".into()),
                effort: None,
            }),
            "set_effort_request": request(protocol::ws::ClientMessage::SetModel {
                session_id: fixed.into(),
                request_id: "effort-1".into(),
                model: None,
                effort: Some("high".into()),
            }),
            "set_mode_request": request(protocol::ws::ClientMessage::SetMode {
                session_id: fixed.into(),
                request_id: "mode-1".into(),
                mode: "plan".into(),
            }),
            "applied_now": answer("model-1", now),
            "applied_next_turn": answer("mode-1", next_turn),
            "applied_effort": answer("effort-1", effort),
            "refused_bypass": answer("mode-2", bypass),
            "refused_partial": answer("model-2", partial),
            "indeterminate": answer("mode-3", lost),
            "effort_notice": notice,
            "session_hosted_starting": starting,
            "session_hosted_live": hosted,
            "session_hosted_live_minor_38": older,
            "session_tmux_live": tmux_live,
        })
    }

    /// **B6 G11: a request too large to show** is refused at once: the host is answered
    /// deny with the reason claude reads, no card is raised, and the run goes on.
    #[tokio::test]
    async fn a_request_too_large_to_show_is_refused_and_raises_no_card() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let huge = "x".repeat(600_000);
        say(
            &mut played.write,
            serde_json::json!({"type": "permission", "seq": 3, "request": {
                "tool_use_id": "toolu_big", "tool_name": "Write",
                "input": {"file_path": "/tmp/big", "content": huge}, "options": {}}}),
        )
        .await;
        let mut acked = false;
        let mut refusal = None;
        for _ in 0..2 {
            match next(&mut played.lines).await {
                Some(ToHost::Ack { seq: 3 }) => acked = true,
                Some(ToHost::Answer {
                    tool_use_id,
                    decision,
                    ..
                }) if tool_use_id == "toolu_big" => refusal = Some(decision),
                other => panic!("{other:?}"),
            }
        }
        assert!(acked);
        assert_eq!(
            refusal,
            Some(protocol::host::Decision::Deny {
                message: "This request is too large to show in the app, so it was refused.".into(),
                interrupt: false,
            })
        );
        assert_eq!(cards(&rig.daemon, &uid).await, (vec![], vec![]));
        assert!(!stored(&rig, &uid)
            .iter()
            .any(|event| event["kind"] == "approval_request"));
        host_asks(&mut played, 4, "toolu_next", serde_json::json!({})).await;
        stop(&played.dir);
    }

    /// **B6 G5: cards across a restart.** A card open when ccd stopped is still held for
    /// the apps, and answerable again once its host re-attaches; its "Allow for this
    /// session" sends the rules its own request suggested, read from the stored card, not
    /// from the host's `state`. One whose request came from a subagent still does not end
    /// the main turn when denied. An answer claimed and never sent is released by the
    /// host's `state`, which still holds its request, so the card is answerable.
    #[tokio::test]
    async fn a_card_open_across_a_restart_is_answered_once_after_it() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let dir = played.dir.clone();
        host_asks(
            &mut played,
            3,
            "toolu_open",
            serde_json::json!({"suggestions": [allow_rule()]}),
        )
        .await;
        host_asks(
            &mut played,
            4,
            "toolu_sub",
            serde_json::json!({"agentID": "agent-1"}),
        )
        .await;
        // An answer claimed whose `answer` never reached the host.
        rig.daemon
            .db
            .claim_answer(crate::store::AnswerClaim {
                session_uid: uid.clone(),
                session_id: uid.clone(),
                request_id: "toolu_open".into(),
                payload_hash: bash_hash(),
                decision: serde_json::to_string(&AnswerDecision::Allow).unwrap(),
                started_at: protocol::time::now_rfc3339(),
            })
            .await
            .unwrap();
        drop(played);
        let (restarted, mut lines, mut write) = restarted(&rig, &dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 4 }));
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 5, "pending": [
                bash_request("toolu_open", serde_json::json!({})),
                bash_request("toolu_sub", serde_json::json!({"agentID": "agent-1"}))]}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 5 }));
        // Still held for the apps, which offer an answer only on a held card
        // (DecisionCard.swift): no hold was ended across the restart.
        let mut holds: Vec<_> = summary(&restarted, &uid)
            .await
            .pending
            .into_iter()
            .map(|card| (card.request_id, card.hold))
            .collect();
        holds.sort_by(|a, b| a.0.cmp(&b.0));
        let held = Some(protocol::ws::QuestionHold::Held);
        assert_eq!(
            holds,
            [
                ("toolu_open".to_string(), held),
                ("toolu_sub".to_string(), held)
            ]
        );
        assert!(!stored(&rig, &uid)
            .iter()
            .any(|event| event["kind"] == "question_hold"));
        let (result, (_, decision)) = tokio::join!(
            app_answers(&restarted, &uid, "toolu_open", allow_for_this_session()),
            host_settles(&mut lines, &mut write, 6, "done")
        );
        assert!(matches!(result, AnswerResult::Applied { .. }), "{result:?}");
        assert_eq!(
            decision,
            protocol::host::Decision::Allow {
                updated_input: None,
                updated_permissions: Some(serde_json::json!([for_the_session_only(allow_rule())])),
            }
        );
        let (result, (_, decision)) = tokio::join!(
            app_answers(&restarted, &uid, "toolu_sub", AnswerDecision::Deny),
            host_settles(&mut lines, &mut write, 7, "done")
        );
        assert!(matches!(result, AnswerResult::Applied { .. }), "{result:?}");
        assert!(matches!(
            decision,
            protocol::host::Decision::Deny {
                interrupt: false,
                ..
            }
        ));
        stop(&dir);
    }

    /// **B6 G5: an answer the host took before a restart.** ccd died after sending it and
    /// before the host's outcome came: the app was told it is not confirmed, and the
    /// restarted ccd, holding nothing of it in memory, records it from the host's replayed
    /// outcome and the answer's claim, closes the card, and answers a retry as a duplicate.
    /// The answer is "Allow for this session", and it is recorded as that choice.
    #[tokio::test]
    async fn an_answer_taken_before_a_restart_is_recorded_from_the_replay() {
        let rig = rig();
        let mut played = live_run(&rig).await;
        let uid = played.uid.clone();
        let dir = played.dir.clone();
        host_asks(
            &mut played,
            3,
            "toolu_1",
            serde_json::json!({"suggestions": [allow_rule()]}),
        )
        .await;
        let (lost, uuid) = tokio::join!(
            app_answers(&rig.daemon, &uid, "toolu_1", allow_for_this_session()),
            async {
                let Some(ToHost::Answer { uuid, .. }) = next(&mut played.lines).await else {
                    panic!("no answer");
                };
                drop(played);
                uuid
            }
        );
        assert!(matches!(lost, AnswerResult::Rejected { .. }), "{lost:?}");
        let (restarted, mut lines, mut write) = restarted(&rig, &dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 3 }));
        say(
            &mut write,
            serde_json::json!({"type": "outcome", "seq": 4, "uuid": uuid, "status": "done"}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 4 }));
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 5, "pending": []}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 5 }));
        assert_eq!(cards(&restarted, &uid).await, (vec![], vec![]));
        let Some((_, recorded)) = restarted
            .db
            .get_answer(uid.clone(), "toolu_1".into())
            .await
            .unwrap()
        else {
            panic!("no answer recorded");
        };
        assert_eq!(recorded.decision, allow_for_this_session());
        assert!(!recorded.indeterminate);
        assert!(matches!(
            app_answers(&restarted, &uid, "toolu_1", allow_for_this_session()).await,
            AnswerResult::Duplicate { .. }
        ));
        stop(&dir);
    }

    /// **G4, the warrant.** A host gone is signalled only under the warrant, and its run
    /// ends either way:
    /// - a pid reused: the recorded host's pid is now a live process of another birth,
    ///   leading a group of its own, so nothing is signalled and it and its child live;
    /// - another boot: the recorded group still has a member, its leader gone, but the
    ///   host was recorded in a boot that is not this one, so the member lives.
    #[tokio::test]
    async fn a_dead_hosts_group_is_signalled_only_under_the_warrant() {
        let rig = rig();
        let (dir, uid) = detached_run(&rig, &stub_host(&rig, HOST_PROTOCOL_VERSION, None)).await;
        let boot = protocol::proc_identity::boot_identity().unwrap();
        let (mut reused, reused_child) = leader(&rig.root.join("reused.child"));
        let pid = reused.id() as i32;
        let mut birth = read_birth_identity(pid).unwrap();
        birth.start_usec += 1;
        record_as_host(&rig, &uid, ProcessIdentity { pid, birth }, boot);
        rig.daemon.watch_hosted_runs().await;
        until_ended(&rig, &uid, "exited", None).await;
        let (leader_ran, child_ran) = (running(pid), running(reused_child));
        let _ = reused.kill();
        let _ = reused.wait();
        // SAFETY: kill(2) of the child this test's leader started, by its pid.
        unsafe { libc::kill(reused_child, libc::SIGKILL) };
        assert!(
            leader_ran && child_ran,
            "a group the warrant refused was signalled"
        );
        assert_eq!(kept(&rig, &uid).0, "exited");
        stop(&dir);

        let rig = self::rig();
        let (dir, uid) = detached_run(&rig, &stub_host(&rig, HOST_PROTOCOL_VERSION, None)).await;
        let (mut gone, member) = leader(&rig.root.join("gone.child"));
        let host = ProcessIdentity {
            pid: gone.id() as i32,
            birth: read_birth_identity(gone.id() as i32).unwrap(),
        };
        let _ = gone.kill();
        let _ = gone.wait();
        let elsewhere = BootIdentity {
            boot_sec: boot.boot_sec + 1,
            boot_usec: boot.boot_usec,
        };
        record_as_host(&rig, &uid, host, elsewhere);
        rig.daemon.watch_hosted_runs().await;
        until_ended(&rig, &uid, "exited", None).await;
        let member_ran = running(member);
        // SAFETY: kill(2) of the child this test's leader started, by its pid.
        unsafe { libc::kill(member, libc::SIGKILL) };
        assert!(member_ran, "a group from another boot was signalled");
        assert_eq!(kept(&rig, &uid).0, "exited");
        stop(&dir);
    }

    /// **G4, a dead host not yet reaped.** The recorded host dies while its parent holds
    /// off reaping it: a zombie, whose birth cannot be read, so the warrant refuses it.
    /// The ladder waits; once the host is reaped and its pid free, its group gets SIGTERM
    /// and the member it held is gone, and only then does the run end. macOS only: Linux
    /// reads a zombie's birth, so its warrant passes at once and there is nothing to wait out.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_dead_host_is_waited_out_until_it_is_reaped() {
        let rig = rig();
        let (dir, uid) = detached_run(&rig, &stub_host(&rig, HOST_PROTOCOL_VERSION, None)).await;
        let (mut zombie, member) = leader(&rig.root.join("zombie.child"));
        let host = ProcessIdentity {
            pid: zombie.id() as i32,
            birth: read_birth_identity(zombie.id() as i32).unwrap(),
        };
        record_as_host(
            &rig,
            &uid,
            host,
            protocol::proc_identity::boot_identity().unwrap(),
        );
        let _ = zombie.kill();
        for _ in 0..100 {
            if is_zombie(host.pid) == Some(true) {
                break;
            }
            std::thread::sleep(POLL);
        }
        assert_eq!(is_zombie(host.pid), Some(true), "the host is not a zombie");
        rig.daemon.watch_hosted_runs().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let waited = kept(&rig, &uid).0 == "live" && running(member);
        let _ = zombie.wait();
        until_ended(&rig, &uid, "exited", Some(member)).await;
        let member_ran = running(member);
        if member_ran {
            // SAFETY: kill(2) of the child this test's leader started, by its pid.
            unsafe { libc::kill(member, libc::SIGKILL) };
        }
        assert!(
            waited,
            "the run ended, or its group was signalled, before the host was reaped"
        );
        assert!(!member_ran, "the group outlived its reaped host");
        assert_eq!(kept(&rig, &uid).0, "exited");
        stop(&dir);
    }

    const SESSION_EVENTS: &str = include_str!("../../../fixtures/claude/sdk-session-events.json");

    /// The recorded session through a live run's link, as the wire delivers its events:
    /// the store's own `Event`s, serialized. Only what differs between runs is pinned,
    /// each checked first: the run's uid, which is also its name, each event's clock read
    /// (`ts`), and the start's uuid, which names the person's first words. The link's
    /// `link_state` fact, the daemon's too, carries no id and is kept as filed.
    async fn the_capture_as_the_wire_delivers_it() -> serde_json::Value {
        let rig = rig();
        let mut played = live_run(&rig).await;
        for (index, message) in capture().into_iter().enumerate() {
            let seq = index as u64 + 3;
            sdk(&mut played.write, seq, message).await;
            assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq }));
        }
        let events = rig
            .daemon
            .db
            .events_after(played.uid.clone(), 0, 1000)
            .await
            .unwrap();
        stop(&played.dir);
        pinned_events(
            &events,
            &played.uid,
            "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "2026-11-13T12:00:00.000Z",
            &played.start_uuid(),
        )
    }

    /// **G6: the events of the recorded session as this build files and sends them**,
    /// the fixture the apps' readers of a host run's events test against.
    #[tokio::test]
    async fn the_sdk_session_events_fixture_is_what_this_build_emits() {
        let committed: serde_json::Value = serde_json::from_str(SESSION_EVENTS).unwrap();
        assert_eq!(
            the_capture_as_the_wire_delivers_it().await,
            committed,
            "the committed sdk session events no longer match what this build files. \
             Regenerate them with cargo test -p ccd -- --ignored \
             regenerate_the_sdk_session_events_fixture"
        );
    }

    /// Rewrite `fixtures/claude/sdk-session-events.json` with what this build files.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_sdk_session_events_fixture() {
        let text =
            serde_json::to_string_pretty(&the_capture_as_the_wire_delivers_it().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/sdk-session-events.json"
            ),
            text + "\n",
        )
        .unwrap();
    }

    /// The agent host's one stand-in claude (mac/agent-host/test/stub-claude.ts), as its
    /// own tests write it (rig.ts `writeStub`): a sh wrapper running it on the built
    /// host as Bun. It answers initialize with a recorded claude's answer and, after the
    /// first message, writes the recorded session's `system/init`; `also` adds the
    /// stand-in's other options (`die_on_init`, ...). `name` keeps each one's files apart.
    fn stand_in_claude(
        rig: &Rig,
        host: &Path,
        name: &str,
        also: serde_json::Value,
    ) -> (PathBuf, serde_json::Value) {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let init =
            std::fs::read_to_string(repo.join("fixtures/claude/initialize-api-key-2.1.319.jsonl"))
                .unwrap();
        let init = init.lines().next().unwrap().to_string();
        let session =
            std::fs::read_to_string(repo.join("fixtures/claude/sdk-session-0.3.322.jsonl"))
                .unwrap();
        let session_init = session
            .lines()
            .find(|line| {
                line.contains(r#""type":"system""#) && line.contains(r#""subtype":"init""#)
            })
            .unwrap();
        let mut options = serde_json::json!({"init": init, "after_user": [session_init],
                                             "log": rig.root.join(format!("stub-{name}.log"))});
        options
            .as_object_mut()
            .unwrap()
            .extend(also.as_object().unwrap().clone());
        let script = rig.root.join(format!("stub-{name}.json"));
        std::fs::write(&script, options.to_string()).unwrap();
        let claude = rig.root.join(format!("claude-{name}"));
        std::fs::write(
            &claude,
            format!(
                "#!/bin/sh\nBUN_BE_BUN=1 exec '{}' '{}' '{}' \"$@\"\n",
                host.display(),
                repo.join("mac/agent-host/test/stub-claude.ts").display(),
                script.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let answer: serde_json::Value = serde_json::from_str(&init).unwrap();
        (claude, answer["response"]["response"]["models"].clone())
    }

    /// The built agent host `CODECONNECT_TEST_AGENT_HOST` names, and a rig whose runs start
    /// through zsh's login shell.
    fn real_host_rig() -> (PathBuf, Rig) {
        let host = PathBuf::from(
            std::env::var("CODECONNECT_TEST_AGENT_HOST")
                .expect("CODECONNECT_TEST_AGENT_HOST names the built agent host"),
        );
        let mut rig = rig();
        rig.environment
            .insert(OsString::from("SHELL"), OsString::from("/bin/zsh"));
        (host, rig)
    }

    /// A live run on the real host `host` with the stand-in claude, which stays until its
    /// input ends: its uid, the host's pid and claude's.
    async fn real_live_run(rig: &Rig, host: &Path, name: &str) -> (String, i32, i32) {
        let cwd = rig.root.to_str().unwrap().to_string();
        let (claude, _) = stand_in_claude(rig, host, name, serde_json::json!({}));
        let started = start(
            &rig.daemon,
            Run {
                claude_path: claude.to_str().unwrap(),
                ..run(rig, name, &cwd, host)
            },
        )
        .await;
        let uid = started_uid(&started);
        let host_pid = read_host_file(&run_dir(&rig.root, &uid).join("host.json"))
            .unwrap()
            .host_pid;
        let claude_pid = std::fs::read_to_string(rig.root.join(format!("stub-{name}.log")))
            .unwrap()
            .lines()
            .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()?["pid"].as_i64())
            .unwrap() as i32;
        (uid, host_pid, claude_pid)
    }

    /// Whether `pid` has stopped running, waiting up to `within` for it. A negative `pid`
    /// names a process group, gone once it has no member.
    async fn gone_within(pid: i32, within: Duration) -> bool {
        let until = Instant::now() + within;
        while Instant::now() < until {
            // SAFETY: kill(2) with signal 0 only asks whether the process, or the group, exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            tokio::time::sleep(POLL).await;
        }
        false
    }

    /// **B7 G3: the escalation, on the real host.** The host is SIGSTOPped, so it cannot
    /// act on `stop`: 10 s after it, the ladder ends the host's group, claude with it, and
    /// the run ends "Stopped". Ignored and gated as the real-host test is.
    #[tokio::test]
    #[ignore = "needs the built agent host: CODECONNECT_TEST_AGENT_HOST=<path>"]
    async fn a_real_host_that_does_not_stop_is_stopped_by_its_group() {
        let (host, rig) = real_host_rig();
        let (uid, host_pid, claude_pid) = real_live_run(&rig, &host, "frozen").await;
        // SAFETY: kill(2) of the host this test's run started, by the pid it recorded.
        unsafe { libc::kill(host_pid, libc::SIGSTOP) };
        let asked = Instant::now();
        assert_eq!(rig.daemon.stop_run(&uid).await, StopRunResult::Stopping);
        let mut ended_at = None;
        for _ in 0..300 {
            if kept(&rig, &uid).0 == "exited" {
                ended_at = Some(asked.elapsed());
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (host_gone, claude_gone) = (
            gone_within(host_pid, Duration::from_secs(1)).await,
            gone_within(claude_pid, Duration::from_secs(1)).await,
        );
        if !host_gone {
            // SAFETY: kill(2) of the process group this test's host leads.
            unsafe { libc::kill(-host_pid, libc::SIGKILL) };
        }
        let ended_at = ended_at.expect("the run never ended");
        assert!(
            ended_at >= STOP_GRACE,
            "ended after {ended_at:?}, before the escalation"
        );
        assert!(
            host_gone && claude_gone,
            "host gone {host_gone}, claude gone {claude_gone}"
        );
        assert_eq!(
            the_end(&rig, &uid),
            serde_json::json!({"exit_code": null, "reason": "Stopped"})
        );
    }

    /// **B7 G7 (a): uninstall's stop, on the real host.** ccd's answer comes only once the
    /// live run's host and claude are gone, the run ended "Stopped". A run still starting is
    /// not stopped: its recorded host (a process this test started) is counted left, by pid.
    #[tokio::test]
    #[ignore = "needs the built agent host: CODECONNECT_TEST_AGENT_HOST=<path>"]
    async fn stopping_the_hosted_runs_leaves_no_host_and_no_claude() {
        let (host, rig) = real_host_rig();
        let (uid, host_pid, claude_pid) = real_live_run(&rig, &host, "uninstalled").await;
        let cwd = rig.root.to_str().unwrap().to_string();
        let (starting, _) = begun(&rig, &cwd, &host, Trust::NotTrusted).await;
        let mut pending = std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pending_pid = pending.id() as i32;
        rig.daemon
            .db
            .record_host(
                starting.clone(),
                ProcessIdentity {
                    pid: pending_pid,
                    birth: read_birth_identity(pending_pid).unwrap(),
                },
                protocol::proc_identity::boot_identity(),
            )
            .await
            .unwrap();
        let (stopped, left) = rig.daemon.stop_hosted_runs(Duration::from_secs(20)).await;
        // SAFETY: kill(2) with signal 0 only asks whether the process exists.
        let (host_ran, claude_ran) =
            unsafe { (libc::kill(host_pid, 0) == 0, libc::kill(claude_pid, 0) == 0) };
        let _ = pending.kill();
        let _ = pending.wait();
        if host_ran {
            // SAFETY: kill(2) of the process group this test's host leads.
            unsafe { libc::kill(-host_pid, libc::SIGKILL) };
        }
        assert!(
            !host_ran && !claude_ran,
            "at the answer: host ran {host_ran}, claude ran {claude_ran}"
        );
        assert_eq!((stopped, left), (1, vec![pending_pid as u32]));
        assert_eq!(kept(&rig, &uid).0, "exited");
        assert_eq!(
            the_end(&rig, &uid),
            serde_json::json!({"exit_code": null, "reason": "Stopped"})
        );
        assert_eq!(kept(&rig, &starting).0, "starting");
    }

    /// **B7 G7 (a), a draining host.** A run whose row has ended while its host still runs,
    /// until its claude ends 2 s later, is waited on: the answer comes only once that host
    /// has exited, though nothing was stopped. Ignored and gated as the real-host test is.
    #[tokio::test]
    #[ignore = "needs the built agent host: CODECONNECT_TEST_AGENT_HOST=<path>"]
    async fn stopping_the_hosted_runs_waits_on_a_draining_host() {
        let (host, rig) = real_host_rig();
        let (uid, host_pid, claude_pid) = real_live_run(&rig, &host, "draining").await;
        force_lifecycle(&rig, &uid, "exited");
        let ends_claude = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            // SAFETY: kill(2) of the stand-in claude this test's run started, by its pid.
            unsafe { libc::kill(claude_pid, libc::SIGTERM) };
        });
        let asked = Instant::now();
        let (stopped, left) = rig.daemon.stop_hosted_runs(Duration::from_secs(20)).await;
        let took = asked.elapsed();
        // SAFETY: kill(2) with signal 0 only asks whether the process exists.
        let host_ran = unsafe { libc::kill(host_pid, 0) } == 0;
        let _ = ends_claude.await;
        if host_ran {
            // SAFETY: kill(2) of the process group this test's host leads.
            unsafe { libc::kill(-host_pid, libc::SIGKILL) };
        }
        assert!(
            !host_ran,
            "the answer came after {took:?} with the host still running"
        );
        assert!(took >= Duration::from_secs(2), "answered after {took:?}");
        assert_eq!((stopped, left), (0, Vec::<u32>::new()));
    }

    /// **B7 G2: End run mid-turn, with a real claude** (P1 5). A run on the real host whose
    /// claude, signed in by its own config, is writing a long answer is ended: the run ends
    /// `exited` well before the escalation's 10 s, so by claude's own exit, and the host and
    /// its group are gone. The end's payload is ccd's, whichever claude exits (G1). Ignored and gated: `CODECONNECT_TEST_AGENT_HOST` names the
    /// built host, `CODECONNECT_TEST_REAL_CLAUDE` the claude, and `CODECONNECT_TEST_RIG_DIR`
    /// a short folder the run's files go under; the host and claude get this test's own
    /// `HOME`, `USER`, `LOGNAME`, `PATH` and `CLAUDE_CONFIG_DIR`, so they run on that
    /// config's login and model.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_real_claude_ended_mid_turn_exits_before_the_escalation() {
        let var =
            |name: &str| std::env::var_os(name).unwrap_or_else(|| panic!("{name} is not set"));
        let host = PathBuf::from(var("CODECONNECT_TEST_AGENT_HOST"));
        let claude = PathBuf::from(var("CODECONNECT_TEST_REAL_CLAUDE"));
        let root = PathBuf::from(var("CODECONNECT_TEST_RIG_DIR")).join("g2");
        std::fs::create_dir(&root).unwrap();
        let daemon = daemon_on(&root);
        let mut environment: HashMap<OsString, OsString> = [
            "HOME",
            "USER",
            "LOGNAME",
            "PATH",
            "CLAUDE_CONFIG_DIR",
            "DISABLE_AUTOUPDATER",
        ]
        .into_iter()
        .map(|name| (OsString::from(name), var(name)))
        .collect();
        environment.insert(OsString::from("SHELL"), OsString::from("/bin/zsh"));
        let cwd = root.to_str().unwrap().to_string();
        let codeconnect = root.join("codeconnect");
        let started = start(
            &daemon,
            Run {
                root: &root,
                request_id: "g2",
                cwd: &cwd,
                prompt: "Write a 2,000-word essay on the history of mechanical clocks.",
                model: None,
                worktree: None,
                trust: Trust::NotTrusted,
                claude_path: claude.to_str().unwrap(),
                host_bin: &host,
                codeconnect: &codeconnect,
                environment: &environment,
            },
        )
        .await;
        let uid = started_uid(&started);
        let host_pid = read_host_file(&run_dir(&root, &uid).join("host.json"))
            .unwrap()
            .host_pid;
        // Mid-turn: the answer is being written.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let asked = Instant::now();
        assert_eq!(daemon.stop_run(&uid).await, StopRunResult::Stopping);
        let mut ended_after = None;
        for _ in 0..200 {
            let lifecycle = daemon
                .db
                .hosted_link(uid.clone())
                .await
                .unwrap()
                .unwrap()
                .lifecycle;
            if lifecycle == Lifecycle::Exited {
                ended_after = Some(asked.elapsed());
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let host_gone = gone_within(host_pid, Duration::from_secs(5)).await;
        if !host_gone {
            // SAFETY: kill(2) of the process group this test's host leads.
            unsafe { libc::kill(-host_pid, libc::SIGKILL) };
        }
        // SAFETY: kill(2) with signal 0 only asks whether the group has a member.
        let group_left = unsafe { libc::kill(-host_pid, 0) } == 0;
        println!("G2: ended {ended_after:?} after the stop; host gone {host_gone}; group left {group_left}");
        let ended_after = ended_after.expect("the run never ended");
        assert!(
            ended_after < STOP_GRACE,
            "ended after {ended_after:?}: by the escalation, not by claude"
        );
        assert!(
            host_gone && !group_left,
            "host gone {host_gone}, group left {group_left}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A real claude across a restart, then End run.** A run on the real host whose claude,
    /// signed in by its own config, is writing a long answer when ccd stops: the first
    /// daemon's runtime is shut down mid-turn, as a crash or an update ends ccd, and a second
    /// daemon on the same store recovers it (`recover`, then `recover_hosted_runs`, in
    /// main.rs's order). The run is live and attached again; End run on the restarted daemon
    /// ends it `exited` before the escalation's 10 s, and the host and its group are gone.
    /// Ignored and gated as G2 is: `CODECONNECT_TEST_AGENT_HOST`,
    /// `CODECONNECT_TEST_REAL_CLAUDE`, `CODECONNECT_TEST_RIG_DIR`.
    #[test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    fn a_real_claude_across_a_restart_is_ended_by_the_restarted_daemon() {
        let var =
            |name: &str| std::env::var_os(name).unwrap_or_else(|| panic!("{name} is not set"));
        let host = PathBuf::from(var("CODECONNECT_TEST_AGENT_HOST"));
        let claude = PathBuf::from(var("CODECONNECT_TEST_REAL_CLAUDE"));
        let root = PathBuf::from(var("CODECONNECT_TEST_RIG_DIR")).join("restart");
        std::fs::create_dir(&root).unwrap();
        let mut environment: HashMap<OsString, OsString> = [
            "HOME",
            "USER",
            "LOGNAME",
            "PATH",
            "CLAUDE_CONFIG_DIR",
            "DISABLE_AUTOUPDATER",
        ]
        .into_iter()
        .map(|name| (OsString::from(name), var(name)))
        .collect();
        environment.insert(OsString::from("SHELL"), OsString::from("/bin/zsh"));
        let cwd = root.to_str().unwrap().to_string();
        let codeconnect = root.join("codeconnect");
        // The first ccd: the run starts, and claude is mid-turn when ccd's runtime ends.
        let first = tokio::runtime::Runtime::new().unwrap();
        let (uid, host_pid) = first.block_on(async {
            let daemon = daemon_on(&root);
            let started = start(
                &daemon,
                Run {
                    root: &root,
                    request_id: "restart",
                    cwd: &cwd,
                    prompt: "Write a 2,000-word essay on the history of mechanical clocks.",
                    model: None,
                    worktree: None,
                    trust: Trust::NotTrusted,
                    claude_path: claude.to_str().unwrap(),
                    host_bin: &host,
                    codeconnect: &codeconnect,
                    environment: &environment,
                },
            )
            .await;
            let uid = started_uid(&started);
            let host_pid = read_host_file(&run_dir(&root, &uid).join("host.json"))
                .unwrap()
                .host_pid;
            tokio::time::sleep(Duration::from_secs(3)).await;
            (uid, host_pid)
        });
        drop(first);
        // The restarted ccd, on the same store.
        let second = tokio::runtime::Runtime::new().unwrap();
        let (attached, ended_after) = second.block_on(async {
            let daemon = daemon_on(&root);
            daemon.recover().await;
            daemon.recover_hosted_runs(&root).await;
            let mut attached = false;
            for _ in 0..100 {
                if summary(&daemon, &uid).await.link == protocol::event::Link::Attached {
                    attached = true;
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
            let asked = Instant::now();
            let answer = daemon.stop_run(&uid).await;
            assert_eq!(
                answer,
                StopRunResult::Stopping,
                "the restarted run is not live"
            );
            let mut ended_after = None;
            for _ in 0..200 {
                let lifecycle = daemon
                    .db
                    .hosted_link(uid.clone())
                    .await
                    .unwrap()
                    .unwrap()
                    .lifecycle;
                if lifecycle == Lifecycle::Exited {
                    ended_after = Some(asked.elapsed());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            (attached, ended_after)
        });
        let host_gone = second.block_on(gone_within(host_pid, Duration::from_secs(5)));
        if !host_gone {
            // SAFETY: kill(2) of the process group this test's host leads.
            unsafe { libc::kill(-host_pid, libc::SIGKILL) };
        }
        // SAFETY: kill(2) with signal 0 only asks whether the group has a member.
        let group_left = unsafe { libc::kill(-host_pid, 0) } == 0;
        println!(
            "RESTART: attached again {attached}; ended {ended_after:?} after the stop; host gone {host_gone}; group left {group_left}"
        );
        assert!(
            attached,
            "the restarted daemon never attached the host again"
        );
        let ended_after = ended_after.expect("the run never ended");
        assert!(
            ended_after < STOP_GRACE,
            "ended after {ended_after:?}: by the escalation, not by claude"
        );
        assert!(
            host_gone && !group_left,
            "host gone {host_gone}, group left {group_left}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Whether the run `uid` is settled after the person's last words: a turn has ended
    /// since them, and nothing new has been filed for 5 s. Waits 180 s at most.
    async fn settled_after_words(rig: &Rig, uid: &str) -> bool {
        let (mut seen, mut quiet) = (0, 0);
        for _ in 0..360 {
            let events = stored(rig, uid);
            let ended = events
                .iter()
                .rposition(|event| event["kind"] == "user_message" && event["source"] == "daemon")
                .is_some_and(|at| {
                    events[at..]
                        .iter()
                        .any(|event| event["kind"] == "turn_complete")
                });
            quiet = if events.len() == seen { quiet + 1 } else { 0 };
            seen = events.len();
            if ended && quiet >= 10 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        false
    }

    /// How often each of `texts` reached claude, by its own transcript of the session
    /// `session_id` under `config` (`projects/*/<session_id>.jsonl`): as a person's line, or
    /// folded into a running tool's result, which claude 2.1.322 keeps as an `attachment` of
    /// type `queued_command` with the words as its `prompt`. `None` when there is no such
    /// file.
    fn transcript_counts(config: &Path, session_id: &str, texts: &[&str]) -> Option<Vec<usize>> {
        let name = format!("{session_id}.jsonl");
        let file = std::fs::read_dir(config.join("projects"))
            .ok()?
            .filter_map(Result::ok)
            .map(|project| project.path().join(&name))
            .find(|path| path.is_file())?;
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(file)
            .ok()?
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Some(
            texts
                .iter()
                .map(|text| {
                    lines
                        .iter()
                        .filter(|line| {
                            (line["type"] == "user" && line["message"]["content"] == *text)
                                || (line["type"] == "attachment"
                                    && line["attachment"]["type"] == "queued_command"
                                    && line["attachment"]["prompt"] == *text)
                        })
                        .count()
                })
                .collect(),
        )
    }

    /// G6's and G9's transcript check without a real claude: a message claude's transcript
    /// holds twice, or not at all, is counted so, and no transcript is `None`; each fails
    /// "exactly once". A message folded into a tool's result counts once, and the same words
    /// both folded and as a person's line count twice. The folded line is app-qa's G9 (a)
    /// transcript's, its ids and long fields cut; the queue's own lines never count.
    #[tokio::test]
    async fn the_transcript_counts_a_message_held_twice_or_not_at_all() {
        let rig = rig();
        let project = rig.root.join("config/projects/a-project");
        std::fs::create_dir_all(&project).unwrap();
        let user = |text: &str| {
            serde_json::json!({"type": "user", "message": {"role": "user", "content": text}})
                .to_string()
        };
        let folded = |text: &str| {
            serde_json::json!({"type": "attachment", "isSidechain": false,
                "attachment": {"type": "queued_command", "prompt": text, "commandMode": "prompt",
                               "origin": {"kind": "human"}, "humanTurn": true},
                "renderedRole": "system"})
            .to_string()
        };
        let queued = |operation: &str, text: &str| {
            serde_json::json!({"type": "queue-operation", "operation": operation, "content": text})
                .to_string()
        };
        let reply =
            serde_json::json!({"type": "assistant", "message": {"content": "twice"}}).to_string();
        let lines = [
            user("once"),
            user("twice"),
            reply,
            user("twice"),
            queued("enqueue", "folded"),
            folded("folded"),
            queued("remove", "folded"),
            user("both"),
            folded("both"),
            queued("enqueue", "dropped"),
        ]
        .join("\n");
        std::fs::write(project.join("s1.jsonl"), lines).unwrap();
        let config = rig.root.join("config");
        let counts = transcript_counts(
            &config,
            "s1",
            &["once", "twice", "never", "folded", "both", "dropped"],
        );
        assert_eq!(counts, Some(vec![1, 2, 0, 1, 2, 0]));
        assert_ne!(counts, Some(vec![1, 1, 1, 1, 1, 1]));
        assert_eq!(transcript_counts(&config, "s2", &["once"]), None);
    }

    /// **B5 G6: messages mid-turn, with a real claude** (app-qa's rig). A run on the real
    /// host whose claude, signed in by its own config, is sent a message while it writes
    /// its first answer; once that has settled, it is asked to read a file, and sent
    /// another message as soon as that tool call is filed (Read needs no permission). Each
    /// message is `sent` and filed once as the person's words, the run settles with a turn
    /// ended after the last, and claude's own transcript holds each exactly once. The
    /// order the run's events came in is printed. Then End run, as G2. Ignored and gated
    /// as G2 is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_real_claude_takes_messages_mid_turn() {
        let var =
            |name: &str| std::env::var_os(name).unwrap_or_else(|| panic!("{name} is not set"));
        let host = PathBuf::from(var("CODECONNECT_TEST_AGENT_HOST"));
        let claude = PathBuf::from(var("CODECONNECT_TEST_REAL_CLAUDE"));
        let config = PathBuf::from(var("CLAUDE_CONFIG_DIR"));
        let root = PathBuf::from(var("CODECONNECT_TEST_RIG_DIR")).join("g6");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("note.txt"), "The keeper's name is Ada.\n").unwrap();
        let mut environment: HashMap<OsString, OsString> = [
            "HOME",
            "USER",
            "LOGNAME",
            "PATH",
            "CLAUDE_CONFIG_DIR",
            "DISABLE_AUTOUPDATER",
        ]
        .into_iter()
        .map(|name| (OsString::from(name), var(name)))
        .collect();
        environment.insert(OsString::from("SHELL"), OsString::from("/bin/zsh"));
        let rig = Rig {
            daemon: daemon_on(&root),
            root,
            environment,
        };
        let cwd = rig.root.to_str().unwrap().to_string();
        let codeconnect = rig.root.join("codeconnect");
        let prompt = "Write a 300-word story about a lighthouse keeper.";
        let started = start(
            &rig.daemon,
            Run {
                root: &rig.root,
                request_id: "g6",
                cwd: &cwd,
                prompt,
                model: None,
                worktree: None,
                trust: Trust::NotTrusted,
                claude_path: claude.to_str().unwrap(),
                host_bin: &host,
                codeconnect: &codeconnect,
                environment: &rig.environment,
            },
        )
        .await;
        let uid = started_uid(&started);
        let host_pid = read_host_file(&run_dir(&rig.root, &uid).join("host.json"))
            .unwrap()
            .host_pid;
        let messages = [
            ("g6-1", "Give the keeper a dog."),
            (
                "g6-2",
                "Use the Read tool to read note.txt in this folder, then tell me the keeper's name.",
            ),
            ("g6-3", "Answer in one sentence."),
        ];
        let mut sent = Vec::new();
        // While the first answer is written.
        tokio::time::sleep(Duration::from_secs(3)).await;
        sent.push(send_as_app(&rig.daemon, &uid, messages[0].0, messages[0].1).await);
        let first_settled = settled_after_words(&rig, &uid).await;
        sent.push(send_as_app(&rig.daemon, &uid, messages[1].0, messages[1].1).await);
        // As soon as the Read call is filed.
        let mut tool_filed = false;
        for _ in 0..1200 {
            let events = stored(&rig, &uid);
            tool_filed = events
                .iter()
                .rposition(|event| event["kind"] == "user_message" && event["source"] == "daemon")
                .is_some_and(|at| {
                    events[at..].iter().any(|event| {
                        event["kind"] == "agent_message"
                            && event["payload"].to_string().contains("\"tool_use\"")
                    })
                });
            if tool_filed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        sent.push(send_as_app(&rig.daemon, &uid, messages[2].0, messages[2].1).await);
        let last_settled = settled_after_words(&rig, &uid).await;
        let order: Vec<String> = stored(&rig, &uid)
            .iter()
            .map(|event| {
                let kind = event["kind"].as_str().unwrap_or_default();
                let payload = event["payload"].to_string();
                if event["source"] == "daemon" && kind == "user_message" {
                    format!("you: {}", event["payload"]["text"])
                } else if kind == "agent_message" && payload.contains("\"tool_use\"") {
                    "tool call".into()
                } else {
                    kind.into()
                }
            })
            .collect();
        println!("G6 ORDER: {}", order.join(" | "));
        let texts: Vec<&str> = messages.iter().map(|(_, text)| *text).collect();
        let transcript = row_of(&rig, &uid)
            .0
            .and_then(|session_id| transcript_counts(&config, &session_id, &texts));
        println!(
            "G6: sent {sent:?}; first settled {first_settled}; tool call filed {tool_filed}; last settled {last_settled}; claude's transcript holds each {transcript:?} times"
        );
        assert_eq!(rig.daemon.stop_run(&uid).await, StopRunResult::Stopping);
        let mut ended = false;
        for _ in 0..200 {
            let lifecycle = rig
                .daemon
                .db
                .hosted_link(uid.clone())
                .await
                .unwrap()
                .unwrap()
                .lifecycle;
            if lifecycle == Lifecycle::Exited {
                ended = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let host_gone = gone_within(host_pid, Duration::from_secs(5)).await;
        if !host_gone {
            // SAFETY: kill(2) of the process group this test's host leads.
            unsafe { libc::kill(-host_pid, libc::SIGKILL) };
        }
        // SAFETY: kill(2) with signal 0 only asks whether the group has a member.
        let group_left = unsafe { libc::kill(-host_pid, 0) } == 0;
        println!("G6: ended {ended}; host gone {host_gone}; group left {group_left}");
        for ((_, text), result) in messages.iter().zip(&sent) {
            assert_eq!(
                *result,
                SendTextResult::Sent {
                    matched: text.to_string()
                },
                "{text}"
            );
        }
        let mut expected = vec![prompt.to_string()];
        expected.extend(texts.iter().map(|text| text.to_string()));
        assert_eq!(words(&rig, &uid), expected, "each filed once, in order");
        assert!(first_settled && last_settled, "the run did not settle");
        assert!(tool_filed, "claude made no tool call");
        assert_eq!(
            transcript,
            Some(vec![1, 1, 1]),
            "claude's transcript holds each message exactly once"
        );
        assert!(ended, "the run never ended");
        assert!(
            host_gone && !group_left,
            "host gone {host_gone}, group left {group_left}"
        );
    }

    /// A run on the real host whose claude, signed in by its own config, is given `prompt`
    /// in a folder of its own, `name`, under `CODECONNECT_TEST_RIG_DIR`, as G6's run is:
    /// the rig, the run's uid and its host's pid.
    async fn real_claude_run(name: &str, prompt: &str) -> (Rig, String, i32) {
        let var =
            |name: &str| std::env::var_os(name).unwrap_or_else(|| panic!("{name} is not set"));
        let host = PathBuf::from(var("CODECONNECT_TEST_AGENT_HOST"));
        let claude = PathBuf::from(var("CODECONNECT_TEST_REAL_CLAUDE"));
        let root = PathBuf::from(var("CODECONNECT_TEST_RIG_DIR")).join(name);
        std::fs::create_dir(&root).unwrap();
        let mut environment: HashMap<OsString, OsString> = [
            "HOME",
            "USER",
            "LOGNAME",
            "PATH",
            "CLAUDE_CONFIG_DIR",
            "DISABLE_AUTOUPDATER",
        ]
        .into_iter()
        .map(|name| (OsString::from(name), var(name)))
        .collect();
        environment.insert(OsString::from("SHELL"), OsString::from("/bin/zsh"));
        let rig = Rig {
            daemon: daemon_on(&root),
            root,
            environment,
        };
        let cwd = rig.root.to_str().unwrap().to_string();
        let codeconnect = rig.root.join("codeconnect");
        let started = start(
            &rig.daemon,
            Run {
                root: &rig.root,
                request_id: name,
                cwd: &cwd,
                prompt,
                model: None,
                worktree: None,
                trust: Trust::NotTrusted,
                claude_path: claude.to_str().unwrap(),
                host_bin: &host,
                codeconnect: &codeconnect,
                environment: &rig.environment,
            },
        )
        .await;
        let uid = started_uid(&started);
        let host_pid = read_host_file(&run_dir(&rig.root, &uid).join("host.json"))
            .unwrap()
            .host_pid;
        (rig, uid, host_pid)
    }

    /// The run `uid`'s first card, once claude has asked: its request id and the hash an
    /// app answers it with, as its `approval_request` carries them. Waits 120 s at most.
    async fn first_card(rig: &Rig, uid: &str) -> Option<(String, String)> {
        for _ in 0..1200 {
            let asked = stored(rig, uid)
                .into_iter()
                .find(|event| event["kind"] == "approval_request");
            if let Some(event) = asked {
                let card = &event["payload"]["card"];
                return Some((
                    card["request_id"].as_str()?.to_string(),
                    card["payload_hash"].as_str()?.to_string(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }

    /// The app's Allow of the card `card` of the run `uid`, from an iPhone.
    async fn allow_from_app(
        rig: &Rig,
        uid: &str,
        card: &Option<(String, String)>,
    ) -> Option<AnswerResult> {
        let (request_id, hash) = card.as_ref()?;
        Some(
            rig.daemon
                .answer(
                    request_id,
                    hash,
                    AnswerDecision::Allow,
                    Some(uid),
                    AnsweredOn::Iphone,
                )
                .await,
        )
    }

    /// What G9's command wrote into the run's folder, `rang.txt`, if it ran. A command that
    /// writes asks for permission; one that only reads runs without a card.
    fn rang(rig: &Rig) -> Option<String> {
        std::fs::read_to_string(rig.root.join("rang.txt")).ok()
    }

    /// The members of the process group `group`, each its pid and command, as pgrep
    /// lists them.
    fn group_members(group: i32) -> Vec<String> {
        std::process::Command::new("/usr/bin/pgrep")
            .args(["-a", "-l", "-f", "-g", &group.to_string()])
            .output()
            .map(|listed| {
                String::from_utf8_lossy(&listed.stdout)
                    .lines()
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Wait up to 10 s for the process group `group` to have no member: none left, or
    /// each one still there, named before the group is ended.
    async fn left_in_group(group: i32) -> Vec<String> {
        if gone_within(-group, Duration::from_secs(10)).await {
            return Vec::new();
        }
        let left = group_members(group);
        // SAFETY: kill(2) of a process group this test's run started.
        unsafe { libc::kill(-group, libc::SIGKILL) };
        left
    }

    /// End run on the run `uid`, as G2 sends it: the answer, whether the run then ended
    /// `exited` within 20 s, whether its host `host_pid` is gone, and what is left in its
    /// group ([`left_in_group`]).
    async fn end_real_run(
        rig: &Rig,
        uid: &str,
        host_pid: i32,
    ) -> (StopRunResult, bool, bool, Vec<String>) {
        let answer = rig.daemon.stop_run(uid).await;
        let mut ended = false;
        for _ in 0..200 {
            let lifecycle = rig
                .daemon
                .db
                .hosted_link(uid.to_string())
                .await
                .unwrap()
                .unwrap()
                .lifecycle;
            if lifecycle == Lifecycle::Exited {
                ended = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let host_gone = gone_within(host_pid, Duration::from_secs(5)).await;
        (answer, ended, host_gone, left_in_group(host_pid).await)
    }

    /// **B6 G9 (a): an allowed command, and a message while it runs, with a real claude**
    /// (app-qa's rig). claude asks to run a Bash command that sleeps 8 s, then writes a
    /// file in the run's folder (a command that only reads runs without a card); the
    /// iPhone's Allow is `Applied`; a message sent 2 s later, before the file is written, is
    /// `sent` and filed once; the command writes what it was asked to; the run settles with
    /// no card open, and claude got the message once ([`transcript_counts`]: sent while the
    /// command runs, it reaches claude with the command's result). Then End run, as G2.
    /// Ignored and gated as G2 is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_real_claude_runs_an_allowed_command_while_a_message_waits() {
        let prompt = "Use the Bash tool to run exactly this command, once: sleep 8 && echo $((6*7))-rang > rang.txt";
        let message = "Then say goodbye in one word.";
        let (rig, uid, host_pid) = real_claude_run("g9a", prompt).await;
        let card = first_card(&rig, &uid).await;
        let answer = allow_from_app(&rig, &uid, &card).await;
        // While the command sleeps.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let written_before = rang(&rig);
        let sent = send_as_app(&rig.daemon, &uid, "g9a-1", message).await;
        let settled = settled_after_words(&rig, &uid).await;
        let written = rang(&rig);
        let open = cards(&rig.daemon, &uid).await;
        let config = PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").unwrap());
        let transcript = row_of(&rig, &uid)
            .0
            .and_then(|session_id| transcript_counts(&config, &session_id, &[message]));
        println!(
            "G9: allow: card {card:?}; answer {answer:?}; written before the message {written_before:?}; sent {sent:?}; settled {settled}; written {written:?}; open {open:?}; claude's transcript holds the message {transcript:?} times"
        );
        let (stopping, ended, host_gone, left) = end_real_run(&rig, &uid, host_pid).await;
        println!(
            "G9: allow: End run {stopping:?}; ended {ended}; host gone {host_gone}; left in its group {left:?}"
        );
        assert!(card.is_some(), "claude asked nothing");
        assert!(
            matches!(answer, Some(AnswerResult::Applied { .. })),
            "{answer:?}"
        );
        assert_eq!(
            written_before, None,
            "the command had written before the message was sent"
        );
        assert_eq!(
            sent,
            SendTextResult::Sent {
                matched: message.to_string()
            }
        );
        assert_eq!(
            words(&rig, &uid),
            vec![prompt.to_string(), message.to_string()],
            "each filed once, in order"
        );
        assert_eq!(
            written.as_deref(),
            Some("42-rang\n"),
            "the allowed command never wrote"
        );
        assert!(settled, "the run did not settle");
        assert_eq!(
            open,
            (Vec::<String>::new(), Vec::<String>::new()),
            "a card left open"
        );
        assert_eq!(transcript, Some(vec![1]), "claude got the message once");
        assert_eq!(stopping, StopRunResult::Stopping);
        assert!(
            ended && host_gone && left.is_empty(),
            "ended {ended}, host gone {host_gone}, left in its group {left:?}"
        );
    }

    /// **B6 G9 (b): End run with a card open, with a real claude.** claude asks to run a
    /// command, and the run is ended before anyone answers: it ends `exited`, the host and
    /// its group are gone, the card closes once with nobody having answered, nothing is
    /// left open, the command never ran, and an app's late Allow is refused. Ignored and
    /// gated as G2 is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_real_claude_ended_with_a_card_open_leaves_it_unanswered() {
        let prompt =
            "Use the Bash tool to run exactly this command, once: echo $((6*7))-rang > rang.txt";
        let (rig, uid, host_pid) = real_claude_run("g9b", prompt).await;
        let card = first_card(&rig, &uid).await;
        let (stopping, ended, host_gone, left) = end_real_run(&rig, &uid, host_pid).await;
        let closed: Vec<serde_json::Value> = stored(&rig, &uid)
            .into_iter()
            .filter(|event| event["kind"] == "approval_resolved")
            .map(|event| event["payload"].clone())
            .collect();
        let open = cards(&rig.daemon, &uid).await;
        let late = allow_from_app(&rig, &uid, &card).await;
        let written = rang(&rig);
        println!(
            "G9: end: card {card:?}; End run {stopping:?}; ended {ended}; host gone {host_gone}; left in its group {left:?}; closed {closed:?}; open {open:?}; late answer {late:?}; written {written:?}"
        );
        assert!(card.is_some(), "claude asked nothing");
        assert_eq!(stopping, StopRunResult::Stopping);
        assert!(
            ended && host_gone && left.is_empty(),
            "ended {ended}, host gone {host_gone}, left in its group {left:?}"
        );
        assert_eq!(closed.len(), 1, "closed once: {closed:?}");
        assert_eq!(closed[0]["resolved_by"], "timeout", "nobody answered");
        assert_eq!(closed[0]["answered_on"], serde_json::Value::Null);
        assert_eq!(
            open,
            (Vec::<String>::new(), Vec::<String>::new()),
            "a card left open"
        );
        assert!(
            matches!(late, Some(AnswerResult::Rejected { .. })),
            "{late:?}"
        );
        assert_eq!(written, None, "the command ran with nobody answering");
    }

    /// **B6 G9 (c): SIGTERM mid-command, with a real claude.** claude's allowed command,
    /// a shell that writes its pid and sleeps 60 s, is running when the host's group is
    /// sent SIGTERM: the host and the command's shell are gone within 10 s, and neither
    /// the host's group nor the shell's has a member left 10 s later. What is left is
    /// named, then ended by the pids and groups this run started. Ignored and gated as G2
    /// is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_real_claude_sent_sigterm_mid_command_leaves_nothing() {
        let prompt =
            "Use the Bash tool to run exactly this command, once: echo $$ > tool.pid && sleep 60";
        let (rig, uid, host_pid) = real_claude_run("g9c", prompt).await;
        let card = first_card(&rig, &uid).await;
        let answer = allow_from_app(&rig, &uid, &card).await;
        // The command's shell, once it has written its pid.
        let mut shell = None;
        for _ in 0..300 {
            shell = std::fs::read_to_string(rig.root.join("tool.pid"))
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok());
            if shell.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // SAFETY: getpgid(2) only reads the group of the command's shell; getpgrp(2) this
        // test's own.
        let (shell_group, own_group) = unsafe {
            (
                shell
                    .map(|pid| libc::getpgid(pid))
                    .filter(|group| *group > 0),
                libc::getpgrp(),
            )
        };
        // SAFETY: kill(2) of the process group this test's host leads.
        unsafe { libc::kill(-host_pid, libc::SIGTERM) };
        let host_gone = gone_within(host_pid, Duration::from_secs(10)).await;
        let shell_gone = match shell {
            Some(pid) => gone_within(pid, Duration::from_secs(10)).await,
            None => false,
        };
        let host_group_left = left_in_group(host_pid).await;
        let shell_group_left = match shell_group {
            Some(group) if group != own_group && group != host_pid => left_in_group(group).await,
            _ => Vec::new(),
        };
        if let (false, Some(pid)) = (shell_gone, shell) {
            // SAFETY: kill(2) of the shell this test's claude started, by its pid.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        println!(
            "G9: sigterm: card {card:?}; answer {answer:?}; shell {shell:?} in group {shell_group:?}; host gone {host_gone}; shell gone {shell_gone}; left in the host's group {host_group_left:?}; left in the shell's group {shell_group_left:?}"
        );
        assert!(card.is_some(), "claude asked nothing");
        assert!(
            matches!(answer, Some(AnswerResult::Applied { .. })),
            "{answer:?}"
        );
        assert!(shell.is_some(), "the command never started");
        assert!(
            host_gone && shell_gone && host_group_left.is_empty() && shell_group_left.is_empty(),
            "host gone {host_gone}, shell gone {shell_gone}, left in the host's group {host_group_left:?}, left in the shell's group {shell_group_left:?}"
        );
    }

    /// **B6 part 2a G8e: a real claude's question answered from the app** (app-qa's rig).
    /// claude is asked to put one question with two options through AskUserQuestion. Its
    /// card is a question; the iPhone picks "Coffee" and the answer is `Applied`, not
    /// unconfirmed; claude's tool result holds that answer; the run settles with no card
    /// open. Then End run, as G2. Ignored and gated as G2 is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_real_claude_question_is_answered_from_the_app() {
        let prompt = "Use the AskUserQuestion tool once to ask me exactly this question, with exactly these two options and nothing else: question \"Tea or coffee?\", header \"Drink\", options \"Tea\" and \"Coffee\". Then tell me which one I picked.";
        let (rig, uid, host_pid) = real_claude_run("g8e", prompt).await;
        let card = first_card(&rig, &uid).await;
        let asked = stored(&rig, &uid)
            .into_iter()
            .find(|event| event["kind"] == "approval_request")
            .map(|event| event["payload"]["card"].clone());
        let coffee = asked.as_ref().and_then(|card| {
            card["tool_input"]["questions"][0]["options"]
                .as_array()?
                .iter()
                .position(|option| option["label"] == "Coffee")
        });
        let answer = match (&card, coffee) {
            (Some((request_id, hash)), Some(index)) => Some(
                rig.daemon
                    .answer(
                        request_id,
                        hash,
                        AnswerDecision::Answers {
                            answers: vec![protocol::ws::QuestionAnswer {
                                selected: vec![index as u32],
                                ..Default::default()
                            }],
                        },
                        Some(&uid),
                        AnsweredOn::Iphone,
                    )
                    .await,
            ),
            _ => None,
        };
        let settled = settled_after_words(&rig, &uid).await;
        let tool_result = stored(&rig, &uid)
            .into_iter()
            .filter(|event| event["kind"] == "tool_result")
            .map(|event| event["payload"].to_string())
            .find(|payload| payload.contains("Tea or coffee?"));
        let open = cards(&rig.daemon, &uid).await;
        println!(
            "G8e: card {card:?}; tool {:?}; Coffee at {coffee:?}; answer {answer:?}; settled {settled}; tool result {tool_result:?}; open {open:?}",
            asked.as_ref().map(|card| card["tool_name"].clone())
        );
        let (stopping, ended, host_gone, left) = end_real_run(&rig, &uid, host_pid).await;
        println!(
            "G8e: End run {stopping:?}; ended {ended}; host gone {host_gone}; left in its group {left:?}"
        );
        assert!(card.is_some(), "claude asked nothing");
        assert_eq!(
            asked.as_ref().map(|card| card["tool_name"].clone()),
            Some(serde_json::json!("AskUserQuestion"))
        );
        assert!(coffee.is_some(), "the question has no Coffee option");
        let Some(AnswerResult::Applied { outcome }) = &answer else {
            panic!("not applied: {answer:?}");
        };
        assert!(!outcome.indeterminate);
        assert!(
            tool_result
                .as_deref()
                .is_some_and(|text| text.contains("Coffee")),
            "claude's tool result does not hold the answer: {tool_result:?}"
        );
        assert!(settled, "the run did not settle");
        assert_eq!(
            open,
            (Vec::<String>::new(), Vec::<String>::new()),
            "a card left open"
        );
        assert_eq!(stopping, StopRunResult::Stopping);
        assert!(
            ended && host_gone && left.is_empty(),
            "ended {ended}, host gone {host_gone}, left in its group {left:?}"
        );
    }

    /// **B6 part 2b G17: "Allow for this session" on a real claude** (app-qa's rig; probe
    /// M1's shape, now through ccd and an app answer). claude is asked to run `touch m1.txt`
    /// twice, one call after the other. Its first card offers "Allow for this session", the
    /// iPhone answers with it, and the answer is `Applied`; the second call is not asked, so
    /// the run had one card, and both calls ran; no settings file changed, byte for byte:
    /// the config's own, and the folder's, which has none. Then End run, as G2. Ignored and
    /// gated as G2 is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_real_claude_allowed_for_the_session_is_not_asked_again() {
        let prompt = "Use the Bash tool to run exactly this command: touch m1.txt. Once it has finished, run exactly the same command again, touch m1.txt, as a second Bash call. Then say done.";
        let config = PathBuf::from(
            std::env::var_os("CLAUDE_CONFIG_DIR").expect("CLAUDE_CONFIG_DIR is not set"),
        );
        let read = |dir: &Path| -> Vec<(String, Option<Vec<u8>>)> {
            ["settings.json", "settings.local.json"]
                .into_iter()
                .map(|name| (name.to_string(), std::fs::read(dir.join(name)).ok()))
                .collect()
        };
        let config_before = read(&config);
        let (rig, uid, host_pid) = real_claude_run("g17", prompt).await;
        let card = first_card(&rig, &uid).await;
        let offered = card.as_ref().map(|(request_id, _)| {
            filed_card(&rig, &uid, request_id)
                .get("allow_for_session")
                .cloned()
        });
        let answer = match &card {
            Some((request_id, hash)) => Some(
                rig.daemon
                    .answer(
                        request_id,
                        hash,
                        allow_for_this_session(),
                        Some(&uid),
                        AnsweredOn::Iphone,
                    )
                    .await,
            ),
            None => None,
        };
        let settled = settled_after_words(&rig, &uid).await;
        let events = stored(&rig, &uid);
        let asked = events
            .iter()
            .filter(|event| event["kind"] == "approval_request")
            .count();
        let results: Vec<serde_json::Value> = events
            .iter()
            .filter(|event| event["kind"] == "tool_result")
            .map(|event| event["payload"]["message"]["content"][0].clone())
            .collect();
        let ran = results
            .iter()
            .filter(|result| result["is_error"] != true)
            .count();
        let open = cards(&rig.daemon, &uid).await;
        let (config_after, folder_after) = (read(&config), read(&rig.root.join(".claude")));
        println!(
            "G17: card {card:?}; offered {offered:?}; answer {answer:?}; settled {settled}; asked {asked}; results {results:?}; open {open:?}; config unchanged {}; folder settings {folder_after:?}",
            config_after == config_before
        );
        let (stopping, ended, host_gone, left) = end_real_run(&rig, &uid, host_pid).await;
        println!("G17: End run {stopping:?}; ended {ended}; host gone {host_gone}; left in its group {left:?}");
        assert!(card.is_some(), "claude asked nothing");
        assert_eq!(
            offered,
            Some(Some(serde_json::Value::Bool(true))),
            "the card did not offer it"
        );
        let Some(AnswerResult::Applied { outcome }) = &answer else {
            panic!("not applied: {answer:?}");
        };
        assert!(!outcome.indeterminate);
        assert_eq!(asked, 1, "the second call was asked again");
        assert!(settled, "the run did not settle");
        assert_eq!((results.len(), ran), (2, 2), "both calls ran: {results:?}");
        assert_eq!(
            open,
            (Vec::<String>::new(), Vec::<String>::new()),
            "a card left open"
        );
        assert!(
            config_after == config_before,
            "the config's settings changed"
        );
        assert!(
            folder_after.iter().all(|(_, bytes)| bytes.is_none()),
            "the folder has settings: {folder_after:?}"
        );
        assert_eq!(stopping, StopRunResult::Stopping);
        assert!(
            ended && host_gone && left.is_empty(),
            "ended {ended}, host gone {host_gone}, left in its group {left:?}"
        );
    }

    /// The run `uid`'s turn running now: its last `session_start`'s `turn_id`, once a turn has
    /// begun since `after` (that many such starts). Waits 120 s at most.
    async fn turn_after(rig: &Rig, uid: &str, after: usize) -> Option<String> {
        for _ in 0..1200 {
            let starts: Vec<String> = stored(rig, uid)
                .into_iter()
                .filter(|event| event["kind"] == "session_start")
                .filter_map(|event| event["turn_id"].as_str().map(str::to_string))
                .collect();
            if starts.len() > after {
                return starts.last().cloned();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }

    /// Whether the run `uid` filed a tool call in its turn `turn`, waiting 120 s at most.
    async fn tool_called(rig: &Rig, uid: &str, turn: &str) -> bool {
        for _ in 0..1200 {
            let called = stored(rig, uid).iter().any(|event| {
                event["kind"] == "agent_message"
                    && event["turn_id"] == turn
                    && event["payload"]["message"]["content"]
                        .as_array()
                        .is_some_and(|blocks| {
                            blocks.iter().any(|block| block["type"] == "tool_use")
                        })
            });
            if called {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// Whether the run `uid` filed the end of its turn `turn`, waiting 30 s at most.
    async fn turn_ended(rig: &Rig, uid: &str, turn: &str) -> bool {
        for _ in 0..300 {
            if stored(rig, uid)
                .iter()
                .any(|event| event["kind"] == "turn_complete" && event["turn_id"] == turn)
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// **B8 G6: what the daemon serves of a real claude's command menu, and how it files a
    /// command's output** (app-qa's rig; the test binary drives the daemon, no app). After a
    /// first turn, the run's catalog is what claude reported: none
    /// of a claude.ai connector's commands, of its screen-only ones or of
    /// [`REFUSED_SCREENS`], and `usage` among them. Then `/effort low` and `/usage`, each
    /// sent as a message, file their output as a terminal run does: a `user_message` with
    /// claude's `<local-command-stdout>` string. `/effort`'s is printed; of `/usage`'s only
    /// its shape (the wrapper and a length), as it holds the signed-in account's own usage.
    /// Then End run, as G2. Ignored and gated as G2 is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_real_claude_lists_its_commands_and_files_their_output() {
        let (rig, uid, host_pid) = real_claude_run("g6b8", "Reply with the single word ok.").await;
        let first = settled_after_words(&rig, &uid).await;
        let catalog = rig.daemon.command_catalog(&uid).await;
        let screen_only = rig
            .daemon
            .db
            .hosted_commands(uid.clone())
            .await
            .unwrap()
            .and_then(|kept| kept.screen_only)
            .unwrap_or_default();
        let outputs = |rig: &Rig| -> Vec<String> {
            stored(rig, &uid)
                .iter()
                .filter(|event| event["kind"] == "user_message")
                .filter_map(|event| event["payload"]["message"]["content"].as_str())
                .filter(|content| content.starts_with("<local-command-std"))
                .map(str::to_string)
                .collect()
        };
        let mut said = Vec::new();
        for command in ["/effort low", "/usage"] {
            let before = outputs(&rig).len();
            let sent = send_as_app(&rig.daemon, &uid, command, command).await;
            let settled = settled_after_words(&rig, &uid).await;
            said.push((command, sent, settled, outputs(&rig)[before..].to_vec()));
        }
        println!(
            "B8 G6: first turn settled {first}; screen-only {screen_only:?}; catalog {catalog:?}"
        );
        for (command, sent, settled, output) in &said {
            let shown: Vec<String> = if *command == "/usage" {
                output
                    .iter()
                    .map(|row| {
                        let wrapper = row.split('>').next().unwrap_or_default();
                        format!("user_message {wrapper}> {} bytes", row.len())
                    })
                    .collect()
            } else {
                output.clone()
            };
            println!("B8 G6: {command}: {sent:?}; settled {settled}; output {shown:?}");
        }
        let (stopping, ended, host_gone, left) = end_real_run(&rig, &uid, host_pid).await;
        println!("B8 G6: End run {stopping:?}; ended {ended}; host gone {host_gone}; left in its group {left:?}");
        assert!(first, "the first turn did not settle");
        let CommandCatalogResult::Available { commands, .. } = &catalog else {
            panic!("no menu: {catalog:?}");
        };
        for name in commands {
            assert!(
                !name.starts_with("claude.ai "),
                "a connector's command: {name}"
            );
            assert!(!screen_only.contains(name), "a screen-only command: {name}");
            assert!(
                REFUSED_SCREENS.iter().all(|(refused, _)| refused != name),
                "a refused screen's command: {name}"
            );
        }
        assert!(commands.iter().any(|name| name == "usage"), "no usage");
        for (command, sent, settled, output) in &said {
            assert!(
                matches!(sent, SendTextResult::Sent { .. }),
                "{command}: {sent:?}"
            );
            assert!(*settled, "{command} did not settle");
            assert!(!output.is_empty(), "{command} filed no output");
        }
        assert!(said[0].3[0].contains("effort"), "{:?}", said[0].3);
        assert_eq!(stopping, StopRunResult::Stopping);
        assert!(ended && host_gone, "the run did not end");
        assert_eq!(left, Vec::<String>::new());
    }

    /// **B8 part 2 S7: a real claude takes its settings from the app** (app-qa's rig, one
    /// prompt). The start asks claude to run `echo s7 > s7.txt && sleep 8` with Bash (a command
    /// that writes, so claude asks), and its card is allowed from the app. While that turn
    /// runs the model is set, which applies from the next turn; once
    /// the turn has settled, the effort and then the mode, which apply now. Printed: each
    /// answer, the run's model, mode and effort after them, and claude's own echoes filed
    /// after the changes (its `<local-command-stdout>` rows and its mode statuses), each cut
    /// to 60 characters. Then End run, as G2. Ignored and gated as G2 is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_real_claude_takes_its_settings_from_the_app() {
        let prompt = "Use the Bash tool to run exactly one command, then say done. The command, once: echo s7 > s7.txt && sleep 8";
        let (rig, uid, host_pid) = real_claude_run("s7b8", prompt).await;
        let card = first_card(&rig, &uid).await;
        let allowed = allow_from_app(&rig, &uid, &card).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let before = stored(&rig, &uid).len();
        let model = rig
            .daemon
            .set_setting(&uid, set_model(Some("sonnet"), None))
            .await;
        let settled = settled_after_words(&rig, &uid).await;
        let effort = rig
            .daemon
            .set_setting(&uid, set_model(None, Some("low")))
            .await;
        let mode = rig
            .daemon
            .set_setting(&uid, Setting::Mode("plan".into()))
            .await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let session = summary(&rig.daemon, &uid).await;
        let echoes: Vec<String> = stored(&rig, &uid)[before..]
            .iter()
            .filter_map(|event| {
                let content = event["payload"]["message"]["content"].as_str();
                match event["kind"].as_str() {
                    Some("user_message") => content
                        .filter(|content| content.starts_with("<local-command-std"))
                        .map(|content| content.chars().take(60).collect()),
                    Some("transcript_permission-mode") => Some(format!(
                        "status permissionMode={}",
                        event["payload"]["permissionMode"]
                    )),
                    _ => None,
                }
            })
            .collect();
        println!("B8P2 S7: card {card:?}; allowed {allowed:?}; turn settled {settled}");
        println!("B8P2 S7: model during the turn {model:?}; effort {effort:?}; mode {mode:?}");
        println!(
            "B8P2 S7: after: model {:?}; mode {:?}; effort {:?}",
            session
                .model
                .as_ref()
                .map(|fact| (&fact.value, fact.source)),
            session.permission_mode.as_ref().map(|fact| &fact.value),
            session.effort.as_ref().map(|fact| &fact.value)
        );
        println!("B8P2 S7: claude's echoes {echoes:?}");
        let (stopping, ended, host_gone, left) = end_real_run(&rig, &uid, host_pid).await;
        println!("B8P2 S7: End run {stopping:?}; ended {ended}; host gone {host_gone}; left in its group {left:?}");
        assert!(card.is_some(), "claude asked nothing");
        assert!(settled, "the turn did not settle");
        assert_eq!(
            model,
            SettingResult::Applied {
                applies: Applies::NextTurn
            }
        );
        for (name, result) in [("effort", &effort), ("mode", &mode)] {
            assert_eq!(
                result,
                &SettingResult::Applied {
                    applies: Applies::Now
                },
                "{name}"
            );
        }
        assert_eq!(
            session.effort.map(|fact| fact.value).as_deref(),
            Some("low")
        );
        assert_eq!(
            session.permission_mode.map(|fact| fact.value).as_deref(),
            Some("plan")
        );
        assert!(
            session
                .model
                .is_some_and(|fact| fact.value.contains("sonnet")),
            "the model claude reports"
        );
        assert_eq!(stopping, StopRunResult::Stopping);
        assert!(
            ended && host_gone && left.is_empty(),
            "ended {ended}, host gone {host_gone}, left in its group {left:?}"
        );
    }

    /// **B7 part 2 S11: a real claude's turn stopped from the app** (app-qa's rig). Turn 1:
    /// claude runs `sleep 20` (a command that only reads, so no card); 3 s after its call the
    /// person sends a message, and 3 s later the app's Stop. The Stop is `aborted`, the message
    /// reads not sent, and claude runs no further turn. Turn 2: the person asks for a command
    /// that writes; its card is left open, and the app's Stop closes it saying the turn was
    /// stopped, and is `aborted`. Then End run, as G2. Ignored and gated as G2 is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_real_claude_turn_is_stopped_from_the_app() {
        let prompt =
            "Use the Bash tool to run exactly this command, once: sleep 20. Then say done.";
        let (rig, uid, host_pid) = real_claude_run("s11", prompt).await;
        let first = turn_after(&rig, &uid, 0).await;
        let called = match &first {
            Some(turn) => tool_called(&rig, &uid, turn).await,
            None => false,
        };
        tokio::time::sleep(Duration::from_secs(3)).await;
        let sent = send_as_app(&rig.daemon, &uid, "s11-message", "Also say hello.").await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let stopped = match &first {
            Some(turn) => Some(stop_as_app(&rig.daemon, &uid, "s11-stop-1", turn).await),
            None => None,
        };
        let ended = match &first {
            Some(turn) => turn_ended(&rig, &uid, turn).await,
            None => false,
        };
        // Long enough for a queued message to start a turn of its own, as it does without
        // `cancel_queued`.
        tokio::time::sleep(Duration::from_secs(10)).await;
        let starts = stored(&rig, &uid)
            .iter()
            .filter(|event| event["kind"] == "session_start")
            .count();
        let not_sent = not_sent_notices(&rig, &uid);
        println!(
            "S11: turn 1 {first:?}; tool called {called}; message {sent:?}; Stop {stopped:?}; turn ended {ended}; turns {starts}; not sent {not_sent:?}"
        );
        let asked = send_as_app(
            &rig.daemon,
            &uid,
            "s11-ask",
            "Use the Bash tool to run exactly this command, once: touch s11.txt",
        )
        .await;
        let second = turn_after(&rig, &uid, starts).await;
        let mut card = None;
        for _ in 0..1200 {
            card = cards(&rig.daemon, &uid).await.1.into_iter().next();
            if card.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let stopped_again = match &second {
            Some(turn) => Some(stop_as_app(&rig.daemon, &uid, "s11-stop-2", turn).await),
            None => None,
        };
        tokio::time::sleep(Duration::from_secs(5)).await;
        let detail = card.as_ref().and_then(|request_id| {
            stored(&rig, &uid)
                .into_iter()
                .find(|event| event["source_event_id"] == format!("resolved:{request_id}"))
                .map(|event| event["payload"]["detail"].clone())
        });
        let open = cards(&rig.daemon, &uid).await;
        println!(
            "S11: turn 2 {second:?}; message {asked:?}; card {card:?}; Stop {stopped_again:?}; card closed with {detail:?}; open {open:?}"
        );
        let (stopping, run_ended, host_gone, left) = end_real_run(&rig, &uid, host_pid).await;
        println!(
            "S11: End run {stopping:?}; ended {run_ended}; host gone {host_gone}; left in its group {left:?}"
        );
        let turn = first.expect("claude began no turn");
        assert!(called, "claude made no tool call");
        assert!(matches!(sent, SendTextResult::Sent { .. }), "{sent:?}");
        assert_eq!(
            stopped,
            Some(protocol::ws::InterruptResult::Aborted { turn_id: turn })
        );
        assert!(ended, "the stopped turn never ended");
        assert_eq!(starts, 1, "claude ran a further turn after the Stop");
        assert_eq!(
            not_sent
                .iter()
                .map(|notice| notice["request_id"].clone())
                .collect::<Vec<_>>(),
            [serde_json::json!("s11-message")],
            "the message the Stop cancelled is not marked"
        );
        assert!(matches!(asked, SendTextResult::Sent { .. }), "{asked:?}");
        let turn = second.expect("the second message began no turn");
        assert!(card.is_some(), "claude asked nothing in the second turn");
        assert_eq!(
            stopped_again,
            Some(protocol::ws::InterruptResult::Aborted { turn_id: turn })
        );
        assert_eq!(
            detail,
            Some(serde_json::json!(
                "The turn was stopped before anyone answered."
            )),
            "the card the Stop ended"
        );
        assert_eq!(
            open,
            (Vec::<String>::new(), Vec::<String>::new()),
            "a card left open"
        );
        assert_eq!(stopping, StopRunResult::Stopping);
        assert!(
            run_ended && host_gone && left.is_empty(),
            "ended {run_ended}, host gone {host_gone}, left in its group {left:?}"
        );
    }

    /// claude's `can_use_tool` request for the Bash call `toolu_rh`, as claude writes it.
    fn claude_asks() -> String {
        serde_json::json!({"type": "control_request", "request_id": "req-toolu_rh",
            "request": {"subtype": "can_use_tool", "tool_name": "Bash",
                        "input": {"command": "ls"}, "tool_use_id": "toolu_rh"}})
        .to_string()
    }

    /// The control responses the stand-in `name` read for claude's request `toolu_rh`,
    /// waiting up to 5 s for the first.
    async fn answers_claude_read(rig: &Rig, name: &str) -> Vec<serde_json::Value> {
        let read = || -> Vec<serde_json::Value> {
            std::fs::read_to_string(rig.root.join(format!("stub-{name}.log")))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter_map(|entry| entry.get("stdin").cloned())
                .filter(|message| {
                    message["type"] == "control_response"
                        && message["response"]["request_id"] == "req-toolu_rh"
                })
                .collect()
        };
        for _ in 0..100 {
            let answers = read();
            if !answers.is_empty() {
                return answers;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        read()
    }

    /// Whether the run `uid` lists the card `toolu_rh`, waiting up to 5 s.
    async fn carded(daemon: &Daemon, uid: &str) -> bool {
        for _ in 0..100 {
            if cards(daemon, uid).await.1 == ["toolu_rh"] {
                return true;
            }
            tokio::time::sleep(POLL).await;
        }
        false
    }

    /// **B6 G2 on the real host.** The stand-in claude asks permission after the first
    /// message; the real host holds the request and its card reaches the apps; the app's
    /// Allow reaches claude's input once, as the SDK's own allow with the call's input, and
    /// is confirmed. Ignored and gated: `CODECONNECT_TEST_AGENT_HOST` names the built host.
    #[tokio::test]
    #[ignore = "needs the built agent host: CODECONNECT_TEST_AGENT_HOST=<path>"]
    async fn a_real_hosts_permission_request_is_answered_from_the_app() {
        let (host, rig) = real_host_rig();
        let cwd = rig.root.to_str().unwrap().to_string();
        let (claude, _) = stand_in_claude(
            &rig,
            &host,
            "perm",
            serde_json::json!({"after_user": [claude_asks()]}),
        );
        let started = start(
            &rig.daemon,
            Run {
                claude_path: claude.to_str().unwrap(),
                ..run(&rig, "perm", &cwd, &host)
            },
        )
        .await;
        let uid = started_uid(&started);
        let host_pid = read_host_file(&run_dir(&rig.root, &uid).join("host.json"))
            .unwrap()
            .host_pid;
        let carded = carded(&rig.daemon, &uid).await;
        let result = rig
            .daemon
            .answer(
                "toolu_rh",
                &bash_hash(),
                AnswerDecision::Allow,
                Some(&uid),
                AnsweredOn::Mac,
            )
            .await;
        let read = answers_claude_read(&rig, "perm").await;
        // SAFETY: kill(2) of the process group this test's host leads.
        unsafe { libc::kill(-host_pid, libc::SIGTERM) };
        assert!(carded, "no card");
        let AnswerResult::Applied { outcome } = result else {
            panic!("not applied: {result:?}");
        };
        assert!(!outcome.indeterminate);
        assert_eq!(read.len(), 1, "claude read the answer once: {read:?}");
        assert_eq!(read[0]["response"]["response"]["behavior"], "allow");
        assert_eq!(
            read[0]["response"]["response"]["updatedInput"],
            serde_json::json!({"command": "ls"})
        );
    }

    /// **B6 G5 on the real host: a card across a ccd restart.** The run's first ccd stops
    /// with the card open and an answer claimed that never reached the host. The restarted
    /// ccd, on the same store, attaches the real host again; the host's `state` still holds
    /// the request, so the claim is released and the card answerable; the app's Allow then
    /// reaches claude's input once. Ignored and gated as G2 on the real host is.
    #[test]
    #[ignore = "needs the built agent host: CODECONNECT_TEST_AGENT_HOST=<path>"]
    fn a_card_open_across_a_real_restart_is_answered_once_after_it() {
        let (host, rig) = real_host_rig();
        let cwd = rig.root.to_str().unwrap().to_string();
        let (claude, _) = stand_in_claude(
            &rig,
            &host,
            "restart",
            serde_json::json!({"after_user": [claude_asks()]}),
        );
        let first = tokio::runtime::Runtime::new().unwrap();
        let (uid, host_pid, carded) = first.block_on(async {
            let daemon = daemon_on(&rig.root);
            let started = start(
                &daemon,
                Run {
                    claude_path: claude.to_str().unwrap(),
                    ..run(&rig, "restart", &cwd, &host)
                },
            )
            .await;
            let uid = started_uid(&started);
            let host_pid = read_host_file(&run_dir(&rig.root, &uid).join("host.json"))
                .unwrap()
                .host_pid;
            let carded = carded(&daemon, &uid).await;
            daemon
                .db
                .claim_answer(crate::store::AnswerClaim {
                    session_uid: uid.clone(),
                    session_id: uid.clone(),
                    request_id: "toolu_rh".into(),
                    payload_hash: bash_hash(),
                    decision: serde_json::to_string(&AnswerDecision::Allow).unwrap(),
                    started_at: protocol::time::now_rfc3339(),
                })
                .await
                .unwrap();
            (uid, host_pid, carded)
        });
        drop(first);
        let second = tokio::runtime::Runtime::new().unwrap();
        let (attached, result, read) = second.block_on(async {
            let daemon = daemon_on(&rig.root);
            daemon.recover().await;
            daemon.recover_hosted_runs(&rig.root).await;
            let mut attached = false;
            for _ in 0..100 {
                if summary(&daemon, &uid).await.link == protocol::event::Link::Attached {
                    attached = true;
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
            // The host's `state` ends the replay and releases the claim.
            let mut result = None;
            for _ in 0..100 {
                let answer = daemon
                    .answer(
                        "toolu_rh",
                        &bash_hash(),
                        AnswerDecision::Allow,
                        Some(&uid),
                        AnsweredOn::Mac,
                    )
                    .await;
                if matches!(answer, AnswerResult::Applied { .. }) {
                    result = Some(answer);
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
            (attached, result, answers_claude_read(&rig, "restart").await)
        });
        // SAFETY: kill(2) of the process group this test's host leads.
        unsafe { libc::kill(-host_pid, libc::SIGTERM) };
        assert!(carded, "no card before the restart");
        assert!(
            attached,
            "the restarted daemon never attached the host again"
        );
        let Some(AnswerResult::Applied { outcome }) = result else {
            panic!("never applied after the restart");
        };
        assert!(!outcome.indeterminate);
        assert_eq!(read.len(), 1, "claude read the answer once: {read:?}");
        assert_eq!(read[0]["response"]["response"]["behavior"], "allow");
    }

    /// Against the real agent host (mac/agent-host/build.sh's `cc-agent-host`), through
    /// zsh's login shell (`-lic`), as a zsh user's start runs it, with the host's
    /// stand-in claude: the recorded pid and birth are the host's own, it leads a session
    /// of its own, the start's outcome makes the run `live` with the initialize answer's
    /// models kept and none of the claude.ai connectors' commands (G5, the initialize
    /// column: the answer carries one, which the host leaves out), claude's init is filed after the person's first words (G8), and the host is
    /// still running after its 30 s no-start exit would have ended it (claude keeps
    /// running, so the host does). A start
    /// whose claude is missing is refused, naming it, and its run is `failed`. So is one
    /// whose claude starts and dies at initialize, or writes a line and then refuses it,
    /// and its host then ends, its claude's end acknowledged. A claude that writes a line
    /// before its initialize answer still
    /// gives a started, live run: the host sends the outcome first. Ignored and gated:
    /// `CODECONNECT_TEST_AGENT_HOST` names the built host.
    #[tokio::test]
    #[ignore = "needs the built agent host: CODECONNECT_TEST_AGENT_HOST=<path>"]
    async fn the_real_host_is_admitted_attached_and_kept_by_start() {
        let host = PathBuf::from(
            std::env::var("CODECONNECT_TEST_AGENT_HOST")
                .expect("CODECONNECT_TEST_AGENT_HOST names the built agent host"),
        );
        let mut rig = rig();
        rig.environment
            .insert(OsString::from("SHELL"), OsString::from("/bin/zsh"));
        let cwd = rig.root.to_str().unwrap().to_string();
        // The recorded initialize answer, with one made-up row of a claude.ai connector's
        // command, as an account with connectors lists it.
        let mut connected: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/claude/initialize-api-key-2.1.319.jsonl"),
            )
            .unwrap()
            .lines()
            .next()
            .unwrap(),
        )
        .unwrap();
        let recorded = connected["response"]["response"]["commands"].clone();
        connected["response"]["response"]["commands"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"name": "claude.ai Example:prompt (MCP)",
                                     "description": "a connected service", "argumentHint": ""}));
        let (claude, models) = stand_in_claude(
            &rig,
            &host,
            "ready",
            serde_json::json!({"init": connected.to_string()}),
        );
        let session = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/claude/sdk-session-0.3.322.jsonl"),
        )
        .unwrap();
        let queued = session.lines().next().unwrap().to_string();
        // A claude that starts and refuses its initialize: one that dies at it, and one
        // that first writes the first message's own line, then answers with an error.
        // Each start is refused, its run `failed`, and its host ends once claude's end is
        // acknowledged.
        for (name, refusal, request) in [
            ("dying", serde_json::json!({"die_on_init": true}), "req-2"),
            (
                "refusing-early",
                serde_json::json!({"before_init": [queued.clone()], "init_error": true}),
                "req-4",
            ),
        ] {
            let (refusing, _) = stand_in_claude(&rig, &host, name, refusal);
            let refused = start(
                &rig.daemon,
                Run {
                    claude_path: refusing.to_str().unwrap(),
                    ..run(&rig, request, &cwd, &host)
                },
            )
            .await;
            let row = rig
                .daemon
                .db
                .hosted_run_for_request(request.into())
                .await
                .unwrap()
                .unwrap();
            let refused_host =
                read_host_file(&run_dir(&rig.root, &row.session_uid).join("host.json"))
                    .unwrap()
                    .host_pid;
            let mut ended = false;
            for _ in 0..100 {
                // SAFETY: kill(2) with signal 0 only asks whether the process exists.
                if unsafe { libc::kill(refused_host, 0) } != 0 {
                    ended = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !ended {
                // SAFETY: kill(2) of the process group this test's host leads.
                unsafe { libc::kill(-refused_host, libc::SIGTERM) };
            }
            assert!(
                matches!(&refused, StartSessionResult::Failed { .. }),
                "{name}: {refused:?}"
            );
            assert_eq!(row.lifecycle, Lifecycle::Failed, "{name}");
            assert!(
                ended,
                "{name}: a refused start's host did not end within 10 s"
            );
        }
        let missing = start(&rig.daemon, run(&rig, "req-0", &cwd, &host)).await;
        assert!(
            matches!(&missing, StartSessionResult::Failed { detail, .. } if detail.contains("/nonexistent/claude")),
            "{missing:?}"
        );
        let refused = rig
            .daemon
            .db
            .hosted_run_for_request("req-0".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(refused.lifecycle, Lifecycle::Failed);
        let result = start(
            &rig.daemon,
            Run {
                claude_path: claude.to_str().unwrap(),
                ..run(&rig, "req-1", &cwd, &host)
            },
        )
        .await;
        let uid = started_uid(&result);
        assert!(
            matches!(&result, StartSessionResult::Started { cwd: Some(folder), .. } if *folder == cwd),
            "{result:?}"
        );
        let (lifecycle, initialize) = kept(&rig, &uid);
        assert_eq!(lifecycle, "live");
        let initialize = initialize.unwrap();
        assert_eq!(initialize["models"], models);
        let stored_commands = initialize["commands"].clone();
        let record = read_host_file(&run_dir(&rig.root, &uid).join("host.json")).unwrap();
        let pid = record.host_pid;
        let stored: (i32, i64, i64) = rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .query_row(
                "SELECT host_pid, host_start_sec, host_start_usec FROM run_hosts
                  WHERE session_uid = ?1",
                [&uid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let birth = read_birth_identity(pid).unwrap();
        assert_eq!(stored, (pid, birth.start_sec, birth.start_usec));
        // The stand-in writes the recorded session's init after the first message: the
        // host relays it once the outcome is committed, and it is filed, then acked.
        let session_init: serde_json::Value = capture()
            .into_iter()
            .find(|message| message["subtype"] == "init")
            .unwrap();
        let mut filed = Vec::new();
        for _ in 0..100 {
            filed = after_words(&rig, &uid);
            if !filed.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let committed = cursor(&rig, &uid);
        // SAFETY: getsid(2) and kill(2) with signal 0 only read one process's state.
        assert_eq!(unsafe { libc::getsid(pid) }, pid);
        tokio::time::sleep(Duration::from_secs(31)).await;
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        // SAFETY: kill(2) of the process group this test's host leads (its recorded pid),
        // which holds the host and its stand-in claude.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        // Checked once the host is stopped, so a failing check leaves no host behind.
        assert_eq!(
            stored_commands, recorded,
            "the stored commands are the recorded ones, the connector's left out"
        );
        assert_eq!(
            filed.len(),
            1,
            "after the person's words, claude's init is the run's one event"
        );
        assert_eq!(filed[0]["kind"], "session_start");
        assert_eq!(filed[0]["source"], "sdk");
        assert_eq!(filed[0]["payload"]["model"], session_init["model"]);
        assert_eq!(filed[0]["turn_id"], session_init["uuid"]);
        assert_eq!(filed[0]["item_id"], session_init["uuid"]);
        assert_eq!(
            committed, 3,
            "the state, the outcome and the init are committed"
        );

        // Claude's own lines before its initialize answer, as a slow answer lets the
        // first message's lines come first: the host sends the start's outcome before
        // any of them, so the start is answered started and the run is live.
        let (early, _) = stand_in_claude(
            &rig,
            &host,
            "early",
            serde_json::json!({"before_init": [queued]}),
        );
        let early_start = start(
            &rig.daemon,
            Run {
                claude_path: early.to_str().unwrap(),
                ..run(&rig, "req-3", &cwd, &host)
            },
        )
        .await;
        let early_run = rig
            .daemon
            .db
            .hosted_run_for_request("req-3".into())
            .await
            .unwrap()
            .unwrap();
        let early_kept = kept(&rig, &early_run.session_uid).0;
        let early_host =
            read_host_file(&run_dir(&rig.root, &early_run.session_uid).join("host.json"))
                .unwrap()
                .host_pid;
        // SAFETY: kill(2) of the process group this test's early host leads.
        unsafe { libc::kill(-early_host, libc::SIGTERM) };
        assert!(alive, "the host ended within 31 s: start never reached it");
        assert!(
            matches!(&early_start, StartSessionResult::Started { .. }),
            "a line before the outcome: {early_start:?}"
        );
        assert_eq!(early_kept, "live", "a line before the outcome");
    }
}
