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
//! the start's uuid when the start has one, and the run is `live`. The thread config and
//! the launch take the person's folder answer (P3 L4): each folder from the repository's
//! top down to the run's own (or the run's folder alone outside git) is trusted or
//! untrusted as they answered, a not-trusted run's repository skills are disabled too, and
//! the only MCP server added is CodeConnect's mail server. The run keeps that answer for
//! its reconnect. A start that fails once the app-server is launched, before or after the
//! answer, marks the run `failed` and stops its process group with the ladder
//! ([`crate::claude_host::stop_group`]): an app-server never ends by itself. A start that a
//! restarted ccd finds still `starting` is failed the same way, whatever became of it
//! ([`crate::state::Daemon::recover_hosted_runs`]): a start is never sent again.
//!
//! The start's connection is then kept as the run's link ([`link`], P3 R1): every
//! notification it brings is mapped by the Codex adapter and filed, for both apps. When a
//! ccd restarts, each `live` run whose app-server is alive is connected again
//! ([`reconnect`]): its thread resumed with the approval and sandbox Codex last named for it
//! and its thread config, the turns it ran meanwhile filed from the answer, and its link kept
//! from there. An app's Stop, its
//! later messages and End run reach the run through its link too ([`LinkCommand`]). A message
//! is a `turn/start` under its own uuid, which Codex runs as a new turn or adds to the running
//! one; it is settled by what Codex then records of it ([`compose`], [`heard`]).
//!
//! A run whose app-server ended without ccd stopping it ("Stopped: its process ended": a crash,
//! or a Mac restart) is relaunched by the person's next message, never at ccd start
//! ([`relaunch`], P3 R4): its app-server is launched again into the run's folder, its thread
//! resumed in the mode it was in when that is narrower than Default (Read Only or Plan), in
//! Default otherwise, and the message sent on the new link.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::agent::AgentKind;
use protocol::event::{
    Event, EventKind, Lifecycle, MenuMode, MenuModel, PendingEvent, RunMenus, SessionKey, Source,
};
use protocol::host::Trust;
use protocol::proc_identity::{
    group_has_members, group_warrant, liveness, read_birth_identity, BootIdentity, GroupWarrant,
    Liveness, ProcessIdentity,
};
use protocol::ws::{Applies, SettingResult, StartFailure, StartSessionResult};
use serde_json::{json, Value};
use tokio::time::Instant;

use crate::codex_adapter::CodexAdapter;
use crate::codex_approval::{Approval, Family};
use crate::codex_client::{CallError, Client};
use crate::state::{Daemon, HostLink, Retirement};
use crate::store::{AnswerStatus, AnswerTerminal, LiveCodexRun, SessionRow};
use protocol::composite_id::{CompositeId, ServerRequestId};
use protocol::ws::{AnswerDecision, AnsweredOn, ClearCause, CodexResolution, ResolutionActor};

/// How often the start looks for the app-server's socket.
const POLL: Duration = Duration::from_millis(50);

/// The line in front of codex, run by `/bin/sh` as `$0` = its name, `$1` = the run's
/// folder, and codex's own argv after it: claim the run with this shell's pid, which
/// noclobber lets only one writer do, then become codex.
const CLAIM: &str = r#"cd "$1" || exit 1; set -C; echo $$ > as.pid || exit 1; shift; exec "$@""#;

/// One Codex start, as the app asked for it and the checks before the row resolved it. A
/// relaunch's launch is one too, asked for by the message that relaunches the run
/// ([`relaunch`]).
pub(crate) struct Run<'a> {
    /// This daemon's state folder, which holds `runs/`.
    pub root: &'a Path,
    pub request_id: &'a str,
    /// The folder the run starts in.
    pub cwd: &'a str,
    /// The run's first message; empty for a run that starts waiting for one.
    pub prompt: &'a str,
    /// The app's model. Codex's own `default` is sent as none, so the thread starts with
    /// the model its settings name.
    pub model: Option<&'a str>,
    /// The models the start's check listed, each as Codex's `model/list` gives it (hidden
    /// ones left out): kept with the run, the models of its settings menu ([`menus`]), the
    /// model its thread starts on put first if they do not list it ([`keep_started_model`]).
    pub models: &'a [Value],
    /// The codex the start's check found.
    pub codex: &'a Path,
    /// The `codeconnect` binary, which runs the run's mail server.
    pub codeconnect: &'a Path,
    /// The git the trust list's repository read runs.
    pub git_bin: Option<&'a str>,
    /// The start's environment, the one its codex check read: codex gets it as it is,
    /// with only `CODECONNECT_SESSION_UID` added.
    pub environment: &'a HashMap<OsString, OsString>,
    /// The person's folder answer, as the start resolved it: the trust list's level.
    pub trust: Trust,
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
    /// The folder answer the run started with.
    trust: Trust,
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
        cwd: run.cwd.to_string(),
        claude_session_id: None,
        transcript_path: None,
        lifecycle: Lifecycle::Starting,
        created_at: now.clone(),
        updated_at: now,
        agent: AgentKind::Codex,
        codex_thread_id: None,
    };
    let model = run.model.filter(|model| *model != "default");
    let trust = serde_json::to_string(&run.trust).expect("a folder answer is always JSON");
    match daemon
        .db
        .begin_run(row, run.request_id.to_string(), Some(trust))
        .await
    {
        Ok(Some(existing)) => return crate::claude_host::answer_for(existing),
        Ok(None) => {}
        Err(err) => return failed(format!("could not record the run: {err:#}")),
    }
    // Kept where a Claude run keeps its start's answer, so its menu is read the same way; a
    // run whose check listed none offers its modes alone, and one that could not keep them
    // offers no settings.
    if let Err(err) = daemon
        .db
        .keep_hosted_models(uid.clone(), serde_json::json!({ "models": run.models }))
        .await
    {
        crate::log_error!("{uid}: could not keep its models for its settings: {err:#}");
    }
    let deadline = Instant::now() + timeout;
    let mut launched = None;
    let dir = crate::claude_host::run_dir(run.root, &uid);
    let brought_up = match crate::start_session::make_run_dir(run.root, &dir) {
        Ok(log) => bring_up(daemon, &uid, &run, &folders, log, deadline, &mut launched).await,
        Err(err) => Err(format!(
            "could not make the run's folder {}: {err}",
            dir.display()
        )),
    };
    match brought_up {
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
                trust: run.trust,
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
        Ok(_) => Lifecycle::Live,
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
    match sent {
        Err(_) => {
            drop(client);
            stop(&uid, send.leader, send.boot).await;
        }
        Ok((thread, answer)) if advanced => {
            daemon.mail_run_live(&uid).await;
            keep_started_model(&daemon, &uid, &answer).await;
            let mut adapter = CodexAdapter::new(SessionKey::new(&uid, &uid));
            file(&daemon, &uid, adapter.settings_answered(&thread, &answer)).await;
            link(
                daemon,
                uid,
                thread,
                client,
                adapter,
                None,
                HashMap::new(),
                None,
            )
            .await;
        }
        Ok(_) => {}
    }
}

/// The run `uid`'s models with the one its thread started on (its start's `answer` names it)
/// put first when its start's check did not list it: a model the person's Codex config names
/// that Codex does not list, or an account default Codex's online catalog gained after the
/// check kept its signed-in answer. So the run's menu marks the model it is on, and offers it
/// again once another is picked. Codex names only its id: its row's label is the id and it
/// lists no efforts, so while the run is on it no effort is offered (a written miss).
async fn keep_started_model(daemon: &Daemon, uid: &str, answer: &Value) {
    let Some(started) = answer["model"].as_str().filter(|model| !model.is_empty()) else {
        return;
    };
    let mut models = match daemon.db.hosted_models(uid.to_string()).await {
        Ok(Some(models)) => models,
        Ok(None) => return,
        Err(err) => {
            crate::log_error!("{uid}: could not read its models: {err:#}");
            return;
        }
    };
    if models.iter().any(|model| model["model"] == started) {
        return;
    }
    models.insert(0, json!({ "model": started }));
    if let Err(err) = daemon
        .db
        .keep_hosted_models(uid.to_string(), json!({ "models": models }))
        .await
    {
        crate::log_error!("{uid}: could not keep the model it started on: {err:#}");
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

/// **What became of one answer the phone asked this link to write.**
///
/// Three endings, and they differ in what they left behind rather than in how they
/// read. A caller that cannot tell "nothing was written and the question still stands"
/// from "this may have actuated and nothing can say" cannot tell a card the operator may
/// answer again from one that must never be answered again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AnswerReport {
    /// Codex ended the request this answer was written to (it resolved it, or its item or
    /// its turn ended), and the link has already retired the card as
    /// [`protocol::ws::ResolutionActor::Phone`] and settled the claim — both in one
    /// commit — or, finding the card already retired, settled the claim.
    Delivered,
    /// **Nothing was applied by THIS answer.** Two shapes reach it and they leave
    /// different things behind, which is why the doc no longer claims one:
    ///
    ///  * refused before the claim — no ledger row exists, the card still stands, and
    ///    the operator's next tap is a first attempt rather than a duplicate;
    ///  * a lost race — the claim is terminal `lost` and the card stands for the
    ///    WINNER's own terminal to retire, so this phone will not answer it again even
    ///    though nothing it chose was applied.
    ///
    /// What is common, and what the sentence must convey, is that this tap changed
    /// nothing upstream. Whether the card can be answered again is the ledger's
    /// business, and `Daemon::answer` asks it before the link is ever reached.
    NotApplied(String),
    /// The answer may have reached Codex and nothing that survives can say, and the phone
    /// is told so.
    ///
    /// Reached when the claim finds this very answer already sent and not settled, when
    /// the connection ends as the answer is written or before Codex ends the request, when
    /// the card's retirement or the claim's settle cannot be recorded, and when the link
    /// task is dropped while it holds the ask. An answer written and not confirmed keeps
    /// its claim `applying`, and a reconnect sends it once more if Codex asks the request
    /// again.
    Unknown(String),
}

impl AnswerReport {
    /// The sentence the phone is shown, for the two endings that are refusals.
    pub(crate) fn reason(&self) -> Option<&str> {
        match self {
            AnswerReport::Delivered => None,
            AnswerReport::NotApplied(why) | AnswerReport::Unknown(why) => Some(why),
        }
    }
}

/// **What became of one interrupt the phone asked this link to write.**
///
/// Three endings, and they differ in what they left behind rather than in how they
/// read — the same distinction [`AnswerReport`] draws, for the same reason: a caller
/// that cannot tell "nothing was written and the turn is still running" from "this
/// may have actuated and nothing can say" cannot tell an ask worth repeating from
/// one that must never be repeated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InterruptReport {
    /// The turn this ask named reached its terminal, and this link watched it do so.
    /// The turn id is carried back because the phone names the turn it stopped.
    Aborted { turn_id: String },
    /// **This exact ask already stopped this exact turn**, and the record says so.
    /// A retry under the same id, carrying the same material — the thing an operator
    /// does when a reply is slow — and the honest answer to "again?" is that it
    /// already happened. Distinct from [`InterruptReport::Aborted`] only in that
    /// nothing was written this time, which is the whole of what a duplicate means.
    Duplicate { turn_id: String },
    /// **Nothing was actuated by THIS ask, and the record says so.** Reached from a
    /// refusal before the claim — the run has no link, or the turn is not the one it
    /// runs — where no ledger row exists and the operator's next tap is a first attempt;
    /// from a retry while a Stop of that turn is still waiting, whose claim stands and
    /// which sends nothing more; from the agent's side refusing the write (Codex's error
    /// answer to `turn/interrupt`, the agent host's refused outcome), which gives the
    /// claim back; and from a turn that ended by itself first, recorded `turn_ended`.
    NotApplied(String),
    /// The interrupt may have reached the app-server and nothing that survives can
    /// say. Terminal precisely so that it is never sent again: the budget expiring,
    /// the connection ending, or the task being dropped. A turn reaching a terminal this
    /// ask cannot claim to have caused is not one: it is recorded `turn_ended`, an
    /// [`InterruptReport::NotApplied`].
    Unknown(String),
}

/// **What became of one compose, in the link's own vocabulary.**
///
/// The link's word, which [`crate::state::Daemon::compose_result`] turns into the phone's.
/// Kept apart from the wire type for [`InterruptReport`]'s reason: the link answers what it
/// observed, and the mapping to what a person is told is one function rather than scattered
/// constructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposeReport {
    /// The thread was idle and these words began a turn.
    Started { turn_id: String },
    /// A turn was running and these words joined it — the same turn, not a new one.
    Steered { turn_id: String },
    /// This exact compose already ran. `started` says which of the two the ORIGINAL was,
    /// read from the snapshotted route, so a replay describes what happened rather than
    /// what would happen now.
    Duplicate { turn_id: String, started: bool },
    /// Nothing was written, and here is why.
    NotApplied(String),
    /// Written, and what became of it is not known. Never re-sent.
    Unknown(String),
}

/// **A settled compose row, replayed.**
///
/// The one mapping the link's own `Applied` arm and
/// [`crate::state::Daemon::compose`]'s terminal read both use, for
/// [`replayed_interrupt_report`]'s reason: the same question asked at two moments must not
/// get two answers. The daemon reaches it only when there is no addressee — with a link to
/// ask, the link's claim is the answer, because it reads and claims without an await
/// between them and this one always could be stale.
///
/// An outcome that names no turn is a refusal, and it replays as one — telling somebody
/// their words reached the model because a row exists would be the one thing this must not
/// do.
pub(crate) fn replayed_compose_report(outcome: &str) -> ComposeReport {
    match crate::store::parse_compose_outcome(outcome) {
        Some((route, turn_id)) => ComposeReport::Duplicate {
            turn_id: turn_id.to_string(),
            started: route == crate::store::COMPOSE_ROUTE_START,
        },
        None => ComposeReport::NotApplied(crate::store::replayed_compose_sentence(outcome)),
    }
}

/// **What a recorded interrupt outcome means, in one place.**
///
/// Both moments that can meet a terminal row read it through here: the daemon's
/// pre-claim replay of a settled row, and the link's own duplicate arm when a claim
/// comes back `Applied`. They used to be two mappings and they disagreed — an exact
/// aborted duplicate was a `Duplicate` at one and a refusal at the other — which is a
/// difference an operator feels as the daemon changing its mind about whether their
/// tap worked.
///
/// **A known ending is never `Unknown`.** `turn_ended` and `refused` are both
/// statements that nothing was actuated and that the record can say so, which is
/// exactly what a refusal is; reporting them as unknown would tell a phone the
/// outcome is unnameable when the ledger has named it, and would earn a turn that
/// demonstrably ended on its own the same treatment as a daemon killed mid-write.
pub(crate) fn replayed_interrupt_report(outcome: &str, turn_id: &str) -> InterruptReport {
    if outcome == crate::store::INTERRUPT_ABORTED {
        return InterruptReport::Duplicate {
            turn_id: turn_id.to_string(),
        };
    }
    InterruptReport::NotApplied(crate::store::replayed_interrupt_sentence(outcome))
}

/// One thing the daemon asks the live connection to do on its socket.
///
/// A struct on a channel of its own ([`answer_channel`]), apart from the link's other
/// asks ([`LinkCommand`]), because the two are alike only in being writes. An answer
/// quotes a wire id this connection was handed and learns its fate when Codex ends that
/// request ([`settle`]); a Stop names a turn and learns its fate from that turn's end.
///
/// **The durable claim is not taken before this is sent, and that is deliberate.**
/// Only the connection knows whether the card is still open, whether it holds a live
/// wire id for it — a card a restart found standing has none until Codex asks again —
/// and whether an answer to it is already waiting, so only the connection can say
/// whether the ask is addressable at all. Claiming first would leave an `applying` row
/// behind for every ask that was never addressable. The claim is therefore taken by the
/// link, at the one moment it knows the answer is about to be written, and the card
/// stays answerable until then.
pub(crate) struct AnswerRequest {
    /// The card's durable, item-derived id — never a wire id. The wire id belongs to
    /// one socket, and the whole point of asking the connection is that only it
    /// knows which one it currently holds.
    pub(crate) request_id: String,
    /// The decision exactly as the app-server offered it: a bare string, or the
    /// single-key object whose body the server itself proposed. Composed by
    /// [`crate::codex_approval`] from the stored card's option table, never from
    /// anything a phone supplied.
    pub(crate) decision: Value,
    /// The immutable material this answer is claimed with. `thread_id` is also the
    /// gate — an answer is written only on a connection bound to the card's own
    /// thread — and `route` is the option the phone named, which is what lets a
    /// recovered terminal say what was attempted.
    pub(crate) claimed: crate::store::ClaimedMaterial,
    /// The kind of device the answer came from, which the card's resolution files.
    pub(crate) answered_on: AnsweredOn,
    /// Where the outcome goes.
    pub(crate) reply: tokio::sync::oneshot::Sender<AnswerReport>,
    /// **The answer gate's read guard, owned and travelling with the ask.** It is taken
    /// in the answer path before the link is looked up and held for the answer's whole
    /// passage: carried here rather than left in the caller's future, so dropping the
    /// caller does not release it. The link moves it into the card's [`Answer::Phone`]
    /// at the write; it is released when the ask is refused or its write fails, when the
    /// card is retired, or when the link ends.
    pub(crate) gate: tokio::sync::OwnedRwLockReadGuard<()>,
}

/// The daemon's end of [`AnswerRequest`]: hand a live connection an answer to send.
///
/// Unbounded because it carries at most one entry per open approval card, and a card
/// exists only because a person is looking at it. Bounding it would add a failure
/// mode (a full queue) with no arrival that could reach it.
#[derive(Clone)]
pub(crate) struct LinkAnswers(tokio::sync::mpsc::UnboundedSender<AnswerRequest>);

/// Mint the daemon's end and the link's end of the answer channel.
///
/// Made where the link starts ([`link`]) and split there: the sender goes into the
/// daemon's host links with the run's link ([`crate::state::HostLink::Codex`]) and is
/// taken out when the link ends ([`crate::state::Daemon::host_link_detached`]); the
/// receiver is the link's own.
pub(crate) fn answer_channel() -> (
    LinkAnswers,
    tokio::sync::mpsc::UnboundedReceiver<AnswerRequest>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (LinkAnswers(tx), rx)
}

impl LinkAnswers {
    /// Ask the link to answer `request_id`, and wait for what became of it.
    ///
    /// **The two `Err` arms are the honest half.** A send that fails means the link
    /// task was already gone, so nothing was claimed and nothing was written — the
    /// card stays answerable. A receive that fails means the task went away after
    /// taking the ask, which the link's own teardown normally forestalls by settling
    /// every open answer; reaching it means the task was aborted mid-answer, and the
    /// only truthful thing to say then is that nothing here knows what happened.
    pub(crate) async fn answer(
        &self,
        request_id: &str,
        decision: Value,
        claimed: crate::store::ClaimedMaterial,
        answered_on: AnsweredOn,
        // The owned answer-gate read guard, moved in so it travels with the ask and is
        // released only at a pre-write refusal or terminal teardown.
        gate: tokio::sync::OwnedRwLockReadGuard<()>,
    ) -> AnswerReport {
        let (reply, outcome) = tokio::sync::oneshot::channel();
        if self
            .0
            .send(AnswerRequest {
                request_id: request_id.to_string(),
                decision,
                claimed,
                answered_on,
                reply,
                gate,
            })
            .is_err()
        {
            return AnswerReport::NotApplied(
                "the link to this Codex session is not running, so nothing was sent; \
                 answer at the computer"
                    .into(),
            );
        }
        outcome.await.unwrap_or_else(|_| {
            AnswerReport::Unknown(
                "the link stopped while this answer was being written, so whether it \
                 reached Codex is not known; it will not be sent again. Check the computer."
                    .into(),
            )
        })
    }
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
    /// A phone's message for the run ([`compose`]).
    Compose(ComposeAsk),
    /// End run's graceful step ([`stop_gracefully`]).
    Stop,
    /// An app's settings change, its outcome told on `reply` ([`change_settings`]).
    Settings {
        setting: crate::claude_host::Setting,
        reply: tokio::sync::oneshot::Sender<SettingResult>,
    },
}

/// A phone's message `text` under its request id and the hash [`Daemon::compose`] binds it
/// to, its outcome told on `reply`.
pub(crate) struct ComposeAsk {
    pub(crate) request_id: String,
    pub(crate) text: String,
    pub(crate) claimed_hash: String,
    pub(crate) reply: tokio::sync::oneshot::Sender<ComposeReport>,
}

/// A phone's message Codex has taken into the turn `turn`, waiting for Codex to record it
/// under its `uuid`, or for that turn's end ([`heard`]).
struct WaitingSend {
    request_id: String,
    claimed_hash: String,
    uuid: String,
    turn: String,
    /// Whether it started `turn` or joined it: [`crate::store::COMPOSE_ROUTE_START`] or
    /// [`crate::store::COMPOSE_ROUTE_STEER`].
    route: &'static str,
}

