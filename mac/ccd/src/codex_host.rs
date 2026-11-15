//! A Codex run's app-server, from ccd's side (P3 L1-L5, L9, R1, R5).
//!
//! ccd writes the run's row first, with the app's start request id, so a repeated
//! request finds the run instead of starting another (H1, as for a Claude run). Then it
//! makes the run's folder `<root>/runs/<uid>/` (0700) and launches the codex the start's
//! check found as `codex -c projects=… app-server --listen unix://<run dir>/as.sock`,
//! through the user's login shell, in a session of its own so it outlives ccd. A
//! `/bin/sh` line in front of codex claims the run first: from the run's folder it writes
//! its own pid to `as.pid` with noclobber, so only one process ever claims a run, then
//! becomes codex. The pid ccd spawned is that one process throughout, the login shell,
//! the claim and codex in turn: the run's identity is that pid and the birth time the
//! kernel gives it, recorded at once, while ccd still holds the child. Its process group,
//! whose id is that pid, holds the app-server whether codex runs it itself or as the
//! npm launcher's child. ccd then connects over `as.sock` and initializes, and the start
//! answers `started`.
//!
//! The run's first message is sent by the run's own task, after the answer, on the same
//! connection ([`first_send`]): `thread/start` with the run's preset, model and thread
//! config, the thread recorded on the row, then `turn/start` with the first message under
//! the start's uuid, and the run is `live`. The thread config keeps the project out (P3
//! L4): each folder from the repository's top down to the run's own (or the run's folder
//! alone outside git) is untrusted, the repository's own skills are disabled, and the
//! only MCP server added is CodeConnect's mail server. A start that fails once the
//! app-server is launched, before or after the answer, marks the run `failed` and stops
//! its process group with the ladder ([`crate::claude_host::stop_group`]): an app-server
//! never ends by itself. A start that a restarted ccd finds still `starting` is failed the
//! same way, whatever became of it ([`crate::state::Daemon::recover_hosted_runs`]): a
//! start is never sent again.
//!
//! The start's connection is then kept as the run's link ([`link`], P3 R1): every
//! notification it brings is mapped by the Codex adapter and filed, for both apps. When a
//! ccd restarts, each `live` run whose app-server is alive is connected again
//! ([`reconnect`]): its thread resumed with the preset and its thread config, the turns it
//! ran meanwhile filed from the answer, and its link kept from there. An app's Stop and End
//! run reach the run through its link too ([`LinkCommand`]).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::agent::AgentKind;
use protocol::event::{Lifecycle, PendingEvent, SessionKey};
use protocol::proc_identity::{
    group_has_members, group_warrant, liveness, read_birth_identity, BootIdentity, GroupWarrant,
    Liveness, ProcessIdentity,
};
use protocol::ws::{StartFailure, StartSessionResult};
use serde_json::{json, Value};
use tokio::time::Instant;

use crate::codex_adapter::CodexAdapter;
use crate::codex_approval::{Approval, Family};
use crate::codex_client::{CallError, Client};
use crate::codex_link::{AnswerReport, AnswerRequest, InterruptReport};
use crate::state::{Daemon, HostLink, Retirement};
use crate::store::{AnswerStatus, AnswerTerminal, LiveCodexRun, SessionRow};
use protocol::composite_id::{CompositeId, ServerRequestId};
use protocol::ws::{AnswerDecision, ClearCause, CodexResolution, ResolutionActor};

/// How often the start looks for the app-server's socket.
const POLL: Duration = Duration::from_millis(50);

/// The line in front of codex, run by `/bin/sh` as `$0` = its name, `$1` = the run's
/// folder, and codex's own argv after it: claim the run with this shell's pid, which
/// noclobber lets only one writer do, then become codex.
const CLAIM: &str = r#"cd "$1" || exit 1; set -C; echo $$ > as.pid || exit 1; shift; exec "$@""#;

/// One Codex start, as the app asked for it and the checks before the row resolved it.
pub(crate) struct Run<'a> {
    /// This daemon's state folder, which holds `runs/`.
    pub root: &'a Path,
    pub request_id: &'a str,
    /// The folder the run starts in.
    pub cwd: &'a str,
    /// The run's first message.
    pub prompt: &'a str,
    /// The app's model. Codex's own `default` is sent as none, so the thread starts with
    /// the model its settings name.
    pub model: Option<&'a str>,
    /// The codex the start's check found.
    pub codex: &'a Path,
    /// The `codeconnect` binary, which runs the run's mail server.
    pub codeconnect: &'a Path,
    /// The git the trust list's repository read runs.
    pub git_bin: Option<&'a str>,
    /// The start's environment, the one its codex check read: codex gets it as it is,
    /// with only `CODECONNECT_SESSION_UID` added.
    pub environment: &'a HashMap<OsString, OsString>,
}

/// What the run's task needs for the first message, owned: it outlives the start.
struct FirstSend {
    uid: String,
    /// The start's uuid, minted with the uid: the first message's `clientUserMessageId`.
    start_uuid: String,
    cwd: String,
    prompt: String,
    model: Option<String>,
    codeconnect: String,
    /// The trust list's folders, top down.
    folders: Vec<PathBuf>,
    /// The recorded leader and boot, for the ladder if the first send fails.
    leader: ProcessIdentity,
    boot: Option<BootIdentity>,
    /// The start's own deadline.
    deadline: Instant,
}

/// Start `run`: its row, its folder, its app-server, the connection and `initialize`, then
/// `started`, with the first message left to the run's own task ([`first_send`]).
pub(crate) async fn start(daemon: &Arc<Daemon>, run: Run<'_>) -> StartSessionResult {
    start_within(daemon, run, crate::start_session::START_TIMEOUT).await
}

/// [`start`], with `timeout` from the launch to the first message's answer: the tests' way
/// to a deadline shorter than START_TIMEOUT.
async fn start_within(daemon: &Arc<Daemon>, run: Run<'_>, timeout: Duration) -> StartSessionResult {
    let folders = match folders(run.cwd, run.git_bin).await {
        Ok(folders) => folders,
        Err(why) => return failed(why),
    };
    // The run and its first message are named together, before the row, as a Claude run's.
    let (uid, start_uuid) =
        match protocol::uid::new().and_then(|uid| Ok((uid, protocol::uid::uuid_v4()?))) {
            Ok(names) => names,
            Err(err) => return failed(format!("could not name the run: {err}")),
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
        agent: AgentKind::Codex,
        codex_thread_id: None,
        codex_socket: None,
    };
    let model = run.model.filter(|model| *model != "default");
    match daemon
        .db
        .begin_run(row, run.request_id.to_string(), None)
        .await
    {
        Ok(Some(existing)) => return crate::claude_host::answer_for(existing),
        Ok(None) => {}
        Err(err) => return failed(format!("could not record the run: {err:#}")),
    }
    let deadline = Instant::now() + timeout;
    let mut launched = None;
    match bring_up(daemon, &uid, &run, &folders, deadline, &mut launched).await {
        Ok(client) => {
            let (leader, boot) = launched.expect("an app-server that answered was recorded");
            let send = FirstSend {
                uid: uid.clone(),
                start_uuid,
                cwd: run.cwd.to_string(),
                prompt: run.prompt.to_string(),
                model: model.map(str::to_string),
                codeconnect: run.codeconnect.to_string_lossy().into_owned(),
                folders,
                leader,
                boot,
                deadline,
            };
            tokio::spawn(first_send(Arc::clone(daemon), client, send));
            StartSessionResult::Started {
                session_uid: uid,
                cwd: Some(run.cwd.to_string()),
            }
        }
        Err(detail) => {
            if let Err(err) = daemon
                .db
                .set_lifecycle(uid.clone(), Lifecycle::Failed)
                .await
            {
                crate::log_error!("could not mark {uid} failed: {err:#}");
            }
            if let Some((leader, boot)) = launched {
                stop(&uid, leader, boot).await;
            }
            failed(detail)
        }
    }
}

fn failed(detail: String) -> StartSessionResult {
    StartSessionResult::Failed {
        reason: StartFailure::LaunchFailed,
        detail,
    }
}

/// The ladder on a failed start's process group, under the warrant of its recorded leader.
pub(crate) async fn stop(uid: &str, leader: ProcessIdentity, boot: Option<BootIdentity>) {
    use crate::claude_host::Ladder;
    let Some(boot) = boot else {
        crate::log_info!(
            "{uid}: no boot was recorded with its app-server, so its group was not signalled"
        );
        return;
    };
    match crate::claude_host::stop_group(leader, boot).await {
        Ladder::Done => {}
        Ladder::Refused(why) => {
            crate::log_info!("{uid}: its app-server's group was not signalled: {why}")
        }
        Ladder::Deferred => crate::log_info!(
            "{uid}: its app-server is not yet reaped, so its group was not signalled"
        ),
    }
}

/// The run's own task after `started` (L9 i-v): the first message sent by the start's
/// deadline, then the run `live` and its connection kept as the run's [`link`]. A failure
/// ends the run `failed`, lets the connection go and stops its group; either way a run the
/// sweep has ended meanwhile stays as the sweep left it, unlinked.
async fn first_send(daemon: Arc<Daemon>, client: Client, send: FirstSend) {
    let uid = send.uid.clone();
    let sent = tokio::time::timeout_at(send.deadline, send_first(&daemon, &client, &send))
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "Codex did not take the first message within {} s",
                crate::start_session::START_TIMEOUT.as_secs()
            ))
        });
    let to = match &sent {
        Ok(()) => Lifecycle::Live,
        Err(why) => {
            crate::log_info!("{uid}: did not start: {why}");
            Lifecycle::Failed
        }
    };
    let advanced = match daemon
        .db
        .advance_lifecycle(uid.clone(), Lifecycle::Starting, to)
        .await
    {
        Ok(true) => true,
        Ok(false) => {
            crate::log_info!("{uid}: the run had already ended; it is left as it is");
            false
        }
        Err(err) => {
            crate::log_error!("{uid}: could not mark the run {to:?}: {err:#}");
            false
        }
    };
    if sent.is_err() {
        drop(client);
        stop(&uid, send.leader, send.boot).await;
    } else if advanced {
        let adapter = CodexAdapter::new(SessionKey::new(&uid, &uid));
        link(daemon, uid, client, adapter, None, HashMap::new()).await;
    }
}

/// A turn a resume found still running, and the resume to send once more when it ends: the
/// items it finished while no connection was subscribed are in no notification, and the
/// answer read while it ran does not describe them.
struct Owed {
    thread: String,
    turn: String,
    resume: Value,
}

/// What the daemon asks a run's link to do beside answering its cards (X8).
pub(crate) enum LinkCommand {
    /// A phone's Interrupt for the turn `turn_id`, its outcome told on `reply`
    /// ([`interrupt_turn`]).
    Interrupt {
        request_id: String,
        turn_id: String,
        reply: tokio::sync::oneshot::Sender<InterruptReport>,
    },
    /// End run's graceful step ([`stop_gracefully`]).
    Stop,
}

/// A phone's Stop that Codex has taken, waiting for its turn's end to say what became of it
/// ([`settle_stop`]).
struct WaitingStop {
    request_id: String,
    turn: String,
    reply: tokio::sync::oneshot::Sender<InterruptReport>,
}

/// How long the link's end waits for the run's app-server to exit: Codex closes its
/// connections a moment before its process ends.
const EXIT_WAIT: Duration = Duration::from_secs(5);

/// How often the link records the run's tool groups while a turn runs ([`record_tools`]).
const TOOL_SCAN: Duration = Duration::from_secs(1);

/// The run's link (P3 R1): every notification its one connection brings, in order, mapped
/// by the Codex adapter and filed ([`Daemon::ingest`]) for both apps, until the connection
/// ends; then the run is ended if its app-server is gone, as at a Claude link's end
/// ([`crate::claude_host::end_if_host_gone`]). The end of the `owed` turn brings its one
/// follow-up resume, whose answer is filed the same way.
///
/// Codex's requests come on the same connection: a command, a file change or an MCP tool
/// call to approve becomes a card ([`request`]), answered from a phone through the link's
/// answer channel ([`answer`]); any other request is answered at once with an error. A card
/// ends when Codex says the request is resolved, or when its item or turn ends ([`settle`]).
/// `cards` holds the run's cards a restart found still standing.
///
/// The daemon's other asks come on the link's command channel ([`LinkCommand`]): a phone's
/// Interrupt for the turn the link is running, and End run's graceful step. The running turn
/// is the one Codex's last `turn/started` named, or the one a resume found still running,
/// until its `turn/completed`, or until Codex answers an interrupt of it: Codex answers just
/// before the turn's end. A phone's Stop that Codex has taken waits for that end.
///
/// When the connection ends, the run is ended once its app-server's leader is gone, within
/// [`EXIT_WAIT`]: Codex closes its connections a moment before it exits.
///
/// Codex runs each command in a group of its own, which outlives the app-server, so the link
/// keeps the run's tool groups ([`record_tools`]): at attach, every [`TOOL_SCAN`] while a turn
/// runs, at each turn's end, and before ccd's own SIGTERM. Once the leader is gone, each
/// recorded group that still has members and that the warrant allows is sent SIGKILL
/// ([`end_tools`]).
async fn link(
    daemon: Arc<Daemon>,
    uid: String,
    mut client: Client,
    mut adapter: CodexAdapter,
    mut owed: Option<Owed>,
    mut cards: HashMap<String, Card>,
) {
    let (answers, mut asked) = crate::codex_link::answer_channel();
    let (commands, mut commanded) = tokio::sync::mpsc::unbounded_channel();
    daemon
        .host_link_attached(&uid, HostLink::Codex(answers, commands))
        .await;
    let session = SessionKey::new(&uid, &uid);
    let mut running = owed
        .as_ref()
        .map(|owed| (owed.thread.clone(), owed.turn.clone()));
    let mut stops: Vec<WaitingStop> = Vec::new();
    let host = daemon
        .db
        .watched_host(uid.clone())
        .await
        .ok()
        .flatten()
        .map(|watch| watch.host);
    let mut tools: Vec<ProcessIdentity> = Vec::new();
    record_tools(host.as_ref(), &mut tools);
    let mut scan = tokio::time::interval(TOOL_SCAN);
    scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = scan.tick(), if running.is_some() => record_tools(host.as_ref(), &mut tools),
            frame = client.server_frame() => {
                let Some(frame) = frame else { break };
                if frame.get("id").is_some() {
                    request(&daemon, &session, &client, &adapter, &mut cards, &frame).await;
                    continue;
                }
                let (thread, turn) = (
                    frame["params"]["threadId"].as_str(),
                    frame["params"]["turn"]["id"].as_str(),
                );
                match (frame["method"].as_str(), thread, turn) {
                    (Some("turn/started"), Some(thread), Some(turn)) => {
                        running = Some((thread.to_string(), turn.to_string()));
                    }
                    (Some("turn/completed"), _, Some(turn)) => {
                        record_tools(host.as_ref(), &mut tools);
                        if running.as_ref().is_some_and(|(_, on)| on == turn) {
                            running = None;
                        }
                        let status = frame["params"]["turn"]["status"].as_str().unwrap_or_default();
                        let (ended, waiting): (Vec<_>, Vec<_>) =
                            std::mem::take(&mut stops).into_iter().partition(|stop| stop.turn == turn);
                        stops = waiting;
                        for stop in ended {
                            settle_stop(&daemon, &uid, stop, status).await;
                        }
                    }
                    _ => {}
                }
                settle(&daemon, &session, &mut cards, &frame).await;
                let ended = (frame["method"] == "turn/completed")
                    .then(|| frame["params"]["turn"]["id"].as_str().map(str::to_string))
                    .flatten();
                file(&daemon, &uid, adapter.ingest(&frame)).await;
                let Some(due) =
                    owed.take_if(|owed| ended.as_deref() == Some(owed.turn.as_str()))
                else {
                    continue;
                };
                match tokio::time::timeout(
                    crate::start_session::START_TIMEOUT,
                    client.call("thread/resume", due.resume),
                )
                .await
                {
                    Ok(Ok(answer)) => {
                        resumed(&daemon, &uid, &mut adapter, &answer, &due.thread).await;
                    }
                    Ok(Err(err)) => crate::log_info!(
                        "{uid}: the follow-up resume after turn {} failed: {err}",
                        due.turn
                    ),
                    Err(_) => crate::log_info!(
                        "{uid}: the follow-up resume after turn {} was not answered within {} s",
                        due.turn,
                        crate::start_session::START_TIMEOUT.as_secs()
                    ),
                }
            }
            Some(ask) = asked.recv() => answer(&daemon, &uid, &client, &mut cards, ask).await,
            Some(command) = commanded.recv() => match command {
                LinkCommand::Interrupt { request_id, turn_id, reply } => {
                    let told = interrupt_turn(
                        &daemon, &uid, &client, &mut running, &stops, &request_id, &turn_id,
                    )
                    .await;
                    match told {
                        Some(report) => {
                            let _ = reply.send(report);
                        }
                        None => stops.push(WaitingStop { request_id, turn: turn_id, reply }),
                    }
                }
                LinkCommand::Stop => {
                    record_tools(host.as_ref(), &mut tools);
                    stop_gracefully(&daemon, &uid, &client, &mut running).await;
                }
            },
        }
    }
    // A Stop Codex took and whose turn's end never came: its claim stays `applying`, for
    // startup's settle.
    for stop in stops {
        let _ = stop.reply.send(InterruptReport::Unknown(
            crate::codex_refusals::interrupt_settled_unknown(
                crate::codex_refusals::INTERRUPT_CONNECTION_ENDED,
            ),
        ));
    }
    // An answer written and not yet confirmed keeps its claim `applying`: a restart's reconnect
    // sends it again if Codex replays the request, and settles it if Codex took it.
    for (_, card) in cards.drain() {
        if let Some(Answer::Phone { reply, .. }) = card.answer {
            let _ = reply.send(AnswerReport::Unknown(
                "the connection to Codex ended before it confirmed your answer; do not answer \
                 it again"
                    .into(),
            ));
        }
    }
    daemon.host_link_detached(&uid).await;
    if let Ok(Some(watch)) = daemon.db.watched_host(uid.clone()).await {
        let deadline = Instant::now() + EXIT_WAIT;
        // Until proven gone. A dying process can read `Unknown` just before and just after it
        // reads `Gone`, so the reading that proved it gone is the one acted on.
        let mut gone = liveness(&watch.host) == Liveness::Gone;
        while !gone && Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            gone = liveness(&watch.host) == Liveness::Gone;
        }
        if gone {
            end_tools(&uid, &tools, watch.boot);
        } else {
            crate::log_info!(
                "{uid}: its app-server was not seen to exit within {} s, so its commands' groups were left to it",
                EXIT_WAIT.as_secs()
            );
        }
    }
    crate::claude_host::end_if_host_gone(&daemon, &uid).await;
}

