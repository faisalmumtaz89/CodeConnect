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
//! app ends a live run ([`stop_run`]): it is `stopping`, its host is sent `stop` if its link
//! is attached, and a host that has not exited 10 s later is stopped by its group
//! ([`escalate_stop`]).
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
use protocol::event::{Lifecycle, SessionKey};
use protocol::host::{
    FromHost, HostFile, McpCommand, OutcomeStatus, StartOptions, ToHost, Trust,
    HOST_PROTOCOL_VERSION,
};
use protocol::proc_identity::{
    group_has_members, group_warrant, is_zombie, liveness, read_birth_identity, BootIdentity,
    GroupWarrant, Liveness, ProcessIdentity,
};
use protocol::sdk_map::Signal;
use protocol::ws::{StartFailure, StartSessionResult, StopRunResult};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::state::Daemon;
use crate::store::{HostedRun, SessionRow};

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
        daemon, uid, start_uuid, start, &dir, spawned, &mut child, deadline,
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
    let start_uuid = match &start {
        Some(ToHost::Start { uuid, .. }) => uuid.clone(),
        _ => String::new(),
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
        // Not trusted, whatever the folder's answer: that answer decides only whether
        // the start goes on (start_session's folder question).
        trust: Trust::NotTrusted,
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
/// mapped, so the init's own events carry their turn.
async fn apply_sdk(
    daemon: &Daemon,
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
    let (mut session_id, mut cwd) = (None, None);
    for signal in mapped.signals {
        match signal {
            Signal::SessionId { id } => session_id = Some(id),
            Signal::Cwd { path } => cwd = Some(path),
            // Deliveries and command lifecycles settle sends (B5); `/clear` keeps the
            // session id, which the next init names.
            Signal::Delivered { .. } | Signal::Lifecycle { .. } | Signal::Reset { .. } => {}
        }
    }
    match daemon
        .ingest_host_message(uid, host, seq, mapped.events, session_id, cwd)
        .await
    {
        Ok(true) => send(write, &ToHost::Ack { seq }).await,
        Ok(false) => Err(format!(
            "message {seq} is not past the run's cursor for this host"
        )),
        Err(err) => Err(format!("{err:#}")),
    }
}

/// What a link knows of its run when it begins: from the start that spawned the host, or,
/// after a restart, from the store.
struct Linked {
    /// The uuid of the run's `start`, whose outcome makes the run `live` or `failed`.
    start_uuid: String,
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
    daemon.host_link_attached(&uid, Some(to_link)).await;
    let Linked {
        start_uuid,
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
            Ok(FromHost::State { seq, .. }) => {
                if let Err(why) = commit_and_ack(&daemon, &uid, host, seq, &mut write).await {
                    break why;
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
                match daemon
                    .db
                    .commit_outcome(uid.clone(), host, seq, lifecycle, initialize)
                    .await
                {
                    Ok(true) => {
                        live = lifecycle == Lifecycle::Live;
                        refused = lifecycle == Lifecycle::Failed;
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

/// End the hosted Claude run `uid` ([`protocol::ws::ClientMessage::StopRun`], H5): a `live`
/// run becomes `stopping`, its link sends the host `stop`, and [`escalate_stop`] follows.
/// One that is stopping is told so again and nothing more is sent; one that has ended is
/// left as it is. A run still starting is refused: its start's own outcome decides it.
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
    match protocol::uid::uuid_v4() {
        Ok(uuid) => {
            // A host not attached is stopped by the escalation.
            daemon.send_to_host(uid, ToHost::Stop { uuid }).await;
        }
        Err(err) => crate::log_error!("{uid}: could not name its stop: {err}"),
    }
    tokio::spawn(escalate_stop(Arc::clone(daemon), uid.to_string()));
    StopRunResult::Stopping
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
            "models": [{"value": "opus"}],
            "account": {"api_provider": "firstParty"}
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
                assert_eq!(options.trust, Trust::NotTrusted);
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

        // The host sends something this daemon does not apply yet, then a state: neither
        // is acknowledged and the cursor stays, so the host keeps both.
        say(
            &mut write,
            serde_json::json!({"type": "withdrawn", "seq": 3, "tool_use_id": "toolu_1"}),
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
    /// command's `queued`, files nothing. The run's events are the mapper's, each turn's
    /// carrying its init's uuid, each broadcast once and in order. The session id stays
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
                assert!(stored(&rig, &played.uid).is_empty(), "the queued line");
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
            serde_json::Value::Array(stored(&rig, &played.uid)),
            committed
        );
        let mut seqs = Vec::new();
        while let Ok(event) = broadcast.try_recv() {
            if event.session_uid == played.uid {
                seqs.push(event.seq);
            }
        }
        assert_eq!(seqs, (1..=109).collect::<Vec<u64>>(), "each once, in order");

        let last = messages.len() as u64 + 2;
        sdk(&mut played.write, last, messages.last().unwrap().clone()).await;
        assert_eq!(next(&mut played.lines).await, None);
        assert_eq!(stored(&rig, &played.uid).len(), 109);
        assert_eq!(cursor(&rig, &played.uid), last as i64);
        stop(&played.dir);
    }

    /// **G1's injected failure:** a result's second event cannot be stored (a test
    /// trigger aborts its insert). Its first event, already inserted, goes too; the
    /// cursor stays, nothing is broadcast, and the message is not acknowledged.
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
        assert!(stored(&rig, &played.uid).is_empty());
        assert_eq!(cursor(&rig, &played.uid), 2);
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
        assert!(stored(&rig, &played.uid).is_empty());
        assert_eq!(cursor(&rig, &played.uid), 2);
        assert_eq!(row_of(&rig, &played.uid).0, None);
        stop(&played.dir);
    }

    /// **G2b:** an SDK message is applied only in a live run. Before the start's outcome,
    /// and after a refusal, the host sends none (the outcome first; nothing but claude's
    /// end after a refusal), and one that came would file nothing and not be
    /// acknowledged.
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
            stop(&played.dir);
            played.uid
        });
        assert!(stored(&rig, &uid).is_empty());
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
        assert!(stored(&rig, &uid).is_empty());
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
        let mode = protocol::event::Fact {
            value: capture()[2]["permissionMode"].as_str().unwrap().to_string(),
            seq: 1,
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
        (BufReader::new(read).lines(), write)
    }

    /// A run's row and its stored start, as a start writes them before anything is
    /// spawned: what a ccd that died then leaves.
    async fn begun(rig: &Rig, cwd: &str, host: &Path) -> (String, ToHost) {
        let uid = protocol::uid::new().unwrap();
        let start = ToHost::Start {
            uuid: protocol::uid::uuid_v4().unwrap(),
            options: start_options(&uid, &run(rig, "req-1", cwd, host)),
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
        let mut expected: Vec<String> = capture()
            .iter()
            .take(3)
            .flat_map(|message| protocol::sdk_map::map(&key, None, message).events)
            .map(|event| event.kind.as_str().to_string())
            .collect();
        expected.push("session_end".into());
        let filed: Vec<String> = stored(&rig, &uid)
            .iter()
            .map(|event| event["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(filed, expected);
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
    /// for the socket, attaches, and at the host's first `state` sends the stored start.
    #[tokio::test]
    async fn a_restarted_daemon_sends_a_start_its_host_never_had() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (uid, start) = begun(&rig, &cwd, &host).await;
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
        assert_eq!(next(&mut lines).await, Some(start));
    }

    /// **G3: a run with no host.** A ccd died after the run's row and before it recorded the
    /// run's host, so no start was sent: the restarted ccd fails the run.
    #[tokio::test]
    async fn a_starting_run_with_no_recorded_host_fails_at_a_restart() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (uid, _) = begun(&rig, &cwd, &host).await;
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
        let continued = stored(&rig, &uid).split_off(before);
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
            filed.push(stored(&rig, &played.uid).len());
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
    /// message this daemon does not apply yet. Its folder and uid.
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
            serde_json::json!({"type": "withdrawn", "seq": 3, "tool_use_id": "toolu_1"}),
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
        let (starting, _) = begun(&rig, &cwd, &host).await;
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
            .filter(|event| event["kind"] != "session_end")
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
    /// each checked first: the run's uid, which is also its name, and each event's clock
    /// read (`ts`).
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
        let mut wire = serde_json::to_value(&events).unwrap();
        for event in wire.as_array_mut().unwrap() {
            assert_eq!(event["session_uid"], played.uid.as_str());
            assert_eq!(event["session_id"], played.uid.as_str());
            assert!(
                protocol::time::unix_ms_from_rfc3339(event["ts"].as_str().unwrap()).is_some(),
                "ts is a clock read: {event}"
            );
            event["session_uid"] = serde_json::json!("01K1B3XQ8ZC0DE5FGH7JKMNPQR");
            event["session_id"] = serde_json::json!("01K1B3XQ8ZC0DE5FGH7JKMNPQR");
            event["ts"] = serde_json::json!("2026-11-13T12:00:00.000Z");
        }
        wire
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

    /// Whether `pid` has stopped running, waiting up to `within` for it.
    async fn gone_within(pid: i32, within: Duration) -> bool {
        let until = Instant::now() + within;
        while Instant::now() < until {
            // SAFETY: kill(2) with signal 0 only asks whether the process exists.
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
        let (starting, _) = begun(&rig, &cwd, &host).await;
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

    /// Against the real agent host (mac/agent-host/build.sh's `cc-agent-host`), through
    /// zsh's login shell (`-lic`), as a zsh user's start runs it, with the host's
    /// stand-in claude: the recorded pid and birth are the host's own, it leads a session
    /// of its own, the start's outcome makes the run `live` with the initialize answer's
    /// models kept and none of the claude.ai connectors' commands (G5, the initialize
    /// column: the answer carries one, which the host leaves out), claude's init is filed as the run's first event (G8), and the host is
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
            filed = self::stored(&rig, &uid);
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
        assert_eq!(filed.len(), 1, "claude's init is the run's one event");
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