impl WaitingSend {
    /// What the phone was told when Codex took it.
    fn report(&self) -> ComposeReport {
        let turn_id = self.turn.clone();
        if self.route == crate::store::COMPOSE_ROUTE_STEER {
            ComposeReport::Steered { turn_id }
        } else {
            ComposeReport::Started { turn_id }
        }
    }
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
/// Interrupt for the turn the link is running, a phone's message for the run's `thread`, and
/// End run's graceful step. A relaunch's message, `first`, is put on the channel before the
/// link is in place, so no other ask reaches Codex ahead of it. The running turn is the one
/// Codex's last `turn/started` named, or
/// the one a resume found still running, until its `turn/completed`, or until Codex answers an
/// interrupt of it: Codex answers just before the turn's end. A phone's Stop that Codex has
/// taken waits for that end; a phone's message that Codex has taken waits for Codex to record
/// it, or for its turn's end ([`heard`]). The person's words a message filed are not filed
/// again from Codex's own record of them ([`unfiled`]).
///
/// When the connection ends, the run is ended once its app-server's leader is gone, within
/// [`EXIT_WAIT`]: Codex closes its connections a moment before it exits.
///
/// Codex runs each command in a group of its own, which outlives the app-server, so the link
/// keeps the run's tool groups ([`record_tools`]): at attach, every [`TOOL_SCAN`] while a turn
/// runs, at each turn's end, and before ccd's own SIGTERM. Once the leader is gone, each
/// recorded group that still has members and that the warrant allows is sent SIGKILL
/// ([`end_tools`]).
#[allow(clippy::too_many_arguments)]
async fn link(
    daemon: Arc<Daemon>,
    uid: String,
    thread: String,
    mut client: Client,
    mut adapter: CodexAdapter,
    mut owed: Option<Owed>,
    mut cards: HashMap<String, Card>,
    first: Option<ComposeAsk>,
) {
    let (answers, mut asked) = answer_channel();
    let (commands, mut commanded) = tokio::sync::mpsc::unbounded_channel();
    if let Some(ask) = first {
        let _ = commands.send(LinkCommand::Compose(ask));
    }
    daemon
        .host_link_attached(&uid, HostLink::Codex(answers, commands))
        .await;
    let session = SessionKey::new(&uid, &uid);
    let mut running = owed
        .as_ref()
        .map(|owed| (owed.thread.clone(), owed.turn.clone()));
    let mut stops: Vec<WaitingStop> = Vec::new();
    let mut sends: Vec<WaitingSend> = Vec::new();
    let host = daemon
        .db
        .watched_host(uid.clone())
        .await
        .ok()
        .flatten()
        .map(|watch| watch.host);
    let launch = host.as_ref().map_or(0, launch_generation);
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
                    request(&daemon, &session, &client, &adapter, &mut cards, &frame, launch).await;
                    continue;
                }
                let (thread, turn) = (
                    frame["params"]["threadId"].as_str(),
                    frame["params"]["turn"]["id"].as_str(),
                );
                match (frame["method"].as_str(), thread, turn) {
                    (Some("turn/started"), Some(thread), Some(turn)) => {
                        running = Some((thread.to_string(), turn.to_string()));
                        daemon.note_turn_running(&uid);
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
                let events = adapter.ingest(&frame);
                let filed =
                    file(&daemon, &uid, unfiled(&daemon, &uid, &[&frame["params"]["item"]], events).await).await;
                // A live turn's end is an occasion to ring the phone again about the decisions
                // still open ([`Daemon::push_turn_complete`]), as a hosted Claude run's is: only a
                // turn that completed, and only as it is filed, so an end a resume already
                // filed, here or before a restart, rings nothing.
                if let Some(end) = filed.iter().find(|event| {
                    event.kind == EventKind::TurnComplete && event.payload["status"] == "completed"
                }) {
                    daemon.push_turn_complete(&session, end).await;
                }
                heard(&daemon, &uid, &mut sends, &frame).await;
                let Some(due) =
                    owed.take_if(|owed| ended.as_deref() == Some(owed.turn.as_str()))
                else {
                    continue;
                };
                // As Codex last named the run's settings, not as the reconnect resumed it: a
                // change made while the turn ran stays made.
                let mut resume = due.resume;
                let kept = adapter.settings().cloned();
                if let Some(kept) = &kept {
                    keep_settings(&mut resume, kept);
                }
                match tokio::time::timeout(
                    crate::start_session::START_TIMEOUT,
                    client.call("thread/resume", resume),
                )
                .await
                {
                    Ok(Ok(answer)) => {
                        let thread = &due.thread;
                        let kept = kept.as_ref();
                        if let Err(why) =
                            resumed(&daemon, &uid, &client, &mut adapter, &answer, thread, kept)
                                .await
                        {
                            // Out of Plan, the run takes no message: the link ends, and the
                            // run is left `live` and unlinked, as a reconnect that failed
                            // leaves it, until the next ccd start resumes it again.
                            crate::log_info!(
                                "{uid}: the follow-up resume after turn {}: {why}; unlinked",
                                due.turn
                            );
                            break;
                        }
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
                LinkCommand::Compose(ask) => {
                    // The turns a message can join: the running one, and one whose Stop Codex
                    // has taken, which runs until its end.
                    let live: Vec<&str> = running
                        .iter()
                        .map(|(_, turn)| turn.as_str())
                        .chain(stops.iter().map(|stop| stop.turn.as_str()))
                        .collect();
                    let told = compose(&daemon, &uid, &client, &thread, &live, &mut sends, &ask).await;
                    let _ = ask.reply.send(told);
                }
                LinkCommand::Stop => {
                    record_tools(host.as_ref(), &mut tools);
                    stop_gracefully(&daemon, &uid, &client, &mut running).await;
                }
                LinkCommand::Settings { setting, reply } => {
                    let current = adapter.settings().cloned();
                    let told = change_settings(
                        &client, &thread, &mut adapter, current, running.is_some(), &setting,
                    )
                    .await;
                    let _ = reply.send(told);
                }
            },
        }
    }
    // A Stop Codex took and whose turn's end never came: its claim stays `applying`, for
    // startup's settle. So does a message Codex took that it never recorded before the
    // connection ended.
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
    /// A phone's, with the kind of device it came from, where its outcome goes and the
    /// answer gate it holds until then.
    Phone {
        decision: AnswerDecision,
        answered_on: AnsweredOn,
        reply: tokio::sync::oneshot::Sender<AnswerReport>,
        _gate: tokio::sync::OwnedRwLockReadGuard<()>,
    },
    /// An answer claimed before a restart (R6): sent again if Codex replays the request,
    /// settled when the card ends; nobody waits on it, and nothing records which device
    /// sent it.
    Claimed { decision: AnswerDecision },
}

/// **Decision 175, where this engine builds its card (owner decision 198): the only answers
/// a card offers.** Never Codex's permanent "don't ask again for commands that start with …"
/// (`acceptWithExecpolicyAmendment`), nor any other decision that writes Codex's settings.
const OFFERED_DECISIONS: [&str; 4] = ["accept", "acceptForSession", "decline", "cancel"];

/// A card's id (X7's row): the run, its thread, the request's JSON-RPC id, which a replay keeps
/// (P7-2), and the `launch` of the app-server that asked ([`launch_generation`]).
fn card_id(uid: &str, thread: &str, wire: &Value, launch: u64) -> Option<String> {
    let server_request_id = match wire {
        Value::Number(number) => ServerRequestId::Number(number.as_i64()?),
        Value::String(text) => ServerRequestId::Text(text.clone()),
        _ => return None,
    };
    CompositeId {
        session_uid: uid.to_string(),
        thread_id: thread.to_string(),
        server_request_id,
        generation: launch,
    }
    .encode()
    .ok()
}

/// The generation a card id carries: the launch of the run's app-server, by its leader's
/// start. Codex numbers its requests from 0 in each app-server, so a relaunched one asks again
/// under ids an earlier launch used on the same thread, and only the launch tells their cards
/// apart; a ccd that connects again to the same app-server reads the same leader, and finds the
/// cards standing from before its restart under the same ids.
fn launch_generation(leader: &ProcessIdentity) -> u64 {
    leader.birth.start_sec.unsigned_abs() * 1_000_000 + leader.birth.start_usec.unsigned_abs()
}

/// One of Codex's requests: a card for a command, a file change or an MCP tool call to
/// approve, or for Plan mode's questions to answer, and an error answer for anything else,
/// so no turn waits on a request nobody will answer. A request whose card a restart found
/// standing is bound to it again, and an answer claimed for it before the restart is sent
/// once more (R6). Its card's id carries `launch`, the app-server's ([`launch_generation`]).
async fn request(
    daemon: &Arc<Daemon>,
    session: &SessionKey,
    client: &Client,
    adapter: &CodexAdapter,
    cards: &mut HashMap<String, Card>,
    frame: &Value,
    launch: u64,
) {
    let uid = &session.uid;
    let (method, params, wire) = (
        frame["method"].as_str().unwrap_or_default(),
        &frame["params"],
        &frame["id"],
    );
    let id = crate::codex_approval::card_family(method, params).and_then(|family| {
        let thread = params["threadId"].as_str()?;
        Some((family, card_id(uid, thread, wire, launch)?))
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
/// and the claim's option, or a question card's claimed answers, never from anything a
/// phone sends now.
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
    let Some(decision) = stored.and_then(|card| match &state.claimed.question_decision {
        Some(answered) => crate::codex_approval::question_reply(&card["tool_input"], answered)
            .ok()
            .map(|(_, reply)| reply),
        None => crate::codex_approval::wire_decision(
            &card["tool_input"]["options"],
            &state.claimed.route,
        ),
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

/// The decision an answer claim records, which the card's resolution files: a question
/// card's own `Answers` or `Decline`, kept whole beside the claim, or the option an
/// approval's route names.
fn claimed_decision(claimed: &crate::store::ClaimedMaterial) -> AnswerDecision {
    claimed
        .question_decision
        .clone()
        .unwrap_or_else(|| AnswerDecision::OptionId {
            option_id: claimed.route.clone(),
        })
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
    let decision = claimed_decision(&ask.claimed);
    let body = crate::codex_approval::reply(card.family, ask.decision);
    if client.respond(&card.wire, body).await.is_err() {
        let _ = ask.reply.send(AnswerReport::Unknown(
            "the connection to Codex ended as your answer was sent; do not answer it again".into(),
        ));
        return;
    }
    card.answer = Some(Answer::Phone {
        decision,
        answered_on: ask.answered_on,
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
        question_decision: None,
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
            return Some(replayed_interrupt_report(&outcome, &turn));
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

/// What became of a phone's Stop that Codex took, settled from its turn's end: a turn that
/// ended `interrupted` was stopped by it; any other end was the turn's own.
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
        AnswerStatus::Settled(outcome) => replayed_interrupt_report(&outcome, turn_id),
        AnswerStatus::Indeterminate => {
            InterruptReport::Unknown(says::INTERRUPT_ALREADY_SENT_UNKNOWN.into())
        }
        AnswerStatus::Applying => {
            InterruptReport::NotApplied(says::INTERRUPT_ALREADY_WAITING.into())
        }
    })
}

/// A phone's message for the run's `thread` (X5). A retry of a message still waiting is told
/// what the first was; otherwise what the record already says of this exact ask is the
/// answer, or the ask is claimed and sent as `turn/start` under its own uuid
/// ([`crate::claude_host::send_uuid`]). Codex runs it as a new turn when the thread is idle
/// and adds it to the turn it is running otherwise, and answers with that turn: one of the
/// `live` turns, which it joined, or a new one, which it started. Once Codex takes it, the
/// person's words are filed with its request id and that turn, and it waits for what Codex
/// records of it ([`heard`]). Codex's own refusal gives the claim back, so the same ask is a first attempt
/// again; a connection that ends first leaves it `applying`, for startup's settle.
async fn compose(
    daemon: &Arc<Daemon>,
    uid: &str,
    client: &Client,
    thread: &str,
    live: &[&str],
    sends: &mut Vec<WaitingSend>,
    ask: &ComposeAsk,
) -> ComposeReport {
    use crate::codex_refusals as says;
    use crate::store::{MutationClaim, OPERATION_COMPOSE};
    if let Some(send) = sends.iter().find(|send| send.request_id == ask.request_id) {
        return if send.claimed_hash == ask.claimed_hash {
            send.report()
        } else {
            ComposeReport::NotApplied(says::COMPOSE_ID_REUSED.into())
        };
    }
    // What is written is a `turn/start` whichever way Codex then runs it, so a retry builds the
    // same material.
    let claimed = crate::store::ClaimedMaterial {
        thread_id: thread.to_string(),
        generation: 0,
        route: crate::store::COMPOSE_ROUTE_START.to_string(),
        target_turn_id: None,
        claimed_hash: ask.claimed_hash.clone(),
        question_decision: None,
    };
    let claim = daemon
        .db
        .claim_mutation(
            OPERATION_COMPOSE,
            uid.to_string(),
            ask.request_id.clone(),
            claimed,
            protocol::time::now_rfc3339(),
        )
        .await;
    match claim {
        Ok(MutationClaim::Claimed) => {}
        Ok(MutationClaim::Applied { outcome, .. }) => return replayed_compose_report(&outcome),
        Ok(MutationClaim::Indeterminate { .. }) => {
            return ComposeReport::Unknown(says::COMPOSE_ALREADY_SENT_UNKNOWN.into())
        }
        Ok(MutationClaim::Conflict) => {
            return ComposeReport::NotApplied(says::COMPOSE_ID_REUSED.into())
        }
        Ok(MutationClaim::NoSession) => {
            return ComposeReport::NotApplied(says::COMPOSE_RUN_IS_GONE.into())
        }
        Err(err) => {
            crate::log_error!(
                "{uid}: could not claim the message {}: {err:#}",
                crate::state::logged_request_id(&ask.request_id)
            );
            return ComposeReport::NotApplied(says::COMPOSE_CLAIM_UNRECORDED.into());
        }
    }
    let uuid = crate::claude_host::send_uuid(uid, &ask.request_id);
    let params = json!({
        "threadId": thread,
        "input": [{"type": "text", "text": ask.text}],
        "clientUserMessageId": uuid,
    });
    let asked = tokio::time::timeout(
        crate::start_session::START_TIMEOUT,
        client.call("turn/start", params),
    )
    .await;
    let answer = match asked {
        Ok(Ok(answer)) => answer,
        Ok(Err(CallError::Rpc { code, message })) => {
            crate::log_info!("{uid}: Codex refused a message: {message} ({code})");
            if let Err(err) = daemon
                .db
                .release_mutation(OPERATION_COMPOSE, uid.to_string(), ask.request_id.clone())
                .await
            {
                crate::log_error!("{uid}: could not give back the refused message: {err:#}");
            }
            return ComposeReport::NotApplied(says::wire_refused(
                &code.to_string(),
                says::COMPOSE_WIRE_SUBJECT,
                says::COMPOSE_WIRE_REMEDY,
            ));
        }
        Ok(Err(CallError::Closed)) | Err(_) => {
            return ComposeReport::Unknown(says::compose_settled_unknown(
                says::COMPOSE_CONNECTION_ENDED,
            ))
        }
    };
    let Some(turn) = answer["turn"]["id"]
        .as_str()
        .filter(|turn| !turn.is_empty())
    else {
        crate::log_error!("{uid}: Codex took a message and named no turn; it is recorded unknown");
        if let Err(err) = daemon
            .db
            .settle_mutation_indeterminate(
                OPERATION_COMPOSE,
                uid.to_string(),
                ask.request_id.clone(),
                protocol::time::now_rfc3339(),
            )
            .await
        {
            crate::log_error!("{uid}: could not record the message as unknown: {err:#}");
        }
        return ComposeReport::Unknown(says::COMPOSE_ANSWER_UNREADABLE.into());
    };
    let route = if live.contains(&turn) {
        crate::store::COMPOSE_ROUTE_STEER
    } else {
        crate::store::COMPOSE_ROUTE_START
    };
    let words = crate::claude_host::person_row(uid, &uuid, &ask.text, Some(&ask.request_id), None)
        .with_turn_id(Some(turn.to_string()));
    file(daemon, uid, vec![words]).await;
    let send = WaitingSend {
        request_id: ask.request_id.clone(),
        claimed_hash: ask.claimed_hash.clone(),
        uuid,
        turn: turn.to_string(),
        route,
    };
    let report = send.report();
    sends.push(send);
    report
}

/// What a frame says of the phone's messages Codex took (X5), by the evidence Codex's own
/// client reads. A message Codex records under its uuid was delivered. One still waiting when
/// its turn ends was dropped by a Stop if that turn ended `interrupted`: Codex drops the input
/// a stopped turn had not yet read, and says nothing of it, so it is settled with its
/// `send_not_sent` notice. After any other end, Codex records what input was left (all but
/// what a hook blocked), so a message it did not record is settled unknown.
async fn heard(daemon: &Daemon, uid: &str, sends: &mut Vec<WaitingSend>, frame: &Value) {
    use crate::store::OPERATION_COMPOSE;
    let params = &frame["params"];
    let settled = match frame["method"].as_str() {
        Some("item/started" | "item/completed") if params["item"]["type"] == "userMessage" => {
            let echoed = params["item"]["clientId"].as_str();
            let Some(at) = sends
                .iter()
                .position(|send| echoed == Some(send.uuid.as_str()))
            else {
                return;
            };
            let send = sends.remove(at);
            let delivered = daemon
                .db
                .settle_mutation(
                    OPERATION_COMPOSE,
                    uid.to_string(),
                    send.request_id.clone(),
                    crate::store::compose_outcome(send.route, &send.turn),
                    protocol::time::now_rfc3339(),
                )
                .await
                .map(|_| ());
            vec![(send, delivered)]
        }
        Some("turn/completed") => {
            let turn = params["turn"]["id"].as_str();
            let interrupted = params["turn"]["status"] == "interrupted";
            let (ended, waiting): (Vec<_>, Vec<_>) = std::mem::take(sends)
                .into_iter()
                .partition(|send| Some(send.turn.as_str()) == turn);
            *sends = waiting;
            let mut settled = Vec::new();
            for send in ended {
                let outcome = if interrupted {
                    let notice =
                        crate::claude_host::not_sent_notice(uid, &send.uuid, &send.request_id);
                    daemon
                        .settle_with_event(
                            uid,
                            OPERATION_COMPOSE,
                            &send.request_id,
                            crate::store::COMPOSE_STOPPED,
                            notice,
                        )
                        .await
                } else {
                    daemon
                        .db
                        .settle_mutation_indeterminate(
                            OPERATION_COMPOSE,
                            uid.to_string(),
                            send.request_id.clone(),
                            protocol::time::now_rfc3339(),
                        )
                        .await
                        .map(|_| ())
                };
                settled.push((send, outcome));
            }
            settled
        }
        _ => return,
    };
    for (send, outcome) in settled {
        if let Err(err) = outcome {
            crate::log_error!(
                "{uid}: could not record what became of the message {}: {err:#}",
                crate::state::logged_request_id(&send.request_id)
            );
        }
    }
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
    let (decision, answered_on, reply) = match answer {
        Some(Answer::Phone {
            decision,
            answered_on,
            reply,
            ..
        }) => (decision, Some(answered_on), Some(reply)),
        Some(Answer::Claimed { decision }) => (decision, None, None),
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
                answered_on,
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

/// File `events` in order ([`Daemon::ingest`]: once each, then out to every subscriber),
/// telling the mail wake rule of each one newly filed ([`crate::mail::note_filed_event`]).
/// The events newly filed; one the log already held is not among them.
async fn file(daemon: &Arc<Daemon>, uid: &str, events: Vec<PendingEvent>) -> Vec<Event> {
    let mut filed = Vec::new();
    for pending in events {
        match daemon.ingest(pending).await {
            Ok(Some(event)) => {
                crate::mail::note_filed_event(daemon, uid, &event).await;
                filed.push(event);
            }
            Ok(None) => {}
            Err(err) => crate::log_error!("{uid}: could not file a Codex event: {err:#}"),
        }
    }
    filed
}

/// `events` but the person's messages among `items` that a phone's message already filed
/// (its `say:` row, named by the item's `clientId`): Codex's own record of those words is not
/// filed a second time. A message whose row was never filed keeps Codex's.
async fn unfiled(
    daemon: &Daemon,
    uid: &str,
    items: &[&Value],
    events: Vec<PendingEvent>,
) -> Vec<PendingEvent> {
    let mut said = Vec::new();
    for item in items {
        let (Some(id), Some(client_id)) = (item["id"].as_str(), item["clientId"].as_str()) else {
            continue;
        };
        if item["type"] != "userMessage" {
            continue;
        }
        let row = daemon
            .db
            .filed_event(uid.to_string(), Source::Daemon, format!("say:{client_id}"))
            .await;
        if matches!(row, Ok(Some(_))) {
            said.push(id);
        }
    }
    events
        .into_iter()
        .filter(|event| {
            event.kind != EventKind::UserMessage
                || !event
                    .item_id
                    .as_deref()
                    .is_some_and(|id| said.contains(&id))
        })
        .collect()
}

/// A `thread/resume` answer's facts filed, then the adapter told what was filed, and the run
/// put back in Plan when the settings it was resumed from (`kept`) were in Plan
/// ([`keep_plan`]); the turns the answer reports still running. Every resume passes here, so
/// none leaves a run wider than its kept mode (decision 253). The answer's own settings are
/// filed only when Plan was not set again: when it was, Codex's report of Plan is the run's
/// next settings fact, so a refusal, a crash or a restart before that report leaves Plan the
/// kept mode, and the next resume sets it again. `Err` when Plan was not put back.
async fn resumed(
    daemon: &Arc<Daemon>,
    uid: &str,
    client: &Client,
    adapter: &mut CodexAdapter,
    answer: &Value,
    thread: &str,
    kept: Option<&Value>,
) -> Result<Vec<String>, String> {
    let (running, settings) = match adapter.plan_resume_seed(answer, thread) {
        Some(mut seed) => {
            let items: Vec<&Value> = answer["thread"]["turns"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|turn| turn["items"].as_array())
                .flatten()
                .collect();
            file(
                daemon,
                uid,
                unfiled(daemon, uid, &items, seed.take_events()).await,
            )
            .await;
            adapter.apply_resume_seed(&seed);
            let settings = adapter.settings_answered(thread, answer);
            (seed.running_turn_ids().to_vec(), settings)
        }
        None => {
            crate::log_info!("{uid}: Codex's resume answer for thread {thread} was not read");
            (Vec::new(), Vec::new())
        }
    };
    let now = adapter.settings().or(kept).cloned();
    if !keep_plan(
        client,
        thread,
        adapter,
        kept,
        answer,
        now,
        !running.is_empty(),
    )
    .await?
    {
        file(daemon, uid, settings).await;
    }
    Ok(running)
}

/// The thread `thread` put back in Plan when the settings it was resumed from (`kept`, a
/// `codex_settings` fact) were in Plan and the resume's `answer` does not name Plan:
/// `thread/resume` takes no collaboration mode, so a resume leaves Plan. One
/// `thread/settings/update`, as an app's change to Plan is ([`change_settings`]), carrying the
/// model and the effort of the settings Codex names `now`, from the next turn while one runs
/// (`running`), the `adapter` taking the thread to be in Plan once Codex takes it. Whether it
/// was sent and taken; `Err`, with Codex's reason, when Codex refuses it or does not answer.
async fn keep_plan(
    client: &Client,
    thread: &str,
    adapter: &mut CodexAdapter,
    kept: Option<&Value>,
    answer: &Value,
    now: Option<Value>,
    running: bool,
) -> Result<bool, String> {
    if kept.is_none_or(|kept| kept["collaboration_mode"] != "plan")
        || answer["collaborationMode"]["mode"] == "plan"
    {
        return Ok(false);
    }
    let plan = crate::claude_host::Setting::Mode {
        mode: "plan".into(),
        confirmed: false,
    };
    match change_settings(client, thread, adapter, now, running, &plan).await {
        SettingResult::Applied { .. } => Ok(true),
        SettingResult::Refused { reason } | SettingResult::Indeterminate { reason } => {
            Err(format!("Plan was not set again: {reason}"))
        }
    }
}

/// `kept`, a run's newest `codex_settings` fact, when it is narrower than the preset Default:
/// Read Only or Plan as the apps read it ([`protocol::facts::codex_mode`]). A run whose Codex is
/// started again comes back in that mode, and in Default otherwise, Full Access included
/// (decisions 233 and 253).
fn narrower_than_default(kept: Value) -> Option<Value> {
    let text = |key: &str| kept[key].as_str().unwrap_or_default();
    matches!(
        protocol::facts::codex_mode(
            text("approval_policy"),
            text("sandbox"),
            text("collaboration_mode")
        ),
        Some("read-only" | "plan")
    )
    .then_some(kept)
}

/// A restarted ccd's link to the `live` run `run`, whose recorded app-server is alive (P3
/// R2): its socket, `initialize`, a fresh skills scan, then `thread/resume` with the approval
/// policy and sandbox Codex last named for it and the run's thread config, the answer filed,
/// and the run's [`link`] from there. Codex restores the thread's model itself; the resume
/// carries the approval and sandbox because Codex takes a resume's as the thread's own (X6
/// P12c), and the same app-server keeps what the run was set to (ruling b). A turn the answer
/// reports running is owed its follow-up. Anything that fails, within START_TIMEOUT, leaves
/// the run `live` and unlinked, and is logged.
pub(crate) async fn reconnect(
    daemon: Arc<Daemon>,
    root: PathBuf,
    mut run: LiveCodexRun,
    codeconnect: PathBuf,
) {
    let uid = run.host.session_uid.clone();
    let resumed = tokio::time::timeout(
        crate::start_session::START_TIMEOUT,
        resume(&daemon, &root, &mut run, &codeconnect),
    )
    .await;
    match resumed {
        Ok(Ok((client, adapter, owed, cards))) => {
            link(daemon, uid, run.thread, client, adapter, owed, cards, None).await
        }
        Ok(Err(why)) => crate::log_info!("{uid}: not reconnected: {why}"),
        Err(_) => crate::log_info!(
            "{uid}: not reconnected within {} s",
            crate::start_session::START_TIMEOUT.as_secs()
        ),
    }
}

async fn resume(
    daemon: &Arc<Daemon>,
    root: &Path,
    run: &mut LiveCodexRun,
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
    let kept = daemon
        .db
        .newest_codex_settings(uid.clone())
        .await
        .map_err(|err| format!("could not read its settings: {err:#}"))?;
    let (adapter, owed, cards) = resume_thread(daemon, &client, run, codeconnect, kept).await?;
    Ok((client, adapter, owed, cards))
}

/// The run's thread resumed on `client`, connected and initialized: a fresh skills scan, then
/// `thread/resume` with the run's thread config and the approval policy and sandbox `kept`
/// names (its newest `codex_settings` fact, [`keep_settings`]), the preset Default's without
/// one, its answer filed and the run put back in Plan when `kept` was in Plan ([`resumed`]).
/// The adapter told what was filed, the turn the answer reports running owed its follow-up, and
/// the run's standing cards. A run that has had no message has a thread with no rollout, which
/// Codex cannot resume: it is given a new thread instead ([`new_thread`]), `run` naming it.
async fn resume_thread(
    daemon: &Arc<Daemon>,
    client: &Client,
    run: &mut LiveCodexRun,
    codeconnect: &Path,
    kept: Option<Value>,
) -> Result<(CodexAdapter, Option<Owed>, HashMap<String, Card>), String> {
    let uid = &run.host.session_uid.clone();
    let folders = folders(&run.cwd, daemon.config.git_bin.as_deref()).await?;
    let skills = client
        .call(
            "skills/list",
            json!({"cwds": [&run.cwd], "forceReload": true}),
        )
        .await
        .map_err(|err| format!("Codex did not list the folder's skills: {err}"))?;
    let mut params = thread_params(&run.cwd, None);
    if let Some(kept) = &kept {
        keep_settings(&mut params, kept);
    }
    params["threadId"] = json!(run.thread);
    params["config"] = thread_config(
        uid,
        &codeconnect.to_string_lossy(),
        &folders,
        run.trust,
        &skills,
    );
    let answer = match client.call("thread/resume", params.clone()).await {
        Ok(answer) => answer,
        Err(CallError::Rpc { message, .. }) if message.starts_with("no rollout found") => {
            return new_thread(daemon, client, run, params, kept).await;
        }
        Err(err) => return Err(format!("Codex did not resume the thread: {err}")),
    };
    let mut adapter = CodexAdapter::new(SessionKey::new(uid, uid));
    let running = resumed(
        daemon,
        uid,
        client,
        &mut adapter,
        &answer,
        &run.thread,
        kept.as_ref(),
    )
    .await?;
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
    Ok((adapter, owed, cards))
}

/// A new thread for the run `run`, whose thread has no rollout (it never ran a turn) and so
/// cannot be resumed: `thread/start` with the resume's `params` but its thread, recorded as
/// the run's only while the run has had no message, so nothing is lost; its settings filed,
/// and the run put back in Plan when `kept` was in Plan, as a resume puts it back. Any other
/// run's thread with no rollout stays an error, as every other failed resume is.
async fn new_thread(
    daemon: &Arc<Daemon>,
    client: &Client,
    run: &mut LiveCodexRun,
    mut params: Value,
    kept: Option<Value>,
) -> Result<(CodexAdapter, Option<Owed>, HashMap<String, Card>), String> {
    let uid = run.host.session_uid.clone();
    if let Some(params) = params.as_object_mut() {
        params.remove("threadId");
    }
    let started = client
        .call("thread/start", params)
        .await
        .map_err(|err| format!("Codex did not start a thread: {err}"))?;
    let thread = started["thread"]["id"]
        .as_str()
        .ok_or("Codex's thread/start named no thread")?
        .to_string();
    match daemon
        .db
        .replace_unsaid_thread(uid.clone(), thread.clone())
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "Codex has no rollout of thread {}, and the run has had a message",
                run.thread
            ))
        }
        Err(err) => return Err(format!("could not record the run's new thread: {err:#}")),
    }
    crate::log_info!(
        "{uid}: thread {} had no message, so no rollout; thread {thread} started in its place",
        run.thread
    );
    run.thread = thread;
    let mut adapter = CodexAdapter::new(SessionKey::new(&uid, &uid));
    let settings = adapter.settings_answered(&run.thread, &started);
    let now = adapter.settings().or(kept.as_ref()).cloned();
    if !keep_plan(
        client,
        &run.thread,
        &mut adapter,
        kept.as_ref(),
        &started,
        now,
        false,
    )
    .await?
    {
        file(daemon, &uid, settings).await;
    }
    Ok((adapter, None, HashMap::new()))
}

/// **The person's message `ask` to the engine run `uid`, relaunching it when its app-server
/// ended without ccd stopping it** (P3 R4): a run `exited` "Stopped: its process ended"
/// ([`crate::claude_host::PROCESS_ENDED`]), on a message only, never at ccd start; not one End
/// run ended, nor a failed start. In one write the run moves to `starting` with its recorded
/// app-server cleared ([`crate::store::Store::begin_relaunch`]), so a second message meets
/// `starting` and is refused, and the watch never reads the old app-server as this launch's.
/// Under the warrant that the old app-server's group, its leader with it, has no member, or
/// that it was recorded in a boot before this one ([`launch_again`]), its `as.pid` and
/// `as.sock` are removed; the codex a start would find, with the environment a
/// start reads, is launched into the run's folder with the run's kept folder answer, appending
/// to its `host.log` ([`bring_up`]); and the thread is resumed on that connection as a
/// reconnect resumes it ([`resume_thread`]), from the run's newest settings when they are
/// narrower than Default ([`narrower_than_default`]: Read Only or Plan, put back before the
/// message is sent) and at Default's approval and sandbox otherwise, Full Access included: a
/// run never gains permissions by restarting, and a new app-server is not given Full Access
/// (decisions 233 and 253). The run is then `live`, before its link is in
/// place, its start is filed under this launch so the apps' timeline reopens, and its [`link`]
/// takes `ask` as its first command.
///
/// Whether the run was one to relaunch; when it was not, nothing was done. Any failure before
/// the link runs the ladder on what was launched, puts the run back to `exited`
/// ([`crate::store::Store::undo_relaunch`]), also when the watch found the new app-server gone
/// first and failed the run for it, and drops `ask` with nothing claimed or sent.
pub(crate) async fn relaunch(daemon: &Arc<Daemon>, uid: &str, ask: ComposeAsk) -> bool {
    let Some((root, launcher)) = daemon.relaunch_from.get() else {
        return false;
    };
    let mut run = match daemon.db.begin_relaunch(uid.to_string()).await {
        Ok(Some(run)) => run,
        Ok(None) => return false,
        Err(err) => {
            crate::log_error!("{uid}: could not begin its relaunch: {err:#}");
            return false;
        }
    };
    let mut launched = None;
    let relaunched = tokio::time::timeout(
        crate::start_session::START_TIMEOUT,
        launch_again(daemon, root, launcher, &mut run, &ask, &mut launched),
    )
    .await
    .unwrap_or_else(|_| {
        Err(format!(
            "not relaunched within {} s",
            crate::start_session::START_TIMEOUT.as_secs()
        ))
    });
    let live = match relaunched {
        Ok(resumed) => match daemon
            .db
            .advance_lifecycle(uid.to_string(), Lifecycle::Starting, Lifecycle::Live)
            .await
        {
            Ok(true) => Ok(resumed),
            Ok(false) => Err("the run left `starting` while it was relaunched".to_string()),
            Err(err) => Err(format!("could not mark the run live: {err:#}")),
        },
        Err(why) => Err(why),
    };
    match live {
        Ok((client, adapter, owed, cards)) => {
            let (leader, _) = launched.expect("an app-server that answered was recorded");
            crate::log_info!(
                "{uid}: relaunched; its app-server's leader is {}",
                leader.pid
            );
            let start = PendingEvent::new(
                &SessionKey::new(uid, uid),
                EventKind::SessionStart,
                json!({"source": "resume"}),
                Source::Daemon,
            )
            .with_source_event_id(format!("start:{uid}:{}", leader.pid));
            file(daemon, uid, vec![start]).await;
            tokio::spawn(link(
                Arc::clone(daemon),
                uid.to_string(),
                run.thread,
                client,
                adapter,
                owed,
                cards,
                Some(ask),
            ));
        }
        Err(why) => {
            crate::log_info!("{uid}: not relaunched: {why}");
            if let Some((leader, boot)) = launched {
                stop(uid, leader, boot).await;
            }
            let launched = launched.map(|(leader, _)| leader);
            if let Err(err) = daemon
                .db
                .undo_relaunch(uid.to_string(), Some(run.host), launched)
                .await
            {
                crate::log_error!("{uid}: could not put the run back to exited: {err:#}");
            }
        }
    }
    true
}

/// [`relaunch`]'s steps from its warrant to its resume: the old app-server's group without a
/// member, `as.pid` and `as.sock` removed, the codex and environment a start reads and
/// `launcher` as the `codeconnect`, the launch into the run's folder in `root` recorded in
/// `launched`, and the thread resumed on it.
///
/// The group's emptiness is the leader's absence too: ccd launched the leader in a session of
/// its own, and a session leader cannot leave its group. A group the kernel cannot be asked
/// about counts as one with a member. When the run's recorded boot is not this boot, the old
/// leader and its group cannot have survived the restart and their numbers mean nothing now, so
/// the group is not consulted; a boot that could not be read, then or now, is no proof of a
/// restart, and the group is asked.
async fn launch_again(
    daemon: &Arc<Daemon>,
    root: &Path,
    launcher: &Result<PathBuf, String>,
    run: &mut LiveCodexRun,
    ask: &ComposeAsk,
    launched: &mut Option<(ProcessIdentity, Option<BootIdentity>)>,
) -> Result<(Client, CodexAdapter, Option<Owed>, HashMap<String, Card>), String> {
    let uid = &run.host.session_uid;
    let old = run.host.host.pid;
    let restarted = matches!(
        (run.host.boot, protocol::proc_identity::boot_identity()),
        (Some(then), Some(now)) if then != now
    );
    if !restarted && group_has_members(old) {
        return Err(format!("its app-server's group {old} still has a member"));
    }
    let dir = crate::claude_host::run_dir(root, uid);
    for name in ["as.pid", "as.sock"] {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("could not remove its {name}: {err}")),
        }
    }
    let environment = crate::start_session::phone_environment();
    let codex = crate::start_session::codex_probe(&environment, daemon.config.codex_bin.as_deref())
        .await
        .map_err(|(_, why)| why)?;
    let codeconnect = launcher.clone()?;
    let git_bin = daemon.config.git_bin.as_deref();
    let folders = folders(&run.cwd, git_bin).await?;
    let log = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("host.log"))
        .map_err(|err| format!("could not open its host.log: {err}"))?;
    let launch = Run {
        root,
        request_id: &ask.request_id,
        cwd: &run.cwd,
        prompt: &ask.text,
        model: None,
        // The run keeps the models its start kept.
        models: &[],
        codex: &codex.path,
        codeconnect: &codeconnect,
        git_bin,
        environment: &environment,
        trust: run.trust,
    };
    let deadline = Instant::now() + crate::start_session::START_TIMEOUT;
    let kept = daemon
        .db
        .newest_codex_settings(uid.clone())
        .await
        .map_err(|err| format!("could not read its settings: {err:#}"))?
        .and_then(narrower_than_default);
    let client = bring_up(daemon, uid, &launch, &folders, log, deadline, launched).await?;
    let (adapter, owed, cards) = resume_thread(daemon, &client, run, &codeconnect, kept).await?;
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
                decision: claimed_decision(&state.claimed),
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
/// message when the start has one: each call's error, or a run ended meanwhile, stops it.
/// The run's thread, and `thread/start`'s answer, which names the settings it runs with.
async fn send_first(
    daemon: &Daemon,
    client: &Client,
    send: &FirstSend,
) -> Result<(String, Value), String> {
    let skills = client
        .call(
            "skills/list",
            json!({"cwds": [&send.cwd], "forceReload": true}),
        )
        .await
        .map_err(|err| format!("Codex did not list the folder's skills: {err}"))?;
    let mut params = thread_params(&send.cwd, send.model.as_deref());
    params["config"] = thread_config(
        &send.uid,
        &send.codeconnect,
        &send.folders,
        send.trust,
        &skills,
    );
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
    if !send.prompt.is_empty() {
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
    }
    Ok((thread.to_string(), started))
}

/// The permission modes an app may set an engine Codex run to (minor 43), the asking default
/// first: our order, not Codex's own. Each is one of Codex's presets as Codex names it; Full
/// Access, which asks nothing, is marked `confirm` (decision 214), and Plan is decision 211's.
const MODES: [(&str, &str, bool); 4] = [
    ("auto", "Default", false),
    ("read-only", "Read Only", false),
    ("plan", "Plan", false),
    ("full-access", "Full Access", true),
];

/// An engine Codex run's menus, from the models its start's check listed, each as Codex's
/// `model/list` gives it, and the one it started on ([`keep_started_model`]): each model's
/// `model`, name (else its `model`), description and efforts, and [`MODES`].
pub(crate) fn menus(models: &[Value]) -> RunMenus {
    let text = |model: &Value, key: &str| {
        model
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    RunMenus {
        models: models
            .iter()
            .filter_map(|model| {
                let value = text(model, "model")?;
                Some(MenuModel {
                    label: text(model, "displayName").unwrap_or_else(|| value.clone()),
                    detail: text(model, "description"),
                    efforts: model
                        .get("supportedReasoningEfforts")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|effort| effort.get("reasoningEffort")?.as_str())
                        .map(str::to_string)
                        .collect(),
                    value,
                })
            })
            .collect(),
        modes: MODES
            .iter()
            .map(|(value, label, confirm)| MenuMode {
                value: value.to_string(),
                label: label.to_string(),
                confirm: *confirm,
            })
            .collect(),
        selected_model: None,
    }
}

/// The `thread/settings/update` fields for `setting`, given the thread's settings as Codex
/// last named them (`current`, its `codex_settings` fact): a model and an effort as they are;
/// a mode as its preset's approval policy and sandbox, Plan keeping the run's own (decision
/// 174), with its collaboration mode, which carries the thread's model and effort since Codex
/// makes the mode's own the thread's (X6 P2). A model or an effort changed in Plan carries
/// Plan again, with them. `None` when a mode finds no settings to take them from.
fn update_params(setting: &crate::claude_host::Setting, current: Option<&Value>) -> Option<Value> {
    use crate::claude_host::Setting;
    let collaboration = |mode: &str, model: &Value, effort: &Value| {
        json!({"mode": mode, "settings": {
            "model": model, "reasoning_effort": effort, "developer_instructions": null}})
    };
    match setting {
        Setting::Model { model, effort } => {
            let mut params = json!({});
            if let Some(model) = model {
                params["model"] = model.as_str().into();
            }
            if let Some(effort) = effort {
                params["effort"] = effort.as_str().into();
            }
            if current.is_some_and(|current| current["collaboration_mode"] == "plan") {
                let current = current?;
                let model = model
                    .as_deref()
                    .map_or(current["model"].clone(), Value::from);
                let effort = effort
                    .as_deref()
                    .map_or(current["effort"].clone(), Value::from);
                params["collaborationMode"] = collaboration("plan", &model, &effort);
            }
            Some(params)
        }
        Setting::Mode { mode, .. } => {
            let current = current?;
            let into = if mode == "plan" { "plan" } else { "default" };
            let mut params = json!({
                "collaborationMode": collaboration(into, &current["model"], &current["effort"]),
            });
            let preset = match mode.as_str() {
                "auto" => Some(("on-request", "workspaceWrite")),
                "read-only" => Some(("on-request", "readOnly")),
                "full-access" => Some(("never", "dangerFullAccess")),
                _ => None,
            };
            if let Some((approval, sandbox)) = preset {
                params["approvalPolicy"] = approval.into();
                params["sandboxPolicy"] = json!({ "type": sandbox });
            }
            Some(params)
        }
    }
}

/// How long the link waits for Codex to answer a settings change.
const SETTING_WAIT: Duration = Duration::from_secs(10);

/// An app's settings change, sent to Codex by the run's link as one `thread/settings/update`
/// on its `thread` ([`update_params`]): `applied`, from the next turn while one runs (Codex
/// takes a change from its next request, X6 P4) and at once otherwise; `refused` with Codex's
/// own words when it answers with an error; `indeterminate` when it does not answer within
/// [`SETTING_WAIT`]. What the run's settings are after it is Codex's own report
/// (`thread/settings/updated`), filed as its facts; until that report, a change Codex took is
/// the thread's settings in the `adapter` ([`CodexAdapter::settings_taken`]), so a resume
/// before it asks for what the person set.
async fn change_settings(
    client: &Client,
    thread: &str,
    adapter: &mut CodexAdapter,
    current: Option<Value>,
    running: bool,
    setting: &crate::claude_host::Setting,
) -> SettingResult {
    let Some(mut params) = update_params(setting, current.as_ref()) else {
        return SettingResult::Refused {
            reason: "Codex hasn't said what this session's settings are yet, so nothing was \
                     changed."
                .into(),
        };
    };
    params["threadId"] = thread.into();
    let call = client.call("thread/settings/update", params.clone());
    match tokio::time::timeout(SETTING_WAIT, call).await {
        Ok(Ok(_)) => {
            adapter.settings_taken(current, &params);
            SettingResult::Applied {
                applies: if running {
                    Applies::NextTurn
                } else {
                    Applies::Now
                },
            }
        }
        Ok(Err(CallError::Rpc { message, .. })) => SettingResult::Refused {
            reason: format!("Codex didn't take the change ({message}); nothing was changed."),
        },
        Ok(Err(CallError::Closed)) | Err(_) => SettingResult::Indeterminate {
            reason: "Codex didn't answer, so the change may or may not have been made.".into(),
        },
    }
}

/// An app's settings change for the engine Codex run `uid`, whose link takes asks on
/// `commands` when it is attached. Refused before anything is sent when the run is not live,
/// its link is not attached, the model or the effort is not on its menu, the mode is not one
/// of [`MODES`], or Full Access comes without the person's confirmation in the app; otherwise
/// the link sends it ([`change_settings`]).
pub(crate) async fn set_setting(
    daemon: &Daemon,
    uid: &str,
    setting: crate::claude_host::Setting,
    commands: Option<tokio::sync::mpsc::UnboundedSender<LinkCommand>>,
) -> SettingResult {
    use crate::claude_host::Setting;
    let refused = |reason: &str| SettingResult::Refused {
        reason: reason.to_string(),
    };
    match &setting {
        Setting::Model {
            model: None,
            effort: None,
        } => return refused("Choose a model or an effort; nothing was changed."),
        Setting::Model { .. } => {}
        Setting::Mode { mode, confirmed } => {
            match MODES.iter().find(|(value, _, _)| value == mode) {
                None => {
                    return refused(
                        "That permission mode isn't offered in this session; nothing was changed.",
                    )
                }
                Some((_, label, true)) if !confirmed => {
                    return refused(&format!(
                        "Turning on {label} needs your confirmation in the app; nothing was \
                         changed."
                    ))
                }
                Some(_) => {}
            }
        }
    }
    match daemon.db.hosted_link(uid.to_string()).await {
        Ok(Some(linked)) if linked.lifecycle == Lifecycle::Live => {}
        Ok(Some(_)) => return refused("This session isn't running, so nothing was changed."),
        Ok(None) => return refused(crate::claude_host::NOT_FROM_THE_APP),
        Err(err) => {
            crate::log_error!("{uid}: could not read its host: {err:#}");
            return refused("This computer could not read the session.");
        }
    }
    if let Setting::Model { model, effort } = &setting {
        let menus = match daemon.db.hosted_models(uid.to_string()).await {
            Ok(models) => menus(&models.unwrap_or_default()),
            Err(err) => {
                crate::log_error!("{uid}: could not read its models: {err:#}");
                return refused("This computer could not read the session.");
            }
        };
        if model
            .as_ref()
            .is_some_and(|model| !menus.models.iter().any(|offered| &offered.value == model))
        {
            return refused("Codex doesn't offer that model in this session; nothing was changed.");
        }
        if effort.as_ref().is_some_and(|effort| {
            !menus
                .models
                .iter()
                .any(|offered| offered.efforts.contains(effort))
        }) {
            return refused(
                "Codex doesn't offer that effort in this session; nothing was changed.",
            );
        }
    }
    let lost =
        || refused("The computer lost its connection to this session, so nothing was changed.");
    let Some(commands) = commands else {
        return lost();
    };
    let (reply, answer) = tokio::sync::oneshot::channel();
    if commands
        .send(LinkCommand::Settings { setting, reply })
        .is_err()
    {
        return lost();
    }
    answer
        .await
        .unwrap_or_else(|_| SettingResult::Indeterminate {
            reason: "Codex didn't answer, so the change may or may not have been made.".into(),
        })
}

/// A `thread/resume`'s approval policy and sandbox (`params`) set to those of the settings
/// Codex last named (`kept`, a `codex_settings` fact), as `thread/resume` takes them; left as
/// they are when `kept` names no approval policy or a sandbox other than Codex's three.
fn keep_settings(params: &mut Value, kept: &Value) {
    let sandbox = match kept["sandbox"].as_str() {
        Some("workspaceWrite") => "workspace-write",
        Some("readOnly") => "read-only",
        Some("dangerFullAccess") => "danger-full-access",
        _ => return,
    };
    if let Some(approval) = kept["approval_policy"].as_str() {
        params["approvalPolicy"] = approval.into();
        params["sandbox"] = sandbox.into();
    }
}

/// `thread/start`'s params but its `config`: the run's folder, its model (none for Codex's own
/// `default`) and the preset Default, `on-request` with a `workspace-write` sandbox (T4,
/// decision 176), so an untrusted folder never falls to Codex's own UnlessTrusted, and a
/// trusted project's own approval policy and sandbox mode never replace them.
fn thread_params(cwd: &str, model: Option<&str>) -> Value {
    json!({
        "cwd": cwd,
        "model": model,
        "approvalPolicy": "on-request",
        "sandbox": "workspace-write",
    })
}

/// The run `uid`'s thread config (P3 L4): its `folders` at the level of the person's answer
/// `trust`, and CodeConnect's mail server, run by `codeconnect` with the run's uid, which
/// Codex hands an MCP server only through its `env`. Not trusted, the repository's own skills
/// that `skills/list` reported are disabled too, since Codex loads them for any folder.
fn thread_config(
    uid: &str,
    codeconnect: &str,
    folders: &[PathBuf],
    trust: Trust,
    skills: &Value,
) -> Value {
    let projects: serde_json::Map<String, Value> = folders
        .iter()
        .map(|folder| {
            (
                folder.to_string_lossy().into_owned(),
                json!({"trust_level": trust_level(trust)}),
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
    if trust == Trust::Trusted {
        return config;
    }
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

/// The trust level Codex's `projects` entries take for the person's answer.
fn trust_level(trust: Trust) -> &'static str {
    match trust {
        Trust::Trusted => "trusted",
        Trust::NotTrusted => "untrusted",
    }
}

/// The run's app-server launched into the run's folder, its output going to `log`, recorded
/// in `launched`, connected and initialized, by `deadline`. A start makes the folder and its
/// `host.log` first; a relaunch launches into the folder the run already has, `log` its
/// `host.log` opened to append ([`relaunch`]).
///
/// **What a launch reads in the run's folder**, so a relaunch reads nothing of an earlier
/// launch's as its own: `as.pid`, which only the claim reads, its noclobber refusing the launch
/// while one exists (ccd's leader is the pid it spawned, never the file); `as.sock`, which codex
/// listens on and ccd dials until it answers; and `host.log`, whose last line is the launch's
/// failure when it ends before listening, read only past the length it had at the launch. A
/// relaunch removes `as.pid` and `as.sock` first, by exact path, under the warrant that the
/// launch before it is gone; nothing else in the folder is read.
async fn bring_up(
    daemon: &Arc<Daemon>,
    uid: &str,
    run: &Run<'_>,
    folders: &[PathBuf],
    log: std::fs::File,
    deadline: Instant,
    launched: &mut Option<(ProcessIdentity, Option<BootIdentity>)>,
) -> Result<Client, String> {
    let dir = crate::claude_host::run_dir(run.root, uid);
    let since = log
        .metadata()
        .map_err(|err| format!("could not read the run's host.log: {err}"))?
        .len();
    let mut listen = OsString::from("unix://");
    listen.push(dir.join("as.sock"));
    let projects = format!("projects={}", launch_projects(folders, run.trust));
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
    let attached = attach(daemon, uid, &dir, since, &mut child, deadline, launched).await;
    crate::start_session::reap(child);
    attached
}

/// [`bring_up`]'s launch recorded, then its `as.sock` dialled and initialized; `since` is where
/// the launch's own output begins in `host.log`.
async fn attach(
    daemon: &Arc<Daemon>,
    uid: &str,
    dir: &Path,
    since: u64,
    child: &mut tokio::process::Child,
    deadline: Instant,
    launched: &mut Option<(ProcessIdentity, Option<BootIdentity>)>,
) -> Result<Client, String> {
    let socket = dir.join("as.sock");
    let pid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("the login shell ended as it started")?;
    // A codex that ended at once is an unreaped child here, and macOS gives no start time for
    // one: what it said is the start's failure.
    let Some(birth) = read_birth_identity(pid) else {
        crate::start_session::give_up_if_ended(
            dir,
            since,
            child,
            deadline,
            "Codex's app-server did not listen",
        )?;
        return Err("could not read the app-server's start time".into());
    };
    let leader = ProcessIdentity { pid, birth };
    let boot = protocol::proc_identity::boot_identity();
    *launched = Some((leader, boot));
    daemon
        .db
        .record_host(uid.to_string(), leader, boot)
        .await
        .map_err(|err| format!("could not record the app-server: {err:#}"))?;

    let client = loop {
        match tokio::time::timeout_at(deadline, crate::codex_client::connect(&socket)).await {
            Ok(Ok(client)) => break client,
            Ok(Err(err)) if not_listening(&err) => {
                crate::start_session::give_up_if_ended(
                    dir,
                    since,
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

/// `folders`, each at the level of the person's answer `trust`, as the TOML inline table the
/// launch's `-c projects=` takes: the same level as the thread config's, since a reload
/// rebuilds the project's config from this list. Each path is a TOML basic string, written as
/// JSON writes a string.
fn launch_projects(folders: &[PathBuf], trust: Trust) -> String {
    let level = trust_level(trust);
    let entries: Vec<String> = folders
        .iter()
        .map(|folder| {
            let key =
                serde_json::to_string(&folder.to_string_lossy()).expect("a string is always JSON");
            format!(r#"{key} = {{ trust_level = "{level}" }}"#)
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
        rig_sending(Arc::new(crate::apns::LoggingPushSender::new()))
    }

    /// [`rig`], its daemon's pushes going to `push`.
    fn rig_sending(push: Arc<dyn crate::apns::PushSender>) -> Rig {
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
            daemon: daemon_sending(&root, "/usr/bin/false", push),
            project: root.join("project").display().to_string(),
            root,
            environment,
        }
    }

    /// A daemon on the store in `root`, as a restarted ccd opens it again. Its mail wakes find
    /// `/usr/bin/false` for codex (config.json's `codex_bin`), so a test whose wake rule fires
    /// never runs a real codex.
    fn daemon_on(root: &Path) -> Arc<Daemon> {
        daemon_with(root, "/usr/bin/false")
    }

    /// [`daemon_on`], with `codex` as config.json's `codex_bin`.
    fn daemon_with(root: &Path, codex: &str) -> Arc<Daemon> {
        daemon_sending(root, codex, Arc::new(crate::apns::LoggingPushSender::new()))
    }

    /// [`daemon_with`], its pushes going to `push`.
    fn daemon_sending(
        root: &Path,
        codex: &str,
        push: Arc<dyn crate::apns::PushSender>,
    ) -> Arc<Daemon> {
        let store = Store::open(&root.join("events.db")).unwrap();
        Daemon::new(
            Config {
                codex_bin: Some(codex.into()),
                ..Config::default()
            },
            Arc::new(store),
            push,
            Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
        )
    }

    /// A stand-in codex running `script`, launched as codex is: in the run's folder, after
    /// the claim, with codex's arguments. It keeps its arguments in the run's `argv` and
    /// counts its launches in `<root>/launches`. Asked its version, as a start's check asks
    /// codex, it answers 0.167.0 and does nothing else.
    fn stand_in(rig: &Rig, script: &str) -> PathBuf {
        let path = rig.root.join("stand-in-codex");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'codex-cli 0.167.0'; exit 0; }}\n\
                 printf '%s\\0' \"$@\" > argv\necho launch >> '{}'\n{script}\n",
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
            models: &[],
            codex,
            codeconnect: Path::new(CODECONNECT),
            git_bin: None,
            environment: &rig.environment,
            trust: Trust::NotTrusted,
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

    /// The folder of the run being started, once the start has made it: the one no stand-in
    /// listens in yet, so a second run in a rig is served apart from the first.
    async fn starting_run_folder(rig: &Rig) -> PathBuf {
        loop {
            let found = std::fs::read_dir(rig.root.join("runs"))
                .ok()
                .and_then(|entries| {
                    entries
                        .flatten()
                        .map(|entry| entry.path())
                        .find(|dir| !dir.join("as.sock").exists())
                });
            if let Some(dir) = found {
                return dir;
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
        let dir = starting_run_folder(rig).await;
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

    /// Whether `pgid`'s group has a member within 15 s: a child that makes its own group
    /// may not have made it yet when its pid is read.
    async fn group_forms(pgid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !group_alive(pgid) && Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
        }
        group_alive(pgid)
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

    /// **A Codex run started without a first message is `live` on its own thread with no
    /// turn, and a ccd restart gives it a new thread**: its start sends no `turn/start`, so
    /// its thread has no rollout and Codex answers the reconnect's `thread/resume` "no rollout
    /// found"; the run has had no message, so ccd starts a new thread with the resume's
    /// settings and records it as the run's, and the run is linked on it.
    #[tokio::test]
    async fn a_codex_run_started_without_a_message_waits_and_gets_a_new_thread_on_a_restart() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let mut script = whole_start();
        script.retain(|(method, _)| *method != "turn/start");
        let quiet = Run {
            prompt: "",
            ..run(&rig, "req-quiet", &codex)
        };
        let (result, (seen, first)) =
            tokio::join!(start(&rig.daemon, quiet), play_app_server(&rig, script));
        let uid = started_uid(&result);
        let row = row_once(&rig, &uid, Lifecycle::Live).await;
        assert_eq!(row.lifecycle, Lifecycle::Live);
        assert_eq!(row.codex_thread_id.as_deref(), Some("thr-x3"));
        assert_eq!(
            methods(&seen),
            ["initialize", "initialized", "skills/list", "thread/start"]
        );

        drop(first);
        let dir = run_folder(&rig).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::remove_file(dir.join("as.sock")).unwrap();
        let listener = UnixListener::bind(dir.join("as.sock")).unwrap();
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        let ((), (resume, started, _ws)) = tokio::join!(
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
                let resume = asked(&mut ws, "thread/resume").await;
                tell(
                    &mut ws,
                    &json!({"id": resume["id"], "error": {"code": -32600,
                            "message": "no rollout found for thread id thr-x3"}}),
                )
                .await;
                let started = asked(&mut ws, "thread/start").await;
                answer(
                    &mut ws,
                    &started,
                    json!({"thread": {"id": "thr-new"}, "model": "gpt-test"}),
                )
                .await;
                (resume, started, ws)
            }
        );
        let mut expected = resume["params"].clone();
        expected.as_object_mut().unwrap().remove("threadId");
        assert_eq!(
            started["params"], expected,
            "the resume's settings, no thread"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let summary = loop {
            let summary = summary_of(&restarted, &uid).await;
            if (summary.codex_thread_id.as_deref() == Some("thr-new")
                && summary.codex_link == protocol::event::CodexLink::Subscribed)
                || Instant::now() >= deadline
            {
                break summary;
            }
            tokio::time::sleep(POLL).await;
        };
        end_run(&rig, &uid).await;
        assert_eq!(summary.codex_thread_id.as_deref(), Some("thr-new"));
        assert_eq!(summary.codex_link, protocol::event::CodexLink::Subscribed);
        assert_eq!(summary.lifecycle, Lifecycle::Live);
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

    /// **XT G1: a run started trusted launches and threads its folders trusted**, the
    /// repository's skills left on as Codex loads them, CodeConnect's mail server as for any
    /// run, and the answer kept in its `start` record for its reconnect.
    #[tokio::test]
    async fn a_run_started_trusted_launches_and_threads_its_folders_trusted() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let run = Run {
            trust: Trust::Trusted,
            ..run(&rig, "req-trusted", &codex)
        };
        let (result, (seen, _ws)) = tokio::join!(
            start(&rig.daemon, run),
            play_app_server(&rig, whole_start())
        );
        let uid = started_uid(&result);
        row_once(&rig, &uid, Lifecycle::Live).await;
        let dir = crate::claude_host::run_dir(&rig.root, &uid);
        let kept = rig
            .daemon
            .db
            .hosted_link(uid.clone())
            .await
            .unwrap()
            .unwrap();
        end_run(&rig, &uid).await;

        let real = std::fs::canonicalize(&rig.project)
            .unwrap()
            .display()
            .to_string();
        let argv = String::from_utf8(std::fs::read(dir.join("argv")).unwrap()).unwrap();
        let launched = format!(r#"projects={{ "{real}" = {{ trust_level = "trusted" }} }}"#);
        assert!(argv.contains(&launched), "{argv:?}");
        let config = seen
            .iter()
            .find(|frame| frame["method"] == "thread/start")
            .unwrap()["params"]["config"]
            .clone();
        assert_eq!(
            config["projects"],
            json!({real: {"trust_level": "trusted"}})
        );
        assert!(config.get("skills.config").is_none(), "{config}");
        assert_eq!(config["mcp_servers.codeconnect"]["command"], CODECONNECT);
        assert_eq!(kept.start.as_deref(), Some(r#""trusted""#));
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
        let run = rig
            .daemon
            .db
            .hosted_run_for_request("req-silent".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.lifecycle, Lifecycle::Failed);
        // The leader ccd recorded and stopped: a start ended at its deadline may not have
        // run the claim that writes `as.pid`.
        let leader = rig
            .daemon
            .db
            .watched_host(run.session_uid)
            .await
            .unwrap()
            .expect("a start that did not listen recorded its leader")
            .host
            .pid;
        assert!(
            group_ends(leader).await,
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

    /// **A codex that has ended before its start time is read fails the start with what it
    /// said last**: an ended child is not yet reaped then, and macOS gives no start time for
    /// one.
    #[tokio::test]
    async fn a_codex_ended_before_its_start_time_is_read_fails_with_its_last_line() {
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-gone", "thr-gone", "turn-1").await;
        app_server_ended(&rig, &uid, ws).await;
        let dir = crate::claude_host::run_dir(&rig.root, &uid);
        let since = std::fs::metadata(dir.join("host.log")).unwrap().len();
        let mut child = tokio::process::Command::new("/bin/sh")
            .args([
                "-c",
                "read _; echo 'error: unexpected argument' >> host.log; exit 2",
            ])
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id().unwrap() as i32;
        let launched_as = ProcessIdentity {
            pid,
            birth: read_birth_identity(pid).unwrap(),
        };
        drop(child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while protocol::proc_identity::liveness(&launched_as)
            != protocol::proc_identity::Liveness::Gone
        {
            assert!(Instant::now() < deadline, "the stand-in did not end");
            tokio::time::sleep(POLL).await;
        }
        let mut launched = None;
        let failed = attach(
            &rig.daemon,
            &uid,
            &dir,
            since,
            &mut child,
            Instant::now() + Duration::from_secs(20),
            &mut launched,
        )
        .await;
        assert_eq!(failed.err().as_deref(), Some("error: unexpected argument"));
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
            cwd: rig.project.clone(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: Lifecycle::Starting,
            created_at: now.clone(),
            updated_at: now,
            agent: AgentKind::Codex,
            codex_thread_id: None,
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
        link_attached(&rig.daemon, &uid).await;
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
    /// turn ends, ccd resumes once more and files the command, once: after the turn's reply
    /// and its end, which arrived live first. v0.18.0's read-back placed it before them; this
    /// is a written miss (`CodexAdapter::plan_resume_seed`).
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
        // The reconnect's resume carries the preset Default, the run having named no settings
        // before the restart; the follow-up, the settings the reconnect's answer then named
        // (this capture's read-only), as Codex last named them.
        for (resume, sandbox) in [
            (&first_resume, "workspace-write"),
            (&follow_up, "read-only"),
        ] {
            let params = &resume["params"];
            assert_eq!(params["threadId"], thread.as_str());
            assert_eq!(params["approvalPolicy"], "on-request");
            assert_eq!(params["sandbox"], sandbox);
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
        engine_run_as(
            rig,
            request_id,
            thread,
            turn,
            stand_in_script,
            Trust::NotTrusted,
        )
        .await
    }

    /// [`engine_run_on`], started with the folder answer `trust`.
    async fn engine_run_as(
        rig: &Rig,
        request_id: &str,
        thread: &str,
        turn: &str,
        stand_in_script: &str,
        trust: Trust,
    ) -> (String, Socket) {
        let codex = stand_in(rig, stand_in_script);
        let mut script = whole_start();
        script.truncate(3);
        script[2].1 = json!({"result": {"thread": {"id": thread}, "model": "gpt-test"}});
        let run = Run {
            trust,
            ..run(rig, request_id, &codex)
        };
        let (result, ws) = tokio::join!(start(&rig.daemon, run), async {
            let (_, mut ws) = play_app_server(rig, script).await;
            let first_turn = asked(&mut ws, "turn/start").await;
            answer(&mut ws, &first_turn, json!({"turn": {"id": turn}})).await;
            ws
        });
        let uid = started_uid(&result);
        row_once(rig, &uid, Lifecycle::Live).await;
        // The run is `live` before its link attaches; until then a message or a Stop is refused.
        link_attached(&rig.daemon, &uid).await;
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
        decided(
            daemon,
            uid,
            card,
            AnswerDecision::OptionId {
                option_id: option.into(),
            },
        )
        .await
    }

    /// A phone's answer to `card` with `decision`.
    async fn decided(
        daemon: &Daemon,
        uid: &str,
        card: &protocol::ws::ApprovalCard,
        decision: AnswerDecision,
    ) -> protocol::ws::AnswerResult {
        daemon
            .answer(
                &card.request_id,
                &card.payload_hash,
                decision,
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
        deciding(
            daemon,
            uid,
            card,
            AnswerDecision::OptionId {
                option_id: option.into(),
            },
        )
    }

    /// [`answering`] with `decision`.
    fn deciding(
        daemon: &Arc<Daemon>,
        uid: &str,
        card: &protocol::ws::ApprovalCard,
        decision: AnswerDecision,
    ) -> tokio::task::JoinHandle<protocol::ws::AnswerResult> {
        let (daemon, uid, card) = (Arc::clone(daemon), uid.to_string(), card.clone());
        tokio::spawn(async move { decided(&daemon, &uid, &card, decision).await })
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
    /// waits on it: a question that does not hold its turn (X7b's written limit: [`PLAN_QUESTION`]
    /// with `isBlocking: false`), an MCP form, a request ccd has no use for, and an unknown one.
    /// A stand-in that waited for these answers before going on would wait for ever without
    /// them.
    #[tokio::test]
    async fn a_request_with_no_card_is_answered_at_once_with_an_error() {
        let rig = rig();
        let (thread, turn) = ("thread-b", "turn-b");
        let (uid, mut ws) = engine_run(&rig, "req-none", thread, turn).await;
        let params = json!({"threadId": thread, "turnId": turn, "itemId": "item-b"});
        let (mut not_blocking, _, _) = plan_question();
        not_blocking["id"] = json!(1);
        not_blocking["params"]["isBlocking"] = json!(false);
        let requests = [
            not_blocking,
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
        let resume: serde_json::Value = serde_json::from_str(RUNNING_TURN).unwrap();
        let (restarted, ws, _) = resumed_with(rig, first, resume["result"].clone()).await;
        (restarted, ws)
    }

    /// [`restarted_under`], its reconnect answered with `resume_answer`, with the params of the
    /// `thread/resume` the reconnect sent.
    async fn resumed_with(
        rig: &Rig,
        first: Socket,
        resume_answer: serde_json::Value,
    ) -> (Arc<Daemon>, Socket, serde_json::Value) {
        resumed_on(rig, first, resume_answer, daemon_on(&rig.root)).await
    }

    /// [`resumed_with`], the restarted daemon being `restarted`.
    async fn resumed_on(
        rig: &Rig,
        first: Socket,
        resume_answer: serde_json::Value,
        restarted: Arc<Daemon>,
    ) -> (Arc<Daemon>, Socket, serde_json::Value) {
        drop(first);
        let dir = run_folder(rig).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::remove_file(dir.join("as.sock")).unwrap();
        let listener = UnixListener::bind(dir.join("as.sock")).unwrap();
        restarted.recover().await;
        let ((), (ws, params)) = tokio::join!(
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
                answer(&mut ws, &resumed, resume_answer).await;
                (ws, resumed["params"].clone())
            }
        );
        (restarted, ws, params)
    }

    /// **XT G3: a reconnect resumes the thread with the run's own folder answer**, not the
    /// folder's answer now, and so does the follow-up resume once the turn it found running
    /// ends: a run started trusted resumes trusted after the folder became `not_trusted`, and
    /// one started not trusted stays so after it became `trusted`.
    #[tokio::test]
    async fn a_reconnect_resumes_with_the_runs_own_folder_answer() {
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let mid_turn = &capture["answerMidTurn"]["result"];
        let thread = mid_turn["thread"]["id"].as_str().unwrap();
        let first_turn = capture["answerAfter"]["result"]["thread"]["turns"][0]["id"]
            .as_str()
            .unwrap();
        for (started, changed) in [
            (Trust::Trusted, Trust::NotTrusted),
            (Trust::NotTrusted, Trust::Trusted),
        ] {
            let rig = rig();
            let (uid, ws) = engine_run_as(
                &rig,
                "req-kept",
                thread,
                first_turn,
                "exec sleep 30",
                started,
            )
            .await;
            let real = std::fs::canonicalize(&rig.project)
                .unwrap()
                .display()
                .to_string();
            rig.daemon
                .db
                .set_folder_trust(real.clone(), changed)
                .await
                .unwrap();
            let (_restarted, mut ws, resumed) = resumed_with(&rig, ws, mid_turn.clone()).await;
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
            let want = json!({real: {"trust_level": trust_level(started)}});
            for (which, params) in [("reconnect", &resumed), ("follow-up", &follow_up["params"])] {
                assert_eq!(
                    params["config"]["projects"], want,
                    "the {which}: started {started:?}, the folder's answer then {changed:?}"
                );
            }
            drop(ws);
            end_run(&rig, &uid).await;
        }
    }

    /// **A mode set while a reconnected turn runs stays set at its follow-up resume**: the
    /// follow-up asks for the run's settings as Codex last named them, not as the reconnect
    /// resumed it, since Codex takes a resume's approval and sandbox as the thread's own (X6
    /// P12c). A run at Full Access, its ccd restarted mid-turn and resumed with Full Access, is
    /// set to Default while that turn runs; its follow-up asks for Default.
    #[tokio::test]
    async fn a_mode_set_while_a_reconnected_turn_runs_stays_set_at_its_follow_up() {
        use crate::claude_host::Setting;
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let mid_turn = &capture["answerMidTurn"]["result"];
        let thread = mid_turn["thread"]["id"].as_str().unwrap();
        let first_turn = capture["answerAfter"]["result"]["thread"]["turns"][0]["id"]
            .as_str()
            .unwrap();
        let on_thread = |mut report: serde_json::Value| {
            report["params"]["threadId"] = json!(thread);
            report
        };
        let rig = rig();
        let (uid, mut ws) = engine_run(&rig, "req-follow-kept", thread, first_turn).await;
        let full = settings_report(
            "gpt-6.1-sol",
            "high",
            "never",
            "dangerFullAccess",
            "default",
        );
        tell(&mut ws, &on_thread(full)).await;
        settings_as_sent(&rig.daemon, &uid, "full-access").await;
        let (restarted, mut ws, resumed) = resumed_with(&rig, ws, mid_turn.clone()).await;
        assert_eq!(
            (&resumed["approvalPolicy"], &resumed["sandbox"]),
            (&json!("never"), &json!("danger-full-access"))
        );
        link_attached(&restarted, &uid).await;
        let default = Setting::Mode {
            mode: "auto".into(),
            confirmed: false,
        };
        let (told, ()) = tokio::join!(restarted.set_setting(&uid, default), async {
            let update = asked(&mut ws, "thread/settings/update").await;
            answer(&mut ws, &update, json!({})).await;
        });
        assert!(matches!(told, SettingResult::Applied { .. }), "{told:?}");
        let auto = settings_report(
            "gpt-6.1-sol",
            "high",
            "on-request",
            "workspaceWrite",
            "default",
        );
        tell(&mut ws, &on_thread(auto)).await;
        settings_as_sent(&restarted, &uid, "auto").await;
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
        assert_eq!(
            (
                &follow_up["params"]["approvalPolicy"],
                &follow_up["params"]["sandbox"]
            ),
            (&json!("on-request"), &json!("workspace-write")),
            "the follow-up undid the mode set while the turn ran"
        );
        drop(ws);
        end_run(&rig, &uid).await;
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
        let host = restarted
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .host;
        restarted
            .end_hosted_run(
                &uid,
                host,
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

    // ------------------------------- Plan mode's questions (X7b)

    /// The question codex 0.167.0 sent in Plan mode (cx/x7b/p/probe.out P1), verbatim.
    const PLAN_QUESTION: &str = include_str!("../../../fixtures/codex/plan-question-0.167.0.json");

    /// [`PLAN_QUESTION`]'s frame, its thread and its turn.
    fn plan_question() -> (serde_json::Value, String, String) {
        let asked: serde_json::Value = serde_json::from_str(PLAN_QUESTION).unwrap();
        let thread = asked["params"]["threadId"].as_str().unwrap().to_string();
        let turn = asked["params"]["turnId"].as_str().unwrap().to_string();
        (asked, thread, turn)
    }

    /// [`PLAN_QUESTION`] with a second question after its own, as Codex asks one to three.
    fn two_questions() -> (serde_json::Value, String, String) {
        let (mut asked, thread, turn) = plan_question();
        let mut size = asked["params"]["questions"][0].clone();
        size["id"] = json!("size");
        size["header"] = json!("Size");
        size["question"] = json!("Which size?");
        size["options"] = json!([
            {"label": "Small", "description": "a small one"},
            {"label": "Large", "description": "a large one"}
        ]);
        asked["params"]["questions"]
            .as_array_mut()
            .unwrap()
            .push(size);
        (asked, thread, turn)
    }

    /// The answer that picks `index` of a question's options.
    fn pick(index: u32) -> protocol::ws::QuestionAnswer {
        protocol::ws::QuestionAnswer {
            selected: vec![index],
            ..Default::default()
        }
    }

    /// The resolution filed for the card `id`, as an app decodes it, once it is filed (within
    /// 10 s).
    async fn resolution_of(daemon: &Daemon, uid: &str, id: &str) -> CodexResolution {
        let source = format!("resolved:{id}");
        filed_until(daemon, uid, &source).await;
        let event = daemon
            .db
            .events_after(uid.to_string(), 0, 10_000)
            .await
            .unwrap()
            .into_iter()
            .find(|event| {
                event
                    .source_event_id
                    .as_deref()
                    .is_some_and(|filed| filed.ends_with(&source))
            })
            .expect("the card's resolution was filed");
        serde_json::from_value(event.payload).unwrap()
    }

    /// **G1 (X7b): a question Codex asks in Plan mode becomes a question card**
    /// ([`PLAN_QUESTION`] on the run's link). Its `tool_name` is Codex's own
    /// `request_user_input`; its `tool_input` is the questions in the shape the apps draw a
    /// question card in, each with Codex's `id` inside it, so the id an answer is filed under is
    /// stored and hashed with the options; `display_text` is `"{tool_name}\n{tool_input}"` and
    /// `payload_hash` its SHA-256, the apps' two checks (CardVerification.swift); and the stored
    /// family reads back as a question, as a restart reads it.
    ///
    /// **Must-fail:** take the question arm out of `codex_approval::card_family`, and Codex's
    /// request is answered -32601 and no card is filed.
    #[tokio::test]
    async fn a_plan_question_becomes_a_question_card_with_codexs_ids_inside_it() {
        let rig = rig();
        let (asked, thread, turn) = plan_question();
        let (uid, mut ws) = engine_run(&rig, "req-question", &thread, &turn).await;
        tell(&mut ws, &asked).await;
        let cards = cards_of(&rig.daemon, &uid, 1).await;
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        assert_eq!(card.tool_name, "request_user_input");
        assert_eq!(
            card.tool_input,
            json!({"questions": [{
                "id": "colour", "question": "Which colour?", "header": "Colour",
                "multiSelect": false,
                "options": [{"label": "Red", "description": "the red one"},
                            {"label": "Blue", "description": "the blue one"}]
            }]})
        );
        assert_eq!(
            card.display_text,
            format!("request_user_input\n{}", card.tool_input)
        );
        assert_eq!(
            card.payload_hash,
            protocol::hash::sha256_hex(card.display_text.as_bytes())
        );
        let rows = rig
            .daemon
            .db
            .codex_pending_approvals(uid.clone())
            .await
            .unwrap();
        assert_eq!(
            Family::of_item_type(&rows[0].family),
            Some(Family::Question)
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **G2 (X7b): a question card's answers reach Codex as Codex's own TUI sends them, and the
    /// resolution filed is the answers sent.** Two questions, each answered under its own id in
    /// the card's order: an option as its label, "Other" as `"None of the above"` and the
    /// person's words, trimmed, as a note. An answer with the wrong count, one naming an option
    /// its question does not have, and an option id as an approval is answered with, are each
    /// refused with no claim and nothing written: the first reply Codex is sent is the right
    /// one. Codex's resolved notice retires the card, files the answers and settles the claim
    /// delivered.
    ///
    /// **Must-fail:** take the question branch out of `Daemon::answer_codex`, and every answer
    /// is refused as "a Codex card is answered by naming one of the options".
    #[tokio::test]
    async fn a_question_cards_answers_reach_codex_as_its_own_tui_sends_them_and_are_filed() {
        let rig = rig();
        let (asked, thread, turn) = two_questions();
        let (uid, mut ws) = engine_run(&rig, "req-answers", &thread, &turn).await;
        tell(&mut ws, &asked).await;
        let cards = cards_of(&rig.daemon, &uid, 1).await;
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        for (refused, why) in [
            (
                AnswerDecision::Answers {
                    answers: vec![pick(1)],
                },
                "1 answer(s) for 2 question(s)",
            ),
            (
                AnswerDecision::Answers {
                    answers: vec![pick(2), pick(0)],
                },
                "question 1 has no option 3",
            ),
            (
                AnswerDecision::OptionId {
                    option_id: "accept".into(),
                },
                "answered with an answer to each question, or declined",
            ),
        ] {
            match decided(&rig.daemon, &uid, card, refused).await {
                protocol::ws::AnswerResult::Rejected { reason } => {
                    assert!(reason.contains(why), "{reason}")
                }
                other => panic!("an answer that does not fit the card was taken: {other:?}"),
            }
            assert_eq!(
                ledger(&rig.daemon, &uid, &card.request_id).await,
                None,
                "a refused answer was claimed"
            );
        }
        let answers = AnswerDecision::Answers {
            answers: vec![
                pick(1),
                protocol::ws::QuestionAnswer {
                    other: Some("  a big one\n".into()),
                    ..Default::default()
                },
            ],
        };
        let id = &asked["id"];
        let (result, response) =
            tokio::join!(decided(&rig.daemon, &uid, card, answers.clone()), async {
                let response = response_to(&mut ws, id, Duration::from_secs(20)).await;
                tell(&mut ws, &resolved(&asked["params"]["threadId"], id)).await;
                response
            });
        assert_eq!(
            response,
            Some(json!({"id": id, "result": {"answers": {
                "colour": {"answers": ["Blue"]},
                "size": {"answers": ["None of the above", "user_note: a big one"]}
            }}}))
        );
        assert!(
            matches!(result, protocol::ws::AnswerResult::Applied { .. }),
            "{result:?}"
        );
        assert_eq!(
            resolution_of(&rig.daemon, &uid, &card.request_id).await,
            CodexResolution::Answered {
                by: ResolutionActor::Phone,
                decision: Some(answers),
                answered_on: Some(AnsweredOn::Iphone)
            }
        );
        assert!(cards_of(&rig.daemon, &uid, 0).await.is_empty());
        assert_eq!(
            ledger(&rig.daemon, &uid, &card.request_id).await,
            Some(AnswerStatus::Settled("delivered".into()))
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **G2 (X7b): a declined question card answers no question, `{"answers": {}}`, and files
    /// the decline**: Codex is told nothing was chosen, and the turn goes on.
    #[tokio::test]
    async fn a_declined_question_card_answers_no_question_and_files_the_decline() {
        let rig = rig();
        let (asked, thread, turn) = plan_question();
        let (uid, mut ws) = engine_run(&rig, "req-decline", &thread, &turn).await;
        tell(&mut ws, &asked).await;
        let cards = cards_of(&rig.daemon, &uid, 1).await;
        assert_eq!(cards.len(), 1);
        let id = &asked["id"];
        let (result, response) = tokio::join!(
            decided(&rig.daemon, &uid, &cards[0], AnswerDecision::Decline),
            async {
                let response = response_to(&mut ws, id, Duration::from_secs(20)).await;
                tell(&mut ws, &resolved(&asked["params"]["threadId"], id)).await;
                response
            }
        );
        assert_eq!(response, Some(json!({"id": id, "result": {"answers": {}}})));
        assert!(
            matches!(result, protocol::ws::AnswerResult::Applied { .. }),
            "{result:?}"
        );
        assert_eq!(
            resolution_of(&rig.daemon, &uid, &cards[0].request_id).await,
            CodexResolution::Answered {
                by: ResolutionActor::Phone,
                decision: Some(AnswerDecision::Decline),
                answered_on: Some(AnsweredOn::Iphone)
            }
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **G3 (X7b): a question's answer in flight across a restart is sent once more, with the
    /// answers it was claimed with, and filed as those answers.** [`PLAN_QUESTION`], moved onto
    /// the running turn of [`RUNNING_TURN`]'s thread, is answered, and ccd's connection ends
    /// before Codex resolves it. The restarted daemon takes the card back from the store
    /// (`standing_cards`: its family read back, its claim `applying`), Codex asks again under the
    /// same id (P3), and the claimed reply is sent once and only once. One card throughout; the
    /// resolved notice files the answers sent and settles the claim delivered.
    ///
    /// **Must-fail:** claim the answer without its decision (`question_decision: None` in
    /// `Daemon::answer_codex`), and the restarted link cannot rebuild the reply ("could not be
    /// rebuilt; not sent again") and sends nothing.
    #[tokio::test]
    async fn a_question_answer_in_flight_at_a_restart_is_sent_once_more_and_filed_as_sent() {
        let rig = rig();
        let resume: serde_json::Value = serde_json::from_str(RUNNING_TURN).unwrap();
        let thread = resume["result"]["thread"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let turn = resume["result"]["thread"]["turns"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (mut asked, _, _) = plan_question();
        asked["params"]["threadId"] = json!(thread);
        asked["params"]["turnId"] = json!(turn);
        let id = asked["id"].clone();
        let (uid, mut ws) = engine_run(&rig, "req-question-r6", &thread, &turn).await;
        tell(&mut ws, &asked).await;
        let cards = cards_of(&rig.daemon, &uid, 1).await;
        assert_eq!(cards.len(), 1);
        let answers = AnswerDecision::Answers {
            answers: vec![pick(1)],
        };
        let reply =
            Some(json!({"id": id, "result": {"answers": {"colour": {"answers": ["Blue"]}}}}));
        let _phone = deciding(&rig.daemon, &uid, &cards[0], answers.clone());
        assert_eq!(
            response_to(&mut ws, &id, Duration::from_secs(20)).await,
            reply
        );
        assert_eq!(
            ledger(&rig.daemon, &uid, &cards[0].request_id).await,
            Some(AnswerStatus::Applying)
        );
        let (restarted, mut ws) = restarted_under(&rig, ws).await;
        tell(&mut ws, &asked).await;
        assert_eq!(
            response_to(&mut ws, &id, Duration::from_secs(20)).await,
            reply,
            "the claimed answers were not sent again"
        );
        assert_eq!(
            response_to(&mut ws, &id, Duration::from_secs(3)).await,
            None,
            "the claimed answers were sent again more than once"
        );
        assert_eq!(cards_of(&restarted, &uid, 1).await.len(), 1);
        tell(&mut ws, &resolved(&json!(thread), &id)).await;
        assert_eq!(
            resolution_of(&restarted, &uid, &cards[0].request_id).await,
            CodexResolution::Answered {
                by: ResolutionActor::Phone,
                decision: Some(answers),
                answered_on: None
            }
        );
        assert!(cards_of(&restarted, &uid, 0).await.is_empty());
        assert_eq!(
            ledger(&restarted, &uid, &cards[0].request_id).await,
            Some(AnswerStatus::Settled("delivered".into()))
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **A Codex question's resolution says which kind of device answered it**: `mac` for an
    /// answer from a connection that said hello as the Mac app
    /// ([`AnsweredOn::MAC_CLIENT_NAME`]), `iphone` from any other client, as Claude's
    /// answers say.
    ///
    /// **Must-fail:** file `answered_on: None` for a phone's answer in [`retire`], and the Mac's
    /// answer reads `None`.
    #[tokio::test]
    async fn a_codex_questions_resolution_names_the_kind_of_device_that_answered() {
        for (client_name, device) in [
            (AnsweredOn::MAC_CLIENT_NAME, AnsweredOn::Mac),
            ("CodeConnect iPhone", AnsweredOn::Iphone),
        ] {
            let rig = rig();
            let (asked, thread, turn) = plan_question();
            let (uid, mut ws) = engine_run(&rig, "req-answered-on", &thread, &turn).await;
            tell(&mut ws, &asked).await;
            let card = cards_of(&rig.daemon, &uid, 1).await.remove(0);
            let answers = AnswerDecision::Answers {
                answers: vec![pick(1)],
            };
            let (result, _) = tokio::join!(
                rig.daemon.answer(
                    &card.request_id,
                    &card.payload_hash,
                    answers.clone(),
                    Some(&uid),
                    AnsweredOn::from_client_name(Some(client_name)),
                ),
                async {
                    response_to(&mut ws, &asked["id"], Duration::from_secs(20)).await;
                    tell(
                        &mut ws,
                        &resolved(&asked["params"]["threadId"], &asked["id"]),
                    )
                    .await;
                }
            );
            assert!(
                matches!(result, protocol::ws::AnswerResult::Applied { .. }),
                "{client_name}: {result:?}"
            );
            assert_eq!(
                resolution_of(&rig.daemon, &uid, &card.request_id).await,
                CodexResolution::Answered {
                    by: ResolutionActor::Phone,
                    decision: Some(answers),
                    answered_on: Some(device)
                },
                "{client_name}"
            );
            drop(ws);
            end_run(&rig, &uid).await;
        }
    }

    // ------------------------------- a Codex question card, as the apps are sent it

    /// The run the question-card fixture is emitted for, written in place of the run's own uid.
    const QUESTION_CARD_UID: &str = "01K1B3XQ8ZC0DE5FGH7QCARD00";
    /// The one clock read in the file, replaced with a fixed stamp.
    const QUESTION_CARD_AT: &str = "2026-11-16T00:00:00.000Z";
    /// The launch the file's card id names ([`launch_generation`]): an app-server whose leader
    /// started at [`QUESTION_CARD_AT`], 1794787200 s.
    const QUESTION_CARD_LAUNCH: u64 = 1_794_787_200_000_000;
    const QUESTION_CARD: &str = include_str!("../../../fixtures/codex/question-card-wire.json");
    const REGENERATE_QUESTION_CARD: &str =
        "cargo test -p ccd --bin ccd -- --ignored regenerate_the_codex_question_card_fixture";

    /// The fixture's runs, each with the decision an app answers the card with: the second
    /// option, and a decline.
    fn question_card_runs() -> [(&'static str, AnswerDecision); 2] {
        [
            (
                "answered",
                AnswerDecision::Answers {
                    answers: vec![pick(1)],
                },
            ),
            ("declined", AnswerDecision::Decline),
        ]
    }

    /// The events this build files for a run started on the engine whose link carries
    /// [`PLAN_QUESTION`], answered from an app with `decision` and then resolved by Codex, as the
    /// `event` frames a subscribed app is sent, in seq order: the link's `attached`, the card
    /// (`approval_request`), its resolution (`approval_resolved`). The run's uid is written
    /// [`QUESTION_CARD_UID`], the card's request id the one the daemon derives under that uid
    /// and [`QUESTION_CARD_LAUNCH`], and each clock read [`QUESTION_CARD_AT`].
    async fn question_card_run(decision: AnswerDecision) -> serde_json::Value {
        let rig = rig();
        let (asked, thread, turn) = plan_question();
        let (uid, mut ws) = engine_run(&rig, "req-question-card", &thread, &turn).await;
        link_states_once(&rig.daemon, &uid, 1).await;
        tell(&mut ws, &asked).await;
        let card = cards_of(&rig.daemon, &uid, 1).await.remove(0);
        let (result, _) = tokio::join!(decided(&rig.daemon, &uid, &card, decision), async {
            response_to(&mut ws, &asked["id"], Duration::from_secs(20)).await;
            tell(
                &mut ws,
                &resolved(&asked["params"]["threadId"], &asked["id"]),
            )
            .await;
        });
        assert!(
            matches!(result, protocol::ws::AnswerResult::Applied { .. }),
            "{result:?}"
        );
        filed_until(&rig.daemon, &uid, &format!("resolved:{}", card.request_id)).await;
        let frames: Vec<serde_json::Value> = rig
            .daemon
            .db
            .events_after(uid.clone(), 0, 10_000)
            .await
            .unwrap()
            .into_iter()
            .map(|event| {
                serde_json::to_value(protocol::ws::ServerMessage::Event { event }).unwrap()
            })
            .collect();
        drop(ws);
        end_run(&rig, &uid).await;
        let pinned = card_id(
            QUESTION_CARD_UID,
            &thread,
            &asked["id"],
            QUESTION_CARD_LAUNCH,
        )
        .unwrap();
        let mut out: serde_json::Value = serde_json::from_str(
            &json!({ "frames": frames })
                .to_string()
                .replace(&card.request_id, &pinned)
                .replace(&uid, QUESTION_CARD_UID),
        )
        .unwrap();
        for frame in out["frames"].as_array_mut().unwrap() {
            let ts = &mut frame["event"]["ts"];
            assert!(
                ts.as_str()
                    .is_some_and(|read| read.len() == 24 && read.ends_with('Z')),
                "ts was expected to be an RFC3339 clock read, and is {ts}"
            );
            *ts = json!(QUESTION_CARD_AT);
        }
        out
    }

    /// Every run of the question-card fixture, by name.
    async fn question_card_wire() -> serde_json::Value {
        let mut runs = serde_json::Map::new();
        for (run, decision) in question_card_runs() {
            runs.insert(run.to_string(), question_card_run(decision).await);
        }
        serde_json::Value::Object(runs)
    }

    /// **A Codex question card and its resolution reach the apps as this file says, and this
    /// file is what this build sends for them.**
    ///
    /// `fixtures/codex/question-card-wire.json` is the apps' input for a Codex question card:
    /// drawn from its `tool_input`, answered with `Answers` or `Decline`, and closed, on an app
    /// that did not answer it, with the decision its resolution carries. Gate one reads the
    /// committed bytes: in each run, one card whose `tool_name` is `request_user_input` and whose
    /// `display_text` hashes to its `payload_hash`, and one resolution naming that card and
    /// carrying that run's decision, from an iPhone. Gate two re-derives the file from this build
    /// and requires the same bytes.
    ///
    /// **Mutation:** edit a question's words in the checked-in file and gate one fails at the
    /// hash; change the card's shape in `codex_approval` and gate two fails, naming the command
    /// that rewrites the file.
    #[tokio::test]
    async fn the_codex_question_card_fixture_is_what_this_build_emits() {
        let committed: serde_json::Value =
            serde_json::from_str(QUESTION_CARD).expect("the fixture must be JSON");
        let runs = question_card_runs();
        assert_eq!(
            committed
                .as_object()
                .expect("the file is its runs, by name")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            runs.iter().map(|(run, _)| *run).collect::<Vec<_>>(),
        );
        for (run, decision) in runs {
            let events: Vec<protocol::event::Event> = committed[run]["frames"]
                .as_array()
                .expect("a run is a list of frames")
                .iter()
                .map(|frame| {
                    serde_json::from_value(frame["event"].clone())
                        .expect("an Event the apps decode")
                })
                .collect();
            let of_kind = |kind: EventKind| {
                events
                    .iter()
                    .filter(|event| event.kind == kind)
                    .collect::<Vec<_>>()
            };
            let cards = of_kind(EventKind::ApprovalRequest);
            assert_eq!(cards.len(), 1, "{run}: one card");
            let card: protocol::ws::ApprovalCard =
                serde_json::from_value(cards[0].payload["card"].clone())
                    .expect("a card the apps decode");
            assert_eq!(card.tool_name, "request_user_input", "{run}");
            assert_eq!(
                card.payload_hash,
                protocol::hash::sha256_hex(card.display_text.as_bytes()),
                "{run}: the hash the apps check"
            );
            let resolutions = of_kind(EventKind::ApprovalResolved);
            assert_eq!(resolutions.len(), 1, "{run}: one resolution");
            let resolved: protocol::ws::CodexResolutionPayload =
                serde_json::from_value(resolutions[0].payload.clone())
                    .expect("a resolution the apps decode");
            assert_eq!(resolved.request_id, card.request_id, "{run}");
            assert_eq!(
                resolved.resolution,
                CodexResolution::Answered {
                    by: ResolutionActor::Phone,
                    decision: Some(decision),
                    answered_on: Some(AnsweredOn::Iphone)
                },
                "{run}"
            );
        }

        let derived = question_card_wire().await;
        assert_eq!(
            derived, committed,
            "the committed question card no longer matches what this build emits. \
             Regenerate it: {REGENERATE_QUESTION_CARD}"
        );
        assert_eq!(
            QUESTION_CARD,
            format!("{}\n", serde_json::to_string_pretty(&derived).unwrap()),
            "the committed bytes are not this build's rendering of them (a reformat, or an edit \
             by hand). Regenerate it: {REGENERATE_QUESTION_CARD}"
        );
    }

    /// Rewrite `fixtures/codex/question-card-wire.json` with what this build sends.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_codex_question_card_fixture() {
        let wire = serde_json::to_string_pretty(&question_card_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/codex/question-card-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// The run the plan-item fixture is emitted for, written in place of the run's own uid.
    const PLAN_ITEM_UID: &str = "01K1B3XQ8ZC0DE5FGH7QP1AN00";
    const PLAN_ITEM_WIRE: &str = include_str!("../../../fixtures/codex/plan-item-wire.json");
    const REGENERATE_PLAN_ITEM: &str =
        "cargo test -p ccd --bin ccd -- --ignored regenerate_the_codex_plan_item_fixture";
    /// The plan codex 0.170.0 proposed in a real Plan turn once its question was answered: the
    /// item of its `item/completed`, whole, as ccd filed it. Its id is its turn's plus `-plan`.
    const PLAN_ITEM: &str = r#"{"completedAtMs":1795011310270,"id":"01a1ef02-a587-7982-99d5-2b63bab1d1c6-plan","startedAtMs":1795011309959,"text":"Once `notes.txt` exists, rename it to `a.txt`.\n","type":"plan"}"#;

    /// [`PLAN_ITEM`] as the plan of `turn`: its id named from `turn` as Codex names it.
    fn plan_item(turn: &str) -> serde_json::Value {
        let mut item: serde_json::Value = serde_json::from_str(PLAN_ITEM).unwrap();
        item["id"] = json!(format!("{turn}-plan"));
        item
    }

    /// The events this build files for a Plan turn on a run started on the engine, as the
    /// `event` frames a subscribed app is sent, in seq order: the link's `attached`, the
    /// person's message as Codex records it, [`PLAN_QUESTION`]'s card answered from an app with
    /// its second option and resolved by Codex, the turn's plan ([`plan_item`]) and the turn's
    /// end. Codex's frames are in the shape codex 0.164.0 sent them
    /// (`fixtures/codex/resume-follow-up-turn-0.164.0.json`), on the question's thread and turn.
    /// The run's uid is written [`PLAN_ITEM_UID`], the card's request id the one the daemon
    /// derives under that uid and [`QUESTION_CARD_LAUNCH`], and each clock read
    /// [`QUESTION_CARD_AT`].
    async fn plan_item_wire() -> serde_json::Value {
        let rig = rig();
        let (asked, thread, turn) = plan_question();
        let (uid, mut ws) = engine_run(&rig, "req-plan-item", &thread, &turn).await;
        link_states_once(&rig.daemon, &uid, 1).await;
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let mut said = capture["live"]["untilThePromptAnswer"][3].clone();
        said["params"]["threadId"] = json!(thread);
        said["params"]["turnId"] = json!(turn);
        tell(&mut ws, &turn_started(&thread, &turn)).await;
        tell(&mut ws, &said).await;
        let card = {
            tell(&mut ws, &asked).await;
            cards_of(&rig.daemon, &uid, 1).await.remove(0)
        };
        let answers = AnswerDecision::Answers {
            answers: vec![pick(1)],
        };
        let (result, _) = tokio::join!(decided(&rig.daemon, &uid, &card, answers), async {
            response_to(&mut ws, &asked["id"], Duration::from_secs(20)).await;
            tell(
                &mut ws,
                &resolved(&asked["params"]["threadId"], &asked["id"]),
            )
            .await;
        });
        assert!(
            matches!(result, protocol::ws::AnswerResult::Applied { .. }),
            "{result:?}"
        );
        filed_until(&rig.daemon, &uid, &format!("resolved:{}", card.request_id)).await;
        let item = plan_item(&turn);
        tell(
            &mut ws,
            &json!({"emittedAtMs": item["completedAtMs"], "method": "item/completed",
                    "params": {"completedAtMs": item["completedAtMs"], "item": item,
                               "threadId": thread, "turnId": turn}}),
        )
        .await;
        let mut ended = capture["live"]["afterTheMidTurnAnswer"][4].clone();
        ended["params"]["threadId"] = json!(thread);
        ended["params"]["turn"]["id"] = json!(turn);
        ended["params"]["turn"]["rootTurnId"] = json!(turn);
        ended["params"]["turn"]["items"] = json!([]);
        tell(&mut ws, &ended).await;
        filed_until(&rig.daemon, &uid, &format!(":turn:{turn}")).await;
        let frames: Vec<serde_json::Value> = rig
            .daemon
            .db
            .events_after(uid.clone(), 0, 10_000)
            .await
            .unwrap()
            .into_iter()
            .map(|event| {
                serde_json::to_value(protocol::ws::ServerMessage::Event { event }).unwrap()
            })
            .collect();
        drop(ws);
        end_run(&rig, &uid).await;
        let pinned = card_id(PLAN_ITEM_UID, &thread, &asked["id"], QUESTION_CARD_LAUNCH).unwrap();
        let mut out: serde_json::Value = serde_json::from_str(
            &json!({ "completed": { "frames": frames } })
                .to_string()
                .replace(&card.request_id, &pinned)
                .replace(&uid, PLAN_ITEM_UID),
        )
        .unwrap();
        for frame in out["completed"]["frames"].as_array_mut().unwrap() {
            let ts = &mut frame["event"]["ts"];
            assert!(
                ts.as_str()
                    .is_some_and(|read| read.len() == 24 && read.ends_with('Z')),
                "ts was expected to be an RFC3339 clock read, and is {ts}"
            );
            *ts = json!(QUESTION_CARD_AT);
        }
        out
    }

    /// **A Codex plan reaches the apps as this file says, and this file is what this build
    /// sends for it.**
    ///
    /// `fixtures/codex/plan-item-wire.json` is the apps' input for the plan Codex proposes in
    /// Plan mode, which ccd files as a `codex_plan` fact. Gate one reads the committed bytes:
    /// one turn, in which the person's message, the question's card and its resolution come
    /// before one `codex_plan` whose payload is the real item whole, which comes before the
    /// turn's end. Gate two re-derives the file from this build and requires the same bytes.
    ///
    /// **Mutation:** edit the plan's words in the checked-in file and gate one fails at the
    /// payload; stop filing an item type ccd does not model and gate two fails, naming the
    /// command that rewrites the file.
    #[tokio::test]
    async fn the_codex_plan_item_fixture_is_what_this_build_emits() {
        let committed: serde_json::Value =
            serde_json::from_str(PLAN_ITEM_WIRE).expect("the fixture must be JSON");
        let events: Vec<protocol::event::Event> = committed["completed"]["frames"]
            .as_array()
            .expect("the run is a list of frames")
            .iter()
            .map(|frame| {
                serde_json::from_value(frame["event"].clone()).expect("an Event the apps decode")
            })
            .collect();
        let (_, _, turn) = plan_question();
        // A resolution names its card, not a turn.
        let at = |kind: EventKind| {
            let found: Vec<_> = events.iter().filter(|event| event.kind == kind).collect();
            assert_eq!(found.len(), 1, "one {kind:?}: {events:?}");
            let named = (kind != EventKind::ApprovalResolved).then_some(turn.as_str());
            assert_eq!(found[0].turn_id.as_deref(), named, "{kind:?}'s turn");
            found[0].seq
        };
        let plan = at(EventKind::Other("codex_plan".into()));
        let order = [
            at(EventKind::UserMessage),
            at(EventKind::ApprovalRequest),
            at(EventKind::ApprovalResolved),
            plan,
            at(EventKind::TurnComplete),
        ];
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
        let filed = events.iter().find(|event| event.seq == plan).unwrap();
        assert_eq!(
            filed.payload,
            plan_item(&turn),
            "the plan's payload is the item, whole"
        );

        let derived = plan_item_wire().await;
        assert_eq!(
            derived, committed,
            "the committed plan item no longer matches what this build emits. \
             Regenerate it: {REGENERATE_PLAN_ITEM}"
        );
        assert_eq!(
            PLAN_ITEM_WIRE,
            format!("{}\n", serde_json::to_string_pretty(&derived).unwrap()),
            "the committed bytes are not this build's rendering of them (a reformat, or an edit \
             by hand). Regenerate it: {REGENERATE_PLAN_ITEM}"
        );
    }

    /// Rewrite `fixtures/codex/plan-item-wire.json` with what this build sends.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_codex_plan_item_fixture() {
        let wire = serde_json::to_string_pretty(&plan_item_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/codex/plan-item-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
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
                .map(|folders| launch_projects(&folders, Trust::NotTrusted))
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
        turn_begun(&rig.daemon, &uid, &mut ws, thread, turn).await;
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
        turn_begun(&rig.daemon, &uid, &mut ws, thread, turn).await;
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
            group_ends(leader).await,
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
        assert!(group_ends(leader).await, "the app-server outlived the stop");
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
        turn_begun(&rig.daemon, &uid, &mut ws, thread, turn).await;
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

    /// **X8-1 G9: with its link not attached, an engine run refuses an answer, a Stop and (X5)
    /// a message with the fact alone**, before its reconnect and after a reconnect that failed.
    #[tokio::test]
    async fn an_engine_run_with_no_link_refuses_an_answer_a_stop_and_a_message_with_the_fact() {
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
            let said = restarted
                .compose(
                    &uid,
                    "req-nolink-say",
                    "hello".into(),
                    &protocol::hash::compose_hash(&uid, "hello"),
                )
                .await;
            (answered, stopped, said)
        };
        let fact = crate::codex_refusals::ENGINE_NO_CONNECTION.to_string();
        let expected = (
            protocol::ws::AnswerResult::Rejected {
                reason: fact.clone(),
            },
            protocol::ws::InterruptResult::Rejected {
                reason: fact.clone(),
            },
            protocol::ws::ComposeResult::Rejected {
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

    /// A phone's message `text` for the run under `request_id`, in a task of its own, since it
    /// is answered only once Codex answers its `turn/start`.
    fn saying(
        daemon: &Arc<Daemon>,
        uid: &str,
        request_id: &str,
        text: &str,
    ) -> tokio::task::JoinHandle<protocol::ws::ComposeResult> {
        let (daemon, uid, request_id, text) = (
            Arc::clone(daemon),
            uid.to_string(),
            request_id.to_string(),
            text.to_string(),
        );
        tokio::spawn(async move {
            let hash = protocol::hash::compose_hash(&uid, &text);
            daemon.compose(&uid, &request_id, text, &hash).await
        })
    }

    /// The captured run's `turn` begun on the link: Codex's notice that it started, then its
    /// record of the turn's first message, waited for until filed. The link reads its frames in
    /// order, so it is then running the turn: a message sent after this joins it.
    async fn turn_begun(daemon: &Daemon, uid: &str, ws: &mut Socket, thread: &str, turn: &str) {
        let frames = frames_of(LIVE_FRAMES);
        tell(ws, &turn_started(thread, turn)).await;
        tell(ws, &frames[4]).await;
        tell(ws, &frames[5]).await;
        let item = frames[5]["params"]["item"]["id"].as_str().unwrap();
        let ids = filed_until(daemon, uid, &format!(":item:{item}")).await;
        assert_eq!(count(&ids, &format!(":item:{item}")), 1, "{ids:?}");
    }

    /// Codex's record of a person's message `text` under the client id `uuid`: the item `id`.
    fn message_item(id: &str, uuid: &str, text: &str) -> serde_json::Value {
        json!({"type": "userMessage", "id": id, "clientId": uuid,
               "content": [{"type": "text", "text": text, "text_elements": []}]})
    }

    /// Codex recording `item` in `turn` on `thread`: the item's start and its end.
    fn recorded(thread: &str, turn: &str, item: &serde_json::Value) -> [serde_json::Value; 2] {
        let params = json!({"threadId": thread, "turnId": turn, "item": item});
        [
            json!({"method": "item/started", "params": params}),
            json!({"method": "item/completed", "params": params}),
        ]
    }

    /// The payloads of the run's filed person's messages that read `text`, from ccd and from
    /// Codex's own record alike.
    async fn words(daemon: &Daemon, uid: &str, text: &str) -> Vec<serde_json::Value> {
        daemon
            .db
            .events_after(uid.to_string(), 0, 10_000)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == EventKind::UserMessage && event.payload["text"] == text)
            .map(|event| event.payload)
            .collect()
    }

    /// The compose ledger's state for the run's message `request_id`.
    async fn said(daemon: &Daemon, uid: &str, request_id: &str) -> Option<AnswerStatus> {
        daemon
            .db
            .mutation_status(
                crate::store::OPERATION_COMPOSE,
                uid.into(),
                request_id.into(),
            )
            .await
            .unwrap()
            .map(|state| state.status)
    }

    /// Once the run's message `request_id` is settled `outcome`, within 10 s.
    async fn settled_as(daemon: &Daemon, uid: &str, request_id: &str, outcome: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let want = Some(AnswerStatus::Settled(outcome.to_string()));
        while said(daemon, uid, request_id).await != want {
            assert!(
                Instant::now() < deadline,
                "{request_id} is {:?}, not {want:?}",
                said(daemon, uid, request_id).await
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// The run's `send_not_sent` notices, by source event id.
    async fn notices(daemon: &Daemon, uid: &str) -> Vec<(String, serde_json::Value)> {
        daemon
            .db
            .events_after(uid.to_string(), 0, 10_000)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.payload["notification_type"] == "send_not_sent")
            .map(|event| (event.source_event_id.unwrap_or_default(), event.payload))
            .collect()
    }

    /// The captured follow-up's answer once its turns ended, with its second turn's message
    /// replaced by `item`: a resume answer for the run on its thread.
    fn resumed_answer_with(item: &serde_json::Value) -> serde_json::Value {
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let mut after = capture["answerAfter"]["result"].clone();
        assert_eq!(
            after["thread"]["turns"][1]["items"][0]["type"],
            "userMessage"
        );
        after["thread"]["turns"][1]["items"][0] = item.clone();
        after
    }

    /// **X5-1 G1: a phone's message to an idle engine run starts a turn, and its words are
    /// filed once.** The link claims it and sends `turn/start` on the run's thread under the
    /// message's uuid. Codex's answer names a new turn: the phone is told `Started`, and the
    /// person's row is filed with its request id while the claim waits. Codex then records
    /// the message under that uuid, which settles it delivered and files nothing more, and
    /// neither does a restart's resume whose answer carries the same record. The same ask
    /// again is answered from the first and sends nothing.
    #[tokio::test]
    async fn a_message_to_an_idle_engine_run_starts_a_turn_and_its_words_are_filed_once() {
        let rig = rig();
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let turns = &capture["answerAfter"]["result"]["thread"]["turns"];
        let thread = capture["answerAfter"]["result"]["thread"]["id"]
            .as_str()
            .unwrap();
        let (first, turn) = (
            turns[0]["id"].as_str().unwrap(),
            turns[1]["id"].as_str().unwrap(),
        );
        let (uid, mut ws) = engine_run(&rig, "req-say", thread, first).await;
        let text = "and the docs";
        let uuid = crate::claude_host::send_uuid(&uid, "say-1");
        let sent = saying(&rig.daemon, &uid, "say-1", text);
        let start = asked(&mut ws, "turn/start").await;
        assert_eq!(
            start["params"],
            json!({"threadId": thread, "input": [{"type": "text", "text": text}],
                   "clientUserMessageId": uuid})
        );
        answer(&mut ws, &start, json!({"turn": {"id": turn}})).await;
        assert_eq!(
            sent.await.unwrap(),
            protocol::ws::ComposeResult::Started {
                turn_id: turn.into()
            }
        );
        assert_eq!(
            words(&rig.daemon, &uid, text).await,
            [json!({"text": text, "request_id": "say-1"})]
        );
        assert_eq!(
            said(&rig.daemon, &uid, "say-1").await,
            Some(AnswerStatus::Applying)
        );
        let item = message_item(turns[1]["items"][0]["id"].as_str().unwrap(), &uuid, text);
        tell(&mut ws, &turn_started(thread, turn)).await;
        for frame in recorded(thread, turn, &item) {
            tell(&mut ws, &frame).await;
        }
        let delivered = crate::store::compose_outcome(crate::store::COMPOSE_ROUTE_START, turn);
        settled_as(&rig.daemon, &uid, "say-1", &delivered).await;
        assert_eq!(
            words(&rig.daemon, &uid, text).await.len(),
            1,
            "after the echo"
        );
        assert_eq!(
            saying(&rig.daemon, &uid, "say-1", text).await.unwrap(),
            protocol::ws::ComposeResult::Duplicate {
                turn_id: turn.into(),
                started: true
            }
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(500), next_frame(&mut ws))
                .await
                .is_err(),
            "the same ask was sent again"
        );
        let (restarted, ws, _) = resumed_with(&rig, ws, resumed_answer_with(&item)).await;
        let ids = filed_until(&restarted, &uid, &format!(":turn:{turn}")).await;
        assert_eq!(
            words(&restarted, &uid, text).await.len(),
            1,
            "after the resume: {ids:?}"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **A phone's message is filed naming the turn it started or joined**, so a device that did
    /// not send it learns the turn is running, and can stop it, before Codex records any item of
    /// it: Codex's own record of those words, which names the turn, is not filed a second time.
    #[tokio::test]
    async fn a_message_names_the_turn_it_started_or_joined() {
        let rig = rig();
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let turns = &capture["answerAfter"]["result"]["thread"]["turns"];
        let thread = capture["answerAfter"]["result"]["thread"]["id"]
            .as_str()
            .unwrap();
        let (first, turn) = (
            turns[0]["id"].as_str().unwrap(),
            turns[1]["id"].as_str().unwrap(),
        );
        let (uid, mut ws) = engine_run(&rig, "req-named", thread, first).await;
        let named = |daemon: Arc<Daemon>, uid: String| async move {
            daemon
                .db
                .events_after(uid, 0, 10_000)
                .await
                .unwrap()
                .into_iter()
                .map(|event| (event.payload["request_id"].clone(), event.turn_id))
                .collect::<Vec<_>>()
        };
        let sent = saying(&rig.daemon, &uid, "say-start", "and the docs");
        let start = asked(&mut ws, "turn/start").await;
        answer(&mut ws, &start, json!({"turn": {"id": turn}})).await;
        assert_eq!(
            sent.await.unwrap(),
            protocol::ws::ComposeResult::Started {
                turn_id: turn.into()
            }
        );
        assert!(
            named(Arc::clone(&rig.daemon), uid.clone())
                .await
                .contains(&(json!("say-start"), Some(turn.to_string()))),
            "the message that started the turn: {:?}",
            named(Arc::clone(&rig.daemon), uid.clone()).await
        );
        tell(&mut ws, &turn_started(thread, turn)).await;
        let thought =
            json!({"type": "reasoning", "id": "item-thought", "summary": [], "content": []});
        for frame in recorded(thread, turn, &thought) {
            tell(&mut ws, &frame).await;
        }
        filed_until(&rig.daemon, &uid, ":item:item-thought").await;
        let sent = saying(&rig.daemon, &uid, "say-steer", "and the tests");
        let steer = asked(&mut ws, "turn/start").await;
        answer(&mut ws, &steer, json!({"turn": {"id": turn}})).await;
        assert_eq!(
            sent.await.unwrap(),
            protocol::ws::ComposeResult::Steered {
                turn_id: turn.into()
            }
        );
        assert!(
            named(Arc::clone(&rig.daemon), uid.clone())
                .await
                .contains(&(json!("say-steer"), Some(turn.to_string()))),
            "the message that joined the turn: {:?}",
            named(Arc::clone(&rig.daemon), uid.clone()).await
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **X5-1 G2: a message added to a running turn that a Stop ends before Codex reads it is
    /// handed back; one Codex recorded first is not.** Two messages with the same words under
    /// two request ids join the running turn: the phone is told `Steered` for each. Codex
    /// records the second, which settles it delivered. A Stop then ends the turn
    /// `interrupted`: the first, which Codex never recorded, settles `stopped` with its
    /// `send_not_sent` notice, and the second gets none. Asked again, the first is told it was
    /// not sent, and nothing is sent.
    #[tokio::test]
    async fn a_message_a_stop_cuts_off_is_handed_back_and_one_codex_recorded_is_not() {
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[2]["params"]["threadId"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-cut", thread, turn).await;
        turn_begun(&rig.daemon, &uid, &mut ws, thread, turn).await;
        let text = "and the docs";
        let steered = protocol::ws::ComposeResult::Steered {
            turn_id: turn.into(),
        };
        for request_id in ["say-a", "say-b"] {
            let sent = saying(&rig.daemon, &uid, request_id, text);
            let start = asked(&mut ws, "turn/start").await;
            answer(&mut ws, &start, json!({"turn": {"id": turn}})).await;
            assert_eq!(sent.await.unwrap(), steered, "{request_id}");
        }
        let (a, b) = (
            crate::claude_host::send_uuid(&uid, "say-a"),
            crate::claude_host::send_uuid(&uid, "say-b"),
        );
        for frame in recorded(thread, turn, &message_item("item-said-b", &b, text)) {
            tell(&mut ws, &frame).await;
        }
        let delivered = crate::store::compose_outcome(crate::store::COMPOSE_ROUTE_STEER, turn);
        settled_as(&rig.daemon, &uid, "say-b", &delivered).await;
        let stop = stopping(&rig.daemon, &uid, "stop-cut", turn);
        let interrupt = asked(&mut ws, "turn/interrupt").await;
        answer(&mut ws, &interrupt, json!({})).await;
        tell(&mut ws, &its_turn_ended("interrupted")).await;
        assert_eq!(
            stop.await.unwrap(),
            protocol::ws::InterruptResult::Aborted {
                turn_id: turn.into()
            }
        );
        settled_as(&rig.daemon, &uid, "say-a", crate::store::COMPOSE_STOPPED).await;
        assert_eq!(
            notices(&rig.daemon, &uid).await,
            [(
                format!("not_sent:{a}"),
                json!({"notification_type": "send_not_sent", "message": "Not sent: stopped",
                       "request_id": "say-a", "reason": "stopped"})
            )]
        );
        assert_eq!(
            said(&rig.daemon, &uid, "say-b").await,
            Some(AnswerStatus::Settled(delivered))
        );
        assert_eq!(
            words(&rig.daemon, &uid, text).await,
            [
                json!({"text": text, "request_id": "say-a"}),
                json!({"text": text, "request_id": "say-b"})
            ]
        );
        assert_eq!(
            saying(&rig.daemon, &uid, "say-a", text).await.unwrap(),
            protocol::ws::ComposeResult::Rejected {
                reason: crate::codex_refusals::REPLAY_COMPOSE_STOPPED.into()
            }
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(500), next_frame(&mut ws))
                .await
                .is_err(),
            "the stopped message was sent again"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **X5-1, the crash seam: a message whose row ccd never filed is shown once, from Codex's
    /// own record.** ccd stopped between Codex taking the message and filing the person's row:
    /// its claim is `applying` and there is no row. The restarted ccd's resume answer carries
    /// Codex's record of the message, which is filed once.
    #[tokio::test]
    async fn a_message_whose_row_was_never_filed_is_shown_once_from_codexs_record() {
        let rig = rig();
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let turns = &capture["answerAfter"]["result"]["thread"]["turns"];
        let thread = capture["answerAfter"]["result"]["thread"]["id"]
            .as_str()
            .unwrap();
        let (first, turn) = (
            turns[0]["id"].as_str().unwrap(),
            turns[1]["id"].as_str().unwrap(),
        );
        let (uid, ws) = engine_run(&rig, "req-seam", thread, first).await;
        let text = "and the docs";
        let claimed = crate::store::ClaimedMaterial {
            thread_id: thread.into(),
            generation: 0,
            route: crate::store::COMPOSE_ROUTE_START.into(),
            target_turn_id: None,
            claimed_hash: protocol::hash::compose_hash(&uid, text),
            question_decision: None,
        };
        assert_eq!(
            rig.daemon
                .db
                .claim_mutation(
                    crate::store::OPERATION_COMPOSE,
                    uid.clone(),
                    "say-seam".into(),
                    claimed,
                    protocol::time::now_rfc3339(),
                )
                .await
                .unwrap(),
            crate::store::MutationClaim::Claimed
        );
        let uuid = crate::claude_host::send_uuid(&uid, "say-seam");
        let item = message_item(turns[1]["items"][0]["id"].as_str().unwrap(), &uuid, text);
        let (restarted, ws, _) = resumed_with(&rig, ws, resumed_answer_with(&item)).await;
        let ids = filed_until(&restarted, &uid, &format!(":turn:{turn}")).await;
        assert_eq!(
            words(&restarted, &uid, text).await,
            [json!({"text": text, "interrupted": false})],
            "{ids:?}"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **X5-1: a message retried after its run's link has gone is answered from the record.**
    /// Codex took the first into the running turn and the connection ended before it recorded
    /// it, so its claim is still `applying`: the retry is told it was sent and what became of
    /// it is not known, never that nothing was.
    #[tokio::test]
    async fn a_message_retried_after_its_link_has_gone_is_answered_from_the_record() {
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[2]["params"]["threadId"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-resay", thread, turn).await;
        turn_begun(&rig.daemon, &uid, &mut ws, thread, turn).await;
        let sent = saying(&rig.daemon, &uid, "say-gone", "and the docs");
        let start = asked(&mut ws, "turn/start").await;
        answer(&mut ws, &start, json!({"turn": {"id": turn}})).await;
        assert!(matches!(
            sent.await.unwrap(),
            protocol::ws::ComposeResult::Steered { .. }
        ));
        drop(ws);
        link_gone(&rig.daemon, &uid).await;
        assert_eq!(
            said(&rig.daemon, &uid, "say-gone").await,
            Some(AnswerStatus::Applying)
        );
        assert_eq!(
            saying(&rig.daemon, &uid, "say-gone", "and the docs")
                .await
                .unwrap(),
            protocol::ws::ComposeResult::Indeterminate {
                reason: crate::codex_refusals::COMPOSE_ALREADY_SENT_UNKNOWN.into()
            }
        );
        end_run(&rig, &uid).await;
    }

    /// The run `uid`'s summary as the fleet reports it.
    async fn summary_of(daemon: &Daemon, uid: &str) -> protocol::event::SessionSummary {
        daemon
            .sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|summary| summary.session_uid == uid)
            .expect("the run is in the fleet")
    }

    /// **X5-1, round 1's F1: the fleet offers an engine run's link while it is attached and
    /// live.** Both apps offer Stop and the composer on the summary's `codex_link`. It reads
    /// `subscribed` for a live run whose link is attached, and `none` when a restarted ccd has not
    /// connected the run again, and while the run is stopping.
    #[tokio::test]
    async fn the_fleet_offers_an_engine_runs_link_only_while_it_is_attached_and_live() {
        use protocol::event::CodexLink;
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-fleet", "thr-fleet", "turn-1").await;
        assert_eq!(
            summary_of(&rig.daemon, &uid).await.codex_link,
            CodexLink::Subscribed
        );
        let restarted = daemon_on(&rig.root);
        restarted.recover().await;
        assert_eq!(
            summary_of(&restarted, &uid).await.codex_link,
            CodexLink::None,
            "before its reconnect"
        );
        assert_eq!(
            rig.daemon.stop_run(&uid).await,
            protocol::ws::StopRunResult::Stopping
        );
        assert_eq!(
            summary_of(&rig.daemon, &uid).await.codex_link,
            CodexLink::None,
            "while it is stopping"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// The `link` word of each of the run's `link_state` facts, in the order filed.
    async fn link_states(daemon: &Daemon, uid: &str) -> Vec<String> {
        daemon
            .db
            .events_after(uid.to_string(), 0, 10_000)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == EventKind::LinkState)
            .map(|event| {
                event.payload["link"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    /// The run's `link_state` facts once there are `n`, within 10 s.
    async fn link_states_once(daemon: &Daemon, uid: &str, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let states = link_states(daemon, uid).await;
            if states.len() >= n || Instant::now() >= deadline {
                return states;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// **An engine run's link files a `link_state` fact when it attaches, on a start and on a
    /// reconnect, once the link is in place**: the fleet then offers the run (`subscribed`), so
    /// an app that reads the fleet on the fact sees what it can do. The first daemon's dropped
    /// connection, its run still live, files one `detached`.
    #[tokio::test]
    async fn an_engine_runs_link_files_its_attach_on_a_start_and_a_reconnect() {
        use protocol::event::CodexLink;
        let rig = rig();
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let after = capture["answerAfter"]["result"].clone();
        let thread = after["thread"]["id"].as_str().unwrap();
        let first = after["thread"]["turns"][0]["id"].as_str().unwrap();
        let (uid, ws) = engine_run(&rig, "req-attach", thread, first).await;
        assert_eq!(link_states_once(&rig.daemon, &uid, 1).await, ["attached"]);
        assert_eq!(
            summary_of(&rig.daemon, &uid).await.codex_link,
            CodexLink::Subscribed,
            "at the start's attach"
        );
        let (restarted, ws, _) = resumed_with(&rig, ws, after).await;
        assert_eq!(
            link_states_once(&restarted, &uid, 3).await,
            ["attached", "detached", "attached"]
        );
        assert_eq!(
            summary_of(&restarted, &uid).await.codex_link,
            CodexLink::Subscribed,
            "at the reconnect's attach"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **A run ended by End run files no `detached` fact**: its link ends while it is
    /// `stopping`, and its end is the fact that tells.
    #[tokio::test]
    async fn an_end_run_files_no_detached_fact() {
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-end-quiet", "thr-end-quiet", "turn-1").await;
        assert_eq!(link_states_once(&rig.daemon, &uid, 1).await, ["attached"]);
        assert_eq!(
            rig.daemon.stop_run(&uid).await,
            protocol::ws::StopRunResult::Stopping
        );
        drop(ws);
        row_once(&rig, &uid, Lifecycle::Exited).await;
        link_gone(&rig.daemon, &uid).await;
        assert_eq!(link_states(&rig.daemon, &uid).await, ["attached"]);
        end_run(&rig, &uid).await;
    }

    /// The run `uid`'s summary as the fleet sends it, its identity, folder, times and count
    /// of events fixed so the fixture is the same on every run.
    async fn summary_as_sent(daemon: &Daemon, uid: &str) -> serde_json::Value {
        let mut sent = serde_json::to_value(summary_of(daemon, uid).await).unwrap();
        let fixed = [
            ("session_uid", json!("01K1B3XQ8ZC0DE5FGH7JKMNPQR")),
            ("session_id", json!("01K1B3XQ8ZC0DE5FGH7JKMNPQR")),
            ("cwd", json!("/Users/person/project")),
            ("project_label", json!("project")),
            ("created_at", json!("2026-11-15T00:00:00Z")),
            ("updated_at", json!("2026-11-15T00:00:00Z")),
            ("last_seq", json!(0)),
        ];
        for (key, value) in fixed {
            assert!(sent.get(key).is_some(), "the summary carries no {key}");
            sent[key] = value;
        }
        sent
    }

    /// The run `uid`'s events filed after the seq `after`, each as a phone reads its kind, source event id and turn,
    /// the run's uid written `{uid}`, an end's launch `{leader}` and a settings fact's own id `{report}`; and the last
    /// seq among them.
    async fn events_after_as_sent(
        daemon: &Daemon,
        uid: &str,
        after: u64,
    ) -> (Vec<serde_json::Value>, u64) {
        let events = daemon
            .db
            .events_after(uid.to_string(), after, 10_000)
            .await
            .unwrap();
        let last = events.last().map_or(after, |event| event.seq);
        let sent = events
            .into_iter()
            .map(|event| {
                let id = event.source_event_id.map(|id| match id.replace(uid, "{uid}") {
                    end if end.starts_with("exit:{uid}:") => "exit:{uid}:{leader}".to_string(),
                    named if named.contains(":settings:") => {
                        let (thread, _) = named.split_once(":settings:").unwrap();
                        format!("{thread}:settings:{{report}}")
                    }
                    id => id,
                });
                json!({
                    "kind": event.kind.as_str(),
                    "source_event_id": id,
                    "turn_id": event.turn_id,
                    "link": event.payload.get("link").filter(|_| event.kind == EventKind::LinkState),
                })
            })
            .collect();
        (sent, last)
    }

    /// One step of [`derived_fleet_over_a_run`]: the events filed since the step before (`seen`, moved on), and the
    /// run's summary as the fleet sends it now.
    async fn fleet_step(
        daemon: &Daemon,
        uid: &str,
        step: &str,
        seen: &mut u64,
    ) -> serde_json::Value {
        let (events, last) = events_after_as_sent(daemon, uid, *seen).await;
        *seen = last;
        json!({"step": step, "events": events, "summary": summary_as_sent(daemon, uid).await})
    }

    /// Once `daemon` has the run `uid`'s link attached, within 10 s: in place to take commands,
    /// and its `link_state` "attached", filed just after, the run's last.
    async fn link_attached(daemon: &Daemon, uid: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon.engine_commands_for_tests(uid).await.is_none()
            || link_states(daemon, uid).await.last().map(String::as_str) != Some("attached")
        {
            assert!(Instant::now() < deadline, "the run's link did not attach");
            tokio::time::sleep(POLL).await;
        }
    }

    /// **The fleet over a run started on the engine, as a phone reads it**, step by step, each with the events
    /// filed since the step before:
    /// - started, still `starting`;
    /// - its first message taken and recorded by Codex, live;
    /// - a ccd restart before its reconnect;
    /// - after its reconnect, whose resume answer's events are filed before the link attaches;
    /// - End run, `stopping`;
    /// - its end.
    async fn derived_fleet_over_a_run() -> serde_json::Value {
        let rig = rig();
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let after = capture["answerAfter"]["result"].clone();
        let thread = after["thread"]["id"].as_str().unwrap().to_string();
        let first = after["thread"]["turns"][0].clone();
        let first_turn = first["id"].as_str().unwrap().to_string();
        let codex = stand_in(&rig, "exec sleep 30");
        let mut script = whole_start();
        script.truncate(3);
        script[2].1 = json!({"result": {"thread": {"id": thread}, "model": "gpt-test"}});
        let (result, (_, mut ws)) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-life", &codex)),
            play_app_server(&rig, script)
        );
        let uid = started_uid(&result);
        // The start answers before its first message's task records the thread Codex started.
        let deadline = Instant::now() + Duration::from_secs(10);
        while rig
            .daemon
            .db
            .get_session(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .codex_thread_id
            .is_none()
        {
            assert!(
                Instant::now() < deadline,
                "the run's thread was not recorded"
            );
            tokio::time::sleep(POLL).await;
        }
        let mut seen = 0;
        let mut steps = vec![fleet_step(&rig.daemon, &uid, "started", &mut seen).await];
        let asked_first = asked(&mut ws, "turn/start").await;
        answer(&mut ws, &asked_first, json!({"turn": {"id": first_turn}})).await;
        tell(&mut ws, &turn_started(&thread, &first_turn)).await;
        for frame in recorded(&thread, &first_turn, &first["items"][0]) {
            tell(&mut ws, &frame).await;
        }
        let item = first["items"][0]["id"].as_str().unwrap();
        filed_until(&rig.daemon, &uid, &format!(":item:{item}")).await;
        link_attached(&rig.daemon, &uid).await;
        steps.push(fleet_step(&rig.daemon, &uid, "first message taken", &mut seen).await);
        let before = daemon_on(&rig.root);
        before.recover().await;
        steps.push(
            fleet_step(
                &before,
                &uid,
                "ccd restarted, before its reconnect",
                &mut seen,
            )
            .await,
        );
        let (restarted, ws, _) = resumed_with(&rig, ws, after).await;
        link_attached(&restarted, &uid).await;
        steps.push(fleet_step(&restarted, &uid, "reconnected", &mut seen).await);
        assert_eq!(
            restarted.stop_run(&uid).await,
            protocol::ws::StopRunResult::Stopping
        );
        steps.push(fleet_step(&restarted, &uid, "End run", &mut seen).await);
        drop(ws);
        row_once(&rig, &uid, Lifecycle::Exited).await;
        // The escalation can end the run before its link has seen the connection close.
        link_gone(&restarted, &uid).await;
        steps.push(fleet_step(&restarted, &uid, "ended", &mut seen).await);
        end_run(&rig, &uid).await;
        json!(steps)
    }

    /// The summary, as the fleet sends it, of a run started on the engine whose app-server ended
    /// without ccd stopping it: one the person's next message relaunches.
    async fn relaunchable_summary() -> serde_json::Value {
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-relaunchable", "thr-relaunchable", "turn-1").await;
        app_server_ended(&rig, &uid, ws).await;
        summary_as_sent(&rig.daemon, &uid).await
    }

    /// **A message to a run started on the engine, then Stop, as this build files them**: the
    /// person's row with its request id, and the `send_not_sent` notice for it, under
    /// `fixtures/claude/minor-37-wire.json`'s keys and in its shape. Beside them, what the apps
    /// decide on whether to offer the composer and Stop: the capabilities a paired phone is
    /// offered, the run's summary as the fleet sends it while it is live, while it is stopping
    /// and once End run has ended it, the summary of a run a message relaunches
    /// ([`relaunchable_summary`]), and the fleet over a run's whole life
    /// ([`derived_fleet_over_a_run`]).
    async fn derived_compose_stop_wire() -> serde_json::Value {
        let fleet_over_a_run = derived_fleet_over_a_run().await;
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[2]["params"]["threadId"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-wire", thread, turn).await;
        turn_begun(&rig.daemon, &uid, &mut ws, thread, turn).await;
        let live = summary_as_sent(&rig.daemon, &uid).await;
        let sent = saying(&rig.daemon, &uid, "send-1", "and the docs");
        let start = asked(&mut ws, "turn/start").await;
        answer(&mut ws, &start, json!({"turn": {"id": turn}})).await;
        sent.await.unwrap();
        let stop = stopping(&rig.daemon, &uid, "stop-1", turn);
        let interrupt = asked(&mut ws, "turn/interrupt").await;
        answer(&mut ws, &interrupt, json!({})).await;
        tell(&mut ws, &its_turn_ended("interrupted")).await;
        stop.await.unwrap();
        settled_as(&rig.daemon, &uid, "send-1", crate::store::COMPOSE_STOPPED).await;
        let person_row = words(&rig.daemon, &uid, "and the docs").await;
        let send_not_sent = notices(&rig.daemon, &uid).await;
        assert_eq!(
            rig.daemon.stop_run(&uid).await,
            protocol::ws::StopRunResult::Stopping
        );
        let ending = summary_as_sent(&rig.daemon, &uid).await;
        drop(ws);
        row_once(&rig, &uid, Lifecycle::Exited).await;
        link_gone(&rig.daemon, &uid).await;
        let ended = summary_as_sent(&rig.daemon, &uid).await;
        end_run(&rig, &uid).await;
        assert_eq!((person_row.len(), send_not_sent.len()), (1, 1));
        json!({
            "hello_ack_capabilities": crate::ws_server::capabilities(&rig.daemon, false, false, true),
            "session_summary": live,
            "session_summary_stopping": ending,
            "session_summary_ended_by_end_run": ended,
            "session_summary_relaunchable": relaunchable_summary().await,
            "fleet_over_a_run": fleet_over_a_run,
            "person_row": person_row[0],
            "send_not_sent": send_not_sent[0].1,
        })
    }

    /// **A Codex run's message and its Stop, as this build files them**, against
    /// `fixtures/codex/compose-stop-wire.json`.
    #[tokio::test]
    async fn the_codex_compose_stop_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/codex/compose-stop-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_compose_stop_wire().await,
            committed,
            "the committed Codex compose-and-Stop records no longer match what this build files. \
             Regenerate them with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_codex_compose_stop_wire_fixture"
        );
    }

    /// Rewrite `fixtures/codex/compose-stop-wire.json` with what this build files.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_codex_compose_stop_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_compose_stop_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/codex/compose-stop-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    // ------------------------------- mail on a run started on the engine

    /// One message turn as codex 0.147 sent it, from its thread's start to its end; its user
    /// message the person's, with no client id of ccd's (fixtures/codex/lifecycle.jsonl).
    const LIFECYCLE: &str = include_str!("../../../fixtures/codex/lifecycle.jsonl");
    /// [`LIFECYCLE`]'s thread.
    const LIFECYCLE_THREAD: &str = "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86";
    /// [`LIFECYCLE`]'s turn.
    const LIFECYCLE_TURN: &str = "01a0127a-d9cd-7461-84d7-6eea6d0b98a5";
    /// The words of [`LIFECYCLE`]'s user message.
    const LIFECYCLE_WORDS: &str = "reply with the word pong and nothing else";

    /// [`LIFECYCLE`]'s frames, its user message `line`: with a wake line, the turn a mail wake
    /// starts, as Codex runs a line `codex queue` put on the thread.
    fn lifecycle_turn(line: &str) -> Vec<serde_json::Value> {
        frames_of(&LIFECYCLE.replace(LIFECYCLE_WORDS, line))
    }

    /// The runs the mailbox reads as taking mail, each with the minor kept for it.
    async fn mail_minors(daemon: &Daemon) -> Vec<(String, u32)> {
        daemon
            .db
            .mail_runs()
            .await
            .unwrap()
            .into_iter()
            .map(|(row, minor)| (row.session_uid, minor))
            .collect()
    }

    /// What an agent's message to the run `uid`, from another run on this computer (`studio`,
    /// as the recorded tailnet names it), is told.
    async fn mail_from_another_run(daemon: &Arc<Daemon>, uid: &str) -> protocol::ipc::MailState {
        let tailnet = protocol::tailnet::Tailnet::parse(include_str!(
            "../../../fixtures/tailscale/status-six-nodes.json"
        ))
        .unwrap();
        let sender = protocol::uid::new().unwrap();
        let row = SessionRow {
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
        assert_eq!(daemon.store.begin_run(&row, &sender, None).unwrap(), None);
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

    /// **The mail item's D1 and D2 on Codex: a run started on the engine takes mail from the
    /// moment its first send makes it `live`.** It is kept with this build's minor, so it is
    /// listed and mail from another run on this computer waits for it. A start whose first send
    /// fails is never kept, and mail for it is refused.
    #[tokio::test]
    async fn an_engine_codex_run_takes_mail_once_live_and_a_failed_start_never_does() {
        let _env = crate::ProcessEnvRead::take();
        let (live, failing) = (rig(), rig());
        let (uid, ws) = engine_run(&live, "req-mail", "thr-mail", "turn-1").await;
        // The link attaches after the run is kept.
        link_states_once(&live.daemon, &uid, 1).await;
        assert_eq!(
            mail_minors(&live.daemon).await,
            [(uid.clone(), protocol::PROTOCOL_MINOR)]
        );
        assert_eq!(
            mail_from_another_run(&live.daemon, &uid).await,
            protocol::ipc::MailState::Waiting
        );
        drop(ws);
        end_run(&live, &uid).await;

        let codex = stand_in(&failing, "exec sleep 30");
        let script = [
            whole_start()[0].clone(),
            ("skills/list", json!({"result": {"data": []}})),
            (
                "thread/start",
                json!({"error": {"code": -32600, "message": "invalid config"}}),
            ),
        ];
        let (result, (_, _ws)) = tokio::join!(
            start(&failing.daemon, run(&failing, "req-mail-failed", &codex)),
            play_app_server(&failing, script.to_vec())
        );
        let uid = started_uid(&result);
        assert_eq!(
            row_once(&failing, &uid, Lifecycle::Failed).await.lifecycle,
            Lifecycle::Failed
        );
        assert!(mail_minors(&failing.daemon).await.is_empty());
        assert_eq!(
            mail_from_another_run(&failing.daemon, &uid).await,
            protocol::ipc::MailState::Refused
        );
    }

    /// Unread mail `message_id` for the run `uid`, as the mailbox keeps an agent's message.
    fn mail_kept_for(rig: &Rig, uid: &str, message_id: &str) {
        rig.daemon
            .store
            .take_mail(
                &crate::store::MailIntake {
                    message_id: message_id.into(),
                    context_id: format!("ctx-{message_id}"),
                    in_reply_to: None,
                    session_uid: uid.into(),
                    sender_computer: String::new(),
                    sender_uid: "01JQXV9K7B0000000000000SND".into(),
                    sender: "01JQXV9K7B0000000000000SND@studio".into(),
                    sender_label: "cc-2@studio".into(),
                    sender_agent: "Codex".into(),
                    sender_project: "api".into(),
                    text: "hi".into(),
                },
                protocol::time::now_unix_ms(),
                crate::mail::LIMITS,
            )
            .unwrap();
    }

    /// [`engine_run`] with unread mail and a wake outstanding for it, as the mailbox leaves them
    /// once it has queued a Codex nudge.
    async fn engine_run_woken(
        rig: &Rig,
        request_id: &str,
        thread: &str,
        turn: &str,
    ) -> (String, Socket) {
        let (uid, ws) = engine_run(rig, request_id, thread, turn).await;
        mail_kept_for(rig, &uid, "m1");
        assert!(
            rig.daemon
                .store
                .claim_mail_wake(&uid, protocol::time::now_unix_ms(), false)
                .unwrap(),
            "the premise: a wake is outstanding for unread mail"
        );
        (uid, ws)
    }

    /// The run's `(outstanding, misses)` in the mailbox's wake table.
    fn wake_row(rig: &Rig, uid: &str) -> (i64, i64) {
        rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .query_row(
                "SELECT outstanding, misses FROM mail_wake WHERE session_uid = ?1",
                [uid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    /// Whether the wake table reaches `want` within 5 s: the rule runs on a task of its own.
    async fn wake_row_reaches(rig: &Rig, uid: &str, want: (i64, i64)) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while wake_row(rig, uid) != want {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
        true
    }

    /// Whether a wake for the run's unread mail is claimed within 5 s. The rule's own re-wake,
    /// which cannot be issued here, may hold the claim for a moment before it gives it back.
    async fn claimed_within(rig: &Rig, uid: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let claimed = rig
                .daemon
                .store
                .claim_mail_wake(uid, protocol::time::now_unix_ms(), false)
                .unwrap();
            if claimed || Instant::now() >= deadline {
                return claimed;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// **Through the engine link, a turn that did not carry the nudge leaves the wake
    /// outstanding** (`codex_link.rs`'s M-07, and the design check's F1): Codex runs a nudge
    /// queued during a busy turn only after that turn ends, and counting the busy turn's end
    /// queued a second nudge, whose turn read "No new messages" (measured at 5d42ce3).
    ///
    /// **Mutation:** count every turn end in [`crate::mail::note_filed_event`]. The busy turn
    /// then clears the wake.
    #[tokio::test]
    async fn a_turn_on_the_engine_that_did_not_carry_the_nudge_leaves_the_wake_outstanding() {
        let rig = rig();
        let (uid, mut ws) = engine_run_woken(&rig, "req-busy", LIFECYCLE_THREAD, "turn-1").await;
        for frame in lifecycle_turn(LIFECYCLE_WORDS) {
            tell(&mut ws, &frame).await;
        }
        let ended = format!(":turn:{LIFECYCLE_TURN}");
        let ids = filed_until(&rig.daemon, &uid, &ended).await;
        assert_eq!(count(&ids, &ended), 1, "{ids:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            wake_row(&rig, &uid),
            (1, 0),
            "the person's busy turn ended, and the nudge queued behind it has not run"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **Through the engine link, a nudge turn a resume answer describes ends the outstanding
    /// wake, once** (`codex_link.rs`'s rule, measured on the rig at 19244c6: a woken Codex was
    /// never woken again): a restarted ccd learns of the woken turn only from its reconnect's
    /// answer, and the live frame for the same turn after it counts nothing more.
    ///
    /// **Mutation:** make [`file`] ignore what [`Daemon::ingest`] newly filed. The answer then
    /// ends nothing, and `misses` stays 0.
    #[tokio::test]
    async fn an_engine_nudge_turn_described_by_a_resume_answer_ends_the_outstanding_wake_once() {
        let _env = crate::ProcessEnvRead::take();
        let rig = rig();
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let thread = &capture["answerAfter"]["result"]["thread"];
        let (uid, ws) = engine_run_woken(
            &rig,
            "req-resumed",
            thread["id"].as_str().unwrap(),
            thread["turns"][0]["id"].as_str().unwrap(),
        )
        .await;
        let mut nudge = thread["turns"][1]["items"][0].clone();
        nudge["clientId"] = serde_json::Value::Null;
        nudge["content"][0]["text"] = json!(protocol::ipc::MAIL_NUDGE_ACT);
        let (_restarted, mut ws, _) = resumed_with(&rig, ws, resumed_answer_with(&nudge)).await;
        assert!(
            wake_row_reaches(&rig, &uid, (0, 1)).await,
            "the described nudge turn's end reached the wake rule: {:?}",
            wake_row(&rig, &uid)
        );

        // A wake outstanding again, so that a second turn end would show as a miss.
        assert!(
            claimed_within(&rig, &uid).await,
            "a wake outstanding again: {:?}",
            wake_row(&rig, &uid)
        );
        let live_end = capture["live"]["afterTheMidTurnAnswer"]
            .as_array()
            .unwrap()
            .iter()
            .find(|frame| frame["method"] == "turn/completed")
            .expect("the capture completes the turn")
            .clone();
        tell(&mut ws, &live_end).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            wake_row(&rig, &uid),
            (1, 1),
            "the live frame for a turn the log already holds is no second turn end"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **Through the engine link, a person's message resumes wakes that had stopped**
    /// (`codex_link.rs`'s M-08): after three woken turns left the mail unread, a message from an
    /// app is the person's turn (its words are no wake line), and wakes start again, as a
    /// prompt does for Claude.
    ///
    /// **Mutation:** make [`file`] ignore what [`Daemon::ingest`] newly filed. The misses stay at
    /// three and the mail is never woken for again.
    #[tokio::test]
    async fn a_persons_message_to_an_engine_run_resumes_wakes_that_had_stopped() {
        let _env = crate::ProcessEnvRead::take();
        let rig = rig();
        let (uid, mut ws) = engine_run_woken(&rig, "req-person", "thr-person", "turn-1").await;
        for miss in 1..=3 {
            if miss > 1 {
                assert!(rig
                    .daemon
                    .store
                    .claim_mail_wake(&uid, protocol::time::now_unix_ms(), false)
                    .unwrap());
            }
            rig.daemon
                .store
                .mail_turn_ended(&uid, protocol::time::now_unix_ms(), 3)
                .unwrap();
        }
        assert_eq!(
            wake_row(&rig, &uid),
            (0, 3),
            "the premise: wakes have stopped"
        );
        let sent = saying(&rig.daemon, &uid, "say-1", "and the docs");
        let start = asked(&mut ws, "turn/start").await;
        answer(&mut ws, &start, json!({"turn": {"id": "turn-2"}})).await;
        sent.await.unwrap();
        assert!(
            wake_row_reaches(&rig, &uid, (0, 0)).await,
            "the person's message resumed wakes: {:?}",
            wake_row(&rig, &uid)
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **The mail item's D4: an engine run woken once is woken again for new mail.** Mail waits
    /// and a wake is claimed; the nudge's turn runs and ends on the run's link, which ends the
    /// wake; new mail then gets a second claim. While the engine link told the wake rule
    /// nothing of what it filed, the first wake stayed outstanding for ever.
    #[tokio::test]
    async fn an_engine_run_woken_once_is_woken_again_for_new_mail() {
        let _env = crate::ProcessEnvRead::take();
        let rig = rig();
        let (uid, mut ws) = engine_run_woken(&rig, "req-twice", LIFECYCLE_THREAD, "turn-1").await;
        for frame in lifecycle_turn(protocol::ipc::MAIL_NUDGE_ACT) {
            tell(&mut ws, &frame).await;
        }
        let ended = format!(":turn:{LIFECYCLE_TURN}");
        let ids = filed_until(&rig.daemon, &uid, &ended).await;
        assert_eq!(count(&ids, &ended), 1, "{ids:?}");
        mail_kept_for(&rig, &uid, "m2");
        assert!(
            claimed_within(&rig, &uid).await,
            "no second wake for new mail: {:?}",
            wake_row(&rig, &uid)
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    // ------------------------------- a mail wake's turn, as the apps are sent it

    /// The run the mail-wake fixture is emitted for, written in place of the run's own uid.
    const MAIL_WAKE_UID: &str = "01K1B3XQ8ZC0DE5FGH7MAWAKE0";
    /// The one clock read in the file, replaced with a fixed stamp.
    const MAIL_WAKE_AT: &str = "2026-11-13T00:00:00.000Z";
    const MAIL_WAKE_TURN: &str = include_str!("../../../fixtures/codex/mail-wake-turn-wire.json");
    const REGENERATE_MAIL_WAKE_TURN: &str =
        "cargo test -p ccd --bin ccd -- --ignored regenerate_the_mail_wake_turn_fixture";
    /// The fixture's runs: each wake line the mail switch can pick, under its name.
    const MAIL_WAKE_RUNS: [(&str, &str); 2] = [
        ("act", protocol::ipc::MAIL_NUDGE_ACT),
        ("show", protocol::ipc::MAIL_NUDGE_SHOW),
    ];

    /// The events this build files for a run started on the engine whose link then carries a
    /// mail wake's turn with `line`, as the `event` frames a subscribed app is sent, in seq order:
    /// a stand-in app-server plays [`lifecycle_turn`] on the run's link once it has attached, and
    /// every event the run filed is kept, the link's `attached` first, the run's uid written
    /// [`MAIL_WAKE_UID`] and each clock read [`MAIL_WAKE_AT`].
    async fn mail_wake_turn(line: &str) -> serde_json::Value {
        let rig = rig();
        let (uid, mut ws) = engine_run(&rig, "req-wake", LIFECYCLE_THREAD, "turn-1").await;
        link_states_once(&rig.daemon, &uid, 1).await;
        for frame in lifecycle_turn(line) {
            tell(&mut ws, &frame).await;
        }
        filed_until(&rig.daemon, &uid, &format!(":turn:{LIFECYCLE_TURN}")).await;
        let frames: Vec<serde_json::Value> = rig
            .daemon
            .db
            .events_after(uid.clone(), 0, 10_000)
            .await
            .unwrap()
            .into_iter()
            .map(|event| {
                serde_json::to_value(protocol::ws::ServerMessage::Event { event }).unwrap()
            })
            .collect();
        drop(ws);
        end_run(&rig, &uid).await;
        let mut out: serde_json::Value = serde_json::from_str(
            &json!({ "frames": frames })
                .to_string()
                .replace(&uid, MAIL_WAKE_UID),
        )
        .unwrap();
        for frame in out["frames"].as_array_mut().unwrap() {
            let ts = &mut frame["event"]["ts"];
            assert!(
                ts.as_str()
                    .is_some_and(|read| read.len() == 24 && read.ends_with('Z')),
                "ts was expected to be an RFC3339 clock read, and is {ts}"
            );
            *ts = json!(MAIL_WAKE_AT);
        }
        out
    }

    /// Both runs of the mail-wake fixture.
    async fn mail_wake_turns() -> serde_json::Value {
        let mut runs = serde_json::Map::new();
        for (run, line) in MAIL_WAKE_RUNS {
            runs.insert(run.to_string(), mail_wake_turn(line).await);
        }
        serde_json::Value::Object(runs)
    }

    /// **A mail wake's turn on a run started on the engine reaches the apps as a `user_message`
    /// whose text is the wake line, and this file is what this build sends for it.**
    ///
    /// `fixtures/codex/mail-wake-turn-wire.json` is the apps' pin for telling the wake line from
    /// the person's own words: the run's link files the line as the turn's user message, which
    /// the mailbox's wake rule counts ([`protocol::ipc::is_mail_nudge`]). Gate one reads the
    /// committed bytes: in each run, the daemon's own numbering from 1 with no holes, one
    /// `user_message` carrying exactly that run's wake line, then the turn's end. Gate two
    /// re-derives the file from this build and requires the same bytes.
    ///
    /// **Mutation:** edit the wake line in the checked-in file and gate one fails; change either
    /// line in `protocol::ipc` and gate two fails, naming the command that rewrites the file.
    #[tokio::test]
    async fn the_mail_wake_turn_fixture_is_what_this_build_emits() {
        let committed: serde_json::Value =
            serde_json::from_str(MAIL_WAKE_TURN).expect("the fixture must be JSON");
        let runs = committed
            .as_object()
            .expect("the file is its runs, by name");
        assert_eq!(
            runs.keys().map(String::as_str).collect::<Vec<_>>(),
            MAIL_WAKE_RUNS.map(|(run, _)| run).to_vec(),
        );
        for (run, line) in MAIL_WAKE_RUNS {
            let events: Vec<protocol::event::Event> = committed[run]["frames"]
                .as_array()
                .expect("a run is a list of frames")
                .iter()
                .map(|frame| {
                    serde_json::from_value(frame["event"].clone())
                        .expect("an Event the apps decode")
                })
                .collect();
            assert_eq!(
                events.iter().map(|event| event.seq).collect::<Vec<_>>(),
                (1..=events.len() as u64).collect::<Vec<_>>(),
                "{run}: the daemon's own numbering, in order, with no holes"
            );
            let messages: Vec<_> = events
                .iter()
                .filter(|event| event.kind == EventKind::UserMessage)
                .collect();
            assert_eq!(messages.len(), 1, "{run}: the turn's one user message");
            assert_eq!(
                messages[0].payload,
                json!({ "interrupted": false, "text": line }),
                "{run}: the wake line, as a Codex user message carries text"
            );
            assert!(
                events
                    .iter()
                    .any(|event| event.kind == EventKind::TurnComplete
                        && event.seq > messages[0].seq),
                "{run}: the turn's end follows its wake line"
            );
        }

        let derived = mail_wake_turns().await;
        assert_eq!(
            derived, committed,
            "the committed mail-wake turn no longer matches what this build emits. \
             Regenerate it: {REGENERATE_MAIL_WAKE_TURN}"
        );
        assert_eq!(
            MAIL_WAKE_TURN,
            format!("{}\n", serde_json::to_string_pretty(&derived).unwrap()),
            "the committed bytes are not this build's rendering of them (a reformat, or an edit \
             by hand). Regenerate it: {REGENERATE_MAIL_WAKE_TURN}"
        );
    }

    /// Rewrite `fixtures/codex/mail-wake-turn-wire.json` with what this build sends.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_mail_wake_turn_fixture() {
        let wire = serde_json::to_string_pretty(&mail_wake_turns().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/codex/mail-wake-turn-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
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
        // The stand-in writes the pid once it has forked, which the start does not wait for.
        let pid_file = run_folder(&rig).await.join("tool.pid");
        let deadline = Instant::now() + Duration::from_secs(10);
        let tool: i32 = loop {
            match std::fs::read_to_string(&pid_file).map(|pid| pid.trim().parse()) {
                Ok(Ok(pid)) => break pid,
                _ => {
                    assert!(Instant::now() < deadline, "the stand-in wrote no tool.pid");
                    tokio::time::sleep(POLL).await;
                }
            }
        };
        assert!(
            group_forms(tool).await,
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

    // ------------------------------- a message's relaunch (X5-2)

    /// A daemon on the rig's store whose codex is the rig's stand-in and whose runs are in the
    /// rig's folder: one a message's relaunch runs on, as a ccd does after an app-server's end.
    fn relauncher(rig: &Rig) -> Arc<Daemon> {
        let codex = rig.root.join("stand-in-codex").display().to_string();
        let daemon = daemon_with(&rig.root, &codex);
        daemon
            .relaunch_from
            .set((rig.root.clone(), Ok(PathBuf::from(CODECONNECT))))
            .unwrap();
        daemon
    }

    /// The run `uid`'s app-server ended without ccd stopping it: its recorded group killed, the
    /// stand-in's end of its link closed, and the run `exited`. The leader that ended.
    async fn app_server_ended(rig: &Rig, uid: &str, ws: Socket) -> i32 {
        let leader = rig
            .daemon
            .db
            .watched_host(uid.to_string())
            .await
            .unwrap()
            .unwrap()
            .host
            .pid;
        // SAFETY: kill(2) of the group the run's stand-in leads.
        unsafe { libc::kill(-leader, libc::SIGKILL) };
        drop(ws);
        assert_eq!(
            row_once(rig, uid, Lifecycle::Exited).await.lifecycle,
            Lifecycle::Exited
        );
        leader
    }

    /// The live run `uid` ended "Stopped: its process ended" while its app-server's group still
    /// has a member, which only the warrant then stands against: its recorded app-server.
    async fn ended_alive(rig: &Rig, uid: &str) -> crate::store::WatchedHost {
        let watch = rig
            .daemon
            .db
            .watched_host(uid.to_string())
            .await
            .unwrap()
            .unwrap();
        let ended = rig
            .daemon
            .end_hosted_run(
                uid,
                watch.host,
                None,
                Lifecycle::Live,
                Lifecycle::Exited,
                json!({"exit_code": null, "reason": crate::claude_host::PROCESS_ENDED}),
            )
            .await
            .unwrap();
        assert!(ended);
        watch
    }

    /// The stand-in's side of the `n`th launch: once codex is launched that many times, it
    /// listens where the app-server would, takes ccd's connection, and answers `initialize`,
    /// `skills/list` and `thread/resume`, the last with `resume_answer`. The connection, and the
    /// `thread/resume` ccd sent.
    async fn relaunch_served(
        rig: &Rig,
        n: usize,
        resume_answer: serde_json::Value,
    ) -> (Socket, serde_json::Value) {
        relaunch_served_with(rig, n, |_| resume_answer).await
    }

    /// [`relaunch_served`], its `thread/resume` answered with what `answer_for` makes of it.
    async fn relaunch_served_with(
        rig: &Rig,
        n: usize,
        answer_for: impl FnOnce(&serde_json::Value) -> serde_json::Value,
    ) -> (Socket, serde_json::Value) {
        let dir = run_folder(rig).await;
        let deadline = Instant::now() + Duration::from_secs(20);
        while launches(rig) < n {
            assert!(Instant::now() < deadline, "codex was not launched again");
            tokio::time::sleep(POLL).await;
        }
        let listener = UnixListener::bind(dir.join("as.sock")).unwrap();
        let (stream, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
            .await
            .expect("ccd connected to the new launch")
            .unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let init = asked(&mut ws, "initialize").await;
        answer(&mut ws, &init, json!({"userAgent": "stand-in"})).await;
        let skills = asked(&mut ws, "skills/list").await;
        answer(&mut ws, &skills, json!({"data": []})).await;
        let resumed = asked(&mut ws, "thread/resume").await;
        let resume_answer = answer_for(&resumed);
        answer(&mut ws, &resumed, resume_answer).await;
        (ws, resumed)
    }

    /// A phone's message to the run `uid` on `daemon`, answered within 20 s.
    async fn said_now(
        daemon: &Arc<Daemon>,
        uid: &str,
        request_id: &str,
    ) -> protocol::ws::ComposeResult {
        let text = "are you still there";
        let hash = protocol::hash::compose_hash(uid, text);
        tokio::time::timeout(
            Duration::from_secs(20),
            daemon.compose(uid, request_id, text.into(), &hash),
        )
        .await
        .expect("the message was answered at once")
    }

    fn engine_no_connection() -> protocol::ws::ComposeResult {
        protocol::ws::ComposeResult::Rejected {
            reason: crate::codex_refusals::ENGINE_NO_CONNECTION.into(),
        }
    }

    /// The source ids of the run's filed ends, in order.
    async fn ends_of(daemon: &Daemon, uid: &str) -> Vec<String> {
        daemon
            .db
            .events_after(uid.to_string(), 0, 10_000)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == EventKind::SessionEnd)
            .filter_map(|event| event.source_event_id)
            .collect()
    }

    /// **X5-2 G1: a message relaunches an engine run whose app-server ended without ccd
    /// stopping it**, into the folder and the log the run kept. The run reads
    /// `bound_not_started`. The message launches codex again with codex's arguments and the
    /// run's own folder answer, its claim the new leader ccd records, resumes the run's thread
    /// with that answer on the new connection, and is sent there once: `Started`, the run `live`
    /// and `subscribed`, its start filed under the new launch. When the new launch's app-server
    /// ends in turn, its end is filed beside the first, under its own id.
    #[tokio::test]
    async fn a_message_relaunches_an_engine_run_whose_app_server_ended() {
        let _env = crate::ProcessEnvRead::take();
        use protocol::event::CodexLink;
        let rig = rig();
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let after = capture["answerAfter"]["result"].clone();
        let thread = after["thread"]["id"].as_str().unwrap().to_string();
        let first = after["thread"]["turns"][0]["id"].as_str().unwrap();
        let (uid, ws) = engine_run_as(
            &rig,
            "req-relaunch",
            &thread,
            first,
            "exec sleep 30",
            Trust::Trusted,
        )
        .await;
        let ended = app_server_ended(&rig, &uid, ws).await;
        let dir = crate::claude_host::run_dir(&rig.root, &uid);
        let earlier_log = std::fs::read(dir.join("host.log")).unwrap();
        let daemon = relauncher(&rig);
        assert_eq!(
            summary_of(&daemon, &uid).await.codex_link,
            CodexLink::BoundNotStarted
        );

        let mut said = saying(&daemon, &uid, "say-again", "are you still there");
        let (mut ws, resumed) = tokio::select! {
            served = relaunch_served(&rig, 2, after.clone()) => served,
            refused = &mut said => panic!("answered before codex was launched again: {refused:?}"),
        };
        let sent = asked(&mut ws, "turn/start").await;
        answer(&mut ws, &sent, json!({"turn": {"id": "turn-relaunched"}})).await;
        assert_eq!(
            said.await.unwrap(),
            protocol::ws::ComposeResult::Started {
                turn_id: "turn-relaunched".into()
            }
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(500), next_frame(&mut ws))
                .await
                .is_err(),
            "ccd sent the app-server more than the one message"
        );

        let real = std::fs::canonicalize(&rig.project)
            .unwrap()
            .display()
            .to_string();
        let argv: Vec<String> = std::fs::read(dir.join("argv"))
            .unwrap()
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect();
        assert_eq!(
            argv,
            [
                "-c".to_string(),
                format!(r#"projects={{ "{real}" = {{ trust_level = "trusted" }} }}"#),
                "app-server".into(),
                "--listen".into(),
                format!("unix://{}", dir.join("as.sock").display()),
            ]
        );
        assert_eq!(resumed["params"]["threadId"], thread);
        assert_eq!(
            resumed["params"]["config"]["projects"],
            json!({real: {"trust_level": "trusted"}})
        );
        assert_eq!(sent["params"]["threadId"], thread);
        assert_eq!(
            sent["params"]["input"],
            json!([{"type": "text", "text": "are you still there"}])
        );
        let relaunched = daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .host
            .pid;
        let claimed: i32 = std::fs::read_to_string(dir.join("as.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_ne!(relaunched, ended);
        assert_eq!(claimed, relaunched, "the new claim is the recorded leader");
        assert!(
            std::fs::read(dir.join("host.log"))
                .unwrap()
                .starts_with(&earlier_log),
            "the run's log was not kept"
        );
        assert_eq!(
            row_once(&rig, &uid, Lifecycle::Live).await.lifecycle,
            Lifecycle::Live
        );
        assert_eq!(
            summary_of(&daemon, &uid).await.codex_link,
            CodexLink::Subscribed
        );
        let start = format!("start:{uid}:{relaunched}");
        assert_eq!(count(&filed_until(&daemon, &uid, &start).await, &start), 1);

        let again = app_server_ended(&rig, &uid, ws).await;
        assert_eq!(
            ends_of(&daemon, &uid).await,
            [format!("exit:{uid}:{ended}"), format!("exit:{uid}:{again}")]
        );
        assert_eq!(
            summary_of(&daemon, &uid).await.codex_link,
            CodexLink::BoundNotStarted
        );
    }

    /// **X5-2 G2: a run ended by End run is not relaunched.** It reads `none`, and a message to
    /// it is refused with nothing launched.
    #[tokio::test]
    async fn a_run_ended_by_end_run_is_not_relaunched_by_a_message() {
        use protocol::event::CodexLink;
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-ended", "thr-ended", "turn-1").await;
        assert_eq!(
            rig.daemon.stop_run(&uid).await,
            protocol::ws::StopRunResult::Stopping
        );
        drop(ws);
        row_once(&rig, &uid, Lifecycle::Exited).await;
        let daemon = relauncher(&rig);
        assert_eq!(summary_of(&daemon, &uid).await.codex_link, CodexLink::None);
        assert_eq!(
            said_now(&daemon, &uid, "say-ended").await,
            engine_no_connection()
        );
        assert_eq!(launches(&rig), 1);
    }

    /// A message refused by the warrant on the run `uid`, whose recorded app-server `watch`
    /// still has a member: nothing launched, `as.pid` and `as.sock` left, and the run `exited`
    /// again with `watch`'s app-server recorded.
    async fn refused_by_the_warrant(rig: &Rig, uid: &str, watch: &crate::store::WatchedHost) {
        let daemon = relauncher(rig);
        assert_eq!(
            said_now(&daemon, uid, "say-warrant").await,
            engine_no_connection()
        );
        let dir = crate::claude_host::run_dir(&rig.root, uid);
        let claimed: i32 = std::fs::read_to_string(dir.join("as.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(launches(rig), 1);
        assert_eq!(claimed, watch.host.pid);
        assert!(dir.join("as.sock").exists());
        let back = daemon
            .db
            .watched_host(uid.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((back.lifecycle, back.host), (Lifecycle::Exited, watch.host));
    }

    /// **X5-2 G2: a run whose recorded app-server still runs is not relaunched.**
    #[tokio::test]
    async fn a_run_whose_app_server_still_runs_is_not_relaunched() {
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-alive", "thr-alive", "turn-1").await;
        let watch = ended_alive(&rig, &uid).await;
        refused_by_the_warrant(&rig, &uid, &watch).await;
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **X5-2 G2: a run whose app-server's leader is gone while its group lives on is not
    /// relaunched.**
    #[tokio::test]
    async fn a_run_whose_app_servers_group_outlived_its_leader_is_not_relaunched() {
        let rig = rig();
        let (uid, ws) =
            engine_run_on(&rig, "req-group", "thr-group", "turn-1", "sleep 30 &\nwait").await;
        let leader = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .host;
        // SAFETY: kill(2) of the stand-in's leader alone; its child stays in its group.
        unsafe { libc::kill(leader.pid, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while liveness(&leader) != Liveness::Gone {
            assert!(Instant::now() < deadline, "the leader did not end");
            tokio::time::sleep(POLL).await;
        }
        assert!(group_alive(leader.pid));
        let watch = ended_alive(&rig, &uid).await;
        refused_by_the_warrant(&rig, &uid, &watch).await;
        // SAFETY: kill(2) of the group the stand-in's child is left in.
        unsafe { libc::kill(-leader.pid, libc::SIGKILL) };
        drop(ws);
    }

    /// **X5-2 G2: a relaunch whose launch fails puts the run back**: the message is refused,
    /// nothing is claimed, and the run is `exited` and relaunchable again.
    #[tokio::test]
    async fn a_relaunch_that_fails_puts_the_run_back_with_nothing_claimed() {
        let _env = crate::ProcessEnvRead::take();
        use protocol::event::CodexLink;
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-fails", "thr-fails", "turn-1").await;
        app_server_ended(&rig, &uid, ws).await;
        stand_in(&rig, "echo codex did not start >&2\nexit 1");
        let daemon = relauncher(&rig);
        assert_eq!(
            said_now(&daemon, &uid, "say-fails").await,
            engine_no_connection()
        );
        assert_eq!(launches(&rig), 2);
        assert_eq!(said(&daemon, &uid, "say-fails").await, None);
        assert_eq!(
            row_once(&rig, &uid, Lifecycle::Exited).await.lifecycle,
            Lifecycle::Exited
        );
        assert_eq!(
            summary_of(&daemon, &uid).await.codex_link,
            CodexLink::BoundNotStarted
        );
    }

    /// **DC4: a launch into a folder whose `host.log` holds an earlier launch's output reads only
    /// its own.** A launch that ends without a word is told so, never the earlier launch's last
    /// line.
    #[tokio::test]
    async fn a_launch_into_a_kept_folder_reads_only_its_own_log() {
        use std::io::Write;
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-log", "thr-log", "turn-1").await;
        app_server_ended(&rig, &uid, ws).await;
        let dir = crate::claude_host::run_dir(&rig.root, &uid);
        std::fs::remove_file(dir.join("as.pid")).unwrap();
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("host.log"))
            .unwrap();
        writeln!(log, "an earlier launch's last words").unwrap();
        let codex = stand_in(&rig, "exit 1");
        let mut launched = None;
        let failed = bring_up(
            &rig.daemon,
            &uid,
            &run(&rig, "req-log", &codex),
            &[],
            log,
            Instant::now() + Duration::from_secs(20),
            &mut launched,
        )
        .await;
        let why = failed.err().expect("the launch ended at once");
        assert!(!why.contains("earlier launch"), "{why}");
        assert!(why.starts_with("the agent host ended"), "{why}");
    }

    /// **X5-2 G2, in the store: two messages meet one swap.** The first message's swap moves
    /// the run to `starting`; a second message is refused with nothing launched, and its own
    /// swap finds nothing to relaunch.
    #[tokio::test]
    async fn a_second_message_during_a_relaunch_is_refused() {
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-two", "thr-two", "turn-1").await;
        app_server_ended(&rig, &uid, ws).await;
        let daemon = relauncher(&rig);
        assert!(daemon
            .db
            .begin_relaunch(uid.clone())
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            said_now(&daemon, &uid, "say-second").await,
            engine_no_connection()
        );
        assert_eq!(daemon.db.begin_relaunch(uid.clone()).await.unwrap(), None);
        assert_eq!(launches(&rig), 1);
    }

    /// **X5-2 G2, in the store: a delete and a relaunch meet in one order or the other.** A run
    /// deleted first leaves nothing to relaunch; a run whose relaunch has begun is not deleted.
    #[tokio::test]
    async fn a_delete_before_or_after_the_swap_leaves_one_of_the_two() {
        use crate::store::DeleteOutcome;
        let deleted_first = rig();
        let (uid, ws) = engine_run(&deleted_first, "req-del-1", "thr-del-1", "turn-1").await;
        app_server_ended(&deleted_first, &uid, ws).await;
        let db = &deleted_first.daemon.db;
        assert!(matches!(
            db.delete_exited_session(uid.clone()).await.unwrap(),
            DeleteOutcome::Deleted { .. }
        ));
        assert_eq!(db.begin_relaunch(uid).await.unwrap(), None);

        let swapped_first = rig();
        let (uid, ws) = engine_run(&swapped_first, "req-del-2", "thr-del-2", "turn-1").await;
        app_server_ended(&swapped_first, &uid, ws).await;
        let db = &swapped_first.daemon.db;
        assert!(db.begin_relaunch(uid.clone()).await.unwrap().is_some());
        assert_eq!(
            db.delete_exited_session(uid).await.unwrap(),
            DeleteOutcome::NotExited {
                lifecycle: "starting".into()
            }
        );
    }

    /// **X5-2 G2, DC1: the watch between the swap and the new leader's record leaves the
    /// relaunch alone.** With the old app-server cleared at the swap, the watch finds no host to
    /// check, and the run stays `starting` with no end filed.
    #[tokio::test]
    async fn the_watch_between_the_swap_and_the_record_leaves_the_relaunch_alone() {
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-watch", "thr-watch", "turn-1").await;
        app_server_ended(&rig, &uid, ws).await;
        assert!(rig
            .daemon
            .db
            .begin_relaunch(uid.clone())
            .await
            .unwrap()
            .is_some());
        crate::claude_host::end_if_host_gone(&rig.daemon, &uid).await;
        let row = rig
            .daemon
            .db
            .get_session(uid.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.lifecycle, Lifecycle::Starting);
        assert_eq!(ends_of(&rig.daemon, &uid).await.len(), 1);
    }

    /// **X5-2 G2, a Mac restart: a run whose app-server was recorded in an earlier boot is
    /// relaunched, whatever now holds its old group's number.** A live process group of another
    /// process has that number; it is not consulted, and nothing of it is signalled.
    #[tokio::test]
    async fn a_run_recorded_in_an_earlier_boot_is_relaunched_whatever_holds_its_old_group_number() {
        let _env = crate::ProcessEnvRead::take();
        use std::os::unix::process::CommandExt;
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-boot", "thr-boot", "turn-1").await;
        app_server_ended(&rig, &uid, ws).await;
        let mut stranger = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = stranger.id() as i32;
        let now = protocol::proc_identity::boot_identity().unwrap();
        let earlier = BootIdentity {
            boot_sec: now.boot_sec - 3600,
            ..now
        };
        let recorded = ProcessIdentity {
            pid,
            birth: read_birth_identity(pid).unwrap(),
        };
        rig.daemon
            .db
            .record_host(uid.clone(), recorded, Some(earlier))
            .await
            .unwrap();
        assert!(group_alive(pid), "the old number leads a live group");

        let daemon = relauncher(&rig);
        let mut said = saying(&daemon, &uid, "say-boot", "are you still there");
        let resume = json!({"thread": {"id": "thr-boot", "turns": []}});
        let (mut ws, _) = tokio::select! {
            served = relaunch_served(&rig, 2, resume) => served,
            refused = &mut said => panic!("answered before codex was launched again: {refused:?}"),
        };
        let sent = asked(&mut ws, "turn/start").await;
        answer(&mut ws, &sent, json!({"turn": {"id": "turn-after-boot"}})).await;
        assert_eq!(
            said.await.unwrap(),
            protocol::ws::ComposeResult::Started {
                turn_id: "turn-after-boot".into()
            }
        );
        assert!(
            stranger.try_wait().unwrap().is_none(),
            "the process holding the old number was signalled"
        );
        stranger.kill().unwrap();
        stranger.wait().unwrap();
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// The run `uid` on `thread`, whose app-server ended, relaunched by a message whose turn is
    /// `turn`: the daemon it was relaunched on, the stand-in's end of the new link, and the
    /// `thread/resume` ccd sent it.
    async fn relaunched(
        rig: &Rig,
        uid: &str,
        thread: &str,
        turn: &str,
    ) -> (Arc<Daemon>, Socket, serde_json::Value) {
        let launch = launches(rig) + 1;
        let daemon = relauncher(rig);
        let mut said = saying(&daemon, uid, "say-relaunched", "are you still there");
        let resume = json!({"thread": {"id": thread, "turns": []}});
        let (mut ws, resumed) = tokio::select! {
            served = relaunch_served(rig, launch, resume) => served,
            refused = &mut said => panic!("answered before codex was launched again: {refused:?}"),
        };
        let sent = asked(&mut ws, "turn/start").await;
        answer(&mut ws, &sent, json!({"turn": {"id": turn}})).await;
        let said = said.await.unwrap();
        assert!(
            matches!(said, protocol::ws::ComposeResult::Started { .. }),
            "{said:?}"
        );
        (daemon, ws, resumed)
    }

    /// A process group whose leader is gone while one member, which notes each SIGTERM in
    /// `<dir>/term` and lives on, keeps the group: the leader, as a launch's recorded host. The
    /// leader is ended only once the member says it is up, in `<dir>/up`.
    fn group_without_its_leader(dir: &Path) -> ProcessIdentity {
        use std::os::unix::process::CommandExt;
        let script = format!(
            "(trap 'echo term >> {dir}/term' TERM; echo up > {dir}/up; while :; do sleep 0.2; done) & wait",
            dir = dir.display()
        );
        let mut leader = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = leader.id() as i32;
        let host = ProcessIdentity {
            pid,
            birth: read_birth_identity(pid).unwrap(),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !dir.join("up").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the member did not start"
            );
            std::thread::sleep(POLL);
        }
        leader.kill().unwrap();
        leader.wait().unwrap();
        host
    }

    /// A process that ran and has been reaped, its group with it: a launch's recorded host that
    /// died at once.
    fn host_gone_with_its_group() -> ProcessIdentity {
        use std::os::unix::process::CommandExt;
        let mut leader = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = leader.id() as i32;
        let host = ProcessIdentity {
            pid,
            birth: read_birth_identity(pid).unwrap(),
        };
        leader.kill().unwrap();
        leader.wait().unwrap();
        host
    }

    /// **A relaunch whose new app-server dies leaves the run `exited` and relaunchable,
    /// whichever of the watch and the relaunch's put-back runs first.** The watch
    /// that read the run `starting` before the put-back changes nothing after it; a watch that
    /// failed the run first for the relaunch's own app-server is undone by the put-back.
    #[tokio::test]
    async fn a_dead_relaunch_ends_exited_whichever_of_the_watch_and_its_undo_runs_first() {
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-r1", "thr-r1", "turn-1").await;
        app_server_ended(&rig, &uid, ws).await;
        let db = &rig.daemon.db;
        let boot = protocol::proc_identity::boot_identity();
        let lifecycle = || async {
            db.get_session(uid.clone())
                .await
                .unwrap()
                .unwrap()
                .lifecycle
        };

        // The watch reads the run while it is starting, then the relaunch puts it back.
        let old = db.begin_relaunch(uid.clone()).await.unwrap().unwrap().host;
        let noted = rig.root.join("term");
        let dying = group_without_its_leader(&rig.root);
        db.record_host(uid.clone(), dying, boot).await.unwrap();
        let watch = {
            let (daemon, uid) = (Arc::clone(&rig.daemon), uid.clone());
            tokio::spawn(async move { crate::claude_host::end_if_host_gone(&daemon, &uid).await })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !noted.exists() {
            assert!(
                Instant::now() < deadline,
                "the watch did not reach the ladder"
            );
            tokio::time::sleep(POLL).await;
        }
        assert!(db
            .undo_relaunch(uid.clone(), Some(old), Some(dying))
            .await
            .unwrap());
        watch.await.unwrap();
        assert_eq!(
            lifecycle().await,
            Lifecycle::Exited,
            "after the watch ended"
        );

        // The watch fails the run first, for the relaunch's own app-server, then the put-back.
        let old = db.begin_relaunch(uid.clone()).await.unwrap().unwrap().host;
        let dead = host_gone_with_its_group();
        db.record_host(uid.clone(), dead, boot).await.unwrap();
        crate::claude_host::end_if_host_gone(&rig.daemon, &uid).await;
        assert_eq!(
            lifecycle().await,
            Lifecycle::Failed,
            "the watch's own write"
        );
        assert!(db
            .undo_relaunch(uid.clone(), Some(old), Some(dead))
            .await
            .unwrap());
        assert_eq!(lifecycle().await, Lifecycle::Exited);
        assert!(db.relaunchable(uid.clone()).await.unwrap());
    }

    /// **After a relaunch, an end found for the old app-server writes nothing.** A watch
    /// that inspected the dead leader and commits only now, with the run
    /// relaunched and live on another, is rejected: the run stays `live`, its new link attached,
    /// and no end is filed.
    #[tokio::test]
    async fn a_stale_end_for_the_old_app_server_leaves_a_relaunched_run_live() {
        let _env = crate::ProcessEnvRead::take();
        use protocol::event::CodexLink;
        let rig = rig();
        let (uid, ws) = engine_run(&rig, "req-r2", "thr-r2", "turn-1").await;
        let old = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap()
            .host;
        app_server_ended(&rig, &uid, ws).await;
        let (daemon, ws, _) = relaunched(&rig, &uid, "thr-r2", "turn-2").await;
        let stale = daemon
            .end_hosted_run(
                &uid,
                old,
                None,
                Lifecycle::Live,
                Lifecycle::Exited,
                json!({"exit_code": null, "reason": crate::claude_host::PROCESS_ENDED}),
            )
            .await
            .unwrap();
        assert!(!stale, "the stale end was taken");
        assert_eq!(
            row_once(&rig, &uid, Lifecycle::Live).await.lifecycle,
            Lifecycle::Live
        );
        assert_eq!(ends_of(&daemon, &uid).await.len(), 1);
        assert_eq!(
            summary_of(&daemon, &uid).await.codex_link,
            CodexLink::Subscribed
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **A relaunched run's first approval is a card of its own.** The first
    /// launch's app-server asked under JSON-RPC id 0 and was answered; the relaunched one, which
    /// numbers its requests from 0 again, asks under id 0 too. Its card is a new one, filed for
    /// the apps, and an answer to it reaches Codex.
    #[tokio::test]
    async fn a_relaunched_runs_first_approval_is_a_new_card_an_answer_reaches() {
        let _env = crate::ProcessEnvRead::take();
        let rig = rig();
        let frames = frames_of(LIVE_FRAMES);
        let thread = frames[1]["params"]["thread"]["id"].as_str().unwrap();
        let turn = frames[29]["params"]["turn"]["id"].as_str().unwrap();
        let (uid, mut ws) = engine_run(&rig, "req-k1", thread, turn).await;
        for frame in &frames[1..=17] {
            tell(&mut ws, frame).await;
        }
        let first = cards_of(&rig.daemon, &uid, 1).await.remove(0);
        let id = &frames[17]["id"];
        let (result, _) = tokio::join!(answered(&rig.daemon, &uid, &first, "cancel"), async {
            response_to(&mut ws, id, Duration::from_secs(20)).await;
            tell(&mut ws, &frames[18]).await;
        });
        assert!(
            matches!(result, protocol::ws::AnswerResult::Applied { .. }),
            "{result:?}"
        );
        app_server_ended(&rig, &uid, ws).await;

        let again = "turn-after-relaunch";
        let (daemon, mut ws, _) = relaunched(&rig, &uid, thread, again).await;
        let mut started = frames[16].clone();
        let mut asked = frames[17].clone();
        let item = format!("{}-again", asked["params"]["itemId"].as_str().unwrap());
        started["params"]["turnId"] = json!(again);
        started["params"]["item"]["id"] = json!(item);
        asked["params"]["turnId"] = json!(again);
        asked["params"]["itemId"] = json!(item);
        assert_eq!(
            &asked["id"], id,
            "the relaunched app-server asks under the same id"
        );
        tell(&mut ws, &turn_started(thread, again)).await;
        tell(&mut ws, &started).await;
        tell(&mut ws, &asked).await;
        let card = cards_of(&daemon, &uid, 1).await.remove(0);
        assert_ne!(
            card.request_id, first.request_id,
            "the card took the first launch's id"
        );
        let perm = format!("perm:{}", card.request_id);
        assert_eq!(count(&filed_until(&daemon, &uid, &perm).await, &perm), 1);
        let (result, response) = tokio::join!(answered(&daemon, &uid, &card, "cancel"), async {
            let response = response_to(&mut ws, id, Duration::from_secs(20)).await;
            tell(&mut ws, &resolved(&json!(thread), id)).await;
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
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **A row this build cannot read is refused plainly, not replayed as a steer.**
    ///
    /// `parse_compose_outcome` split on the first space and returned whatever it found, and
    /// `replayed_compose_report` then asked `route == COMPOSE_ROUTE_START` — so ANY other
    /// word came back `Duplicate{started:false}`, which the phone renders as "your words
    /// joined a running turn". About words this daemon cannot account for. Reachable from
    /// a corrupt row, or from a third route a future build writes and this one does not
    /// know.
    ///
    /// **Mutation:** drop the `matches!` on the route and the first two rows go green as
    /// duplicates.
    #[test]
    fn an_unreadable_compose_outcome_is_refused_rather_than_replayed_as_a_steer() {
        for corrupt in [
            "turn_bogus 01a0-a-real-looking-turn",
            "delivered 01a0-a-real-looking-turn",
            " 01a0-a-real-looking-turn",
            "turn_start ",
            "turn_start",
        ] {
            assert_eq!(
                crate::store::parse_compose_outcome(corrupt),
                None,
                "{corrupt:?} names no route this build wrote"
            );
            let report = replayed_compose_report(corrupt);
            assert!(
                matches!(report, ComposeReport::NotApplied(_)),
                "{corrupt:?} must replay as a refusal, not as a turn it joined: {report:?}"
            );
            // And the sentence carries none of the row — a corrupt outcome can hold a turn
            // id the phone never sent.
            let ComposeReport::NotApplied(reason) = report else {
                unreachable!()
            };
            assert!(
                !reason.contains("01a0-a-real-looking-turn"),
                "the stored word reached the phone: {reason}"
            );
        }
        // The two real routes still read.
        for (route, started) in [
            (crate::store::COMPOSE_ROUTE_START, true),
            (crate::store::COMPOSE_ROUTE_STEER, false),
        ] {
            assert_eq!(
                replayed_compose_report(&crate::store::compose_outcome(route, "01a0-t")),
                ComposeReport::Duplicate {
                    turn_id: "01a0-t".into(),
                    started,
                }
            );
        }
    }

    // ------------------------------- X6: a Codex run's settings from an app (minor 43)

    /// **What a settings change sends Codex** ([`update_params`]), against the thread's settings
    /// as Codex last named them: a model or an effort as it is; a mode as its preset with the
    /// collaboration mode carrying the thread's model and effort, Plan keeping the run's own
    /// approval and sandbox; a model changed in Plan carrying Plan again with it; and nothing
    /// for a mode before any report has named the model.
    #[test]
    fn a_settings_change_is_one_update_carrying_the_threads_model_and_effort() {
        use crate::claude_host::Setting;
        let report = |mode: &str| {
            json!({"model": "gpt-6-astra", "effort": "high", "approval_policy": "on-request",
                   "sandbox": "workspaceWrite", "collaboration_mode": mode})
        };
        let collaboration = |mode: &str, model: &str| {
            json!({"mode": mode, "settings": {
                "model": model, "reasoning_effort": "high", "developer_instructions": null}})
        };
        let mode = |mode: &str| Setting::Mode {
            mode: mode.into(),
            confirmed: true,
        };
        assert_eq!(
            update_params(&mode("plan"), Some(&report("default"))),
            Some(json!({"collaborationMode": collaboration("plan", "gpt-6-astra")}))
        );
        for (value, approval, sandbox) in [
            ("auto", "on-request", "workspaceWrite"),
            ("read-only", "on-request", "readOnly"),
            ("full-access", "never", "dangerFullAccess"),
        ] {
            assert_eq!(
                update_params(&mode(value), Some(&report("plan"))),
                Some(
                    json!({"collaborationMode": collaboration("default", "gpt-6-astra"),
                            "approvalPolicy": approval, "sandboxPolicy": {"type": sandbox}})
                ),
                "{value}"
            );
        }
        assert_eq!(
            update_params(&mode("plan"), None),
            None,
            "nothing names the model"
        );
        let to_sol = Setting::Model {
            model: Some("gpt-6-sol".into()),
            effort: None,
        };
        assert_eq!(
            update_params(&to_sol, None),
            Some(json!({"model": "gpt-6-sol"}))
        );
        assert_eq!(
            update_params(&to_sol, Some(&report("plan"))),
            Some(json!({"model": "gpt-6-sol",
                        "collaborationMode": collaboration("plan", "gpt-6-sol")}))
        );
    }

    /// Codex's recorded models (codex 0.167.0 signed in with an API key), as a start's check
    /// keeps them: the entries its `model/list` gives, hidden ones left out.
    fn recorded_models() -> Vec<serde_json::Value> {
        include_str!("../../../fixtures/codex/app-server-models-api-key-0.167.0.jsonl")
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find_map(|line| line["result"]["data"].as_array().cloned())
            .expect("the recorded model list")
            .into_iter()
            .filter(|model| model["hidden"] != true)
            .collect()
    }

    /// A run started on a stand-in with the models its start's check listed (`models`), its
    /// `thread/start` answered as Codex 0.167 answers one, naming the thread `thr-x6`'s settings
    /// ([`START_SETTINGS`]) with `model` as its model, and its first turn answered: its uid and
    /// the stand-in's end of its link.
    async fn engine_run_offering(
        rig: &Rig,
        models: &[serde_json::Value],
        model: &str,
    ) -> (String, Socket) {
        let codex = stand_in(rig, "exec sleep 30");
        let mut script = whole_start();
        script.truncate(3);
        let mut started = START_SETTINGS.clone();
        started["model"] = model.into();
        started["thread"] = json!({"id": "thr-x6"});
        script[2].1 = json!({ "result": started });
        let run = Run {
            models,
            ..run(rig, "req-x6", &codex)
        };
        let (result, ws) = tokio::join!(start(&rig.daemon, run), async {
            let (_, mut ws) = play_app_server(rig, script).await;
            let first_turn = asked(&mut ws, "turn/start").await;
            answer(&mut ws, &first_turn, json!({"turn": {"id": "turn-1"}})).await;
            ws
        });
        let uid = started_uid(&result);
        row_once(rig, &uid, Lifecycle::Live).await;
        link_attached(&rig.daemon, &uid).await;
        (uid, ws)
    }

    /// The settings a `thread/start` answer names (X6 P12a, codex 0.167) but its model: an
    /// effort left to the model's own (`null`), the preset Default ccd starts a thread with, and
    /// no collaboration mode.
    static START_SETTINGS: std::sync::LazyLock<serde_json::Value> =
        std::sync::LazyLock::new(|| {
            json!({"modelProvider": "openai", "reasoningEffort": null,
               "approvalPolicy": "on-request", "approvalsReviewer": "user",
               "sandbox": {"type": "workspaceWrite", "writableRoots": [], "networkAccess": false}})
        });

    /// Codex's own report of the thread `thr-x6`'s settings, in first-turn.jsonl:2's shape.
    fn settings_report(
        model: &str,
        effort: &str,
        approval: &str,
        sandbox: &str,
        mode: &str,
    ) -> serde_json::Value {
        json!({"method": "thread/settings/updated", "params": {"threadId": "thr-x6",
            "threadSettings": {"model": model, "effort": effort, "approvalPolicy": approval,
                "sandboxPolicy": {"type": sandbox},
                "collaborationMode": {"mode": mode, "settings": {"model": model,
                    "reasoning_effort": effort, "developer_instructions": null}}}}})
    }

    /// **A Codex run's settings are what its start's and its resume's answers name, and a
    /// restart keeps them** (X6 P12): Codex reports none at a start, a resume or a plain turn.
    /// A run set to Full Access, its ccd restarted, is resumed on the same app-server with the
    /// Full Access it was set to (ruling b), and reads what the resume's answer names, here a
    /// recorded one naming Default, not what ccd asked or read back.
    #[tokio::test]
    async fn a_codex_runs_settings_are_its_start_and_resume_answers() {
        let rig = rig();
        let (uid, mut ws) = engine_run_offering(&rig, &recorded_models(), "gpt-6.1-sol").await;
        let started = settings_as_sent(&rig.daemon, &uid, "auto").await;
        assert_eq!(
            (&started["model"]["value"], started.get("effort")),
            (&json!("gpt-6.1-sol"), None),
            "the start's answer names the model and leaves the effort to it"
        );
        tell(
            &mut ws,
            &settings_report(
                "gpt-6.1-sol",
                "high",
                "never",
                "dangerFullAccess",
                "default",
            ),
        )
        .await;
        settings_as_sent(&rig.daemon, &uid, "full-access").await;
        // A resume answer as Codex 0.164 gave one after a finished turn: gpt-6.1-sol, effort
        // high, on-request, workspace-write, Default; its thread is the run's.
        let recorded: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/codex/resume-after-approved-tool-turn-0.164.0.json"
        ))
        .unwrap();
        let mut resume = recorded["result"].clone();
        resume["thread"]["id"] = json!("thr-x6");
        resume["thread"]["sessionId"] = json!("thr-x6");
        let (restarted, _ws, params) = resumed_with(&rig, ws, resume).await;
        assert_eq!(
            (&params["approvalPolicy"], &params["sandbox"]),
            (&json!("never"), &json!("danger-full-access")),
            "the same app-server keeps what the run was set to"
        );
        let resumed = settings_as_sent(&restarted, &uid, "auto").await;
        assert_eq!(resumed["effort"]["value"], "high");
        end_run(&rig, &uid).await;
    }

    /// **A relaunched Codex run in Full Access comes back at Default** (decision 233): a run
    /// set to Full Access whose app-server ended is resumed with the preset Default, as its
    /// start was.
    #[tokio::test]
    async fn a_relaunched_codex_run_resumes_at_default() {
        let _env = crate::ProcessEnvRead::take();
        let rig = rig();
        let (uid, mut ws) = engine_run_offering(&rig, &recorded_models(), "gpt-6.1-sol").await;
        tell(
            &mut ws,
            &settings_report(
                "gpt-6.1-sol",
                "high",
                "never",
                "dangerFullAccess",
                "default",
            ),
        )
        .await;
        settings_as_sent(&rig.daemon, &uid, "full-access").await;
        app_server_ended(&rig, &uid, ws).await;
        let (_daemon, ws, resumed) = relaunched(&rig, &uid, "thr-x6", "turn-2").await;
        assert_eq!(
            (
                &resumed["params"]["approvalPolicy"],
                &resumed["params"]["sandbox"]
            ),
            (&json!("on-request"), &json!("workspace-write"))
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// A `thread/resume` answer for `thr-x6` as Codex gives one: a recorded answer (Codex
    /// 0.164, gpt-6.1-sol, Default's collaboration mode, which a resume does not carry) on the
    /// run's thread, naming the approval policy and sandbox the resume `asked` for, which Codex
    /// takes as the thread's own (X6 P12c).
    fn resume_answer_as_asked(asked: &serde_json::Value) -> serde_json::Value {
        let sandbox = match asked["params"]["sandbox"].as_str() {
            Some("read-only") => "readOnly",
            Some("danger-full-access") => "dangerFullAccess",
            _ => "workspaceWrite",
        };
        let recorded: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/codex/resume-after-approved-tool-turn-0.164.0.json"
        ))
        .unwrap();
        let mut resume = recorded["result"].clone();
        resume["thread"]["id"] = json!("thr-x6");
        resume["thread"]["sessionId"] = json!("thr-x6");
        resume["approvalPolicy"] = asked["params"]["approvalPolicy"].clone();
        resume["sandbox"]["type"] = json!(sandbox);
        resume
    }

    /// The engine run `uid` on `thr-x6`, which Codex reported at `approval`, `sandbox` and
    /// collaboration `mode` (the apps reading `wire`), its app-server ended, then relaunched by
    /// a message, the stand-in answering its resume as Codex does ([`resume_answer_as_asked`]),
    /// a `thread/settings/update` with `update` (its error when `None`) and a `turn/start` with
    /// `turn-2`. The daemon it was relaunched on, the stand-in's end, the `thread/resume` ccd
    /// sent, every call it made after it in order, and what the message was told.
    async fn relaunched_from(
        rig: &Rig,
        uid: &str,
        mut ws: Socket,
        (approval, sandbox, mode, wire): (&str, &str, &str, &str),
        update: Option<serde_json::Value>,
    ) -> (
        Arc<Daemon>,
        Socket,
        serde_json::Value,
        Vec<serde_json::Value>,
        protocol::ws::ComposeResult,
    ) {
        tell(
            &mut ws,
            &settings_report("gpt-6.1-sol", "high", approval, sandbox, mode),
        )
        .await;
        settings_as_sent(&rig.daemon, uid, wire).await;
        app_server_ended(rig, uid, ws).await;
        let launch = launches(rig) + 1;
        let daemon = relauncher(rig);
        let mut said = saying(&daemon, uid, "say-kept", "are you still there");
        let (mut ws, resumed) = tokio::select! {
            served = relaunch_served_with(rig, launch, resume_answer_as_asked) => served,
            refused = &mut said => panic!("answered before codex was launched again: {refused:?}"),
        };
        let mut calls = Vec::new();
        let told = loop {
            tokio::select! {
                frame = next_frame(&mut ws) => {
                    let Some(frame) = frame else { break (&mut said).await.unwrap() };
                    if frame.get("id").is_none() || frame.get("method").is_none() {
                        continue;
                    }
                    calls.push(frame.clone());
                    match (frame["method"].as_str(), &update) {
                        (Some("thread/settings/update"), Some(result)) => {
                            answer(&mut ws, &frame, result.clone()).await
                        }
                        (Some("thread/settings/update"), None) => {
                            let refusal = json!({"id": frame["id"],
                                "error": {"code": -32600, "message": "not now"}});
                            ws.send(Message::Text(refusal.to_string())).await.unwrap();
                        }
                        (Some("turn/start"), _) => {
                            answer(&mut ws, &frame, json!({"turn": {"id": "turn-2"}})).await
                        }
                        _ => {}
                    }
                }
                told = &mut said => break told.unwrap(),
            }
        };
        (daemon, ws, resumed, calls, told)
    }

    /// **A relaunched run in Read Only comes back in Read Only** (decision 253): its message's
    /// turn starts on a thread resumed with Read Only's approval and sandbox, and the apps read
    /// Read Only from Codex's answer, not Default.
    #[tokio::test]
    async fn a_relaunched_read_only_run_comes_back_read_only() {
        let _env = crate::ProcessEnvRead::take();
        let rig = rig();
        let (uid, ws) = engine_run_offering(&rig, &recorded_models(), "gpt-6.1-sol").await;
        let kept = ("on-request", "readOnly", "default", "read-only");
        let (daemon, ws, resumed, calls, told) =
            relaunched_from(&rig, &uid, ws, kept, Some(json!({}))).await;
        assert!(
            matches!(told, protocol::ws::ComposeResult::Started { .. }),
            "{told:?}"
        );
        assert_eq!(
            (
                &resumed["params"]["approvalPolicy"],
                &resumed["params"]["sandbox"]
            ),
            (&json!("on-request"), &json!("read-only"))
        );
        assert_eq!(methods(&calls), ["turn/start"]);
        settings_as_sent(&daemon, &uid, "read-only").await;
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **A relaunched run in Plan comes back in Plan before its message is sent** (decision
    /// 253): a resume cannot carry Plan, so Plan is set on the resumed thread, with the run's
    /// own approval and sandbox (decision 174), and only then is the message's turn started.
    #[tokio::test]
    async fn a_relaunched_plan_run_comes_back_in_plan_before_its_message() {
        let _env = crate::ProcessEnvRead::take();
        for (sandbox, resumed_in) in [
            ("workspaceWrite", "workspace-write"),
            ("readOnly", "read-only"),
        ] {
            let rig = rig();
            let (uid, ws) = engine_run_offering(&rig, &recorded_models(), "gpt-6.1-sol").await;
            let kept = ("on-request", sandbox, "plan", "plan");
            let (daemon, mut ws, resumed, calls, told) =
                relaunched_from(&rig, &uid, ws, kept, Some(json!({}))).await;
            assert!(
                matches!(told, protocol::ws::ComposeResult::Started { .. }),
                "{told:?}"
            );
            assert_eq!(
                (
                    &resumed["params"]["approvalPolicy"],
                    &resumed["params"]["sandbox"]
                ),
                (&json!("on-request"), &json!(resumed_in))
            );
            assert_eq!(methods(&calls), ["thread/settings/update", "turn/start"]);
            assert_eq!(
                calls[0]["params"],
                json!({"threadId": "thr-x6", "collaborationMode": {"mode": "plan",
                    "settings": {"model": "gpt-6.1-sol", "reasoning_effort": "high",
                        "developer_instructions": null}}})
            );
            tell(
                &mut ws,
                &settings_report("gpt-6.1-sol", "high", "on-request", sandbox, "plan"),
            )
            .await;
            settings_as_sent(&daemon, &uid, "plan").await;
            drop(ws);
            end_run(&rig, &uid).await;
        }
    }

    /// **A relaunched run in Default, Full Access, or Full Access in Plan comes back in
    /// Default** (decisions 233 and 253): Default's approval and sandbox, nothing set before
    /// its message.
    #[tokio::test]
    async fn a_relaunched_run_not_narrower_than_default_comes_back_in_default() {
        let _env = crate::ProcessEnvRead::take();
        for kept in [
            ("on-request", "workspaceWrite", "default", "auto"),
            ("never", "dangerFullAccess", "default", "full-access"),
            ("never", "dangerFullAccess", "plan", "full-access"),
        ] {
            let rig = rig();
            let (uid, ws) = engine_run_offering(&rig, &recorded_models(), "gpt-6.1-sol").await;
            let (daemon, ws, resumed, calls, told) =
                relaunched_from(&rig, &uid, ws, kept, Some(json!({}))).await;
            assert!(
                matches!(told, protocol::ws::ComposeResult::Started { .. }),
                "{kept:?}: {told:?}"
            );
            assert_eq!(
                (
                    &resumed["params"]["approvalPolicy"],
                    &resumed["params"]["sandbox"]
                ),
                (&json!("on-request"), &json!("workspace-write")),
                "{kept:?}"
            );
            assert_eq!(methods(&calls), ["turn/start"], "{kept:?}");
            settings_as_sent(&daemon, &uid, "auto").await;
            drop(ws);
            end_run(&rig, &uid).await;
        }
    }

    /// **A relaunch whose Plan Codex does not take sends nothing** (decision 253): the message
    /// is not sent and the person is told, the run back `exited`, rather than a turn at Default.
    #[tokio::test]
    async fn a_relaunch_whose_plan_is_refused_sends_nothing() {
        let _env = crate::ProcessEnvRead::take();
        let rig = rig();
        let (uid, ws) = engine_run_offering(&rig, &recorded_models(), "gpt-6.1-sol").await;
        let kept = ("on-request", "workspaceWrite", "plan", "plan");
        let (daemon, ws, _resumed, calls, told) = relaunched_from(&rig, &uid, ws, kept, None).await;
        assert_eq!(told, engine_no_connection());
        assert_eq!(methods(&calls), ["thread/settings/update"]);
        assert_eq!(
            row_once(&rig, &uid, Lifecycle::Exited).await.lifecycle,
            Lifecycle::Exited
        );
        drop(ws);
        // The person tries again: Plan is still the run's kept mode, and is set again before
        // the message's turn.
        let launch = launches(&rig) + 1;
        let mut again = saying(&daemon, &uid, "say-again", "are you still there");
        let (mut ws, _) = tokio::select! {
            served = relaunch_served_with(&rig, launch, resume_answer_as_asked) => served,
            refused = &mut again => panic!("answered before codex was launched again: {refused:?}"),
        };
        let update = asked(&mut ws, "thread/settings/update").await;
        assert_eq!(update["params"]["collaborationMode"]["mode"], "plan");
        answer(&mut ws, &update, json!({})).await;
        let start = asked(&mut ws, "turn/start").await;
        answer(&mut ws, &start, json!({"turn": {"id": "turn-3"}})).await;
        let told = again.await.unwrap();
        assert!(
            matches!(told, protocol::ws::ComposeResult::Started { .. }),
            "{told:?}"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **No resume leaves a run in Plan out of it** (decision 253): a run in Plan whose ccd
    /// restarts mid-turn is put back in Plan after the reconnect's resume, whose answer names
    /// Default, and again after the follow-up resume once that turn ends, also when the turn's
    /// end reaches ccd ahead of Codex's report of Plan.
    #[tokio::test]
    async fn every_resume_puts_a_plan_run_back_in_plan() {
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let mid_turn = &capture["answerMidTurn"]["result"];
        let thread = mid_turn["thread"]["id"].as_str().unwrap();
        let first_turn = capture["answerAfter"]["result"]["thread"]["turns"][0]["id"]
            .as_str()
            .unwrap();
        let plan = || {
            let mut report =
                settings_report("gpt-6.1-sol", "high", "on-request", "readOnly", "plan");
            report["params"]["threadId"] = json!(thread);
            report
        };
        let wants_plan = |update: &serde_json::Value| {
            assert_eq!(update["params"]["threadId"], thread);
            assert_eq!(update["params"]["collaborationMode"]["mode"], "plan");
        };
        let rig = rig();
        let (uid, mut ws) = engine_run(&rig, "req-plan-kept", thread, first_turn).await;
        tell(&mut ws, &plan()).await;
        settings_as_sent(&rig.daemon, &uid, "plan").await;
        let (_restarted, mut ws, _) = resumed_with(&rig, ws, mid_turn.clone()).await;
        let update = asked(&mut ws, "thread/settings/update").await;
        wants_plan(&update);
        answer(&mut ws, &update, json!({})).await;
        for frame in capture["live"]["afterTheMidTurnAnswer"].as_array().unwrap() {
            tell(&mut ws, frame).await;
        }
        tell(&mut ws, &plan()).await;
        let follow_up = asked(&mut ws, "thread/resume").await;
        answer(
            &mut ws,
            &follow_up,
            capture["answerAfter"]["result"].clone(),
        )
        .await;
        let again = asked(&mut ws, "thread/settings/update").await;
        wants_plan(&again);
        answer(&mut ws, &again, json!({})).await;
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **A Plan run whose follow-up resume could not set Plan again is put back in Plan by the
    /// next ccd start** (decision 253): the link ends, a message is refused meanwhile, and the
    /// reconnect sets Plan again, since the follow-up's answer, which named Default, was not
    /// filed as the run's settings.
    #[tokio::test]
    async fn a_plan_the_follow_up_could_not_set_is_set_by_the_next_start() {
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let mid_turn = &capture["answerMidTurn"]["result"];
        let thread = mid_turn["thread"]["id"].as_str().unwrap();
        let first_turn = capture["answerAfter"]["result"]["thread"]["turns"][0]["id"]
            .as_str()
            .unwrap();
        let plan = || {
            let mut report =
                settings_report("gpt-6.1-sol", "high", "on-request", "readOnly", "plan");
            report["params"]["threadId"] = json!(thread);
            report
        };
        let rig = rig();
        let (uid, mut ws) = engine_run(&rig, "req-plan-refused", thread, first_turn).await;
        tell(&mut ws, &plan()).await;
        settings_as_sent(&rig.daemon, &uid, "plan").await;
        let (restarted, mut ws, _) = resumed_with(&rig, ws, mid_turn.clone()).await;
        let update = asked(&mut ws, "thread/settings/update").await;
        answer(&mut ws, &update, json!({})).await;
        tell(&mut ws, &plan()).await;
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
        let refused = asked(&mut ws, "thread/settings/update").await;
        let refusal = json!({"id": refused["id"], "error": {"code": -32600, "message": "not now"}});
        ws.send(Message::Text(refusal.to_string())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while next_frame(&mut ws).await.is_some() {}
        })
        .await
        .expect("ccd did not end its link");
        assert_eq!(
            said_now(&restarted, &uid, "say-meanwhile").await,
            engine_no_connection()
        );
        let (_again, mut ws, _) =
            resumed_with(&rig, ws, capture["answerAfter"]["result"].clone()).await;
        let update = asked(&mut ws, "thread/settings/update").await;
        assert_eq!(update["params"]["collaborationMode"]["mode"], "plan");
        answer(&mut ws, &update, json!({})).await;
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **A mode an app sets while a reconnected turn runs is what the follow-up resume asks
    /// for** (decision 253): a Default run whose ccd restarted mid-turn is set to Read Only
    /// during that turn and Codex takes it; its follow-up resume, once the turn ends, asks for
    /// Read Only before Codex has reported the change.
    #[tokio::test]
    async fn a_mode_set_during_a_reconnected_turn_is_what_its_follow_up_asks_for() {
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let mut mid_turn = capture["answerMidTurn"]["result"].clone();
        mid_turn["sandbox"]["type"] = json!("workspaceWrite");
        let thread = mid_turn["thread"]["id"].as_str().unwrap().to_string();
        let first_turn = capture["answerAfter"]["result"]["thread"]["turns"][0]["id"]
            .as_str()
            .unwrap();
        let mut auto = settings_report(
            "gpt-6.1-sol",
            "high",
            "on-request",
            "workspaceWrite",
            "default",
        );
        auto["params"]["threadId"] = json!(thread);
        let rig = rig();
        let (uid, mut ws) = engine_run(&rig, "req-ro-mid", &thread, first_turn).await;
        tell(&mut ws, &auto).await;
        settings_as_sent(&rig.daemon, &uid, "auto").await;
        let (restarted, mut ws, _) = resumed_with(&rig, ws, mid_turn).await;
        link_attached(&restarted, &uid).await;
        let read_only = crate::claude_host::Setting::Mode {
            mode: "read-only".into(),
            confirmed: false,
        };
        let (told, ()) = tokio::join!(restarted.set_setting(&uid, read_only), async {
            let update = asked(&mut ws, "thread/settings/update").await;
            answer(&mut ws, &update, json!({})).await;
        });
        assert!(matches!(told, SettingResult::Applied { .. }), "{told:?}");
        for frame in capture["live"]["afterTheMidTurnAnswer"].as_array().unwrap() {
            tell(&mut ws, frame).await;
        }
        let follow_up = asked(&mut ws, "thread/resume").await;
        assert_eq!(
            (
                &follow_up["params"]["approvalPolicy"],
                &follow_up["params"]["sandbox"]
            ),
            (&json!("on-request"), &json!("read-only")),
            "the follow-up asked for wider than the Read Only the person set"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **A Plan set again does not override a person's later change** (decision 253): a Plan
    /// run whose ccd restarted mid-turn is put back in Plan, then set to Default from an app
    /// during that turn; the follow-up resume asks for Default's sandbox and does not set Plan
    /// again.
    #[tokio::test]
    async fn a_plan_set_again_gives_way_to_a_later_change() {
        let capture: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let mid_turn = &capture["answerMidTurn"]["result"];
        let thread = mid_turn["thread"]["id"].as_str().unwrap();
        let first_turn = capture["answerAfter"]["result"]["thread"]["turns"][0]["id"]
            .as_str()
            .unwrap();
        let mut plan = settings_report("gpt-6.1-sol", "high", "on-request", "readOnly", "plan");
        plan["params"]["threadId"] = json!(thread);
        let rig = rig();
        let (uid, mut ws) = engine_run(&rig, "req-plan-then-auto", thread, first_turn).await;
        tell(&mut ws, &plan).await;
        settings_as_sent(&rig.daemon, &uid, "plan").await;
        let (restarted, mut ws, _) = resumed_with(&rig, ws, mid_turn.clone()).await;
        let update = asked(&mut ws, "thread/settings/update").await;
        assert_eq!(update["params"]["collaborationMode"]["mode"], "plan");
        answer(&mut ws, &update, json!({})).await;
        link_attached(&restarted, &uid).await;
        let auto = crate::claude_host::Setting::Mode {
            mode: "auto".into(),
            confirmed: false,
        };
        let (told, ()) = tokio::join!(restarted.set_setting(&uid, auto), async {
            let update = asked(&mut ws, "thread/settings/update").await;
            assert_eq!(update["params"]["collaborationMode"]["mode"], "default");
            answer(&mut ws, &update, json!({})).await;
        });
        assert!(matches!(told, SettingResult::Applied { .. }), "{told:?}");
        for frame in capture["live"]["afterTheMidTurnAnswer"].as_array().unwrap() {
            tell(&mut ws, frame).await;
        }
        let follow_up = asked(&mut ws, "thread/resume").await;
        assert_eq!(follow_up["params"]["sandbox"], "workspace-write");
        answer(
            &mut ws,
            &follow_up,
            capture["answerAfter"]["result"].clone(),
        )
        .await;
        let again = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let frame = next_frame(&mut ws).await?;
                if frame["method"] == "thread/settings/update" && frame.get("id").is_some() {
                    return Some(frame);
                }
            }
        })
        .await
        .ok()
        .flatten();
        assert!(
            again.is_none(),
            "the follow-up set Plan again over the change to Default: {again:?}"
        );
        drop(ws);
        end_run(&rig, &uid).await;
    }

    /// **A Codex run offers the model it started on, and can go back to it.** A start's answer
    /// naming a model its start's check did not list (P12a's `stub-model`, which the person's
    /// config names; or, signed in, Codex's default from its catalog online) puts it first among
    /// the run's models, marked chosen, its id its label and no efforts, since Codex names none
    /// for it. Once another model is picked it is still offered, and picking it again goes to
    /// Codex.
    #[tokio::test]
    async fn a_codex_run_offers_the_model_it_started_on_and_can_go_back_to_it() {
        use crate::claude_host::Setting;
        let rig = rig();
        let (uid, mut ws) = engine_run_offering(&rig, &recorded_models(), "stub-model").await;
        let started = settings_as_sent(&rig.daemon, &uid, "auto").await;
        assert_eq!(
            started["menus"]["models"].as_array().map(Vec::len),
            Some(recorded_models().len() + 1)
        );
        assert_eq!(
            started["menus"]["models"][0],
            json!({"value": "stub-model", "label": "stub-model", "efforts": []})
        );
        assert_eq!(started["menus"]["selected_model"], "stub-model");

        let pick = |model: &str| Setting::Model {
            model: Some(model.into()),
            effort: None,
        };
        let (picked, _) = changed(&rig, &mut ws, &uid, pick("gpt-6-astra")).await;
        assert!(
            matches!(picked, SettingResult::Applied { .. }),
            "{picked:?}"
        );
        tell(
            &mut ws,
            &settings_report(
                "gpt-6-astra",
                "high",
                "on-request",
                "workspaceWrite",
                "default",
            ),
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while summary_of(&rig.daemon, &uid)
            .await
            .model
            .is_none_or(|fact| fact.value != "gpt-6-astra")
        {
            assert!(
                Instant::now() < deadline,
                "the run's model never read gpt-6-astra"
            );
            tokio::time::sleep(POLL).await;
        }
        let moved = summary_as_sent(&rig.daemon, &uid).await;
        assert_eq!(moved["menus"]["models"][0]["value"], "stub-model");
        assert_eq!(moved["menus"]["selected_model"], "gpt-6-astra");

        let (back, asked) = changed(&rig, &mut ws, &uid, pick("stub-model")).await;
        assert!(matches!(back, SettingResult::Applied { .. }), "{back:?}");
        assert_eq!(asked, json!({"threadId": "thr-x6", "model": "stub-model"}));
        end_run(&rig, &uid).await;
    }

    /// The run `uid`'s summary as the fleet sends it ([`summary_as_sent`]) once its mode fact
    /// reads `mode`, within 10 s.
    async fn settings_as_sent(daemon: &Daemon, uid: &str, mode: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let summary = summary_of(daemon, uid).await;
            let read = summary
                .permission_mode
                .as_ref()
                .is_some_and(|fact| fact.value == mode);
            if read || Instant::now() >= deadline {
                assert!(
                    read,
                    "the run's mode never read {mode}: {:?}",
                    summary.permission_mode
                );
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        summary_as_sent(daemon, uid).await
    }

    /// `setting` set from an app on the run `uid`, the stand-in answering the one
    /// `thread/settings/update` it gets: ccd's answer, and what it asked Codex.
    async fn changed(
        rig: &Rig,
        ws: &mut Socket,
        uid: &str,
        setting: crate::claude_host::Setting,
    ) -> (SettingResult, serde_json::Value) {
        tokio::join!(rig.daemon.set_setting(uid, setting), async {
            let update = asked(ws, "thread/settings/update").await;
            answer(ws, &update, json!({})).await;
            update["params"].clone()
        })
    }

    /// **X6: a Codex run's settings, as the apps are sent them (minor 43).**
    /// `fixtures/codex/codex-settings-wire.json` is the apps' pin, emitted by this build from a run on
    /// a stand-in app-server that names its settings in Codex's own shapes: the run's menus
    /// (its models from the start's check, the four modes with Full Access marked `confirm`)
    /// and facts from its start's answer; a model and an effort applied while no turn runs; Plan
    /// applied while one does, from its next turn; Full Access refused without the person's
    /// confirmation, nothing sent, then applied with it; the run once Codex reports it; and a
    /// run started on a model its start's check did not list.
    #[tokio::test]
    async fn the_codex_settings_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/codex/codex-settings-wire.json");
        let committed: serde_json::Value =
            serde_json::from_str(FIXTURE).expect("the fixture must be JSON");
        assert_eq!(
            committed["session_codex_full_access"]["permission_mode"]["value"],
            "full-access"
        );
        assert_eq!(
            derived_codex_settings_wire().await,
            committed,
            "the committed Codex settings wire fixture no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_codex_settings_wire_fixture, then copy it over its copy under ios/"
        );
    }

    /// Rewrite `fixtures/codex/codex-settings-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_codex_settings_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_codex_settings_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/codex/codex-settings-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// The frames of a Codex run's settings and each answer ccd gives, and what ccd asks
    /// Codex for each, asserted here.
    async fn derived_codex_settings_wire() -> serde_json::Value {
        use crate::claude_host::Setting;
        let rig = rig();
        let fixed = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
        let (uid, mut ws) = engine_run_offering(&rig, &recorded_models(), "gpt-6.1-sol").await;
        let live = settings_as_sent(&rig.daemon, &uid, "auto").await;

        let (model, asked_model) = changed(
            &rig,
            &mut ws,
            &uid,
            Setting::Model {
                model: Some("gpt-6-astra".into()),
                effort: None,
            },
        )
        .await;
        assert_eq!(
            asked_model,
            json!({"threadId": "thr-x6", "model": "gpt-6-astra"})
        );
        let (effort, asked_effort) = changed(
            &rig,
            &mut ws,
            &uid,
            Setting::Model {
                model: None,
                effort: Some("high".into()),
            },
        )
        .await;
        assert_eq!(
            asked_effort,
            json!({"threadId": "thr-x6", "effort": "high"})
        );
        tell(
            &mut ws,
            &settings_report(
                "gpt-6-astra",
                "high",
                "on-request",
                "workspaceWrite",
                "default",
            ),
        )
        .await;
        settings_as_sent(&rig.daemon, &uid, "auto").await;

        // A turn runs: Plan is answered from the next turn.
        tell(
            &mut ws,
            &json!({"method": "turn/started", "params": {"threadId": "thr-x6",
                "turn": {"id": "turn-2", "status": "inProgress", "items": []}}}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (plan, asked_plan) = changed(
            &rig,
            &mut ws,
            &uid,
            Setting::Mode {
                mode: "plan".into(),
                confirmed: false,
            },
        )
        .await;
        assert_eq!(
            asked_plan,
            json!({"threadId": "thr-x6", "collaborationMode": {"mode": "plan", "settings": {
                "model": "gpt-6-astra", "reasoning_effort": "high", "developer_instructions": null}}}),
            "Plan keeps the run's approval and sandbox"
        );
        tell(
            &mut ws,
            &settings_report(
                "gpt-6-astra",
                "high",
                "on-request",
                "workspaceWrite",
                "plan",
            ),
        )
        .await;
        settings_as_sent(&rig.daemon, &uid, "plan").await;
        tell(
            &mut ws,
            &json!({"method": "turn/completed", "params": {"threadId": "thr-x6",
                "turn": {"id": "turn-2", "status": "completed", "items": []}}}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let full_access = |confirmed| Setting::Mode {
            mode: "full-access".into(),
            confirmed,
        };
        let bare = rig.daemon.set_setting(&uid, full_access(false)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(300), next_frame(&mut ws))
                .await
                .is_err(),
            "the unconfirmed Full Access sent Codex nothing"
        );
        let (confirmed, asked_full) = changed(&rig, &mut ws, &uid, full_access(true)).await;
        assert_eq!(
            asked_full,
            json!({"threadId": "thr-x6", "approvalPolicy": "never",
                "sandboxPolicy": {"type": "dangerFullAccess"},
                "collaborationMode": {"mode": "default", "settings": {
                    "model": "gpt-6-astra", "reasoning_effort": "high",
                    "developer_instructions": null}}})
        );
        tell(
            &mut ws,
            &settings_report(
                "gpt-6-astra",
                "high",
                "never",
                "dangerFullAccess",
                "default",
            ),
        )
        .await;
        let full = settings_as_sent(&rig.daemon, &uid, "full-access").await;
        end_run(&rig, &uid).await;

        // A run whose start's answer names a model its start's check did not list.
        let other = self::rig();
        let (unlisted, unlisted_ws) =
            engine_run_offering(&other, &recorded_models(), "stub-model").await;
        let unlisted_default = settings_as_sent(&other.daemon, &unlisted, "auto").await;
        end_run(&other, &unlisted).await;
        drop(unlisted_ws);

        let answer = |request_id: &str, result: SettingResult| {
            serde_json::to_value(protocol::ws::ServerMessage::SettingResult {
                session_id: fixed.into(),
                request_id: request_id.into(),
                result,
            })
            .unwrap()
        };
        let set_model = |request_id: &str, model: Option<&str>, effort: Option<&str>| {
            serde_json::to_value(protocol::ws::ClientMessage::SetModel {
                session_id: fixed.into(),
                request_id: request_id.into(),
                model: model.map(str::to_string),
                effort: effort.map(str::to_string),
            })
            .unwrap()
        };
        let set_mode = |request_id: &str, mode: &str, confirmed: bool| {
            serde_json::to_value(protocol::ws::ClientMessage::SetMode {
                session_id: fixed.into(),
                request_id: request_id.into(),
                mode: mode.into(),
                confirmed,
            })
            .unwrap()
        };
        json!({
            "session_codex_live": live,
            "set_model_request": set_model("model-1", Some("gpt-6-astra"), None),
            "applied_model": answer("model-1", model),
            "set_effort_request": set_model("effort-1", None, Some("high")),
            "applied_effort": answer("effort-1", effort),
            "set_mode_plan_request": set_mode("mode-1", "plan", false),
            "applied_plan_during_a_turn": answer("mode-1", plan),
            "set_mode_bare_request": set_mode("mode-2", "full-access", false),
            "refused_unconfirmed": answer("mode-2", bare),
            "set_mode_confirmed_request": set_mode("mode-3", "full-access", true),
            "applied_confirmed": answer("mode-3", confirmed),
            "session_codex_full_access": full,
            "session_codex_unlisted_default": unlisted_default,
        })
    }

    // ------------------------------- an engine turn's end as a doorbell

    /// How many doorbells `capture` holds once the grace `hold` keeps is ended and `n` have
    /// rung, or 10 s have passed, and 300 ms more for any ring beyond them.
    async fn rung(
        capture: &crate::apns::CaptureSender,
        hold: &crate::push_gate::GraceHold,
        n: usize,
    ) -> usize {
        hold.end();
        let deadline = Instant::now() + Duration::from_secs(10);
        while capture.0.lock().unwrap().len() < n && Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        capture.0.lock().unwrap().len()
    }

    /// The stand-in tells the run on `thread` one agent message `id`, in its turn `turn`, and
    /// the test waits until ccd has filed it: the link reads a run's frames in order, so every
    /// frame told before it has been read, and every push it admitted has taken its grace.
    async fn read_up_to_here(
        daemon: &Daemon,
        uid: &str,
        ws: &mut Socket,
        thread: &str,
        turn: &str,
        id: &str,
    ) {
        tell(
            ws,
            &json!({"method": "item/completed", "params": {"threadId": thread, "turnId": turn,
                "item": {"type": "agentMessage", "id": id, "text": "read up to here",
                    "phase": "final_answer", "memoryCitation": null}}}),
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let events = daemon
                .db
                .events_after(uid.to_string(), 0, 10_000)
                .await
                .unwrap();
            if events.iter().any(|event| {
                event
                    .source_event_id
                    .as_deref()
                    .is_some_and(|filed| filed.ends_with(id))
            }) {
                return;
            }
            assert!(Instant::now() < deadline, "ccd never filed {id}");
            tokio::time::sleep(POLL).await;
        }
    }

    /// The stand-in tells the run on `thread` its turn `turn` started.
    async fn turn_starts(ws: &mut Socket, thread: &str, turn: &str) {
        tell(
            ws,
            &json!({"method": "turn/started", "params": {"threadId": thread,
                "turn": {"id": turn, "status": "inProgress", "items": []}}}),
        )
        .await;
    }

    /// The stand-in tells the run on `thread` its turn `turn` ended `status`.
    async fn turn_ends(ws: &mut Socket, thread: &str, turn: &str, status: &str) {
        tell(
            ws,
            &json!({"method": "turn/completed", "params": {"threadId": thread,
                "turn": {"id": turn, "status": status, "items": []}}}),
        )
        .await;
    }

    /// A rig whose doorbells land in a capture, and an engine run on it holding one approval
    /// card the phone can answer, its own doorbell already rung: the decision open elsewhere
    /// a turn's end rings about. The run's uid and the stand-in's end of its link.
    async fn a_codex_card_open() -> (
        Rig,
        Arc<crate::apns::CaptureSender>,
        crate::push_gate::GraceHold,
        String,
        Socket,
    ) {
        let capture = Arc::new(crate::apns::CaptureSender::new());
        let rig = rig_sending(Arc::clone(&capture) as Arc<dyn crate::apns::PushSender>);
        let hold = rig.daemon.push_gate.hold_grace();
        let frames = frames_of(FILE_FRAMES);
        let (thread, turn, started, asked) = on_the_running_turn(&frames, 17, 19);
        let (uid, mut ws) = engine_run(&rig, "req-card", &thread, &turn).await;
        tell(&mut ws, &started).await;
        tell(&mut ws, &asked).await;
        assert_eq!(cards_of(&rig.daemon, &uid, 1).await.len(), 1);
        read_up_to_here(&rig.daemon, &uid, &mut ws, &thread, &turn, "read-card").await;
        assert_eq!(rung(&capture, &hold, 1).await, 1, "the card's own doorbell");
        (rig, capture, hold, uid, ws)
    }

    /// **An engine run's turn end rings again about a decision still open, and about
    /// nothing else**, as a hosted Claude run's does. With nothing the phone can answer open,
    /// a Codex turn completing rings nothing; with an approval open on another run, it rings
    /// once more, about that approval, by the run that finished.
    ///
    /// **Mutation:** take the push out of the link and the second leg rings nothing.
    #[tokio::test]
    async fn an_engine_turns_end_rings_only_about_a_decision_open_elsewhere() {
        let capture = Arc::new(crate::apns::CaptureSender::new());
        let quiet = rig_sending(Arc::clone(&capture) as Arc<dyn crate::apns::PushSender>);
        let hold = quiet.daemon.push_gate.hold_grace();
        let (uid, mut ws) = engine_run(&quiet, "req-quiet", "thr-q", "turn-q1").await;
        turn_starts(&mut ws, "thr-q", "turn-q2").await;
        turn_ends(&mut ws, "thr-q", "turn-q2", "completed").await;
        read_up_to_here(&quiet.daemon, &uid, &mut ws, "thr-q", "turn-q2", "read-q2").await;
        assert_eq!(rung(&capture, &hold, 0).await, 0, "with nothing open");
        drop(ws);
        end_run(&quiet, &uid).await;

        let (rig, capture, hold, asking, asking_ws) = a_codex_card_open().await;
        let (uid, mut ws) = engine_run(&rig, "req-b", "thr-b", "turn-b1").await;
        turn_starts(&mut ws, "thr-b", "turn-b2").await;
        turn_ends(&mut ws, "thr-b", "turn-b2", "completed").await;
        read_up_to_here(&rig.daemon, &uid, &mut ws, "thr-b", "turn-b2", "read-b2").await;
        assert_eq!(
            rung(&capture, &hold, 2).await,
            2,
            "the card's, then the turn's end about it"
        );
        {
            let rings = capture.0.lock().unwrap();
            let (hint, _) = rings.last().unwrap();
            assert_eq!(
                (hint.kind, hint.decisions),
                (crate::apns::PushKind::Approval, 1)
            );
            assert_eq!(hint.session_uid, uid, "rung by the run that finished");
        }
        drop((ws, asking_ws));
        end_run(&rig, &uid).await;
        end_run(&rig, &asking).await;
    }

    /// **Two engine turns ending inside one dispatch grace ring once**: the second turn's
    /// start cancels the first one's doorbell, and its end rings in its place.
    ///
    /// **Mutation:** take the push out of the link and nothing rings after the card's.
    #[tokio::test]
    async fn two_engine_turns_ending_inside_one_grace_ring_once() {
        let (rig, capture, hold, asking, asking_ws) = a_codex_card_open().await;
        let (uid, mut ws) = engine_run(&rig, "req-b", "thr-b", "turn-b1").await;
        turn_starts(&mut ws, "thr-b", "turn-b2").await;
        turn_ends(&mut ws, "thr-b", "turn-b2", "completed").await;
        turn_starts(&mut ws, "thr-b", "turn-b3").await;
        turn_ends(&mut ws, "thr-b", "turn-b3", "completed").await;
        read_up_to_here(&rig.daemon, &uid, &mut ws, "thr-b", "turn-b3", "read-b3").await;
        assert_eq!(rung(&capture, &hold, 2).await, 2, "the card's, then one");
        drop((ws, asking_ws));
        end_run(&rig, &uid).await;
        end_run(&rig, &asking).await;
    }

    /// **An engine turn starting inside the grace silences the last one's doorbell**: the
    /// run is working again, so there is no finished turn left to announce.
    ///
    /// **Mutation:** take `note_turn_running` out of the link and the first turn's end rings.
    #[tokio::test]
    async fn an_engine_turn_starting_inside_the_grace_silences_the_last_ones_doorbell() {
        let (rig, capture, hold, asking, asking_ws) = a_codex_card_open().await;
        let (uid, mut ws) = engine_run(&rig, "req-b", "thr-b", "turn-b1").await;
        turn_starts(&mut ws, "thr-b", "turn-b2").await;
        turn_ends(&mut ws, "thr-b", "turn-b2", "completed").await;
        turn_starts(&mut ws, "thr-b", "turn-b3").await;
        read_up_to_here(&rig.daemon, &uid, &mut ws, "thr-b", "turn-b3", "read-b3").await;
        assert_eq!(rung(&capture, &hold, 1).await, 1, "the card's alone");
        drop((ws, asking_ws));
        end_run(&rig, &uid).await;
        end_run(&rig, &asking).await;
    }

    /// **An engine turn that did not complete rings nothing**: an interrupted turn was
    /// stopped by the person the doorbell would wake.
    ///
    /// **Mutation:** ring any turn's end and the interrupted one rings.
    #[tokio::test]
    async fn an_interrupted_engine_turns_end_rings_nothing() {
        let (rig, capture, hold, asking, asking_ws) = a_codex_card_open().await;
        let (uid, mut ws) = engine_run(&rig, "req-b", "thr-b", "turn-b1").await;
        turn_starts(&mut ws, "thr-b", "turn-b2").await;
        turn_ends(&mut ws, "thr-b", "turn-b2", "interrupted").await;
        read_up_to_here(&rig.daemon, &uid, &mut ws, "thr-b", "turn-b2", "read-b2").await;
        assert_eq!(rung(&capture, &hold, 1).await, 1, "the card's alone");
        drop((ws, asking_ws));
        end_run(&rig, &uid).await;
        end_run(&rig, &asking).await;
    }

    /// **A turn's end a restarted ccd files from a resume rings nothing**: it ended while ccd
    /// was down, and nobody is waiting to hear of it now. The run's card stands on its running
    /// turn across the restart, so a ring would have a decision to be about.
    ///
    /// **Mutation:** ring a resume's filed turn end too, and it rings.
    #[tokio::test]
    async fn a_turns_end_a_restarted_ccd_files_from_a_resume_rings_nothing() {
        let (rig, _, _, uid, ws) = a_codex_card_open().await;
        let capture = Arc::new(crate::apns::CaptureSender::new());
        let restarted = daemon_sending(
            &rig.root,
            "/usr/bin/false",
            Arc::clone(&capture) as Arc<dyn crate::apns::PushSender>,
        );
        let hold = restarted.push_gate.hold_grace();
        let follow_up: serde_json::Value = serde_json::from_str(FOLLOW_UP).unwrap();
        let mut finished = follow_up["answerMidTurn"]["result"]["thread"]["turns"][0].clone();
        assert_eq!(finished["status"], "completed");
        let mut resume: serde_json::Value = serde_json::from_str(RUNNING_TURN).unwrap();
        let turns = resume["result"]["thread"]["turns"].as_array_mut().unwrap();
        // A resume lists its turns in the order they started: the finished one before.
        let running_since = turns[0]["startedAt"].as_i64().unwrap();
        finished["startedAt"] = json!(running_since - 10);
        finished["completedAt"] = json!(running_since - 5);
        turns.insert(0, finished);
        let (restarted, ws, _) = resumed_on(&rig, ws, resume["result"].clone(), restarted).await;
        link_attached(&restarted, &uid).await;
        assert!(
            restarted
                .db
                .events_after(uid.clone(), 0, 10_000)
                .await
                .unwrap()
                .iter()
                .any(|event| event.kind == EventKind::TurnComplete),
            "the resume filed the finished turn's end"
        );
        assert_eq!(
            cards_of(&restarted, &uid, 1).await.len(),
            1,
            "the card stands"
        );
        assert_eq!(rung(&capture, &hold, 0).await, 0, "the resume's end");
        drop(ws);
        end_run(&rig, &uid).await;
    }
}