/// The group rule's one record (ruling D5), kept in the link's memory (D6): each group the
/// run's members started and lead themselves
/// ([`protocol::proc_identity::tool_groups`]) that the link does not hold yet.
fn record_tools(host: Option<&ProcessIdentity>, tools: &mut Vec<ProcessIdentity>) {
    let Some(host) = host else { return };
    for found in protocol::proc_identity::tool_groups(host.pid) {
        if !tools.contains(&found) {
            tools.push(found);
        }
    }
}

/// Once the run's app-server is gone: SIGKILL to each recorded tool group that still has a
/// member, under B3's warrant on the leader recorded for it (the same boot; the leader's pid
/// unoccupied or still its recorded birth). Nothing else is signalled, and a group the warrant
/// refuses says why in the log, as does a record that holds none.
fn end_tools(uid: &str, tools: &[ProcessIdentity], boot: Option<BootIdentity>) {
    if tools.is_empty() {
        crate::log_info!("{uid}: its app-server is gone; no group of its commands was recorded");
        return;
    }
    let Some(boot) = boot else {
        crate::log_info!(
            "{uid}: no boot was recorded with its app-server, so its commands' groups were not signalled"
        );
        return;
    };
    for tool in tools {
        match group_warrant(tool, boot) {
            GroupWarrant::Warranted if group_has_members(tool.pid) => {
                // SAFETY: kill(2) of a recorded tool group, which the warrant ties to its leader.
                unsafe { libc::kill(-tool.pid, libc::SIGKILL) };
                crate::log_info!(
                    "{uid}: its app-server is gone; the group of {} it started was ended",
                    tool.pid
                );
            }
            GroupWarrant::Warranted => {}
            GroupWarrant::Refused(why) => crate::log_info!(
                "{uid}: its app-server is gone; the group of {} it started was not signalled: {why}",
                tool.pid
            ),
        }
    }
}

/// A card the run's link raised, or took back after a restart, by its card id.
struct Card {
    /// The request's JSON-RPC id on this connection; `Null` for a card a restart found
    /// standing, until Codex replays its request.
    wire: Value,
    family: Family,
    turn: String,
    item: String,
    answer: Option<Answer>,
}

/// An answer written to a card and waiting for Codex's word.
enum Answer {
    /// A phone's, with where its outcome goes and the answer gate it holds until then.
    Phone {
        decision: AnswerDecision,
        reply: tokio::sync::oneshot::Sender<AnswerReport>,
        _gate: tokio::sync::OwnedRwLockReadGuard<()>,
    },
    /// An answer claimed before a restart (R6): sent again if Codex replays the request,
    /// settled when the card ends; nobody waits on it.
    Claimed { decision: AnswerDecision },
}

/// **Decision 175, where this engine builds its card (owner decision 198): the only answers
/// a card offers.** Never Codex's permanent "don't ask again for commands that start with …"
/// (`acceptWithExecpolicyAmendment`), nor any other decision that writes Codex's settings.
const OFFERED_DECISIONS: [&str; 4] = ["accept", "acceptForSession", "decline", "cancel"];

/// A card's id (X7's row): the run, its thread and the request's JSON-RPC id, which a
/// replay keeps (P7-2).
fn card_id(uid: &str, thread: &str, wire: &Value) -> Option<String> {
    let server_request_id = match wire {
        Value::Number(number) => ServerRequestId::Number(number.as_i64()?),
        Value::String(text) => ServerRequestId::Text(text.clone()),
        _ => return None,
    };
    CompositeId {
        session_uid: uid.to_string(),
        thread_id: thread.to_string(),
        server_request_id,
        generation: 0,
    }
    .encode()
    .ok()
}

/// One of Codex's requests: a card for a command, a file change or an MCP tool call to
/// approve, and an error answer for anything else, so no turn waits on a request nobody
/// will answer. A request whose card a restart found standing is bound to it again, and an
/// answer claimed for it before the restart is sent once more (R6).
async fn request(
    daemon: &Arc<Daemon>,
    session: &SessionKey,
    client: &Client,
    adapter: &CodexAdapter,
    cards: &mut HashMap<String, Card>,
    frame: &Value,
) {
    let uid = &session.uid;
    let (method, params, wire) = (
        frame["method"].as_str().unwrap_or_default(),
        &frame["params"],
        &frame["id"],
    );
    let id = crate::codex_approval::card_family(method, params).and_then(|family| {
        let thread = params["threadId"].as_str()?;
        Some((family, card_id(uid, thread, wire)?))
    });
    let Some((family, id)) = id else {
        crate::log_info!("{uid}: Codex asked {method}, which has no card; answered with an error");
        let _ = client
            .refuse(wire, -32601, "CodeConnect does not answer this request")
            .await;
        return;
    };
    if let Some(card) = cards.get_mut(&id) {
        card.wire = wire.clone();
        resend(daemon, session, client, &id, card).await;
        return;
    }
    let started = params["itemId"].as_str().and_then(|item| {
        adapter.open_item(
            params["threadId"].as_str()?,
            params["turnId"].as_str()?,
            item,
        )
    });
    let mut approval = match Approval::read(family, params, started) {
        Ok(approval) => approval,
        Err(why) => {
            crate::log_info!(
                "{uid}: Codex's {method} could not be read ({why}); answered with an error"
            );
            let _ = client
                .refuse(wire, -32602, "CodeConnect could not read this request")
                .await;
            return;
        }
    };
    approval
        .choices
        .retain(|choice| OFFERED_DECISIONS.contains(&choice.id.as_str()));
    let item = if approval.item_id.is_empty() {
        id.clone()
    } else {
        approval.item_id.clone()
    };
    let raised = daemon
        .raise_codex_approval(
            session,
            approval.card(id.clone(), 0),
            &approval.thread_id,
            &approval.turn_id,
            &item,
            family.as_str(),
        )
        .await;
    if !raised {
        let _ = client
            .refuse(wire, -32603, "CodeConnect could not show this request")
            .await;
        return;
    }
    cards.insert(
        id,
        Card {
            wire: wire.clone(),
            family,
            turn: approval.turn_id,
            item,
            answer: None,
        },
    );
}

/// After a restart: the answer claimed for the card `id`, if its claim is still
/// `applying`, sent once more to the request Codex replayed, rebuilt from the stored card
/// and the claim's option, never from anything a phone sends now.
async fn resend(daemon: &Daemon, session: &SessionKey, client: &Client, id: &str, card: &Card) {
    let uid = &session.uid;
    let Ok(Some(state)) = daemon
        .db
        .mutation_status(crate::store::OPERATION_ANSWER, uid.clone(), id.to_string())
        .await
    else {
        return;
    };
    if state.status != AnswerStatus::Applying {
        return;
    }
    let stored = daemon
        .db
        .codex_pending_approvals(uid.clone())
        .await
        .ok()
        .and_then(|rows| rows.into_iter().find(|row| row.request_id == id))
        .and_then(|row| serde_json::from_str::<Value>(&row.card).ok());
    let Some(decision) = stored.and_then(|card| {
        crate::codex_approval::wire_decision(&card["tool_input"]["options"], &state.claimed.route)
    }) else {
        crate::log_error!(
            "{uid}: the answer claimed for {id} could not be rebuilt; not sent again"
        );
        return;
    };
    let body = crate::codex_approval::reply(card.family, decision);
    if client.respond(&card.wire, body).await.is_ok() {
        crate::log_info!("{uid}: the answer claimed for {id} before the restart was sent again");
    }
}

/// What an answer to a card is told while an earlier answer to it waits for Codex to confirm
/// it: by the link, and by the daemon when the run has no link ([`Daemon::answer`]).
pub(crate) const ANSWER_ALREADY_WAITING: &str =
    "an answer to that card is already waiting to be confirmed, so nothing was sent again";

/// A phone's answer to one of the link's cards: claimed, then written to the request. Its
/// outcome is told when Codex says the request is resolved ([`settle`]).
async fn answer(
    daemon: &Daemon,
    uid: &str,
    client: &Client,
    cards: &mut HashMap<String, Card>,
    ask: AnswerRequest,
) {
    let not_applied = |ask: AnswerRequest, why: &str| {
        let _ = ask.reply.send(AnswerReport::NotApplied(why.to_string()));
    };
    let Some(card) = cards.get_mut(&ask.request_id) else {
        return not_applied(ask, "this card is no longer open, so nothing was sent");
    };
    if card.answer.is_some() {
        return not_applied(ask, ANSWER_ALREADY_WAITING);
    }
    if card.wire.is_null() {
        return not_applied(
            ask,
            "this card is waiting for Codex to ask again after the computer restarted; nothing \
             was sent",
        );
    }
    let claimed = daemon
        .db
        .claim_answer_mutation(
            uid.to_string(),
            ask.request_id.clone(),
            ask.claimed.clone(),
            protocol::time::now_rfc3339(),
        )
        .await;
    match claimed {
        Ok(crate::store::MutationClaim::Claimed) => {}
        Ok(crate::store::MutationClaim::Applied { outcome, .. }) => {
            return not_applied(ask, &crate::store::replayed_answer_sentence(&outcome));
        }
        Ok(crate::store::MutationClaim::Indeterminate { .. }) => {
            let _ = ask.reply.send(AnswerReport::Unknown(
                "an answer to this card was already sent and what became of it is not known; \
                 it will not be sent again. Check the computer."
                    .into(),
            ));
            return;
        }
        Ok(crate::store::MutationClaim::Conflict) => {
            return not_applied(
                ask,
                "this card has changed since it was answered, so nothing was sent",
            );
        }
        Ok(crate::store::MutationClaim::NoSession) => {
            return not_applied(ask, "this run is gone, so its card cannot be answered");
        }
        Err(err) => {
            return not_applied(
                ask,
                &format!(
                    "could not record that this answer is being sent ({err}); nothing was sent"
                ),
            );
        }
    }
    let decision = AnswerDecision::OptionId {
        option_id: ask.claimed.route.clone(),
    };
    let body = crate::codex_approval::reply(card.family, ask.decision);
    if client.respond(&card.wire, body).await.is_err() {
        let _ = ask.reply.send(AnswerReport::Unknown(
            "the connection to Codex ended as your answer was sent; do not answer it again".into(),
        ));
        return;
    }
    card.answer = Some(Answer::Phone {
        decision,
        reply: ask.reply,
        _gate: ask.gate,
    });
}

/// A phone's Interrupt for the run's turn `turn_id` (P3 S5). What the record already says
/// about this exact ask is the answer, as in [`Daemon::interrupt`]'s own replay. A turn
/// whose interrupt Codex has already taken is ending, and nothing more is sent; a turn the
/// link is not running is refused. Otherwise the ask is claimed and sent as
/// `turn/interrupt`. Once Codex takes it (`None`), the turn no longer counts as running,
/// and the ask waits for the turn's end ([`settle_stop`]). Codex's own refusal gives the
/// claim back, so the same ask is a first attempt again; a connection that ends first
/// leaves it `applying`, for startup's settle.
async fn interrupt_turn(
    daemon: &Daemon,
    uid: &str,
    client: &Client,
    running: &mut Option<(String, String)>,
    stops: &[WaitingStop],
    request_id: &str,
    turn_id: &str,
) -> Option<InterruptReport> {
    use crate::codex_refusals as says;
    use crate::store::{MutationClaim, OPERATION_INTERRUPT};
    if let Some(report) = recorded_interrupt(daemon, uid, request_id, turn_id).await {
        return Some(report);
    }
    if stops.iter().any(|stop| stop.turn == turn_id) {
        return Some(InterruptReport::NotApplied(
            says::INTERRUPT_ALREADY_WAITING.into(),
        ));
    }
    let Some((thread, turn)) = running.clone().filter(|(_, turn)| turn == turn_id) else {
        return Some(InterruptReport::NotApplied(
            says::INTERRUPT_TURN_NOT_RUNNING.into(),
        ));
    };
    let claimed = crate::store::ClaimedMaterial {
        thread_id: thread.clone(),
        generation: 0,
        route: crate::store::INTERRUPT_ROUTE.to_string(),
        target_turn_id: Some(turn.clone()),
        claimed_hash: protocol::hash::interrupt_hash(uid, &turn),
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
            return Some(crate::codex_link::replayed_interrupt_report(
                &outcome, &turn,
            ));
        }
        Ok(MutationClaim::Indeterminate { .. }) => {
            return Some(InterruptReport::Unknown(
                says::INTERRUPT_ALREADY_SENT_UNKNOWN.into(),
            ));
        }
        Ok(MutationClaim::Conflict) => {
            return Some(InterruptReport::NotApplied(
                says::INTERRUPT_ID_REUSED_ON_LINK.into(),
            ));
        }
        Ok(MutationClaim::NoSession) => {
            return Some(InterruptReport::NotApplied(
                says::INTERRUPT_RUN_IS_GONE.into(),
            ));
        }
        Err(err) => {
            crate::log_error!("{uid}: could not claim the interrupt of turn {turn}: {err:#}");
            return Some(InterruptReport::NotApplied(
                says::INTERRUPT_CLAIM_UNRECORDED.into(),
            ));
        }
    }
    let params = json!({"threadId": thread, "turnId": turn});
    let asked = tokio::time::timeout(
        crate::start_session::START_TIMEOUT,
        client.call("turn/interrupt", params),
    )
    .await;
    match asked {
        Ok(Ok(_)) => {
            *running = None;
            None
        }
        Ok(Err(CallError::Rpc { code, message })) => {
            crate::log_info!("{uid}: Codex refused to interrupt turn {turn}: {message} ({code})");
            if let Err(err) = daemon
                .db
                .release_mutation(OPERATION_INTERRUPT, uid.to_string(), request_id.to_string())
                .await
            {
                crate::log_error!("{uid}: could not give back the refused interrupt: {err:#}");
            }
            Some(InterruptReport::NotApplied(says::wire_refused(
                &code.to_string(),
                says::INTERRUPT_WIRE_SUBJECT,
                says::INTERRUPT_WIRE_REMEDY,
            )))
        }
        Ok(Err(CallError::Closed)) | Err(_) => Some(InterruptReport::Unknown(
            says::interrupt_settled_unknown(says::INTERRUPT_CONNECTION_ENDED),
        )),
    }
}

/// What became of a phone's Stop that Codex took, settled from its turn's end, by the rule
/// the tmux-era link keeps (codex_link's `settle_interrupts_for_turn`): a turn that ended
/// `interrupted` was stopped by it; any other end was the turn's own.
async fn settle_stop(daemon: &Daemon, uid: &str, stop: WaitingStop, status: &str) {
    use crate::codex_refusals as says;
    let (outcome, report) = if status == "interrupted" {
        (
            crate::store::INTERRUPT_ABORTED,
            InterruptReport::Aborted {
                turn_id: stop.turn.clone(),
            },
        )
    } else {
        (
            crate::store::INTERRUPT_TURN_ENDED,
            InterruptReport::NotApplied(says::interrupt_turn_ended_itself(status)),
        )
    };
    let settled = daemon
        .db
        .settle_mutation(
            crate::store::OPERATION_INTERRUPT,
            uid.to_string(),
            stop.request_id.clone(),
            outcome.to_string(),
            protocol::time::now_rfc3339(),
        )
        .await;
    let report = match settled {
        Ok(true) => report,
        Ok(false) => InterruptReport::Unknown(says::INTERRUPT_SETTLED_ELSEWHERE.into()),
        Err(err) => {
            crate::log_error!(
                "{uid}: could not record what became of the stop of turn {}: {err:#}",
                stop.turn
            );
            InterruptReport::Unknown(says::INTERRUPT_OUTCOME_UNRECORDED.into())
        }
    };
    let _ = stop.reply.send(report);
}

/// What the record already says about the phone's Interrupt `request_id` for the run's turn
/// `turn_id`, compared on what the phone bound (the turn and its hash), as
/// [`Daemon::interrupt`] compares it; `None` when the record says nothing.
async fn recorded_interrupt(
    daemon: &Daemon,
    uid: &str,
    request_id: &str,
    turn_id: &str,
) -> Option<InterruptReport> {
    use crate::codex_refusals as says;
    let state = daemon
        .db
        .mutation_status(
            crate::store::OPERATION_INTERRUPT,
            uid.to_string(),
            request_id.to_string(),
        )
        .await
        .ok()??;
    let same = state.claimed.target_turn_id.as_deref() == Some(turn_id)
        && state.claimed.claimed_hash == protocol::hash::interrupt_hash(uid, turn_id);
    if !same {
        return Some(InterruptReport::NotApplied(
            says::INTERRUPT_ID_REUSED_ON_LINK.into(),
        ));
    }
    Some(match state.status {
        AnswerStatus::Settled(outcome) => {
            crate::codex_link::replayed_interrupt_report(&outcome, turn_id)
        }
        AnswerStatus::Indeterminate => {
            InterruptReport::Unknown(says::INTERRUPT_ALREADY_SENT_UNKNOWN.into())
        }
        AnswerStatus::Applying => {
            InterruptReport::NotApplied(says::INTERRUPT_ALREADY_WAITING.into())
        }
    })
}

/// End run's graceful step for the run (P3 S5, ruling D1): the turn it is running
/// interrupted, then its app-server's process group sent SIGTERM under the warrant, so
/// Codex drains and exits. The escalation that follows the stop takes it from there
/// ([`crate::claude_host::escalate_stop`]).
async fn stop_gracefully(
    daemon: &Daemon,
    uid: &str,
    client: &Client,
    running: &mut Option<(String, String)>,
) {
    if let Some((thread, turn)) = running.clone() {
        let asked = tokio::time::timeout(
            crate::start_session::START_TIMEOUT,
            client.call(
                "turn/interrupt",
                json!({"threadId": thread, "turnId": turn}),
            ),
        )
        .await;
        if matches!(asked, Ok(Ok(_))) {
            *running = None;
        } else {
            crate::log_info!("{uid}: its turn {turn} was not interrupted before its stop");
        }
    }
    match daemon.db.watched_host(uid.to_string()).await {
        Ok(Some(watch)) => match watch.boot {
            Some(boot) => {
                if !crate::claude_host::term_group(&watch.host, boot) {
                    crate::log_info!("{uid}: its app-server's group was not sent SIGTERM");
                }
            }
            None => crate::log_info!(
                "{uid}: no boot was recorded with its app-server, so its group was not signalled"
            ),
        },
        Ok(None) => {}
        Err(err) => crate::log_error!("{uid}: could not read its app-server to stop it: {err:#}"),
    }
}

/// The cards a notification ends: the one whose request Codex says is resolved, the one
/// whose item completed, or every one of a turn that ended. Each is retired once, an
/// answered one with its claim settled delivered and its phone told.
async fn settle(
    daemon: &Daemon,
    session: &SessionKey,
    cards: &mut HashMap<String, Card>,
    frame: &Value,
) {
    let params = &frame["params"];
    let ended: Vec<(String, ClearCause)> = match frame["method"].as_str() {
        Some("serverRequest/resolved") => cards
            .iter()
            .filter(|(_, card)| !card.wire.is_null() && card.wire == params["requestId"])
            .map(|(id, _)| (id.clone(), ClearCause::Superseded))
            .collect(),
        Some("item/completed") => cards
            .iter()
            .filter(|(_, card)| params["item"]["id"].as_str() == Some(card.item.as_str()))
            .map(|(id, _)| (id.clone(), ClearCause::ItemCompleted))
            .collect(),
        Some("turn/completed") => {
            let cause = if params["turn"]["status"] == "interrupted" {
                ClearCause::TurnAborted
            } else {
                ClearCause::TurnCompleted
            };
            cards
                .iter()
                .filter(|(_, card)| params["turn"]["id"].as_str() == Some(card.turn.as_str()))
                .map(|(id, _)| (id.clone(), cause))
                .collect()
        }
        _ => return,
    };
    for (id, cause) in ended {
        if let Some(card) = cards.remove(&id) {
            retire(daemon, session, &id, card.answer, cause).await;
        }
    }
}

/// Retire the card `id`: as answered, its claim settled delivered, when an answer was
/// written to it; as cleared by `cause` otherwise.
async fn retire(
    daemon: &Daemon,
    session: &SessionKey,
    id: &str,
    answer: Option<Answer>,
    cause: ClearCause,
) {
    let (decision, reply) = match answer {
        Some(Answer::Phone {
            decision, reply, ..
        }) => (decision, Some(reply)),
        Some(Answer::Claimed { decision }) => (decision, None),
        None => {
            daemon
                .retire_codex_answered(session, id, CodexResolution::Cleared { cause }, None)
                .await;
            return;
        }
    };
    let retired = daemon
        .retire_codex_answered(
            session,
            id,
            CodexResolution::Answered {
                by: ResolutionActor::Phone,
                decision: Some(decision),
            },
            Some(AnswerTerminal::Settled(crate::store::ANSWER_DELIVERED)),
        )
        .await;
    let report = match retired {
        Retirement::Retired => AnswerReport::Delivered,
        Retirement::AlreadyGone => match daemon
            .db
            .settle_answer_mutation(
                session.uid.clone(),
                id.to_string(),
                crate::store::ANSWER_DELIVERED,
                protocol::time::now_rfc3339(),
            )
            .await
        {
            Ok(_) => AnswerReport::Delivered,
            Err(_) => AnswerReport::Unknown(
                "your answer reached Codex but the computer could not record it, so do not \
                 answer it again; check the computer"
                    .into(),
            ),
        },
        Retirement::Failed => AnswerReport::Unknown(
            "your answer reached Codex but the computer could not record it, so the card is \
             still showing; do not answer it again"
                .into(),
        ),
    };
    if let Some(reply) = reply {
        let _ = reply.send(report);
    }
}

/// File `events` in order ([`Daemon::ingest`]: once each, then out to every subscriber).
async fn file(daemon: &Daemon, uid: &str, events: Vec<PendingEvent>) {
    for pending in events {
        if let Err(err) = daemon.ingest(pending).await {
            crate::log_error!("{uid}: could not file a Codex event: {err:#}");
        }
    }
}

/// A `thread/resume` answer's facts filed, then the adapter told what was filed; the turns
/// the answer reports still running.
async fn resumed(
    daemon: &Daemon,
    uid: &str,
    adapter: &mut CodexAdapter,
    answer: &Value,
    thread: &str,
) -> Vec<String> {
    let Some(mut seed) = adapter.plan_resume_seed(answer, thread) else {
        crate::log_info!("{uid}: Codex's resume answer for thread {thread} was not read");
        return Vec::new();
    };
    file(daemon, uid, seed.take_events()).await;
    adapter.apply_resume_seed(&seed);
    seed.running_turn_ids().to_vec()
}

/// A restarted ccd's link to the `live` run `run`, whose recorded app-server is alive (P3
/// R2): its socket, `initialize`, a fresh skills scan, then `thread/resume` with the preset
/// and the run's thread config, the answer filed, and the run's [`link`] from there. Codex
/// restores the thread's model itself; the resume carries the preset and the thread config,
/// as the start did. A turn the answer reports running is owed its follow-up. Anything that
/// fails, within START_TIMEOUT, leaves the run `live` and unlinked, and is logged.
pub(crate) async fn reconnect(
    daemon: Arc<Daemon>,
    root: PathBuf,
    run: LiveCodexRun,
    codeconnect: PathBuf,
) {
    let uid = run.host.session_uid.clone();
    let resumed = tokio::time::timeout(
        crate::start_session::START_TIMEOUT,
        resume(&daemon, &root, &run, &codeconnect),
    )
    .await;
    match resumed {
        Ok(Ok((client, adapter, owed, cards))) => {
            link(daemon, uid, client, adapter, owed, cards).await
        }
        Ok(Err(why)) => crate::log_info!("{uid}: not reconnected: {why}"),
        Err(_) => crate::log_info!(
            "{uid}: not reconnected within {} s",
            crate::start_session::START_TIMEOUT.as_secs()
        ),
    }
}

async fn resume(
    daemon: &Daemon,
    root: &Path,
    run: &LiveCodexRun,
    codeconnect: &Path,
) -> Result<(Client, CodexAdapter, Option<Owed>, HashMap<String, Card>), String> {
    let uid = &run.host.session_uid;
    let socket = crate::claude_host::run_dir(root, uid).join("as.sock");
    let client = crate::codex_client::connect(&socket)
        .await
        .map_err(|err| format!("could not reach Codex's app-server: {err:#}"))?;
    client
        .initialize()
        .await
        .map_err(|err| format!("Codex's app-server did not initialize: {err}"))?;
    let folders = folders(&run.cwd, daemon.config.git_bin.as_deref()).await?;
    let skills = client
        .call(
            "skills/list",
            json!({"cwds": [&run.cwd], "forceReload": true}),
        )
        .await
        .map_err(|err| format!("Codex did not list the folder's skills: {err}"))?;
    let mut params = thread_params(&run.cwd, None);
    params["threadId"] = json!(run.thread);
    params["config"] = thread_config(uid, &codeconnect.to_string_lossy(), &folders, &skills);
    let answer = client
        .call("thread/resume", params.clone())
        .await
        .map_err(|err| format!("Codex did not resume the thread: {err}"))?;
    let mut adapter = CodexAdapter::new(SessionKey::new(uid, uid));
    let running = resumed(daemon, uid, &mut adapter, &answer, &run.thread).await;
    crate::log_info!(
        "{uid}: connected again to its app-server; thread {} resumed, {} turn(s) still running",
        run.thread,
        running.len()
    );
    let cards = standing_cards(daemon, &SessionKey::new(uid, uid), &running).await;
    let owed = running.into_iter().next().map(|turn| Owed {
        thread: run.thread.clone(),
        turn,
        resume: params,
    });
    Ok((client, adapter, owed, cards))
}

/// The run's cards a restart found standing. One whose turn the resume answer shows ended is
/// retired (as answered, if its answer was claimed); the rest are kept for Codex's replay of
/// their request or their item's or turn's end (P7-2). A claimed answer is kept as such
/// (R6): sent again if Codex replays the request, settled delivered when the card ends.
async fn standing_cards(
    daemon: &Daemon,
    session: &SessionKey,
    running: &[String],
) -> HashMap<String, Card> {
    let mut cards = HashMap::new();
    let rows = match daemon.db.codex_pending_approvals(session.uid.clone()).await {
        Ok(rows) => rows,
        Err(err) => {
            crate::log_error!(
                "{}: could not read its standing cards: {err:#}",
                session.uid
            );
            return cards;
        }
    };
    for row in rows {
        let Some(family) = Family::of_item_type(&row.family) else {
            continue;
        };
        let answer = match daemon
            .db
            .mutation_status(
                crate::store::OPERATION_ANSWER,
                session.uid.clone(),
                row.request_id.clone(),
            )
            .await
        {
            Ok(Some(state)) if state.status == AnswerStatus::Applying => Some(Answer::Claimed {
                decision: AnswerDecision::OptionId {
                    option_id: state.claimed.route,
                },
            }),
            _ => None,
        };
        if running.contains(&row.turn_id) {
            cards.insert(
                row.request_id,
                Card {
                    wire: Value::Null,
                    family,
                    turn: row.turn_id,
                    item: row.item_id,
                    answer,
                },
            );
        } else {
            retire(
                daemon,
                session,
                &row.request_id,
                answer,
                ClearCause::TurnCompleted,
            )
            .await;
        }
    }
    cards
}

/// `skills/list`, `thread/start`, the thread recorded, then `turn/start` with the first
/// message: each call's error, or a run ended meanwhile, stops it.
async fn send_first(daemon: &Daemon, client: &Client, send: &FirstSend) -> Result<(), String> {
    let skills = client
        .call(
            "skills/list",
            json!({"cwds": [&send.cwd], "forceReload": true}),
        )
        .await
        .map_err(|err| format!("Codex did not list the folder's skills: {err}"))?;
    let mut params = thread_params(&send.cwd, send.model.as_deref());
    params["config"] = thread_config(&send.uid, &send.codeconnect, &send.folders, &skills);
    let started = client
        .call("thread/start", params)
        .await
        .map_err(|err| format!("Codex did not start a thread: {err}"))?;
    let thread = started["thread"]["id"]
        .as_str()
        .ok_or("Codex's thread/start named no thread")?;
    match daemon
        .db
        .record_hosted_thread(send.uid.clone(), thread.to_string())
        .await
    {
        Ok(true) => {}
        Ok(false) => return Err("the run ended before its thread was recorded".into()),
        Err(err) => return Err(format!("could not record the run's thread: {err:#}")),
    }
    client
        .call(
            "turn/start",
            json!({
                "threadId": thread,
                "input": [{"type": "text", "text": send.prompt}],
                "clientUserMessageId": send.start_uuid,
            }),
        )
        .await
        .map_err(|err| format!("Codex did not take the first message: {err}"))?;
    Ok(())
}

/// `thread/start`'s params but its `config`: the run's folder, its model (none for Codex's own
/// `default`) and the preset Default, `on-request` with a `workspace-write` sandbox (T4,
/// decision 176), so an untrusted folder never falls to Codex's own UnlessTrusted.
fn thread_params(cwd: &str, model: Option<&str>) -> Value {
    json!({
        "cwd": cwd,
        "model": model,
        "approvalPolicy": "on-request",
        "sandbox": "workspace-write",
    })
}

/// The run `uid`'s thread config (P3 L4): its `folders` untrusted, the repository's own
/// skills that `skills/list` reported disabled, and CodeConnect's mail server, run by
/// `codeconnect` with the run's uid, which Codex hands an MCP server only through its `env`.
fn thread_config(uid: &str, codeconnect: &str, folders: &[PathBuf], skills: &Value) -> Value {
    let projects: serde_json::Map<String, Value> = folders
        .iter()
        .map(|folder| {
            (
                folder.to_string_lossy().into_owned(),
                json!({"trust_level": "untrusted"}),
            )
        })
        .collect();
    let mut config = json!({
        "projects": projects,
        "mcp_servers.codeconnect": {
            "command": codeconnect,
            "args": protocol::ipc::mail_server_args(uid, &protocol::socket_path()),
            "env": {protocol::ENV_SESSION_UID: uid},
        },
    });
    let disabled: Vec<Value> = skills["data"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|entry| entry["skills"].as_array().into_iter().flatten())
        .filter(|skill| skill["scope"] == "repo")
        .map(|skill| json!({"path": skill["path"], "enabled": false}))
        .collect();
    if !disabled.is_empty() {
        config["skills.config"] = Value::Array(disabled);
    }
    config
}

/// The run's folder, its app-server launched and recorded in `launched`, connected and
/// initialized, by `deadline`.
async fn bring_up(
    daemon: &Arc<Daemon>,
    uid: &str,
    run: &Run<'_>,
    folders: &[PathBuf],
    deadline: Instant,
    launched: &mut Option<(ProcessIdentity, Option<BootIdentity>)>,
) -> Result<Client, String> {
    let dir = crate::claude_host::run_dir(run.root, uid);
    let log = crate::start_session::make_run_dir(run.root, &dir)
        .map_err(|err| format!("could not make the run's folder {}: {err}", dir.display()))?;
    let socket = dir.join("as.sock");
    let mut listen = OsString::from("unix://");
    listen.push(&socket);
    let projects = format!("projects={}", launch_projects(folders));
    let mut child = crate::start_session::spawn_detached(
        run.environment,
        &[
            OsStr::new("/bin/sh"),
            OsStr::new("-c"),
            OsStr::new(CLAIM),
            OsStr::new("codeconnect-codex"),
            dir.as_os_str(),
            run.codex.as_os_str(),
            OsStr::new("-c"),
            OsStr::new(&projects),
            OsStr::new("app-server"),
            OsStr::new("--listen"),
            &listen,
        ],
        Path::new(run.cwd),
        uid,
        log,
    )
    .map_err(|err| format!("could not run the login shell: {err}"))?;
    let attached = attach(daemon, uid, &dir, &socket, &mut child, deadline, launched).await;
    crate::start_session::reap(child);
    attached
}

async fn attach(
    daemon: &Arc<Daemon>,
    uid: &str,
    dir: &Path,
    socket: &Path,
    child: &mut tokio::process::Child,
    deadline: Instant,
    launched: &mut Option<(ProcessIdentity, Option<BootIdentity>)>,
) -> Result<Client, String> {
    let pid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("the login shell ended as it started")?;
    let leader = ProcessIdentity {
        pid,
        birth: read_birth_identity(pid).ok_or("could not read the app-server's start time")?,
    };
    let boot = protocol::proc_identity::boot_identity();
    *launched = Some((leader, boot));
    daemon
        .db
        .record_host(uid.to_string(), leader, boot)
        .await
        .map_err(|err| format!("could not record the app-server: {err:#}"))?;

    let client = loop {
        match tokio::time::timeout_at(deadline, crate::codex_client::connect(socket)).await {
            Ok(Ok(client)) => break client,
            Ok(Err(err)) if not_listening(&err) => {
                crate::start_session::give_up_if_ended(
                    dir,
                    child,
                    deadline,
                    "Codex's app-server did not listen",
                )?;
                tokio::time::sleep(POLL).await;
            }
            Ok(Err(err)) => return Err(format!("could not reach Codex's app-server: {err:#}")),
            Err(_) => {
                return Err(format!(
                    "Codex's app-server did not listen within {} s",
                    crate::start_session::START_TIMEOUT.as_secs()
                ))
            }
        }
    };
    match tokio::time::timeout_at(deadline, client.initialize()).await {
        Ok(Ok(_)) => Ok(client),
        Ok(Err(err)) => Err(format!("Codex's app-server did not initialize: {err}")),
        Err(_) => Err(format!(
            "Codex's app-server did not answer initialize within {} s",
            crate::start_session::START_TIMEOUT.as_secs()
        )),
    }
}

/// Whether a dial failed only because nothing listens on the socket yet.
fn not_listening(err: &anyhow::Error) -> bool {
    err.downcast_ref::<io::Error>().is_some_and(|err| {
        matches!(
            err.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    })
}

/// The folders codex is told the trust of, by their real paths, top down: every folder from
/// the repository's top down to `cwd`, or `cwd` alone outside git.
async fn folders(cwd: &str, git_bin: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let folder = std::fs::canonicalize(cwd).map_err(|err| format!("{cwd}: {err}"))?;
    let top = crate::git::toplevel(cwd, git_bin, crate::start_session::GIT_READ_TIMEOUT)
        .await
        .and_then(|top| std::fs::canonicalize(top).ok());
    let mut folders: Vec<PathBuf> = match &top {
        Some(top) if folder.starts_with(top) => folder
            .ancestors()
            .take_while(|at| at.starts_with(top))
            .map(Path::to_path_buf)
            .collect(),
        _ => vec![folder],
    };
    folders.reverse();
    Ok(folders)
}

/// `folders`, each untrusted, as the TOML inline table the launch's `-c projects=` takes.
/// Each path is a TOML basic string, written as JSON writes a string.
fn launch_projects(folders: &[PathBuf]) -> String {
    let entries: Vec<String> = folders
        .iter()
        .map(|folder| {
            let key =
                serde_json::to_string(&folder.to_string_lossy()).expect("a string is always JSON");
            format!(r#"{key} = {{ trust_level = "untrusted" }}"#)
        })
        .collect();
    format!("{{ {} }}", entries.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Endpoint;
    use crate::store::Store;
    use futures_util::{SinkExt, StreamExt};
    use protocol::config::Config;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;
    use tokio_tungstenite::tungstenite::protocol::Message;

    type Socket = tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>;

    /// A state folder of its own, short and under `/tmp` so `runs/<uid>/as.sock` fits
    /// `sun_path`, with a daemon on a store in it. Removed when dropped.
    struct Rig {
        root: PathBuf,
        daemon: Arc<Daemon>,
        /// The folder the rig's runs start in.
        project: String,
        /// What a start gives codex: a home of the rig's own and the system shell.
        environment: HashMap<OsString, OsString>,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn rig() -> Rig {
        let uid = protocol::uid::new().unwrap();
        let root = PathBuf::from(format!("/tmp/ccx3.{}", &uid[16..]));
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        let environment = HashMap::from([
            (OsString::from("HOME"), root.join("home").into_os_string()),
            (OsString::from("SHELL"), OsString::from("/bin/sh")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        Rig {
            daemon: daemon_on(&root),
            project: root.join("project").display().to_string(),
            root,
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

    /// A stand-in codex running `script`, launched as codex is: in the run's folder, after
    /// the claim, with codex's arguments. It keeps its arguments in the run's `argv` and
    /// counts its launches in `<root>/launches`.
    fn stand_in(rig: &Rig, script: &str) -> PathBuf {
        let path = rig.root.join("stand-in-codex");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" > argv\necho launch >> '{}'\n{script}\n",
                rig.root.join("launches").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    const CODECONNECT: &str = "/nonexistent/codeconnect";

    fn run<'a>(rig: &'a Rig, request_id: &'a str, codex: &'a Path) -> Run<'a> {
        Run {
            root: &rig.root,
            request_id,
            cwd: &rig.project,
            prompt: "fix the tests",
            model: Some("gpt-test"),
            codex,
            codeconnect: Path::new(CODECONNECT),
            git_bin: None,
            environment: &rig.environment,
        }
    }

    fn launches(rig: &Rig) -> usize {
        std::fs::read_to_string(rig.root.join("launches"))
            .map(|launches| launches.lines().count())
            .unwrap_or(0)
    }

    /// The run's folder, once the start has made one.
    async fn run_folder(rig: &Rig) -> PathBuf {
        loop {
            if let Some(dir) = std::fs::read_dir(rig.root.join("runs"))
                .ok()
                .and_then(|mut entries| entries.next())
            {
                return dir.unwrap().path();
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// A repository skill and a user skill, as `skills/list` reports them.
    const REPO_SKILL: &str = "/repo/.codex/skills/deploy/SKILL.md";

    /// What a stand-in app-server answers, in order, for a run that starts its thread and
    /// takes its first message.
    fn whole_start() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("initialize", json!({"result": {"userAgent": "stand-in"}})),
            (
                "skills/list",
                json!({"result": {"data": [{"cwd": "/repo", "errors": [], "skills": [
                    {"name": "deploy", "path": REPO_SKILL, "scope": "repo", "enabled": true},
                    {"name": "mine", "path": "/home/.codex/skills/mine/SKILL.md", "scope": "user",
                     "enabled": true}
                ]}]}}),
            ),
            (
                "thread/start",
                json!({"result": {"thread": {"id": "thr-x3"}, "model": "gpt-test"}}),
            ),
            ("turn/start", json!({"result": {"turn": {"id": "turn-1"}}})),
        ]
    }

    /// The test's side of `as.sock`: it waits for the stand-in to be launched, listens
    /// where the app-server would, takes ccd's connection, and answers each of `script`'s
    /// methods, in order, with its answer. It gives back every frame ccd sent so far, and
    /// the connection.
    async fn play_app_server(
        rig: &Rig,
        script: Vec<(&str, serde_json::Value)>,
    ) -> (Vec<serde_json::Value>, Socket) {
        let dir = run_folder(rig).await;
        while launches(rig) == 0 {
            tokio::time::sleep(POLL).await;
        }
        let listener = UnixListener::bind(dir.join("as.sock")).unwrap();
        let (stream, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
            .await
            .expect("ccd connected")
            .unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut seen = Vec::new();
        for (method, answer) in script {
            let asked = loop {
                let frame = next_frame(&mut ws).await.expect("ccd hung up mid-script");
                seen.push(frame.clone());
                if frame["method"] == method && frame.get("id").is_some() {
                    break frame;
                }
            };
            let mut reply = answer;
            reply["id"] = asked["id"].clone();
            ws.send(Message::Text(reply.to_string())).await.unwrap();
        }
        (seen, ws)
    }

    /// Every frame ccd sends until it lets the connection go.
    async fn until_closed(mut ws: Socket) -> Vec<serde_json::Value> {
        let mut seen = Vec::new();
        while let Some(frame) = next_frame(&mut ws).await {
            seen.push(frame);
        }
        seen
    }

    async fn next_frame(ws: &mut Socket) -> Option<serde_json::Value> {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => return Some(serde_json::from_str(&text).unwrap()),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                Some(Ok(_)) => continue,
            }
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Whether the process group `pgid` has a member.
    fn group_alive(pgid: i32) -> bool {
        protocol::proc_identity::group_has_members(pgid)
    }

    /// The run's row once it is `want`, within 10 s.
    async fn row_once(rig: &Rig, uid: &str, want: Lifecycle) -> SessionRow {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let row = rig
                .daemon
                .db
                .get_session(uid.to_string())
                .await
                .unwrap()
                .unwrap();
            if row.lifecycle == want || Instant::now() >= deadline {
                return row;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Whether `pgid`'s group is empty within 15 s (the ladder's zombie wait and grace).
    async fn group_ends(pgid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while group_alive(pgid) && Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
        }
        !group_alive(pgid)
    }

    /// The ladder on the run's recorded group, so a test leaves no process behind.
    async fn end_run(rig: &Rig, uid: &str) {
        if let Some(watch) = rig.daemon.db.watched_host(uid.to_string()).await.unwrap() {
            crate::claude_host::stop_group(watch.host, watch.boot.unwrap()).await;
        }
    }

    fn started_uid(result: &StartSessionResult) -> String {
        match result {
            StartSessionResult::Started { session_uid, .. } => session_uid.clone(),
            other => panic!("expected a start, got {other:?}"),
        }
    }

    fn methods(frames: &[serde_json::Value]) -> Vec<&str> {
        frames.iter().filter_map(|f| f["method"].as_str()).collect()
    }

    /// **A Codex start writes its row, launches codex in a session of its own after the
    /// claim, records the launch's leader, connects and initializes**, then answers
    /// `started` with its folder. The leader is the spawned pid, which the claim wrote. A
    /// repeat of the request, on this daemon or on a restarted one, answers the same run
    /// and launches nothing.
    #[tokio::test]
    async fn a_codex_start_launches_after_the_claim_records_its_leader_and_initializes() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let (result, (seen, _ws)) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-1", &codex)),
            play_app_server(&rig, whole_start())
        );
        let uid = started_uid(&result);
        assert_eq!(
            result,
            StartSessionResult::Started {
                session_uid: uid.clone(),
                cwd: Some(rig.project.clone())
            }
        );
        let dir = crate::claude_host::run_dir(&rig.root, &uid);
        row_once(&rig, &uid, Lifecycle::Live).await;
        let watch = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap();
        end_run(&rig, &uid).await;

        assert_eq!(seen[0]["method"], "initialize");
        assert_eq!(seen[0]["params"]["capabilities"]["experimentalApi"], true);
        assert_eq!(seen[1], json!({"method": "initialized"}));
        let claimed: i32 = std::fs::read_to_string(dir.join("as.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(watch.host.pid, claimed, "the claim is the recorded leader");
        assert!(watch.boot.is_some());
        assert_eq!((mode(&dir), mode(&dir.join("host.log"))), (0o700, 0o600));
        let argv = std::fs::read(dir.join("argv")).unwrap();
        let argv: Vec<String> = argv
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect();
        let real = std::fs::canonicalize(&rig.project)
            .unwrap()
            .display()
            .to_string();
        assert_eq!(
            argv,
            [
                "-c".to_string(),
                format!(r#"projects={{ "{real}" = {{ trust_level = "untrusted" }} }}"#),
                "app-server".into(),
                "--listen".into(),
                format!("unix://{}", dir.join("as.sock").display()),
            ]
        );

        let again = start(&rig.daemon, run(&rig, "req-1", &codex)).await;
        let restarted = start(&daemon_on(&rig.root), run(&rig, "req-1", &codex)).await;
        assert_eq!(again, result);
        assert_eq!(restarted, again);
        assert_eq!(launches(&rig), 1, "a repeat launched another app-server");
    }

    /// The leader leads its own group and session: `setsid` ran before codex.
    #[tokio::test]
    async fn the_leader_leads_its_own_group_and_session() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let (result, (_, _ws)) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-group", &codex)),
            play_app_server(&rig, whole_start())
        );
        let uid = started_uid(&result);
        row_once(&rig, &uid, Lifecycle::Live).await;
        let watch = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap();
        let pid = watch.host.pid;
        // SAFETY: getpgid(2) and getsid(2) only read.
        let (group, session) = unsafe { (libc::getpgid(pid), libc::getsid(pid)) };
        end_run(&rig, &uid).await;
        assert_eq!((group, session), (pid, pid));
        assert_ne!(group, unsafe { libc::getpgid(0) });
    }

    /// **A started Codex run sends its first message on a thread of its own, then is
    /// `live`** (L9 i-v): `skills/list` for the run's folder, `thread/start` with the
    /// folder, the start's model, the preset and the keep-out config (the folder
    /// untrusted, the repository's skill disabled and the user's kept, CodeConnect's mail
    /// server with the run's uid), the thread recorded on the row, then `turn/start` with
    /// the first message under the start's uuid. The connection is kept as the run's link.
    #[tokio::test]
    async fn a_started_codex_run_sends_its_first_message_on_its_own_thread_and_goes_live() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let (result, (seen, _ws)) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-first", &codex)),
            play_app_server(&rig, whole_start())
        );
        let uid = started_uid(&result);
        let row = row_once(&rig, &uid, Lifecycle::Live).await;
        end_run(&rig, &uid).await;

        assert_eq!(row.lifecycle, Lifecycle::Live);
        assert_eq!(row.codex_thread_id.as_deref(), Some("thr-x3"));
        assert_eq!(
            methods(&seen),
            [
                "initialize",
                "initialized",
                "skills/list",
                "thread/start",
                "turn/start"
            ]
        );
        let params = |method: &str| {
            seen.iter().find(|frame| frame["method"] == method).unwrap()["params"].clone()
        };
        assert_eq!(
            params("skills/list"),
            json!({"cwds": [rig.project], "forceReload": true})
        );
        let real = std::fs::canonicalize(&rig.project)
            .unwrap()
            .display()
            .to_string();
        assert_eq!(
            params("thread/start"),
            json!({
                "cwd": rig.project,
                "model": "gpt-test",
                "approvalPolicy": "on-request",
                "sandbox": "workspace-write",
                "config": {
                    "projects": {real: {"trust_level": "untrusted"}},
                    "skills.config": [{"path": REPO_SKILL, "enabled": false}],
                    "mcp_servers.codeconnect": {
                        "command": CODECONNECT,
                        "args": protocol::ipc::mail_server_args(&uid, &protocol::socket_path()),
                        "env": {"CODECONNECT_SESSION_UID": uid},
                    },
                },
            })
        );
        let turn = params("turn/start");
        assert_eq!(turn["threadId"], "thr-x3");
        assert_eq!(
            turn["input"],
            json!([{"type": "text", "text": "fix the tests"}])
        );
        let uuid = turn["clientUserMessageId"].as_str().unwrap();
        assert_eq!((uuid.len(), uuid.matches('-').count()), (36, 4), "{uuid}");
    }

    /// **The start answers before the thread is made, and a `skills/list` that fails ends
    /// the run**: `started` reaches the app while `skills/list` is still unanswered; then
    /// its error makes the run `failed` and the ladder empties its group.
    #[tokio::test]
    async fn the_start_answers_before_its_thread_and_a_failed_skills_list_ends_the_run() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let (answered, unanswered) = tokio::sync::oneshot::channel();
        let starting = async {
            let result = start(&rig.daemon, run(&rig, "req-scan", &codex)).await;
            answered.send(()).unwrap();
            result
        };
        let serving = async {
            let (_, mut ws) = play_app_server(&rig, whole_start()[..1].to_vec()).await;
            assert_eq!(next_frame(&mut ws).await.unwrap()["method"], "initialized");
            let asked = next_frame(&mut ws).await.unwrap();
            assert_eq!(asked["method"], "skills/list");
            tokio::time::timeout(Duration::from_secs(5), unanswered)
                .await
                .expect("the start waited for skills/list")
                .unwrap();
            let refused = json!({"id": asked["id"], "error": {"code": -32603, "message": "no"}});
            ws.send(Message::Text(refused.to_string())).await.unwrap();
            until_closed(ws).await
        };
        let (result, after) = tokio::join!(starting, serving);
        let uid = started_uid(&result);
        let row = row_once(&rig, &uid, Lifecycle::Failed).await;
        let claimed: i32 =
            std::fs::read_to_string(crate::claude_host::run_dir(&rig.root, &uid).join("as.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
        assert_eq!(row.lifecycle, Lifecycle::Failed);
        assert!(group_ends(claimed).await, "the run's group outlived it");
        assert!(methods(&after).is_empty(), "{after:?}");
    }

    /// **A refused `thread/start` ends the run**, `failed` with its group emptied and no
    /// first message sent; Codex's own `default` model reaches `thread/start` as none, and
    /// a folder with no repository skill gets no skills override.
    #[tokio::test]
    async fn a_refused_thread_start_fails_the_run_and_sends_nothing() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let mut default = run(&rig, "req-refused", &codex);
        default.model = Some("default");
        let script = [
            whole_start()[0].clone(),
            ("skills/list", json!({"result": {"data": []}})),
            (
                "thread/start",
                json!({"error": {"code": -32600, "message": "invalid config"}}),
            ),
        ];
        let (result, (mut seen, ws)) = tokio::join!(
            start(&rig.daemon, default),
            play_app_server(&rig, script.to_vec())
        );
        seen.extend(until_closed(ws).await);
        let uid = started_uid(&result);
        let row = row_once(&rig, &uid, Lifecycle::Failed).await;
        let claimed: i32 =
            std::fs::read_to_string(crate::claude_host::run_dir(&rig.root, &uid).join("as.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
        assert_eq!(row.lifecycle, Lifecycle::Failed);
        assert_eq!(row.codex_thread_id, None);
        assert!(group_ends(claimed).await, "the run's group outlived it");
        assert_eq!(
            methods(&seen),
            ["initialize", "initialized", "skills/list", "thread/start"]
        );
        let thread = &seen
            .iter()
            .find(|frame| frame["method"] == "thread/start")
            .unwrap()["params"];
        assert_eq!(thread["model"], serde_json::Value::Null);
        assert!(thread["config"].get("skills.config").is_none(), "{thread}");
    }

    /// **A live Codex run whose app-server dies ends, and can be removed** (T11): the
    /// liveness sweep's check finds its leader gone and ends the row `exited` in Codex's
    /// own table, as a Claude run's ends.
    #[tokio::test]
    async fn a_live_codex_run_whose_app_server_dies_ends_and_can_be_removed() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let (result, (_, _ws)) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-dies", &codex)),
            play_app_server(&rig, whole_start())
        );
        let uid = started_uid(&result);
        row_once(&rig, &uid, Lifecycle::Live).await;
        let leader = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .host
            .pid;
        end_run(&rig, &uid).await;
        assert!(group_ends(leader).await);
        crate::claude_host::end_if_host_gone(&rig.daemon, &uid).await;
        let row = row_once(&rig, &uid, Lifecycle::Exited).await;
        assert_eq!(row.lifecycle, Lifecycle::Exited);
        assert!(matches!(
            rig.daemon
                .db
                .delete_exited_session(uid.clone())
                .await
                .unwrap(),
            crate::store::DeleteOutcome::Deleted { .. }
        ));
    }

    /// **A codex that never listens is stopped, and its run failed**: at the deadline the
    /// start answers why, the run is `failed`, and the ladder empties the run's group, a
    /// child that ignores SIGTERM included.
    #[tokio::test]
    async fn a_codex_that_never_listens_is_stopped_and_its_run_failed() {
        let rig = rig();
        let codex = stand_in(&rig, "(trap '' TERM; exec sleep 60) &\nexec sleep 60");
        let result = start_within(
            &rig.daemon,
            run(&rig, "req-silent", &codex),
            Duration::from_secs(2),
        )
        .await;
        assert!(
            matches!(&result, StartSessionResult::Failed { detail, .. } if detail.contains("did not listen")),
            "{result:?}"
        );
        let dir = run_folder(&rig).await;
        let leader: i32 = std::fs::read_to_string(dir.join("as.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let run = rig
            .daemon
            .db
            .hosted_run_for_request("req-silent".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.lifecycle, Lifecycle::Failed);
        assert!(
            !group_alive(leader),
            "the run's group outlived its failed start"
        );
    }

    /// **A codex that ends at once fails the start with what it said last**, and the run
    /// is `failed`.
    #[tokio::test]
    async fn a_codex_that_ends_at_once_fails_its_start_with_its_last_line() {
        let rig = rig();
        let codex = stand_in(&rig, "echo 'error: unexpected argument' >&2; exit 2");
        let result = start(&rig.daemon, run(&rig, "req-ends", &codex)).await;
        assert_eq!(
            result,
            StartSessionResult::Failed {
                reason: StartFailure::LaunchFailed,
                detail: "error: unexpected argument".into(),
            }
        );
        let run = rig
            .daemon
            .db
            .hosted_run_for_request("req-ends".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.lifecycle, Lifecycle::Failed);
    }

    /// A start left as a ccd that dies mid-start leaves it: abandoned once its app-server's
    /// leader is recorded, codex running on in its own session. The run's uid and leader.
    async fn abandoned_start(
        rig: &Rig,
        request_id: &str,
        codex: &Path,
    ) -> (String, ProcessIdentity) {
        let recorded = async {
            loop {
                let run = rig
                    .daemon
                    .db
                    .hosted_run_for_request(request_id.into())
                    .await
                    .unwrap();
                if let Some(run) = run {
                    let watch = rig
                        .daemon
                        .db
                        .watched_host(run.session_uid.clone())
                        .await
                        .unwrap();
                    if let Some(watch) = watch {
                        return (run.session_uid, watch.host);
                    }
                }
                tokio::time::sleep(POLL).await;
            }
        };
        tokio::select! {
            result = start_within(&rig.daemon, run(rig, request_id, codex), Duration::from_secs(60)) => {
                panic!("the start ended before its leader was recorded: {result:?}")
            }
            recorded = recorded => recorded,
        }
    }

    /// A ccd restarted on the rig's store, recovered as `main` recovers it before it listens.
    async fn restarted(rig: &Rig) -> Arc<Daemon> {
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        restarted.recover_hosted_runs(&rig.root).await;
        restarted
    }

    /// The row `uid` is `failed` on `daemon`, and a repeat of its request is told so.
    async fn failed_at_restart(daemon: &Arc<Daemon>, uid: &str, request_id: &str) {
        let row = daemon
            .db
            .get_session(uid.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.lifecycle, Lifecycle::Failed);
        let repeat = daemon
            .db
            .hosted_run_for_request(request_id.into())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            crate::claude_host::answer_for(repeat),
            StartSessionResult::Failed { detail, .. } if detail == "did not start"
        ));
    }

    /// **G1 (a): a start with no app-server recorded** (the ccd died between the row and the
    /// record) is failed when the restarted ccd recovers.
    #[tokio::test]
    async fn a_codex_start_with_no_recorded_app_server_fails_at_a_restart() {
        let rig = rig();
        let uid = protocol::uid::new().unwrap();
        let now = protocol::time::now_rfc3339();
        let row = SessionRow {
            session_uid: uid.clone(),
            session_id: uid.clone(),
            tmux_session: String::new(),
            tmux_socket: String::new(),
            cwd: rig.project.clone(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: Lifecycle::Starting,
            created_at: now.clone(),
            updated_at: now,
            agent: AgentKind::Codex,
            codex_thread_id: None,
            codex_socket: None,
        };
        let begun = rig
            .daemon
            .db
            .begin_run(row, "req-unrecorded".into(), None)
            .await
            .unwrap();
        assert_eq!(begun, None);
        failed_at_restart(&restarted(&rig).await, &uid, "req-unrecorded").await;
    }

    /// **G1 (b): a start whose recorded app-server is alive** is failed when the restarted ccd
    /// recovers, and the ladder empties its group: the start is not sent again.
    #[tokio::test]
    async fn a_codex_start_whose_app_server_lives_is_failed_and_stopped_at_a_restart() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let (uid, leader) = abandoned_start(&rig, "req-alive", &codex).await;
        assert!(group_alive(leader.pid));
        failed_at_restart(&restarted(&rig).await, &uid, "req-alive").await;
        assert!(
            group_ends(leader.pid).await,
            "the run's group outlived the restart"
        );
    }

    /// **G1 (c): a start whose recorded leader is gone but whose group is not** (the npm
    /// launcher killed alone, its app-server orphaned in the group) is failed when the
    /// restarted ccd recovers, and the ladder empties the group.
    #[tokio::test]
    async fn a_codex_start_whose_leader_died_alone_is_failed_and_its_group_emptied_at_a_restart() {
        let rig = rig();
        let codex = stand_in(&rig, "sleep 30 &\necho $! > child\nwait");
        let (uid, leader) = abandoned_start(&rig, "req-orphan", &codex).await;
        let dir = run_folder(&rig).await;
        while !dir.join("child").exists() {
            tokio::time::sleep(POLL).await;
        }
        // SAFETY: kill(2) of the rig's own stand-in leader, which this test launched.
        unsafe { libc::kill(leader.pid, libc::SIGKILL) };
        tokio::time::timeout(Duration::from_secs(10), async {
            while protocol::proc_identity::liveness(&leader)
                != protocol::proc_identity::Liveness::Gone
            {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the killed leader is gone");
        assert!(group_alive(leader.pid), "the leader's child left its group");
        failed_at_restart(&restarted(&rig).await, &uid, "req-orphan").await;
        assert!(
            group_ends(leader.pid).await,
            "the orphaned child outlived the restart"
        );
    }

    /// A Codex turn with a command, as codex sent it (fixtures/codex/command-execution.jsonl).
    const LIVE_FRAMES: &str = include_str!("../../../fixtures/codex/command-execution.jsonl");
    /// A turn a resume found running, and the answers before and after its end, as codex
    /// sent them (fixtures/codex/resume-follow-up-turn-0.164.0.json).
    const FOLLOW_UP: &str =
        include_str!("../../../fixtures/codex/resume-follow-up-turn-0.164.0.json");

    /// The method call `method` ccd sends next on `ws`, within 20 s, every other frame before
    /// it passed over.
    async fn asked(ws: &mut Socket, method: &str) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let frame = next_frame(ws).await.expect("ccd hung up");
                if frame["method"] == method && frame.get("id").is_some() {
                    return frame;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("ccd did not call {method} within 20 s"))
    }

    async fn answer(ws: &mut Socket, asked: &serde_json::Value, result: serde_json::Value) {
        let reply = json!({"id": asked["id"], "result": result});
        ws.send(Message::Text(reply.to_string())).await.unwrap();
    }

    async fn tell(ws: &mut Socket, frame: &serde_json::Value) {
        ws.send(Message::Text(frame.to_string())).await.unwrap();
    }

    /// The source ids of the run's filed events once one ends with `last`, within 10 s.
    async fn filed_until(daemon: &Daemon, uid: &str, last: &str) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ids: Vec<String> = daemon
                .db
                .events_after(uid.to_string(), 0, 10_000)
                .await
                .unwrap()
                .into_iter()
                .filter_map(|event| event.source_event_id)
                .collect();
            if ids.iter().any(|id| id.ends_with(last)) || Instant::now() >= deadline {
                return ids;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    fn count(ids: &[String], end: &str) -> usize {
        ids.iter().filter(|id| id.ends_with(end)).count()
    }

    async fn link_of(daemon: &Daemon, uid: &str) -> Option<protocol::event::Link> {
        daemon
            .sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|session| session.session_uid == uid)
            .map(|session| session.link)
    }

    /// **G1: the start's connection is kept as the run's link.** Codex announces the thread
    /// before it answers the first turn, while the row is still `starting`; the rest of the
    /// turn, a command among it, follows. Each fact is filed once, the command's call and
    /// its result as two, and the run reads linked. Once its app-server is gone and the
    /// connection ends, the run ends `exited`.
    #[tokio::test]
    async fn a_started_runs_connection_is_kept_and_each_event_filed_once() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let frames: Vec<serde_json::Value> = LIVE_FRAMES
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let thread = frames[1]["params"]["thread"]["id"].clone();
        let tool = frames[16]["params"]["item"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let turn = frames[29]["params"]["turn"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut script = whole_start();
        script.truncate(3);
        script[2].1 = json!({"result": {"thread": {"id": thread}, "model": "gpt-test"}});
        let (result, ws) = tokio::join!(start(&rig.daemon, run(&rig, "req-kept", &codex)), async {
            let (_, mut ws) = play_app_server(&rig, script).await;
            let first_turn = asked(&mut ws, "turn/start").await;
            tell(&mut ws, &frames[1]).await;
            answer(&mut ws, &first_turn, json!({"turn": {"id": turn}})).await;
            for frame in &frames[2..] {
                tell(&mut ws, frame).await;
            }
            ws
        });
        let uid = started_uid(&result);
        let ids = filed_until(&rig.daemon, &uid, &format!(":turn:{turn}")).await;
        assert_eq!(count(&ids, ":thread_started"), 1, "{ids:?}");
        assert_eq!(count(&ids, &format!(":pre:{tool}")), 1, "{ids:?}");
        assert_eq!(count(&ids, &format!(":post:{tool}")), 1, "{ids:?}");
        for message in [&frames[10], &frames[25]] {
            let item = message["params"]["item"]["id"].as_str().unwrap();
            assert_eq!(count(&ids, &format!(":item:{item}")), 1, "{ids:?}");
        }
        assert_eq!(count(&ids, &format!(":turn:{turn}")), 1, "{ids:?}");
        assert_eq!(
            link_of(&rig.daemon, &uid).await,
            Some(protocol::event::Link::Attached)
        );
        let leader = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .host
            .pid;
        end_run(&rig, &uid).await;
        assert!(group_ends(leader).await);
        drop(ws);
        assert_eq!(
            row_once(&rig, &uid, Lifecycle::Exited).await.lifecycle,
            Lifecycle::Exited
        );
    }

    /// **A linked Codex run whose leader dies alone is ended by the sweep** (X2 decision 1's
    /// npm-launcher case): with the launcher killed, its child still in the group and the
    /// connection still open, the liveness sweep runs the ladder on the group and ends the run
    /// `exited`.
    #[tokio::test]
    async fn a_linked_codex_run_whose_leader_dies_alone_is_ended_by_the_sweep() {
        let rig = rig();
        let codex = stand_in(&rig, "sleep 30 &\necho $! > child\nwait");
        let (result, (_, _ws)) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-leader-dies", &codex)),
            play_app_server(&rig, whole_start())
        );
        let uid = started_uid(&result);
        row_once(&rig, &uid, Lifecycle::Live).await;
        assert_eq!(
            link_of(&rig.daemon, &uid).await,
            Some(protocol::event::Link::Attached)
        );
        let dir = run_folder(&rig).await;
        while !dir.join("child").exists() {
            tokio::time::sleep(POLL).await;
        }
        let leader = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .host;
        // SAFETY: kill(2) of the rig's own stand-in leader, which this test launched.
        unsafe { libc::kill(leader.pid, libc::SIGKILL) };
        tokio::time::timeout(Duration::from_secs(10), async {
            while protocol::proc_identity::liveness(&leader)
                != protocol::proc_identity::Liveness::Gone
            {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the killed leader is gone");
        assert!(group_alive(leader.pid), "the leader's child left its group");
        rig.daemon.watch_hosted_runs().await;
        assert!(
            group_ends(leader.pid).await,
            "the orphaned child outlived the sweep"
        );
        assert_eq!(
            row_once(&rig, &uid, Lifecycle::Exited).await.lifecycle,
            Lifecycle::Exited
        );
    }

    /// **G2: a `live` run is connected again at a restart.** The restarted ccd resumes the
    /// run's thread with the preset and its thread config, and files the answer. The answer
    /// reports a turn still running, whose command finished while no ccd listened; when that
    /// turn ends, ccd resumes once more and files the command, once.
    #[tokio::test]
    async fn a_live_run_is_connected_again_at_a_restart_and_its_running_turn_followed_up() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let thread = capture["answerMidTurn"]["result"]["thread"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let after = &capture["answerAfter"]["result"]["thread"]["turns"];
        let item_of = |kind: &str| {
            after[1]["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == kind)
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let (command, message) = (item_of("commandExecution"), item_of("agentMessage"));
        let turns = [after[0]["id"].clone(), after[1]["id"].clone()];
        let mut script = whole_start();
        script[2].1 = json!({"result": {"thread": {"id": thread}, "model": "gpt-test"}});
        let (result, (_, first)) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-again", &codex)),
            play_app_server(&rig, script)
        );
        let uid = started_uid(&result);
        row_once(&rig, &uid, Lifecycle::Live).await;
        // The first ccd's connection ends with it; the app-server lives on, listening anew.
        drop(first);
        let dir = run_folder(&rig).await;
        std::fs::remove_file(dir.join("as.sock")).unwrap();
        let listener = UnixListener::bind(dir.join("as.sock")).unwrap();
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        let ((), ((first_resume, follow_up), ws)) = tokio::join!(
            restarted.recover_codex_runs(&rig.root, Ok(PathBuf::from(CODECONNECT))),
            async {
                let (stream, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
                    .await
                    .expect("ccd connected again")
                    .unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let init = asked(&mut ws, "initialize").await;
                answer(&mut ws, &init, json!({"userAgent": "stand-in"})).await;
                let skills = asked(&mut ws, "skills/list").await;
                answer(&mut ws, &skills, json!({"data": []})).await;
                let first_resume = asked(&mut ws, "thread/resume").await;
                answer(
                    &mut ws,
                    &first_resume,
                    capture["answerMidTurn"]["result"].clone(),
                )
                .await;
                for frame in capture["live"]["afterTheMidTurnAnswer"].as_array().unwrap() {
                    tell(&mut ws, frame).await;
                }
                let follow_up = asked(&mut ws, "thread/resume").await;
                answer(
                    &mut ws,
                    &follow_up,
                    capture["answerAfter"]["result"].clone(),
                )
                .await;
                ((first_resume, follow_up), ws)
            }
        );
        for resume in [&first_resume, &follow_up] {
            let params = &resume["params"];
            assert_eq!(params["threadId"], thread.as_str());
            assert_eq!(params["approvalPolicy"], "on-request");
            assert_eq!(params["sandbox"], "workspace-write");
            assert!(params["config"]["projects"].is_object(), "{params}");
            assert!(
                params["config"]["mcp_servers.codeconnect"].is_object(),
                "{params}"
            );
        }
        let ids = filed_until(&restarted, &uid, &format!(":post:{command}")).await;
        assert_eq!(count(&ids, &format!(":post:{command}")), 1, "{ids:?}");
        assert_eq!(
            ids.iter().filter(|id| id.contains(&message)).count(),
            1,
            "{ids:?}"
        );
        for turn in &turns {
            assert_eq!(
                count(&ids, &format!(":turn:{}", turn.as_str().unwrap())),
                1,
                "{ids:?}"
            );
        }
        assert_eq!(
            link_of(&restarted, &uid).await,
            Some(protocol::event::Link::Attached)
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    const FILE_FRAMES: &str = include_str!("../../../fixtures/codex/file-change.jsonl");
    /// The approval codex 0.167.0 asked for codeconnect's `send_message` (cx/x7 P7-6b).
    const MAIL_APPROVAL: &str =
        include_str!("../../../fixtures/codex/mcp-tool-approval-0.167.0.json");
    /// A resume answer for a thread whose one turn still runs, as codex 0.164.0 sent it.
    const RUNNING_TURN: &str =
        include_str!("../../../fixtures/codex/resume-running-tool-turn-0.164.0.json");

    fn frames_of(capture: &str) -> Vec<serde_json::Value> {
        capture
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// A run started on a stand-in, its first turn `turn` on `thread` answered: its uid and
    /// the stand-in's end of the run's link.
    async fn engine_run(rig: &Rig, request_id: &str, thread: &str, turn: &str) -> (String, Socket) {
        engine_run_on(rig, request_id, thread, turn, "exec sleep 30").await
    }

    /// [`engine_run`] on a stand-in running `stand_in_script`.
    async fn engine_run_on(
        rig: &Rig,
        request_id: &str,
        thread: &str,
        turn: &str,
        stand_in_script: &str,
    ) -> (String, Socket) {
        let codex = stand_in(rig, stand_in_script);
        let mut script = whole_start();
        script.truncate(3);
        script[2].1 = json!({"result": {"thread": {"id": thread}, "model": "gpt-test"}});
        let (result, ws) = tokio::join!(start(&rig.daemon, run(rig, request_id, &codex)), async {
            let (_, mut ws) = play_app_server(rig, script).await;
            let first_turn = asked(&mut ws, "turn/start").await;
            answer(&mut ws, &first_turn, json!({"turn": {"id": turn}})).await;
            ws
        });
        let uid = started_uid(&result);
        row_once(rig, &uid, Lifecycle::Live).await;
        (uid, ws)
    }

    /// The run's stored cards once there are `n`, within 10 s.
    async fn cards_of(daemon: &Daemon, uid: &str, n: usize) -> Vec<protocol::ws::ApprovalCard> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let rows = daemon
                .db
                .codex_pending_approvals(uid.to_string())
                .await
                .unwrap();
            if rows.len() == n || Instant::now() >= deadline {
                return rows
                    .iter()
                    .map(|row| serde_json::from_str(&row.card).unwrap())
                    .collect();
            }
            tokio::time::sleep(POLL).await;
        }
    }

    fn option_ids(card: &protocol::ws::ApprovalCard) -> Vec<String> {
        card.tool_input["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["id"].as_str().unwrap().to_string())
            .collect()
    }

    /// ccd's next answer to the request `id`, within `within`; `None` if none comes.
    async fn response_to(
        ws: &mut Socket,
        id: &serde_json::Value,
        within: Duration,
    ) -> Option<serde_json::Value> {
        tokio::time::timeout(within, async {
            loop {
                let frame = next_frame(ws).await?;
                if &frame["id"] == id && frame.get("method").is_none() {
                    return Some(frame);
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    async fn answered(
        daemon: &Daemon,
        uid: &str,
        card: &protocol::ws::ApprovalCard,
        option: &str,
    ) -> protocol::ws::AnswerResult {
        daemon
            .answer(
                &card.request_id,
                &card.payload_hash,
                AnswerDecision::OptionId {
                    option_id: option.into(),
                },
                Some(uid),
                protocol::ws::AnsweredOn::Iphone,
            )
            .await
    }

    /// A phone answering `card` with `option` in a task of its own, since the answer is told
    /// its outcome only when Codex resolves the request or the link ends.
    fn answering(
        daemon: &Arc<Daemon>,
        uid: &str,
        card: &protocol::ws::ApprovalCard,
        option: &'static str,
    ) -> tokio::task::JoinHandle<protocol::ws::AnswerResult> {
        let (daemon, uid, card) = (Arc::clone(daemon), uid.to_string(), card.clone());
        tokio::spawn(async move { answered(&daemon, &uid, &card, option).await })
    }

    async fn ledger(daemon: &Daemon, uid: &str, id: &str) -> Option<AnswerStatus> {
        daemon
            .db
            .mutation_status(crate::store::OPERATION_ANSWER, uid.into(), id.into())
            .await
            .unwrap()
            .map(|state| state.status)
    }

    fn resolved(thread: &serde_json::Value, id: &serde_json::Value) -> serde_json::Value {
        json!({"method": "serverRequest/resolved", "params": {"threadId": thread, "requestId": id}})
    }

    /// **G1 (X7a): a command approval becomes a card, and a phone's answer reaches Codex.**
    /// The card offers Codex's decisions within decision 175's list, here accept and cancel and
    /// never the amendment. A phone's cancel is written to the request's own id, and Codex's
    /// resolved notice retires the card once, settles the claim delivered and tells the phone.
    #[tokio::test]
    async fn a_command_approval_becomes_a_card_and_a_phones_answer_reaches_codex() {
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[1]["params"]["thread"]["id"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-card", thread, turn).await;
        for frame in &frames[1..=17] {
            tell(&mut ws, frame).await;
        }
        let cards = cards_of(&rig.daemon, &uid, 1).await;
        assert_eq!(cards.len(), 1);
        assert_eq!(option_ids(&cards[0]), ["accept", "cancel"]);
        let id = &frames[17]["id"];
        let (result, response) =
            tokio::join!(answered(&rig.daemon, &uid, &cards[0], "cancel"), async {
                let response = response_to(&mut ws, id, Duration::from_secs(20)).await;
                tell(&mut ws, &frames[18]).await;
                response
            });
        assert_eq!(
            response,
            Some(json!({"id": id, "result": {"decision": "cancel"}}))
        );
        assert!(
            matches!(result, protocol::ws::AnswerResult::Applied { .. }),
            "{result:?}"
        );
        assert!(cards_of(&rig.daemon, &uid, 0).await.is_empty());
        let card = &cards[0].request_id;
        let ids = filed_until(&rig.daemon, &uid, &format!("resolved:{card}")).await;
        assert_eq!(count(&ids, &format!("perm:{card}")), 1, "{ids:?}");
        assert_eq!(count(&ids, &format!("resolved:{card}")), 1, "{ids:?}");
        assert_eq!(
            ledger(&rig.daemon, &uid, card).await,
            Some(AnswerStatus::Settled("delivered".into()))
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **G1f (X7a): a file-change card carries the changes its item started with.** The
    /// request names only the item; the card's changes come from the item's `item/started`.
    #[tokio::test]
    async fn a_file_change_card_carries_the_changes_its_item_started_with() {
        let rig = rig();
        let frames = frames_of(FILE_FRAMES);
        let thread = frames[1]["params"]["thread"]["id"].as_str().unwrap();
        let turn = frames[43]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-patch", thread, turn).await;
        for frame in &frames[1..=19] {
            tell(&mut ws, frame).await;
        }
        let cards = cards_of(&rig.daemon, &uid, 1).await;
        assert_eq!(cards.len(), 1);
        assert!(
            !cards[0].tool_input["changes"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{}",
            cards[0].tool_input
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **G1m (X7a): codeconnect's own Mail approval becomes a card, and "Allow for this
    /// session" is answered as Codex reads it** (P7-6b's request as codex 0.167.0 sent it). It
    /// has no item id, so it is keyed by the request's id; it offers Allow, Allow for this
    /// session and Deny, never "always".
    #[tokio::test]
    async fn a_mail_approval_becomes_a_card_and_allow_for_this_session_is_sent_as_codex_reads_it() {
        let rig = rig();
        let request: serde_json::Value = serde_json::from_str(MAIL_APPROVAL).unwrap();
        let thread = request["params"]["threadId"].as_str().unwrap();
        let turn = request["params"]["turnId"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-mail", thread, turn).await;
        tell(&mut ws, &request).await;
        let cards = cards_of(&rig.daemon, &uid, 1).await;
        assert_eq!(cards.len(), 1);
        assert_eq!(
            option_ids(&cards[0]),
            ["accept", "acceptForSession", "decline"]
        );
        let id = &request["id"];
        let (result, response) = tokio::join!(
            answered(&rig.daemon, &uid, &cards[0], "acceptForSession"),
            async {
                let response = response_to(&mut ws, id, Duration::from_secs(20)).await;
                tell(&mut ws, &resolved(&request["params"]["threadId"], id)).await;
                response
            }
        );
        assert_eq!(
            response,
            Some(json!({"id": id, "result":
                {"action": "accept", "content": null, "_meta": {"persist": "session"}}}))
        );
        assert!(
            matches!(result, protocol::ws::AnswerResult::Applied { .. }),
            "{result:?}"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **G1b (X7a): a request with no card is answered at once with an error**, so no turn
    /// waits on it: a question, an MCP form, a request ccd has no use for, and an unknown one.
    /// A stand-in that waited for these answers before going on would wait for ever without
    /// them.
    #[tokio::test]
    async fn a_request_with_no_card_is_answered_at_once_with_an_error() {
        let rig = rig();
        let (thread, turn) = ("thread-b", "turn-b");
        let (uid, mut ws) = engine_run(&rig, "req-none", thread, turn).await;
        let params = json!({"threadId": thread, "turnId": turn, "itemId": "item-b"});
        let requests = [
            json!({"id": 1, "method": "item/tool/requestUserInput", "params": params}),
            json!({"id": 2, "method": "mcpServer/elicitation/request", "params":
                {"threadId": thread, "serverName": "theirs", "mode": "form", "message": "Name?",
                 "requestedSchema": {"type": "object", "properties": {}}}}),
            json!({"id": 3, "method": "currentTime/read", "params": {}}),
            json!({"id": 4, "method": "something/new", "params": params}),
        ];
        for request in &requests {
            tell(&mut ws, request).await;
            let response = response_to(&mut ws, &request["id"], Duration::from_secs(20)).await;
            assert!(
                response.as_ref().is_some_and(|r| r.get("error").is_some()),
                "{} got {response:?}",
                request["method"]
            );
        }
        assert!(cards_of(&rig.daemon, &uid, 0).await.is_empty());
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **G1e (X7a): a card ends with its item or its turn when no resolved notice comes on
    /// this connection** (P7-2: the notice can go to a connection that has since closed).
    #[tokio::test]
    async fn a_card_ends_with_its_item_or_its_turn_when_no_resolved_notice_comes() {
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[1]["params"]["thread"]["id"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        // The item's end: the capture with its resolved notice (frame 18) left out.
        for ending in [&frames[19..=20], &frames[29..=29]] {
            let rig = rig();
            let (uid, mut ws) = engine_run(&rig, "req-end", thread, turn).await;
            for frame in &frames[1..=17] {
                tell(&mut ws, frame).await;
            }
            assert_eq!(cards_of(&rig.daemon, &uid, 1).await.len(), 1);
            for frame in ending {
                tell(&mut ws, frame).await;
            }
            assert!(
                cards_of(&rig.daemon, &uid, 0).await.is_empty(),
                "the card outlived {}",
                ending.last().unwrap()["method"]
            );
            drop(ws);
            end_run(&rig, &uid).await;
        }
    }

    /// The approval of `capture`'s frame `request` (with its `item/started` at `started`),
    /// moved onto the running turn of [`RUNNING_TURN`]'s thread so a restart's resume can
    /// describe it.
    fn on_the_running_turn(
        capture: &[serde_json::Value],
        started: usize,
        request: usize,
    ) -> (String, String, serde_json::Value, serde_json::Value) {
        let resume: serde_json::Value = serde_json::from_str(RUNNING_TURN).unwrap();
        let thread = resume["result"]["thread"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let turn = resume["result"]["thread"]["turns"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut item = capture[started].clone();
        item["params"]["threadId"] = json!(thread);
        item["params"]["turnId"] = json!(turn);
        let mut asked = capture[request].clone();
        asked["params"]["threadId"] = json!(thread);
        asked["params"]["turnId"] = json!(turn);
        (thread, turn, item, asked)
    }

    /// A ccd restart under the run: the first link's connection closed, the app-server
    /// listening anew, a fresh daemon on the same store recovering, its reconnect answered
    /// with [`RUNNING_TURN`]. The restarted daemon and the stand-in's new end of the link.
    async fn restarted_under(rig: &Rig, first: Socket) -> (Arc<Daemon>, Socket) {
        drop(first);
        let resume: serde_json::Value = serde_json::from_str(RUNNING_TURN).unwrap();
        let dir = run_folder(rig).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::remove_file(dir.join("as.sock")).unwrap();
        let listener = UnixListener::bind(dir.join("as.sock")).unwrap();
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        let ((), ws) = tokio::join!(
            restarted.recover_codex_runs(&rig.root, Ok(PathBuf::from(CODECONNECT))),
            async {
                let (stream, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
                    .await
                    .expect("ccd connected again")
                    .unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let init = asked(&mut ws, "initialize").await;
                answer(&mut ws, &init, json!({"userAgent": "stand-in"})).await;
                let skills = asked(&mut ws, "skills/list").await;
                answer(&mut ws, &skills, json!({"data": []})).await;
                let resumed = asked(&mut ws, "thread/resume").await;
                answer(&mut ws, &resumed, resume["result"].clone()).await;
                ws
            }
        );
        (restarted, ws)
    }

    /// **R6 (X7a): an answer in flight across a restart is sent once more to Codex's
    /// replay** (97's R6 (i)). A file-change card is answered and ccd's connection ends
    /// before Codex resolves it. Startup leaves the claim `applying`; the reconnected link
    /// binds Codex's replay to the stored card by its id (with no `item/started` this time,
    /// so the request could not be read again) and sends the claimed answer once. Another
    /// device's answer meanwhile is refused; the resolved notice settles the claim delivered.
    #[tokio::test]
    async fn an_answer_in_flight_at_a_restart_is_sent_once_more_to_codexs_replay() {
        let rig = rig();
        let frames = frames_of(FILE_FRAMES);
        let (thread, turn, started, asked_once) = on_the_running_turn(&frames, 17, 19);
        let id = asked_once["id"].clone();
        let (uid, mut ws) = engine_run(&rig, "req-r6", &thread, &turn).await;
        tell(&mut ws, &started).await;
        tell(&mut ws, &asked_once).await;
        let cards = cards_of(&rig.daemon, &uid, 1).await;
        assert_eq!(cards.len(), 1);
        let phone = answering(&rig.daemon, &uid, &cards[0], "accept");
        let first = response_to(&mut ws, &id, Duration::from_secs(20)).await;
        assert_eq!(
            first,
            Some(json!({"id": id, "result": {"decision": "accept"}}))
        );
        assert_eq!(
            ledger(&rig.daemon, &uid, &cards[0].request_id).await,
            Some(AnswerStatus::Applying)
        );
        let (restarted, mut ws) = restarted_under(&rig, ws).await;
        assert!(
            matches!(
                phone.await.unwrap(),
                protocol::ws::AnswerResult::Rejected { .. }
            ),
            "the first phone hears its answer's fate is unknown when the link ends"
        );
        tell(&mut ws, &asked_once).await;
        let again = response_to(&mut ws, &id, Duration::from_secs(20)).await;
        assert_eq!(
            again,
            Some(json!({"id": id, "result": {"decision": "accept"}}))
        );
        let other = answering(&restarted, &uid, &cards[0], "cancel");
        assert_eq!(
            response_to(&mut ws, &id, Duration::from_secs(3)).await,
            None,
            "a second answer was written to the request"
        );
        let other = tokio::time::timeout(Duration::from_secs(10), other)
            .await
            .expect("the other device was answered")
            .unwrap();
        assert!(
            matches!(other, protocol::ws::AnswerResult::Rejected { .. }),
            "{other:?}"
        );
        tell(&mut ws, &resolved(&json!(thread), &id)).await;
        assert!(cards_of(&restarted, &uid, 0).await.is_empty());
        assert_eq!(
            ledger(&restarted, &uid, &cards[0].request_id).await,
            Some(AnswerStatus::Settled("delivered".into()))
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **A Codex card a restart restored closes as a Codex card when its run ends.** A run
    /// started on the engine has a hosted row, as a Claude run in an agent host does; the
    /// card a ccd restart brings back is still its link's, so the run's end retires it as
    /// a Codex card and its stored row goes, rather than closing it as a host's card.
    #[tokio::test]
    async fn a_restored_codex_card_closes_as_a_codex_card_when_its_run_ends() {
        let rig = rig();
        let frames = frames_of(FILE_FRAMES);
        let (thread, turn, started, asked_once) = on_the_running_turn(&frames, 17, 19);
        let (uid, mut ws) = engine_run(&rig, "req-restored", &thread, &turn).await;
        tell(&mut ws, &started).await;
        tell(&mut ws, &asked_once).await;
        assert_eq!(cards_of(&rig.daemon, &uid, 1).await.len(), 1);
        let (restarted, ws) = restarted_under(&rig, ws).await;
        restarted
            .end_hosted_run(
                &uid,
                None,
                Lifecycle::Live,
                Lifecycle::Exited,
                json!({"exit_code": null, "reason": "Stopped: its process ended"}),
            )
            .await
            .unwrap();
        assert!(
            cards_of(&restarted, &uid, 0).await.is_empty(),
            "the restored card outlived its run"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **R6 (X7a), two restarts in a row: one actuation.** The answer sent again after the
    /// first restart lands; ccd stops before the resolved notice; after the second restart
    /// Codex has nothing to replay and ccd sends nothing, and the item's end settles the
    /// claim delivered.
    #[tokio::test]
    async fn two_restarts_in_a_row_still_send_an_answer_once() {
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let (thread, turn, started, asked_once) = on_the_running_turn(&frames, 16, 17);
        let id = asked_once["id"].clone();
        let (uid, mut ws) = engine_run(&rig, "req-twice", &thread, &turn).await;
        tell(&mut ws, &started).await;
        tell(&mut ws, &asked_once).await;
        let cards = cards_of(&rig.daemon, &uid, 1).await;
        assert_eq!(cards.len(), 1);
        let _phone = answering(&rig.daemon, &uid, &cards[0], "accept");
        assert!(response_to(&mut ws, &id, Duration::from_secs(20))
            .await
            .is_some());
        let (_, mut ws) = restarted_under(&rig, ws).await;
        tell(&mut ws, &asked_once).await;
        assert!(response_to(&mut ws, &id, Duration::from_secs(20))
            .await
            .is_some());
        let (restarted, mut ws) = restarted_under(&rig, ws).await;
        assert_eq!(
            response_to(&mut ws, &id, Duration::from_secs(3)).await,
            None
        );
        let mut done = frames[20].clone();
        done["params"]["threadId"] = json!(thread);
        done["params"]["turnId"] = json!(turn);
        tell(&mut ws, &done).await;
        assert!(cards_of(&restarted, &uid, 0).await.is_empty());
        assert_eq!(
            ledger(&restarted, &uid, &cards[0].request_id).await,
            Some(AnswerStatus::Settled("delivered".into()))
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// `git -C dir <args>`, quietly.
    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// **The trust list is every folder from the repository's top down to the run's, by
    /// their real paths, each untrusted, or the run's folder alone outside git**; a path
    /// is one TOML string however it is spelled.
    #[tokio::test]
    async fn the_trust_list_is_the_real_folders_from_the_top_down() {
        let rig = rig();
        let top = rig.root.join("repo");
        let deep = top.join("a b").join("q\"uo\\te");
        std::fs::create_dir_all(&deep).unwrap();
        git(&top, &["init", "-q", "-b", "main"]);
        let real = |path: &Path| {
            serde_json::to_string(&std::fs::canonicalize(path).unwrap().display().to_string())
                .unwrap()
        };
        let entry = |path: &Path| format!(r#"{} = {{ trust_level = "untrusted" }}"#, real(path));
        let projects = |cwd: PathBuf| async move {
            folders(cwd.to_str().unwrap(), None)
                .await
                .map(|folders| launch_projects(&folders))
        };
        assert_eq!(
            projects(deep.clone()).await.unwrap(),
            format!(
                "{{ {}, {}, {} }}",
                entry(&top),
                entry(&top.join("a b")),
                entry(&deep)
            )
        );
        let plain = rig.root.join("project");
        assert_eq!(
            projects(plain.clone()).await.unwrap(),
            format!("{{ {} }}", entry(&plain))
        );
        assert!(
            real(&plain).starts_with("\"/private/tmp/") || !cfg!(target_os = "macos"),
            "the folder is listed by its real path: {}",
            real(&plain)
        );
        assert!(projects(PathBuf::from("/nonexistent/folder"))
            .await
            .is_err());
    }

    /// The run's one `SessionEnd` payload.
    fn the_end(rig: &Rig, uid: &str) -> serde_json::Value {
        let store = rusqlite::Connection::open(rig.root.join("events.db")).unwrap();
        let mut statement = store
            .prepare("SELECT payload FROM events WHERE session_uid = ?1 AND kind = 'session_end'")
            .unwrap();
        let ends: Vec<String> = statement
            .query_map([uid], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(ends.len(), 1, "{ends:?}");
        serde_json::from_str(&ends[0]).unwrap()
    }

    /// Codex's notice that `turn` started on `thread`.
    fn turn_started(thread: &str, turn: &str) -> serde_json::Value {
        json!({"method": "turn/started", "params": {"threadId": thread,
               "turn": {"id": turn, "items": [], "status": "inProgress"}}})
    }

    /// The captured run's turn end (`LIVE_FRAMES`' frame 29), ended `status`.
    fn its_turn_ended(status: &str) -> serde_json::Value {
        let mut ended = frames_of(LIVE_FRAMES)[29].clone();
        ended["params"]["turn"]["status"] = json!(status);
        ended
    }

    /// A phone's Stop of the run's `turn` in a task of its own, since it is told its outcome
    /// only when the turn ends.
    fn stopping(
        daemon: &Arc<Daemon>,
        uid: &str,
        request_id: &str,
        turn: &str,
    ) -> tokio::task::JoinHandle<protocol::ws::InterruptResult> {
        let (daemon, uid, request_id, turn) = (
            Arc::clone(daemon),
            uid.to_string(),
            request_id.to_string(),
            turn.to_string(),
        );
        tokio::spawn(async move {
            let hash = protocol::hash::interrupt_hash(&uid, &turn);
            daemon.interrupt(&uid, &request_id, &turn, &hash).await
        })
    }

    /// **X8-1 G1 and G8: End run ends a run started on the engine, which can then be
    /// deleted.** The answer is `Stopping` and the run `stopping`; the link sends the
    /// app-server's group SIGTERM, the group empties, and once the app-server's connection
    /// closes the run ends `exited`, "Stopped", its card closed. Delete is refused while the
    /// run runs and removes it once it has ended.
    #[tokio::test]
    async fn end_run_ends_a_run_started_on_the_engine_which_can_then_be_deleted() {
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[2]["params"]["threadId"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-end", thread, turn).await;
        for frame in &frames[1..=17] {
            tell(&mut ws, frame).await;
        }
        assert_eq!(cards_of(&rig.daemon, &uid, 1).await.len(), 1);
        assert_eq!(
            rig.daemon.delete_exited_session(&uid).await.unwrap(),
            protocol::ws::DeleteSessionResult::StillRunning
        );
        let leader = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .host
            .pid;
        assert_eq!(
            rig.daemon.stop_run(&uid).await,
            protocol::ws::StopRunResult::Stopping
        );
        assert_eq!(
            row_once(&rig, &uid, Lifecycle::Stopping).await.lifecycle,
            Lifecycle::Stopping
        );
        assert!(
            group_ends(leader).await,
            "the app-server's group outlived its SIGTERM"
        );
        drop(ws);
        assert_eq!(
            row_once(&rig, &uid, Lifecycle::Exited).await.lifecycle,
            Lifecycle::Exited
        );
        assert_eq!(
            the_end(&rig, &uid),
            json!({"exit_code": null, "reason": "Stopped"})
        );
        assert!(cards_of(&rig.daemon, &uid, 0).await.is_empty());
        assert!(matches!(
            rig.daemon.delete_exited_session(&uid).await.unwrap(),
            protocol::ws::DeleteSessionResult::Deleted { .. }
        ));
    }

    /// **X8-1 G5: a phone's Stop interrupts the turn waiting on a card.** The run's link
    /// claims the ask and sends `turn/interrupt` for that turn on its thread. Codex answers,
    /// then ends the turn `interrupted`, resolving the card's request; the phone is told
    /// `Aborted` with the turn, and the card closes. The same ask again is answered from the
    /// record, with nothing sent, by the daemon and by the link itself, which a retry the
    /// daemon let through while the first was in flight reaches.
    #[tokio::test]
    async fn a_phones_stop_interrupts_the_turn_waiting_on_a_card() {
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[2]["params"]["threadId"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-stop", thread, turn).await;
        tell(&mut ws, &turn_started(thread, turn)).await;
        for frame in &frames[1..=17] {
            tell(&mut ws, frame).await;
        }
        assert_eq!(cards_of(&rig.daemon, &uid, 1).await.len(), 1);
        let stop = stopping(&rig.daemon, &uid, "req-stop-1", turn);
        let interrupt = asked(&mut ws, "turn/interrupt").await;
        assert_eq!(
            interrupt["params"],
            json!({"threadId": thread, "turnId": turn})
        );
        answer(&mut ws, &interrupt, json!({})).await;
        tell(&mut ws, &resolved(&json!(thread), &frames[17]["id"])).await;
        tell(&mut ws, &its_turn_ended("interrupted")).await;
        let aborted = protocol::ws::InterruptResult::Aborted {
            turn_id: turn.to_string(),
        };
        assert_eq!(stop.await.unwrap(), aborted);
        assert!(cards_of(&rig.daemon, &uid, 0).await.is_empty());
        let duplicate = protocol::ws::InterruptResult::Duplicate {
            turn_id: turn.to_string(),
        };
        assert_eq!(
            stopping(&rig.daemon, &uid, "req-stop-1", turn)
                .await
                .unwrap(),
            duplicate
        );
        let (reply, told) = tokio::sync::oneshot::channel();
        let commands = rig.daemon.engine_commands_for_tests(&uid).await.unwrap();
        assert!(commands
            .send(LinkCommand::Interrupt {
                request_id: "req-stop-1".into(),
                turn_id: turn.into(),
                reply,
            })
            .is_ok());
        assert_eq!(
            Daemon::interrupt_result_for_tests(told.await.unwrap()),
            duplicate,
            "the link itself"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **X8-1 G5, a turn that finishes first: a Stop whose turn then ends on its own is told
    /// so.** Codex answers an interrupt it holds at the turn's end either way, so its answer
    /// says nothing about the turn; the turn's `completed` end does.
    #[tokio::test]
    async fn a_stop_whose_turn_then_completes_is_told_it_ended_on_its_own() {
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[2]["params"]["threadId"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-finish", thread, turn).await;
        tell(&mut ws, &turn_started(thread, turn)).await;
        let stop = stopping(&rig.daemon, &uid, "req-finish-1", turn);
        let interrupt = asked(&mut ws, "turn/interrupt").await;
        answer(&mut ws, &interrupt, json!({})).await;
        tell(&mut ws, &its_turn_ended("completed")).await;
        assert_eq!(
            stop.await.unwrap(),
            protocol::ws::InterruptResult::Rejected {
                reason: crate::codex_refusals::interrupt_turn_ended_itself("completed")
            }
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **X8-1 G5, two Stops at once: a Stop for a turn whose interrupt Codex has taken is
    /// refused at once, and nothing more is sent.** The first is told when the turn ends.
    #[tokio::test]
    async fn a_second_stop_while_the_first_waits_is_refused_at_once_and_sends_nothing() {
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[2]["params"]["threadId"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-two", thread, turn).await;
        tell(&mut ws, &turn_started(thread, turn)).await;
        let first = stopping(&rig.daemon, &uid, "req-two-1", turn);
        let interrupt = asked(&mut ws, "turn/interrupt").await;
        answer(&mut ws, &interrupt, json!({})).await;
        let second = tokio::time::timeout(
            Duration::from_secs(5),
            stopping(&rig.daemon, &uid, "req-two-2", turn),
        )
        .await
        .expect("the second Stop was not answered at once");
        assert_eq!(
            second.unwrap(),
            protocol::ws::InterruptResult::Rejected {
                reason: crate::codex_refusals::INTERRUPT_ALREADY_WAITING.into()
            }
        );
        tell(&mut ws, &its_turn_ended("interrupted")).await;
        assert_eq!(
            first.await.unwrap(),
            protocol::ws::InterruptResult::Aborted {
                turn_id: turn.to_string()
            }
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(500), next_frame(&mut ws))
                .await
                .is_err(),
            "ccd sent Codex something more"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **X8-1 G2, in Codex's order: End run ends the run once its app-server exits, though it
    /// closes its connection first.** The stand-in takes SIGTERM, its connection closes, and it
    /// exits a second later; the run is `exited`, "Stopped", well before the escalation's 10 s.
    #[tokio::test]
    async fn end_run_ends_the_run_when_codex_exits_after_closing_its_connection() {
        let rig = rig();
        let (uid, ws) = engine_run_on(
            &rig,
            "req-drain",
            "thr-drain",
            "turn-1",
            "trap 'touch got-term; sleep 1; exit 0' TERM\nsleep 30 &\nwait",
        )
        .await;
        let asked = Instant::now();
        assert_eq!(
            rig.daemon.stop_run(&uid).await,
            protocol::ws::StopRunResult::Stopping
        );
        let term = run_folder(&rig).await.join("got-term");
        while !term.exists() && asked.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(POLL).await;
        }
        assert!(term.exists(), "the app-server was not sent SIGTERM");
        drop(ws);
        assert_eq!(
            row_once(&rig, &uid, Lifecycle::Exited).await.lifecycle,
            Lifecycle::Exited
        );
        assert!(
            asked.elapsed() < Duration::from_secs(5),
            "the run ended {:?} after End run",
            asked.elapsed()
        );
        assert_eq!(
            the_end(&rig, &uid),
            json!({"exit_code": null, "reason": "Stopped"})
        );
    }

    /// **X8-1 G6: a run `stopping` across a ccd restart is ended by the restarted ccd.** Its
    /// app-server ignores SIGTERM. The restarted daemon's recovery starts the run's
    /// escalation again, whose ladder ends the group 10 s later, and the run ends `exited`,
    /// "Stopped".
    #[tokio::test]
    async fn a_codex_run_stopping_across_a_restart_is_ended_by_the_restarted_daemon() {
        let rig = rig();
        let codex = stand_in(&rig, "trap '' TERM\nexec sleep 60");
        let (result, (_, ws)) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-stopping", &codex)),
            play_app_server(&rig, whole_start())
        );
        let uid = started_uid(&result);
        row_once(&rig, &uid, Lifecycle::Live).await;
        let leader = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .host
            .pid;
        // The row a stop left when its ccd stopped before escalating.
        assert!(rig
            .daemon
            .db
            .advance_lifecycle(uid.clone(), Lifecycle::Live, Lifecycle::Stopping)
            .await
            .unwrap());
        drop(ws);
        let restarted = daemon_on(&rig.root);
        restarted
            .recover_codex_runs(&rig.root, Ok(PathBuf::from(CODECONNECT)))
            .await;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut lifecycle = Lifecycle::Stopping;
        while lifecycle != Lifecycle::Exited && Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            // The stand-in's parent is this test's process, the ccd that started it. After a
            // real restart that ccd is gone and launchd reaps the app-server at once; here the
            // test reaps it the same way, so its end does not wait on a zombie.
            // SAFETY: waitpid(2) without blocking, on the stand-in this test launched.
            unsafe { libc::waitpid(leader, std::ptr::null_mut(), libc::WNOHANG) };
            // The liveness sweep, as the restarted ccd runs it on its tick: it ends a run whose
            // app-server is gone, which only the escalation's SIGKILL makes this one.
            restarted.watch_hosted_runs().await;
            lifecycle = restarted
                .db
                .get_session(uid.clone())
                .await
                .unwrap()
                .unwrap()
                .lifecycle;
        }
        assert_eq!(lifecycle, Lifecycle::Exited, "the run stayed stopping");
        assert!(
            !group_alive(leader),
            "the app-server outlived the escalation"
        );
        assert_eq!(
            the_end(&rig, &uid),
            json!({"exit_code": null, "reason": "Stopped"})
        );
    }

    /// **X8-1 G7, the engine's half: uninstall's stop ends a run started on the engine**, as
    /// End run does, and no host is left (the Claude half is B7's real-host tests).
    #[tokio::test]
    async fn stopping_the_hosted_runs_ends_a_run_started_on_the_engine() {
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-uninstall", "thr-uninstall", "turn-1").await;
        let leader = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .host
            .pid;
        let (stopped, left) = rig.daemon.stop_hosted_runs(Duration::from_secs(20)).await;
        assert_eq!((stopped, left), (1, Vec::<u32>::new()));
        assert!(!group_alive(leader), "the app-server outlived the stop");
        drop(ws);
    }

    /// Once the run `uid`'s link has detached, within 10 s.
    async fn link_gone(daemon: &Daemon, uid: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon.engine_commands_for_tests(uid).await.is_some() {
            assert!(Instant::now() < deadline, "the run's link did not detach");
            tokio::time::sleep(POLL).await;
        }
    }

    /// **A Stop retried after its run's link has gone is answered from the record.** Codex
    /// took the first and the connection then ended, so its claim is still `applying`: the
    /// retry is told it was sent and what became of it is not known, never that nothing was.
    #[tokio::test]
    async fn a_stop_retried_after_its_link_has_gone_is_answered_from_the_record() {
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[2]["params"]["threadId"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-restop", thread, turn).await;
        tell(&mut ws, &turn_started(thread, turn)).await;
        let first = stopping(&rig.daemon, &uid, "req-restop-1", turn);
        let interrupt = asked(&mut ws, "turn/interrupt").await;
        answer(&mut ws, &interrupt, json!({})).await;
        drop(ws);
        assert!(matches!(
            first.await.unwrap(),
            protocol::ws::InterruptResult::Indeterminate { .. }
        ));
        link_gone(&rig.daemon, &uid).await;
        let claim = rig
            .daemon
            .db
            .mutation_status(
                crate::store::OPERATION_INTERRUPT,
                uid.clone(),
                "req-restop-1".into(),
            )
            .await
            .unwrap()
            .map(|state| state.status);
        assert_eq!(claim, Some(AnswerStatus::Applying));
        assert_eq!(
            stopping(&rig.daemon, &uid, "req-restop-1", turn)
                .await
                .unwrap(),
            protocol::ws::InterruptResult::Indeterminate {
                reason: crate::codex_refusals::INTERRUPT_ALREADY_SENT_UNKNOWN.into()
            }
        );
        end_run(&rig, &uid).await;
    }

    /// **An answer retried after its run's link has gone is answered from the record.** The
    /// first was written and Codex had not confirmed it when the connection ended, so its claim
    /// is still `applying`: the retry is told it is waiting, never that nothing was sent.
    #[tokio::test]
    async fn an_answer_retried_after_its_link_has_gone_is_answered_from_the_record() {
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[2]["params"]["threadId"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-reanswer", thread, turn).await;
        for frame in &frames[1..=17] {
            tell(&mut ws, frame).await;
        }
        let cards = cards_of(&rig.daemon, &uid, 1).await;
        assert_eq!(cards.len(), 1);
        let first = answering(&rig.daemon, &uid, &cards[0], "accept");
        assert!(
            response_to(&mut ws, &frames[17]["id"], Duration::from_secs(10))
                .await
                .is_some(),
            "the first answer was not written"
        );
        drop(ws);
        assert!(matches!(
            first.await.unwrap(),
            protocol::ws::AnswerResult::Rejected { .. }
        ));
        link_gone(&rig.daemon, &uid).await;
        assert_eq!(
            ledger(&rig.daemon, &uid, &cards[0].request_id).await,
            Some(AnswerStatus::Applying)
        );
        assert_eq!(
            answered(&rig.daemon, &uid, &cards[0], "accept").await,
            protocol::ws::AnswerResult::Rejected {
                reason: ANSWER_ALREADY_WAITING.into()
            }
        );
        end_run(&rig, &uid).await;
    }

    /// **X8-1 G9: with its link not attached, an engine run refuses an answer and a Stop with
    /// the fact alone**, before its reconnect and after a reconnect that failed.
    #[tokio::test]
    async fn an_engine_run_with_no_link_refuses_an_answer_and_a_stop_with_the_fact() {
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[2]["params"]["threadId"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-nolink", thread, turn).await;
        for frame in &frames[1..=17] {
            tell(&mut ws, frame).await;
        }
        let cards = cards_of(&rig.daemon, &uid, 1).await;
        assert_eq!(cards.len(), 1);
        drop(ws);
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        let refused = || async {
            let answered = answered(&restarted, &uid, &cards[0], "accept").await;
            let hash = protocol::hash::interrupt_hash(&uid, turn);
            let stopped = restarted
                .interrupt(&uid, "req-nolink-stop", turn, &hash)
                .await;
            (answered, stopped)
        };
        let fact = crate::codex_refusals::ENGINE_NO_CONNECTION.to_string();
        let expected = (
            protocol::ws::AnswerResult::Rejected {
                reason: fact.clone(),
            },
            protocol::ws::InterruptResult::Rejected {
                reason: fact.clone(),
            },
        );
        assert_eq!(refused().await, expected, "before the reconnect");
        let dir = run_folder(&rig).await;
        let _ = std::fs::remove_file(dir.join("as.sock"));
        let live = restarted.db.codex_live_runs().await.unwrap();
        assert_eq!(live.len(), 1);
        reconnect(
            Arc::clone(&restarted),
            rig.root.clone(),
            live.into_iter().next().unwrap(),
            PathBuf::from(CODECONNECT),
        )
        .await;
        assert_eq!(refused().await, expected, "after a failed reconnect");
        end_run(&rig, &uid).await;
    }

    /// **X8-2, G1's clause: End run ends the groups the run's app-server started.** The
    /// stand-in starts a child that leads its own group, as Codex runs a command, and the link
    /// records it. End run's SIGTERM ends the app-server's group alone; once the app-server is
    /// gone, the link's end SIGKILLs the recorded group.
    #[tokio::test]
    async fn end_run_ends_the_groups_its_app_server_started() {
        let rig = rig();
        let (uid, ws) = engine_run_on(
            &rig,
            "req-tools",
            "thr-tools",
            "turn-1",
            "perl -e 'setpgrp(0, 0); sleep 60' &\necho $! > tool.pid\nexec sleep 30",
        )
        .await;
        let tool: i32 = std::fs::read_to_string(run_folder(&rig).await.join("tool.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            group_alive(tool),
            "the stand-in's command leads its own group"
        );
        let leader = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .host
            .pid;
        assert_eq!(
            rig.daemon.stop_run(&uid).await,
            protocol::ws::StopRunResult::Stopping
        );
        assert!(
            group_ends(leader).await,
            "the app-server outlived its SIGTERM"
        );
        assert!(
            group_alive(tool),
            "End run's SIGTERM reached the command's own group"
        );
        drop(ws);
        let ended = group_ends(tool).await;
        if !ended {
            // SAFETY: kill(2) of the group this test's stand-in started, so none is left behind.
            unsafe { libc::kill(-tool, libc::SIGKILL) };
        }
        assert!(ended, "the command's group outlived its app-server");
    }

    /// **X8-2's warrant control: a recorded group whose leader is now another process gets no
    /// signal.** The same group recorded with its leader's true birth is ended.
    #[test]
    fn a_recorded_group_whose_leader_has_another_birth_gets_no_signal() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let recorded = ProcessIdentity {
            pid,
            birth: read_birth_identity(pid).unwrap(),
        };
        let mut another = recorded;
        another.birth.start_usec ^= 0x5A5A;
        let boot = protocol::proc_identity::boot_identity();
        end_tools("warrant", &[another], boot);
        std::thread::sleep(POLL);
        assert!(
            child.try_wait().unwrap().is_none(),
            "a group whose leader has another birth was signalled"
        );
        end_tools("warrant", &[recorded], boot);
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }
}
