//! Daemon state and the paths that mutate it.
//!
//! Everything that can change a session lives here so the ordering rules are in
//! one place:
//!
//!   * **Ingest** assigns `seq` (in the store) and then broadcasts, and does
//!     both **under one per-session gate**. Assigning and publishing are two
//!     steps, so without the gate two tasks can commit 1 and 2 and then publish
//!     2 and 1 — and a subscriber that has seen 2 has no way to accept 1
//!     afterwards. The gate makes "the order events were assigned" and "the
//!     order they were published" the same sentence. A subscriber that joins
//!     between the two steps still sees the event, because `subscribe` reads
//!     the backlog *after* joining the broadcast and drops anything at or below
//!     its watermark.
//!   * **Answering** serialises per request, claims in memory, claims *durably*,
//!     hands the answer over, then records the terminal outcome. Serialising
//!     first is what makes a duplicate tap arriving *during* a handoff return the
//!     original outcome rather than a rejection. The durable claim is what makes
//!     a daemon killed mid-handoff able to say "I do not know whether that
//!     landed" instead of sending it a second time.
//!   * **Card identity.** A Claude card is a hosted run's permission request,
//!     answered only through that run's agent host, by the request's
//!     `tool_use_id`.
//!
//! **Identity.** Every map here is keyed by `session_uid`, never by the name. A
//! name can be reused by the next session, so keying by it meant a new run
//! adopted the dead one's log and its answered approvals.
//! Anything a client names is resolved through [`Daemon::resolve`] first, which
//! is the single place that decides what a bare `cc-1` refers to.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use protocol::event::{
    Event, EventKind, Lifecycle, Link, PendingEvent, PendingRef, SessionKey, SessionSummary, Source,
};
use protocol::hash::{approval_payload_hash, approval_payload_text, sha256_hex};
use protocol::pairing::DeviceSummary;
use protocol::ws::{
    AnswerDecision, AnswerOutcome, AnswerPath, AnswerResult, AnsweredOn, ApprovalCard,
    QuestionHold, ResolvedBy, SendTextResult,
};
use tokio::sync::{broadcast, Mutex};

use crate::apns::{PushHint, PushSender};
use crate::db::Db;
use crate::store::{
    AnswerClaim, DeviceLookup, DeviceRow, PairingConsume, PrunedSession, SessionRow, Store,
};
use protocol::config::Config;

/// Broadcast ring size. A subscriber that falls this far behind gets an
/// explicit `Resync` rather than a silent gap.
const BROADCAST_CAPACITY: usize = 1024;

/// Revocation ring size. Tiny on purpose: a human revoking devices produces a
/// handful of messages in a lifetime, and a connection that somehow lags off
/// this ring still has the per-message and keepalive lookups underneath it.
const REVOCATION_CAPACITY: usize = 64;

/// `(session_uid, request_id)`. An approval belongs to one run, and a request id
/// is only unique within one.
type ApprovalId = (String, String);

/// What one terminal did to one Codex card.
///
/// **Three answers, because two of them used to be the same `false` and they are
/// opposites.** "Somebody else already retired this" means nothing is owed;
/// "the commit failed" means everything is still owed — the claim is back, the
/// card is still on a phone, and whatever else would let a later terminal finish
/// the job must be kept too. A caller that cannot tell them apart throws away
/// the wire mapping on both, and a failed retirement silently becomes a
/// permanent one. See [`Daemon::retire_codex_approval`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retirement {
    /// This terminal owned the card, and its removal and resolution are
    /// committed.
    Retired,
    /// There was no claim to take: an earlier terminal won the race. Nothing
    /// failed, and nothing is owed.
    AlreadyGone,
    /// The transaction failed. The claim has been put back and the card is still
    /// open; the caller owes it every route a later terminal could arrive by.
    Failed,
}

/// Where the phone should connect. Resolved once at startup and printed into
/// the QR, so the code the operator scans and the socket the daemon opened can
/// never describe different endpoints.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

pub struct Daemon {
    pub config: Config,
    /// The synchronous store.
    ///
    /// Kept for the handful of places that genuinely have no runtime to defer
    /// to — the startup integrity check, and the tests. Every path inside an
    /// `async fn` uses [`Daemon::db`] instead, which runs the same operations on
    /// the blocking pool rather than on a runtime worker.
    pub store: Arc<Store>,
    /// The async handle to the same store. See [`crate::db`].
    pub db: Db,
    pub events_tx: broadcast::Sender<Event>,
    /// Device ids whose access has just been withdrawn.
    ///
    /// Revocation used to reach an open socket only when that socket next did
    /// something — a message, or the 30-second keepalive. A phone sitting idle
    /// on a subscription kept receiving the event log for up to half a minute
    /// after the operator revoked it, which is not what `codeconnect revoke` means. This
    /// is the push half: every connection selects on it and closes immediately
    /// when its own id arrives, and the periodic lookup stays as the backstop
    /// for a connection that was not listening when the message went out.
    pub revocations_tx: broadcast::Sender<String>,
    pub push: Arc<dyn PushSender>,
    /// What may ring a phone — see [`crate::push_gate`]. Its own locks, held
    /// for map lookups only; never across an await.
    pub push_gate: Arc<crate::push_gate::PushGate>,
    /// **Doorbells reach the senders in the order they were counted.** Held from
    /// a doorbell's count of waiting decisions through its hand-off to the
    /// sender, whose registry read and enqueue are synchronous. A device's queue
    /// keeps the doorbell that arrives last, and every doorbell shares one
    /// collapse slot on the phone, so arrival order has to be count order: a
    /// count taken before a decision was raised, posted after the doorbell that
    /// counted it, would tell the phone nothing waits.
    ///
    /// **The cost is head-of-line.** A send that is slow — a registry read
    /// stalled on the database — delays every doorbell behind it by as long.
    /// Measured on a debug build over a real store through the relay sender, a
    /// hold is a median 20 µs with one device and 576 µs with fifty, against a
    /// 400 ms dispatch grace. What a doorbell checked before the wait is checked
    /// again after it.
    ring_order: tokio::sync::Mutex<()>,
    pub endpoint: Endpoint,
    /// The listener's actual bind address, told to the daemon by `main` once
    /// the socket exists. A `OnceLock` rather than a constructor parameter so
    /// the tests — which never open a real listener — say nothing instead of
    /// inventing an address.
    pub bind_ip: std::sync::OnceLock<String>,
    /// Which daemon this is (`protocol::machine_id_path`), told by `main` once it
    /// has read or minted it, and sent in every `hello_ack`. A `OnceLock` for the
    /// same reason as `bind_ip`: the tests never mint one, and say none.
    pub machine_id: std::sync::OnceLock<String>,
    /// (path, sha256) of the executable this process is running, captured by
    /// `main` at startup for the staleness line in `daemon status`.
    pub exe_identity: std::sync::OnceLock<(String, String)>,
    /// Why no phone can reach this daemon, when its listener refuses every
    /// connection: told by `main` from the advice it logs at startup, so
    /// `codeconnect pair` is refused with that reason instead of being handed a
    /// code no phone could use.
    pub listener_refusal: std::sync::OnceLock<String>,
    /// What a message relaunches an engine Codex run with ([`crate::codex_host::relaunch`]),
    /// told by `main`: the state folder whose `runs/` holds the runs ccd starts (the one
    /// [`protocol::root_dir`] names), and the `codeconnect` a run's thread config names for its
    /// mail server ([`codex_launcher`]), or why there is none. A `OnceLock` for `bind_ip`'s
    /// reason: a test that never tells it relaunches nothing.
    pub relaunch_from:
        std::sync::OnceLock<(std::path::PathBuf, Result<std::path::PathBuf, String>)>,
    /// When this process started, and which job it belongs to. Captured once at
    /// construction: [`SERVICE_LABEL_VAR`] is set by launchd, or by CodeConnect's
    /// systemd unit, at exec, and reading it later would be reading whatever the
    /// environment has become.
    pub(crate) started_at: String,
    launchd_label: Option<String>,
    inner: Arc<Mutex<Inner>>,
    /// One gate per run, held across *both* halves of an ingest.
    ///
    /// Separate from `inner` on purpose: it is held while SQLite commits, and
    /// folding it into the state lock would serialise every unrelated query
    /// behind a write. Kept in its own map so the lock ordering is one-way —
    /// gate, then `inner`, never the reverse.
    publish_gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// One per run folder: no two of its diff, repository read, session-list refresh and
    /// action run git at once ([`Daemon::git_gate`]).
    git_gates: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// The runs' GitHub reads, bounded per branch ([`crate::github::Reads`]).
    github: crate::github::Reads,
    /// One gate per run, held by a person's message to a run with a switch of account from reading the run through
    /// sending it, and by a mail wake from its claim through its issue ([`Daemon::move_gate`]).
    move_gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// One gate per session, held across a hosted run's recorded end and what
    /// follows it ([`Daemon::end_hosted_run`]).
    ///
    /// The same shape as [`Daemon::publish_gate`], for the same reason: a per-uid
    /// `Arc<Mutex<()>>` in its own map, so the lock ordering stays one-way — gate,
    /// then `inner`, never the reverse.
    registration_gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The tailnet [`Daemon::read_tailnet`] reads in a test.
    #[cfg(test)]
    pub(crate) test_tailnet: std::sync::Mutex<Option<protocol::tailnet::Tailnet>>,
    /// One gate per session, its read side held across a phone answer's passage
    /// through a Codex run's link. Nothing takes its write side.
    ///
    /// **An `RwLock`, and not a mutex, because answers are not rivals of each
    /// other.** A session can have two cards open and answer both at once, so
    /// answers take the READ side and stay concurrent.
    ///
    /// Same shape and same one-way ordering as the two gates above — gate, then
    /// `inner`, never the reverse.
    actuation_gates: Mutex<HashMap<String, Arc<tokio::sync::RwLock<()>>>>,
    /// The phone's session starts, by request id.
    pub(crate) starts: crate::start_session::Starts,
    /// Files sent for runs' agents, by upload id ([`crate::uploads`]).
    pub(crate) uploads: crate::uploads::Uploads,
    /// The runs' shells (`run_terminal`).
    pub(crate) terminals: crate::terminal::Terminals,
    /// Which agents could start, as last checked.
    pub(crate) agents: crate::start_session::KnownAgents,
    /// The account sign-in this computer is running, if any ([`crate::accounts::sign_in`]).
    pub(crate) sign_ins: Arc<crate::accounts::SignIns>,
    /// What each account says of itself (email, plan, usage), held in memory only ([`crate::accounts::Facts`]).
    pub(crate) account_facts: crate::accounts::Facts,
    /// The accounts listing, sent once whenever it changes, for each connection that reads accounts to push.
    pub(crate) accounts_tx: broadcast::Sender<protocol::ws::ServerMessage>,
}

/// **What the Claude-only gate found** — see [`Daemon::refuse_unless_claude`].
///
/// Two refusals, not one. A bare `Option<String>` had exactly one shape for "no", so an
/// unknown session had to be either refused with the wrong sentence or admitted, and
/// it was admitted. That made a gate whose whole contract is to fail closed fail open
/// on the one input it cannot vouch for.
#[derive(Debug, PartialEq, Eq)]
pub enum ClaudeOnly {
    /// The run is hosted, and it is Claude's. The operation may proceed.
    Admitted,
    /// The run is hosted and is not Claude's. The connection is welcome; this
    /// particular thing is not something the daemon can do for this agent.
    WrongAgent(String),
    /// **The reference names no run this daemon can vouch for**, or the row could not
    /// be read. Not evidence that the run is Claude's.
    Unknown(String),
}

impl ClaudeOnly {
    /// The sentence, for a caller with one refusal to give whichever way it went.
    pub fn reason(self) -> Option<String> {
        match self {
            ClaudeOnly::Admitted => None,
            ClaudeOnly::WrongAgent(why) | ClaudeOnly::Unknown(why) => Some(why),
        }
    }
}

/// How a `hello` was (or was not) authenticated.
#[derive(Debug)]
pub enum AuthOutcome {
    /// The static bearer token from `codeconnect token`.
    Static,
    /// A per-device token minted by an earlier pairing.
    Device(Box<DeviceRow>),
    /// A pairing code was redeemed; these credentials are new.
    Paired {
        device_id: String,
        device_name: String,
        token: String,
    },
    /// Refused. The string is for *our* log; the peer gets one opaque error,
    /// because telling an unauthenticated caller which of "wrong", "expired"
    /// and "already used" applies is a free oracle.
    Rejected(String),
    /// A token that matches a pairing the computer has blocked (`revoked_at` set): the
    /// device keeps its token, so the phone is told "revoked" and waits to be allowed
    /// again, not sent to pair with a new code.
    Revoked,
}

/// What `codeconnect revoke` did.
#[derive(Debug)]
pub struct RevokeOutcome {
    pub device: DeviceSummary,
    pub token_revoked: bool,
}

/// A hosted run's link, as [`Inner::host_links`] holds it.
pub(crate) enum HostLink {
    /// A Claude run's: the way to send its agent host a command ([`crate::claude_host`]).
    Claude(tokio::sync::mpsc::UnboundedSender<protocol::host::ToHost>),
    /// A Codex run's: the way to hand its link a phone's answer to one of its cards, and the
    /// way to ask it to stop its turn or the run ([`crate::codex_host`]).
    Codex(
        crate::codex_host::LinkAnswers,
        tokio::sync::mpsc::UnboundedSender<crate::codex_host::LinkCommand>,
    ),
}

/// One settings line sent to a run's agent host ([`Inner::host_settings`]).
pub(crate) struct PendingSetting {
    /// The run's `session_uid`.
    pub uid: String,
    /// The effort the line sets, filed as the run's once claude has taken it.
    pub effort: Option<String>,
    /// Whoever waits: told whether claude took it, with its reason when it did not.
    pub tell: tokio::sync::oneshot::Sender<(bool, Option<String>)>,
}

#[derive(Default)]
struct Inner {
    /// Per-device floor between deliberate test pushes. A stolen credential
    /// must not become an APNs harassment primitive, and 30 seconds costs a
    /// legitimate tester nothing.
    test_pushes: HashMap<String, std::time::Instant>,
    /// Hosted runs whose agent host or app-server this daemon is attached to now, by
    /// `session_uid`: what makes such a run's link read attached.
    host_links: HashMap<String, HostLink>,
    /// Claude runs whose link attached again to a running host whose replay has not ended yet: what ccd has filed of
    /// such a run may be behind what its host holds ([`Daemon::replaying`]).
    replaying: HashSet<String>,
    /// Messages sent to a run's agent host and waiting for its outcome, by the message's
    /// uuid, each with its run's `session_uid`: told `true` once the host has taken the
    /// message into claude's input, `false` once it refused it. Dropped when the run's
    /// link ends, so a waiter then hears nothing more.
    host_sends: HashMap<String, (String, tokio::sync::oneshot::Sender<bool>)>,
    /// Answers sent to a run's agent host and waiting for its outcome, by the
    /// answer's uuid: the outcome the run's link records once the host says the
    /// answer reached claude. A restarted daemon holds none, and records the
    /// outcome from the answer's claim instead.
    host_answers: HashMap<String, AnswerOutcome>,
    /// App Stops sent to a run's agent host and waiting for its outcome, by the
    /// interrupt's uuid, each with its run's `session_uid`: told what became of the Stop
    /// once the host's outcome is applied ([`crate::claude_host::interrupt`]). Dropped when
    /// the run's link ends, so a waiter then hears nothing more.
    host_stops: HashMap<
        String,
        (
            String,
            tokio::sync::oneshot::Sender<crate::codex_host::InterruptReport>,
        ),
    >,
    /// Settings sent to a run's agent host and waiting for its outcome, by the line's uuid
    /// ([`crate::claude_host::set_setting`]). Kept past the app's wait, so an outcome that
    /// comes late still files the effort it set; dropped when the run's link ends.
    host_settings: HashMap<String, PendingSetting>,
    pending: HashMap<ApprovalId, PendingApproval>,
    /// The last [`PendingApproval::raise`] handed out.
    next_raise: u64,
    /// One async lock per outstanding approval, so two taps on the same card are
    /// applied one after the other rather than one being rejected mid-flight.
    ///
    /// A `tokio::sync::Mutex` and not a flag: the loser has to *wait* for the
    /// winner's ledger write before it can be told the original outcome, and the
    /// wait spans an await (handing the answer over).
    answer_locks: HashMap<ApprovalId, Arc<Mutex<()>>>,
    /// Per run: its model and permission mode as of the newest event that
    /// decided each, for the session list. Rebuilt from the log by
    /// [`Daemon::recover`] and kept current by every ingest after it. One entry
    /// per run that has ingested anything.
    session_facts: HashMap<String, protocol::facts::SessionFacts>,
    /// Per run: its working tree's branch and line counts as git last read them,
    /// for the session list. Filled by [`Daemon::refresh_tree`] when a turn
    /// ends and at start; never on a listing.
    tree_facts: HashMap<String, crate::git::TreeFacts>,
    /// When pairing attempts failed, inside the current window.
    ///
    /// Oldest first, and never longer than `pairing_max_attempts`, so this is a
    /// fixed-size structure rather than an unbounded record of everything that
    /// has ever knocked. Memory-only on purpose: a restart clearing the counter
    /// costs an attacker one daemon crash they cannot cause, and persisting it
    /// would mean a failed guess could lock the operator out of pairing.
    pairing_failures: std::collections::VecDeque<i64>,
}

/// Takes a Codex raise's card back out of `pending` unless the raise disarms it
/// — on a refused commit, or when the raise is dropped at any await before it
/// completes. It removes the entry only while it is still the one this raise
/// inserted ([`PendingApproval::raise`]), never one put there since.
struct CardTakeBack {
    inner: Arc<Mutex<Inner>>,
    id: ApprovalId,
    raise: u64,
    armed: bool,
}

impl CardTakeBack {
    fn remove(inner: &mut Inner, id: &ApprovalId, raise: u64) {
        if inner
            .pending
            .get(id)
            .is_some_and(|entry| entry.raise == raise)
        {
            inner.pending.remove(id);
        }
    }

    /// Take the card back now, for a raise that did not open — if this raise
    /// put it there.
    async fn now(mut self) {
        if self.armed {
            Self::remove(&mut *self.inner.lock().await, &self.id, self.raise);
            self.armed = false;
        }
    }
}

impl Drop for CardTakeBack {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Dropping cannot wait for the lock. Held elsewhere at this instant, the
        // removal is handed to a task.
        match self.inner.try_lock() {
            Ok(mut inner) => Self::remove(&mut inner, &self.id, self.raise),
            Err(_) => {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let (inner, id, raise) = (Arc::clone(&self.inner), self.id.clone(), self.raise);
                    runtime
                        .spawn(async move { Self::remove(&mut *inner.lock().await, &id, raise) });
                }
            }
        }
    }
}

struct PendingApproval {
    card: ApprovalCard,
    session: SessionKey,
    /// Which Codex raise put this card here, so that raise's take-back removes
    /// only its own card ([`CardTakeBack`]). Unique per raise; 0 on a card no
    /// Codex raise inserted.
    raise: u64,
    claimed: bool,
    /// The call's own result arrived for this request id: the call is over.
    tool_ran: bool,
    /// What the run this card came from is called.
    ///
    /// **Captured with the card, not looked up when a doorbell rings.** A
    /// notification that speaks for the fleet names the one blocked run, and
    /// reading that name from the database means releasing the lock the count
    /// was taken under and awaiting SQLite — during which the card can be
    /// answered or another run can block, so the name and the number stop
    /// describing the same moment.
    project_label: String,
    /// Which agent this card belongs to, captured with the card for the same
    /// reason the label is.
    agent: protocol::agent::AgentKind,
    /// Asked by a background agent rather than the main conversation.
    fork: bool,
    /// Answered through the run's host link: true on a hosted run's card; false on a
    /// Codex card and on a Claude card recovery restored without reading its run's host.
    link: bool,
    /// An `AskUserQuestion` card.
    question: bool,
}

/// The card a decision push is about, by the request id that filed it.
#[derive(Debug, Clone)]
struct CardKey(String);

/// Whether the card a decision push announces is still there to be answered.
///
/// **Asked when the push is about to ring, not when it was admitted.** A card
/// can be answered, or its call can run, inside the dispatch grace — or, after a
/// restart, in the instant between reading the card and taking the gate. Ringing
/// then would send someone to a decision list with nothing in it. A card whose
/// call has run is answered, though its entry stays until it is retired.
fn card_is_open(inner: &Inner, session_uid: &str, key: &CardKey) -> bool {
    the_card(inner, session_uid, key).is_some_and(|entry| !entry.tool_ran)
}

fn the_card<'a>(inner: &'a Inner, session_uid: &str, key: &CardKey) -> Option<&'a PendingApproval> {
    inner.pending.get(&(session_uid.to_string(), key.0.clone()))
}

/// Whether an answer from the phone can reach this card now: a Codex card's
/// through its link, a hosted run's card through its host's link. Not once the
/// call has run, and not while an answer is already on its way.
fn phone_can_answer(entry: &PendingApproval) -> bool {
    !entry.tool_ran
        && !entry.claimed
        && (matches!(entry.agent, protocol::agent::AgentKind::Codex) || entry.link)
}

/// The decisions a doorbell may speak of, read under one lock: how many cards
/// the phone can answer are open, what the one run holding them is called
/// when exactly one run is (however many it holds), and whether the one
/// decision is a question. One run can hold several at once, since background
/// agents each ask their own.
///
/// **Every agent's runs, whichever agent rang.** A phone keeps one CodeConnect
/// notification — every doorbell shares one collapse id, and a device's queue
/// replaces a waiting doorbell with the newer one — and its badge is this
/// count.
struct Waiting {
    decisions: usize,
    label: Option<String>,
    question: bool,
}

impl Waiting {
    fn of(inner: &Inner) -> Waiting {
        let open: Vec<&PendingApproval> = inner
            .pending
            .values()
            .filter(|entry| phone_can_answer(entry))
            .collect();
        let mut runs: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for entry in &open {
            runs.insert(&entry.session.uid, &entry.project_label);
        }
        Waiting {
            decisions: open.len(),
            label: match runs.into_values().collect::<Vec<_>>().as_slice() {
                [label] => Some(label.to_string()),
                _ => None,
            },
            // A Claude question carries its question state; a Codex one is a card of Codex's
            // own question tool.
            question: matches!(open.as_slice(), [entry] if entry.question
                || entry.card.tool_name == crate::codex_approval::QUESTION_TOOL_NAME),
        }
    }
}

impl Daemon {
    pub fn new(
        config: Config,
        store: Arc<Store>,
        push: Arc<dyn PushSender>,
        endpoint: Endpoint,
    ) -> Arc<Daemon> {
        let (events_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (revocations_tx, _) = broadcast::channel(REVOCATION_CAPACITY);
        let db = Db::new(Arc::clone(&store));
        Arc::new(Daemon {
            config,
            store,
            db,
            events_tx,
            revocations_tx,
            push,
            push_gate: Arc::new(crate::push_gate::PushGate::new()),
            ring_order: tokio::sync::Mutex::new(()),
            endpoint,
            bind_ip: std::sync::OnceLock::new(),
            machine_id: std::sync::OnceLock::new(),
            exe_identity: std::sync::OnceLock::new(),
            listener_refusal: std::sync::OnceLock::new(),
            relaunch_from: std::sync::OnceLock::new(),
            started_at: protocol::time::now_rfc3339(),
            launchd_label: launchd_label(),
            inner: Arc::new(Mutex::new(Inner::default())),
            publish_gates: Mutex::new(HashMap::new()),
            git_gates: Arc::new(Mutex::new(HashMap::new())),
            github: crate::github::Reads::default(),
            move_gates: Mutex::new(HashMap::new()),
            registration_gates: Mutex::new(HashMap::new()),
            #[cfg(test)]
            test_tailnet: std::sync::Mutex::new(None),
            actuation_gates: Mutex::new(HashMap::new()),
            starts: crate::start_session::Starts::default(),
            uploads: crate::uploads::Uploads::default(),
            terminals: crate::terminal::Terminals::default(),
            agents: crate::start_session::KnownAgents::default(),
            sign_ins: Arc::default(),
            account_facts: crate::accounts::Facts::default(),
            accounts_tx: broadcast::channel(16).0,
        })
    }

    /// The agents this daemon can actually host, in one place so every surface that
    /// advertises them reads the same list. Claude is always the floor, and a daemon
    /// never advertises an agent it cannot drive.
    ///
    /// What this does **not** open is the shared approval/text ledgers: see
    /// [`Daemon::shared_ledgers_admit`], which used to be defined in terms of
    /// this list and no longer is, precisely so that admitting the agent and
    /// splitting those four tables stay separate decisions.
    pub fn supported_agents(&self) -> Vec<protocol::agent::AgentKind> {
        vec![
            protocol::agent::AgentKind::Claude,
            protocol::agent::AgentKind::Codex,
            protocol::agent::AgentKind::Antigravity,
        ]
    }

    /// Whether a run hosted by `agent` may put a row in the tables that are
    /// still shared between agents: `text_mutations`, `answer_claims`,
    /// `answers`.
    ///
    /// **There were four, and `pending_approvals` left.** It was split into
    /// `codex_pending_approvals` the day it got a Codex producer — the approval
    /// observer — because a table split ahead of its producer is the speculative
    /// half-surface this design refuses, and a table left shared *behind* one is
    /// the rollback hazard the whole split exists to close. The three that
    /// remain have no Codex producer, so they are still shared and this refusal
    /// is still what keeps them clean.
    ///
    /// **Temporary scaffolding, and the reason it exists is a rollback.** Sessions
    /// were split by agent so a rolled-back v0.6.0 daemon cannot touch a Codex
    /// run; these were deliberately left shared, because that daemon reads
    /// them *globally* rather than by walking a `sessions` row and no split would
    /// have hidden them anyway. What it does with them is not passive:
    /// `recover_text_mutations` rewrites every `applying` row it finds to
    /// `indeterminate`, and its approval recovery deletes `answer_claims` and the
    /// pending cards that match them — for runs it has no other way of seeing.
    /// The justification for leaving them shared was that no Codex row could
    /// reach them, and that holds only while the entry producers refuse one:
    /// [`Daemon::send_text`] and [`Daemon::answer`]. This is that refusal, in one
    /// place, so the two read as one decision rather than two coincidences.
    ///
    /// The phase that splits the remaining tables per agent is what deletes this
    /// method and its two call sites.
    ///
    /// **This was `supported_agents().contains(agent)`, and the day that stopped
    /// being right has arrived.** The coupling was deliberate and it was a
    /// tripwire: the `..._is_refused_before_...` tests name
    /// `AgentKind::Codex` outright so that admitting the agent would turn them
    /// red rather than quietly opening these tables to a Codex run. It did.
    /// The decision it forced is recorded here: **the refusal stays, and the
    /// coupling goes.** Hosting a Codex session — listing it, observing its
    /// turns, reporting its exit — is what this build gained; writing Codex
    /// text mutations and Codex answers into tables a rolled-back v0.6.0
    /// daemon rewrites and deletes globally is not, and the phase that splits
    /// them is where the change that makes it safe lives.
    ///
    /// So the question is now asked of the value, not of the list — the one place
    /// in this daemon where that is the right question, because what is being
    /// asked is not "can this build drive the agent" but "is this run's state
    /// safe in a table the previous build sweeps". Only the agent that predates
    /// the split is, and it is the *only* agent a v0.6.0 daemon can even name.
    /// The tests continue to drive the real producers against a real Codex
    /// row, begun as the engine begins one, so the refusal stays proven at the
    /// place a later change would have to remove it.
    ///
    /// **Asking this of a row is only sound because the answer cannot change
    /// under the asker.** Each producer reads the row's agent and
    /// then does its durable write later — `send_text` in the same breath,
    /// `answer` five awaits later. A run's agent is filed once, when the run
    /// begins, and nothing writes it again.
    ///
    /// An Antigravity run is admitted: it is a hosted run as a Claude run is, its row in
    /// `sessions` beside theirs (`store::session_tables_for`), so a v0.6.0 daemon that sweeps
    /// its texts and answers already sweeps the row they belong to.
    fn shared_ledgers_admit(&self, agent: &protocol::agent::AgentKind) -> bool {
        matches!(
            agent,
            protocol::agent::AgentKind::Claude | protocol::agent::AgentKind::Antigravity
        )
    }

    /// Re-derive from the database everything a restart would otherwise lose.
    ///
    /// Two things, both of which used to be memory-only:
    ///
    /// 1. **Open approval cards.** A card is a durable fact in the log and a
    ///    live question on somebody's phone; after a restart the daemon used to
    ///    answer "unknown or already-resolved request" to a tap on a card that
    ///    was still on screen. A hosted run's card is answered again once its host
    ///    re-attaches ([`Daemon::host_cards_reconcile`]), and an engine Codex run's
    ///    once Codex asks for it again.
    /// 2. **Mutations that were claimed but never settled.** A claim with no
    ///    outcome means the daemon died between claiming and recording. That is
    ///    recorded as a terminal *indeterminate* result, and never retried.
    pub async fn recover(&self) {
        match self.db.unresolved_answer_claims().await {
            Ok(claims) if !claims.is_empty() => {
                // A hosted run's answer is settled by its host: the replay says
                // whether it reached claude, and a request the host still holds
                // never had it ([`Daemon::host_cards_reconcile`]).
                let mut settled = 0;
                for claim in claims {
                    if let Ok(Some(_)) = self.db.hosted_link(claim.session_uid.clone()).await {
                        continue;
                    }
                    self.settle_indeterminate(&claim).await;
                    settled += 1;
                }
                if settled > 0 {
                    crate::log_warn!(
                        "recovery: {settled} answer(s) were claimed but never settled; recording \
                         them as indeterminate rather than sending again"
                    );
                }
            }
            Ok(_) => {}
            Err(err) => crate::log_error!("recovery: could not read answer claims: {err:#}"),
        }

        match self.db.recover_mail().await {
            Ok(0) => {}
            Ok(count) => crate::log_info!(
                "recovery: {count} message(s) from other agents were being read when the daemon \
                 stopped; they are unread again"
            ),
            Err(err) => crate::log_error!("recovery: could not release offered mail: {err:#}"),
        }

        // Cards of runs that had already ended, left by a daemon that did not close
        // them. Closed last, once any answer in flight on one has been settled.
        let mut of_ended_runs: Vec<(SessionKey, String)> = Vec::new();
        match self.db.list_pending_approvals().await {
            Ok(rows) if !rows.is_empty() => {
                // Resolved before the lock, because naming a run reads the
                // database and the lock below admits no awaits.
                let mut labels: HashMap<String, String> = HashMap::new();
                // The card's agent, resolved here for the same reason the label
                // is: `blocked_runs` reads it under a lock that admits no awaits.
                // A row whose session has since gone is dropped rather than
                // guessed — a recovered card counted under the wrong agent would
                // be a doorbell authorized to devices that cannot open it, which
                // is the failure the agent-scoped count exists to prevent.
                let mut agents: HashMap<String, protocol::agent::AgentKind> = HashMap::new();
                let mut ended: HashSet<String> = HashSet::new();
                // Runs started in an agent host: their cards are the host's, held
                // across the restart, and its link settles them once it is back.
                let mut hosted: HashSet<String> = HashSet::new();
                for row in &rows {
                    if !labels.contains_key(&row.session_uid) {
                        let label = self.effective_project_label(&row.session_uid).await;
                        labels.insert(row.session_uid.clone(), label);
                    }
                    if !agents.contains_key(&row.session_uid) {
                        if let Ok(Some(session)) =
                            self.db.get_session(row.session_uid.clone()).await
                        {
                            if session.lifecycle == Lifecycle::Exited {
                                ended.insert(row.session_uid.clone());
                            }
                            agents.insert(row.session_uid.clone(), session.agent);
                        }
                        if let Ok(Some(_)) = self.db.hosted_link(row.session_uid.clone()).await {
                            hosted.insert(row.session_uid.clone());
                        }
                    }
                }

                let mut inner = self.inner.lock().await;
                let mut restored = 0usize;
                for row in rows {
                    let Ok(card) = serde_json::from_str::<ApprovalCard>(&row.card) else {
                        crate::log_error!(
                            "recovery: dropping an undecodable pending card for {}",
                            row.request_id
                        );
                        continue;
                    };
                    let Some(agent) = agents.get(&row.session_uid).cloned() else {
                        crate::log_error!(
                            "recovery: dropping a pending card for {} whose run is gone; its \
                             agent cannot be known, and a card counted under the wrong one \
                             rings phones that cannot open it",
                            row.session_uid
                        );
                        continue;
                    };
                    // A Codex run started on the engine has a hosted row too, for its
                    // app-server; its cards are Codex cards, never an agent host's.
                    let link = agent.is_claude() && hosted.contains(&row.session_uid);
                    let question = card.tool_name == protocol::host::ASK_USER_QUESTION;
                    if ended.contains(&row.session_uid) {
                        of_ended_runs.push((
                            SessionKey::new(&row.session_uid, &row.session_id),
                            row.request_id.clone(),
                        ));
                    }
                    inner.pending.insert(
                        (row.session_uid.clone(), row.request_id.clone()),
                        PendingApproval {
                            card,
                            session: SessionKey::new(&row.session_uid, &row.session_id),
                            project_label: labels
                                .get(&row.session_uid)
                                .cloned()
                                .unwrap_or_default(),
                            agent,
                            raise: 0,
                            claimed: false,
                            tool_ran: false,
                            // Not recorded with the card: a host's card has its
                            // `fork` from the host's `state` when its link is back.
                            fork: false,
                            link,
                            question,
                        },
                    );
                    restored += 1;
                }
                drop(inner);
                crate::log_info!(
                    "recovery: {restored} approval card(s) restored; a hosted run's can be \
                     answered once its host is back, and an engine Codex run's once Codex asks \
                     for it again"
                );
            }
            Ok(_) => {}
            Err(err) => crate::log_error!("recovery: could not read pending approvals: {err:#}"),
        }
        // Nobody is left to answer these.
        for (session, request_id) in of_ended_runs {
            self.close_card_of_ended_run(&session, &request_id).await;
        }
        // An interrupt and a compose have no card: there is nothing beside their ledger
        // rows to retire, and nothing the card read above could make this get wrong.
        self.recover_codex_interrupts().await;
        self.recover_codex_composes().await;
        self.recover_session_facts().await;
    }

    /// Rebuild every run's summary facts from the newest event that decided each,
    /// so a run that has been quiet since the last start reports them too. Every
    /// ingest keeps them current from here on.
    async fn recover_session_facts(&self) {
        let rows = match self.db.list_sessions().await {
            Ok(rows) => rows,
            Err(err) => {
                crate::log_error!(
                    "recovery: could not list sessions to rebuild their facts: {err:#}"
                );
                return;
            }
        };
        let mut rebuilt = Vec::with_capacity(rows.len());
        for row in rows {
            if row.lifecycle == Lifecycle::Live {
                self.refresh_tree(&row.session_uid);
            }
            match self.db.session_facts(row.session_uid.clone()).await {
                Ok(facts) => rebuilt.push((row.session_uid, facts)),
                Err(err) => crate::log_error!(
                    "recovery: could not rebuild the facts of {}: {err:#}",
                    row.session_uid
                ),
            }
        }
        self.inner.lock().await.session_facts.extend(rebuilt);
    }

    /// Record a claimed-but-unsettled answer as a terminal unknown: one a
    /// stopped daemon left behind, or one whose card closed before its host
    /// confirmed it.
    async fn settle_indeterminate(&self, claim: &AnswerClaim) {
        let session = SessionKey::new(&claim.session_uid, &claim.session_id);
        let decision =
            serde_json::from_str::<AnswerDecision>(&claim.decision).unwrap_or(AnswerDecision::Deny);
        // Every answer this build claims is a hosted Claude run's, returned through
        // its agent host. `recover` also settles a claim whose run's host link it
        // could not read, a store error only: that claim is settled here rather than
        // by its host's replay, along the same path.
        let outcome = AnswerOutcome {
            request_id: claim.request_id.clone(),
            session_id: session.name.clone(),
            decision,
            resolved_by: ResolvedBy::Phone,
            answered_on: None,
            applied_via: AnswerPath::HookReturn,
            resolved_at: protocol::time::now_rfc3339(),
            detail: Some(format!(
                "the daemon stopped after claiming this answer and before recording what \
                 became of it (claimed at {}); whether it was handed to Claude's permission \
                 hook, and whether Claude took it, was never observed, and it will not be \
                 sent again",
                claim.started_at
            )),
            indeterminate: true,
        };
        if let Err(err) = self
            .db
            .record_answer(
                claim.session_uid.clone(),
                claim.request_id.clone(),
                claim.payload_hash.clone(),
                outcome.clone(),
            )
            .await
        {
            crate::log_error!(
                "recovery: could not record the indeterminate outcome for {}: {err:#}",
                claim.request_id
            );
            return;
        }
        let _ = self
            .store
            .delete_pending_approval(&claim.session_uid, &claim.request_id);
        let pending = PendingEvent::new(
            &session,
            EventKind::ApprovalResolved,
            serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null),
            Source::Daemon,
        )
        .with_source_event_id(format!("resolved:{}", claim.request_id));
        if let Err(err) = self.ingest(pending).await {
            crate::log_error!("recovery: could not record the resolution event: {err:#}");
        }
    }

    /// The daemon's own account of itself, for `codeconnect daemon status` and for
    /// `codeconnect daemon install` deciding whether it is about to fight a process
    /// somebody started by hand.
    pub async fn info(&self) -> protocol::ipc::DaemonInfo {
        protocol::ipc::DaemonInfo {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: protocol::PROTOCOL_VERSION,
            protocol_minor: protocol::PROTOCOL_MINOR,
            started_at: self.started_at.clone(),
            launchd_label: self.launchd_label.clone(),
            endpoint_host: self.endpoint.host.clone(),
            endpoint_port: self.endpoint.port,
            tls: self.endpoint.tls,
            bind_ip: self.bind_ip.get().cloned(),
            exe_path: self.exe_identity.get().map(|(path, _)| path.clone()),
            exe_sha: self.exe_identity.get().map(|(_, sha)| sha.clone()),
            build_id: {
                let identity = protocol::build_identity::installed();
                identity
                    .short
                    .is_some()
                    .then(protocol::build_identity::build_tag)
            },
        }
    }

    // ---------------------------------------------------------------- ingest

    /// The gate that makes assignment order and publication order the same.
    ///
    /// One `tokio::sync::Mutex` per run, dropped once nothing else holds it so a
    /// long-lived daemon does not accumulate one per session it ever saw.
    async fn publish_gate(&self, session_uid: &str) -> Arc<Mutex<()>> {
        let mut gates = self.publish_gates.lock().await;
        gates.retain(|_, gate| Arc::strong_count(gate) > 1);
        Arc::clone(gates.entry(session_uid.to_string()).or_default())
    }

    /// `text` as a run's agent is sent it: led, when commits were made from the run's Changes
    /// panel since it was last told, by one line for each (the newest
    /// [`crate::store::MAX_COMMIT_LINES`], oldest first, in the panel's words), and the seq of
    /// the newest notice that carries, to pass to [`Daemon::commits_told`] once it is sent.
    pub(crate) async fn with_commit_note(
        &self,
        session_uid: &str,
        text: &str,
    ) -> (String, Option<u64>) {
        let commits = match self.db.untold_commits(session_uid.to_string()).await {
            Ok(commits) => commits,
            Err(err) => {
                crate::log_error!(
                    "{session_uid}: could not read the commits to tell its agent: {err:#}"
                );
                Vec::new()
            }
        };
        let Some(&(through, _, _)) = commits.last() else {
            return (text.to_string(), None);
        };
        let lines: Vec<String> = commits
            .iter()
            .map(|(_, oid, files)| {
                let files = if *files == 1 {
                    "1 file".to_string()
                } else {
                    format!("{files} files")
                };
                format!("You committed {files} · {}", oid.get(..7).unwrap_or(oid))
            })
            .collect();
        (format!("{}\n\n{text}", lines.join("\n")), Some(through))
    }

    /// The message `request_id` told the run's agent of the commits through `through`
    /// ([`Daemon::with_commit_note`]); a Stop that drops it later takes that back.
    pub(crate) async fn commits_told(
        &self,
        session_uid: &str,
        request_id: &str,
        through: Option<u64>,
    ) {
        let Some(through) = through else { return };
        if let Err(err) = self
            .db
            .commits_told(session_uid.to_string(), request_id.to_string(), through)
            .await
        {
            crate::log_error!(
                "{session_uid}: could not record the commits its agent was told: {err:#}"
            );
        }
    }

    /// Whether the run `row` may be writing to its folder: a card open, or, until it has
    /// ended, a turn running. Starting or stopping, it may be; live, a Codex run's turn is as
    /// its link says (a resumed turn's items are not filed, so the log cannot) and a Claude
    /// run's as its log says; any other live run, and a Codex run not yet linked, cannot be
    /// told and may be.
    async fn may_be_working(&self, row: &crate::store::SessionRow) -> bool {
        let (card, link) = {
            let inner = self.inner.lock().await;
            let card = inner.pending.keys().any(|(uid, _)| *uid == row.session_uid);
            let link = match inner.host_links.get(&row.session_uid) {
                Some(HostLink::Codex(_, commands)) => Some(commands.clone()),
                _ => None,
            };
            (card, link)
        };
        if card {
            return true;
        }
        match row.lifecycle {
            Lifecycle::Starting | Lifecycle::Stopping => true,
            Lifecycle::Live => match (&row.agent, link) {
                (protocol::agent::AgentKind::Codex, Some(commands)) => {
                    let (reply, told) = tokio::sync::oneshot::channel();
                    commands
                        .send(crate::codex_host::LinkCommand::TurnRunning(reply))
                        .is_err()
                        || told.await.unwrap_or(true)
                }
                (protocol::agent::AgentKind::Claude, _) => self
                    .db
                    .turn_running(row.session_uid.clone())
                    .await
                    .unwrap_or(true),
                _ => true,
            },
            _ => false,
        }
    }

    /// The run folder `cwd`'s git gate. Every reader of a run's repository
    /// ([`Daemon::diff`], [`Daemon::run_git`], the session list's refresh) and every action on
    /// it ([`Daemon::git_action`]) holds it for all its git children: a read refreshes the
    /// index, a commit writes it, and none may meet another; a read answered before an
    /// action's result was read before it. Same eviction rule as [`Daemon::publish_gate`].
    async fn git_gate(&self, cwd: &str) -> Arc<Mutex<()>> {
        git_gate_in(&self.git_gates, cwd).await
    }

    /// The run `session_uid`'s move gate (306): a person's message to a run due to move to another account decides
    /// whether it moves and sends or relaunches with it as one step under it, from before any read of the run's
    /// switch, lifecycle or link; a mail wake holds it from its claim through its issue, so none slips between the
    /// decision and the stop. Same eviction rule as [`Daemon::publish_gate`].
    pub(crate) async fn move_gate(&self, session_uid: &str) -> Arc<Mutex<()>> {
        let mut gates = self.move_gates.lock().await;
        gates.retain(|_, gate| Arc::strong_count(gate) > 1);
        Arc::clone(gates.entry(session_uid.to_string()).or_default())
    }

    /// The run `session_uid`'s move gate, held while the run has a switch, read under it: a person's message to such a
    /// run is decided and sent under it ([`crate::switch::moves`]), and a settings change, an interrupt or a Stop to it
    /// is delivered under it, so no move ends the process it is on its way to. `None`, the gate let go, for a run
    /// with none, which is sent to as before, a message at a time no more than before.
    pub(crate) async fn move_guard(
        &self,
        session_uid: &str,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        let guard = self.move_gate(session_uid).await.lock_owned().await;
        matches!(
            self.db.switch_state(session_uid.to_string()).await,
            Ok(Some(_))
        )
        .then_some(guard)
    }

    /// The gate serializing one session's recorded end ([`Daemon::end_hosted_run`]).
    /// Same eviction rule as [`Daemon::publish_gate`]: an entry nobody holds is
    /// dropped, so this map is bounded by live sessions rather than by every session
    /// ever seen.
    pub(crate) async fn registration_gate(&self, session_uid: &str) -> Arc<Mutex<()>> {
        let mut gates = self.registration_gates.lock().await;
        gates.retain(|_, gate| Arc::strong_count(gate) > 1);
        Arc::clone(gates.entry(session_uid.to_string()).or_default())
    }

    /// The gate serializing everything that answers one approval. Bounded:
    /// entries are dropped once nothing else holds them, so a long-lived daemon
    /// does not accumulate one lock per approval ever shown.
    async fn answer_gate(&self, id: &ApprovalId) -> Arc<Mutex<()>> {
        let mut inner = self.inner.lock().await;
        inner
            .answer_locks
            .retain(|_, lock| Arc::strong_count(lock) > 1);
        Arc::clone(inner.answer_locks.entry(id.clone()).or_default())
    }

    /// The gate a phone answer to a Codex card holds the read side of; see
    /// [`Daemon::actuation_gates`].
    ///
    /// Same eviction rule as its two siblings: an entry nobody holds is dropped, so
    /// the map is bounded by live sessions rather than by every session ever seen.
    async fn actuation_gate(&self, session_uid: &str) -> Arc<tokio::sync::RwLock<()>> {
        let mut gates = self.actuation_gates.lock().await;
        gates.retain(|_, gate| Arc::strong_count(gate) > 1);
        Arc::clone(gates.entry(session_uid.to_string()).or_default())
    }

    /// Persist a fact and fan it out. Returns `None` if it was a duplicate.
    pub async fn ingest(&self, pending: PendingEvent) -> Result<Option<Event>> {
        let mut pending = pending;
        self.truncate_payload(&mut pending);
        let session_uid = pending.session_uid.clone();

        let gate = self.publish_gate(&session_uid).await;
        let event = {
            let _ordered = gate.lock().await;
            let event = self.db.append_event(pending.clone()).await?;
            if let Some(event) = &event {
                // Published inside the gate, so no later seq can overtake this
                // one on the way to a socket. A send error only means nobody is
                // subscribed; the log already has it.
                let _ = self.events_tx.send(event.clone());
            }
            event
        };

        if let Some(event) = &event {
            self.published(&session_uid, std::slice::from_ref(event))
                .await;
        }
        Ok(event)
    }

    /// Persist one message from a run's agent host, message `seq` from the host
    /// `host`, with its cursor ([`crate::store::Store::append_host_message`]), then
    /// fan out what was filed, and return it. `None` when the cursor did not move:
    /// nothing was written, and the message must not be acknowledged.
    pub async fn ingest_host_message(
        &self,
        session_uid: &str,
        host: protocol::proc_identity::ProcessIdentity,
        seq: u64,
        pendings: Vec<PendingEvent>,
        notes: crate::store::RunNotes,
    ) -> Result<Option<Vec<Event>>> {
        let mut pendings = pendings;
        for pending in &mut pendings {
            self.truncate_payload(pending);
        }
        let gate = self.publish_gate(session_uid).await;
        let events = {
            // Held across the commit and the broadcast, as in `ingest`.
            let _ordered = gate.lock().await;
            let Some(events) = self
                .db
                .append_host_message(session_uid.to_string(), host, seq, pendings, notes)
                .await?
            else {
                return Ok(None);
            };
            for event in &events {
                let _ = self.events_tx.send(event.clone());
            }
            events
        };
        self.published(session_uid, &events).await;
        Ok(Some(events))
    }

    /// What follows filing a run's events, once they are broadcast: its facts, the
    /// calls their results end, and its tree after a turn.
    async fn published(&self, session_uid: &str, events: &[Event]) {
        if events.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().await;
        for event in events {
            inner
                .session_facts
                .entry(session_uid.to_string())
                .or_default()
                .observe(event);
            note_tool_result(&mut inner, event);
        }
        drop(inner);
        if events
            .iter()
            .any(|event| event.kind == EventKind::TurnComplete)
        {
            self.refresh_tree(session_uid);
        }
    }

    /// Cap a single payload so one enormous tool response cannot bloat the log
    /// or blow past a WebSocket frame limit. The fact survives; only the bulk is
    /// dropped, and the truncation is recorded rather than hidden.
    ///
    /// The cut lands on a character boundary. `String::truncate` **panics** on
    /// any other byte, and a tool response is exactly the kind of payload that
    /// carries multi-byte text — a path with an accent, a diff with an emoji —
    /// so the naive cut was a panic waiting for the right file name.
    ///
    /// **The limit holds for the replacement as written.** The preview is JSON
    /// inside JSON, so every quote and backslash in it is escaped again: cut at
    /// the limit's byte count, a backslash-heavy payload's replacement was
    /// 1,048,622 bytes against the default 524,288.
    ///
    /// **A user line keeps everything but its attachments.** Replaced whole, a
    /// typed prompt with a large pasted image lost `promptSource`, `origin`,
    /// `uuid` and its words, so the phone drew no row for it and read the
    /// session as done while Claude worked on it — 6 such prompts in a real
    /// history, the largest line 817,592 bytes. Any user line over the limit
    /// whose content is blocks — measured, a typed prompt or an `isMeta` line;
    /// transcript-only, task and compaction lines are plain text — keeps its fields
    /// and its text, every other block cut to its `type`, with the same two
    /// markers. If that still does not fit, or the
    /// content is a bare string, it is replaced like any payload. A tool's
    /// result is still replaced: the phone says its output was too large, where
    /// a result cut to its blocks' names would read as a tool that printed
    /// nothing.
    fn truncate_payload(&self, pending: &mut PendingEvent) {
        let encoded = pending.payload.to_string();
        // Measured before the cut, not after: the whole point of recording it is
        // to say how much was dropped, and the truncated length says nothing.
        let original_bytes = encoded.len();
        if original_bytes <= self.config.max_payload_bytes {
            return;
        }
        if pending.kind == EventKind::UserMessage {
            if let Some(mut prompt) = Self::without_attachments(&pending.payload) {
                prompt["_codeconnect_truncated"] = serde_json::json!(true);
                prompt["_original_bytes"] = serde_json::json!(original_bytes);
                if prompt.to_string().len() <= self.config.max_payload_bytes {
                    pending.payload = prompt;
                    return;
                }
            }
        }
        let replacement = |preview: &str| {
            serde_json::json!({
                "_codeconnect_truncated": true,
                "_original_bytes": original_bytes,
                "_preview": preview,
            })
        };
        // `encoded` is JSON, which holds no raw control character: written again
        // inside the replacement, a quote or a backslash takes two bytes and
        // anything else its own, so half the room always fits.
        let mut end = (self.config.max_payload_bytes - replacement("").to_string().len()) / 2;
        while !encoded.is_char_boundary(end) {
            end -= 1;
        }
        pending.payload = replacement(&encoded[..end]);
    }

    /// A user line with its content blocks other than text cut to their `type`,
    /// or `None` for a line whose content is not blocks. Every such block in a
    /// real history (34 of 34) is a pasted image, `{type, source}`, and its
    /// `source` is the image.
    fn without_attachments(line: &serde_json::Value) -> Option<serde_json::Value> {
        let mut prompt = line.clone();
        let blocks = prompt
            .get_mut("message")?
            .get_mut("content")?
            .as_array_mut()?;
        for block in blocks.iter_mut().filter_map(|block| block.as_object_mut()) {
            if block.get("type").and_then(|kind| kind.as_str()) != Some("text") {
                block.retain(|key, _| key == "type");
            }
        }
        Some(prompt)
    }

    /// The run that asked this card has ended, so nobody is left to answer it, and
    /// the card closes. An answer already in flight on it finishes first.
    ///
    /// Either agent's card: a Claude card is its run's host's, and closes as one; a
    /// Codex card is retired in its own store, the way the frames that settle it retire
    /// it, as a turn that went away.
    async fn close_card_of_ended_run(&self, session: &SessionKey, request_id: &str) {
        let id: ApprovalId = (session.uid.clone(), request_id.to_string());
        let gate = self.answer_gate(&id).await;
        let _serialised = gate.lock().await;
        let claude = match self.inner.lock().await.pending.get_mut(&id) {
            Some(entry) => entry.agent.is_claude(),
            None => return,
        };
        // A Claude card was its run's host's: nobody answered it, and nothing was
        // typed.
        if claude {
            self.inner.lock().await.pending.remove(&id);
            self.close_host_card(&session.uid, request_id).await;
            return;
        }
        self.retire_codex_approval(
            session,
            request_id,
            protocol::ws::CodexResolution::Cleared {
                cause: protocol::ws::ClearCause::TurnAborted,
            },
        )
        .await;
    }

    /// Ring the phones that have not seen `trigger_seq`, after a short grace.
    ///
    /// **It rings only for a decision the phone can answer.** Whatever admitted
    /// it — a card filed, a run going idle or finishing, a permission notice —
    /// at the moment it would ring it counts the open cards the phone can
    /// answer ([`phone_can_answer`]), fleet-wide. None, and it does not ring: a
    /// tap would open onto nothing to answer. Some, and the doorbell is about
    /// them ([`PushHint::for_decisions`]), not about the event that admitted it.
    ///
    /// The grace closes a real race: `ingest` broadcasts the event, the socket
    /// tasks write it and record delivery — and this push races those writes.
    /// Dispatching immediately would ring a phone that is milliseconds from
    /// receiving the fact on its live socket. The exclusion list is computed
    /// *after* the pause, at send time, from what was actually written.
    ///
    /// The grace also opens a window the world can move through, so the push
    /// carries the `ticket` its admission minted and re-validates it on
    /// waking — atomically with the exclusion snapshot, so a deletion or
    /// prune is seen whole or not at all, and (for quiet-state pushes,
    /// `heed_progress`) a turn starting after admission means the announced
    /// wait is over and the push dies.
    ///
    /// **Best-effort exactly-once, explicitly.** A socket write that
    /// completes in the sliver after the exclusion snapshot can produce one
    /// duplicate doorbell if the user backgrounds the app at that same
    /// instant; while it stays foregrounded the app declines to present the
    /// banner. Closing the sliver itself needs client delivery
    /// acknowledgements — a protocol change deliberately not taken for a
    /// doorbell whose whole content is a few words and a count.
    ///
    /// # Held while someone is at the Mac
    ///
    /// The phone is for when the owner is away. After the grace, a ring on a Mac
    /// that has had keyboard or mouse input within [`crate::presence::RING_IDLE`]
    /// waits, reading the idle time again when the Mac could first count as left
    /// ([`crate::presence::until_away`]), at least [`crate::presence::POLL`] and at most a
    /// second later, and rings the first time the Mac has been left that long. While it
    /// waits it asks once a
    /// second whether it still matters — the ticket, the card, both in memory —
    /// and ends if not; before it goes it asks everything, the database
    /// included, so what rings is still true when it rings. The hold lives in this task alone, as the gate lives in
    /// memory alone: a daemon restart drops a held ring.
    ///
    /// The cadence is measured, not guessed: process CPU, release build, someone
    /// at the Mac, three 5 s windows. Re-checking everything each poll cost 1.1–1.7
    /// % of a core for 10 held rings and 15.7–18.5 % for 100; reading only the
    /// idle time each poll costs 0.06 % and 0.15–0.30 %, plus the HID read itself,
    /// 10.5 µs (20,000 reads): about 0.01 % of a core per held ring. Reading it only when the Mac
    /// could first count as left takes a held ring's daemon from about 21 context switches a second
    /// to about 3 (release build, someone at the Mac, four 60 s windows each).
    ///
    /// # The grace is also a best-effort CANCELLATION window, and it is not
    /// coordinated with anything
    ///
    /// A cancel that arrives after [`crate::push_gate::GRACE`] has elapsed, with nobody at the Mac,
    /// does not stop the push: the task has already woken, taken its exclusion
    /// snapshot and sent. Nothing waits for a cancel that might be in flight, and
    /// nothing is going to — the failure direction is one stale doorbell, and the
    /// coordination that would close it (making the dispatcher wait on a signal
    /// from a control link that may not exist) trades a bounded, occasional
    /// annoyance for a completion that can be held up indefinitely.
    ///
    /// What can hold a cancel back is the link it comes from: an engine run's link
    /// ([`crate::codex_host`]) reads its frames in order, so a `turn/started` queued
    /// behind the follow-up resume of a turn a reconnect found running is read once
    /// that resume's answer is filed. If a resume answer large enough to outlast the
    /// grace is ever observed, the fix is to bound the recovery, not to build a
    /// rendezvous around the doorbell.
    fn dispatch_push(
        self: &Arc<Self>,
        session_uid: String,
        trigger_seq: u64,
        ticket: crate::push_gate::Ticket,
        heed_progress: bool,
        needs_card: Option<CardKey>,
    ) {
        const LOOK: Duration = Duration::from_secs(1);
        let grace = self.push_gate.grace();
        let gate = Arc::clone(&self.push_gate);
        let sender = Arc::clone(&self.push);
        let inner = Arc::clone(&self.inner);
        let db = self.db.clone();
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            grace.await;
            let mut looked: Option<tokio::time::Instant> = None;
            loop {
                // **The phone is for when the owner is away.** Someone who has
                // used the Mac within `RING_IDLE` is there to see it, so the ring
                // waits for them to leave, reading only the HID idle time each
                // poll. Whether it still matters — the ticket, the card — is
                // asked once a `LOOK`, so a ring that stopped mattering ends
                // within one, and everything is asked again before it goes. An
                // idle time that cannot be read rings, as it did before this
                // check existed: a read that fails for good must not silence
                // every notification for good.
                if let Some(left) = crate::presence::until_away(crate::presence::RING_IDLE) {
                    if looked.is_none_or(|at| at.elapsed() >= LOOK) {
                        if looked.is_none() {
                            crate::log_debug!(
                                "push for {} held: someone is at this computer",
                                session_uid
                            );
                        }
                        let stands = gate
                            .exclusions_if_valid(&session_uid, ticket, heed_progress, trigger_seq)
                            .is_some()
                            && match &needs_card {
                                Some(key) => card_is_open(&*inner.lock().await, &session_uid, key),
                                None => true,
                            };
                        if !stands {
                            crate::log_debug!(
                                "push for {} dropped: it stopped mattering while it was held",
                                session_uid
                            );
                            return;
                        }
                        looked = Some(tokio::time::Instant::now());
                    }
                    // Nobody can count as gone before `left`; whether the ring still matters is
                    // asked once a `LOOK`.
                    tokio::time::sleep(left.clamp(crate::presence::POLL, LOOK)).await;
                    continue;
                }
                // **The run still has to exist.** The ticket catches a session
                // evicted *after* this push was admitted, but not one evicted
                // just before: an admission already under way mints its ticket
                // against the epoch the eviction had already bumped. Asked here, the question has one answer for
                // both — a doorbell for a run nobody can open is never rung.
                //
                // A database that will not answer is not an answer: the push goes
                // out, because staying silent about a real decision is the worse
                // of the two mistakes.
                //
                // And for a quiet state, still be running: a run that has ended
                // waits for nobody, whatever a notification that reached the
                // daemon around its end says.
                match db.get_session(session_uid.clone()).await {
                    Ok(None) => {
                        crate::log_debug!(
                            "push for {} dropped: the run was removed before the doorbell rang",
                            session_uid
                        );
                        return;
                    }
                    Ok(Some(row)) if heed_progress && row.lifecycle == Lifecycle::Exited => {
                        crate::log_debug!(
                            "push for {} dropped: the run ended before the doorbell rang",
                            session_uid
                        );
                        return;
                    }
                    Err(err) => crate::log_warn!(
                        "push for {session_uid}: could not confirm the run still exists: {err:#}"
                    ),
                    Ok(Some(_)) => {}
                }
                // Count and hand-off as one ordered step: see `Daemon::ring_order`.
                let _in_count_order = daemon.ring_order.lock().await;
                // **Asked after the wait for the ordering step**, which can be as
                // long as another doorbell's send: the owner may be back, and the
                // run may have moved on or reached the phone over its socket.
                if crate::presence::at_the_mac(crate::presence::RING_IDLE).unwrap_or(false) {
                    continue;
                }
                let Some(excluded) =
                    gate.exclusions_if_valid(&session_uid, ticket, heed_progress, trigger_seq)
                else {
                    crate::log_debug!(
                        "push for {} dropped: superseded before it rang",
                        session_uid
                    );
                    return;
                };
                let waiting = {
                    let held = inner.lock().await;
                    if let Some(key) = &needs_card {
                        if !card_is_open(&held, &session_uid, key) {
                            crate::log_debug!(
                                "push for {} dropped: its card was answered before the doorbell rang",
                                session_uid
                            );
                            return;
                        }
                    }
                    Waiting::of(&held)
                };
                // **The one rule.** Nothing the phone can answer is open, so a tap
                // would open onto nothing: whatever admitted this push, it does not
                // ring.
                if waiting.decisions == 0 {
                    crate::log_debug!(
                        "push for {} dropped: nothing the phone can answer is open",
                        session_uid
                    );
                    return;
                }
                let described = PushHint::for_decisions(
                    session_uid,
                    waiting.decisions,
                    waiting.label,
                    waiting.question,
                );
                // **The exclusion list is one thing: who already knows.** It carries
                // the devices whose live socket wrote this event to them during the
                // grace, and nothing else — a doorbell must not ring a phone that is
                // already looking at the fact.
                sender.send(&described, &excluded);
                return;
            }
        });
    }

    /// **A turn is running, which is the run demonstrably moving.**
    ///
    /// The movement is read off the wire: a Codex run's `turn/started`, in its link
    /// ([`crate::codex_host`]), and a Claude run CodeConnect started by the `system/init`
    /// that begins each of its turns ([`crate::claude_host`]'s `apply_sdk`).
    ///
    /// **What it buys is the cancellation, not a notification.** A turn ending
    /// admits a quiet-state doorbell that then waits out its dispatch grace; if the
    /// next turn *starts* inside that window, the ticket describes a run that is
    /// working again, and the ring would announce a completion the reader can no
    /// longer act on. Without this, only a second turn *ending* inside the grace
    /// cancelled the first — a turn that merely started did not, and the doorbell
    /// went out while the agent was mid-turn.
    ///
    /// A resume is not read as movement: the turn a Codex run's resume answer reports
    /// still running is owed its follow-up resume ([`crate::codex_host`]), and the
    /// resume's filings neither cancel nor ring ([`Daemon::push_turn_complete`]).
    pub(crate) fn note_turn_running(&self, session_uid: &str) {
        self.push_gate.note_progress(session_uid);
    }

    /// **Raise a Codex approval card, through the machinery that already raises
    /// Claude's.**
    ///
    /// The same `ApprovalCard`, the same `inner.pending` map keyed by
    /// `(session_uid, request_id)`, the same `EventKind::ApprovalRequest` event,
    /// the same `PushKind::Approval` doorbell. Four things differ, and each is
    /// a measured difference rather than a style choice:
    ///
    ///   * the card goes to `codex_pending_approvals`, because a rolled-back
    ///     v0.6.0 daemon enumerates and deletes rows in the shared table without
    ///     ever walking a session row (see `Store::create_schema`);
    ///   * `agent` is Codex on the card;
    ///   * there is no responder and no prompt fingerprint. Nothing is blocked
    ///     on this daemon's answer — the human at the TUI is being asked — and
    ///     there is no pane this daemon can type into, which is why
    ///     `identity_bound` is false and stays false;
    ///   * the row and the event are **one commit**. A Codex card
    ///     has no responder and nothing waiting: durability is the entire thing
    ///     it is for, and a half-written one is either a fact with no card or a
    ///     card with no fact. See [`crate::store::Store::raise_codex_pending_approval`].
    ///
    /// Returns whether a card is now open for this request, so a caller can
    /// remember it. A re-delivery of an approval already carded answers `true`
    /// without ringing a second time: the event dedups on its
    /// `source_event_id`, and the doorbell hangs off a filed event exactly as
    /// the turn-completion one does. **Every failure answers `false` and leaves
    /// nothing behind** — no event, no in-memory card, no ring — because an
    /// approval this daemon cannot mirror honestly is one the operator is still
    /// being asked at the keyboard.
    pub(crate) async fn raise_codex_approval(
        self: &Arc<Self>,
        session: &SessionKey,
        card: ApprovalCard,
        thread_id: &str,
        turn_id: &str,
        item_id: &str,
        family: &str,
    ) -> bool {
        let request_id = card.request_id.clone();
        let generation = card.generation;
        let created_ms = protocol::time::now_unix_ms();

        // The fact the card stands behind. It is committed with the row rather
        // than before it — see the commit boundary below — and a `None` back
        // from that commit is a re-delivery: the same item, the same derived
        // request id, the same `perm:` source id the first sighting filed.
        let payload = serde_json::json!({ "card": card });

        // **The aggregate bound, checked on the serialised card against the
        // limit that would destroy it.**
        //
        // [`Daemon::truncate_payload`] does not *trim* an oversized card, it
        // REPLACES it with a preview object — and a phone handed that decodes no
        // card at all. `codex_approval`'s per-field bounds keep this rare, and
        // they cannot make it impossible: nothing on the wire bounds a path, an
        // execpolicy amendment, or a field a later codex adds, and every field
        // rides the payload twice because `display_text` is `tool_input`
        // stringified. So the *total* is asked here, where the real limit lives,
        // and an approval that cannot be carded honestly is not carded at all.
        //
        // Refusing loudly is the right failure: the question is being asked at
        // the keyboard and is still answerable there, as it would be with no
        // card at all. A silently dead card is not.
        let encoded_payload = payload.to_string();
        if encoded_payload.len() > self.config.max_payload_bytes {
            crate::log_error!(
                "refusing to card the Codex approval {request_id} for {}: the event would be \
                 {} bytes against a {}-byte payload limit, and a payload the daemon replaces \
                 is a card the phone cannot decode. The request stands and is answerable at \
                 the keyboard.",
                session.name,
                encoded_payload.len(),
                self.config.max_payload_bytes
            );
            return false;
        }
        // **The turn this card belongs to, on the envelope, because a Stop has to
        // name one.**
        //
        // `ClientMessage::Interrupt` requires a `turn_id`. Without this the phone's
        // only source for one was the envelope of some *earlier* event — the
        // `tool_call` an `item/started` files — and "the approval always follows its
        // item's start" is an ordering, not a contract: a link that rebound
        // mid-turn cards a command it never saw start. So the card's own request
        // says which turn it is about, on the same envelope field every other Codex
        // event already carries it on.
        //
        // Read from the approval, never from the link's `RunningTurn`. The
        // `*/requestApproval` frame declares `turnId` required in both measured
        // releases and `codex_approval::Approval::read` refuses a frame without one
        // (`Refusal::Malformed`), so the turn a card is about is a fact the card was
        // built from; the link's belief about what is running is an inference, and
        // it is empty on exactly the rebind case above. `None` is therefore
        // unreachable from the one production caller — it is here because an empty
        // string is not a turn any interrupt could name, and a `Some("")` on the
        // wire would hand the phone a Stop button that cannot work.
        let pending =
            PendingEvent::new(session, EventKind::ApprovalRequest, payload, Source::Daemon)
                .with_source_event_id(format!("perm:{request_id}"))
                .with_turn_id((!turn_id.is_empty()).then(|| turn_id.to_string()));

        let encoded = match serde_json::to_string(&card) {
            Ok(encoded) => encoded,
            Err(err) => {
                crate::log_error!("could not encode the Codex card for {request_id}: {err:#}");
                return false;
            }
        };
        let row = crate::store::CodexPendingApprovalRow {
            session_uid: session.uid.clone(),
            session_id: session.name.clone(),
            request_id: request_id.clone(),
            card: encoded,
            generation,
            created_ms,
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            item_id: item_id.to_string(),
            family: family.to_string(),
        };

        let label = self.effective_project_label(&session.uid).await;
        let id: ApprovalId = (session.uid.clone(), request_id.clone());
        let card_entry = |raise: u64| PendingApproval {
            card: card.clone(),
            session: session.clone(),
            project_label: label.clone(),
            agent: protocol::agent::AgentKind::Codex,
            raise,
            claimed: false,
            tool_ran: false,
            // No Claude agent asked it.
            fork: false,
            link: false,
            question: false,
        };

        // **One commit boundary, and nothing is exposed before it holds.**
        //
        // The event used to be filed first and the row written after, which made
        // three failures reachable and all three silent. A constraint refusal —
        // the unique index catching a broken derivation — left the request event
        // filed with no card and no terminal that could ever retire it. An
        // ordinary write failure returned success and rang the phone for a card
        // a restart would forget. And the reverse ordering would have lost the
        // fact instead of the card.
        //
        // So the row and the event share a transaction, and **every** failure of
        // it is answered the same way: no event, no in-memory card, no ring, and
        // a loud log. That is not a degraded mode — the request is being asked at
        // the keyboard and is still answerable there, which is exactly where it
        // was before this daemon observed anything.
        //
        // Published inside the gate for `ingest`'s reason: no later seq may
        // overtake this one on the way to a socket.
        //
        // **The card is pending before the commit**, and inserted only once the
        // gate is held. The request event becomes visible at the commit — a replay
        // or an earlier page can read it from the log from then on — and on the
        // broadcast after it, so a phone that sees the event and asks for the
        // session list must find the card there. A re-delivery finds its card
        // pending already; a raise that does not open takes back the card it put
        // there.
        //
        // **Taken back at whatever await the raise stops on.** A link teardown
        // aborts the task that raises; aborted after the insert, the card would
        // otherwise stay with no row and no event — one nothing can expire,
        // retire or delete. A commit already handed to the blocking pool still
        // finishes, so an abort inside it can leave the row and the event with no
        // card: that is a durable card the next start's recovery restores, as
        // before this card was inserted early.
        let gate = self.publish_gate(&session.uid).await;
        let (raised, registered, raise) = {
            let _ordered = gate.lock().await;
            let mut take_back = {
                let mut inner = self.inner.lock().await;
                inner.next_raise += 1;
                let raise = inner.next_raise;
                let registered = match inner.pending.entry(id.clone()) {
                    std::collections::hash_map::Entry::Occupied(_) => false,
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(card_entry(raise));
                        true
                    }
                };
                CardTakeBack {
                    inner: Arc::clone(&self.inner),
                    id: id.clone(),
                    raise,
                    armed: registered,
                }
            };
            let raised = self.db.raise_codex_pending_approval(row, pending).await;
            if let Ok(crate::store::CodexCardRaise {
                event: Some(event), ..
            }) = &raised
            {
                let _ = self.events_tx.send(event.clone());
            }
            let (registered, raise) = (take_back.armed, take_back.raise);
            if matches!(&raised, Ok(raise) if raise.is_open()) {
                take_back.armed = false;
            } else {
                take_back.now().await;
            }
            (raised, registered, raise)
        };
        let approval_event = match raised {
            Ok(raise) if raise.is_open() => Some(raise.event),
            // The re-delivery carried a DIFFERENT question under the same item
            // id. Refused loudly and the stored card stands: merging it would
            // move the row and the in-memory card to the new content while the
            // filed `ApprovalRequest` and every connected phone kept the old
            // one — half a representation, changed with no event and no ring to
            // say so. No capture has ever produced this, so there is nothing
            // measured to reconcile it against.
            Ok(crate::store::CodexCardRaise {
                outcome: crate::store::CodexCardOutcome::ContentChanged,
                ..
            }) => {
                crate::log_error!(
                    "refusing a re-delivery of the Codex approval {request_id} for {}: item \
                     {item_id} already holds a card for a DIFFERENT question. The card the \
                     phone is holding stands, because changing it under a filed request \
                     event would leave the two disagreeing. This shape is unmeasured.",
                    session.name
                );
                None
            }
            // The run was deleted between the frame arriving and the card being
            // filed. Neither half landed, so there is nothing to undo.
            Ok(_) => {
                crate::log_info!(
                    "dropping Codex approval {request_id} for {}: the session was deleted \
                     while it was in flight",
                    session.name
                );
                None
            }
            Err(err) => {
                crate::log_error!(
                    "refusing the Codex approval {request_id} for {}: neither the card nor \
                     the request event could be committed, so nothing is raised and nothing \
                     rings. The request stands and is answerable at the keyboard: {err:#}",
                    session.name
                );
                None
            }
        };
        let Some(approval_event) = approval_event else {
            return false;
        };

        // A re-delivery's card is replaced, as it always was; a first
        // sighting's is already the one above.
        if !registered {
            self.inner
                .lock()
                .await
                .pending
                .insert(id, card_entry(raise));
        }

        // The doorbell hangs off the FILED event, and only a first sighting
        // files one. A re-delivery after a reconnect is not a second question.
        if let Some(event) = approval_event {
            if let Some(ticket) = self.push_gate.admit_decision(&session.uid, &request_id) {
                self.dispatch_push(
                    session.uid.clone(),
                    event.seq,
                    ticket,
                    false,
                    Some(CardKey(request_id.clone())),
                );
            }
        }
        true
    }

    /// **Retire one Codex card, first terminal wins.**
    ///
    /// Claim by removal: the caller that takes the entry out of `inner.pending` owns
    /// the resolution, so a `serverRequest/resolved` arriving *after* a `turn/completed
    /// {interrupted}` already cleared the card finds nothing and overwrites
    /// nothing. That ordering is not hypothetical — it is what the wire does,
    /// measured in `fixtures/codex/interrupt.jsonl`, where the turn terminal is
    /// frame 33 and the resolution is frame 34.
    ///
    /// **This claim is not the durable guard.** The claim is per-process and dies
    /// with a restart, while the `resolved:{request_id}` source id is in the log —
    /// so the log is what still holds after a reconnect on a different connection,
    /// which is exactly the case a re-delivered request produces.
    ///
    /// **It does not touch `answers`.** That table is one of the four a
    /// rolled-back v0.6.0 daemon rewrites globally, and Codex's resolution
    /// taxonomy does not fit `AnswerOutcome`'s `decision + resolved_by +
    /// applied_via` shape anyway. [`protocol::ws::CodexResolution`] rides its
    /// own event for both reasons at once.
    ///
    /// # The two durable halves are one commit, and a failure is retryable
    ///
    /// Deleting the row and filing the resolution were two writes with the claim
    /// already given up between them, and both orderings lost something. A
    /// failed delete followed by a filed resolution left a row recovery restores
    /// — an already-answered question back on a phone after a restart, because
    /// nothing writes a Codex row into `answers` and that is the only terminal
    /// the recovery read knows how to ask about. A successful delete followed by
    /// a failed append lost the card's only terminal.
    ///
    /// They now share a transaction, and the claim is **put back** when it
    /// fails: the card is still open, still on the phone, and the next terminal
    /// the wire produces — the item's own `item/completed`, the turn's, or the
    /// `/new` sweep — retires it for real. Giving the claim away on a failed
    /// write would make the first terminal both the winner and the loser.
    ///
    /// # Three answers, not two
    ///
    /// "There was no claim to take" and "the commit failed" are opposite facts
    /// and were both `false`. A caller that cannot tell them apart cannot know
    /// whether it still owes this card anything, and throws away the wire mapping
    /// on both, which is how a failed retirement quietly becomes a permanent one.
    pub(crate) async fn retire_codex_approval(
        &self,
        session: &SessionKey,
        request_id: &str,
        resolution: protocol::ws::CodexResolution,
    ) -> Retirement {
        self.retire_codex_answered(session, request_id, resolution, None)
            .await
    }

    /// [`Daemon::retire_codex_approval`], plus the phone answer's own terminal in
    /// the same commit.
    ///
    /// **Why the two cannot be two commits.** A settled claim over a card that is
    /// still standing is a question the operator can never answer again — the ledger
    /// refuses the second tap, and the card goes on being displayed. A retired card
    /// over a live claim is the mirror: recovery finds the claim at the next start
    /// and files a second terminal for a card that already has one. The transaction
    /// is what makes both unrepresentable rather than unlikely.
    ///
    /// `None` for every terminal that is not a phone answer.
    pub(crate) async fn retire_codex_answered(
        &self,
        session: &SessionKey,
        request_id: &str,
        resolution: protocol::ws::CodexResolution,
        answer: Option<crate::store::AnswerTerminal>,
    ) -> Retirement {
        let id: ApprovalId = (session.uid.clone(), request_id.to_string());
        let Some(claimed) = self.inner.lock().await.pending.remove(&id) else {
            return Retirement::AlreadyGone;
        };
        // **The payload names the card it retires.**
        //
        // The resolution on its own is a terminal with no subject: `{"status":
        // "cleared","cause":"item_completed"}` says what happened and not what it
        // happened to. The only correlation used to be the `source_event_id` beneath
        // this line — a client stripping `"resolved:"` off an `Option<String>` — and
        // a prefix parse fails silently, leaving an answered card standing on a
        // phone for ever. Claude's `AnswerOutcome` has carried `request_id` inside
        // its payload since minor 0; this is the same field in the same place, so a
        // client correlates the two agents with one rule.
        //
        // Flattened, so every `status` arm is byte-identical to what it was and
        // this daemon's own readers — which decode the payload as a bare
        // `CodexResolution` — keep working.
        let resolved = protocol::ws::CodexResolutionPayload {
            request_id: request_id.to_string(),
            resolution,
        };
        let pending = PendingEvent::new(
            session,
            EventKind::ApprovalResolved,
            serde_json::to_value(&resolved).unwrap_or(serde_json::Value::Null),
            Source::Daemon,
        )
        .with_source_event_id(format!("resolved:{request_id}"));

        // Published inside the gate for `ingest`'s reason: no later seq may
        // overtake this one on the way to a socket.
        let gate = self.publish_gate(&session.uid).await;
        let retired = {
            let _ordered = gate.lock().await;
            let retired = self
                .db
                .retire_codex_pending_approval(
                    session.uid.clone(),
                    request_id.to_string(),
                    pending,
                    answer,
                )
                .await;
            if let Ok(Some(event)) = &retired {
                let _ = self.events_tx.send(event.clone());
            }
            retired
        };
        if let Err(err) = retired {
            // The claim goes back, so this terminal has neither retired the card
            // nor consumed the right to: a later one still can.
            self.inner.lock().await.pending.insert(id, claimed);
            crate::log_error!(
                "could not retire the Codex approval {request_id} in {}: neither the card \
                 nor its resolution was committed, so the card stays open and the next \
                 terminal will try again: {err:#}",
                session.name
            );
            return Retirement::Failed;
        }
        crate::log_info!(
            "codex approval {request_id} in {} retired: {:?}",
            session.name,
            resolved.resolution
        );
        Retirement::Retired
    }

    /// **A turn this daemon watched finish, as an occasion to ring.**
    ///
    /// A Codex run's `turn/completed`, the only completion signal Codex has: the design
    /// refuses Codex's `notify` hook and its `PermissionRequest` hook precisely so that
    /// `turn/completed` is the one thing that means a turn ended. And a Claude run
    /// CodeConnect started, at the `result` that ends each of its turns, read after its
    /// host's replay ([`crate::claude_host`]'s `apply_sdk`): an end the host replays to a
    /// restarted ccd is old news, as a Codex resume's is. So the same gate, the same
    /// ticket, the same dispatch — and, like every doorbell, it rings only if a decision
    /// the phone can answer is open when it goes, and is then about that decision
    /// (`dispatch_push`). What differs is only where the fact came from.
    ///
    /// # Live-only, and how that is guaranteed rather than checked
    ///
    /// A Codex turn terminal reaches the store from two places, and by deliberate design
    /// they are byte-identical facts under one dedup key: the live
    /// `turn/completed` frame, and a `thread/resume` answer describing a turn that
    /// finished while this daemon was not subscribed. Ringing about the second
    /// would be a doorbell for news that is already old — the exact thing the whole
    /// push design exists to avoid.
    ///
    /// Nothing distinguishes the two *facts*, so nothing tries to. The two **call
    /// sites** are distinct instead, and only the live one calls this: an engine run's
    /// link ([`crate::codex_host`]) as it files a frame read off the wire, while its
    /// resume hands its events to [`Daemon::ingest`] and stops
    /// there. Adding a provenance field to the event would have been the wrong fix
    /// twice over: it would break the identity that makes recovery dedup-safe, and
    /// under first-wins the marker of whichever copy arrived first would become
    /// permanent.
    ///
    /// **The dedup answer is the second guarantee, and it is free.** `ingest`
    /// returns `None` for a key already filed, so a live terminal for a turn some
    /// earlier resume already described never reaches this call — a re-attach
    /// cannot re-ring a turn the reader was told about when it happened.
    ///
    /// # Which ends ring
    ///
    /// A Codex `interrupted` or `failed` terminal is no occasion. `interrupted` means
    /// a human stopped the turn at the keyboard, and the person who did it is the
    /// person the doorbell would wake. A Claude turn's `result` rings however it
    /// ended, an error included, except one a Stop ended (`terminal_reason`
    /// `aborted…`), for the same reason. A command's own end (`local_command`:
    /// `/clear`, `/compact`, `/cost`) is a turn's end like any other.
    ///
    /// # One ring per turn, except when two turns share a dispatch grace
    ///
    /// Each terminal is admitted on its own — the latch is reopened by the very
    /// event that closes it — so nothing here collapses turns. What collapses them
    /// is the 400 ms grace every push waits out: a second turn finishing inside the
    /// first's grace records progress, and progress after an admission is what
    /// cancels a quiet-state push. Two turns 300 ms apart therefore produce one
    /// notification, and it is the *later* one — the state the run is actually in.
    ///
    /// A second turn merely **starting** inside the grace cancels it too, and
    /// nothing rings: the run is working again, so there is no completion left to
    /// announce. See [`Daemon::note_turn_running`].
    ///
    /// Which is the right answer for the thing this is: a doorbell whose whole
    /// content is a few words and a count. Two rings 300 ms apart about the same
    /// open decisions are one fact told twice to somebody whose phone is on a
    /// table.
    pub(crate) async fn push_turn_complete(
        self: &Arc<Self>,
        session: &SessionKey,
        trigger: &Event,
    ) {
        // **The movement and the news are one transition** — see
        // `PushGate::admit_turn_end`. Recorded apart, an unrelated progress landing
        // between them folds into this ticket and the push it should have cancelled
        // survives its grace.
        let ticket = self.push_gate.admit_turn_end(&session.uid);
        // **Content-free.** The terminal's payload carries the turn's status,
        // timings and any error text; none of it is read here. An APNs payload
        // passes through Apple, and nothing an agent produced ever does.
        self.dispatch_push(
            session.uid.clone(),
            trigger.seq,
            ticket,
            // A quiet-state push: progress landing during the grace means the run
            // has moved on from the state this announces, and it should not ring.
            true,
            // No card behind it. An approval's push is its raise's
            // ([`Daemon::raise_codex_approval`], [`Daemon::host_card_asked`]), and it
            // carries its own.
            None,
        );
    }

    /// What the fleet would call this run, from the cwd the daemon has persisted.
    async fn effective_project_label(&self, session_uid: &str) -> String {
        match self.db.get_session(session_uid.to_string()).await {
            Ok(Some(row)) => crate::project_label::project_label(&row.cwd),
            // No row: the run is gone. Say nothing rather than reach for its
            // name.
            Ok(None) => String::new(),
            // A database that will not answer is not the same thing as a run
            // with no project, even though the notification says the same
            // words for both — so it is said out loud here.
            Err(err) => {
                crate::log_warn!("push: no project label for {session_uid}: {err:#}");
                String::new()
            }
        }
    }

    /// Resolve whatever a client named — a uid or a name — to a real run.
    ///
    /// A uid is exact. A **name** is not an identity, so it is resolved by the
    /// store's policy — the newest run under that name. That rule does not read
    /// `lifecycle`: a session that ended while the daemon was down was never
    /// observed exiting, so `lifecycle` says `Live` forever and ranking on it
    /// would put that ghost above the run that is genuinely there.
    pub async fn resolve(&self, reference: &str) -> Result<SessionRow> {
        self.resolve_optional(reference)
            .await?
            .ok_or_else(|| anyhow!("unknown session {reference}"))
    }

    /// The same resolution, with **"named nothing" as a value and a broken store
    /// as an error**.
    ///
    /// [`Daemon::resolve`] folds the two together because its callers answer both
    /// the same way — with a refusal carrying a message. A caller that has to act
    /// differently on them (a client error versus an operational fault) cannot
    /// recover the distinction from the outside: asking the store a second time to
    /// tell them apart is a race, and treating every error as absence is how a
    /// failing database comes to look like an empty fleet.
    async fn resolve_optional(&self, reference: &str) -> Result<Option<SessionRow>> {
        if protocol::uid::is_well_formed(reference) {
            if let Some(row) = self.db.get_session(reference.to_string()).await? {
                return Ok(Some(row));
            }
        }
        self.store.find_session(reference)
    }

    // --------------------------------------------------------------- answers

    /// Which run an answer is for.
    ///
    /// A `session_uid` settles it outright. An answer without a `session_id` (the
    /// field is optional on the wire; see Known limits in `mac/README.md`) is matched
    /// against the live approvals by request id; if none matches (the card was
    /// answered and the entry is gone) the ledger is consulted, so a retried tap still
    /// gets its original outcome back. Every run's name is its uid, so a `session_id`
    /// that is not a uid matches nothing and is refused as an unknown session.
    ///
    /// Two live approvals sharing a request id is the one case that cannot be
    /// worked out, and it is refused with an instruction rather than resolved by
    /// coin flip.
    async fn approval_target(
        &self,
        request_id: &str,
        session_ref: Option<&str>,
    ) -> std::result::Result<String, String> {
        // A **uid** is an identity: it names one run and there is nothing to
        // disambiguate.
        if let Some(uid) = session_ref.filter(|value| protocol::uid::is_well_formed(value)) {
            return match self.db.get_session(uid.to_string()).await {
                Ok(Some(row)) => Ok(row.session_uid),
                Ok(None) => Err(format!("unknown session {uid}")),
                // **Logged here, not carried.** This `Err` becomes an
                // `AnswerResult::Rejected` reason verbatim in [`Daemon::answer`], and a
                // store `Display` is an anyhow chain whose open paths name absolute
                // filesystem locations. Same rule as
                // [`crate::codex_refusals::SESSION_LOOKUP_FAILED`], which is the
                // catalogued twin of this sentence on the other two verbs.
                Err(err) => {
                    crate::log_error!("answer for {uid}: session lookup failed: {err:#}");
                    Err("session lookup failed".to_string())
                }
            };
        }

        // No uid: matched by request id alone (a `session_id` that is not a uid
        // names no run, so it matches nothing). If this request id is open in more
        // than one run, the right answer cannot be worked out and is not guessed at.
        let matches: Vec<String> = {
            let inner = self.inner.lock().await;
            inner
                .pending
                .iter()
                // `Option::is_none_or` would read better but was stabilised
                // after the toolchain this workspace declares support for.
                .filter(|((_, id), entry)| {
                    id == request_id
                        && match session_ref {
                            Some(name) => entry.session.name == name,
                            None => true,
                        }
                })
                .map(|((uid, _), _)| uid.clone())
                .collect()
        };
        match matches.len() {
            1 => return Ok(matches.into_iter().next().expect("length checked")),
            0 => {}
            _ => {
                return Err(
                    "this request id is open in more than one run of that name; \
                     answer with the session_uid the card came from"
                        .into(),
                )
            }
        }

        // No live approval matches. Either the card was already answered — in
        // which case the ledger replays its outcome — or the name is unknown.
        if let Some(name) = session_ref {
            if self
                .db
                .find_session(name.to_string())
                .await
                .ok()
                .flatten()
                .is_none()
            {
                return Err(format!("unknown session {name}"));
            }
        }
        match self.db.find_answer_by_request(request_id.to_string()).await {
            Ok(Some((uid, _, _))) => Ok(uid),
            Ok(None) => Err("unknown or already-resolved request".into()),
            // Logged here, not carried, for the session lookup's reason above.
            Err(err) => {
                crate::log_error!(
                    "answer for {}: the answers ledger could not be read: {err:#}",
                    logged_request_id(request_id)
                );
                Err("ledger read failed".to_string())
            }
        }
    }

    /// **Answer one Codex card from the phone.**
    ///
    /// Reached from [`Daemon::answer`] under that function's per-approval
    /// serialisation, so everything below runs with at most one tap in flight for
    /// this card.
    ///
    /// # What makes this a different function rather than a branch
    ///
    /// A Claude card is answered through its run's agent host
    /// ([`crate::claude_host::answer`]). Here the answer is a JSON-RPC response written on the
    /// link's own socket, and the app-server actuates it.
    ///
    /// # The four tables it does not touch
    ///
    /// `answers`, `answer_claims`, `text_mutations` and `pending_approvals` are read
    /// globally by a rolled-back v0.6.0 daemon, so a Codex row in any of them is the
    /// rollback hazard [`Daemon::shared_ledgers_admit`] exists to prevent. The claim
    /// this path takes goes in `mutation_ledger` — the generalized Codex ledger,
    /// which no v0.6.0 statement names — and the tripwire in the tests is pointed at
    /// exactly that claim.
    ///
    /// # Validate here, claim and write there
    ///
    /// Everything this function checks is a property of the STORED card: that it is
    /// still open, that the hash the phone echoed is the one it was displayed under,
    /// and that the option named is one the app-server itself proposed, or, on a question
    /// card, that the answers fit the questions it stored. None of that needs a socket.
    /// Everything that needs the socket — whether the connection
    /// holds a live wire id for this card, on the card's own thread, with no switch
    /// in flight — is asked of the link, and the link takes the durable claim at the
    /// one moment it knows the answer is about to be written. That is what keeps a
    /// refused ask from leaving an `applying` row behind for recovery to turn into a
    /// spurious `Unknown`.
    async fn answer_codex(
        &self,
        row: &crate::store::SessionRow,
        request_id: &str,
        payload_hash: &str,
        decision: AnswerDecision,
        answered_on: AnsweredOn,
    ) -> AnswerResult {
        // An approval card is answered by naming one of its options and a question card
        // with its answers or a decline; no Codex card takes any other decision.
        let names_no_option = || AnswerResult::Rejected {
            reason: "a Codex card is answered by naming one of the options it \
                     offered; this decision names none of them, so nothing was sent"
                .into(),
        };
        if !matches!(
            decision,
            AnswerDecision::OptionId { .. }
                | AnswerDecision::Answers { .. }
                | AnswerDecision::Decline
        ) {
            return names_no_option();
        }
        let session = SessionKey::new(&row.session_uid, &row.session_id);

        // 1. **The durable ledger wins over everything, including the card.** A
        //    terminal claim can outlive the question it answered: a lost race leaves
        //    the card standing for the winner's own terminal to retire, and a bounce
        //    re-delivers a request the app-server really is still waiting on. In both
        //    the card is there and the phone must not answer it again — and the
        //    operator needs to be told which of those it is, not merely refused. The
        //    link would refuse this on its own claim; asking here is what makes the
        //    refusal a sentence rather than "there is no link".
        let mut in_flight = false;
        match self
            .db
            .answer_status(session.uid.clone(), request_id.to_string())
            .await
        {
            Ok(Some(crate::store::AnswerStatus::Settled(outcome))) => {
                // **Branch on what the ledger recorded, not on the fact that it is
                // terminal.** `lost` is the record that the earlier answer reached
                // nothing; replaying it as "already answered" tells the operator the
                // opposite of what happened, at the moment they are asking.
                return AnswerResult::Rejected {
                    reason: crate::store::replayed_answer_sentence(&outcome),
                };
            }
            Ok(Some(crate::store::AnswerStatus::Indeterminate)) => {
                return AnswerResult::Rejected {
                    reason: "an answer to this card was already sent and what became of it \
                             is not known; it will not be sent again. Check the computer."
                        .into(),
                }
            }
            // `Applying` is an answer sent and not yet confirmed, by this daemon or, across a
            // restart, by the one before it. A link decides it; an engine run with no link
            // answers it from here, below.
            Ok(Some(crate::store::AnswerStatus::Applying)) => in_flight = true,
            Ok(None) => {}
            // Logged here, not carried: a store `Display` is an anyhow chain whose open
            // paths name absolute filesystem locations. The phone is told the fact and
            // the consequence, which is all it can act on.
            Err(err) => {
                crate::log_error!(
                    "answer for {}: could not read the answer ledger for {}: {err:#}",
                    session.uid,
                    logged_request_id(request_id)
                );
                return AnswerResult::Rejected {
                    reason: "could not read this card's answer ledger; nothing was sent".into(),
                };
            }
        }

        // 2. The card, as this daemon filed it. Not the phone's copy of it: the
        //    options an answer is checked against have to be the ones the
        //    app-server proposed, and the hash below is what ties the two together.
        let cards = match self.db.codex_pending_approvals(session.uid.clone()).await {
            Ok(cards) => cards,
            // Logged here, not carried. See the ledger read above.
            Err(err) => {
                crate::log_error!(
                    "answer for {}: could not read the open cards for {}: {err:#}",
                    session.uid,
                    logged_request_id(request_id)
                );
                return AnswerResult::Rejected {
                    reason: "could not read this run's open cards; nothing was sent".into(),
                };
            }
        };
        let Some(held) = cards.into_iter().find(|card| card.request_id == request_id) else {
            return AnswerResult::Rejected {
                reason: "unknown or already-resolved request".into(),
            };
        };
        let Ok(card) = serde_json::from_str::<protocol::ws::ApprovalCard>(&held.card) else {
            return AnswerResult::Rejected {
                reason: "the stored card could not be read, so what it offered cannot be \
                         established; nothing was sent"
                    .into(),
            };
        };
        // **The hash gate, and it covers the option set.** `tool_input.options` is
        // inside the preimage `payload_hash` is taken over, so a phone echoing the
        // hash it displayed cannot be answering an option table that has since been
        // replaced — which is the whole reason the options ride inside the hash.
        if card.payload_hash != payload_hash {
            return AnswerResult::Rejected {
                reason: "stale payload_hash: the card you answered is out of date".into(),
            };
        }
        // 3. The wire decision, rebuilt from the card's own options. A phone sends
        //    an opaque id; the body of an amendment is never anything it supplied. A
        //    question card's whole reply is built from the questions it stored, and the
        //    answers are kept whole for its resolution and for a restart's resend.
        let question = crate::codex_approval::Family::of_item_type(&held.family)
            == Some(crate::codex_approval::Family::Question);
        let (route, wire, question_decision) = if question {
            match crate::codex_approval::question_reply(&card.tool_input, &decision) {
                Ok((route, reply)) => (route.to_string(), reply, Some(decision.clone())),
                Err(why) => {
                    return AnswerResult::Rejected {
                        reason: format!("{why}. Nothing was sent."),
                    }
                }
            }
        } else {
            let AnswerDecision::OptionId { option_id } = &decision else {
                return names_no_option();
            };
            let Some(wire) =
                crate::codex_approval::wire_decision(&card.tool_input["options"], option_id)
            else {
                return AnswerResult::Rejected {
                    reason: format!(
                        "{option_id:?} is not one of the options this card offered; nothing was sent"
                    ),
                };
            };
            (option_id.clone(), wire, None)
        };

        // 4. The material this answer will be claimed with — the whole authorization
        //    surface, so the same id carrying a different decision, a different card
        //    or a different thread is a conflict rather than a replay.
        let claimed_hash =
            protocol::hash::answer_hash(request_id, payload_hash, &route, &held.thread_id, &wire);
        let claimed = crate::store::ClaimedMaterial {
            thread_id: held.thread_id.clone(),
            generation: held.generation,
            route,
            target_turn_id: Some(held.turn_id.clone()),
            claimed_hash,
            question_decision,
        };

        // 5. The link claims and writes, in that order, and files the terminal
        //    itself — see [`crate::codex_host::AnswerRequest`].
        //
        //    The READ side of [`Daemon::actuation_gates`], taken before `inner`, as an
        //    OWNED guard: it travels with the ask into
        //    [`crate::codex_host::AnswerRequest`]/`PendingAnswer` and is only released at
        //    pre-write refusal or terminal teardown.
        let quiesce = self.actuation_gate(&session.uid).await;
        let admitted = Arc::clone(&quiesce).read_owned().await;
        let link = match self.inner.lock().await.host_links.get(&session.uid) {
            Some(HostLink::Codex(answers, _)) => Some(answers.clone()),
            _ => None,
        };
        let report = match link {
            Some(answers) => {
                answers
                    .answer(request_id, wire, claimed, answered_on, admitted)
                    .await
            }
            // An engine run with no link. An answer already sent stays claimed for the run's
            // reconnect, which sends it once more if Codex asks again.
            None if matches!(self.db.hosted_link(session.uid.clone()).await, Ok(Some(_))) => {
                crate::codex_host::AnswerReport::NotApplied(if in_flight {
                    crate::codex_host::ANSWER_ALREADY_WAITING.into()
                } else {
                    crate::codex_refusals::ENGINE_NO_CONNECTION.into()
                })
            }
            None => crate::codex_host::AnswerReport::NotApplied(
                "there is no live link to this Codex session, so nothing was sent; \
                 answer at the computer"
                    .into(),
            ),
        };
        match report.reason() {
            Some(reason) => AnswerResult::Rejected {
                reason: reason.to_string(),
            },
            None => AnswerResult::Applied {
                outcome: protocol::ws::AnswerOutcome {
                    request_id: request_id.to_string(),
                    session_id: session.name.clone(),
                    decision,
                    resolved_by: protocol::ws::ResolvedBy::Phone,
                    answered_on: Some(answered_on),
                    applied_via: protocol::ws::AnswerPath::CodexResponse,
                    resolved_at: protocol::time::now_rfc3339(),
                    detail: None,
                    indeterminate: false,
                },
            },
        }
    }

    /// **Stop the turn a Codex session is running, from a phone**, or a Claude run
    /// CodeConnect started, through its agent host ([`crate::claude_host::interrupt`]).
    ///
    /// Idempotent by `(session, request_id)` in the generalized mutation ledger, and
    /// bound to one turn by `payload_hash` — so a retry can never abort a *different*
    /// turn the session has since moved on to, which is the whole reason the hash is
    /// on the wire.
    ///
    /// # What this refuses before it claims anything, and why each one is here
    ///
    ///   * **The agent.** "Not Claude" is not evidence of Codex, so every kind is
    ///     named and anything this build does not understand fails closed — the same
    ///     rule [`Daemon::answer`] states at the same point in its own path.
    ///   * **The turn.** The phone names a turn it saw. The `payload_hash` beside it
    ///     is a CHECKSUM and not a binding — it is recomputed here from the very two
    ///     fields the request carries, so it can only catch a request whose parts were
    ///     corrupted or reassembled in transit, never one whose turn a caller chose to
    ///     change. The turn id itself is the authorization, and the run's link checks
    ///     it against the turn the run is running.
    pub async fn interrupt(
        &self,
        session_ref: &str,
        request_id: &str,
        turn_id: &str,
        payload_hash: &str,
    ) -> protocol::ws::InterruptResult {
        use protocol::ws::InterruptResult;

        let refuse = |reason: String| InterruptResult::Rejected { reason };

        // 1. **The run, and the agent that is running in it — before any lock or
        //    durable claim.** An unreadable row is a refusal too: unreadable is not
        //    evidence of Codex any more than "not Claude" is.
        let row = match self.resolve_optional(session_ref).await {
            Ok(Some(row)) => row,
            Ok(None) => return refuse(crate::codex_refusals::unknown_session(session_ref)),
            // **Logged here, not carried.** A store `Display` is an anyhow chain whose
            // open paths name absolute filesystem locations, and the phone is told the
            // fact rather than this Mac's directory layout. The operator diagnosing the
            // disk reads the chain here, where they are entitled to it.
            Err(err) => {
                crate::log_error!(
                    "interrupt for {}: session lookup failed: {err:#}",
                    logged_session_ref(session_ref)
                );
                return refuse(crate::codex_refusals::SESSION_LOOKUP_FAILED.to_string());
            }
        };
        let moving = self.move_guard(&row.session_uid).await;
        // A Claude run CodeConnect started: its lifecycle, for its own branch below.
        let mut hosted = None;
        match row.agent {
            protocol::agent::AgentKind::Codex => {}
            // A Claude or Antigravity run CodeConnect started is stopped through its agent
            // host. Any other has no interrupt on this wire: its stop control is the keyboard
            // at the Mac, and saying so is more useful than a bare refusal.
            protocol::agent::AgentKind::Claude | protocol::agent::AgentKind::Antigravity => {
                match self.db.hosted_link(row.session_uid.clone()).await {
                    Ok(Some(linked)) => hosted = Some(linked.lifecycle),
                    _ => {
                        return refuse(crate::codex_refusals::interrupt_on_claude(&row.session_uid))
                    }
                }
            }
            protocol::agent::AgentKind::Unsupported(ref name) => {
                return refuse(crate::codex_refusals::interrupt_on_unsupported(
                    &row.session_uid,
                    name,
                ))
            }
        }
        let session = SessionKey::new(&row.session_uid, &row.session_id);

        // 2. **The checksum, and it is called that on purpose.** It is recomputed
        //    here from the same two fields the request carries, so it catches a
        //    request whose parts were corrupted or reassembled in transit and nothing
        //    else — a caller that changes the turn and recomputes the hash passes it
        //    trivially. What actually authorizes the stop is the turn id, which the
        //    run's link checks against the turn the run is running.
        let expected = protocol::hash::interrupt_hash(session_ref, turn_id);
        if expected != payload_hash {
            return refuse(crate::codex_refusals::INTERRUPT_STALE_HASH.into());
        }
        if turn_id.is_empty() {
            return refuse(crate::codex_refusals::INTERRUPT_NAMES_NO_TURN.into());
        }

        // 3. **The durable ledger wins over everything.** A terminal claim outlives
        //    the turn it named, and a duplicate must replay what the first attempt
        //    recorded rather than aim a second stop at whatever is running now.
        //
        //    **And a duplicate is a second ask carrying the same material, not merely
        //    the same id.** The ledger's own claim primitive will not call two asks a
        //    duplicate unless every field agrees, and reading a terminal here without
        //    applying that same law is how one id reused for the turn running NOW
        //    comes back as `Duplicate` naming that turn — telling the operator the
        //    turn in front of them has already been stopped when nothing has touched
        //    it. So the material comes back with the status and is compared before
        //    anything is replayed.
        //
        //    **Compared on what the CLIENT bound**: the turn it named and the hash
        //    binding this ask to that turn. The thread and the visit generation are
        //    this daemon's own bookkeeping — a person who reused an id has done
        //    nothing wrong about a visit they cannot see — and they stay where they
        //    can mean something, as the live pre-write checks the link makes.
        let mut in_flight = false;
        match self
            .db
            .mutation_status(
                crate::store::OPERATION_INTERRUPT,
                session.uid.clone(),
                request_id.to_string(),
            )
            .await
        {
            Ok(Some(state)) => {
                // Compared against the NORMALISED material below, for the reason given
                // there: the caller's own hash is a checksum over whichever reference
                // they used, and two spellings of one run must not read as two asks.
                let bound_to_the_same_turn = state.claimed.target_turn_id.as_deref()
                    == Some(turn_id)
                    && state.claimed.claimed_hash
                        == protocol::hash::interrupt_hash(&row.session_uid, turn_id);
                if !bound_to_the_same_turn {
                    return refuse(crate::codex_refusals::INTERRUPT_ID_REUSED.into());
                }
                match state.status {
                    // Replayed through the one mapping both this path and the link's
                    // own duplicate arm use, so the two can never disagree about what
                    // a recorded outcome means.
                    crate::store::AnswerStatus::Settled(outcome) => {
                        return Daemon::interrupt_result(
                            crate::codex_host::replayed_interrupt_report(&outcome, turn_id),
                        )
                    }
                    crate::store::AnswerStatus::Indeterminate => {
                        return InterruptResult::Indeterminate {
                            reason: crate::codex_refusals::INTERRUPT_ALREADY_SENT_UNKNOWN.into(),
                        }
                    }
                    // `Applying` is an attempt this very daemon has made. A link decides
                    // it; an engine run with no link answers it from here, below.
                    crate::store::AnswerStatus::Applying => in_flight = true,
                }
            }
            Ok(None) => {}
            // Logged here, not carried, for the session lookup's reason above.
            Err(err) => {
                crate::log_error!(
                    "interrupt for {}: could not read the interrupt ledger for {}: {err:#}",
                    session.uid,
                    logged_request_id(request_id)
                );
                return refuse(crate::codex_refusals::INTERRUPT_LEDGER_UNREADABLE.to_string());
            }
        }

        // **A Claude run CodeConnect started is stopped through its agent host**, which
        // runs the turn and refuses one it is not running ([`crate::claude_host::interrupt`]).
        // First, since the engine branch below knows a run only by its `run_hosts` row, which
        // a Claude run with a host has too.
        if let Some(lifecycle) = hosted {
            return Daemon::interrupt_result(
                crate::claude_host::interrupt(self, &session.uid, request_id, turn_id, lifecycle)
                    .await,
            );
        }

        // **A run started on the engine is stopped through its own link**, which claims the
        // ask, sends `turn/interrupt` and settles it ([`crate::codex_host`]). With its link
        // not attached, nothing is sent and the person is told so; an ask already sent is
        // answered from the ledger, since startup records it unknown and never sends it again.
        let engine = match self.inner.lock().await.host_links.get(&session.uid) {
            Some(HostLink::Codex(_, commands)) => Some(commands.clone()),
            _ => None,
        };
        if let Some(commands) = engine {
            return Daemon::interrupt_result(
                self.interrupt_on_engine(&commands, &session.uid, request_id, turn_id, moving)
                    .await,
            );
        }
        if in_flight {
            return InterruptResult::Indeterminate {
                reason: crate::codex_refusals::INTERRUPT_ALREADY_SENT_UNKNOWN.into(),
            };
        }
        refuse(crate::codex_refusals::ENGINE_NO_CONNECTION.into())
    }

    /// An engine run's Interrupt, handed to its link
    /// ([`crate::codex_host::LinkCommand::Interrupt`]). A link that ended before answering is
    /// reported by what the ledger then holds: no claim means nothing was sent, and a claim
    /// means the stop may have reached Codex.
    async fn interrupt_on_engine(
        &self,
        commands: &tokio::sync::mpsc::UnboundedSender<crate::codex_host::LinkCommand>,
        uid: &str,
        request_id: &str,
        turn_id: &str,
        moving: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> crate::codex_host::InterruptReport {
        use crate::codex_host::InterruptReport;
        let (reply, told) = tokio::sync::oneshot::channel();
        let asked = commands.send(crate::codex_host::LinkCommand::Interrupt {
            request_id: request_id.to_string(),
            turn_id: turn_id.to_string(),
            reply,
        });
        // Handed to the link, which takes its commands in order and reads a stop it holds as a turn running: the move
        // gate is let go here, not when Codex ends the turn, which it may never do, so an End run is never held behind
        // it.
        drop(moving);
        if asked.is_ok() {
            if let Ok(report) = told.await {
                return report;
            }
        }
        match self
            .db
            .mutation_status(
                crate::store::OPERATION_INTERRUPT,
                uid.to_string(),
                request_id.to_string(),
            )
            .await
        {
            Ok(None) => {
                InterruptReport::NotApplied(crate::codex_refusals::ENGINE_NO_CONNECTION.into())
            }
            _ => InterruptReport::Unknown(crate::codex_refusals::interrupt_settled_unknown(
                crate::codex_refusals::INTERRUPT_CONNECTION_ENDED,
            )),
        }
    }

    /// **The one place a link's report becomes the phone's answer.**
    ///
    /// A single mapping because the alternative was two, and they disagreed: the
    /// daemon's own replay of a settled row and the link's duplicate arm are the same
    /// question asked at two moments, and one of them used to call an exact aborted
    /// duplicate a refusal. Whichever moment answers, the operator is owed the same
    /// sentence.
    /// **Test-only.** The mapping above, reachable from the link's own tests so they
    /// can assert what the phone is actually told rather than only what the link
    /// reported.
    #[cfg(test)]
    pub(crate) fn interrupt_result_for_tests(
        report: crate::codex_host::InterruptReport,
    ) -> protocol::ws::InterruptResult {
        Daemon::interrupt_result(report)
    }

    /// **Test-only.** An engine run's link commands, so a test can hand its link an ask the
    /// daemon's own replay would have answered first.
    #[cfg(test)]
    pub(crate) async fn engine_commands_for_tests(
        &self,
        uid: &str,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<crate::codex_host::LinkCommand>> {
        match self.inner.lock().await.host_links.get(uid) {
            Some(HostLink::Codex(_, commands)) => Some(commands.clone()),
            _ => None,
        }
    }

    /// **Say something to a Codex session, from a phone.**
    ///
    /// [`Daemon::interrupt`]'s shape, and every check is here for the reason its twin
    /// gives. What differs is what the phone is authorized to name, and that difference is
    /// the whole security argument: an interrupt names a TURN and is bound to it, while a
    /// compose names nothing but WORDS. There is no turn to check the ask against, because
    /// the phone does not choose one — the run's link does, at the moment of the write.
    ///
    /// So what stands between a phone and the model's mouth is: the agent, the words'
    /// checksum, the ledger, and then the run's own link ([`crate::codex_host`]).
    ///
    /// # The route is not decided here, and that is deliberate
    ///
    /// Only the connection knows whether a turn is running, and knowing it a moment
    /// earlier than the write is not knowing it.
    ///
    /// `attachments` are `connection`'s uploads (minor 50): claimed before the words go, and
    /// carried as their path lines after them ([`crate::uploads`]).
    #[allow(clippy::too_many_arguments)]
    pub async fn compose(
        self: &Arc<Self>,
        session_ref: &str,
        request_id: &str,
        text: String,
        payload_hash: &str,
        connection: u64,
        attachments: &[String],
    ) -> protocol::ws::ComposeResult {
        use protocol::ws::ComposeResult;

        let refuse = |reason: String| ComposeResult::Rejected { reason };
        let arrived = tokio::time::Instant::now();

        // 1. **The run, and the agent that is running in it — before any lock or durable
        //    claim.** An unreadable row is a refusal too: unreadable is not evidence of
        //    Codex any more than "not Claude" is.
        let row = match self.resolve_optional(session_ref).await {
            Ok(Some(row)) => row,
            Ok(None) => return refuse(crate::codex_refusals::unknown_session(session_ref)),
            // Logged here, not carried. See [`Daemon::interrupt`]'s twin of this arm.
            Err(err) => {
                crate::log_error!(
                    "compose for {}: session lookup failed: {err:#}",
                    logged_session_ref(session_ref)
                );
                return refuse(crate::codex_refusals::SESSION_LOOKUP_FAILED.to_string());
            }
        };
        match row.agent {
            protocol::agent::AgentKind::Codex => {}
            // **The mirror of `send_text`'s refusal, and it names the thing that works.**
            // Claude's free-text takeover is [`Daemon::send_text`], which types at the
            // Mac's TTY and refuses a Codex run by name; this refuses a Claude run by
            // name. Neither silently does the other's job.
            protocol::agent::AgentKind::Claude => {
                return refuse(crate::codex_refusals::compose_on_claude(&row.session_uid))
            }
            protocol::agent::AgentKind::Antigravity => {
                return refuse(crate::codex_refusals::compose_on_unsupported(
                    &row.session_uid,
                    "Antigravity",
                ))
            }
            protocol::agent::AgentKind::Unsupported(ref name) => {
                return refuse(crate::codex_refusals::compose_on_unsupported(
                    &row.session_uid,
                    name,
                ))
            }
        }
        let session = SessionKey::new(&row.session_uid, &row.session_id);

        // 2. **The words themselves.** Bounded for `send_text`'s reason read across: a
        //    megabyte put into a model's mouth is not a message. Empty is refused because
        //    there is nothing to say and the app-server's own refusal for it is a shape
        //    this daemon would then have to explain. Attachments alone are not empty: their
        //    path lines are the words.
        if text.is_empty() && attachments.is_empty() {
            return refuse(crate::codex_refusals::COMPOSE_EMPTY.into());
        }
        if text.len() > protocol::ws::MAX_COMPOSE_BYTES {
            return refuse(crate::codex_refusals::compose_too_long(
                &text.len().to_string(),
                &protocol::ws::MAX_COMPOSE_BYTES.to_string(),
            ));
        }

        // 3. **The checksum, and it is called that on purpose** — [`Daemon::interrupt`]'s
        //    note applies verbatim. It catches a request whose parts were corrupted in
        //    transit and nothing else. What authorizes the words is that the phone is
        //    paired and the run is this Mac's.
        if protocol::hash::compose_hash_with(session_ref, &text, attachments) != payload_hash {
            return refuse(crate::codex_refusals::COMPOSE_STALE_HASH.into());
        }
        // **Normalised over the RUN, not over the reference the caller used.** A phone may
        // name one run by its uid on one tap and by its name on the next; storing the
        // caller's own hash would make those two spellings different material under one
        // id, which the ledger would call a conflict.
        let claimed_hash = protocol::hash::compose_hash_with(&row.session_uid, &text, attachments);
        // The files are the message's from here, before anything is said. A message the ledger
        // already holds claimed them when it was first sent, and is answered from its record
        // below, after a restart too.
        let recorded = matches!(
            self.db
                .mutation_status(
                    crate::store::OPERATION_COMPOSE,
                    row.session_uid.clone(),
                    request_id.to_string(),
                )
                .await,
            Ok(Some(_))
        );
        let text = if recorded {
            text
        } else {
            match self.claim_uploads(connection, &row.session_uid, attachments, text) {
                Ok(text) => text,
                Err(reason) => return refuse(reason),
            }
        };

        // **A run started on the engine is spoken to through its own link**, which claims the
        // message, sends it and settles it on Codex's evidence ([`crate::codex_host`]). With
        // its link not attached, a message already sent is answered from the ledger, since
        // startup records it unknown and never sends it again; a run whose app-server ended
        // without ccd stopping it is relaunched by the message, which its new link then sends
        // ([`crate::codex_host::relaunch`]); otherwise nothing is sent and the person is told
        // so. A run due to move to another account and certainly quiet is ended by its switch
        // first and relaunched by the message the same way ([`crate::switch::moves`]), all under
        // the run's move gate.
        let moving = self.move_guard(&session.uid).await;
        // One deadline for a message's relaunch, a move's end of the old app-server included: a move's runs from the
        // message's arrival, within the apps' wait for an answer, so one that cannot start is refused, nothing sent,
        // before they stop waiting.
        let mut deadline = tokio::time::Instant::now() + crate::start_session::START_TIMEOUT;
        let mut engine = match self.inner.lock().await.host_links.get(&session.uid) {
            Some(HostLink::Codex(_, commands)) => Some(commands.clone()),
            _ => None,
        };
        if let Some(commands) = engine.clone() {
            if moving.is_some() {
                let (reply, told) = tokio::sync::oneshot::channel();
                let running = commands
                    .send(crate::codex_host::LinkCommand::TurnRunning(reply))
                    .is_err()
                    || told.await.unwrap_or(true);
                // A message the ledger holds is answered from it, and moves nothing.
                if !recorded && crate::switch::moves(self, &session.uid, request_id, running).await
                {
                    deadline = arrived + crate::claude_host::RELAUNCH_WAIT;
                    if !crate::switch::ended_for_move(self, &session.uid, deadline).await {
                        return refuse(crate::claude_host::STILL_ENDING.into());
                    }
                    engine = None;
                }
            }
        }
        if let Some(commands) = engine {
            return Daemon::compose_result(
                self.compose_on_engine(&commands, &session.uid, request_id, text, claimed_hash)
                    .await,
            );
        }
        if let Some(recorded) = self
            .settled_compose(&row.session_uid, request_id, &claimed_hash)
            .await
        {
            return recorded;
        }
        let in_flight = matches!(
            self.db
                .mutation_status(
                    crate::store::OPERATION_COMPOSE,
                    session.uid.clone(),
                    request_id.to_string(),
                )
                .await,
            Ok(Some(state)) if state.status == crate::store::AnswerStatus::Applying
                && state.claimed.claimed_hash == claimed_hash
        );
        if in_flight {
            return ComposeResult::Indeterminate {
                reason: crate::codex_refusals::COMPOSE_ALREADY_SENT_UNKNOWN.into(),
            };
        }
        let (reply, told) = tokio::sync::oneshot::channel();
        let ask = crate::codex_host::ComposeAsk {
            request_id: request_id.to_string(),
            text,
            claimed_hash,
            reply,
        };
        // A run its switch ended, moved or failed: its relaunch has the move's deadline, from the message's arrival.
        if crate::switch::switched(self, &session.uid).await {
            deadline = arrived + crate::claude_host::RELAUNCH_WAIT;
        }
        if crate::codex_host::relaunch(self, &session.uid, ask, deadline).await {
            return Daemon::compose_result(
                self.engine_report(&session.uid, request_id, told).await,
            );
        }
        refuse(crate::codex_refusals::ENGINE_NO_CONNECTION.into())
    }

    /// An engine run's message, handed to its link
    /// ([`crate::codex_host::LinkCommand::Compose`]) and reported as [`Daemon::engine_report`]
    /// says.
    async fn compose_on_engine(
        &self,
        commands: &tokio::sync::mpsc::UnboundedSender<crate::codex_host::LinkCommand>,
        uid: &str,
        request_id: &str,
        text: String,
        claimed_hash: String,
    ) -> crate::codex_host::ComposeReport {
        let (reply, told) = tokio::sync::oneshot::channel();
        // A link that has gone drops the ask, and with it `reply`.
        let _ = commands.send(crate::codex_host::LinkCommand::Compose(
            crate::codex_host::ComposeAsk {
                request_id: request_id.to_string(),
                text,
                claimed_hash,
                reply,
            },
        ));
        self.engine_report(uid, request_id, told).await
    }

    /// What an engine run's link `told` of the message `request_id` it was handed. One that
    /// ended, or never took it, is reported by what the ledger then holds: no claim means
    /// nothing was sent, and a claim means the message may have reached Codex.
    async fn engine_report(
        &self,
        uid: &str,
        request_id: &str,
        told: tokio::sync::oneshot::Receiver<crate::codex_host::ComposeReport>,
    ) -> crate::codex_host::ComposeReport {
        use crate::codex_host::ComposeReport;
        if let Ok(report) = told.await {
            return report;
        }
        match self
            .db
            .mutation_status(
                crate::store::OPERATION_COMPOSE,
                uid.to_string(),
                request_id.to_string(),
            )
            .await
        {
            Ok(None) => {
                ComposeReport::NotApplied(crate::codex_refusals::ENGINE_NO_CONNECTION.into())
            }
            _ => ComposeReport::Unknown(crate::codex_refusals::compose_settled_unknown(
                crate::codex_refusals::COMPOSE_CONNECTION_ENDED,
            )),
        }
    }

    /// **The one place a link's compose report becomes the phone's answer.**
    ///
    /// One mapping for [`Daemon::interrupt_result`]'s reason: the daemon's own replay of a
    /// settled row and the link's duplicate arm are the same question asked at two
    /// moments, and the operator is owed the same sentence whichever answers.
    /// **What the ledger already knows about this exact ask, if it is finished.**
    ///
    /// Read ONLY when there is no addressee, and only terminal rows are answered from —
    /// the two restrictions that keep this from becoming the duplicate
    /// [`Daemon::compose`] deleted. `None` means "the record has nothing final to say", and
    /// every caller's fallback is the sentence about the link it already had.
    ///
    /// # Why `applying` is not answered here
    ///
    /// It is a claim some attempt still owns. Deciding it needs the claim itself, which is
    /// taken in the same critical section as the write and therefore lives in the link. A
    /// reader here could only guess, and the guess would be about a write that may be
    /// landing as it guesses.
    ///
    /// # Why the material is compared before anything is replayed
    ///
    /// [`Daemon::interrupt`]'s reason, in the form a compose takes it: a duplicate is a
    /// second ask carrying the SAME words. An id reused for different words is two
    /// mutations under one key, and replaying the first one's turn id at the second would
    /// tell somebody their new message reached the model when it never left this machine.
    /// Compared on the checksum alone — the thread, the route and the visit generation in
    /// the row are this daemon's own bookkeeping, and a person who reused an id did nothing
    /// wrong about state they cannot see.
    ///
    /// # Why an unreadable ledger is `None` rather than a refusal
    ///
    /// The caller's fallback already refuses, with a sentence about the link that is true.
    /// The store's own error is logged here, on the machine entitled to it, and never
    /// interpolated into what the phone reads — a rule a new reader of this table has to
    /// obey too.
    async fn settled_compose(
        &self,
        session_uid: &str,
        request_id: &str,
        claimed_hash: &str,
    ) -> Option<protocol::ws::ComposeResult> {
        let state = match self
            .db
            .mutation_status(
                crate::store::OPERATION_COMPOSE,
                session_uid.to_string(),
                request_id.to_string(),
            )
            .await
        {
            Ok(state) => state?,
            Err(err) => {
                crate::log_error!(
                    "codex compose for {session_uid}: could not read the compose ledger \
                     for {request_id}: {err:#}"
                );
                return None;
            }
        };
        if matches!(state.status, crate::store::AnswerStatus::Applying) {
            return None;
        }
        if state.claimed.claimed_hash != claimed_hash {
            return Some(protocol::ws::ComposeResult::Rejected {
                reason: crate::codex_refusals::COMPOSE_ID_REUSED.into(),
            });
        }
        Some(match state.status {
            // Through the one mapping the link's own duplicate arm uses, so the two can
            // never disagree about what a recorded outcome means.
            crate::store::AnswerStatus::Settled(outcome) => {
                Daemon::compose_result(crate::codex_host::replayed_compose_report(&outcome))
            }
            crate::store::AnswerStatus::Indeterminate => {
                Daemon::compose_result(crate::codex_host::ComposeReport::Unknown(
                    crate::codex_refusals::COMPOSE_ALREADY_SENT_UNKNOWN.into(),
                ))
            }
            // Returned above. Restated rather than folded into a wildcard so a fourth
            // status has to be decided here instead of inheriting a replay.
            crate::store::AnswerStatus::Applying => return None,
        })
    }

    fn compose_result(report: crate::codex_host::ComposeReport) -> protocol::ws::ComposeResult {
        use protocol::ws::ComposeResult;
        match report {
            crate::codex_host::ComposeReport::Started { turn_id } => {
                ComposeResult::Started { turn_id }
            }
            crate::codex_host::ComposeReport::Steered { turn_id } => {
                ComposeResult::Steered { turn_id }
            }
            crate::codex_host::ComposeReport::Duplicate { turn_id, started } => {
                ComposeResult::Duplicate { turn_id, started }
            }
            crate::codex_host::ComposeReport::NotApplied(reason) => {
                ComposeResult::Rejected { reason }
            }
            crate::codex_host::ComposeReport::Unknown(reason) => {
                ComposeResult::Indeterminate { reason }
            }
        }
    }

    fn interrupt_result(
        report: crate::codex_host::InterruptReport,
    ) -> protocol::ws::InterruptResult {
        use protocol::ws::InterruptResult;
        match report {
            crate::codex_host::InterruptReport::Aborted { turn_id } => {
                InterruptResult::Aborted { turn_id }
            }
            crate::codex_host::InterruptReport::Duplicate { turn_id } => {
                InterruptResult::Duplicate { turn_id }
            }
            crate::codex_host::InterruptReport::NotApplied(reason) => {
                InterruptResult::Rejected { reason }
            }
            crate::codex_host::InterruptReport::Unknown(reason) => {
                InterruptResult::Indeterminate { reason }
            }
        }
    }

    /// **Settle every phone interrupt this daemon was in the middle of when it
    /// stopped.**
    ///
    /// An interrupt has no card, so there is nothing to retire beside the ledger row.
    ///
    /// An `applying` claim under `interrupt` is one whose write may or may not have
    /// reached the app-server — the claim is taken only once the live connection has
    /// accepted the ask, and the write follows immediately — and the turn's terminal,
    /// which is the only evidence there ever was, is a live notification that is never
    /// replayed. So nothing that survives the restart can say, and terminal `Unknown`
    /// is both the truth and the point: the turn is not stopped a second time against
    /// a session that may have moved on.
    async fn recover_codex_interrupts(&self) {
        let claims = match self
            .db
            .unsettled_claims(crate::store::OPERATION_INTERRUPT)
            .await
        {
            Ok(claims) => claims,
            Err(err) => {
                crate::log_error!("recovery: could not read codex interrupt claims: {err:#}");
                return;
            }
        };
        if claims.is_empty() {
            return;
        }
        crate::log_warn!(
            "recovery: {} codex interrupt(s) were in flight; recording them as unknown \
             rather than sending again",
            claims.len()
        );
        for claim in claims {
            self.settle_stranded_interrupt_claim(&claim).await;
        }
    }

    /// **Make one stranded interrupt claim terminal, without being able to say what
    /// it did.**
    async fn settle_stranded_interrupt_claim(&self, claim: &crate::store::MutationClaimRow) {
        match self
            .db
            .settle_mutation_indeterminate(
                crate::store::OPERATION_INTERRUPT,
                claim.session_uid.clone(),
                claim.client_request_id.clone(),
                protocol::time::now_rfc3339(),
            )
            .await
        {
            Ok(true) => crate::log_warn!(
                "the interrupt {} for {} (turn {:?}) is recorded as unknown: it was in \
                 flight when this daemon stopped and will not be sent again",
                claim.client_request_id,
                claim.session_uid,
                claim.claimed.target_turn_id
            ),
            // Something already made it terminal. Nothing to do and nothing wrong:
            // first terminal wins here as it does everywhere else in this ledger.
            Ok(false) => {}
            Err(err) => crate::log_error!(
                "recovery: could not settle the interrupt claim {} for {}: {err:#}",
                claim.client_request_id,
                claim.session_uid
            ),
        }
    }

    /// **Settle every phone compose this daemon was in the middle of when it stopped.**
    ///
    /// [`Daemon::recover_codex_interrupts`]'s counterpart, and the stake is the higher of
    /// the two: an interrupt sent twice stops a turn that is already stopped, and a
    /// compose sent twice puts the same words in the model's mouth again. The claim is
    /// taken only once the live connection has accepted the ask and the write follows
    /// immediately, so an `applying` row is one whose frame may or may not have reached
    /// the app-server — and the answer that would have said which is a response to a
    /// connection that no longer exists. Terminal `Unknown` is both the truth and the
    /// point.
    async fn recover_codex_composes(&self) {
        let claims = match self
            .db
            .unsettled_claims(crate::store::OPERATION_COMPOSE)
            .await
        {
            Ok(claims) => claims,
            Err(err) => {
                crate::log_error!("recovery: could not read codex compose claims: {err:#}");
                return;
            }
        };
        if claims.is_empty() {
            return;
        }
        crate::log_warn!(
            "recovery: {} codex compose(s) were in flight; recording them as unknown \
             rather than saying them again",
            claims.len()
        );
        for claim in claims {
            self.settle_stranded_compose_claim(&claim).await;
        }
    }

    /// **Make one stranded compose claim terminal, without being able to say what it
    /// did.**
    async fn settle_stranded_compose_claim(&self, claim: &crate::store::MutationClaimRow) {
        match self
            .db
            .settle_mutation_indeterminate(
                crate::store::OPERATION_COMPOSE,
                claim.session_uid.clone(),
                claim.client_request_id.clone(),
                protocol::time::now_rfc3339(),
            )
            .await
        {
            Ok(true) => crate::log_warn!(
                "the compose {} for {} (route {}) is recorded as unknown: it was in flight \
                 when this daemon stopped and will not be sent again",
                claim.client_request_id,
                claim.session_uid,
                claim.claimed.route
            ),
            // Something already made it terminal. First terminal wins here as everywhere
            // else in this ledger.
            Ok(false) => {}
            Err(err) => crate::log_error!(
                "recovery: could not settle the compose claim {} for {}: {err:#}",
                claim.client_request_id,
                claim.session_uid
            ),
        }
    }

    /// **Refuse an operation that only makes sense against Claude, naming the run's
    /// own agent.**
    ///
    /// [`ClaudeOnly::Admitted`] when the operation may proceed, and one of two
    /// refusals otherwise. Both carry a sentence a phone can show, because every one
    /// of these is something a person deliberately asked for and is owed an
    /// explanation rather than a silence — and they are kept apart because "this run
    /// is not Claude's" and "this Mac has no such run" are different things to be
    /// told.
    ///
    /// # What is behind it
    ///
    /// **The command catalog**, the slash commands a run's claude reported. A Codex run
    /// has no claude to report them, and the truth to tell is that, rather than a
    /// missing field or an empty menu.
    ///
    /// **An unreadable row refuses**, for [`Daemon::answer`]'s reason: unreadable is
    /// not evidence of Claude.
    pub async fn refuse_unless_claude(&self, session_ref: &str, what: &str) -> ClaudeOnly {
        match self.resolve_optional(session_ref).await {
            Ok(Some(row)) => match row.agent {
                protocol::agent::AgentKind::Claude => ClaudeOnly::Admitted,
                protocol::agent::AgentKind::Codex => ClaudeOnly::WrongAgent(format!(
                    "{} is a Codex session, and {what} is a Claude feature this daemon \
                     does not offer for it",
                    row.session_uid
                )),
                protocol::agent::AgentKind::Antigravity => ClaudeOnly::WrongAgent(format!(
                    "{} is an Antigravity session, and {what} is something this daemon \
                     can only do for Claude",
                    row.session_uid
                )),
                protocol::agent::AgentKind::Unsupported(ref name) => {
                    ClaudeOnly::WrongAgent(format!(
                        "{} is a {name} session, and {what} is something this daemon \
                         can only do for Claude",
                        row.session_uid
                    ))
                }
            },
            // **A reference that names nothing is refused here too, and the caller
            // still chooses the words.** It used to be admitted, on the reasoning that
            // an unknown session is the caller's sentence to write rather than this
            // gate's — which is true, and is why the two refusals are told apart
            // instead of being merged. What was not true is that admitting it was
            // free: this gate's whole contract is to fail closed, and a row this
            // daemon could not find is not evidence that the run is Claude's.
            Ok(None) => {
                ClaudeOnly::Unknown(format!("{session_ref} is not a run the computer hosts"))
            }
            Err(err) => ClaudeOnly::Unknown(format!("session lookup failed: {err}")),
        }
    }

    /// Apply a phone answer. Idempotent by `(session, request_id)`, guarded by
    /// `payload_hash`.
    pub async fn answer(
        &self,
        request_id: &str,
        payload_hash: &str,
        decision: AnswerDecision,
        session_ref: Option<&str>,
        answered_on: AnsweredOn,
    ) -> AnswerResult {
        let session_uid = match self.approval_target(request_id, session_ref).await {
            Ok(uid) => uid,
            Err(reason) => return AnswerResult::Rejected { reason },
        };

        // **Refused before the claim, and before the decision below.** See
        // [`Daemon::shared_ledgers_admit`]. `approval_target` answers *which run*
        // and never *which agent*, and no check between here and step 4 asks:
        // `claim_answer` writes `answer_claims` with no existence guard of its
        // own, and a refusal after the claim releases the claim it finds, so the
        // leak it would leave is a window rather than a residue: a rolled-back
        // daemon starting inside it deletes the claim and the card as its own,
        // and a crash inside it leaves a claim only `settle_indeterminate` can
        // retire — into `answers`, the fourth shared table.
        //
        // **It is now a fork rather than a dead end, and the gate has not moved.**
        // A Codex answer is not typed and touches none of these four tables; it
        // goes to [`Daemon::answer_codex`], whose durable claim lands in
        // `mutation_ledger` — the generalized Codex ledger, which no v0.6.0
        // statement names. What `shared_ledgers_admit` says is unchanged — these
        // ledgers are Claude's — and what changed is that the other agent now has
        // somewhere to be sent instead of nowhere.
        //
        // **The branch names the agent it routes to, and refuses the rest.**
        // `AgentKind::Unsupported` is a run some future build wrote; its contract
        // is that every actuation site fails closed, and "not Claude" is not
        // evidence of Codex. Refused here, before any lock or durable claim, so a
        // database from a build this one does not understand costs nothing but a
        // sentence.
        let codex = match self.db.get_session(session_uid.clone()).await {
            Ok(Some(row)) => match row.agent {
                // A hosted run: its agent host holds the request.
                protocol::agent::AgentKind::Claude | protocol::agent::AgentKind::Antigravity => {
                    None
                }
                protocol::agent::AgentKind::Codex => Some(row),
                protocol::agent::AgentKind::Unsupported(ref name) => {
                    return AnswerResult::Rejected {
                        reason: format!(
                            "{session_uid} is a {name} session, which this daemon does not \
                             know how to answer; nothing was sent"
                        ),
                    }
                }
            },
            // No row: the uid came from the answers ledger for a run since
            // deleted (`approval_target`'s replay arm), so there is no agent to
            // read and step 1 below replays the stored outcome as it always has.
            Ok(None) => None,
            // The same shape `approval_target` uses for a failed session lookup.
            // Unreadable is not evidence of Claude, and this gate is the one
            // check on this path that may not be skipped on a bad day.
            // Logged here, not carried, for `approval_target`'s reason — this is the
            // same shape and it says the same sentence.
            Err(err) => {
                crate::log_error!("answer for {session_uid}: session lookup failed: {err:#}");
                return AnswerResult::Rejected {
                    reason: "session lookup failed".into(),
                };
            }
        };

        let id: ApprovalId = (session_uid.clone(), request_id.to_string());

        // 0. Serialise on this one approval. Two taps arriving together used to
        //    make the second one a *rejection* ("already being applied"), which
        //    contradicts the rule that a duplicate returns the original
        //    outcome — and made a double-tap on a flaky link look like a
        //    failure. Waiting for the winner costs the handoff's latency and
        //    turns the racer into a well-formed duplicate.
        let gate = self.answer_gate(&id).await;
        let _serialised = gate.lock().await;

        // **The fork, taken under the same serialisation and no earlier.** Two
        // taps on one Codex card queue behind each other here exactly as two on a
        // Claude card do, so the second is a well-formed duplicate that reads the
        // first one's row rather than a race that claims twice.
        if let Some(row) = codex {
            return self
                .answer_codex(&row, request_id, payload_hash, decision, answered_on)
                .await;
        }

        // 1. Durable ledger wins over everything: an already-answered request is
        //    a no-op that replays the original outcome.
        match self
            .db
            .get_answer(session_uid.clone(), request_id.to_string())
            .await
        {
            Ok(Some((stored_hash, outcome))) => {
                return AnswerResult::Duplicate {
                    outcome,
                    stale_payload_hash: stored_hash != payload_hash,
                };
            }
            Ok(None) => {}
            Err(err) => {
                crate::log_error!("ledger read failed: {err:#}");
                return AnswerResult::Rejected {
                    reason: "ledger unavailable".into(),
                };
            }
        }

        // A hosted run's card is answered through its agent host, and its claim is
        // settled by the host's outcome. No other Claude card takes an answer.
        match self.db.hosted_link(session_uid.clone()).await {
            Ok(Some(_)) => {
                crate::claude_host::answer(
                    self,
                    &session_uid,
                    request_id,
                    payload_hash,
                    decision,
                    answered_on,
                )
                .await
            }
            Ok(None) => AnswerResult::Rejected {
                reason: "Only a run CodeConnect started takes answers; nothing was sent.".into(),
            },
            Err(err) => {
                crate::log_error!(
                    "answer for {session_uid}: could not read its host link: {err:#}"
                );
                AnswerResult::Rejected {
                    reason: "This computer could not read the session; nothing was sent.".into(),
                }
            }
        }
    }

    /// A message from the phone to a Claude run CodeConnect started, which its host
    /// delivers ([`crate::claude_host::send_text`]). `session_ref` is a uid or a name.
    ///
    /// A mutation with an identity, so a phone on a flaky link can retry the same
    /// message without it arriving twice. The body is **bounded**: a megabyte is not
    /// a message.
    ///
    /// `attachments` are `connection`'s uploads (minor 50), as [`Daemon::compose`] takes them.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_text(
        self: &Arc<Self>,
        session_ref: &str,
        text: String,
        request_id: Option<&str>,
        payload_hash: Option<&str>,
        submit: bool,
        connection: u64,
        attachments: &[String],
    ) -> SendTextResult {
        let arrived = tokio::time::Instant::now();
        if text.len() > protocol::ws::MAX_SEND_TEXT_BYTES {
            return SendTextResult::Refused {
                reason: format!(
                    "text is {} bytes; the ceiling is {}",
                    text.len(),
                    protocol::ws::MAX_SEND_TEXT_BYTES
                ),
            };
        }
        let session_uid = match self.resolve(session_ref).await {
            // **Refused here, on the row the resolver already read**, before the
            // host writes its durable claim. See [`Daemon::shared_ledgers_admit`]
            // for what a rolled-back daemon does to a `text_mutations` row it finds.
            Ok(row) if !self.shared_ledgers_admit(&row.agent) => {
                return SendTextResult::Refused {
                    reason: format!(
                        "{} is a {} session and this daemon only sends messages to Claude \
                         sessions; nothing was sent",
                        row.session_uid,
                        row.agent.as_str()
                    ),
                }
            }
            Ok(row) => row.session_uid,
            Err(err) => {
                return SendTextResult::Refused {
                    reason: format!("{err}"),
                }
            }
        };
        // The identity, if the client offered one. A `request_id` without a
        // hash is refused rather than trusted: the id alone says "this is a
        // retry" without saying a retry *of what*, and the ledger would then
        // happily suppress a completely different takeover.
        let identity = match (request_id, payload_hash) {
            (Some(request_id), Some(given)) => {
                let expected =
                    protocol::hash::send_text_hash_with(session_ref, &text, submit, attachments);
                if !constant_time_eq(given.as_bytes(), expected.as_bytes()) {
                    return SendTextResult::Refused {
                        reason: "payload_hash does not match the text, session and submit flag \
                                 in this request"
                            .into(),
                    };
                }
                Some((request_id.to_string(), expected))
            }
            (Some(_), None) => {
                return SendTextResult::Refused {
                    reason: "request_id without payload_hash: an idempotent mutation has to say \
                             what it is a retry of"
                        .into(),
                }
            }
            (None, _) => None,
        };

        // A Claude run started in an agent host takes the message through its host;
        // no other run has anything to deliver it. Read and sent under the run's move gate while it has a switch.
        let _moving = self.move_guard(&session_uid).await;
        match self.db.hosted_link(session_uid.clone()).await {
            Ok(Some(linked)) => {
                // The files are the message's from here, before anything is said. A message the
                // ledger already holds claimed them when it was first sent, and is answered from
                // its record, after a restart too.
                let recorded = match &identity {
                    Some((request_id, _)) => matches!(
                        self.db.send_request_ids(session_uid.clone()).await,
                        Ok(ids) if ids.contains(request_id)
                    ),
                    None => false,
                };
                let text = if recorded {
                    text
                } else {
                    match self.claim_uploads(connection, &session_uid, attachments, text) {
                        Ok(text) => text,
                        Err(reason) => return SendTextResult::Refused { reason },
                    }
                };
                crate::claude_host::send_text(
                    self,
                    &session_uid,
                    linked.lifecycle,
                    crate::claude_host::Message {
                        text,
                        identity,
                        submit,
                        arrived,
                    },
                )
                .await
            }
            Ok(None) => SendTextResult::Refused {
                reason: "Only a run CodeConnect started takes messages; nothing was sent.".into(),
            },
            Err(err) => {
                crate::log_error!("send_text for {session_uid}: could not read its run: {err:#}");
                SendTextResult::Refused {
                    reason: "This computer could not read the session; nothing was sent.".into(),
                }
            }
        }
    }

    /// `text` with `attachments`' paths, once `connection` has handed them to this message
    /// ([`crate::uploads::Uploads::claim`]); why not, with nothing attached.
    fn claim_uploads(
        &self,
        connection: u64,
        session_uid: &str,
        attachments: &[String],
        text: String,
    ) -> std::result::Result<String, String> {
        if attachments.is_empty() {
            return Ok(text);
        }
        let paths = self.uploads.claim(
            &crate::uploads::root(),
            connection,
            session_uid,
            attachments,
        )?;
        Ok(crate::uploads::attached(text, &paths))
    }

    /// Which slash commands this session's Claude Code has: for a run CodeConnect
    /// started, what its claude last reported ([`crate::claude_host::catalog`]).
    /// `Unavailable` is a complete answer — the phone falls back to its
    /// conservative static policy — so every failure path returns one with its
    /// reason rather than an error.
    pub async fn command_catalog(&self, session_ref: &str) -> protocol::ws::CommandCatalogResult {
        use protocol::ws::CommandCatalogResult;

        let row = match self.resolve(session_ref).await {
            Ok(row) => row,
            Err(err) => {
                return CommandCatalogResult::Unavailable {
                    reason: format!("{err}"),
                }
            }
        };
        // **The agent, read explicitly, before the menu is looked for**: a Codex run has
        // no claude to report one. See [`Daemon::refuse_unless_claude`].
        //
        // **Asked about the row this function already resolved**, not about the
        // reference again. A name can resolve to a different run between two reads,
        // so resolving twice makes the agent this refuses on a possibly different run
        // from the one it answers for.
        if let Some(reason) = self
            .refuse_unless_claude(&row.session_uid, "the slash-command list")
            .await
            .reason()
        {
            return CommandCatalogResult::Unavailable { reason };
        }
        match self.db.hosted_commands(row.session_uid.clone()).await {
            Ok(Some(kept)) => crate::claude_host::catalog(kept),
            Ok(None) => CommandCatalogResult::Unavailable {
                reason: "only a run CodeConnect started reports its commands".into(),
            },
            Err(err) => CommandCatalogResult::Unavailable {
                reason: format!("{err:#}"),
            },
        }
    }

    // ------------------------------------------------------------------ ends

    /// What follows a run's recorded end, its `SessionEnd` already in the log: its
    /// cards closed. `commit` is the run's registration gate, held since the end was
    /// recorded; it is released before the cards close.
    async fn after_the_end(&self, session: &SessionKey, commit: tokio::sync::OwnedMutexGuard<()>) {
        // Nobody is left to answer this run's cards, whichever agent asked them.
        // They are read under the gate the end was recorded under, and closed
        // after it is released, so an answer in flight on one finishes first.
        let open: Vec<String> = self
            .inner
            .lock()
            .await
            .pending
            .keys()
            .filter(|(uid, _)| *uid == session.uid)
            .map(|(_, request_id)| request_id.clone())
            .collect();
        drop(commit);
        for request_id in open {
            self.close_card_of_ended_run(session, &request_id).await;
        }
    }

    /// End the hosted run `session_uid`'s launch whose leader is `host` (H6, H7): in one
    /// transaction, with `seq` the host's message that reports it, the row from `from` to
    /// `to` and its `SessionEnd` with `payload`, published as `ingest_host_message`
    /// publishes; then what follows any run's end. The end's id names the launch,
    /// `exit:{uid}:{leader pid}`, so a run a message relaunched files each of its ends. `false`
    /// when `host` is no longer the run's recorded host, or the cursor did not move: nothing
    /// was written, and the message must not be acknowledged.
    pub(crate) async fn end_hosted_run(
        &self,
        session_uid: &str,
        host: protocol::proc_identity::ProcessIdentity,
        seq: Option<u64>,
        from: Lifecycle,
        to: Lifecycle,
        payload: serde_json::Value,
    ) -> Result<bool> {
        // A hosted run is named by its uid.
        let session = SessionKey::new(session_uid, session_uid);
        let gate = self.registration_gate(session_uid).await;
        let commit = gate.lock_owned().await;
        let end = PendingEvent::new(&session, EventKind::SessionEnd, payload, Source::Daemon)
            .with_source_event_id(format!("exit:{session_uid}:{}", host.pid));
        let publish = self.publish_gate(session_uid).await;
        let events = {
            let _ordered = publish.lock().await;
            let Some(events) = self
                .db
                .end_hosted_run(session_uid.to_string(), host, seq, from, to, end)
                .await?
            else {
                return Ok(false);
            };
            for event in &events {
                let _ = self.events_tx.send(event.clone());
            }
            events
        };
        if events.is_empty() {
            // The row had already left `from`: someone else ended it.
            return Ok(true);
        }
        self.published(session_uid, &events).await;
        self.end_mail(session_uid).await;
        // The run's shell ends with it, unless this end is an account switch's: the run is moved, not ended, and
        // keeps its shell (`switch::ended` ends it when that run is ended in its turn).
        if self
            .db
            .switch_state(session_uid.to_string())
            .await
            .ok()
            .flatten()
            != Some(crate::store::Store::SWITCH_MOVED)
        {
            self.terminals
                .end(session_uid, protocol::ws::terminal_closed::RUN_ENDED);
        }
        self.after_the_end(&session, commit).await;
        Ok(true)
    }

    // -------------------------------------------------------------- queries

    /// The fleet, as a description to render.
    ///
    /// # Every database read happens BEFORE the lock
    ///
    /// Two of them: the session rows, and each row's `max_seq`. The second used to be
    /// awaited *inside* the loop, with `inner` held — which is the thing the lock
    /// ordering forbids (see the restart recovery, which counts approval requests up
    /// front to avoid the same shape) and which serialised every other task behind one
    /// listing's worth of blocking-pool round trips. Hoisted, so the locked section is
    /// pure in-memory assembly and contains no awaits at all.
    ///
    /// # A row and its link are read at two moments, and that is ACCEPTED
    ///
    /// The rows come from the store and the link and `blocked_on` come from `inner`, so
    /// a link attaching or ending between the two reads is listed as it stood a moment
    /// before. This is a *display*: it is re-read continuously, it resolves itself on the
    /// next listing, and nothing is actuated from it.
    pub async fn sessions(&self) -> Result<Vec<SessionSummary>> {
        // A move can end while a listing is read (its run live again, its mark changed, its gate let go, in that
        // order): a listing whose marks changed meanwhile is read again, so none lists a moving run as it stood half
        // moved. Each read again follows a mark a move changed.
        loop {
            let (sessions, switching) = self.listing().await?;
            if self.switch_marks().await? == switching {
                return Ok(sessions);
            }
        }
    }

    /// The runs' switch marks ([`crate::store::Store::SWITCH_WAITING`] and on), by run.
    async fn switch_marks(&self) -> Result<HashMap<String, i64>> {
        Ok(self.db.switching_runs().await?.into_iter().collect())
    }

    /// [`Daemon::sessions`] as read once, with the switch marks it was read with.
    async fn listing(&self) -> Result<(Vec<SessionSummary>, HashMap<String, i64>)> {
        // A run moving to another account reads live throughout (306 Slice E, S1). Its mark is read before its row:
        // the mark goes only once the run is live again, so a row read after it is never an earlier one unmarked.
        let switching = self.switch_marks().await?;
        let accounts: HashMap<String, crate::store::RecordedAccount> =
            self.db.run_accounts().await?.into_iter().collect();
        let mut limits: HashMap<String, protocol::event::RunLimit> =
            self.db.run_limits().await?.into_iter().collect();
        let rows = self.db.list_sessions().await?;
        // Read up front, before the lock is taken, one query per row.
        let mut last_seqs: HashMap<String, u64> = HashMap::with_capacity(rows.len());
        for row in &rows {
            last_seqs.insert(
                row.session_uid.clone(),
                self.db.max_seq(row.session_uid.clone()).await?,
            );
        }
        // Every run's title, as kept.
        let titles = self.db.run_titles().await?;
        // And the request event of every open card, one query per card on the
        // dedup index: the seq its pending ref names, and a Codex card's turn. The
        // cards are snapshotted first, so one raised after the snapshot is listed in
        // `blocked_on` without a ref this time; its event reaches the phone live.
        let cards: Vec<(ApprovalId, Source)> = {
            let inner = self.inner.lock().await;
            inner
                .pending
                .iter()
                .map(|(id, entry)| (id.clone(), request_event_source(entry)))
                .collect()
        };
        let mut requests: HashMap<ApprovalId, (u64, Option<String>)> =
            HashMap::with_capacity(cards.len());
        for ((uid, request_id), source) in cards {
            let filed = self
                .db
                .filed_event(uid.clone(), source, format!("perm:{request_id}"))
                .await?;
            if let Some(filed) = filed {
                requests.insert((uid, request_id), filed);
            }
        }
        // A live run CodeConnect started offers its settings: a Claude run's from its start's
        // answer, with the one its model fact names marked (its newest receipt, kept in its
        // facts, read for that); a Codex run's from its start's check and the model it started
        // on, and an Antigravity run's from its start's outcome, each with its model fact marked as it is.
        let mut menus: HashMap<String, protocol::event::RunMenus> = HashMap::new();
        // Every Claude run CodeConnect started keeps its start's models, ended or not: its model
        // fact is named by the chosen entry's id, never by claude's receipt of a change.
        let mut choosing: Vec<(String, Vec<serde_json::Value>)> = Vec::new();
        let mut codex_menus: Vec<String> = Vec::new();
        // A Codex run that is not live keeps its menu only to name its model by.
        let mut codex_ended: Vec<(String, protocol::event::RunMenus)> = Vec::new();
        for row in &rows {
            // A run its switch is moving offers them while a message moves it (its move gate held): its next process
            // takes a change. Ended by its switch with nothing moving it, it offers none, as an ended run.
            let live = row.lifecycle == Lifecycle::Live
                || switching.get(&row.session_uid) == Some(&crate::store::Store::SWITCH_MOVED)
                    && self.move_gate(&row.session_uid).await.try_lock().is_err();
            let Some(models) = self.db.hosted_models(row.session_uid.clone()).await? else {
                continue;
            };
            match row.agent {
                protocol::agent::AgentKind::Claude => {
                    if live {
                        menus.insert(row.session_uid.clone(), crate::claude_host::menus(&models));
                    }
                    choosing.push((row.session_uid.clone(), models));
                }
                protocol::agent::AgentKind::Codex if live => {
                    menus.insert(row.session_uid.clone(), crate::codex_host::menus(&models));
                    codex_menus.push(row.session_uid.clone());
                }
                protocol::agent::AgentKind::Codex => {
                    codex_ended.push((row.session_uid.clone(), crate::codex_host::menus(&models)));
                }
                // agy's model families and modes, its model fact a family as its menu names it, as Codex's is.
                protocol::agent::AgentKind::Antigravity => {
                    let modes = self.db.hosted_modes(row.session_uid.clone()).await?;
                    let menu = crate::claude_host::acp_menus(&models, &modes);
                    if live {
                        menus.insert(row.session_uid.clone(), menu);
                        codex_menus.push(row.session_uid.clone());
                    } else {
                        codex_ended.push((row.session_uid.clone(), menu));
                    }
                }
                protocol::agent::AgentKind::Unsupported(_) => {}
            }
        }
        // An ended run the person's next message relaunches, of either agent.
        let mut relaunchable: HashSet<String> = HashSet::new();
        for row in &rows {
            if row.lifecycle == Lifecycle::Exited
                && self.db.relaunchable(row.session_uid.clone()).await?
            {
                relaunchable.insert(row.session_uid.clone());
            }
        }
        let inner = self.inner.lock().await;
        let mut model_ids: HashMap<String, String> = HashMap::new();
        let mut model_labels: HashMap<String, String> = HashMap::new();
        for (uid, models) in &choosing {
            let Some(facts) = inner.session_facts.get(uid) else {
                continue;
            };
            let Some(fact) = facts.model.as_ref() else {
                continue;
            };
            let receipt = facts.model_receipt.as_ref().map(|fact| fact.value.as_str());
            let chosen = crate::claude_host::selected_model(models, &fact.value, receipt);
            if let Some(id) = chosen
                .as_deref()
                .and_then(|value| crate::claude_host::entry_model_id(models, value))
            {
                if !menus.contains_key(uid) {
                    if let Some(label) = crate::claude_host::entry_label(models, &id) {
                        model_labels.insert(uid.clone(), label);
                    }
                }
                model_ids.insert(uid.clone(), id);
            }
            if let Some(menu) = menus.get_mut(uid) {
                menu.selected_model = chosen;
            }
        }
        for uid in &codex_menus {
            let fact = inner
                .session_facts
                .get(uid)
                .and_then(|facts| facts.model.as_ref());
            if let (Some(fact), Some(menu)) = (fact, menus.get_mut(uid)) {
                menu.selected_model = menu
                    .models
                    .iter()
                    .any(|model| model.value == fact.value)
                    .then(|| fact.value.clone());
            }
        }
        // A Codex or Antigravity entry's label is its model's name, else the model itself: only a name is given.
        for (uid, menu) in &codex_ended {
            let fact = inner
                .session_facts
                .get(uid)
                .and_then(|facts| facts.model.as_ref());
            if let Some(entry) = fact.and_then(|fact| {
                menu.models
                    .iter()
                    .find(|model| model.value == fact.value && model.label != model.value)
            }) {
                model_labels.insert(uid.clone(), entry.label.clone());
            }
        }
        let mut sessions = Self::assemble_sessions(
            &inner,
            rows,
            &last_seqs,
            &requests,
            &menus,
            &relaunchable,
            &titles,
        );
        for session in &mut sessions {
            if let (Some(fact), Some(id)) =
                (session.model.as_mut(), model_ids.get(&session.session_uid))
            {
                fact.value = id.clone();
            }
            session.model_label = model_labels.remove(&session.session_uid);
            // The account its process runs on: Default, or an account still kept; one since removed is not known.
            session.account = match accounts.get(&session.session_uid) {
                Some(None) => Some(protocol::event::RunAccount {
                    id: None,
                    label: "Default".to_string(),
                }),
                Some(Some((id, Some(label)))) => Some(protocol::event::RunAccount {
                    id: Some(id.clone()),
                    label: label.clone(),
                }),
                _ => None,
            };
            session.limit = limits.remove(&session.session_uid);
            match switching.get(&session.session_uid).copied() {
                Some(crate::store::Store::SWITCH_FAILED) => {
                    session.switching = Some(protocol::event::Switching::Failed)
                }
                // Due to move, or moving: its process being ended by a message and its next one started, when it
                // reads linked throughout, as a run that is answered and takes a message.
                Some(state) => {
                    session.lifecycle = Lifecycle::Live;
                    session.relaunchable = false;
                    session.switching = Some(protocol::event::Switching::Waiting);
                    if state == crate::store::Store::SWITCH_MOVED {
                        session.switching = Some(protocol::event::Switching::Moved);
                        session.link = protocol::event::Link::Attached;
                        if session.agent == protocol::agent::AgentKind::Codex {
                            session.codex_link = protocol::event::CodexLink::Subscribed;
                        }
                    }
                }
                None => {}
            }
        }
        Ok((sessions, switching))
    }

    /// The run `session_uid`'s agent host or app-server is attached, by `link`. Then, with the
    /// link in place, a `link_state` fact says so, and an app can read the fleet again on it. A
    /// Codex run is `live` by then, so the fleet offers its controls; a Claude run's link
    /// attaches while the run is `starting`, and the start's outcome makes it `live`.
    pub(crate) async fn host_link_attached(&self, session_uid: &str, link: HostLink) {
        self.inner
            .lock()
            .await
            .host_links
            .insert(session_uid.to_string(), link);
        self.file_host_link_state(session_uid, "attached", "agent host linked")
            .await;
    }

    /// Whether the Claude run `session_uid`'s link, attached again to its running host, has yet to read the end of that
    /// host's replay: until it has, what ccd has filed of the run may be behind what its host holds, so no message moves
    /// the run ([`crate::switch::moves`]).
    pub(crate) async fn replaying(&self, session_uid: &str) -> bool {
        self.inner.lock().await.replaying.contains(session_uid)
    }

    /// The Claude run `session_uid`'s link has begun reading its host's replay (`true`) or has read its end (`false`).
    pub(crate) async fn set_replaying(&self, session_uid: &str, replaying: bool) {
        let mut inner = self.inner.lock().await;
        if replaying {
            inner.replaying.insert(session_uid.to_string());
        } else {
            inner.replaying.remove(session_uid);
        }
    }

    /// A hosted run's link change as a `link_state` fact.
    async fn file_host_link_state(&self, session_uid: &str, link: &str, reason: &str) {
        let pending = PendingEvent::new(
            &SessionKey::new(session_uid, session_uid),
            EventKind::LinkState,
            serde_json::json!({"link": link, "reason": reason}),
            Source::Daemon,
        );
        if let Err(err) = self.ingest(pending).await {
            crate::log_error!("{session_uid}: could not record its link {link}: {err:#}");
        }
    }

    /// Whether a card of the run `session_uid` waits on an answer.
    pub(crate) async fn has_pending_card(&self, session_uid: &str) -> bool {
        self.inner
            .lock()
            .await
            .pending
            .keys()
            .any(|(uid, _)| uid == session_uid)
    }

    /// Send the run `session_uid`'s agent host `command` through its link. Whether a Claude
    /// link was attached to take it.
    pub(crate) async fn send_to_host(
        &self,
        session_uid: &str,
        command: protocol::host::ToHost,
    ) -> bool {
        match self.inner.lock().await.host_links.get(session_uid) {
            Some(HostLink::Claude(link)) => link.send(command).is_ok(),
            _ => false,
        }
    }

    /// The first, graceful step of ending the run `session_uid`, through its link if one is
    /// attached: a Claude host is sent `stop`; a Codex link interrupts the turn it runs and
    /// sends its app-server's group SIGTERM. Whether a link took it. A run with none is
    /// stopped by the escalation ([`crate::claude_host::escalate_stop`]).
    pub(crate) async fn stop_through_link(&self, session_uid: &str) -> bool {
        let inner = self.inner.lock().await;
        match inner.host_links.get(session_uid) {
            Some(HostLink::Claude(link)) => match protocol::uid::uuid_v4() {
                Ok(uuid) => link.send(protocol::host::ToHost::Stop { uuid }).is_ok(),
                Err(err) => {
                    crate::log_error!("{session_uid}: could not name its stop: {err}");
                    false
                }
            },
            Some(HostLink::Codex(_, commands)) => {
                commands.send(crate::codex_host::LinkCommand::Stop).is_ok()
            }
            None => false,
        }
    }

    /// [`ClientMessage::StopRun`](protocol::ws::ClientMessage::StopRun): end a run
    /// CodeConnect started, Claude's or Codex's ([`crate::claude_host::stop_run`], which
    /// refuses any other).
    pub async fn stop_run(self: &Arc<Self>, session_ref: &str) -> protocol::ws::StopRunResult {
        let refuse = |reason: &str| protocol::ws::StopRunResult::Refused {
            reason: reason.to_string(),
        };
        let row = match self.resolve_optional(session_ref).await {
            Ok(Some(row)) => row,
            Ok(None) => return refuse("There is no such session on this computer."),
            Err(err) => {
                crate::log_error!(
                    "stop for {}: session lookup failed: {err:#}",
                    logged_session_ref(session_ref)
                );
                return refuse("This computer could not read its sessions.");
            }
        };
        let _moving = self.move_guard(&row.session_uid).await;
        crate::claude_host::stop_run(self, &row.session_uid).await
    }

    /// [`ClientMessage::SetModel`](protocol::ws::ClientMessage::SetModel) and
    /// [`ClientMessage::SetMode`](protocol::ws::ClientMessage::SetMode): change a Claude run
    /// CodeConnect started ([`crate::claude_host::set_setting`]), or a Codex one through its
    /// link ([`crate::codex_host::set_setting`], minor 43).
    pub async fn set_setting(
        &self,
        session_ref: &str,
        setting: crate::claude_host::Setting,
    ) -> protocol::ws::SettingResult {
        let refuse = |reason: &str| protocol::ws::SettingResult::Refused {
            reason: reason.to_string(),
        };
        let row = match self.resolve_optional(session_ref).await {
            Ok(Some(row)) => row,
            Ok(None) => return refuse("There is no such session on this computer."),
            Err(err) => {
                crate::log_error!(
                    "setting for {}: session lookup failed: {err:#}",
                    logged_session_ref(session_ref)
                );
                return refuse("This computer could not read its sessions.");
            }
        };
        // Delivered under the run's move gate, marked or not: a choice and a message that come meanwhile move nothing
        // until it is answered.
        let _moving = self.move_gate(&row.session_uid).await.lock_owned().await;
        if row.agent == protocol::agent::AgentKind::Codex {
            let commands = match self.inner.lock().await.host_links.get(&row.session_uid) {
                Some(HostLink::Codex(_, commands)) => Some(commands.clone()),
                _ => None,
            };
            return crate::codex_host::set_setting(self, &row.session_uid, setting, commands).await;
        }
        crate::claude_host::set_setting(self, &row.session_uid, setting).await
    }

    /// [`ClientFrame::StopHostedRuns`](protocol::ipc::ClientFrame::StopHostedRuns), for
    /// `codeconnect daemon uninstall`: every hosted run whose host is alive, a Claude run's
    /// agent host or a Codex run's app-server, is stopped as [`Self::stop_run`] stops one,
    /// read again on each pass so a run that turns live meanwhile is stopped too, until no
    /// host but a starting run's is left, or `within` has passed. What it stopped, and the
    /// pids of the hosts still running.
    pub async fn stop_hosted_runs(self: &Arc<Self>, within: Duration) -> (usize, Vec<u32>) {
        let deadline = tokio::time::Instant::now() + within;
        let mut stopped = HashSet::new();
        loop {
            let mut alive = Vec::new();
            for agent in [
                protocol::agent::AgentKind::Claude,
                protocol::agent::AgentKind::Codex,
            ] {
                match self.db.recorded_hosts(agent).await {
                    Ok(recorded) => alive.extend(recorded.into_iter().filter(|watch| {
                        protocol::proc_identity::liveness(&watch.host)
                            == protocol::proc_identity::Liveness::Alive
                    })),
                    Err(err) => {
                        crate::log_error!("could not read the hosted runs to stop them: {err:#}")
                    }
                }
            }
            for watch in &alive {
                if watch.lifecycle == Lifecycle::Live
                    && crate::claude_host::stop_run(self, &watch.session_uid).await
                        == protocol::ws::StopRunResult::Stopping
                {
                    stopped.insert(watch.session_uid.clone());
                }
            }
            let waiting = alive
                .iter()
                .any(|watch| watch.lifecycle != Lifecycle::Starting);
            if !waiting || tokio::time::Instant::now() >= deadline {
                let left = alive
                    .iter()
                    .filter_map(|watch| u32::try_from(watch.host.pid).ok())
                    .collect();
                return (stopped.len(), left);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// H7: each hosted run that has not ended is checked by its recorded identity, and a host
    /// gone ends its run ([`crate::claude_host::end_if_host_gone`]), each in a task of its
    /// own; but a Claude run whose host's link is attached, since that host is the link's
    /// peer and its group's leader, so the link's end checks it. A Codex run is checked even
    /// while linked: its leader is the launch's, which can die alone while its app-server
    /// keeps the link. On the liveness tick.
    pub async fn watch_hosted_runs(self: &Arc<Self>) {
        let watched = match self.db.watched_hosts().await {
            Ok(watched) => watched,
            Err(err) => {
                crate::log_error!("could not read the hosted runs to watch: {err:#}");
                return;
            }
        };
        let attached: HashSet<String> = self
            .inner
            .lock()
            .await
            .host_links
            .iter()
            .filter(|(_, link)| matches!(link, HostLink::Claude(_)))
            .map(|(uid, _)| uid.clone())
            .collect();
        for watch in watched {
            if attached.contains(&watch.session_uid) {
                continue;
            }
            let daemon = Arc::clone(self);
            tokio::spawn(async move {
                crate::claude_host::end_if_host_gone(&daemon, &watch.session_uid).await;
            });
        }
    }

    /// H8, once at start, before any socket is open. A `starting` hosted Claude run with no
    /// recorded host becomes `failed`: the ccd that began it never sent its `start`. Every
    /// hosted Claude run whose recorded host is alive, whatever its lifecycle, is attached again
    /// from its cursor ([`crate::claude_host::attach_run`]), each in a task of its own, its
    /// runs' folders in `root`: a run going on gets the rest of its messages, and a host
    /// whose run has ended is drained, so it can exit. A run stopping is sent `stop` again
    /// and its escalation starts again ([`crate::claude_host::escalate_stop`]), so a host
    /// that cannot answer is still stopped. A host gone is the liveness watch's
    /// ([`Self::watch_hosted_runs`]). Last, the Codex runs ([`Self::recover_codex_runs`]).
    pub async fn recover_hosted_runs(self: &Arc<Self>, root: &std::path::Path) {
        match self.db.fail_unhosted_starts().await {
            Ok(0) => {}
            Ok(failed) => crate::log_info!(
                "{failed} hosted run(s) had no host recorded when ccd stopped, so they did not start"
            ),
            Err(err) => crate::log_error!("could not fail the hosted runs with no host: {err:#}"),
        }
        if let Err(err) = self.db.recover_moves().await {
            crate::log_error!("could not put back the moves a stopped ccd left: {err:#}");
        }
        let recorded = match self
            .db
            .recorded_hosts(protocol::agent::AgentKind::Claude)
            .await
        {
            Ok(recorded) => recorded,
            Err(err) => {
                crate::log_error!("could not read the hosted runs to attach again: {err:#}");
                return;
            }
        };
        for watch in recorded {
            if protocol::proc_identity::liveness(&watch.host)
                != protocol::proc_identity::Liveness::Alive
            {
                continue;
            }
            if watch.lifecycle == Lifecycle::Stopping {
                tokio::spawn(crate::claude_host::escalate_stop(
                    Arc::clone(self),
                    watch.session_uid.clone(),
                ));
            }
            tokio::spawn(crate::claude_host::attach_run(
                Arc::clone(self),
                root.to_path_buf(),
                watch.session_uid,
                watch.host,
            ));
        }
        self.recover_codex_runs(root, codex_launcher()).await;
    }

    /// The Codex half of [`Self::recover_hosted_runs`], before any socket is open. Every
    /// Codex run still `starting` becomes `failed`, whatever became of its start, and each
    /// recorded app-server's group gets the ladder under its warrant, each in a task of its
    /// own: a start is never sent again. A run `stopping` whose recorded app-server is alive
    /// has its escalation started again ([`crate::claude_host::escalate_stop`]), as a Claude
    /// run's is. A `live` run its switch was ending has its app-server stopped, so its end is the switch's. Then
    /// every other `live` Codex run whose recorded app-server is alive is connected
    /// again ([`crate::codex_host::reconnect`]), each in a task of its own, with `launcher`
    /// as the `codeconnect` its thread config names; one whose app-server is gone is the
    /// liveness watch's.
    pub(crate) async fn recover_codex_runs(
        self: &Arc<Self>,
        root: &std::path::Path,
        launcher: Result<std::path::PathBuf, String>,
    ) {
        match self.db.fail_codex_starts().await {
            Ok((0, _)) => {}
            Ok((failed, hosts)) => {
                crate::log_info!(
                    "{failed} Codex run(s) were starting when ccd stopped, so they did not start"
                );
                for watch in hosts {
                    tokio::spawn(async move {
                        crate::codex_host::stop(&watch.session_uid, watch.host, watch.boot).await;
                    });
                }
            }
            Err(err) => crate::log_error!("could not fail the Codex runs still starting: {err:#}"),
        }
        match self
            .db
            .recorded_hosts(protocol::agent::AgentKind::Codex)
            .await
        {
            Ok(recorded) => {
                for watch in recorded {
                    if watch.lifecycle == Lifecycle::Stopping
                        && protocol::proc_identity::liveness(&watch.host)
                            == protocol::proc_identity::Liveness::Alive
                    {
                        crate::log_info!(
                            "{}: was stopping when ccd stopped; its escalation starts again",
                            watch.session_uid
                        );
                        tokio::spawn(crate::claude_host::escalate_stop(
                            Arc::clone(self),
                            watch.session_uid,
                        ));
                    }
                }
            }
            Err(err) => crate::log_error!("could not read the Codex runs stopping: {err:#}"),
        }
        let mut live = match self.db.codex_live_runs().await {
            Ok(live) => live,
            Err(err) => {
                crate::log_error!("could not read the live Codex runs to connect again: {err:#}");
                return;
            }
        };
        // A run its switch was ending has its app-server's stop finished, not its link again: its end is the switch's.
        let mut moving = Vec::new();
        for run in &live {
            if let Ok(Some(crate::store::Store::SWITCH_MOVED)) =
                self.db.switch_state(run.session_uid.clone()).await
            {
                moving.push(run.session_uid.clone());
                if let Some(watch) = run.host.clone() {
                    tokio::spawn(async move {
                        crate::codex_host::stop(&watch.session_uid, watch.host, watch.boot).await;
                    });
                }
            }
        }
        live.retain(|run| !moving.contains(&run.session_uid));
        if live.is_empty() {
            return;
        }
        let codeconnect = match launcher {
            Ok(codeconnect) => codeconnect,
            Err(why) => {
                crate::log_info!(
                    "{} live Codex run(s) were not connected again: {why}",
                    live.len()
                );
                return;
            }
        };
        for run in live {
            if !run.host.as_ref().is_some_and(|watch| {
                protocol::proc_identity::liveness(&watch.host)
                    == protocol::proc_identity::Liveness::Alive
            }) {
                continue;
            }
            tokio::spawn(crate::codex_host::reconnect(
                Arc::clone(self),
                root.to_path_buf(),
                run,
                codeconnect.clone(),
            ));
        }
    }

    /// The run `session_uid`'s connection to its agent host has ended. A `link_state` fact says
    /// so only while the run is still `live` (its host died, or the connection dropped). A run
    /// that is ending is `stopping` here (a Codex link ends before its run does) or already
    /// `exited` (a Claude host reports claude's exit first), and its end is the fact that tells.
    pub(crate) async fn host_link_detached(&self, session_uid: &str) {
        {
            let mut inner = self.inner.lock().await;
            inner.host_links.remove(session_uid);
            inner.replaying.remove(session_uid);
            inner.host_sends.retain(|_, (uid, _)| uid != session_uid);
            inner.host_stops.retain(|_, (uid, _)| uid != session_uid);
            inner
                .host_settings
                .retain(|_, pending| pending.uid != session_uid);
        }
        // A run its switch is ending lets its link go on purpose: its next process's link is the same run's.
        let live = matches!(
            self.db.get_session(session_uid.to_string()).await,
            Ok(Some(row)) if row.lifecycle == Lifecycle::Live
        ) && !matches!(
            self.db.switch_state(session_uid.to_string()).await,
            Ok(Some(crate::store::Store::SWITCH_MOVED))
        );
        if live {
            self.file_host_link_state(session_uid, "detached", "agent host disconnected")
                .await;
        }
    }

    /// Wait for the outcome of the settings line `uuid` this daemon is sending the run
    /// `session_uid`'s agent host, which sets `effort` when it names one.
    pub(crate) async fn await_host_setting(
        &self,
        session_uid: &str,
        uuid: &str,
        effort: Option<String>,
    ) -> tokio::sync::oneshot::Receiver<(bool, Option<String>)> {
        let (tell, told) = tokio::sync::oneshot::channel();
        self.inner.lock().await.host_settings.insert(
            uuid.to_string(),
            PendingSetting {
                uid: session_uid.to_string(),
                effort,
                tell,
            },
        );
        told
    }

    /// The settings line `uuid`, when this daemon sent it and its outcome has not come.
    pub(crate) async fn take_host_setting(&self, uuid: &str) -> Option<PendingSetting> {
        self.inner.lock().await.host_settings.remove(uuid)
    }

    /// Whether this daemon is attached to the hosted run `session_uid`'s agent host now.
    pub(crate) async fn host_attached(&self, session_uid: &str) -> bool {
        self.inner.lock().await.host_links.contains_key(session_uid)
    }

    /// Wait for the outcome of the app's Stop `uuid` this daemon is sending the run
    /// `session_uid`'s agent host ([`Self::host_stop_told`]).
    pub(crate) async fn await_host_stop(
        &self,
        session_uid: &str,
        uuid: &str,
    ) -> tokio::sync::oneshot::Receiver<crate::codex_host::InterruptReport> {
        let (tell, told) = tokio::sync::oneshot::channel();
        self.inner
            .lock()
            .await
            .host_stops
            .insert(uuid.to_string(), (session_uid.to_string(), tell));
        told
    }

    /// What became of the app's Stop `uuid`, or nobody waits any more (`None`): whoever
    /// waits for it hears so.
    pub(crate) async fn host_stop_told(
        &self,
        uuid: &str,
        report: Option<crate::codex_host::InterruptReport>,
    ) {
        let waiter = self.inner.lock().await.host_stops.remove(uuid);
        if let (Some((_, tell)), Some(report)) = (waiter, report) {
            let _ = tell.send(report);
        }
    }

    /// Wait for the outcome of the message `uuid` this daemon is sending the run
    /// `session_uid`'s agent host ([`Self::host_send_taken`]).
    pub(crate) async fn await_host_send(
        &self,
        session_uid: &str,
        uuid: &str,
    ) -> tokio::sync::oneshot::Receiver<bool> {
        let (tell, told) = tokio::sync::oneshot::channel();
        self.inner
            .lock()
            .await
            .host_sends
            .insert(uuid.to_string(), (session_uid.to_string(), tell));
        told
    }

    /// The host took the message `uuid` into claude's input (`true`) or refused it, or
    /// nobody waits any more: whoever waits for it hears so.
    pub(crate) async fn host_send_taken(&self, uuid: &str, taken: bool) {
        if let Some((_, tell)) = self.inner.lock().await.host_sends.remove(uuid) {
            let _ = tell.send(taken);
        }
    }

    /// Commit the host's outcome for the send `request_id` with its cursor, the person's
    /// words `row` filed when the host took it, then fan them out
    /// ([`crate::store::Store::commit_host_send`]). `false` when the cursor did not move:
    /// nothing was written, and the outcome must not be acknowledged.
    pub(crate) async fn commit_host_send(
        &self,
        session_uid: &str,
        host: protocol::proc_identity::ProcessIdentity,
        seq: u64,
        request_id: &str,
        row: Option<PendingEvent>,
    ) -> Result<bool> {
        let gate = self.publish_gate(session_uid).await;
        let events = {
            let _ordered = gate.lock().await;
            let Some(events) = self
                .db
                .commit_host_send(
                    session_uid.to_string(),
                    host,
                    seq,
                    request_id.to_string(),
                    row,
                )
                .await?
            else {
                return Ok(false);
            };
            for event in &events {
                let _ = self.events_tx.send(event.clone());
            }
            events
        };
        self.published(session_uid, &events).await;
        Ok(true)
    }

    /// Settle the run's claim `request_id` of `operation_kind` as `outcome`, with `event`
    /// filed by that settle alone, then fan out what was filed
    /// ([`crate::store::Store::settle_mutation_with_event`]), and say whether it was. A claim
    /// already terminal, or gone with its run, is left as it is, and nothing is filed.
    pub(crate) async fn settle_with_event(
        &self,
        session_uid: &str,
        operation_kind: &'static str,
        request_id: &str,
        outcome: &str,
        event: PendingEvent,
    ) -> Result<bool> {
        let gate = self.publish_gate(session_uid).await;
        let events = {
            let _ordered = gate.lock().await;
            let events = self
                .db
                .settle_mutation_with_event(
                    operation_kind,
                    session_uid.to_string(),
                    request_id.to_string(),
                    outcome.to_string(),
                    protocol::time::now_rfc3339(),
                    event,
                )
                .await?;
            for event in &events {
                let _ = self.events_tx.send(event.clone());
            }
            events
        };
        self.published(session_uid, &events).await;
        Ok(!events.is_empty())
    }

    /// Commit the run's start outcome with its cursor and, when it makes the run live,
    /// `events`, the person's first words, then fan them out
    /// ([`crate::store::Store::commit_outcome`]); a relaunch's start settles the person's
    /// message `send` with it. `false` when the cursor did not move: nothing was written, and
    /// the outcome must not be acknowledged.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn commit_start_outcome(
        &self,
        session_uid: &str,
        host: protocol::proc_identity::ProcessIdentity,
        seq: u64,
        lifecycle: Lifecycle,
        initialize: Option<String>,
        events: Vec<PendingEvent>,
        send: Option<String>,
    ) -> Result<bool> {
        let gate = self.publish_gate(session_uid).await;
        let events = {
            let _ordered = gate.lock().await;
            let Some(events) = self
                .db
                .commit_outcome(
                    session_uid.to_string(),
                    host,
                    seq,
                    lifecycle,
                    initialize,
                    events,
                    send,
                )
                .await?
            else {
                return Ok(false);
            };
            for event in &events {
                let _ = self.events_tx.send(event.clone());
            }
            events
        };
        self.published(session_uid, &events).await;
        Ok(true)
    }

    /// The hosted Claude run `uid`'s model as its menu marks it chosen
    /// ([`crate::claude_host::selected_model`], what the fleet's `menus.selected_model` is): from
    /// its start's kept models, its model fact and its newest receipt of a model change, which
    /// all outlive the run's end. `None` when it has no kept models or no model fact, or the
    /// fact names none of them.
    pub(crate) async fn selected_model(&self, uid: &str) -> Result<Option<String>> {
        let Some(models) = self.db.hosted_models(uid.to_string()).await? else {
            return Ok(None);
        };
        let inner = self.inner.lock().await;
        let Some(facts) = inner.session_facts.get(uid) else {
            return Ok(None);
        };
        let receipt = facts.model_receipt.as_ref().map(|fact| fact.value.as_str());
        Ok(facts
            .model
            .as_ref()
            .and_then(|fact| crate::claude_host::selected_model(&models, &fact.value, receipt)))
    }

    /// Commit one of a hosted run's card lines with its cursor ([`Db::commit_host_card`])
    /// under the run's publish order, then fan out what it filed. `None` when the cursor
    /// did not move: nothing was written, and the line must not be acknowledged.
    #[allow(clippy::too_many_arguments)]
    async fn commit_host_card(
        &self,
        uid: &str,
        host: protocol::proc_identity::ProcessIdentity,
        seq: u64,
        request_id: &str,
        card: Option<(String, i64)>,
        events: Vec<PendingEvent>,
    ) -> Result<Option<Vec<Event>>> {
        let gate = self.publish_gate(uid).await;
        let events = {
            let _ordered = gate.lock().await;
            let Some(events) = self
                .db
                .commit_host_card(
                    uid.to_string(),
                    uid.to_string(),
                    host,
                    seq,
                    request_id.to_string(),
                    card,
                    events,
                )
                .await?
            else {
                return Ok(None);
            };
            for event in &events {
                let _ = self.events_tx.send(event.clone());
            }
            events
        };
        self.published(uid, &events).await;
        Ok(Some(events))
    }

    /// A permission request from the agent host of the hosted run `uid`, message `seq`
    /// from the host `host`: today's card, held for the apps and answered only through
    /// the host. Its `approval_request` event and its row are committed with the
    /// cursor, then it rings. A request whose card could not
    /// reach an app whole is refused at once instead: the host is answered deny,
    /// claude reads why, and no card is raised. `false` when the cursor did not move.
    pub(crate) async fn host_card_asked(
        self: &Arc<Self>,
        uid: &str,
        host: protocol::proc_identity::ProcessIdentity,
        seq: u64,
        request: protocol::host::PermissionRequest,
    ) -> Result<bool> {
        let session = SessionKey::new(uid, uid);
        let request_id = request.tool_use_id;
        let option = |key: &str| request.options.get(key).filter(|value| !value.is_null());
        let suggestions = (option("suppressAlwaysAllowRule")
            != Some(&serde_json::Value::Bool(true)))
        .then(|| option("suggestions").cloned())
        .flatten();
        let fork = option("agentID").is_some();
        // A question takes answers, never an allow.
        let question = request.tool_name == protocol::host::ASK_USER_QUESTION;
        let allow_for_session = !question && session_rules(suggestions.as_ref()).is_some();
        let card = ApprovalCard {
            request_id: request_id.clone(),
            payload_hash: approval_payload_hash(&request.tool_name, &request.input),
            display_text: approval_payload_text(&request.tool_name, &request.input),
            risk: Some(protocol::risk::classify(&request.tool_name, &request.input)),
            tool_name: request.tool_name,
            tool_input: request.input,
            permission_suggestions: suggestions,
            prompt_id: None,
            permission_mode: None,
            generation: 0,
            identity_bound: !question,
            question_hold: Some(QuestionHold::Held),
            allow_for_session,
        };
        let payload = serde_json::json!({ "card": card });
        if payload.to_string().len() > self.config.max_payload_bytes {
            crate::log_info!(
                "{uid}: {request_id} is refused: its card would be past the {}-byte payload \
                 limit, which no app could show",
                self.config.max_payload_bytes
            );
            if self
                .commit_host_card(uid, host, seq, &request_id, None, Vec::new())
                .await?
                .is_none()
            {
                return Ok(false);
            }
            let refusal = protocol::host::ToHost::Answer {
                uuid: crate::claude_host::answer_uuid(uid, &request_id),
                tool_use_id: request_id,
                decision: protocol::host::Decision::Deny {
                    message: TOO_LARGE_TO_SHOW.into(),
                    interrupt: false,
                },
            };
            self.send_to_host(uid, refusal).await;
            return Ok(true);
        }
        let created_ms = protocol::time::now_unix_ms();
        let label = self.effective_project_label(uid).await;
        let id: ApprovalId = (uid.to_string(), request_id.clone());
        let registered = {
            let mut inner = self.inner.lock().await;
            match inner.pending.entry(id.clone()) {
                std::collections::hash_map::Entry::Occupied(_) => false,
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(PendingApproval {
                        card: card.clone(),
                        session: session.clone(),
                        project_label: label,
                        agent: protocol::agent::AgentKind::Claude,
                        raise: 0,
                        claimed: false,
                        tool_ran: false,
                        fork,
                        link: true,
                        question,
                    });
                    true
                }
            }
        };
        let event = PendingEvent::new(
            &session,
            EventKind::ApprovalRequest,
            payload,
            Source::Daemon,
        )
        .with_source_event_id(format!("perm:{request_id}"));
        let encoded = serde_json::to_string(&card)?;
        let filed = match self
            .commit_host_card(
                uid,
                host,
                seq,
                &request_id,
                Some((encoded, created_ms)),
                vec![event],
            )
            .await
        {
            Ok(Some(filed)) => filed,
            other => {
                if registered {
                    self.inner.lock().await.pending.remove(&id);
                }
                return other.map(|_| false);
            }
        };
        // A replayed request files nothing and rings nothing.
        if let Some(event) = filed.first() {
            if let Some(ticket) = self.push_gate.admit_decision(uid, &request_id) {
                self.dispatch_push(
                    uid.to_string(),
                    event.seq,
                    ticket,
                    false,
                    Some(CardKey(request_id)),
                );
            }
        }
        Ok(true)
    }

    /// The agent host of the hosted run `uid` withdrew the request `request_id`, as
    /// message `seq` from the host `host`: claude no longer waits on it (an app's Stop,
    /// End run, or claude's end), so its card closes, nobody having answered, with no
    /// answer recorded. While an app's Stop of the run waits (its claim `applying`), the
    /// card says the turn was stopped; otherwise that the session ended. `false` when the
    /// cursor did not move.
    pub(crate) async fn host_card_withdrawn(
        &self,
        uid: &str,
        host: protocol::proc_identity::ProcessIdentity,
        seq: u64,
        request_id: &str,
    ) -> Result<bool> {
        let stopped = matches!(
            self.db
                .unsettled_claims_for(crate::store::OPERATION_INTERRUPT, uid.to_string())
                .await,
            Ok(claims) if !claims.is_empty()
        );
        let detail = if stopped {
            STOPPED_BEFORE_ANSWER
        } else {
            RUN_ENDED
        };
        let closure = host_card_closure(&SessionKey::new(uid, uid), request_id, detail);
        if self
            .commit_host_card(uid, host, seq, request_id, None, vec![closure])
            .await?
            .is_none()
        {
            return Ok(false);
        }
        self.inner
            .lock()
            .await
            .pending
            .remove(&(uid.to_string(), request_id.to_string()));
        Ok(true)
    }

    /// The hosted run `uid`'s cards against its host's `pending` requests, at the end of
    /// a re-attach's replay. A card the host no longer holds was withdrawn while this
    /// daemon was away, and closes. A claimed card whose request the host still holds
    /// never had its answer applied: its claim is released and it is answerable again.
    /// Each card still held takes its `fork` from the host's request.
    pub(crate) async fn host_cards_reconcile(
        &self,
        uid: &str,
        pending: &[protocol::host::PermissionRequest],
    ) {
        let held: HashMap<&str, bool> = pending
            .iter()
            .map(|request| {
                let fork = request
                    .options
                    .get("agentID")
                    .is_some_and(|id| !id.is_null());
                (request.tool_use_id.as_str(), fork)
            })
            .collect();
        let claims = self
            .db
            .answer_claims_of(uid.to_string())
            .await
            .unwrap_or_default();
        let (gone, released) = {
            let mut inner = self.inner.lock().await;
            let mut gone = Vec::new();
            let mut released = Vec::new();
            for ((card_uid, request_id), entry) in inner.pending.iter_mut() {
                if card_uid != uid || !entry.link {
                    continue;
                }
                match held.get(request_id.as_str()) {
                    None => gone.push(request_id.clone()),
                    Some(fork) => {
                        entry.fork = *fork;
                        if claims.iter().any(|claim| &claim.request_id == request_id) {
                            entry.claimed = false;
                            released.push(request_id.clone());
                        }
                    }
                }
            }
            for request_id in &gone {
                inner.pending.remove(&(uid.to_string(), request_id.clone()));
            }
            (gone, released)
        };
        for request_id in released {
            if let Err(err) = self
                .db
                .release_answer_claim(uid.to_string(), request_id.clone())
                .await
            {
                crate::log_error!("{uid}: could not release the claim on {request_id}: {err:#}");
            }
        }
        for request_id in gone {
            self.close_host_card(uid, &request_id).await;
        }
    }

    /// Close the hosted run `uid`'s card `request_id` with no host line to commit it
    /// with: its host withdrew it while this daemon was away, or the run has ended. No
    /// answer is recorded; an answer claimed on it and never confirmed is settled
    /// unknown, since nothing can confirm it any more.
    async fn close_host_card(&self, uid: &str, request_id: &str) {
        if let Ok(Some(claim)) = self
            .db
            .answer_claim(uid.to_string(), request_id.to_string())
            .await
        {
            self.settle_indeterminate(&claim).await;
            return;
        }
        let _ = self
            .db
            .delete_pending_approval(uid.to_string(), request_id.to_string())
            .await;
        if let Err(err) = self
            .ingest(host_card_closure(
                &SessionKey::new(uid, uid),
                request_id,
                RUN_ENDED,
            ))
            .await
        {
            crate::log_error!("{uid}: could not close the card {request_id}: {err:#}");
        }
    }

    /// Hold the outcome of the answer `uuid` sent to a run's agent host, for its link
    /// to record once the host says the answer reached claude.
    pub(crate) async fn hold_host_answer(&self, uuid: &str, outcome: AnswerOutcome) {
        self.inner
            .lock()
            .await
            .host_answers
            .insert(uuid.to_string(), outcome);
    }

    /// The outcome held for the answer `uuid`, if this daemon sent it.
    pub(crate) async fn take_host_answer(&self, uuid: &str) -> Option<AnswerOutcome> {
        self.inner.lock().await.host_answers.remove(uuid)
    }

    /// Mark the hosted run `uid`'s card `request_id` answerable again, its answer
    /// having gone nowhere.
    pub(crate) async fn unclaim_host_card(&self, uid: &str, request_id: &str) {
        if let Some(entry) = self
            .inner
            .lock()
            .await
            .pending
            .get_mut(&(uid.to_string(), request_id.to_string()))
        {
            entry.claimed = false;
        }
    }

    /// Claim the hosted run `uid`'s card `request_id` for the answer `decision`: the card,
    /// the host's decision for it, and its run, or why the answer is refused.
    pub(crate) async fn claim_host_card(
        &self,
        uid: &str,
        request_id: &str,
        payload_hash: &str,
        decision: &AnswerDecision,
        answered_on: AnsweredOn,
    ) -> std::result::Result<(SessionKey, protocol::host::Decision), String> {
        let mut inner = self.inner.lock().await;
        let Some(entry) = inner
            .pending
            .get_mut(&(uid.to_string(), request_id.to_string()))
        else {
            return Err("unknown or already-resolved request".into());
        };
        if entry.card.payload_hash != payload_hash {
            return Err("stale payload_hash: the card you answered is out of date".into());
        }
        let host_decision = host_decision(entry, decision, answered_on)?;
        entry.claimed = true;
        Ok((entry.session.clone(), host_decision))
    }

    /// Remove the hosted run `uid`'s card `request_id`, answered.
    pub(crate) async fn answered_host_card(&self, uid: &str, request_id: &str) {
        self.inner
            .lock()
            .await
            .pending
            .remove(&(uid.to_string(), request_id.to_string()));
    }

    /// Record the host's outcome for an answer with its cursor ([`Db::commit_host_answer`])
    /// under the run's publish order, then fan out what it filed. `false` when the
    /// cursor did not move.
    pub(crate) async fn commit_host_answer(
        &self,
        uid: &str,
        host: protocol::proc_identity::ProcessIdentity,
        seq: u64,
        request_id: &str,
        outcome: Option<(String, AnswerOutcome)>,
    ) -> Result<bool> {
        let events = match &outcome {
            Some((_, recorded)) => vec![PendingEvent::new(
                &SessionKey::new(uid, uid),
                EventKind::ApprovalResolved,
                serde_json::to_value(recorded).unwrap_or(serde_json::Value::Null),
                Source::Daemon,
            )
            .with_source_event_id(format!("resolved:{request_id}"))],
            None => Vec::new(),
        };
        let gate = self.publish_gate(uid).await;
        let events = {
            let _ordered = gate.lock().await;
            let Some(events) = self
                .db
                .commit_host_answer(
                    uid.to_string(),
                    host,
                    seq,
                    request_id.to_string(),
                    outcome,
                    events,
                )
                .await?
            else {
                return Ok(false);
            };
            for event in &events {
                let _ = self.events_tx.send(event.clone());
            }
            events
        };
        self.published(uid, &events).await;
        Ok(true)
    }

    /// Build the fleet listing from rows already read and a state already locked.
    ///
    /// Split out so the read above is plainly *all* database work and this is plainly
    /// *no* database work — the property the lock ordering cares about, made structural
    /// rather than something the loop has to keep remembering.
    fn assemble_sessions(
        inner: &Inner,
        rows: Vec<SessionRow>,
        last_seqs: &HashMap<String, u64>,
        requests: &HashMap<ApprovalId, (u64, Option<String>)>,
        menus: &HashMap<String, protocol::event::RunMenus>,
        relaunchable: &HashSet<String>,
        titles: &HashMap<String, String>,
    ) -> Vec<SessionSummary> {
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            // A hosted run's link is its connection to its agent host or app-server,
            // which is there or not.
            let link = if inner.host_links.contains_key(&row.session_uid) {
                Link::Attached
            } else {
                Link::Detached
            };
            let blocked_on = inner
                .pending
                .values()
                .filter(|p| p.session.uid == row.session_uid)
                .map(|p| p.card.request_id.clone())
                .collect();
            // The same cards, by reference. A card whose request event was not
            // found is not yet in the log, and is left out.
            let mut pending: Vec<PendingRef> = inner
                .pending
                .iter()
                .filter(|((uid, _), _)| *uid == row.session_uid)
                .filter_map(|(id, entry)| {
                    let (seq, turn_id) = requests.get(id)?;
                    Some(PendingRef {
                        request_id: id.1.clone(),
                        seq: *seq,
                        hold: entry.card.question_hold,
                        // The request event's own turn: a Codex card's, and none
                        // on a Claude card, whose request event carries none.
                        turn_id: turn_id.clone(),
                    })
                })
                .collect();
            pending.sort_by_key(|card| card.seq);
            let facts = inner.session_facts.get(&row.session_uid);
            let tree = inner.tree_facts.get(&row.session_uid);
            // A run started on the engine: its own link started or resumed its thread and
            // carries every ask to it, so while that link is attached and the run is live,
            // an ask aimed at it can land.
            let engine_link = matches!(
                inner.host_links.get(&row.session_uid),
                Some(HostLink::Codex(..))
            ) && row.lifecycle == Lifecycle::Live;
            let codex = row.agent == protocol::agent::AgentKind::Codex;
            out.push(SessionSummary {
                session_uid: row.session_uid.clone(),
                session_id: row.session_id.clone(),
                // Resolved here rather than on the phone, because a push has
                // to be named by something the phone cannot compute — it may
                // not be running — and two rules for one name produce two
                // names. Lexical, so it costs nothing on a read this hot. See
                // `project_label`.
                project_label: crate::project_label::project_label(&row.cwd),
                cwd: row.cwd,
                lifecycle: row.lifecycle,
                link,
                // Claude Code's own session, which an app resumes with `claude --resume`: an
                // Antigravity run's ACP session is kept in the same column for its relaunch alone.
                claude_session_id: row.claude_session_id.filter(|_| row.agent.is_claude()),
                transcript_path: row.transcript_path,
                last_seq: last_seqs.get(&row.session_uid).copied().unwrap_or_default(),
                created_at: row.created_at,
                updated_at: row.updated_at,
                blocked_on,
                // The per-session agent fact the client scopes its offerings to.
                agent: row.agent,
                // The thread the run's link started or resumed, as the row holds it.
                codex_thread_id: row.codex_thread_id,
                // **Whether an ask aimed at THIS run can land right now** — the fact
                // `codex_thread_id` above cannot carry. A client that wanted to know
                // whether one session could be stopped had to offer the button and let
                // the refusal answer, which is the affordance this app's rule forbids.
                //
                // A run started on the engine reads `subscribed` while its link is attached and
                // it is live; `bound_not_started` once its app-server ended without ccd stopping
                // it, which the person's next message relaunches ([`crate::codex_host::relaunch`]),
                // so the apps offer the composer and not Stop; and `none` otherwise: before its
                // reconnect, after one that failed, while it is stopping and once End run ended
                // it. Both apps offer Stop and the composer on this field.
                codex_link: if engine_link {
                    protocol::event::CodexLink::Subscribed
                } else if codex && relaunchable.contains(&row.session_uid) {
                    protocol::event::CodexLink::BoundNotStarted
                } else {
                    protocol::event::CodexLink::None
                },
                pending,
                model: facts.and_then(|facts| facts.model.clone()),
                permission_mode: facts.and_then(|facts| facts.permission_mode.clone()),
                // An ended run names the effort its process ended with; any other, the effort its process has now.
                effort: facts.and_then(|facts| {
                    if row.lifecycle == Lifecycle::Exited {
                        facts.ended_with.clone()
                    } else {
                        facts.effort.clone()
                    }
                }),
                menus: menus.get(&row.session_uid).cloned(),
                model_label: None,
                // Read from the cache `refresh_tree` fills, never from git here:
                // this listing is answered on every fleet refresh.
                git_branch: tree.and_then(|tree| tree.branch.clone()),
                diff_stat: tree.and_then(|tree| tree.diff_stat),
                relaunchable: relaunchable.contains(&row.session_uid),
                switching: None,
                account: None,
                title: titles.get(&row.session_uid).cloned(),
                limit: None,
            });
        }
        out
    }

    /// The per-device floor between deliberate test pushes: `None` when this
    /// device may send now (and the send is recorded), or the seconds left.
    pub async fn test_push_gate(&self, device_id: &str) -> Option<u32> {
        const FLOOR: std::time::Duration = std::time::Duration::from_secs(30);
        let mut inner = self.inner.lock().await;
        let now = std::time::Instant::now();
        if let Some(last) = inner.test_pushes.get(device_id) {
            let elapsed = now.duration_since(*last);
            if elapsed < FLOOR {
                // Ceiling, not floor: "try again in 1s" must never be sayable
                // while 1.9s actually remain — a wait the answer understates is
                // a refusal the user cannot act on.
                return Some((FLOOR - elapsed).as_secs_f64().ceil() as u32);
            }
        }
        inner.test_pushes.insert(device_id.to_string(), now);
        None
    }

    /// Remove one ended run, for the phone's swipe.
    ///
    /// The lifecycle rule lives in the SQL, so this cannot weaken it by
    /// forgetting a check. The other guard it *must* carry itself, because it is
    /// a fact no query can see: a run with an approval still open. That is the
    /// same guard [`Self::prune_ended_sessions`] refuses on, and skipping it here
    /// would have made a swipe the one route past a protection the bulk path
    /// calls non-negotiable.
    ///
    /// Not theoretical. Recording a run's end does not retire `inner.pending` at
    /// once, so an approval can outlive the run it belongs to until something
    /// resolves or expires it. Delete the rows underneath one and its later
    /// resolution writes events for a session that no longer exists — the schema
    /// has no foreign key to stop it.
    ///
    /// A refusal reads as `StillRunning` on the wire. That is the honest word for
    /// it: whatever the lifecycle column says, this daemon is still holding live
    /// state for that run.
    pub async fn delete_exited_session(
        &self,
        session_uid: &str,
    ) -> Result<protocol::ws::DeleteSessionResult> {
        {
            let inner = self.inner.lock().await;
            if inner.pending.keys().any(|(uid, _)| uid == session_uid) {
                crate::log_info!(
                    "refused to delete {session_uid}: still holding live state for it"
                );
                return Ok(protocol::ws::DeleteSessionResult::StillRunning);
            }
        }
        let uid = session_uid.to_string();
        let outcome = self.db.delete_exited_session(uid).await?;
        Ok(match outcome {
            crate::store::DeleteOutcome::Deleted { events } => {
                crate::log_info!("deleted session {session_uid} and {events} event(s) on request");
                // Nothing left to ring about, and stale latches must not leak
                // onto a future run.
                self.push_gate.evict_session(session_uid);
                self.uploads
                    .remove_session(&crate::uploads::root(), session_uid);
                protocol::ws::DeleteSessionResult::Deleted { events }
            }
            // The store's word for it, carried through rather than flattened.
            // `Live` and `Starting` mean the agent is there; `Unknown` means the
            // daemon never found out, and those are not the same refusal.
            crate::store::DeleteOutcome::NotExited { lifecycle }
                if lifecycle == crate::store::lifecycle_str(Lifecycle::Unknown) =>
            {
                protocol::ws::DeleteSessionResult::NotExited { lifecycle }
            }
            crate::store::DeleteOutcome::NotExited { .. } => {
                protocol::ws::DeleteSessionResult::StillRunning
            }
            crate::store::DeleteOutcome::NotFound => protocol::ws::DeleteSessionResult::NotFound,
        })
    }

    /// Remove ended runs from the log, at an operator's explicit request.
    ///
    /// The counterpart to reconciliation rather than an afterthought to it. A
    /// real machine accumulates ended sessions exactly the way the owner's did
    /// — 45 rows, most of them from soak runs and long-dead experiments — and
    /// without a supported way to clear them the only options are living with
    /// the clutter or deleting `events.db`, which throws away the history of
    /// the sessions that *are* running along with it.
    ///
    /// Never automatic, and never on a timer. The event log is the source of
    /// truth, and a daemon that pruned it on its own judgement would be
    /// deciding which of somebody's agent history was worth keeping.
    ///
    /// **What it refuses to touch.** Anything that has not ended (`Exited` or
    /// `Failed`) — the store enforces that — plus one thing only this process knows:
    /// a run with an approval still open. That would mean the daemon holds live state
    /// for a session it had just erased the record of. It should be impossible for an
    /// ended row, which is exactly why it is checked: if it ever happens, it is a bug,
    /// and deleting the evidence of it is the worst possible response.
    pub async fn prune_ended_sessions(&self, dry_run: bool) -> Result<Vec<PrunedSession>> {
        let protect: Vec<String> = {
            let inner = self.inner.lock().await;
            inner
                .pending
                .keys()
                .map(|(uid, _)| uid.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        let removed = self.db.prune_exited_sessions(protect, dry_run).await?;
        if !dry_run {
            for row in &removed {
                self.push_gate.evict_session(&row.session_uid);
                self.uploads
                    .remove_session(&crate::uploads::root(), &row.session_uid);
            }
        }
        if !removed.is_empty() && !dry_run {
            crate::log_info!(
                "prune: removed {} ended session(s) and {} event(s) at the operator's request",
                removed.len(),
                removed.iter().map(|row| row.events).sum::<u64>()
            );
        }
        Ok(removed)
    }

    // ------------------------------------------------------------ diff

    /// `git diff HEAD` for a session's working directory.
    ///
    /// The client names a *session*, never a path: the directory comes from our
    /// own registry, so no request can aim this at an arbitrary place on the
    /// filesystem.
    pub async fn diff(&self, session_ref: &str) -> Result<crate::git::Diff> {
        let row = self.resolve(session_ref).await?;
        let gate = self.git_gate(&row.cwd).await;
        let _one_at_a_time = gate.lock().await;
        Ok(crate::git::collect(
            &row.cwd,
            self.config.git_bin.as_deref(),
            self.config.diff_max_bytes,
            Duration::from_millis(self.config.diff_timeout_ms),
        )
        .await)
    }

    /// A run's repository for its Changes panel, read in the run's own folder from our
    /// registry, as [`Self::diff`] is. The inner `Err` is the note saying why git gave none.
    pub async fn run_git(
        &self,
        session_ref: &str,
    ) -> Result<std::result::Result<protocol::ws::RunGit, String>> {
        let row = self.resolve(session_ref).await?;
        let timeout = Duration::from_millis(self.config.diff_timeout_ms);
        let git_bin = self.config.git_bin.as_deref();
        let (read, urls) = {
            let gate = self.git_gate(&row.cwd).await;
            let _one_at_a_time = gate.lock().await;
            let read = crate::git::run_git(&row.cwd, git_bin, timeout).await;
            let urls = match &read {
                Ok(git) => match &git.branch {
                    Some(branch) => {
                        crate::git::push_remote_urls(&row.cwd, git_bin, branch, timeout).await
                    }
                    None => None,
                },
                Err(_) => None,
            };
            (read, urls)
        };
        let Ok(mut git) = read else {
            return Ok(read);
        };
        let Some(branch) = git.branch.clone() else {
            return Ok(Ok(git));
        };
        // Asked of GitHub outside the folder's gate: it is the network, not the folder.
        let repos = urls.and_then(|remotes| {
            Some((
                crate::github::parse_remote(&remotes.head)?,
                crate::github::parse_remote(&remotes.target)?,
            ))
        });
        match repos {
            Some((head, target)) => {
                let read = self
                    .github
                    .read(
                        self.config.gh_bin.as_deref(),
                        &head,
                        &target,
                        &branch,
                        git.head_oid.as_deref(),
                    )
                    .await;
                git.gh = Some(read.gh);
                git.gh_repo = Some(format!("{}/{}", target.owner, target.name));
                git.gh_host = Some(target.host);
                git.pr = read.pr;
                git.gh_read_at = read.read_at;
            }
            _ => git.gh = Some(protocol::ws::GhState::NoGithubRemote),
        }
        Ok(Ok(git))
    }

    /// Do `action` on the run `session_uid`'s repository, kept per `request_id`
    /// ([`protocol::ws::ClientMessage::GitAction`]). The claim is taken under the folder's
    /// git gate, so a retry arriving while the first try runs waits and replays its answer.
    pub async fn git_action(
        &self,
        session_uid: &str,
        request_id: &str,
        action: &protocol::ws::GitAction,
    ) -> protocol::ws::GitActionResult {
        use protocol::ws::{git_refusal as code, GitAction, GitActionResult};
        let refused = |code: &str, detail: Option<String>| GitActionResult::Refused {
            code: code.to_string(),
            detail,
        };
        let (route, claimed_text) = match action {
            GitAction::Commit { message } => {
                let message = message.trim();
                if message.is_empty()
                    || message.len() > protocol::ws::MAX_COMMIT_MESSAGE_BYTES
                    || message.contains('\0')
                {
                    return refused(code::BAD_REQUEST, None);
                }
                (
                    "commit",
                    format!("codeconnect.git_action.commit.v1\n{message}"),
                )
            }
            GitAction::CreatePr => (
                "create_pr",
                "codeconnect.git_action.create_pr.v1".to_string(),
            ),
            GitAction::Merge {
                method,
                head_oid,
                number,
            } => {
                if head_oid.is_empty() || !head_oid.chars().all(|c| c.is_ascii_hexdigit()) {
                    return refused(code::BAD_REQUEST, None);
                }
                (
                    "merge",
                    format!("codeconnect.git_action.merge.v1\n{method:?}\n{head_oid}\n{number}"),
                )
            }
        };
        if request_id.is_empty() {
            return refused(code::BAD_REQUEST, None);
        }
        let row = match self.db.get_session(session_uid.to_string()).await {
            Ok(Some(row)) => row,
            Ok(None) => return refused(code::UNKNOWN_SESSION, None),
            Err(err) => return refused(code::GIT_FAILED, Some(format!("{err:#}"))),
        };
        let gate = self.git_gate(&row.cwd).await;
        let _one_at_a_time = gate.lock().await;
        let claimed = crate::store::ClaimedMaterial {
            thread_id: String::new(),
            generation: 0,
            route: route.into(),
            target_turn_id: None,
            claimed_hash: protocol::hash::sha256_hex(claimed_text.as_bytes()),
            question_decision: None,
        };
        let claim = self
            .db
            .claim_mutation(
                crate::store::OPERATION_GIT_ACTION,
                session_uid.to_string(),
                request_id.to_string(),
                claimed,
                protocol::time::now_rfc3339(),
            )
            .await;
        match claim {
            Ok(crate::store::MutationClaim::Claimed) => {}
            Ok(crate::store::MutationClaim::Applied { outcome, .. }) => {
                return serde_json::from_str(&outcome).unwrap_or_else(|_| {
                    refused(
                        code::INDETERMINATE,
                        Some("its answer could not be read again".into()),
                    )
                })
            }
            Ok(crate::store::MutationClaim::Indeterminate { .. }) => {
                return refused(
                    code::INDETERMINATE,
                    Some("an earlier try did not finish".into()),
                )
            }
            Ok(crate::store::MutationClaim::Conflict) => return refused(code::BAD_REQUEST, None),
            Ok(crate::store::MutationClaim::NoSession) => {
                return refused(code::UNKNOWN_SESSION, None)
            }
            Err(err) => return refused(code::GIT_FAILED, Some(format!("{err:#}"))),
        }
        // Any run in this folder may be writing to it, not only the one acting.
        let busy = match self.db.list_sessions().await {
            Ok(rows) => {
                let mut busy = false;
                for other in rows.iter().filter(|other| other.cwd == row.cwd) {
                    if self.may_be_working(other).await {
                        busy = true;
                        break;
                    }
                }
                busy
            }
            Err(_) => true,
        };
        let outcome = if busy {
            ActionOutcome::Refused(code::BUSY, None)
        } else {
            match action {
                GitAction::Commit { message } => {
                    self.commit_outcome(session_uid, request_id, &row, message.trim())
                        .await
                }
                GitAction::CreatePr => self.create_pr(&row).await,
                GitAction::Merge {
                    method,
                    head_oid,
                    number,
                } => self.merge(&row, *method, head_oid, *number).await,
            }
        };
        let result = match outcome {
            ActionOutcome::Done {
                result,
                notice,
                what,
            } => {
                let outcome = serde_json::to_string(&result).unwrap_or_default();
                let settled = match notice {
                    Some(notice) => {
                        self.settle_with_event(
                            session_uid,
                            crate::store::OPERATION_GIT_ACTION,
                            request_id,
                            &outcome,
                            *notice,
                        )
                        .await
                    }
                    None => {
                        self.db
                            .settle_mutation(
                                crate::store::OPERATION_GIT_ACTION,
                                session_uid.to_string(),
                                request_id.to_string(),
                                outcome,
                                protocol::time::now_rfc3339(),
                            )
                            .await
                    }
                };
                match settled {
                    Ok(true) => result,
                    // Settled by no row (its run was removed meanwhile): nothing kept, as below.
                    Ok(false) => refused(
                        code::INDETERMINATE,
                        Some(format!("{what}, but the computer could not record it")),
                    ),
                    // Done, but its answer is not kept: a retry finds the claim unsettled, so
                    // this answer says the same.
                    Err(err) => {
                        crate::log_error!(
                            "{session_uid}: {what}, but could not record it: {err:#}"
                        );
                        refused(
                            code::INDETERMINATE,
                            Some(format!("{what}, but the computer could not record it")),
                        )
                    }
                }
            }
            ActionOutcome::Refused(code, detail) => {
                let result = refused(code, detail);
                // The refusal stands only once kept: a retry reads what was kept.
                match self
                    .db
                    .settle_mutation(
                        crate::store::OPERATION_GIT_ACTION,
                        session_uid.to_string(),
                        request_id.to_string(),
                        serde_json::to_string(&result).unwrap_or_default(),
                        protocol::time::now_rfc3339(),
                    )
                    .await
                {
                    Ok(true) => result,
                    settled => {
                        if let Err(err) = settled {
                            crate::log_error!(
                                "{session_uid}: could not record a refused {route}: {err:#}"
                            );
                        }
                        refused(code::INDETERMINATE, None)
                    }
                }
            }
            ActionOutcome::Unknown(detail) => {
                if let Err(err) = self
                    .db
                    .settle_mutation_indeterminate(
                        crate::store::OPERATION_GIT_ACTION,
                        session_uid.to_string(),
                        request_id.to_string(),
                        protocol::time::now_rfc3339(),
                    )
                    .await
                {
                    crate::log_error!(
                        "{session_uid}: could not record an unknown {route}: {err:#}"
                    );
                }
                refused(code::INDETERMINATE, Some(detail))
            }
        };
        self.refresh_tree(session_uid);
        result
    }

    /// A commit of every change in `row`'s folder, and the notice it files.
    async fn commit_outcome(
        &self,
        session_uid: &str,
        request_id: &str,
        row: &crate::store::SessionRow,
        message: &str,
    ) -> ActionOutcome {
        let outcome = crate::git::commit(
            &row.cwd,
            self.config.git_bin.as_deref(),
            message,
            Duration::from_millis(self.config.diff_timeout_ms),
            &crate::start_session::phone_environment(),
        )
        .await;
        match outcome {
            crate::git::CommitOutcome::Committed {
                sha,
                files,
                message,
            } => {
                let notice = PendingEvent::new(
                    &SessionKey::new(session_uid, &row.session_id),
                    EventKind::Notification,
                    serde_json::json!({
                        "notification_type": protocol::event::notice::COMMITTED,
                        "oid": sha,
                        "files": files,
                        "message": message,
                    }),
                    Source::Daemon,
                )
                .with_source_event_id(format!("git_commit:{request_id}"));
                ActionOutcome::Done {
                    what: format!("committed {sha}"),
                    result: protocol::ws::GitActionResult::Done {
                        sha: Some(sha),
                        pr: None,
                    },
                    notice: Some(Box::new(notice)),
                }
            }
            crate::git::CommitOutcome::Refused { code, detail } => {
                ActionOutcome::Refused(code, detail)
            }
            crate::git::CommitOutcome::Unknown(detail) => ActionOutcome::Unknown(detail),
        }
    }

    /// `row`'s branch's open pull request merged by `method` at `head_oid`
    /// ([`protocol::ws::GitAction::Merge`]): refused when there is none, when its head is not
    /// `head_oid`, or when its repository does not allow `method`; then `gh pr merge`, and GitHub
    /// asked again for what became of it. Nothing local changes. The branch's GitHub read is
    /// asked again next time.
    async fn merge(
        &self,
        row: &crate::store::SessionRow,
        method: protocol::ws::MergeMethod,
        head_oid: &str,
        number: u64,
    ) -> ActionOutcome {
        use crate::github::Merged;
        use protocol::ws::{git_refusal as code, PrState};
        let timeout = Duration::from_millis(self.config.diff_timeout_ms);
        let gh_bin = self.config.gh_bin.as_deref();
        let git = match crate::git::run_git(&row.cwd, self.config.git_bin.as_deref(), timeout).await
        {
            Ok(git) => git,
            Err(note) => return ActionOutcome::Refused(code::NOT_A_REPOSITORY, Some(note)),
        };
        let Some(branch) = git.branch else {
            return ActionOutcome::Refused(code::DETACHED, None);
        };
        let (_, head, target) = match self.pr_repositories(row, &branch).await {
            Ok(repositories) => repositories,
            Err(refused) => return refused,
        };
        let outcome = 'outcome: {
            let pr = match crate::github::open_pr(gh_bin, &head, &target, &branch).await {
                Ok(Some(pr)) => pr,
                Ok(None) => break 'outcome ActionOutcome::Refused(code::NO_PULL_REQUEST, None),
                Err(unreachable) => break 'outcome unavailable(unreachable),
            };
            // Not the pull request the panel showed (another one opened from the branch since), or
            // not at the commit it showed.
            if pr.number != number || pr.head_oid != head_oid {
                break 'outcome ActionOutcome::Refused(code::HEAD_MOVED, None);
            }
            match crate::github::merge_methods(gh_bin, &target, &pr).await {
                Ok(methods) if methods.contains(&method) => {}
                Ok(_) => break 'outcome ActionOutcome::Refused(code::METHOD_NOT_ALLOWED, None),
                Err(unreachable) => break 'outcome unavailable(unreachable),
            }
            let merged =
                crate::github::merge_pr(&row.cwd, gh_bin, &target, pr.number, method, head_oid)
                    .await;
            // What GitHub has now: a merge queue may hold it open, and a lost answer may hide a
            // merge that happened.
            let now = crate::github::pull_by_number(gh_bin, &target, pr.number).await;
            let done = ActionOutcome::Done {
                what: format!("merged pull request #{}", pr.number),
                result: protocol::ws::GitActionResult::Done {
                    sha: None,
                    pr: Some(protocol::ws::PrLink {
                        number: pr.number,
                        url: pr.url.clone(),
                    }),
                },
                notice: None,
            };
            match (merged, now) {
                (_, Ok(now)) if now.state == PrState::Merged => done,
                (Merged::Done, _) => done,
                (Merged::HeadMoved, _) => ActionOutcome::Refused(code::HEAD_MOVED, None),
                (Merged::Refused(detail), Ok(_)) => {
                    ActionOutcome::Refused(code::MERGE_FAILED, Some(detail))
                }
                // gh failed and GitHub could not be asked after: it may have merged it.
                (Merged::Refused(_), Err(_)) => ActionOutcome::Unknown(
                    "GitHub may have merged it: read the run's repository again".into(),
                ),
                (Merged::Unknown, _) => ActionOutcome::Unknown(
                    "GitHub may have merged it: read the run's repository again".into(),
                ),
            }
        };
        self.github.forget(&head, &target, &branch).await;
        outcome
    }

    /// The repository `row`'s `branch` pushes to and the one its pull request goes into, as
    /// [`Self::run_git`] names them; refused `no_github_remote` when either is not on github.com.
    async fn pr_repositories(
        &self,
        row: &crate::store::SessionRow,
        branch: &str,
    ) -> Result<
        (
            crate::git::PrRemotes,
            crate::github::Repo,
            crate::github::Repo,
        ),
        ActionOutcome,
    > {
        let timeout = Duration::from_millis(self.config.diff_timeout_ms);
        let remotes =
            crate::git::push_remote_urls(&row.cwd, self.config.git_bin.as_deref(), branch, timeout)
                .await;
        remotes
            .and_then(|remotes| {
                let head = crate::github::parse_remote(&remotes.head)?;
                let target = crate::github::parse_remote(&remotes.target)?;
                Some((remotes, head, target))
            })
            .ok_or(ActionOutcome::Refused(
                protocol::ws::git_refusal::NO_GITHUB_REMOTE,
                None,
            ))
    }

    /// A pull request from `row`'s branch ([`protocol::ws::GitAction::CreatePr`]): the open one
    /// already there, else the branch pushed (no force) and one
    /// opened with gh. Whatever the outcome, the branch's GitHub read is asked again next time.
    async fn create_pr(&self, row: &crate::store::SessionRow) -> ActionOutcome {
        use crate::github::Created;
        use protocol::ws::git_refusal as code;
        let timeout = Duration::from_millis(self.config.diff_timeout_ms);
        let git_bin = self.config.git_bin.as_deref();
        let gh_bin = self.config.gh_bin.as_deref();
        let git = match crate::git::run_git(&row.cwd, git_bin, timeout).await {
            Ok(git) => git,
            Err(note) => return ActionOutcome::Refused(code::NOT_A_REPOSITORY, Some(note)),
        };
        let Some(branch) = git.branch.clone() else {
            return ActionOutcome::Refused(code::DETACHED, None);
        };
        if git.unmerged {
            return ActionOutcome::Refused(code::UNMERGED, None);
        }
        // Without a known base nothing says this branch is not the default one: never pushed.
        let Some(base) = git.base.as_ref().map(|base| base.name.as_str()) else {
            return ActionOutcome::Refused(code::BASE_UNKNOWN, None);
        };
        if base == branch {
            return ActionOutcome::Refused(code::ON_BASE_BRANCH, None);
        }
        if git.ahead == Some(0) {
            return ActionOutcome::Refused(code::NOTHING_TO_PROPOSE, None);
        }
        let (remotes, head, target) = match self.pr_repositories(row, &branch).await {
            Ok(repositories) => repositories,
            Err(refused) => return refused,
        };
        let done = |pr: protocol::ws::PrLink| ActionOutcome::Done {
            what: format!("opened pull request #{}", pr.number),
            result: protocol::ws::GitActionResult::Done {
                sha: None,
                pr: Some(pr),
            },
            notice: None,
        };
        let link = |pr: protocol::ws::PullRequest| protocol::ws::PrLink {
            number: pr.number,
            url: pr.url,
        };
        let outcome = 'outcome: {
            match crate::github::open_pr(gh_bin, &head, &target, &branch).await {
                Ok(Some(pr)) => break 'outcome done(link(pr)),
                Ok(None) => {}
                Err(unreachable) => break 'outcome unavailable(unreachable),
            }
            // Always pushed (no force): nothing when the remote has the commit checked out, refused
            // when the branch there moved on, so the pull request is of the commit shown.
            let pushed = crate::git::push(
                &row.cwd,
                git_bin,
                &remotes.push,
                &branch,
                &crate::start_session::phone_environment(),
            )
            .await;
            match pushed {
                Ok(()) => {}
                Err(crate::git::PushFailure::Refused(detail)) => {
                    break 'outcome ActionOutcome::Refused(code::GIT_FAILED, Some(detail))
                }
                Err(crate::git::PushFailure::Timeout) => {
                    break 'outcome ActionOutcome::Refused(code::TIMEOUT, Some("push".into()))
                }
            }
            // Titled by the run when it has a title; else gh's own title from the commits.
            let title = self
                .db
                .run_titles()
                .await
                .ok()
                .and_then(|titles| titles.get(&row.session_uid).cloned());
            let created = crate::github::create_pr(
                &row.cwd,
                gh_bin,
                &head,
                &target,
                &branch,
                base,
                title.as_deref(),
            )
            .await;
            if let Created::Opened(pr) = created {
                break 'outcome done(pr);
            }
            // A lost answer or a race: the pull request may be there, and is never opened twice.
            match (
                crate::github::open_pr(gh_bin, &head, &target, &branch).await,
                created,
            ) {
                (Ok(Some(pr)), _) => done(link(pr)),
                (Ok(None), Created::Failed(detail)) => {
                    ActionOutcome::Refused(code::CREATE_FAILED, Some(format!("pushed; {detail}")))
                }
                // gh gave no answer, or GitHub could not be asked again: it may have one.
                _ => ActionOutcome::Unknown(
                    "pushed; GitHub may have the pull request: read the run's repository again"
                        .into(),
                ),
            }
        };
        self.github.forget(&head, &target, &branch).await;
        outcome
    }

    /// Read a run's branch and line counts from git into the session list, on a
    /// task of its own so no caller waits on git. A run whose row is gone is
    /// left alone.
    fn refresh_tree(&self, session_uid: &str) {
        let db = self.db.clone();
        let inner = Arc::clone(&self.inner);
        let git_bin = self.config.git_bin.clone();
        let timeout = Duration::from_millis(self.config.diff_timeout_ms);
        let session_uid = session_uid.to_string();
        let gates = Arc::clone(&self.git_gates);
        tokio::spawn(async move {
            let Ok(Some(row)) = db.get_session(session_uid.clone()).await else {
                return;
            };
            let gate = git_gate_in(&gates, &row.cwd).await;
            let _one_at_a_time = gate.lock().await;
            let tree = crate::git::tree_facts(&row.cwd, git_bin.as_deref(), timeout).await;
            inner.lock().await.tree_facts.insert(session_uid, tree);
        });
    }

    // --------------------------------------------------------- pairing

    /// Mint a single-use pairing code. Only the hash is stored.
    pub async fn create_pairing(&self, ttl_secs: u64) -> Result<(String, String)> {
        let code = crate::secret::pairing_code()?;
        let now_ms = protocol::time::now_unix_ms();
        let expires_ms = now_ms + (ttl_secs as i64) * 1000;
        let expires_at = protocol::time::rfc3339_from_unix_ms(expires_ms);
        self.db
            .create_pairing_code(
                sha256_hex(code.as_bytes()),
                expires_at.clone(),
                expires_ms,
                now_ms,
            )
            .await?;
        crate::log_info!("pairing code issued, valid until {expires_at}");
        Ok((code, expires_at))
    }

    /// Store where a device wants its pushes sent.
    ///
    /// **A token belongs to one row.** A phone that re-pairs arrives under a new
    /// device id carrying the same APNs token, and the store moves it — so the
    /// rows it was taken from are told to the sender here. Their queues hold
    /// work for a token they no longer own, and their workers would otherwise
    /// wait on a phone that has already come back under another name.
    ///
    /// **The three values are one fact.** A relay credential authorises sending
    /// to *this* token and no other, so it is written with the token in a single
    /// transaction: a rotation that landed the new token beside the old bearer
    /// would present the relay a pair it has no binding for, and every push
    /// would be refused as a credential failure that no repair could fix.
    pub async fn register_push(
        &self,
        device_id: &str,
        token: &str,
        environment: &str,
        credential: Option<&protocol::secret::Redacted>,
    ) -> Result<()> {
        let displaced = self
            .db
            .set_push_token(
                device_id.to_string(),
                token.to_string(),
                environment.to_string(),
                credential.cloned(),
            )
            .await?;
        for previous in displaced {
            crate::log_info!("push: {previous} lost its token to {device_id}; retiring its queue");
            self.push.retire(&previous);
        }
        Ok(())
    }

    /// Authenticate a `hello`.
    ///
    /// A token wins over a pairing code when both are offered: re-pairing an
    /// already-paired phone would mint a second credential for one device and
    /// leave the first one live but unlisted against it.
    ///
    /// `tailnet` is what [`protocol::tailnet::Tailnet::sender`] said of the address
    /// the connection came from: the Tailscale device, when it vouches for it (one
    /// of this computer's own user's, not tagged, not removed), or why not. A token
    /// presented from such a device records it on the token's row; a code redeemed
    /// from it pairs that device; and a hello with neither is paired as a code would
    /// pair it, unless that device is kept out
    /// ([`crate::store::Store::admit_tailnet_device`]).
    pub async fn authenticate(
        &self,
        static_token: &str,
        token: Option<&str>,
        pairing_code: Option<&str>,
        client_name: Option<&str>,
        tailnet: Result<&str, protocol::tailnet::Refusal>,
    ) -> AuthOutcome {
        let tailnet_node = tailnet.ok();
        if let Some(token) = token.filter(|t| !t.is_empty()) {
            if constant_time_eq(token.as_bytes(), static_token.as_bytes()) {
                return AuthOutcome::Static;
            }
            let token_hash = sha256_hex(token.as_bytes());
            // Before the lookup, which sees active rows only: a revoked token
            // presented from a Tailscale device is what tells this daemon which
            // device to keep out.
            if let Some(node) = tailnet_node {
                if let Err(err) = self
                    .db
                    .stamp_tailnet_node(token_hash.clone(), node.to_string())
                    .await
                {
                    crate::log_error!("could not record a device's Tailscale node: {err:#}");
                }
            }
            return match self.db.device_by_token_hash(token_hash).await {
                Ok(crate::store::TokenAuth::Active(device)) => {
                    let _ = self
                        .db
                        .touch_device(device.device_id.clone(), protocol::time::now_rfc3339())
                        .await;
                    AuthOutcome::Device(device)
                }
                Ok(crate::store::TokenAuth::Revoked) => AuthOutcome::Revoked,
                Ok(crate::store::TokenAuth::Missing) => {
                    AuthOutcome::Rejected("token matches no device".into())
                }
                // A lookup that errored is not a block: stay "unauthorized" so a transient
                // database fault cannot read as "this device is blocked".
                Err(err) => AuthOutcome::Rejected(format!("device lookup failed: {err:#}")),
            };
        }

        if let Some(code) = pairing_code {
            return self.pair(code, client_name, tailnet_node).await;
        }
        let node =
            match tailnet {
                Ok(node) => node,
                Err(protocol::tailnet::Refusal::Blocked) => return AuthOutcome::Rejected(
                    "hello carried neither a token nor a pairing code, from a Tailscale device \
                     that was revoked"
                        .into(),
                ),
                Err(_) => return AuthOutcome::Rejected(
                    "hello carried neither a token nor a pairing code, from an address Tailscale \
                     does not vouch for"
                        .into(),
                ),
            };
        match self.admit_tailnet_device(client_name, node).await {
            Ok(outcome) => outcome,
            Err(err) => AuthOutcome::Rejected(format!("Tailscale sign-in failed: {err:#}")),
        }
    }

    /// Pair the Tailscale device `node` with no code: a fresh device and token,
    /// exactly as a code would mint them, unless that device is kept out.
    async fn admit_tailnet_device(
        &self,
        client_name: Option<&str>,
        node: &str,
    ) -> Result<AuthOutcome> {
        let device_id = crate::secret::device_id()?;
        let token = crate::secret::device_token()?;
        let name = self
            .db
            .unique_device_name(client_name.unwrap_or("device").to_string())
            .await?;
        let admission = self
            .db
            .admit_tailnet_device(
                device_id.clone(),
                name.clone(),
                sha256_hex(token.as_bytes()),
                protocol::time::now_rfc3339(),
                node.to_string(),
            )
            .await?;
        Ok(match admission {
            crate::store::Admission::Admitted => {
                crate::log_info!("paired device {device_id} ({name}) through Tailscale");
                AuthOutcome::Paired {
                    device_id,
                    device_name: name,
                    token,
                }
            }
            crate::store::Admission::Blocked => {
                AuthOutcome::Rejected(format!("Tailscale device {node} was revoked"))
            }
            crate::store::Admission::Paused => AuthOutcome::Rejected(
                "Tailscale sign-in is paused: a device paired before it existed was revoked, \
                 and which Tailscale device it was is unknown"
                    .into(),
            ),
        })
    }

    async fn pair(
        &self,
        code: &str,
        client_name: Option<&str>,
        tailnet_node: Option<&str>,
    ) -> AuthOutcome {
        let now_ms = protocol::time::now_unix_ms();
        if !self.admit_pairing_attempt(now_ms).await {
            // Loud: a human pairs a phone by typing one code. Reaching this
            // limit is not a user having a bad day, it is something guessing.
            crate::log_warn!(
                "PAIRING RATE LIMIT: refusing further attempts; {} failures inside {}s. If you \
                 did not just mistype a code, something is guessing at this daemon.",
                self.config.pairing_max_attempts,
                self.config.pairing_window_secs,
            );
            // The same opaque refusal every other failure gets. Telling a peer
            // it is being rate-limited hands it the one thing it needs to pace
            // itself under the limit.
            return AuthOutcome::Rejected("too many pairing attempts".into());
        }
        let outcome = self.pair_once(code, client_name, tailnet_node).await;
        if matches!(outcome, AuthOutcome::Rejected(_)) {
            self.record_pairing_failure(now_ms).await;
        }
        outcome
    }

    /// May another pairing attempt be made right now?
    ///
    /// A pairing code carries real entropy and lives five minutes, so this is
    /// not what stops a *guess* — it is what stops an unbounded *number* of
    /// guesses. The safety argument for a short-lived code is "you cannot try
    /// enough of them in five minutes", and that argument is only true if
    /// something is counting.
    ///
    /// Global rather than per-peer, deliberately. There is one operator and
    /// pairing is a rare, deliberate act at the keyboard, so a global ceiling
    /// cannot inconvenience a human — while a per-peer limit would be defeated
    /// by the one thing an attacker on a tailnet can trivially vary.
    async fn admit_pairing_attempt(&self, now_ms: i64) -> bool {
        let max = self.config.pairing_max_attempts;
        if max == 0 {
            return true;
        }
        let window_ms = (self.config.pairing_window_secs as i64).saturating_mul(1_000);
        let mut inner = self.inner.lock().await;
        // Pruned on every check rather than on a timer: the deque is bounded by
        // `max`, so this is a handful of comparisons and there is no sweeper to
        // forget to schedule.
        while inner
            .pairing_failures
            .front()
            .is_some_and(|at| now_ms.saturating_sub(*at) >= window_ms)
        {
            inner.pairing_failures.pop_front();
        }
        (inner.pairing_failures.len() as u32) < max
    }

    async fn record_pairing_failure(&self, at_ms: i64) {
        if self.config.pairing_max_attempts == 0 {
            return;
        }
        let mut inner = self.inner.lock().await;
        inner.pairing_failures.push_back(at_ms);
        // Only failures are counted, and only `max` of them are ever held: a
        // successful pairing leaves nothing behind, so a phone that pairs
        // normally can never push the daemon towards its own limit.
        while inner.pairing_failures.len() > self.config.pairing_max_attempts as usize {
            inner.pairing_failures.pop_front();
        }
    }

    async fn pair_once(
        &self,
        code: &str,
        client_name: Option<&str>,
        tailnet_node: Option<&str>,
    ) -> AuthOutcome {
        let code = protocol::pairing::normalize_code(code);
        // Shape-checked before the database is touched, so a malformed code
        // costs a string scan rather than a query.
        if !protocol::pairing::is_well_formed(&code) {
            return AuthOutcome::Rejected("malformed pairing code".into());
        }
        match self
            .db
            .consume_pairing_code(sha256_hex(code.as_bytes()), protocol::time::now_unix_ms())
            .await
        {
            Ok(PairingConsume::Consumed) => {}
            Ok(PairingConsume::NotFound) => {
                return AuthOutcome::Rejected("unknown pairing code".into())
            }
            Ok(PairingConsume::Expired) => {
                return AuthOutcome::Rejected("pairing code expired".into())
            }
            Ok(PairingConsume::AlreadyUsed) => {
                return AuthOutcome::Rejected("pairing code already used".into())
            }
            Err(err) => return AuthOutcome::Rejected(format!("pairing store failed: {err:#}")),
        }

        // Past this point the code is spent. Any failure below must therefore
        // be reported rather than retried with the same code.
        match self.mint_device(client_name, tailnet_node).await {
            Ok(outcome) => outcome,
            Err(err) => {
                crate::log_error!("pairing consumed a code but failed to complete: {err:#}");
                AuthOutcome::Rejected(format!("pairing failed: {err:#}"))
            }
        }
    }

    async fn mint_device(
        &self,
        client_name: Option<&str>,
        tailnet_node: Option<&str>,
    ) -> Result<AuthOutcome> {
        let device_id = crate::secret::device_id()?;
        let token = crate::secret::device_token()?;
        let name = self
            .db
            .unique_device_name(client_name.unwrap_or("device").to_string())
            .await?;
        self.db
            .insert_device(
                device_id.clone(),
                name.clone(),
                sha256_hex(token.as_bytes()),
                protocol::time::now_rfc3339(),
                tailnet_node.map(str::to_string),
            )
            .await?;

        crate::log_info!("paired device {device_id} ({name})");
        Ok(AuthOutcome::Paired {
            device_id,
            device_name: name,
            token,
        })
    }

    /// Every paired device. Runs on the blocking pool, because a database read
    /// has no business on a runtime worker.
    pub async fn list_devices(&self) -> Result<Vec<DeviceSummary>> {
        self.db.device_summaries().await
    }

    /// Revoke a device's access.
    ///
    /// The device token is the whole of what this daemon hands out, so taking it
    /// away takes away everything *it* granted. A failure to revoke is fatal and
    /// reported as such — an operator who is told a phone was cut off must not
    /// have to wonder.
    ///
    /// **It is not the only thing a phone may hold, which is why the legacy
    /// sweep runs here too.** Earlier releases also wrote the phone's public key
    /// into `~/.ssh/authorized_keys`, and that grant is outside this database
    /// entirely: withdrawing the token does nothing to it. The daemon sweeps
    /// that file at startup, but a startup sweep only describes startup — a
    /// `~/.ssh` that was unwritable then, a backup restored since, a Mac that has
    /// not rebooted in months — and this command is the one the documentation
    /// presents as *the* way to take a device's access away. Revoking a phone
    /// and leaving it a working shell is the gap that narrows here.
    ///
    /// **Narrows, and not more than that: a revocation is the whole truth about
    /// the token and a best effort at everything else.** The token is this
    /// daemon's to withdraw and it is withdrawn or this call fails. The sweep
    /// edits a file this daemon does not own, and it removes nothing at all in
    /// five situations it can see — no absolute `$HOME`, a file it cannot read, a
    /// file it cannot rewrite, a file somebody else rewrote while it was
    /// working, and tagged lines that are not the marker-and-key pair it
    /// recognises — none of which fails the revocation. Beyond those it is
    /// blind by construction: a key outside `$HOME/.ssh/authorized_keys`, or
    /// inside it in any shape other than the one earlier releases wrote, is
    /// never a candidate. So this command withdrawing the token is a fact, and
    /// the shell being gone is a claim only `grep -n codeconnect:` on the file
    /// itself can settle — which is why every one of those five says so in the
    /// daemon log rather than being folded into what this returns.
    ///
    /// The sweep takes the whole file rather than this device's tag: see
    /// [`crate::legacy_credentials`] for why a tag's id decides nothing. It is
    /// never fatal, and quiet when there is nothing to do — a revocation must
    /// still succeed, and still report accurately, over a file it does not
    /// control, and a Mac that never granted SSH access has one file read and
    /// nothing written on every revoke. It is not silent when it fails: every
    /// path that removes nothing warns, which is the only place that fact is
    /// recorded.
    pub async fn revoke(&self, needle: &str) -> Result<RevokeOutcome> {
        let device = match self.db.find_device(needle.to_string()).await? {
            DeviceLookup::Found(device) => *device,
            DeviceLookup::NotFound => {
                anyhow::bail!("no device matches {needle:?}; `codeconnect devices` lists them")
            }
            DeviceLookup::Ambiguous(ids) => anyhow::bail!(
                "{needle:?} matches {} devices ({}); name one exactly",
                ids.len(),
                ids.join(", ")
            ),
        };

        // Every device on the same Tailscale device goes with it: they are one
        // phone or one computer, and one left active would let it straight back.
        let revoked = self
            .db
            .revoke_device(device.device_id.clone(), protocol::time::now_rfc3339())
            .await?;
        let token_revoked = revoked.contains(&device.device_id);
        for device_id in &revoked {
            // Published before the re-read: the point of revocation is that it
            // takes effect *now*, and a socket that is idle would otherwise keep
            // serving the event log until its next keepalive. `send` fails only
            // when nobody is subscribed, which is the ordinary case for a Mac
            // with no phone connected.
            let _ = self.revocations_tx.send(device_id.clone());
            self.push_gate.evict_device(device_id);
            // And the sender's queue for it, which holds work authorised before
            // this moment and a worker that would otherwise wait on a phone that
            // is never coming back.
            self.push.retire(device_id);
        }

        // And the grant that is not this daemon's to begin with. Unconditional,
        // including on a device that was already revoked: re-running `revoke` is
        // then the operator's retry for a sweep the file refused earlier, and
        // the only one they have short of restarting the daemon.
        //
        // The count is deliberately not folded into what this function returns.
        // Zero covers six different outcomes there — nothing to do, no `$HOME`,
        // unreadable, unwritable, rewritten by somebody else mid-sweep, a tagged
        // line that is not a whole pair — and a number carried up to
        // `codeconnect revoke` would be read as "the file is clean now" on five
        // of them. The sweep says which of the six it was in the daemon log, in
        // its own words, and that is the honest record.
        crate::legacy_credentials::purge_authorized_keys_off_runtime().await;

        // Re-read so the reported state is what is now stored, not what we
        // believe we just wrote.
        let fresh = match self.db.find_device(device.device_id.clone()).await? {
            DeviceLookup::Found(row) => *row,
            _ => device,
        };
        let summary = fresh.to_summary();
        crate::log_info!(
            "revoked device {} (token_revoked={token_revoked})",
            summary.device_id
        );
        Ok(RevokeOutcome {
            device: summary,
            token_revoked,
        })
    }

    /// `codeconnect unblock <device>`: let a revoked device back in, and every
    /// revoked device on the same Tailscale device. Each keeps its token, so the
    /// device signs in again with the pairing it already holds, as the same
    /// device; nothing is paired afresh.
    pub async fn unblock(&self, needle: &str) -> Result<DeviceSummary> {
        let device = match self.db.find_device(needle.to_string()).await? {
            DeviceLookup::Found(device) => *device,
            DeviceLookup::NotFound => {
                anyhow::bail!("no device matches {needle:?}; `codeconnect devices` lists them")
            }
            DeviceLookup::Ambiguous(ids) => anyhow::bail!(
                "{needle:?} matches {} devices ({}); name one exactly",
                ids.len(),
                ids.join(", ")
            ),
        };
        if self.db.unblock_device(device.device_id.clone()).await? == 0 {
            anyhow::bail!("{} is not blocked", device.name);
        }
        crate::log_info!("unblocked device {} ({})", device.device_id, device.name);
        // Re-read so the answer is what is now stored, as `revoke` answers.
        let fresh = match self.db.find_device(device.device_id.clone()).await? {
            DeviceLookup::Found(row) => *row,
            _ => device,
        };
        Ok(fresh.to_summary())
    }
}

/// Why a decision cannot answer a question card.
const QUESTION_NEEDS_CARD: &str = "Claude is asking you a question. Answer it at the computer, or \
                                   update CodeConnect to answer it here. Nothing was typed.";

/// Why a question decision cannot answer an ordinary card.
const NOT_A_QUESTION: &str = "this answers a question, and this card is not one; nothing was sent";

/// How a card closes when the run that asked it ends first.
const RUN_ENDED: &str = "the session ended before anyone answered";

/// Why a hosted run's card closed when an app's Stop ended its turn before anyone answered.
const STOPPED_BEFORE_ANSWER: &str = "The turn was stopped before anyone answered.";
/// What claude is told of a hosted run's request whose card no app could show.
const TOO_LARGE_TO_SHOW: &str = "This request is too large to show in the app, so it was refused.";

/// The closing of a hosted run's card nobody answered: its host withdrew the
/// request, or its run ended. It closes the card; no answer is recorded, so a late
/// answer to it finds no card rather than an answer.
fn host_card_closure(session: &SessionKey, request_id: &str, detail: &str) -> PendingEvent {
    let outcome = AnswerOutcome {
        request_id: request_id.to_string(),
        session_id: session.name.clone(),
        decision: AnswerDecision::Deny,
        resolved_by: ResolvedBy::Timeout,
        answered_on: None,
        applied_via: AnswerPath::HookReturn,
        resolved_at: protocol::time::now_rfc3339(),
        detail: Some(detail.into()),
        indeterminate: false,
    };
    PendingEvent::new(
        session,
        EventKind::ApprovalResolved,
        serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null),
        Source::Daemon,
    )
    .with_source_event_id(format!("resolved:{request_id}"))
}

/// The host's decision for an answer to a hosted run's card: allow; a deny, which
/// also ends the main conversation's turn; or a deny with the person's reason. A question takes only its answers, which go back
/// as its input, or a decline, a deny that ends the main conversation's turn as a
/// deny does. A card that offers "Allow for this session" also takes that option: an
/// allow carrying claude's own allow rules for the request, made the session's.
fn host_decision(
    entry: &PendingApproval,
    decision: &AnswerDecision,
    answered_on: AnsweredOn,
) -> std::result::Result<protocol::host::Decision, String> {
    if entry.question {
        return match decision {
            AnswerDecision::Answers { answers } => {
                crate::question::host_updated_input(&entry.card.tool_input, answers)
                    .map(|input| protocol::host::Decision::Allow {
                        updated_input: Some(input),
                        updated_permissions: None,
                    })
                    .map_err(|reason| format!("{reason}. Nothing was sent."))
            }
            AnswerDecision::Decline | AnswerDecision::Deny => Ok(protocol::host::Decision::Deny {
                message: declined_from(answered_on).into(),
                interrupt: !entry.fork,
            }),
            _ => Err(QUESTION_NEEDS_CARD.into()),
        };
    }
    if matches!(
        decision,
        AnswerDecision::Answers { .. } | AnswerDecision::Decline
    ) {
        return Err(NOT_A_QUESTION.into());
    }
    match decision {
        AnswerDecision::Allow | AnswerDecision::Option { index: 1 } => {
            Ok(protocol::host::Decision::Allow {
                updated_input: None,
                updated_permissions: None,
            })
        }
        // Decided from the stored card's suggestions, never from the app's word: a card
        // that did not offer it refuses it below.
        AnswerDecision::OptionId { option_id }
            if option_id == ALLOW_FOR_SESSION && entry.card.allow_for_session =>
        {
            Ok(protocol::host::Decision::Allow {
                updated_input: None,
                updated_permissions: session_rules(entry.card.permission_suggestions.as_ref()),
            })
        }
        AnswerDecision::Deny => Ok(protocol::host::Decision::Deny {
            message: BACKGROUND_DENIAL.into(),
            interrupt: !entry.fork,
        }),
        AnswerDecision::Text { text } => Ok(protocol::host::Decision::Deny {
            message: format!("{DENIAL_WITH_REASON}{text}"),
            interrupt: false,
        }),
        _ => Err(
            "the app can allow or deny this card, or deny it with a reason; nothing else \
                  is available for this session yet. Nothing was sent."
                .into(),
        ),
    }
}

/// What a background agent is told when its call is denied, in Claude 2.1.290's
/// own words for an Escape at that agent's dialog.
const BACKGROUND_DENIAL: &str = "Permission for this tool use was denied. The tool use was \
                                 rejected (eg. if it was a file edit, the new_string was NOT \
                                 written to the file). Try a different approach or report the \
                                 limitation to complete your task.";

/// A denial with the person's reason, in Claude 2.1.290's own words for "No,
/// and tell Claude what to do differently"; the reason follows.
const DENIAL_WITH_REASON: &str = "The user doesn't want to proceed with this tool use. The tool \
                                  use was rejected (eg. if it was a file edit, the new_string \
                                  was NOT written to the file). To tell you how to proceed, the \
                                  user said:\n";

/// The source a card's `approval_request` event is filed under: this daemon's, for a
/// hosted run's card and a Codex card. `Hook` is left only for a Claude card recovery
/// restored without a host link, because the run's host could not be read then.
/// Accepted miss: that card's request event was filed by the daemon, so it is not found
/// under `Hook` and the fleet lists the card in `blocked_on` without a ref.
fn request_event_source(entry: &PendingApproval) -> Source {
    if entry.link || matches!(entry.agent, protocol::agent::AgentKind::Codex) {
        Source::Daemon
    } else {
        Source::Hook
    }
}

/// The option id an app answers "Allow for this session" with, decision 175's for Codex.
const ALLOW_FOR_SESSION: &str = "acceptForSession";

/// "Allow for this session" on a hosted run's card (owner decision 200): claude's own allow
/// rules for this request (`addRules` with behavior `allow`), each with the session as its
/// destination and nothing else. A mode change or a folder is never among them, and nothing is
/// written to a settings file. `None` when claude suggested no such rule, and then the card
/// offers no session answer.
fn session_rules(suggestions: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let rules: Vec<serde_json::Value> = suggestions?
        .as_array()?
        .iter()
        .filter(|update| update["type"] == "addRules" && update["behavior"] == "allow")
        .map(|update| {
            let mut update = update.clone();
            update["destination"] = "session".into();
            update
        })
        .collect();
    (!rules.is_empty()).then(|| rules.into())
}

/// What claude reads when a question is declined: which kind of device declined it.
fn declined_from(answered_on: AnsweredOn) -> &'static str {
    match answered_on {
        AnsweredOn::Iphone => "Declined from iPhone",
        AnsweredOn::Mac => "Declined from Mac",
    }
}

/// Mark the approval a tool result belongs to, if any: the result's item id *is*
/// the approval's request id, so this is an exact join rather than a heuristic.
fn note_tool_result(inner: &mut Inner, event: &Event) {
    if event.kind != EventKind::ToolResult {
        return;
    }
    let Some(item_id) = &event.item_id else {
        return;
    };
    let id: ApprovalId = (event.session_uid.clone(), item_id.clone());
    if let Some(entry) = inner.pending.get_mut(&id) {
        entry.tool_ran = true;
    }
}

// -------------------------------------------------- the codeconnect launcher

/// The env override naming that `codeconnect` ([`codex_launcher`]), ahead of every
/// derived candidate. The house shape — a
/// `CODECONNECT_<TOOL>_BIN` checked first, then the places the tool actually lives.
pub(crate) const LAUNCHER_BIN_ENV: &str = "CODECONNECT_LAUNCHER_BIN";

/// **The cap on a client-chosen identifier this daemon will write to `ccd.log`.**
///
/// Measured: real request ids run 1–59 bytes, the longest being
/// `"startup-thread-start-<uuid>"`. 128 is more than twice the measured maximum, so it
/// cannot refuse a real one, while making the length of a log line a property of this
/// build rather than of whatever a phone chose to send.
pub(crate) const MAX_LOGGED_CLIENT_ID_BYTES: usize = 128;

/// Is this client-chosen string a plain identifier — safe to write out as it is?
///
/// The rule: ASCII alphanumerics plus `-`, `_`, `.` and `:`, non-empty, and
/// within [`MAX_LOGGED_CLIENT_ID_BYTES`]. A string matching it can hold no newline, no
/// carriage return, no control byte and no quote — which is exactly why it cannot forge
/// a line — and it covers every id form measured on the wire, every session uid (a
/// ULID) and every ordinary session name.
fn is_a_plain_client_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LOGGED_CLIENT_ID_BYTES
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// **A phone-chosen `request_id`, rendered safe to write to `ccd.log`.**
///
/// The id on an `Answer`, an `Interrupt` or a `Compose` frame is chosen entirely by the
/// phone and is interpolated into this daemon's log at eleven sites. A newline in it
/// forges a whole line into the file operators and the live gates read: an id of
/// `"x\n2026-09-09T00:00:00.000Z ERROR codex recovery: ownership asserted"` writes a
/// second line that greps exactly like a real one, and nothing downstream can tell the
/// two apart.
///
/// A conforming id is written AS IT IS, because an operator diagnosing a refusal needs
/// to know *which* ask; anything else is written as its byte count and nothing more.
/// See [`is_a_plain_client_id`] for the grammar.
///
/// A conforming id is written bare rather than through `{:?}`: `ccd.log` is prose whose
/// lines already interpolate the id bare, and the grammar has already excluded every
/// byte the quoting would be protecting against.
pub(crate) fn logged_request_id(id: &str) -> std::borrow::Cow<'_, str> {
    if is_a_plain_client_id(id) {
        std::borrow::Cow::Borrowed(id)
    } else {
        std::borrow::Cow::Owned(format!("<non-conforming request id, {} bytes>", id.len()))
    }
}

/// **A phone-chosen session reference, rendered safe to write to `ccd.log`.**
///
/// [`logged_request_id`]'s twin, and it exists because a `session_id` on the wire is a
/// uid OR a bare name — so, unlike a resolved `session_uid` read back out of the
/// store, it is client-chosen text and is a log-injection channel in exactly the same
/// way. Same grammar; a different noun, so a shape in the log says which field could
/// not be written.
pub(crate) fn logged_session_ref(reference: &str) -> std::borrow::Cow<'_, str> {
    if is_a_plain_client_id(reference) {
        std::borrow::Cow::Borrowed(reference)
    } else {
        std::borrow::Cow::Owned(format!(
            "<non-conforming session reference, {} bytes>",
            reference.len()
        ))
    }
}

/// The `codeconnect` this daemon names to the runs it starts, as their mail server, and
/// leaves out when it looks for an agent's binary.
pub(crate) fn codex_launcher() -> Result<std::path::PathBuf, String> {
    launcher_among(
        std::env::var(LAUNCHER_BIN_ENV).ok(),
        std::env::current_exe().ok(),
        protocol::root_dir(),
        state_dir_owner(),
    )
}

/// **The uid a launcher override has to belong to**, or `None` when this daemon
/// cannot establish one.
///
/// `~/.codeconnect` rather than `getuid(2)`: `ccd` carries no `libc` dependency, and
/// adding one for a single symbol would buy a weaker anchor than this. The state
/// directory holds this daemon's database, its TLS material and its device tokens —
/// everything the process already trusts absolutely — so "owned by whoever owns
/// that" is the ownership the whole daemon rests on rather than a proxy for it. On a
/// machine where the two differ, the override is not the operator's to choose.
fn state_dir_owner() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(protocol::root_dir())
        .ok()
        .map(|meta| meta.uid())
}

/// **Is this override a program this daemon may spawn?**
///
/// `CODECONNECT_LAUNCHER_BIN` was `is_file()` and nothing else, and what it selects
/// is named to every run this daemon starts, by a launchd job holding the APNs signing
/// key and this Mac's tailnet identity — and the file an environment variable names is
/// the file that ends up running.
///
/// Three conditions, and each is a different way the override stops being the
/// operator's own choice:
///
///   * **a regular file.** A directory, a fifo or a device is not a program. This is
///     the question `is_file()` was already asking; it is asked here so all three
///     answers come from one read of one file rather than from three separate ones.
///   * **owned by the uid that owns this daemon's state.** Another user's file is
///     another user's decision about what this daemon runs.
///   * **writable by nobody but its owner.** A group- or other-writable path can be
///     rewritten between this check and the spawn, so a file that passed is not
///     necessarily the file that runs. `mode & 0o022`, which is both bits and no others: the execute
///     and read bits say nothing about who can revise it.
///
/// **No digest pin.** The launcher is this build's own binary and every install
/// replaces it, so a hash would refuse the ordinary upgrade.
///
/// `metadata` and not `symlink_metadata`: a symlink is an ordinary way to name an
/// installed launcher, and the thing that gets executed is the target — so the target
/// is what has to answer for itself.
///
/// **`owner` is `None` when this daemon could not read its own state directory.** The
/// ownership question is then unanswerable, and it is skipped rather than guessed at
/// in either direction; the other two conditions still apply, so the check degrades
/// to a narrower one rather than to none.
fn launcher_is_spawnable(path: &std::path::Path, owner: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).map_err(|err| format!("it cannot be examined ({err})"))?;
    if !meta.is_file() {
        return Err("it is not a regular file".to_string());
    }
    if let Some(owner) = owner {
        if meta.uid() != owner {
            return Err(format!(
                "it belongs to uid {}, and this daemon's own state belongs to uid {owner}",
                meta.uid()
            ));
        }
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o022 != 0 {
        return Err(format!(
            "it is writable by its group or by everyone (mode {mode:04o}), so what runs \
             need not be what was checked"
        ));
    }
    Ok(())
}

/// The candidate list, given its three inputs rather than reading them: the override,
/// then the launcher beside this daemon, then the one in the install prefix.
///
/// **Beside first, because a daemon wants the launcher of its own build** — which is
/// what being beside it means, in a cargo target directory and an installed prefix
/// alike. **The install prefix is a fallback and not a reach across releases:** the
/// launchd job is the only production topology, and there `beside` and `<root>/bin`
/// are the same directory, so the fallback is never taken. What it covers is a `ccd`
/// started by hand from somewhere else on a machine that has an install. A dev tree
/// that also has an install can therefore reach the installed launcher, and the
/// override exists to say otherwise.
///
/// Split out so the precedence is exercised without touching the environment. This
/// process is a daemon with live tasks in it and `setenv` is not thread-safe on this
/// platform — a test that set `CODECONNECT_LAUNCHER_BIN` to prove which candidate wins
/// would be racing every other test in the binary for the answer, and the failure mode
/// of losing that race is a green test.
fn launcher_among(
    named: Option<String>,
    exe: Option<std::path::PathBuf>,
    root: std::path::PathBuf,
    owner: Option<u32>,
) -> Result<std::path::PathBuf, String> {
    let mut looked = Vec::new();
    // An override this daemon will not spawn is REPORTED and then passed, not obeyed:
    // an operator who mistyped the path — or who left it somewhere anyone can rewrite —
    // should learn that from the log. Said at startup by [`warn_about_an_ignored_launcher_override`],
    // because a fallback that works would otherwise swallow it here, and the reason is
    // carried into `looked` so the no-launcher error names it too.
    //
    // **The candidates BESIDE this daemon and in the install prefix are not checked
    // this way, and the asymmetry is the point.** Those two are wherever this binary
    // was installed; an operator who can write there can replace `ccd` itself, so a
    // guard on them would be theatre. The override is the one candidate a *caller* —
    // an environment variable on a launchd job, a shell that started this by hand —
    // gets to choose, which is the only place the choice can be somebody else's.
    if let Some(named) = named.filter(|value| !value.is_empty()) {
        let path = std::path::PathBuf::from(named);
        match launcher_is_spawnable(&path, owner) {
            Ok(()) => return Ok(path),
            Err(why) => looked.push(format!("{} ({LAUNCHER_BIN_ENV}: {why})", path.display())),
        }
    }
    if let Some(beside) = exe.as_deref().and_then(std::path::Path::parent) {
        let beside = beside.join("codeconnect");
        if beside.is_file() {
            return Ok(beside);
        }
        looked.push(format!("{} (beside this daemon)", beside.display()));
    }
    let installed = root.join("bin").join("codeconnect");
    if installed.is_file() {
        return Ok(installed);
    }
    looked.push(format!("{} (the install prefix)", installed.display()));
    Err(format!(
        "no `codeconnect` was found; looked at {}",
        looked.join(", ")
    ))
}

/// Say, once at startup, that `CODECONNECT_LAUNCHER_BIN` names nothing.
///
/// [`launcher_among`] passes such an override and takes the next candidate, so on a
/// machine where a candidate exists the mistyped path is never mentioned anywhere — the
/// daemon runs happily through a launcher the operator did not choose. Said once at
/// startup because it is a condition of the environment this process was started in and
/// cannot change under it.
pub(crate) fn warn_about_an_ignored_launcher_override() {
    if let Some(why) =
        an_ignored_launcher_override(std::env::var(LAUNCHER_BIN_ENV).ok(), state_dir_owner())
    {
        crate::log_warn!("codex recovery: {why}");
    }
}

/// The sentence, given the override rather than reading it — split out for the reason
/// [`launcher_among`] is: `setenv` is not thread-safe on this platform and this process
/// is a daemon with live tasks in it, so a test that set the variable would be racing
/// every other test in the binary and the failure mode of losing that race is a green
/// test.
fn an_ignored_launcher_override(named: Option<String>, owner: Option<u32>) -> Option<String> {
    let named = named.filter(|value| !value.is_empty())?;
    // The same predicate [`launcher_among`] passes the override through, so the
    // complaint cannot describe a different rule from the one that was applied.
    let why = launcher_is_spawnable(std::path::Path::new(&named), owner).err()?;
    Some(format!(
        "{LAUNCHER_BIN_ENV} names {named}, which this daemon will not spawn because \
         {why}; it is ignored and the launcher is looked for in the usual places"
    ))
}

/// The job this process belongs to — launchd's on macOS, CodeConnect's systemd
/// unit on Linux — or `None` if it has none.
///
/// macOS sets `XPC_SERVICE_NAME` for every process, not only for launchd jobs:
/// a program started from a shell inherits the literal string `"0"`, which is
/// the documented "this is not an XPC service" sentinel. Reporting that as a
/// label would make `codeconnect daemon status` say a hand-started daemon is "managed by
/// a different job (0)" — a sentence that sends the operator looking for a
/// job that does not exist.
///
/// "Managed by a different job" is still a distinct outcome worth reporting, so
/// a real label that is not ours is passed through untouched.
///
/// On Linux nothing sets a variable for every service, so CodeConnect's own
/// systemd unit sets [`SERVICE_LABEL_VAR`] to the label, and a `ccd` started any
/// other way has none.
pub(crate) fn launchd_label() -> Option<String> {
    std::env::var(SERVICE_LABEL_VAR)
        .ok()
        .filter(|label| !label.is_empty() && label != "0")
}

/// The variable that names the job running this process: launchd's own on
/// macOS, the one CodeConnect's systemd unit sets on Linux.
#[cfg(target_os = "macos")]
pub(crate) const SERVICE_LABEL_VAR: &str = "XPC_SERVICE_NAME";
#[cfg(target_os = "linux")]
pub(crate) const SERVICE_LABEL_VAR: &str = "CODECONNECT_SERVICE";

/// Compare without an early exit, so a wrong token cannot be discovered a byte
/// at a time by timing the reply.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The git gate of the run folder `cwd` in `gates` ([`Daemon::git_gate`]).
async fn git_gate_in(gates: &Mutex<HashMap<String, Arc<Mutex<()>>>>, cwd: &str) -> Arc<Mutex<()>> {
    let mut gates = gates.lock().await;
    gates.retain(|_, gate| Arc::strong_count(gate) > 1);
    Arc::clone(gates.entry(cwd.to_string()).or_default())
}

/// The refusal for GitHub not giving an answer before anything was done.
fn unavailable(why: crate::github::Unavailable) -> ActionOutcome {
    use crate::github::Unavailable;
    use protocol::ws::git_refusal as code;
    ActionOutcome::Refused(
        match why {
            Unavailable::Missing => code::GH_MISSING,
            Unavailable::SignedOut => code::GH_SIGNED_OUT,
            Unavailable::Down => code::GITHUB_UNAVAILABLE,
        },
        None,
    )
}

/// What an action on a run's repository came to, before it is kept.
enum ActionOutcome {
    /// Done: the answer, the notice it files, and what was done, in words for a log.
    Done {
        result: protocol::ws::GitActionResult,
        notice: Option<Box<PendingEvent>>,
        what: String,
    },
    /// Not done: a [`protocol::ws::git_refusal`] code and, for some, the tool's words.
    Refused(&'static str, Option<String>),
    /// It may have been done, and the tool could not say: never tried again.
    Unknown(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------------------------------------------- the codeconnect launcher

    /// This thread's id, spelled so it can be part of a pathname a shell will see.
    /// `ThreadId`'s own `Debug` is `ThreadId(4)`, and the parentheses in an unquoted
    /// redirect target are a syntax error rather than a filename.
    fn thread_tag() -> String {
        format!("{:?}", std::thread::current().id())
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect()
    }

    /// **An override that names nothing is said once, even when a fallback works.**
    ///
    /// [`launcher_among`] passes an unusable override and takes the next candidate, so
    /// on any machine that has one — which is every installed machine — the operator's
    /// mistyped path is never mentioned anywhere at all: the daemon runs happily,
    /// through a launcher they did not choose, and the only symptom is the wrong build
    /// reading this build's records. The complaint is only reachable today when NO
    /// candidate exists.
    #[test]
    fn an_override_that_names_nothing_is_reported_even_though_it_is_passed() {
        let missing = std::env::temp_dir().join(format!(
            "cc-launcher-override-{}-{}",
            std::process::id(),
            thread_tag()
        ));
        assert!(!missing.is_file(), "the fixture path must not exist");

        let why = an_ignored_launcher_override(Some(missing.display().to_string()), None)
            .expect("an override that names nothing must be reported");
        assert!(
            why.contains(LAUNCHER_BIN_ENV) && why.contains(&missing.display().to_string()),
            "and it must name the variable and the path, or an operator cannot fix it: \
             {why}"
        );

        assert_eq!(
            an_ignored_launcher_override(None, None),
            None,
            "no override is not a complaint"
        );
        assert_eq!(
            an_ignored_launcher_override(Some(String::new()), None),
            None,
            "and an empty one is not an override"
        );
        // **A file the running user owns and only they can write.** It used to be
        // `/bin/sh`, which stopped being an example of an obeyed override the moment
        // the override began to be checked the way a spawned binary is: `/bin/sh`
        // belongs to root, and this daemon does not. See
        // [`a_launcher_override_that_anyone_could_rewrite_is_not_obeyed`].
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let ours = std::env::temp_dir().join(format!(
            "cc-launcher-obeyed-{}-{}",
            std::process::id(),
            thread_tag()
        ));
        std::fs::write(&ours, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&ours, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = std::fs::metadata(&ours).unwrap().uid();
        assert_eq!(
            an_ignored_launcher_override(Some(ours.display().to_string()), Some(owner)),
            None,
            "an override that names a binary this daemon may spawn is obeyed, not \
             complained about"
        );
        std::fs::remove_file(&ours).ok();
    }

    /// **The override names a binary this daemon will SPAWN, so it is asked what a
    /// binary is asked.**
    ///
    /// `CODECONNECT_LAUNCHER_BIN` was `is_file()` and nothing else, and the file it
    /// selects is executed every five minutes by a launchd job holding the APNs signing
    /// key and this Mac's tailnet identity. A world-writable path was obeyed: anybody who
    /// could write it owned that job's next tick.
    ///
    /// Each condition is asserted on its own, because each is a different way the
    /// override stops being the operator's choice and a person told "ignored" without
    /// being told which one cannot fix it. Then the whole thing is asserted through
    /// [`launcher_among`], which is where it matters: a loose override is passed over for
    /// the launcher beside this daemon rather than obeyed, and
    /// [`an_ignored_launcher_override`] says so at startup rather than letting the
    /// working fallback swallow it.
    ///
    /// **Mutation:** delete any one of the three tests in [`launcher_is_spawnable`] and
    /// the leg naming it goes green.
    #[test]
    fn a_launcher_override_that_anyone_could_rewrite_is_not_obeyed() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!(
            "cc-launcher-guard-{}-{}",
            std::process::id(),
            thread_tag()
        ));
        struct Tree(std::path::PathBuf);
        impl Drop for Tree {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).ok();
            }
        }
        let _tree = Tree(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        // Whoever owns this tree is whoever is running the test, which is exactly the
        // relationship production asserts between the override and `~/.codeconnect`.
        // Taken from the filesystem rather than from a syscall for the reason
        // [`state_dir_owner`] gives: this crate has no `libc`.
        let owner = std::fs::metadata(&dir).unwrap().uid();
        let write = |path: &std::path::Path, mode: u32| {
            std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };

        // The one shape that is admitted: a regular file, ours, and ours alone to write.
        let beside = dir.join("beside");
        std::fs::create_dir_all(&beside).unwrap();
        let good = beside.join("codeconnect");
        write(&good, 0o755);
        assert_eq!(
            launcher_is_spawnable(&good, Some(owner)),
            Ok(()),
            "an ordinary 0755 binary this user owns is what the override is FOR"
        );

        // Writable by everyone: rewritable between the check and the spawn.
        let loose = dir.join("loose");
        write(&loose, 0o666);
        let why = launcher_is_spawnable(&loose, Some(owner))
            .expect_err("a world-writable binary must be refused");
        assert!(
            why.contains("writable by its group or by everyone") && why.contains("0666"),
            "and the refusal must name the mode, or an operator cannot fix it: {why}"
        );
        // Group-writable alone is the same fault: `0o022` and not `0o002`.
        let group = dir.join("group");
        write(&group, 0o775);
        assert!(
            launcher_is_spawnable(&group, Some(owner)).is_err(),
            "a group-writable binary is rewritable by everyone in that group"
        );

        // Not a program at all.
        let a_directory = dir.join("adir");
        std::fs::create_dir_all(&a_directory).unwrap();
        let why = launcher_is_spawnable(&a_directory, Some(owner))
            .expect_err("a directory must be refused");
        assert!(why.contains("not a regular file"), "{why}");

        // Somebody else's file, asserted by moving the EXPECTATION rather than the
        // file: a test cannot chown, and the comparison is the thing under test.
        let why = launcher_is_spawnable(&good, Some(owner.wrapping_add(1)))
            .expect_err("another user's binary must be refused");
        assert!(
            why.contains("belongs to uid"),
            "and it must name both uids: {why}"
        );

        // An owner this daemon could not establish leaves the other two conditions
        // standing rather than admitting everything.
        assert_eq!(launcher_is_spawnable(&good, None), Ok(()));
        assert!(
            launcher_is_spawnable(&loose, None).is_err(),
            "an unreadable state directory must not turn the mode check off too"
        );

        // And the whole of it where it acts: the loose override is passed over for the
        // launcher beside this daemon, and the pass-over says why.
        assert_eq!(
            launcher_among(
                Some(loose.display().to_string()),
                Some(beside.join("ccd")),
                dir.join("prefix"),
                Some(owner),
            ),
            Ok(good),
            "an override anyone in the group can rewrite is not this daemon's to spawn"
        );
        let why = an_ignored_launcher_override(Some(loose.display().to_string()), Some(owner))
            .expect("a rewritable override must be reported at startup");
        assert!(
            why.contains(LAUNCHER_BIN_ENV) && why.contains("writable"),
            "or the daemon runs a launcher the operator did not choose and never says so: \
             {why}"
        );
    }

    /// **Which launcher the daemon runs, and what it says when there is none.**
    ///
    /// A daemon running out of a dev tree on a machine that also has an installed
    /// release must reach the launcher OF ITS OWN BUILD — the one beside it — and not
    /// name another release's `codeconnect` to its runs. The override outranks both,
    /// and an override that names nothing is reported rather than obeyed, so a mistyped
    /// path is learned from the log.
    #[test]
    fn the_launcher_is_looked_for_beside_the_daemon_before_the_install_prefix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "cc-codex-sweep-where-{}-{}",
            std::process::id(),
            thread_tag()
        ));
        struct Tree(std::path::PathBuf);
        impl Drop for Tree {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).ok();
            }
        }
        let _tree = Tree(dir.clone());
        let beside = dir.join("beside");
        let prefix = dir.join("prefix").join("bin");
        let named = dir.join("named");
        for d in [&beside, &prefix, &named] {
            std::fs::create_dir_all(d).unwrap();
        }
        for d in [&beside, &prefix, &named] {
            let bin = d.join("codeconnect");
            std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let exe = beside.join("ccd");
        let root = dir.join("prefix");
        // The uid the override is required to belong to. See
        // [`a_launcher_override_that_anyone_could_rewrite_is_not_obeyed`] for what else
        // it is asked; here it is only the precedence that is under test.
        let owner = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&dir).unwrap().uid()
        };

        assert_eq!(
            launcher_among(
                Some(named.join("codeconnect").display().to_string()),
                Some(exe.clone()),
                root.clone(),
                Some(owner),
            ),
            Ok(named.join("codeconnect")),
            "the override outranks everything"
        );
        assert_eq!(
            launcher_among(None, Some(exe.clone()), root.clone(), Some(owner)),
            Ok(beside.join("codeconnect")),
            "the launcher beside this daemon is the one of this build"
        );
        assert_eq!(
            launcher_among(
                Some(String::new()),
                Some(exe.clone()),
                root.clone(),
                Some(owner)
            ),
            Ok(beside.join("codeconnect")),
            "an empty override is not an override"
        );
        std::fs::remove_file(beside.join("codeconnect")).unwrap();
        assert_eq!(
            launcher_among(None, Some(exe.clone()), root.clone(), Some(owner)),
            Ok(prefix.join("codeconnect")),
            "and the install prefix is the fallback"
        );

        std::fs::remove_file(prefix.join("codeconnect")).unwrap();
        let nowhere = launcher_among(
            Some(named.join("gone").display().to_string()),
            Some(exe),
            root,
            Some(owner),
        )
        .expect_err("nothing is there");
        for expected in [
            "CODECONNECT_LAUNCHER_BIN",
            "beside this daemon",
            "the install prefix",
        ] {
            assert!(
                nowhere.contains(expected),
                "every place looked at must be named, so an operator can put one there: \
                 {nowhere}"
            );
        }
    }

    // The sweep's own fixtures, so the tests that hold `revoke` to running it
    // and the tests that hold the sweep to its rules cannot drift into
    // disagreeing about what an installed entry looks like.
    use crate::legacy_credentials::test_support::{
        write as legacy_write, FakeHome, MIXED, SURVIVORS,
    };

    /// **Every test that reaches `revoke` needs one of these, including the ones
    /// that are not about the sweep at all.**
    ///
    /// `revoke` sweeps `$HOME/.ssh/authorized_keys` unconditionally, so a test
    /// that revokes under the developer's own `$HOME` points a function whose
    /// job is deleting lines from `authorized_keys` at the developer's
    /// `authorized_keys`. It also runs beside the sweep's own tests, which is
    /// its own kind of wrong: `FakeHome` holds a process-wide lock precisely so
    /// that only one test at a time is inside a sweep, and a test that skips it
    /// is a second sweep running through the seams the first one armed.
    ///
    /// Named for the test so a leftover directory in `$TMPDIR` says who left it.
    fn redirected_home(tag: &str) -> FakeHome {
        FakeHome::new(tag)
    }

    // ---------------------------------------------------- daemon harness

    /// A daemon with a private database and no agent hosts.
    /// The reason a kept message's sender is told, by the switch of the CodeConnect home these tests run under,
    /// whatever it is set to; what each answer of the switch gives is `mail.rs`'s and `ipc.rs`'s to prove.
    fn shown_only() -> Option<&'static str> {
        (!protocol::ipc::mail_acts(&protocol::root_dir())).then_some(protocol::ipc::MAIL_SHOW_ONLY)
    }

    fn test_daemon() -> Arc<Daemon> {
        daemon_with(Config::default())
    }

    fn daemon_with(config: Config) -> Arc<Daemon> {
        daemon_on(shared_store(), config)
    }

    /// A store two daemons can share, so a test can express "ccd was killed and
    /// came back" as literally that rather than as a mock of it.
    fn shared_store() -> Arc<Store> {
        shared_store_on_disk().0
    }

    /// The same store, keeping the file it lives in — for the tests that have to
    /// ask SQLite directly what is in a table. `Store` exposes readers for the
    /// rows *it* knows how to make, and a test whose whole claim is "no row of
    /// any shape landed here" must not be answered through that surface.
    fn shared_store_on_disk() -> (Arc<Store>, crate::store::TempDb) {
        let store = Store::open_temp("ccd-state");
        let path = store.temp_db();
        (Arc::new(store), path)
    }

    fn daemon_on(store: Arc<Store>, config: Config) -> Arc<Daemon> {
        Daemon::new(
            config,
            store,
            Arc::new(crate::apns::LoggingPushSender::new()),
            Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
        )
    }

    const STATIC_TOKEN: &str = "static-token-for-tests";

    /// The run every hook in these tests belongs to. Fixed rather than minted so
    /// a failure message names the same identity every time.
    const TEST_UID: &str = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";

    fn test_key() -> SessionKey {
        assert!(protocol::uid::is_well_formed(TEST_UID));
        SessionKey::new(TEST_UID, "cc-1")
    }

    async fn hello_with_code(daemon: &Arc<Daemon>, code: &str) -> AuthOutcome {
        daemon
            .authenticate(
                STATIC_TOKEN,
                None,
                Some(code),
                Some("iPhone"),
                Err(protocol::tailnet::Refusal::Unknown),
            )
            .await
    }

    /// One paired phone, and the id `revoke` takes it away by.
    async fn paired_device(daemon: &Arc<Daemon>) -> String {
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        let AuthOutcome::Paired { device_id, .. } = hello_with_code(daemon, &code).await else {
            panic!("pairing must succeed");
        };
        device_id
    }

    // ------------------------------------------------- Tailscale sign-in

    /// A hello from the Tailscale device `node` with neither a token nor a code.
    async fn hello_through_tailscale(daemon: &Arc<Daemon>, node: Option<&str>) -> AuthOutcome {
        daemon
            .authenticate(
                STATIC_TOKEN,
                None,
                None,
                Some("iPad"),
                node.ok_or(protocol::tailnet::Refusal::Unknown),
            )
            .await
    }

    async fn token_works(daemon: &Arc<Daemon>, token: &str) -> bool {
        matches!(
            daemon
                .authenticate(
                    STATIC_TOKEN,
                    Some(token),
                    None,
                    None,
                    Err(protocol::tailnet::Refusal::Unknown)
                )
                .await,
            AuthOutcome::Device(_)
        )
    }

    /// **The owner's own Tailscale device pairs with no code**, as a code would
    /// pair it: a new device, a new token, the Tailscale device recorded. Each such
    /// sign-in mints its own. With no Tailscale device behind it, it is refused.
    #[tokio::test]
    async fn a_hello_with_no_credential_from_a_trusted_tailscale_device_pairs_it() {
        let daemon = test_daemon();
        let AuthOutcome::Paired {
            device_id: first,
            token: first_token,
            ..
        } = hello_through_tailscale(&daemon, Some("n6")).await
        else {
            panic!("a trusted Tailscale device pairs");
        };
        let AuthOutcome::Paired {
            device_id: second,
            token: second_token,
            ..
        } = hello_through_tailscale(&daemon, Some("n6")).await
        else {
            panic!("and pairs again, fresh");
        };
        assert_ne!(first, second);
        assert_ne!(first_token, second_token);
        assert!(token_works(&daemon, &first_token).await);
        assert!(token_works(&daemon, &second_token).await);
        let devices = daemon.list_devices().await.unwrap();
        assert!(devices
            .iter()
            .all(|d| d.tailnet_node.as_deref() == Some("n6") && !d.paired_before_nodes));

        assert!(matches!(
            hello_through_tailscale(&daemon, None).await,
            AuthOutcome::Rejected(_)
        ));
        assert_eq!(daemon.list_devices().await.unwrap().len(), 2);
    }

    /// **Blocking a device keeps the whole Tailscale device out, live, and
    /// after a restart.** Revoking one of its two devices revokes both, publishes
    /// both to the open connections, refuses both tokens, and refuses its next
    /// sign-in, on this daemon and on the next one over the same database.
    #[tokio::test]
    async fn revoking_one_device_keeps_its_whole_tailscale_device_out_across_a_restart() {
        let _home = redirected_home("revoke-node");
        let store = shared_store();
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        let paired = |outcome: AuthOutcome| match outcome {
            AuthOutcome::Paired {
                device_id, token, ..
            } => (device_id, token),
            other => panic!("a trusted Tailscale device pairs: {other:?}"),
        };
        let (a, a_token) = paired(hello_through_tailscale(&daemon, Some("n6")).await);
        let (b, b_token) = paired(hello_through_tailscale(&daemon, Some("n6")).await);
        let mut published = daemon.revocations_tx.subscribe();

        let outcome = daemon.revoke(&a).await.unwrap();
        assert!(outcome.token_revoked);
        let mut closed = Vec::new();
        while let Ok(Ok(id)) =
            tokio::time::timeout(Duration::from_millis(500), published.recv()).await
        {
            closed.push(id);
        }
        closed.sort();
        let mut expected = vec![a.clone(), b.clone()];
        expected.sort();
        assert_eq!(closed, expected, "both open connections are told");

        let restarted = daemon_on(store, Config::default());
        for daemon in [&daemon, &restarted] {
            assert!(!token_works(daemon, &a_token).await);
            assert!(!token_works(daemon, &b_token).await);
            assert!(matches!(
                hello_through_tailscale(daemon, Some("n6")).await,
                AuthOutcome::Rejected(_)
            ));
            assert!(matches!(
                hello_through_tailscale(daemon, Some("n7")).await,
                AuthOutcome::Paired { .. }
            ));
        }
    }

    /// **A token or a code presented from a Tailscale device records it**, so
    /// a device paired by code, revoked later, keeps that device out too, and so
    /// does a revoked device that comes back with its old token.
    #[tokio::test]
    async fn a_token_or_a_code_from_a_tailscale_device_records_it_for_the_block_list() {
        let _home = redirected_home("stamp-node");
        let daemon = test_daemon();

        // Paired by code with no Tailscale behind it, then seen through Tailscale.
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        let AuthOutcome::Paired {
            device_id, token, ..
        } = hello_with_code(&daemon, &code).await
        else {
            panic!("pairing must succeed");
        };
        assert!(matches!(
            daemon
                .authenticate(STATIC_TOKEN, Some(&token), None, None, Ok("n2"))
                .await,
            AuthOutcome::Device(_)
        ));
        daemon.revoke(&device_id).await.unwrap();
        assert!(matches!(
            hello_through_tailscale(&daemon, Some("n2")).await,
            AuthOutcome::Rejected(_)
        ));

        // Revoked before it was ever seen through Tailscale; its old token, from
        // a Tailscale device, is refused and names that device.
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        let AuthOutcome::Paired {
            device_id, token, ..
        } = hello_with_code(&daemon, &code).await
        else {
            panic!("pairing must succeed");
        };
        daemon.revoke(&device_id).await.unwrap();
        assert!(matches!(
            daemon
                .authenticate(STATIC_TOKEN, Some(&token), None, None, Ok("n3"))
                .await,
            AuthOutcome::Revoked
        ));
        assert!(matches!(
            hello_through_tailscale(&daemon, Some("n3")).await,
            AuthOutcome::Rejected(_)
        ));

        // A code redeemed from a Tailscale device records it when the device is
        // made, before the ack.
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        let AuthOutcome::Paired { device_id, .. } = daemon
            .authenticate(STATIC_TOKEN, None, Some(&code), Some("iPhone"), Ok("n4"))
            .await
        else {
            panic!("pairing must succeed");
        };
        let devices = daemon.list_devices().await.unwrap();
        let device = devices.iter().find(|d| d.device_id == device_id).unwrap();
        assert_eq!(device.tailnet_node.as_deref(), Some("n4"));
    }

    /// **`codeconnect unblock` gives a blocked device its own pairing back**: the
    /// token it already holds signs in again as the same device, whether it paired
    /// by code or through Tailscale, and its Tailscale device is no longer kept
    /// out. A device that is not blocked is refused.
    #[tokio::test]
    async fn unblocking_a_device_lets_its_own_token_sign_in_again() {
        let _home = redirected_home("unblock-node");
        let daemon = test_daemon();
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        let AuthOutcome::Paired {
            device_id: phone,
            token: phone_token,
            ..
        } = hello_with_code(&daemon, &code).await
        else {
            panic!("pairing must succeed");
        };
        let AuthOutcome::Paired {
            device_id: ipad,
            token: ipad_token,
            ..
        } = hello_through_tailscale(&daemon, Some("n6")).await
        else {
            panic!("a trusted Tailscale device pairs");
        };
        let refused = daemon.unblock(&ipad).await.unwrap_err().to_string();
        assert_eq!(refused, "iPad is not blocked");

        // Each asks as the app asks: blocked, its token is refused and so is a
        // hello with no credential from where it is; allowed again, its token.
        for (device_id, token, node) in [
            (&phone, &phone_token, None),
            (&ipad, &ipad_token, Some("n6")),
        ] {
            daemon.revoke(device_id).await.unwrap();
            assert!(!token_works(&daemon, token).await);
            assert!(matches!(
                hello_through_tailscale(&daemon, node).await,
                AuthOutcome::Rejected(_)
            ));
            let unblocked = daemon.unblock(device_id).await.unwrap();
            let from = node.ok_or(protocol::tailnet::Refusal::Unknown);
            match daemon
                .authenticate(STATIC_TOKEN, Some(token), None, None, from)
                .await
            {
                AuthOutcome::Device(device) => assert_eq!(&device.device_id, device_id),
                other => panic!("its own token must sign in again: {other:?}"),
            }
            assert_eq!(&unblocked.device_id, device_id);
            assert_eq!(unblocked.revoked_at, None, "answered as it now stands");
        }
        assert!(daemon.db.blocked_tailnet_nodes().await.unwrap().is_empty());
        assert_eq!(
            daemon.list_devices().await.unwrap().len(),
            2,
            "the same two devices, none paired afresh"
        );
    }

    /// A removed computer's hello with no credential is refused for that reason,
    /// which is what the daemon logs, not as an address Tailscale does not know.
    #[tokio::test]
    async fn a_removed_computers_hello_is_refused_as_removed() {
        let daemon = test_daemon();
        let refused = |outcome: AuthOutcome| match outcome {
            AuthOutcome::Rejected(reason) => reason,
            other => panic!("must be refused: {other:?}"),
        };
        let removed = refused(
            daemon
                .authenticate(
                    STATIC_TOKEN,
                    None,
                    None,
                    Some("iPad"),
                    Err(protocol::tailnet::Refusal::Blocked),
                )
                .await,
        );
        assert!(removed.contains("was revoked"), "{removed}");
        let unknown = refused(
            daemon
                .authenticate(
                    STATIC_TOKEN,
                    None,
                    None,
                    Some("iPad"),
                    Err(protocol::tailnet::Refusal::Unknown),
                )
                .await,
        );
        assert!(unknown.contains("does not vouch for"), "{unknown}");
    }

    /// **A computer removed as a viewer gets no Mail traffic either way**, through
    /// the daemon's one reading of the tailnet: a request from it is refused before
    /// anything is served, a message to it is refused before any dial, and it is
    /// neither asked for its agents nor told what became of a message. Unblocked,
    /// it is served and reachable again.
    #[tokio::test]
    async fn a_blocked_computer_gets_no_mail_traffic_either_way() {
        use protocol::peer::{PeerReply, PeerRequest};
        let _home = redirected_home("mail-block");
        let daemon = test_daemon();
        *daemon
            .test_tailnet
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(six_nodes());
        let laptop: std::net::IpAddr = "100.64.0.6".parse().unwrap();
        let from = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &from, "cc-1");
        daemon.mail_run_live(&from).await;
        let ask = || daemon.serve_peer(laptop, PeerRequest::PeerListAgents);
        assert!(matches!(ask().await, PeerReply::PeerAgents { .. }));

        let AuthOutcome::Paired { device_id, .. } =
            hello_through_tailscale(&daemon, Some("n0000000006CNTRL")).await
        else {
            panic!("a trusted Tailscale device pairs");
        };
        daemon.revoke(&device_id).await.unwrap();

        match ask().await {
            PeerReply::PeerRefused { reason } => assert!(reason.contains("removed"), "{reason}"),
            other => panic!("a blocked computer must be refused: {other:?}"),
        }
        let sent = daemon
            .send_agent_message(
                &from,
                &format!("{}@laptop", protocol::uid::new().unwrap()),
                "hi",
                None,
            )
            .await;
        assert_eq!(sent.state, protocol::ipc::MailState::Refused, "{sent:?}");
        assert_eq!(
            sent.reason.as_deref(),
            Some("laptop was removed from this computer")
        );
        let reading = daemon.read_tailnet().await.unwrap();
        assert_eq!(
            reading.reachable_peers().count(),
            0,
            "nobody left to ask or tell"
        );
        assert_eq!(
            reading.locate("laptop"),
            Err(protocol::tailnet::Refusal::Blocked),
            "the sweep's retries and told changes find it the same way"
        );

        daemon.unblock(&device_id).await.unwrap();
        assert!(matches!(ask().await, PeerReply::PeerAgents { .. }));
        assert!(daemon
            .read_tailnet()
            .await
            .unwrap()
            .locate("laptop")
            .is_ok());
    }

    // ------------------------------------------------------------ pairing

    /// A phone that has just paired has been seen: it stays on the connection it
    /// paired over, so without this `codeconnect devices` says "never" for a
    /// phone that is connected right now.
    #[tokio::test]
    async fn a_phone_is_seen_when_it_pairs() {
        let daemon = test_daemon();
        let device_id = paired_device(&daemon).await;
        let devices = daemon.list_devices().await.unwrap();
        let device = devices.iter().find(|d| d.device_id == device_id).unwrap();
        assert_eq!(device.last_seen_at.as_ref(), Some(&device.created_at));
    }

    #[tokio::test]
    async fn a_pairing_code_buys_exactly_one_device_token() {
        let daemon = test_daemon();
        let (code, _) = daemon.create_pairing(300).await.unwrap();

        let token = match hello_with_code(&daemon, &code).await {
            AuthOutcome::Paired {
                token, device_name, ..
            } => {
                assert_eq!(device_name, "iPhone");
                assert_eq!(token.len(), 64);
                token
            }
            other => panic!("pairing must succeed: {other:?}"),
        };

        // The minted token authenticates on its own from now on.
        match daemon
            .authenticate(
                STATIC_TOKEN,
                Some(&token),
                None,
                None,
                Err(protocol::tailnet::Refusal::Unknown),
            )
            .await
        {
            AuthOutcome::Device(device) => assert_eq!(device.name, "iPhone"),
            other => panic!("the device token must authenticate: {other:?}"),
        }

        // And the code is spent: a second phone scanning the same screen fails.
        assert!(matches!(
            hello_with_code(&daemon, &code).await,
            AuthOutcome::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn codes_are_normalised_the_way_a_human_would_type_them() {
        let daemon = test_daemon();
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        let typed = format!(
            "  {}  ",
            protocol::pairing::format_for_display(&code).to_lowercase()
        );
        assert!(matches!(
            hello_with_code(&daemon, &typed).await,
            AuthOutcome::Paired { .. }
        ));
    }

    #[tokio::test]
    async fn a_wrong_or_malformed_code_pairs_nothing() {
        let daemon = test_daemon();
        for attempt in ["", "nope", "ABCD2345", "AAAAAAAA", "0000000O"] {
            assert!(
                matches!(
                    hello_with_code(&daemon, attempt).await,
                    AuthOutcome::Rejected(_)
                ),
                "{attempt:?} must not pair"
            );
        }
        assert!(daemon.list_devices().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_expired_code_pairs_nothing() {
        let daemon = test_daemon();
        // A TTL already in the past: the code exists but can never be redeemed.
        let (code, _) = daemon.create_pairing(0).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(matches!(
            hello_with_code(&daemon, &code).await,
            AuthOutcome::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn a_hello_with_no_credential_at_all_is_refused() {
        let daemon = test_daemon();
        assert!(matches!(
            daemon
                .authenticate(
                    STATIC_TOKEN,
                    None,
                    None,
                    None,
                    Err(protocol::tailnet::Refusal::Unknown)
                )
                .await,
            AuthOutcome::Rejected(_)
        ));
        assert!(matches!(
            daemon
                .authenticate(
                    STATIC_TOKEN,
                    Some(""),
                    None,
                    None,
                    Err(protocol::tailnet::Refusal::Unknown)
                )
                .await,
            AuthOutcome::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn the_static_token_keeps_working_alongside_device_tokens() {
        // The upgrade guarantee: a phone paired before device tokens existed
        // must not be locked out because the Mac learned how to mint them.
        let daemon = test_daemon();
        assert!(matches!(
            daemon
                .authenticate(
                    STATIC_TOKEN,
                    Some(STATIC_TOKEN),
                    None,
                    None,
                    Err(protocol::tailnet::Refusal::Unknown)
                )
                .await,
            AuthOutcome::Static
        ));
        assert!(matches!(
            daemon
                .authenticate(
                    STATIC_TOKEN,
                    Some("not-the-token"),
                    None,
                    None,
                    Err(protocol::tailnet::Refusal::Unknown)
                )
                .await,
            AuthOutcome::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn a_token_wins_over_a_pairing_code_and_leaves_it_unspent() {
        let daemon = test_daemon();
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        // Re-pairing an already-paired phone would strand its first credential.
        assert!(matches!(
            daemon
                .authenticate(
                    STATIC_TOKEN,
                    Some(STATIC_TOKEN),
                    Some(&code),
                    None,
                    Err(protocol::tailnet::Refusal::Unknown)
                )
                .await,
            AuthOutcome::Static
        ));
        assert!(
            matches!(
                hello_with_code(&daemon, &code).await,
                AuthOutcome::Paired { .. }
            ),
            "the unused code must still be redeemable"
        );
    }

    #[tokio::test]
    async fn revoking_a_device_stops_its_token_working() {
        let _home = redirected_home("revoke-token");
        let daemon = test_daemon();
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        let AuthOutcome::Paired {
            token, device_id, ..
        } = hello_with_code(&daemon, &code).await
        else {
            panic!("pairing must succeed");
        };

        let outcome = daemon.revoke(&device_id).await.unwrap();
        assert!(outcome.token_revoked);
        assert!(matches!(
            daemon
                .authenticate(
                    STATIC_TOKEN,
                    Some(&token),
                    None,
                    None,
                    Err(protocol::tailnet::Refusal::Unknown)
                )
                .await,
            AuthOutcome::Revoked
        ));
        // Still listed, so "revoked on the 3rd" survives as a fact.
        let devices = daemon.list_devices().await.unwrap();
        assert_eq!(devices.len(), 1);
        assert!(!devices[0].is_active());
    }

    #[tokio::test]
    async fn revocation_is_visible_to_an_already_open_connection() {
        // A phone holds its socket open for hours, so a check that only runs at
        // `hello` would let a revoked device keep answering approvals and typing
        // into the session's TTY until the socket happened to drop. The ws loop
        // re-reads this on every message and on the keepalive tick.
        let _home = redirected_home("revoke-active");
        let daemon = test_daemon();
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        let AuthOutcome::Paired { device_id, .. } = hello_with_code(&daemon, &code).await else {
            panic!("pairing must succeed");
        };
        assert!(daemon.store.device_is_active(&device_id).unwrap());

        daemon.revoke(&device_id).await.unwrap();
        assert!(
            !daemon.store.device_is_active(&device_id).unwrap(),
            "a live connection must be able to notice the revocation"
        );
        // An id that was never issued is not "active" either.
        assert!(!daemon.store.device_is_active("never-existed").unwrap());
    }

    #[tokio::test]
    async fn revoking_something_that_does_not_exist_says_so() {
        let daemon = test_daemon();
        let err = daemon.revoke("ghost").await.unwrap_err().to_string();
        assert!(err.contains("no device matches"), "{err}");
    }

    // ------------------------------- revoke and the legacy `authorized_keys`
    //
    // The daemon also sweeps `~/.ssh/authorized_keys` at startup, and these
    // four are about everything that happens *after* startup: `revoke` is the
    // command an operator is told takes a device's access away, and it used to
    // leave a phone's shell exactly where it was.
    //
    // Every one of them redirects `HOME` through `FakeHome`, which is the only
    // supported way to reach a sweep from a test — see `legacy_credentials`.

    /// **The restored backup.** The daemon started, swept, and has been up ever
    /// since; the file comes back afterwards, from a Time Machine restore or a
    /// dotfiles resync. Nothing retries a startup sweep, so before this the
    /// phone kept its shell for as long as the Mac stayed booted — and the
    /// operator was told the revocation had taken everything.
    #[tokio::test]
    async fn revoking_a_device_sweeps_a_grant_that_appeared_after_startup() {
        let home = FakeHome::new("revoke-restored");
        let daemon = test_daemon();
        let device_id = paired_device(&daemon).await;

        // After the daemon is up and serving: this is the whole point.
        legacy_write(&home.keys(), MIXED, 0o600);
        daemon.revoke(&device_id).await.unwrap();

        assert_eq!(
            home.read_keys().as_deref(),
            Some(SURVIVORS),
            "the tagged pair goes and the user's own keys come through byte for byte"
        );
    }

    /// The common case, and the reason running this on every revocation is
    /// affordable: a file with nothing of ours in it is read and not written.
    #[tokio::test]
    async fn revoking_a_device_leaves_a_file_with_nothing_to_sweep_untouched() {
        let home = FakeHome::new("revoke-quiet");
        let daemon = test_daemon();
        let device_id = paired_device(&daemon).await;
        legacy_write(&home.keys(), SURVIVORS, 0o644);

        let before = std::fs::metadata(home.keys()).unwrap();
        daemon.revoke(&device_id).await.unwrap();
        let after = std::fs::metadata(home.keys()).unwrap();

        assert_eq!(home.read_keys().as_deref(), Some(SURVIVORS));
        // The inode is the proof that no replacement was renamed into place:
        // every write goes through a fresh temporary, so an untouched file is
        // the same file.
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&before),
            std::os::unix::fs::MetadataExt::ino(&after),
            "a revoke with nothing to sweep must not rewrite the file"
        );
        let strays = home.strays();
        assert!(
            strays.is_empty(),
            "no half-written replacement either: {strays:?}"
        );
    }

    /// **A file this command does not own may not decide what it reports.** The
    /// token withdrawal is done and stored by the time the sweep runs; a
    /// revocation that failed over an unwritable `~/.ssh` would tell the
    /// operator their phone still holds a token it no longer holds, and invite a
    /// retry of something that already happened.
    #[tokio::test]
    async fn a_revoke_succeeds_when_the_sweep_cannot_rewrite_the_file() {
        let home = FakeHome::new("revoke-readonly");
        let daemon = test_daemon();
        let device_id = paired_device(&daemon).await;
        legacy_write(&home.keys(), MIXED, 0o600);
        home.seal();

        let outcome = daemon.revoke(&device_id).await.unwrap();

        assert!(
            outcome.token_revoked,
            "the withdrawal that did happen is what this reports"
        );
        assert!(!daemon.store.device_is_active(&device_id).unwrap());
        assert_eq!(
            home.read_keys().as_deref(),
            Some(MIXED),
            "a failed replacement leaves the original whole"
        );
    }

    /// **The retry, which is the only one an operator has short of restarting
    /// the daemon.** The sweep is best-effort, and the two ways it fails on a
    /// file it can see — `~/.ssh` unwritable, the file rewritten underneath it —
    /// both tell the operator to run `codeconnect revoke` again. That advice is
    /// worth nothing if the second call short-circuits on a device whose token
    /// is already gone: the sweep is unconditional, or the documented recovery
    /// path does not exist.
    #[tokio::test]
    async fn revoking_an_already_revoked_device_sweeps_again() {
        let home = FakeHome::new("revoke-twice");
        let daemon = test_daemon();
        let device_id = paired_device(&daemon).await;

        // The first revocation takes the token, over a Mac that has no such
        // file at all — the sweep runs and finds nothing.
        let first = daemon.revoke(&device_id).await.unwrap();
        assert!(first.token_revoked);
        assert!(home.read_keys().is_none(), "there was nothing to sweep yet");

        // And then the grant comes back — a restore, a resync, or an `~/.ssh`
        // that was unwritable at the first attempt and is not now.
        legacy_write(&home.keys(), MIXED, 0o600);
        let second = daemon.revoke(&device_id).await.unwrap();

        assert!(
            !second.token_revoked,
            "the token was already gone, and the command still reports honestly"
        );
        assert_eq!(
            home.read_keys().as_deref(),
            Some(SURVIVORS),
            "the sweep runs on a device that was already revoked, or the retry an operator \
             is told to run does nothing"
        );
    }

    #[tokio::test]
    async fn pairing_attempts_are_capped_inside_the_window() {
        // A pairing code has real entropy and lives five minutes, so this is
        // not what stops a guess — it is what stops an *unbounded number* of
        // guesses. The safety argument for a short-lived code is "you cannot
        // try enough of them in five minutes", and that is only true if
        // something is counting. Nothing was.
        let daemon = daemon_with(Config {
            pairing_max_attempts: 3,
            pairing_window_secs: 300,
            ..Config::default()
        });
        for attempt in 0..3 {
            let outcome = hello_with_code(&daemon, "AAAA-BBBB").await;
            assert!(
                matches!(outcome, AuthOutcome::Rejected(ref why) if why.contains("unknown")),
                "attempt {attempt} should have been an ordinary rejection: {outcome:?}"
            );
        }
        // The fourth is refused before the store is consulted at all.
        match hello_with_code(&daemon, "AAAA-BBBB").await {
            AuthOutcome::Rejected(why) => assert!(
                why.contains("too many"),
                "the limiter must be what refuses this one: {why}"
            ),
            other => panic!("a fourth attempt must be refused: {other:?}"),
        }

        // And a *real* code is refused too, which is the point: the window has
        // to close for everybody or it closes for nobody.
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        assert!(matches!(
            hello_with_code(&daemon, &code).await,
            AuthOutcome::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn a_successful_pairing_does_not_count_against_the_limit() {
        // A phone pairing normally must never push the daemon towards its own
        // limit, or a household with several devices would lock itself out.
        let daemon = daemon_with(Config {
            pairing_max_attempts: 2,
            pairing_window_secs: 300,
            ..Config::default()
        });
        for _ in 0..5 {
            let (code, _) = daemon.create_pairing(300).await.unwrap();
            assert!(matches!(
                hello_with_code(&daemon, &code).await,
                AuthOutcome::Paired { .. }
            ));
        }
    }

    #[tokio::test]
    async fn the_window_reopens_once_the_failures_age_out() {
        let daemon = daemon_with(Config {
            pairing_max_attempts: 2,
            // One second, so the test measures the sliding window rather than
            // waiting out a production-sized one.
            pairing_window_secs: 1,
            ..Config::default()
        });
        for _ in 0..2 {
            assert!(matches!(
                hello_with_code(&daemon, "AAAA-BBBB").await,
                AuthOutcome::Rejected(_)
            ));
        }
        assert!(
            !daemon
                .admit_pairing_attempt(protocol::time::now_unix_ms())
                .await
        );
        // Far enough past the window that the two recorded failures fall out.
        let later = protocol::time::now_unix_ms() + 1_500;
        assert!(
            daemon.admit_pairing_attempt(later).await,
            "a sliding window that never reopens is a lockout, not a rate limit"
        );
    }

    #[tokio::test]
    async fn a_zero_limit_disables_the_limiter() {
        let daemon = daemon_with(Config {
            pairing_max_attempts: 0,
            ..Config::default()
        });
        for _ in 0..50 {
            assert!(matches!(
                hello_with_code(&daemon, "AAAA-BBBB").await,
                AuthOutcome::Rejected(ref why) if why.contains("unknown")
            ));
        }
    }

    // --------------------------------------------------- session identity

    #[tokio::test]
    async fn an_unknown_session_gets_an_unavailable_not_an_error() {
        let daemon = test_daemon();
        let result = daemon.command_catalog("u-never-existed").await;
        assert!(matches!(
            result,
            protocol::ws::CommandCatalogResult::Unavailable { .. }
        ));
    }

    /// A tool call filed on the run `uid`, called `name`.
    async fn tool_call(daemon: &Arc<Daemon>, name: &str, uid: &str, tool_use_id: &str) {
        let call = PendingEvent::new(
            &SessionKey::new(uid, name),
            EventKind::ToolCall,
            json!({
                "tool_name": "Bash",
                "tool_input": {"command": "echo hi"},
                "tool_use_id": tool_use_id,
            }),
            Source::Daemon,
        )
        .with_source_event_id(tool_use_id.to_string());
        daemon
            .ingest(call)
            .await
            .unwrap()
            .expect("a new call is filed");
    }

    #[test]
    fn the_launchd_sentinel_is_not_mistaken_for_a_job() {
        // Guards a sentence, and the debugging it would cause: macOS hands a
        // shell-started process `XPC_SERVICE_NAME=0`, and reporting that as a
        // label makes `codeconnect daemon status` claim a hand-started daemon belongs to
        // "a different job (0)".
        //
        // The label variable is process-global and these tests run in threads,
        // and one other test writes it: `main`'s
        // `only_codeconnects_own_launchd_job_counts_as_something_that_would_restart_it`,
        // which asserts what `managed_by_codeconnect_job` makes of the same
        // values. Two writers with no lock between them read each other's
        // settings, so both take [`crate::LaunchdLabelEnv`] — the same one — and
        // it restores what was there when the last of them lets go.
        let env = crate::LaunchdLabelEnv::take();

        env.set(Some("0"));
        assert_eq!(launchd_label(), None, "0 means 'not an XPC service'");
        env.set(Some(""));
        assert_eq!(launchd_label(), None);
        env.set(None);
        assert_eq!(launchd_label(), None);

        // A real label is passed through, ours or not — "managed by somebody
        // else" is a distinct problem and must stay visible.
        env.set(Some(protocol::LAUNCHD_LABEL));
        assert_eq!(launchd_label().as_deref(), Some(protocol::LAUNCHD_LABEL));
        env.set(Some("com.example.other"));
        assert_eq!(launchd_label().as_deref(), Some("com.example.other"));
    }

    #[tokio::test]
    async fn an_approval_answered_in_one_run_leaves_the_next_run_s_card_open() {
        // Approval safety, which is why the ledger is keyed per run: the same
        // `request_id` in a later run must be answerable, not reported as an
        // already-applied duplicate of a decision nobody made for it.
        let daemon = test_daemon();
        let first = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
        hosted_run(&daemon, first, "/tmp");
        daemon
            .store
            .record_answer(
                first,
                "toolu_shared",
                "hash",
                &AnswerOutcome {
                    request_id: "toolu_shared".into(),
                    session_id: "cc-1".into(),
                    decision: AnswerDecision::Allow,
                    resolved_by: ResolvedBy::Phone,
                    answered_on: None,
                    applied_via: AnswerPath::HookReturn,
                    resolved_at: protocol::time::now_rfc3339(),
                    detail: None,
                    indeterminate: false,
                },
            )
            .unwrap();

        let second = "01K1B3XZZZC0DE5FGH7JKMNPQR";
        hosted_run(&daemon, second, "/tmp");
        assert!(
            daemon
                .store
                .get_answer(second, "toolu_shared")
                .unwrap()
                .is_none(),
            "the new run has answered nothing"
        );
        // Naming the new run explicitly, an answer is not a duplicate — it is
        // rejected only because there is no such card open, which is the
        // correct and *different* outcome.
        let result = daemon
            .answer(
                "toolu_shared",
                "hash",
                AnswerDecision::Allow,
                Some(second),
                protocol::ws::AnsweredOn::Iphone,
            )
            .await;
        match result {
            AnswerResult::Rejected { reason } => {
                assert!(reason.contains("unknown"), "{reason}");
            }
            other => panic!("must not resolve from another run's ledger: {other:?}"),
        }
    }

    /// **The session list's branch and line counts are read when a turn ends,
    /// and only then**: a listing between turns runs no git and shows the last
    /// read, however the tree has moved since.
    #[tokio::test]
    async fn a_turn_end_reads_the_tree_into_the_session_list() {
        let repo = std::env::temp_dir().join(format!(
            "ccd-tree-facts-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new(crate::git::git_bin(None).unwrap())
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "first"]);
        std::fs::write(repo.join("a.txt"), "a\nb\n").unwrap();

        let daemon = test_daemon();
        hosted_run(&daemon, TEST_UID, repo.to_str().unwrap());
        let turn_end = |turn: &str| {
            let daemon = Arc::clone(&daemon);
            let end = PendingEvent::new(
                &SessionKey::new(TEST_UID, TEST_UID),
                EventKind::TurnComplete,
                json!({}),
                Source::Daemon,
            )
            .with_source_event_id(turn.to_string());
            async move { daemon.ingest(end).await.unwrap().expect("a new turn end") }
        };
        let read_after = |added: u64| {
            let daemon = Arc::clone(&daemon);
            async move {
                for _ in 0..200 {
                    let summary = summary_of(&daemon, TEST_UID).await;
                    if summary.diff_stat.map(|stat| stat.added) == Some(added) {
                        return summary;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                panic!("the tree was not read within 10 s");
            }
        };

        turn_end("turn-1").await;
        let summary = read_after(1).await;
        assert_eq!(summary.git_branch.as_deref(), Some("main"));
        assert_eq!(
            summary.diff_stat,
            Some(protocol::event::DiffStat {
                added: 1,
                removed: 0
            })
        );

        std::fs::write(repo.join("a.txt"), "a\nb\nc\n").unwrap();
        git(&["checkout", "-q", "-b", "next"]);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let between = summary_of(&daemon, TEST_UID).await;
        assert_eq!(
            (
                between.git_branch.as_deref(),
                between.diff_stat.map(|s| s.added)
            ),
            (Some("main"), Some(1)),
            "a listing between turns reads no git"
        );

        turn_end("turn-2").await;
        let summary = read_after(2).await;
        assert_eq!(summary.git_branch.as_deref(), Some("next"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    // -------------------------------------------------- seeded runs

    /// A hosted run's row with no host attached and no events — exactly the
    /// shape a restart inherits from the process before it.
    fn seed_session(daemon: &Arc<Daemon>, uid: &str, name: &str, lifecycle: Lifecycle) {
        let now = protocol::time::now_rfc3339();
        let row = SessionRow {
            session_uid: uid.into(),
            session_id: name.into(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle,
            created_at: now.clone(),
            updated_at: now,
            agent: protocol::agent::AgentKind::Claude,
            codex_thread_id: None,
        };
        let request = format!("seed-{uid}");
        assert_eq!(
            daemon.store.begin_run(&row, &request, None, None).unwrap(),
            None
        );
    }

    /// A live Codex run's hosted row, with no host attached and no events.
    fn seed_codex_session(daemon: &Arc<Daemon>, uid: &str, name: &str) {
        let now = protocol::time::now_rfc3339();
        let row = SessionRow {
            session_uid: uid.into(),
            session_id: name.into(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: Lifecycle::Live,
            created_at: now.clone(),
            updated_at: now,
            agent: protocol::agent::AgentKind::Codex,
            codex_thread_id: None,
        };
        let request = format!("seed-{uid}");
        assert_eq!(
            daemon.store.begin_run(&row, &request, None, None).unwrap(),
            None
        );
    }

    fn lifecycle_of(daemon: &Arc<Daemon>, uid: &str) -> Lifecycle {
        daemon.store.get_session(uid).unwrap().unwrap().lifecycle
    }

    use crate::apns::CaptureSender;

    /// A daemon whose pushes land in a [`CaptureSender`] instead of APNs — the probe
    /// for the doorbell tests below.
    fn capture_daemon() -> (Arc<Daemon>, Arc<CaptureSender>) {
        capture_daemon_on(shared_store())
    }

    /// A decision the phone can answer, open on another run (`Ledger`) for the
    /// rest of the test. Filed straight into the daemon's map, so it rings
    /// nothing itself: a quiet occasion raised after it has a decision to ring
    /// about, and rings as that decision.
    async fn open_a_decision(daemon: &Daemon) {
        let mut card = pending_card("other-run", "Ledger");
        card.agent = protocol::agent::AgentKind::Codex;
        daemon
            .inner
            .lock()
            .await
            .pending
            .insert(("other-run".into(), "r-open".into()), card);
    }

    /// The same, over a store that already exists — so a test can express "ccd
    /// was killed and came back" and watch what the new one rings about.
    fn capture_daemon_on(store: Arc<Store>) -> (Arc<Daemon>, Arc<CaptureSender>) {
        capture_daemon_configured(store, Config::default())
    }

    /// The same, with a configuration of the test's own.
    fn capture_daemon_configured(
        store: Arc<Store>,
        config: Config,
    ) -> (Arc<Daemon>, Arc<CaptureSender>) {
        let capture = Arc::new(CaptureSender::new());
        let daemon = Daemon::new(
            config,
            store,
            Arc::clone(&capture) as Arc<dyn crate::apns::PushSender>,
            Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
        );
        (daemon, capture)
    }

    /// The doorbell's contract, through a hosted card: no agent text in the
    /// payload (a card's command routinely names files, and agent text was once
    /// copied verbatim into the APNs alert), and no ring for a device whose live
    /// socket already carried the fact.
    #[tokio::test]
    async fn the_doorbell_skips_watchers_and_carries_no_agent_text() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();

        // A phone was watching live: its socket has delivered past anything
        // this test files, so the seen-filter must exclude it at dispatch.
        daemon
            .push_gate
            .note_delivered("watching-phone", TEST_UID, 1_000);

        hosted_card(
            &daemon,
            TEST_UID,
            1,
            "toolu_secret",
            "Bash",
            json!({"command": "cat /Users/alice/secrets/deploy_key.pem"}),
        )
        .await;
        // Dispatch runs behind a short grace so in-flight socket writes can
        // record themselves first; the test ends it, then reads what rang.
        hold.end();
        rung_after(&capture, 1).await;

        let rings = capture.0.lock().unwrap();
        assert_eq!(rings.len(), 1, "one card, one ring: {rings:?}");
        let (hint, excluded) = &rings[0];
        assert_eq!(hint.kind.sentence(), "Waiting on an approval");
        assert!(
            !format!("{hint:?}").contains("deploy_key"),
            "nothing the agent wrote may reach the APNs payload"
        );
        assert_eq!(
            excluded,
            &vec!["watching-phone".to_string()],
            "the device that saw it live must not be rung"
        );
    }

    /// A pending card, hand-built and filed with no host behind it: a Claude card
    /// the computer alone can answer until a test makes it a hosted or a Codex one.
    /// A run on `main` in a repository of its own, with one commit and its own identity.
    fn run_in_a_repository(daemon: &Arc<Daemon>, uid: &str) -> std::path::PathBuf {
        run_in_a_repository_as(
            daemon,
            uid,
            protocol::agent::AgentKind::Claude,
            Lifecycle::Live,
        )
    }

    fn run_in_a_repository_as(
        daemon: &Arc<Daemon>,
        uid: &str,
        agent: protocol::agent::AgentKind,
        lifecycle: Lifecycle,
    ) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ccd-git-action-{uid}-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "t"],
            &["config", "user.email", "t@example.com"],
            &["commit", "-q", "--allow-empty", "-m", "first"],
        ] {
            git_in(&dir, args);
        }
        seed_run_in(daemon, uid, &dir, agent, lifecycle);
        dir
    }

    /// The run `uid` in `dir`, as `agent`, in `lifecycle`.
    fn seed_run_in(
        daemon: &Arc<Daemon>,
        uid: &str,
        dir: &std::path::Path,
        agent: protocol::agent::AgentKind,
        lifecycle: Lifecycle,
    ) {
        let now = protocol::time::now_rfc3339();
        let row = SessionRow {
            session_uid: uid.into(),
            session_id: "cc-1".into(),
            cwd: dir.to_str().unwrap().into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle,
            created_at: now.clone(),
            updated_at: now,
            agent,
            codex_thread_id: None,
        };
        assert_eq!(
            daemon
                .store
                .begin_run(&row, &format!("seed-{uid}"), None, None)
                .unwrap(),
            None
        );
    }

    fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new(crate::git::git_bin(None).unwrap())
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn commit(message: &str) -> protocol::ws::GitAction {
        protocol::ws::GitAction::Commit {
            message: message.into(),
        }
    }

    fn refused(code: &str) -> protocol::ws::GitActionResult {
        protocol::ws::GitActionResult::Refused {
            code: code.into(),
            detail: None,
        }
    }

    fn committed_notices(daemon: &Arc<Daemon>, uid: &str) -> Vec<serde_json::Value> {
        daemon
            .store
            .events_after(uid, 0, 1000)
            .unwrap()
            .into_iter()
            .filter(|event| {
                event.payload["notification_type"] == protocol::event::notice::COMMITTED
            })
            .map(|event| event.payload)
            .collect()
    }

    /// **A commit is kept per request id**: it commits once and files one notice; the same
    /// id again answers the same commit and commits nothing more; the same id with another
    /// message is refused.
    #[tokio::test]
    async fn a_commit_is_made_once_per_request_and_files_one_notice() {
        let _env = crate::ProcessEnvRead::take();
        let daemon = test_daemon();
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP01";
        let dir = run_in_a_repository(&daemon, uid);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        let first = daemon.git_action(uid, "r-1", &commit("Add a and b")).await;
        let head = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        assert_eq!(
            first,
            protocol::ws::GitActionResult::Done {
                sha: Some(head.clone()),
                pr: None,
            }
        );
        assert_eq!(
            committed_notices(&daemon, uid),
            vec![serde_json::json!({
                "notification_type": "committed", "oid": head, "files": 2, "message": "Add a and b"
            })]
        );

        std::fs::write(dir.join("c.txt"), "c\n").unwrap();
        assert_eq!(
            daemon.git_action(uid, "r-1", &commit("Add a and b")).await,
            first
        );
        assert_eq!(
            git_in(&dir, &["rev-parse", "HEAD"]).trim(),
            head,
            "nothing committed again"
        );
        assert_eq!(committed_notices(&daemon, uid).len(), 1, "one notice");
        assert_eq!(
            daemon
                .git_action(uid, "r-1", &commit("Something else"))
                .await,
            refused(protocol::ws::git_refusal::BAD_REQUEST)
        );
        assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]).trim(), head);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A run that is working, or waits on a card, is not committed under**, and nothing is
    /// staged; an empty message, and a run this computer does not have, are refused.
    #[tokio::test]
    async fn a_commit_waits_for_a_quiet_run() {
        let _env = crate::ProcessEnvRead::take();
        use protocol::ws::git_refusal as code;
        let daemon = test_daemon();
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP02";
        let dir = run_in_a_repository(&daemon, uid);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();

        daemon
            .ingest(PendingEvent::new(
                &SessionKey::new(uid, "cc-1"),
                EventKind::SessionStart,
                serde_json::json!({}),
                Source::Daemon,
            ))
            .await
            .unwrap();
        assert_eq!(
            daemon.git_action(uid, "r-turn", &commit("m")).await,
            refused(code::BUSY)
        );
        assert_eq!(
            git_in(&dir, &["diff", "--cached", "--name-only"]),
            "",
            "nothing staged"
        );
        daemon
            .ingest(PendingEvent::new(
                &SessionKey::new(uid, "cc-1"),
                EventKind::TurnComplete,
                serde_json::json!({}),
                Source::Daemon,
            ))
            .await
            .unwrap();

        daemon
            .inner
            .lock()
            .await
            .pending
            .insert((uid.into(), "r-card".into()), pending_card(uid, "app"));
        assert_eq!(
            daemon.git_action(uid, "r-card", &commit("m")).await,
            refused(code::BUSY)
        );
        assert_eq!(git_in(&dir, &["diff", "--cached", "--name-only"]), "");
        daemon.inner.lock().await.pending.clear();

        assert_eq!(
            daemon.git_action(uid, "r-empty", &commit("  ")).await,
            refused(code::BAD_REQUEST)
        );
        assert_eq!(
            daemon
                .git_action("01K1B3XQ8ZC0DE5FGH7JKMNP99", "r-x", &commit("m"))
                .await,
            refused(code::UNKNOWN_SESSION)
        );
        assert!(matches!(
            daemon.git_action(uid, "r-quiet", &commit("m")).await,
            protocol::ws::GitActionResult::Done { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A Codex run's turn is asked of its live link**, which knows a resumed turn the log
    /// does not file; a Claude run that ended mid-turn is not working.
    #[tokio::test]
    async fn a_commit_asks_a_codex_link_and_passes_an_ended_run() {
        let _env = crate::ProcessEnvRead::take();
        use protocol::ws::git_refusal as code;
        let daemon = test_daemon();
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP04";
        let dir = run_in_a_repository_as(
            &daemon,
            uid,
            protocol::agent::AgentKind::Codex,
            Lifecycle::Live,
        );
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        assert_eq!(
            daemon.git_action(uid, "r-no-link", &commit("m")).await,
            refused(code::BUSY),
            "a live Codex run with no link yet cannot say it is quiet"
        );
        let (answers, _) = crate::codex_host::answer_channel();
        let (commands, mut asks) = tokio::sync::mpsc::unbounded_channel();
        daemon
            .host_link_attached(uid, HostLink::Codex(answers, commands))
            .await;
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let answering = Arc::clone(&running);
        tokio::spawn(async move {
            while let Some(ask) = asks.recv().await {
                if let crate::codex_host::LinkCommand::TurnRunning(reply) = ask {
                    let _ = reply.send(answering.load(std::sync::atomic::Ordering::SeqCst));
                }
            }
        });
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        assert_eq!(
            daemon.git_action(uid, "r-codex", &commit("m")).await,
            refused(code::BUSY)
        );
        running.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            daemon.git_action(uid, "r-codex-quiet", &commit("m")).await,
            protocol::ws::GitActionResult::Done { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let ended = "01K1B3XQ8ZC0DE5FGH7JKMNP05";
        let dir = run_in_a_repository_as(
            &daemon,
            ended,
            protocol::agent::AgentKind::Claude,
            Lifecycle::Exited,
        );
        daemon
            .ingest(PendingEvent::new(
                &SessionKey::new(ended, "cc-1"),
                EventKind::SessionStart,
                serde_json::json!({}),
                Source::Daemon,
            ))
            .await
            .unwrap();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        assert!(matches!(
            daemon.git_action(ended, "r-ended", &commit("m")).await,
            protocol::ws::GitActionResult::Done { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // Stopping is not ended: its turn may still be writing.
        let stopping = "01K1B3XQ8ZC0DE5FGH7JKMNP06";
        let dir = run_in_a_repository_as(
            &daemon,
            stopping,
            protocol::agent::AgentKind::Claude,
            Lifecycle::Stopping,
        );
        daemon
            .ingest(PendingEvent::new(
                &SessionKey::new(stopping, "cc-1"),
                EventKind::SessionStart,
                serde_json::json!({}),
                Source::Daemon,
            ))
            .await
            .unwrap();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        assert_eq!(
            daemon
                .git_action(stopping, "r-stopping", &commit("m"))
                .await,
            refused(code::BUSY)
        );
        let _ = std::fs::remove_dir_all(&dir);

        // An agent whose turns ccd cannot read counts as working until its run ends.
        for (uid, lifecycle, busy) in [
            ("01K1B3XQ8ZC0DE5FGH7JKMNP07", Lifecycle::Live, true),
            ("01K1B3XQ8ZC0DE5FGH7JKMNP08", Lifecycle::Exited, false),
        ] {
            let dir = run_in_a_repository_as(
                &daemon,
                uid,
                protocol::agent::AgentKind::Antigravity,
                lifecycle,
            );
            std::fs::write(dir.join("a.txt"), "a\n").unwrap();
            let answer = daemon.git_action(uid, "r-agy", &commit("m")).await;
            assert_eq!(answer == refused(code::BUSY), busy, "{answer:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    fn turn_started(daemon: &Arc<Daemon>, uid: &str, kind: EventKind) -> PendingEvent {
        let _ = daemon;
        PendingEvent::new(
            &SessionKey::new(uid, "cc-1"),
            kind,
            serde_json::json!({}),
            Source::Daemon,
        )
    }

    /// **Any run in the folder working holds a commit**: an ended run's commit waits for a
    /// live run in the same folder that is mid-turn; a run starting, and a Codex run
    /// stopping whatever its link says, may be writing too.
    #[tokio::test]
    async fn a_commit_waits_for_every_run_in_its_folder() {
        use protocol::agent::AgentKind;
        use protocol::ws::git_refusal as code;
        let _env = crate::ProcessEnvRead::take();
        let daemon = test_daemon();
        let ended = "01K1B3XQ8ZC0DE5FGH7JKMNP09";
        let working = "01K1B3XQ8ZC0DE5FGH7JKMNP10";
        let dir = run_in_a_repository_as(&daemon, ended, AgentKind::Claude, Lifecycle::Exited);
        seed_run_in(&daemon, working, &dir, AgentKind::Claude, Lifecycle::Live);
        daemon
            .ingest(turn_started(&daemon, working, EventKind::SessionStart))
            .await
            .unwrap();
        std::fs::write(dir.join("half.txt"), "half\n").unwrap();
        assert_eq!(
            daemon.git_action(ended, "r-1", &commit("m")).await,
            refused(code::BUSY)
        );
        daemon
            .ingest(turn_started(&daemon, working, EventKind::TurnComplete))
            .await
            .unwrap();
        assert!(matches!(
            daemon.git_action(ended, "r-2", &commit("m")).await,
            protocol::ws::GitActionResult::Done { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let starting = "01K1B3XQ8ZC0DE5FGH7JKMNP11";
        let dir = run_in_a_repository_as(&daemon, starting, AgentKind::Claude, Lifecycle::Starting);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        assert_eq!(
            daemon.git_action(starting, "r-3", &commit("m")).await,
            refused(code::BUSY)
        );
        let _ = std::fs::remove_dir_all(&dir);

        let stopping = "01K1B3XQ8ZC0DE5FGH7JKMNP12";
        let dir = run_in_a_repository_as(&daemon, stopping, AgentKind::Codex, Lifecycle::Stopping);
        let (answers, _) = crate::codex_host::answer_channel();
        let (commands, mut asks) = tokio::sync::mpsc::unbounded_channel();
        daemon
            .host_link_attached(stopping, HostLink::Codex(answers, commands))
            .await;
        tokio::spawn(async move {
            while let Some(ask) = asks.recv().await {
                if let crate::codex_host::LinkCommand::TurnRunning(reply) = ask {
                    let _ = reply.send(false);
                }
            }
        });
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        assert_eq!(
            daemon.git_action(stopping, "r-4", &commit("m")).await,
            refused(code::BUSY)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A commit whose run is removed while it runs says so**: git committed, but neither
    /// its answer nor its notice could be kept, so it is indeterminate, never done.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_commit_whose_run_is_removed_meanwhile_is_indeterminate() {
        use std::os::unix::fs::PermissionsExt;
        let _env = crate::ProcessEnvRead::take();
        let daemon = test_daemon();
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP13";
        let dir = run_in_a_repository_as(
            &daemon,
            uid,
            protocol::agent::AgentKind::Claude,
            Lifecycle::Exited,
        );
        let hook = dir.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nsleep 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        let before = git_in(&dir, &["rev-parse", "HEAD"]);
        let action = commit("m");
        let (answer, removed) = tokio::join!(daemon.git_action(uid, "r-gone", &action), async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            daemon.delete_exited_session(uid).await
        });
        assert!(removed.is_ok(), "{removed:?}");
        assert_ne!(
            git_in(&dir, &["rev-parse", "HEAD"]),
            before,
            "git committed"
        );
        assert!(
            matches!(&answer, protocol::ws::GitActionResult::Refused { code, .. }
                if code == protocol::ws::git_refusal::INDETERMINATE),
            "{answer:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stand-in `gh` in its own folder, answering `gh api` for the pulls from the files beside
    /// it, as gh 2.87.3 does: `open.json`/`closed.json` on stdout, `api.err` on stderr, exit
    /// `api.exit`; `pr create` from `create.out`/`create.err`/`create.exit`, writing the folder it
    /// ran in to `create.cwd`, after which an open
    /// lookup reads `open.after` when there is one; every call is logged to `calls`, after
    /// `prompt=` and its `GH_PROMPT_DISABLED`.
    /// Signed in by default: exit 0 and GitHub's own cli/cli answers (no credential captures
    /// gh's signed-in run; its body is that answer).
    fn stand_in_gh() -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "ccd-gh-{}-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let gh = dir.join("gh");
        std::fs::write(
            &gh,
            "#!/bin/sh\nd=$(dirname \"$0\")\necho \"prompt=${GH_PROMPT_DISABLED:-on} $*\" >> \"$d/calls\"\ncase \"$*\" in\n\
             \"pr merge\"*) touch \"$d/merged\"; cat \"$d/merge.err\" >&2; cat \"$d/merge.out\"; exit $(cat \"$d/merge.exit\");;\n\
             *\"/pulls/\"[0-9]*) if [ -f \"$d/merged\" ] && [ -f \"$d/pull.after\" ]; then cat \"$d/pull.after\"; else cat \"$d/pull.json\"; fi; exit 0;;\n\
             \"api graphql\"*) cat \"$d/api.err\" >&2; cat \"$d/checks.json\"; exit $(cat \"$d/api.exit\");;\n\
             \"pr create\"*) touch \"$d/created\"; pwd -P > \"$d/create.cwd\"; cat \"$d/create.err\" >&2; cat \"$d/create.out\"; exit $(cat \"$d/create.exit\");;\n\
             *state=open*) cat \"$d/api.err\" >&2; if [ -f \"$d/created\" ] && [ -f \"$d/open.after\" ]; then cat \"$d/open.after\"; else cat \"$d/open.json\"; fi; exit $(cat \"$d/api.exit\");;\n\
             *state=closed*) cat \"$d/api.err\" >&2; cat \"$d/closed.json\"; exit $(cat \"$d/api.exit\");;\n\
             esac\nexit 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fixtures = github_fixtures();
        std::fs::copy(fixtures.join("pulls-open.json"), dir.join("open.json")).unwrap();
        std::fs::copy(fixtures.join("pulls-closed.json"), dir.join("closed.json")).unwrap();
        std::fs::copy(
            fixtures.join("checks-graphql.json"),
            dir.join("checks.json"),
        )
        .unwrap();
        std::fs::write(dir.join("api.err"), "").unwrap();
        std::fs::write(dir.join("api.exit"), "0").unwrap();
        for (name, text) in [
            ("create.out", ""),
            ("create.err", ""),
            ("create.exit", "0"),
            ("merge.out", ""),
            ("merge.err", ""),
            ("merge.exit", "0"),
        ] {
            std::fs::write(dir.join(name), text).unwrap();
        }
        gh
    }

    fn github_fixtures() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/github")
    }

    /// The stand-in answers the pulls calls as gh did in the capture `name` (`*.stderr`, `*.exit`).
    fn gh_answers_as(gh: &std::path::Path, name: &str) {
        let fixtures = github_fixtures();
        std::fs::copy(
            fixtures.join(format!("{name}.stderr")),
            gh.with_file_name("api.err"),
        )
        .unwrap();
        std::fs::copy(
            fixtures.join(format!("{name}.exit")),
            gh.with_file_name("api.exit"),
        )
        .unwrap();
    }

    fn rand_suffix() -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn gh_calls(gh: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(gh.with_file_name("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// **A run's GitHub state, read through gh at most once per window**: signed out and offline
    /// (gh's own captures), signed in with the branch's open PR (GitHub's own answer for cli/cli),
    /// reads asked together and inside the window sharing one call, a new commit asking again, a
    /// push URL naming the PR's owner, a rate limit stopping the host's reads, no gh at all, and
    /// remotes that name no github.com repository.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_runs_github_state_is_read_through_gh_once_per_window() {
        use protocol::agent::AgentKind;
        use protocol::ws::GhState;
        let _env = crate::ProcessEnvRead::take();
        let branch = "dependabot/go_modules/github.com/sigstore/sigstore-go-1.4.1";
        let at = |gh: &std::path::Path, uid: &str, branch: &str| {
            let daemon = daemon_on(
                shared_store(),
                Config {
                    gh_bin: Some(gh.to_str().unwrap().into()),
                    ..Config::default()
                },
            );
            let dir = run_in_a_repository_as(&daemon, uid, AgentKind::Claude, Lifecycle::Live);
            git_in(
                &dir,
                &["remote", "add", "origin", "https://github.com/cli/cli.git"],
            );
            git_in(&dir, &["switch", "-q", "-c", branch]);
            (daemon, dir)
        };
        let done = |gh: &std::path::Path, dir: &std::path::Path| {
            let _ = std::fs::remove_dir_all(dir);
            let _ = std::fs::remove_dir_all(gh.parent().unwrap());
        };

        // gh signed out stops before asking GitHub; offline and a refused token did ask it.
        for (capture, state, asked) in [
            ("api-signed-out", GhState::SignedOut, false),
            ("api-offline", GhState::Unknown, true),
            ("api-token-refused", GhState::SignedOut, true),
        ] {
            let gh = stand_in_gh();
            gh_answers_as(&gh, capture);
            let (daemon, dir) = at(&gh, "01K1B3XQ8ZC0DE5FGH7JKMNP20", branch);
            let read = daemon
                .run_git("01K1B3XQ8ZC0DE5FGH7JKMNP20")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    read.gh,
                    read.gh_host.as_deref(),
                    read.pr,
                    read.gh_read_at.is_some()
                ),
                (Some(state), Some("github.com"), None, asked),
                "{capture}"
            );
            done(&gh, &dir);
        }

        let gh = stand_in_gh();
        let (daemon, dir) = at(&gh, "01K1B3XQ8ZC0DE5FGH7JKMNP21", branch);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP21";
        let (one, two) = tokio::join!(daemon.run_git(uid), daemon.run_git(uid));
        let read = one.unwrap().unwrap();
        assert_eq!(read.gh, Some(GhState::SignedIn));
        assert_eq!(read.pr.as_ref().map(|pr| pr.number), Some(15173));
        assert_eq!(two.unwrap().unwrap().pr, read.pr);
        let pr = read.pr.as_ref().unwrap();
        assert_eq!(
            (pr.checks.as_ref().map(Vec::len), pr.checks_complete),
            (Some(10), Some(true)),
            "an open pull request's checks, from the checks query"
        );
        let calls = gh_calls(&gh);
        assert_eq!(
            calls.len(),
            2,
            "two reads at once: one open pulls call, one checks query: {calls:?}"
        );
        assert!(
            calls[1].contains("api graphql") && calls[1].contains("number=15173"),
            "{calls:?}"
        );
        assert!(
            calls[0].starts_with("prompt=1 "),
            "gh never prompts: {calls:?}"
        );
        assert!(
            calls[0].contains("repos/cli/cli/pulls")
                && calls[0].contains(&format!("head=cli:{branch}")),
            "{calls:?}"
        );
        let again = daemon.run_git(uid).await.unwrap().unwrap();
        assert_eq!(
            (again.pr, again.gh_read_at),
            (read.pr.clone(), read.gh_read_at.clone())
        );
        assert_eq!(gh_calls(&gh).len(), 2, "inside the window gh is not asked");
        git_in(
            &dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "next",
            ],
        );
        daemon.run_git(uid).await.unwrap().unwrap();
        assert_eq!(gh_calls(&gh).len(), 4, "a new commit is read again");
        done(&gh, &dir);

        let gh = stand_in_gh();
        let fork = "fix/issue-14946-existing-linked-branch";
        let (daemon, dir) = at(&gh, "01K1B3XQ8ZC0DE5FGH7JKMNP24", fork);
        git_in(
            &dir,
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                "https://github.com/lazerg/cli-2.git",
            ],
        );
        let read = daemon
            .run_git("01K1B3XQ8ZC0DE5FGH7JKMNP24")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.pr.as_ref().map(|pr| pr.number), Some(15111));
        let calls = gh_calls(&gh);
        assert!(
            calls[0].contains("repos/cli/cli/pulls")
                && calls[0].contains(&format!("head=lazerg:{fork}")),
            "the PR's owner is the push URL's: {calls:?}"
        );
        done(&gh, &dir);

        // A fork clone with its parent added as `upstream`, never fetched: no `upstream/HEAD`.
        let gh = stand_in_gh();
        let (daemon, dir) = at(&gh, "01K1B3XQ8ZC0DE5FGH7JKMNP25", fork);
        git_in(
            &dir,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/lazerg/cli-2.git",
            ],
        );
        git_in(
            &dir,
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/cli/cli.git",
            ],
        );
        let read = daemon
            .run_git("01K1B3XQ8ZC0DE5FGH7JKMNP25")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.pr.as_ref().map(|pr| pr.number), Some(15111));
        let calls = gh_calls(&gh);
        assert!(
            calls[0].contains("repos/cli/cli/pulls")
                && calls[0].contains(&format!("head=lazerg:{fork}")),
            "the PR is looked for on the parent: {calls:?}"
        );
        done(&gh, &dir);

        // `gh pr checkout` of a fork's PR: the branch's remote is the fork's URL, not a remote's name,
        // in a clone of the parent (which has `origin/HEAD`).
        let gh = stand_in_gh();
        let (daemon, dir) = at(&gh, "01K1B3XQ8ZC0DE5FGH7JKMNP26", fork);
        git_in(&dir, &["update-ref", "refs/remotes/origin/trunk", "HEAD"]);
        git_in(
            &dir,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/trunk",
            ],
        );
        git_in(
            &dir,
            &[
                "config",
                &format!("branch.{fork}.remote"),
                "https://github.com/lazerg/cli-2.git",
            ],
        );
        git_in(
            &dir,
            &[
                "config",
                &format!("branch.{fork}.merge"),
                &format!("refs/heads/{fork}"),
            ],
        );
        let read = daemon
            .run_git("01K1B3XQ8ZC0DE5FGH7JKMNP26")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            read.pr.as_ref().map(|pr| pr.number),
            Some(15111),
            "{:?}",
            read.gh
        );
        let calls = gh_calls(&gh);
        assert!(
            calls[0].contains("repos/cli/cli/pulls"),
            "the PR is looked for on the parent: {calls:?}"
        );
        assert!(
            calls[0].contains(&format!("head=lazerg:{fork}")),
            "the fork's owner: {calls:?}"
        );
        done(&gh, &dir);

        let gh = stand_in_gh();
        std::fs::write(
            gh.with_file_name("api.err"),
            "gh: API rate limit exceeded for 203.0.113.9. (HTTP 403)\n",
        )
        .unwrap();
        std::fs::write(gh.with_file_name("api.exit"), "1").unwrap();
        let (daemon, dir) = at(&gh, "01K1B3XQ8ZC0DE5FGH7JKMNP22", branch);
        let read = daemon
            .run_git("01K1B3XQ8ZC0DE5FGH7JKMNP22")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((read.gh, read.pr), (Some(GhState::Unknown), None));
        git_in(&dir, &["switch", "-q", "-c", "another"]);
        let calls = gh_calls(&gh).len();
        let paused = daemon
            .run_git("01K1B3XQ8ZC0DE5FGH7JKMNP22")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (paused.gh, paused.gh_read_at),
            (Some(GhState::Unknown), None),
            "GitHub is not asked"
        );
        assert_eq!(gh_calls(&gh).len(), calls, "the host's reads stopped");
        done(&gh, &dir);

        let gone = gh.with_file_name("no-gh-here");
        let (daemon, dir) = at(&gone, "01K1B3XQ8ZC0DE5FGH7JKMNP23", branch);
        let read = daemon
            .run_git("01K1B3XQ8ZC0DE5FGH7JKMNP23")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (read.gh, read.gh_read_at),
            (Some(GhState::Missing), None),
            "GitHub is not asked"
        );
        let local = daemon_on(Arc::clone(&daemon.store), Config::default());
        for url in ["/srv/git/app.git", "https://gitlab.com/team/app.git"] {
            git_in(&dir, &["remote", "set-url", "origin", url]);
            let read = local
                .run_git("01K1B3XQ8ZC0DE5FGH7JKMNP23")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (read.gh, read.gh_host),
                (Some(GhState::NoGithubRemote), None),
                "{url}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run on `branch` (one commit past the first) whose `origin` is cli/cli over SSH, its
    /// pushes landing in a bare repository beside it through `core.sshCommand`: nothing leaves
    /// the computer, and git still names the GitHub repository. Returns the run's folder and
    /// the bare repository.
    fn run_pushing_to_a_bare(
        daemon: &Arc<Daemon>,
        uid: &str,
        branch: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = run_in_a_repository_as(
            daemon,
            uid,
            protocol::agent::AgentKind::Claude,
            Lifecycle::Exited,
        );
        let bare = dir.with_extension("bare");
        let git = crate::git::git_bin(None).unwrap();
        let made = std::process::Command::new(&git)
            .args(["init", "-q", "--bare"])
            .arg(&bare)
            .status()
            .unwrap();
        assert!(made.success());
        let ssh = dir.with_extension("ssh");
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nexec '{}' receive-pack '{}'\n",
                git.display(),
                bare.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        git_in(
            &dir,
            &[
                "remote",
                "add",
                "origin",
                "ssh://git@github.com/cli/cli.git",
            ],
        );
        git_in(&dir, &["config", "core.sshCommand", ssh.to_str().unwrap()]);
        // As a clone has it: `origin/HEAD` names `main`, at the first commit.
        git_in(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git_in(
            &dir,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        git_in(&dir, &["switch", "-q", "-c", branch]);
        git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "Second"]);
        (dir, bare)
    }

    fn pr(number: u64) -> protocol::ws::GitActionResult {
        protocol::ws::GitActionResult::Done {
            sha: None,
            pr: Some(protocol::ws::PrLink {
                number,
                url: format!("https://github.com/cli/cli/pull/{number}"),
            }),
        }
    }

    fn refused_as(result: &protocol::ws::GitActionResult) -> (String, Option<String>) {
        match result {
            protocol::ws::GitActionResult::Refused { code, detail } => {
                (code.clone(), detail.clone())
            }
            done => panic!("not refused: {done:?}"),
        }
    }

    /// **A pull request is titled by its run**: a run with a title opens its pull request with it,
    /// the body still gh's own from the branch's commits.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pull_request_is_titled_by_its_run() {
        let _env = crate::ProcessEnvRead::take();
        let gh = stand_in_gh();
        std::fs::write(
            gh.with_file_name("create.out"),
            "https://github.com/cli/cli/pull/15210\n",
        )
        .unwrap();
        let daemon = daemon_on(
            shared_store(),
            Config {
                gh_bin: Some(gh.to_str().unwrap().into()),
                ..Config::default()
            },
        );
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP3T";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, "w2-titled");
        daemon
            .store
            .append_event(&crate::claude_host::person_row(
                uid,
                "u-1",
                "Retry the router on 5xx\nand keep the tests green",
                None,
                None,
            ))
            .unwrap();
        assert_eq!(
            daemon
                .git_action(uid, "p-1", &protocol::ws::GitAction::CreatePr)
                .await,
            pr(15210)
        );
        let calls = gh_calls(&gh);
        assert!(
            calls.iter().any(|call| call.ends_with(
                "pr create --repo cli/cli --head w2-titled --base main --fill --title=Retry the router on 5xx"
            )),
            "{calls:?}"
        );
        for path in [dir.clone(), bare, gh.parent().unwrap().to_path_buf()] {
            let _ = std::fs::remove_dir_all(path);
        }
        let _ = std::fs::remove_file(dir.with_extension("ssh"));
    }

    /// **A pull request is opened once, and never twice**: pushed then opened with gh (and the
    /// same request answered again without a call), an open one returned without pushing, gh's
    /// refusal after the push, a lost "already exists" race answered from the lookup, signed out
    /// and a refusing `pre-push` hook stopping before anything is sent, the base branch, a branch
    /// with nothing to propose, and a remote that names no GitHub repository.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pull_request_is_opened_once_and_never_twice() {
        use protocol::ws::git_refusal as code;
        let _env = crate::ProcessEnvRead::take();
        let create = protocol::ws::GitAction::CreatePr;
        let on = |gh: &std::path::Path| {
            daemon_on(
                shared_store(),
                Config {
                    gh_bin: Some(gh.to_str().unwrap().into()),
                    ..Config::default()
                },
            )
        };
        let done = |gh: &std::path::Path, dir: &std::path::Path, bare: &std::path::Path| {
            for path in [
                dir.to_path_buf(),
                bare.to_path_buf(),
                gh.parent().unwrap().to_path_buf(),
            ] {
                let _ = std::fs::remove_dir_all(path);
            }
            let _ = std::fs::remove_file(dir.with_extension("ssh"));
        };
        let refs = |bare: &std::path::Path| {
            git_in(bare, &["for-each-ref", "--format=%(refname) %(objectname)"])
        };

        // Read first (cached), pushed, then opened in the run's folder; the same request is
        // answered again without a call; and the next read asks GitHub again.
        let gh = stand_in_gh();
        std::fs::write(
            gh.with_file_name("create.out"),
            "https://github.com/cli/cli/pull/15200\n",
        )
        .unwrap();
        let daemon = on(&gh);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP30";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, "w2-feature");
        daemon.run_git(uid).await.unwrap().unwrap();
        let read = gh_calls(&gh).len();
        assert_eq!(
            read, 2,
            "open, then closed: the stand-in's open list lacks it"
        );
        assert_eq!(daemon.git_action(uid, "p-1", &create).await, pr(15200));
        let head = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        assert_eq!(refs(&bare).trim(), format!("refs/heads/w2-feature {head}"));
        assert_eq!(
            git_in(
                &dir,
                &["rev-parse", "--abbrev-ref", "w2-feature@{upstream}"]
            )
            .trim(),
            "origin/w2-feature"
        );
        let calls = gh_calls(&gh);
        assert_eq!(calls.len(), read + 2, "{calls:?}");
        assert!(
            calls[read].contains("repos/cli/cli/pulls")
                && calls[read].contains("head=cli:w2-feature"),
            "{calls:?}"
        );
        assert!(
            calls[read + 1]
                .ends_with("pr create --repo cli/cli --head w2-feature --base main --fill"),
            "{calls:?}"
        );
        assert_eq!(
            std::fs::read_to_string(gh.with_file_name("create.cwd"))
                .unwrap()
                .trim(),
            dir.canonicalize().unwrap().to_str().unwrap(),
            "gh reads the branch's commits in the run's folder"
        );
        assert_eq!(daemon.git_action(uid, "p-1", &create).await, pr(15200));
        assert_eq!(gh_calls(&gh).len(), read + 2, "a request is answered once");
        daemon.run_git(uid).await.unwrap().unwrap();
        assert_eq!(
            gh_calls(&gh).len(),
            read + 4,
            "after it, the branch's GitHub read is asked again, not answered from before it"
        );
        done(&gh, &dir, &bare);

        // The branch there moved on since this clone last fetched: the push is refused, and
        // no pull request is opened for a commit the panel does not show.
        let gh = stand_in_gh();
        let daemon = on(&gh);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP37";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, "w2-moved");
        git_in(&dir, &["push", "-q", "-u", "origin", "w2-moved"]);
        git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "Elsewhere"]);
        git_in(&dir, &["push", "-q", "origin", "HEAD:refs/heads/w2-moved"]);
        git_in(&dir, &["reset", "-q", "--hard", "HEAD~1"]);
        git_in(
            &dir,
            &["update-ref", "refs/remotes/origin/w2-moved", "HEAD"],
        );
        let (refused, reason) = refused_as(&daemon.git_action(uid, "p-10", &create).await);
        assert_eq!(refused, code::GIT_FAILED);
        assert!(
            reason
                .as_deref()
                .is_some_and(|line| line.starts_with("! [rejected]")),
            "git's reason, not the line naming the remote: {reason:?}"
        );
        assert_eq!(
            gh_calls(&gh).len(),
            1,
            "only the lookup: {:?}",
            gh_calls(&gh)
        );
        done(&gh, &dir, &bare);

        // An open pull request from the branch: returned, nothing pushed, nothing created.
        let gh = stand_in_gh();
        let daemon = on(&gh);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP31";
        let branch = "dependabot/go_modules/github.com/sigstore/sigstore-go-1.4.1";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, branch);
        assert_eq!(daemon.git_action(uid, "p-2", &create).await, pr(15173));
        assert_eq!(refs(&bare), "");
        assert_eq!(gh_calls(&gh).len(), 1);
        done(&gh, &dir, &bare);

        // gh refuses after the push: said so, and the branch stays pushed.
        let gh = stand_in_gh();
        // gh's words for a refused create, written from its source.
        std::fs::write(
            gh.with_file_name("create.err"),
            "pull request create failed: GraphQL: Head sha can't be blank (createPullRequest)\n",
        )
        .unwrap();
        std::fs::write(gh.with_file_name("create.exit"), "1").unwrap();
        let daemon = on(&gh);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP32";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, "w2-refused");
        assert_eq!(
            refused_as(&daemon.git_action(uid, "p-3", &create).await),
            (
                code::CREATE_FAILED.to_string(),
                Some("pushed; pull request create failed: GraphQL: Head sha can't be blank (createPullRequest)".into())
            )
        );
        assert!(refs(&bare).contains("refs/heads/w2-refused"));
        assert_eq!(gh_calls(&gh).len(), 3, "lookup, create, the lookup after");
        done(&gh, &dir, &bare);

        // "Already exists" (a race): the lookup after finds it, and it is the answer.
        let gh = stand_in_gh();
        // gh's words when one is already open, written from its source.
        std::fs::write(
            gh.with_file_name("create.err"),
            "a pull request for branch \"w2-race\" into branch \"trunk\" already exists:\nhttps://github.com/cli/cli/pull/15300\n",
        )
        .unwrap();
        std::fs::write(gh.with_file_name("create.exit"), "1").unwrap();
        std::fs::write(
            gh.with_file_name("open.after"),
            r#"[{"number": 15300, "state": "open", "html_url": "https://github.com/cli/cli/pull/15300", "merged_at": null,
                "base": {"ref": "trunk"}, "head": {"ref": "w2-race", "sha": "0", "repo": {"full_name": "cli/cli"}}}]"#,
        )
        .unwrap();
        let daemon = on(&gh);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP33";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, "w2-race");
        assert_eq!(daemon.git_action(uid, "p-4", &create).await, pr(15300));
        done(&gh, &dir, &bare);

        // Signed out (gh's own capture): nothing pushed, nothing created.
        let gh = stand_in_gh();
        gh_answers_as(&gh, "api-signed-out");
        let daemon = on(&gh);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP34";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, "w2-out");
        assert_eq!(
            refused_as(&daemon.git_action(uid, "p-5", &create).await).0,
            code::GH_SIGNED_OUT
        );
        assert_eq!(refs(&bare), "");
        done(&gh, &dir, &bare);

        // The repository's pre-push hook refuses: its words, and gh is not asked to create.
        let gh = stand_in_gh();
        let daemon = on(&gh);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP35";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, "w2-hook");
        let hook = dir.join(".git/hooks/pre-push");
        std::fs::write(&hook, "#!/bin/sh\necho 'the tests failed' >&2\nexit 1\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            refused_as(&daemon.git_action(uid, "p-6", &create).await),
            (
                code::GIT_FAILED.to_string(),
                Some("the tests failed".into())
            )
        );
        assert_eq!(refs(&bare), "");
        assert_eq!(gh_calls(&gh).len(), 1, "only the lookup");
        done(&gh, &dir, &bare);

        // A triangle (`remote.pushDefault=fork`, the parent as `origin`): after `push -u fork`
        // the branch tracks the fork, and the pull request is still looked for on the parent.
        let gh = stand_in_gh();
        std::fs::write(
            gh.with_file_name("create.out"),
            "https://github.com/cli/cli/pull/15400\n",
        )
        .unwrap();
        let daemon = on(&gh);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP40";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, "w2-tri");
        git_in(
            &dir,
            &[
                "remote",
                "add",
                "fork",
                "ssh://git@github.com/lazerg/cli-2.git",
            ],
        );
        git_in(&dir, &["update-ref", "refs/remotes/fork/main", "main"]);
        git_in(
            &dir,
            &[
                "symbolic-ref",
                "refs/remotes/fork/HEAD",
                "refs/remotes/fork/main",
            ],
        );
        git_in(&dir, &["config", "remote.pushDefault", "fork"]);
        assert_eq!(daemon.git_action(uid, "p-13", &create).await, pr(15400));
        let calls = gh_calls(&gh);
        assert!(
            calls[1].contains("pr create --repo cli/cli --head lazerg:w2-tri"),
            "{calls:?}"
        );
        daemon.run_git(uid).await.unwrap().unwrap();
        let read = gh_calls(&gh);
        assert!(
            read[2].contains("repos/cli/cli/pulls") && read[2].contains("head=lazerg:w2-tri"),
            "the parent, after the push made the fork the branch's upstream: {read:?}"
        );
        done(&gh, &dir, &bare);

        // No base known (a repository made by `git init` and pushed has no `origin/HEAD`):
        // nothing says this is not the default branch, so nothing is pushed or asked.
        let gh = stand_in_gh();
        let daemon = on(&gh);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP38";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, "w2-nobase");
        git_in(
            &dir,
            &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
        );
        assert_eq!(
            refused_as(&daemon.git_action(uid, "p-11", &create).await).0,
            code::BASE_UNKNOWN
        );
        assert_eq!(refs(&bare), "");
        assert!(gh_calls(&gh).is_empty(), "{:?}", gh_calls(&gh));
        done(&gh, &dir, &bare);

        // gh fails, and GitHub cannot be asked again (its answer is not a pulls list): not known
        // whether it opened one, so never said to have failed.
        let gh = stand_in_gh();
        std::fs::write(
            gh.with_file_name("create.err"),
            "error connecting to api.github.com\n",
        )
        .unwrap();
        std::fs::write(gh.with_file_name("create.exit"), "1").unwrap();
        std::fs::write(gh.with_file_name("open.after"), "not a pulls list").unwrap();
        let daemon = on(&gh);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP39";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, "w2-lost");
        assert_eq!(
            refused_as(&daemon.git_action(uid, "p-12", &create).await).0,
            code::INDETERMINATE
        );
        done(&gh, &dir, &bare);

        // The base branch itself, then a branch with nothing past it, then no GitHub remote.
        let gh = stand_in_gh();
        let daemon = on(&gh);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP36";
        let (dir, bare) = run_pushing_to_a_bare(&daemon, uid, "w2-base");
        git_in(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git_in(
            &dir,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        git_in(&dir, &["switch", "-q", "main"]);
        git_in(&dir, &["reset", "-q", "--hard", "w2-base"]);
        assert_eq!(
            refused_as(&daemon.git_action(uid, "p-7", &create).await).0,
            code::ON_BASE_BRANCH
        );
        git_in(&dir, &["switch", "-q", "-c", "w2-level"]);
        assert_eq!(
            refused_as(&daemon.git_action(uid, "p-8", &create).await).0,
            code::NOTHING_TO_PROPOSE
        );
        git_in(&dir, &["remote", "set-url", "origin", "/srv/git/app.git"]);
        git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "Third"]);
        assert_eq!(
            refused_as(&daemon.git_action(uid, "p-9", &create).await).0,
            code::NO_GITHUB_REMOTE
        );
        assert!(gh_calls(&gh).is_empty(), "{:?}", gh_calls(&gh));
        done(&gh, &dir, &bare);
    }

    /// **A pull request is merged only as the panel showed it**: by a method its repository
    /// allows, at the head shown, then read again (merged, or held open by a merge queue); the
    /// same request answered again without a call; and refused, with nothing merged, when there
    /// is no pull request, its head moved before or as GitHub merges, the method is not allowed,
    /// or GitHub refuses. Never deleting a branch, never `--auto`, never `--admin`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pull_request_is_merged_only_as_the_panel_showed_it() {
        use protocol::agent::AgentKind;
        use protocol::ws::{git_refusal as code, GitAction, GitActionResult, MergeMethod, PrLink};
        let _env = crate::ProcessEnvRead::take();
        let branch = "dependabot/go_modules/github.com/sigstore/sigstore-go-1.4.1";
        let head = "8a93995195c4e5a299e227a00f236bf88199674a";
        let open: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../fixtures/github/pulls-open.json")).unwrap();
        let mut merged = open[0].clone();
        merged["state"] = "closed".into();
        merged["merged_at"] = "2026-11-27T10:00:00Z".into();
        let at = |uid: &str, branch: &str| {
            let gh = stand_in_gh();
            std::fs::write(gh.with_file_name("pull.json"), open[0].to_string()).unwrap();
            let daemon = daemon_on(
                shared_store(),
                Config {
                    gh_bin: Some(gh.to_str().unwrap().into()),
                    ..Config::default()
                },
            );
            let dir = run_in_a_repository_as(&daemon, uid, AgentKind::Claude, Lifecycle::Exited);
            git_in(
                &dir,
                &["remote", "add", "origin", "https://github.com/cli/cli.git"],
            );
            git_in(&dir, &["switch", "-q", "-c", branch]);
            (gh, daemon, dir)
        };
        let done = |gh: &std::path::Path, dir: &std::path::Path| {
            let _ = std::fs::remove_dir_all(dir);
            let _ = std::fs::remove_dir_all(gh.parent().unwrap());
        };
        let merge = |method| GitAction::Merge {
            method,
            head_oid: head.into(),
            number: 15173,
        };
        let merged_pr = GitActionResult::Done {
            sha: None,
            pr: Some(PrLink {
                number: 15173,
                url: "https://github.com/cli/cli/pull/15173".into(),
            }),
        };
        let refused = |result: GitActionResult| match result {
            GitActionResult::Refused { code, detail } => (code, detail),
            done => panic!("not refused: {done:?}"),
        };

        // Merged by squash at the head shown, then read again; the same request answers again.
        let (gh, daemon, dir) = at("01K1B3XQ8ZC0DE5FGH7JKMNP50", branch);
        std::fs::write(gh.with_file_name("pull.after"), merged.to_string()).unwrap();
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP50";
        assert_eq!(
            daemon
                .git_action(uid, "m-1", &merge(MergeMethod::Squash))
                .await,
            merged_pr
        );
        let calls = gh_calls(&gh);
        assert_eq!(
            calls.len(),
            4,
            "the open lookup, its details, the merge, the read after: {calls:?}"
        );
        assert!(
            calls[2].contains(&format!(
                "pr merge 15173 --repo cli/cli --squash --match-head-commit {head}"
            )),
            "{calls:?}"
        );
        for never in ["--delete-branch", "--auto", "--admin", "-d "] {
            assert!(!calls[2].contains(never), "{never}: {calls:?}");
        }
        assert!(calls[3].contains("repos/cli/cli/pulls/15173"), "{calls:?}");
        assert_eq!(
            daemon
                .git_action(uid, "m-1", &merge(MergeMethod::Squash))
                .await,
            merged_pr
        );
        assert_eq!(gh_calls(&gh).len(), 4, "a request is answered once");
        done(&gh, &dir);

        // A merge queue took it: done, and the pull request still open when read.
        let (gh, daemon, dir) = at("01K1B3XQ8ZC0DE5FGH7JKMNP51", branch);
        assert_eq!(
            daemon
                .git_action(
                    "01K1B3XQ8ZC0DE5FGH7JKMNP51",
                    "m-2",
                    &merge(MergeMethod::Merge)
                )
                .await,
            merged_pr
        );
        done(&gh, &dir);

        // Refused before any merge: the method not allowed (rebase), the head moved, no PR.
        let (gh, daemon, dir) = at("01K1B3XQ8ZC0DE5FGH7JKMNP52", branch);
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP52";
        assert_eq!(
            refused(
                daemon
                    .git_action(uid, "m-3", &merge(MergeMethod::Rebase))
                    .await
            )
            .0,
            code::METHOD_NOT_ALLOWED
        );
        let moved = GitAction::Merge {
            method: MergeMethod::Squash,
            head_oid: "0000000000000000000000000000000000000000".into(),
            number: 15173,
        };
        // The panel showed another pull request from this branch (closed since, this one opened).
        let another = GitAction::Merge {
            method: MergeMethod::Squash,
            head_oid: head.into(),
            number: 41,
        };
        assert_eq!(
            refused(daemon.git_action(uid, "m-8", &another).await).0,
            code::HEAD_MOVED
        );
        assert_eq!(
            refused(daemon.git_action(uid, "m-4", &moved).await).0,
            code::HEAD_MOVED
        );
        assert!(
            !gh_calls(&gh).iter().any(|call| call.contains("pr merge")),
            "{:?}",
            gh_calls(&gh)
        );
        done(&gh, &dir);
        let (gh, daemon, dir) = at("01K1B3XQ8ZC0DE5FGH7JKMNP53", "w2-no-pr");
        assert_eq!(
            refused(
                daemon
                    .git_action(
                        "01K1B3XQ8ZC0DE5FGH7JKMNP53",
                        "m-5",
                        &merge(MergeMethod::Squash)
                    )
                    .await
            )
            .0,
            code::NO_PULL_REQUEST
        );
        done(&gh, &dir);

        // GitHub refuses (gh's words, written from its source): the head moved as it merged, a rule.
        for (err, want) in [
            (
                "GraphQL: Head branch was modified. Review and try the merge again. (mergePullRequest)",
                (code::HEAD_MOVED.to_string(), None),
            ),
            (
                "GraphQL: Base branch was modified. Review and try the merge again. (mergePullRequest)\nmore",
                (
                    code::MERGE_FAILED.to_string(),
                    Some("GraphQL: Base branch was modified. Review and try the merge again. (mergePullRequest)".to_string()),
                ),
            ),
        ] {
            let (gh, daemon, dir) = at("01K1B3XQ8ZC0DE5FGH7JKMNP54", branch);
            std::fs::write(gh.with_file_name("merge.err"), format!("{err}\n")).unwrap();
            std::fs::write(gh.with_file_name("merge.exit"), "1").unwrap();
            assert_eq!(
                refused(daemon.git_action("01K1B3XQ8ZC0DE5FGH7JKMNP54", "m-6", &merge(MergeMethod::Squash)).await),
                want
            );
            done(&gh, &dir);
        }

        // On a queue-protected base, gh warns before it tries ("! The merge strategy…"); the
        // refusal shown is GitHub's, after the warning.
        let (gh, daemon, dir) = at("01K1B3XQ8ZC0DE5FGH7JKMNP57", branch);
        std::fs::write(
            gh.with_file_name("merge.err"),
            "! The merge strategy for trunk is set by the merge queue\nGraphQL: Resource not accessible by integration (enablePullRequestAutoMerge)\n",
        )
        .unwrap();
        std::fs::write(gh.with_file_name("merge.exit"), "1").unwrap();
        assert_eq!(
            refused(
                daemon
                    .git_action(
                        "01K1B3XQ8ZC0DE5FGH7JKMNP57",
                        "m-10",
                        &merge(MergeMethod::Squash)
                    )
                    .await
            ),
            (
                code::MERGE_FAILED.to_string(),
                Some(
                    "GraphQL: Resource not accessible by integration (enablePullRequestAutoMerge)"
                        .to_string()
                )
            )
        );
        done(&gh, &dir);

        // gh's answer lost, and GitHub not readable after: not known, never said to have failed.
        let (gh, daemon, dir) = at("01K1B3XQ8ZC0DE5FGH7JKMNP56", branch);
        std::fs::write(
            gh.with_file_name("merge.err"),
            "error connecting to api.github.com\n",
        )
        .unwrap();
        std::fs::write(gh.with_file_name("merge.exit"), "1").unwrap();
        std::fs::write(gh.with_file_name("pull.after"), "not a pull request").unwrap();
        assert_eq!(
            refused(
                daemon
                    .git_action(
                        "01K1B3XQ8ZC0DE5FGH7JKMNP56",
                        "m-9",
                        &merge(MergeMethod::Squash)
                    )
                    .await
            )
            .0,
            code::INDETERMINATE
        );
        done(&gh, &dir);

        // gh's answer lost, but GitHub merged it: done, from the read after.
        let (gh, daemon, dir) = at("01K1B3XQ8ZC0DE5FGH7JKMNP55", branch);
        std::fs::write(
            gh.with_file_name("merge.err"),
            "error connecting to api.github.com\n",
        )
        .unwrap();
        std::fs::write(gh.with_file_name("merge.exit"), "1").unwrap();
        std::fs::write(gh.with_file_name("pull.after"), merged.to_string()).unwrap();
        assert_eq!(
            daemon
                .git_action(
                    "01K1B3XQ8ZC0DE5FGH7JKMNP55",
                    "m-7",
                    &merge(MergeMethod::Squash)
                )
                .await,
            merged_pr,
            "the read after: {:?}",
            gh_calls(&gh)
        );
        done(&gh, &dir);
    }

    /// **A merged pull request carries no checks, and they are not asked for**: only an open
    /// one's are read.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_merged_pull_request_is_read_without_its_checks() {
        use protocol::agent::AgentKind;
        use protocol::ws::PrState;
        let _env = crate::ProcessEnvRead::take();
        let gh = stand_in_gh();
        let daemon = daemon_on(
            shared_store(),
            Config {
                gh_bin: Some(gh.to_str().unwrap().into()),
                ..Config::default()
            },
        );
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP41";
        let dir = run_in_a_repository_as(&daemon, uid, AgentKind::Claude, Lifecycle::Exited);
        git_in(
            &dir,
            &["remote", "add", "origin", "https://github.com/cli/cli.git"],
        );
        git_in(&dir, &["switch", "-q", "-c", "w2-done"]);
        let head = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        std::fs::write(gh.with_file_name("open.json"), "[]").unwrap();
        std::fs::write(
            gh.with_file_name("closed.json"),
            format!(
                r#"[{{"number": 15500, "state": "closed", "html_url": "https://github.com/cli/cli/pull/15500",
                    "merged_at": "2026-11-26T10:00:00Z", "base": {{"ref": "trunk"}},
                    "head": {{"ref": "w2-done", "sha": "{head}", "repo": {{"full_name": "cli/cli"}}}}}}]"#
            ),
        )
        .unwrap();
        let read = daemon.run_git(uid).await.unwrap().unwrap();
        let pr = read.pr.unwrap();
        assert_eq!((pr.number, pr.state), (15500, PrState::Merged));
        assert_eq!((pr.checks, pr.checks_complete), (None, None));
        let calls = gh_calls(&gh);
        assert_eq!(
            calls.len(),
            2,
            "open, then closed; no checks query: {calls:?}"
        );
        assert!(
            !calls.iter().any(|call| call.contains("graphql")),
            "{calls:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(gh.parent().unwrap());
    }

    /// **A read of a run's repository and a commit in it never run git at once**: started
    /// together, every git child of one ends before the next begins.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_read_and_a_commit_take_turns_with_git() {
        let _env = crate::ProcessEnvRead::take();
        use std::os::unix::fs::PermissionsExt;
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNP03";
        let probe = test_daemon();
        let dir = run_in_a_repository(&probe, uid);
        let log = dir.join("..").join(format!("{uid}-git.log"));
        let _ = std::fs::remove_file(&log);
        let wrapper = dir.join("..").join(format!("{uid}-git"));
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\necho start >> '{log}'\nsleep 0.05\n'{git}' \"$@\"\ns=$?\necho end >> '{log}'\nexit $s\n",
                log = log.display(),
                git = crate::git::git_bin(None).unwrap().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let daemon = daemon_on(
            Arc::clone(&probe.store),
            Config {
                git_bin: Some(wrapper.to_str().unwrap().into()),
                ..Config::default()
            },
        );
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        let action = commit("m");
        let (read, committed) = tokio::join!(
            daemon.run_git(uid),
            daemon.git_action(uid, "r-both", &action)
        );
        assert!(read.unwrap().is_ok());
        assert!(matches!(
            committed,
            protocol::ws::GitActionResult::Done { .. }
        ));
        let lines: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert!(lines.len() > 4, "{lines:?}");
        for pair in lines.chunks(2) {
            assert_eq!(
                pair,
                ["start", "end"],
                "two git children at once: {lines:?}"
            );
        }
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(&wrapper);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn pending_card(uid: &str, label: &str) -> PendingApproval {
        PendingApproval {
            card: serde_json::from_value(json!({
                "request_id": "r", "payload_hash": "h", "tool_name": "Bash",
                "tool_input": {}, "display_text": "echo hi"
            }))
            .unwrap(),
            session: SessionKey::new(uid, "cc-1"),
            project_label: label.into(),
            agent: protocol::agent::AgentKind::Claude,
            raise: 0,
            claimed: false,
            tool_ran: false,
            fork: false,
            link: false,
            question: false,
        }
    }

    /// **B6 G7:** a hosted run's card, which no hook holds, is one the phone can answer
    /// through its host, so it counts as a decision; the same card on a run with no host
    /// does not, and neither does one whose answer is already on its way.
    #[test]
    fn a_hosted_runs_card_counts_as_a_decision() {
        let mut inner = Inner::default();
        let mut card = pending_card(TEST_UID, "Aion");
        card.card.question_hold = Some(QuestionHold::Held);
        card.link = true;
        inner
            .pending
            .insert((TEST_UID.to_string(), "r".to_string()), card);
        assert_eq!(Waiting::of(&inner).decisions, 1);
        inner.pending.values_mut().next().unwrap().claimed = true;
        assert_eq!(Waiting::of(&inner).decisions, 0, "an answer on its way");
        let entry = inner.pending.values_mut().next().unwrap();
        entry.claimed = false;
        entry.link = false;
        assert_eq!(Waiting::of(&inner).decisions, 0, "no host");
    }

    /// Two cards on one run are one agent, and the run is named once. Pinned
    /// where the rule lives, on hand-built cards.
    #[test]
    fn two_decisions_on_one_run_are_one_agent() {
        // Hosted: the phone answers it through the run's host.
        let held = |uid: &str, label: &str| {
            let mut card = pending_card(uid, label);
            card.card.question_hold = Some(QuestionHold::Held);
            card.link = true;
            card
        };
        let mut inner = Inner::default();
        for request in ["r-1", "r-2"] {
            inner.pending.insert(
                (TEST_UID.to_string(), request.to_string()),
                held(TEST_UID, "Aion"),
            );
        }
        let waiting = Waiting::of(&inner);
        assert_eq!(
            (
                waiting.decisions,
                waiting.label.as_deref(),
                waiting.question
            ),
            (2, Some("Aion"), false),
            "two cards on one run are one agent, named once"
        );

        // Only the computer can answer a card with no host behind it, so it
        // neither counts nor names a run.
        inner.pending.insert(
            ("other-run".to_string(), "r-3".to_string()),
            pending_card("other-run", "Ledger"),
        );
        let waiting = Waiting::of(&inner);
        assert_eq!(
            (waiting.decisions, waiting.label.as_deref()),
            (2, Some("Aion"))
        );

        // Nor does a card whose call already ran, or one already being answered.
        let mut ran = held("other-run", "Ledger");
        ran.tool_ran = true;
        inner
            .pending
            .insert(("other-run".to_string(), "r-4".to_string()), ran);
        let mut claimed = held("other-run", "Ledger");
        claimed.claimed = true;
        inner
            .pending
            .insert(("other-run".to_string(), "r-5".to_string()), claimed);
        assert_eq!(Waiting::of(&inner).decisions, 2);

        // A Codex card is answered through its link.
        let mut codex = pending_card("other-run", "Ledger");
        codex.agent = protocol::agent::AgentKind::Codex;
        inner
            .pending
            .insert(("other-run".to_string(), "r-6".to_string()), codex);
        let waiting = Waiting::of(&inner);
        assert_eq!(
            (waiting.decisions, waiting.label.as_deref()),
            (3, None),
            "two runs hold decisions, so no one of them is named"
        );

        assert_eq!(Waiting::of(&Inner::default()).decisions, 0);

        // The one decision waiting is a question, and says so.
        let mut inner = Inner::default();
        let mut asked = held(TEST_UID, "Aion");
        asked.question = true;
        inner
            .pending
            .insert((TEST_UID.to_string(), "q".to_string()), asked);
        assert!(Waiting::of(&inner).question);
    }

    /// **A phone that re-pairs takes its token with it.** The store moves an
    /// APNs token to the row that just registered it, so the row it came from
    /// holds work for a token it no longer owns — and a worker waiting on a
    /// phone that has already come back under another name.
    #[tokio::test]
    async fn re_registering_a_token_retires_the_row_it_was_taken_from() {
        let (daemon, capture) = capture_daemon();
        async fn pair(daemon: &Arc<Daemon>) -> String {
            let (code, _) = daemon.create_pairing(300).await.unwrap();
            match hello_with_code(daemon, &code).await {
                AuthOutcome::Paired { device_id, .. } => device_id,
                other => panic!("pairing must succeed: {other:?}"),
            }
        }
        let first = pair(&daemon).await;
        let second = pair(&daemon).await;

        daemon
            .register_push(&first, "tok-shared", "production", None)
            .await
            .unwrap();
        assert!(
            capture.1.lock().unwrap().is_empty(),
            "nothing is displaced by the first registration"
        );

        daemon
            .register_push(&second, "tok-shared", "production", None)
            .await
            .unwrap();
        assert_eq!(
            *capture.1.lock().unwrap(),
            vec![first],
            "the row the token was taken from is retired with it"
        );
    }

    /// **Revoking a device retires its push queue with it.**
    ///
    /// The queue holds work authorised before the revocation, and behind it a
    /// worker that would otherwise wait on a phone that is never coming back.
    /// Retirement is *told*, not inferred from a later send: if nothing rings
    /// again, an inference never happens.
    #[tokio::test]
    async fn revoking_a_device_retires_the_queue_holding_its_pushes() {
        let _home = redirected_home("revoke-pushes");
        let (daemon, capture) = capture_daemon();
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        let AuthOutcome::Paired { device_id, .. } = hello_with_code(&daemon, &code).await else {
            panic!("pairing must succeed");
        };
        assert!(capture.1.lock().unwrap().is_empty(), "nothing retired yet");

        daemon.revoke(&device_id).await.unwrap();
        assert_eq!(
            *capture.1.lock().unwrap(),
            vec![device_id],
            "the sender is told the device is gone, at the moment it goes"
        );
    }

    /// **The doorbell names the run it is about, through the real path.**
    ///
    /// `aion` is holding a decision when a Codex run finishes a turn. One slot
    /// means the finished run's doorbell has to speak for the fleet — and
    /// titling it after that run would send the reader to the wrong project to
    /// look for a card that is not there.
    #[tokio::test]
    async fn the_doorbell_that_speaks_for_the_fleet_names_the_blocked_run() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        let blocked_uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
        hosted_run(&daemon, blocked_uid, "/srv/dev/aion");
        hosted_card(
            &daemon,
            blocked_uid,
            1,
            "toolu_aion",
            "Bash",
            json!({"command": "echo hi"}),
        )
        .await;
        // Past the decision's own grace, so what follows is counted alone.
        hold.end();
        rung_after(&capture, 1).await;
        capture.0.lock().unwrap().clear();

        raise_ring(&daemon, Ring::CodexCompleted).await;

        hold.end();
        rung_after(&capture, 1).await;
        let rings = capture.0.lock().unwrap();
        let (hint, _) = rings.first().expect("the completion rings");
        assert_eq!(
            (hint.kind, hint.project_label.as_str()),
            (crate::apns::PushKind::Approval, "aion"),
            "the decision's project, not the run that happened to ring: {rings:?}"
        );
    }

    /// **The badge is a count of decisions, read at the moment of ringing.**
    ///
    /// Asserting it on a hand-made hint would only re-check the composer's
    /// arithmetic; this drives two runs to a decision each and reads the count
    /// off the hint the daemon actually sent. Both say two: the second run blocked while the
    /// first doorbell was still inside its dispatch grace, and a doorbell
    /// describes the fleet it is about to interrupt someone for.
    #[tokio::test]
    async fn the_badge_counts_the_decisions_waiting_across_runs() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        hosted_card(
            &daemon,
            TEST_UID,
            1,
            "toolu_a",
            "Bash",
            json!({"command": "echo a"}),
        )
        .await;
        hosted_card(
            &daemon,
            "01K1B3XQ8ZC0DE5FGH7JKMNPQS",
            1,
            "toolu_b",
            "Bash",
            json!({"command": "echo b"}),
        )
        .await;

        hold.end();
        rung_after(&capture, 2).await;
        let rings = capture.0.lock().unwrap();
        let counts: Vec<usize> = rings.iter().map(|(h, _)| h.decisions).collect();
        assert_eq!(
            counts,
            vec![2, 2],
            "the count is of the fleet when the doorbell rings, and by then both \
             runs were blocked: {rings:?}"
        );
    }

    /// **The count is the app's: every decision waiting, not every run.**
    ///
    /// One run holding two decisions and another holding one is three things
    /// to answer, and the app's fleet says "3 waiting for you"; a doorbell
    /// saying 2 in the same words would contradict it on the lock screen.
    #[tokio::test]
    async fn the_doorbell_counts_every_decision_waiting() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        hosted_card(
            &daemon,
            TEST_UID,
            1,
            "toolu_a1",
            "Bash",
            json!({"command": "echo a1"}),
        )
        .await;
        hosted_card(
            &daemon,
            TEST_UID,
            2,
            "toolu_a2",
            "Bash",
            json!({"command": "echo a2"}),
        )
        .await;
        hosted_card(
            &daemon,
            "01K1B3XQ8ZC0DE5FGH7JKMNPQS",
            1,
            "toolu_b",
            "Bash",
            json!({"command": "echo b"}),
        )
        .await;

        hold.end();
        rung_after(&capture, 3).await;
        let rings = capture.0.lock().unwrap();
        let (last, _) = rings.last().expect("a doorbell rang");
        assert_eq!(last.decisions, 3, "{rings:?}");
        // The badge is this same count (`ApnsPushSender::payload`, the relay's
        // `doorbell`), so one number reaches both.
        assert_eq!(crate::apns::alert(last).1, "3 waiting for you");
    }

    /// A run deleted while a push waits out its dispatch grace must stay
    /// silent. Without the ticket check, the eviction emptied the delivery
    /// watermarks and the task then woke to an *empty* exclusion list — a
    /// ring, to every device, about a session that no longer exists.
    #[tokio::test]
    async fn a_push_in_flight_when_its_session_is_deleted_never_rings() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        open_a_decision(&daemon).await;

        // An ended run with no card of its own, so the deletion below is legal:
        // the gate under test is the push's own ticket, not delete's guards.
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Exited);
        let ticket = daemon
            .push_gate
            .admit_decision(TEST_UID, "r-deleted")
            .expect("a new card is news");
        daemon.dispatch_push(TEST_UID.into(), 1, ticket, false, None);
        let outcome = daemon
            .delete_exited_session(TEST_UID)
            .await
            .expect("the delete call itself succeeds");
        assert!(
            matches!(outcome, protocol::ws::DeleteSessionResult::Deleted { .. }),
            "the premise is a *deleted* run: {outcome:?}"
        );
        hold.end();
        rung_after(&capture, 0).await;

        let rings = capture.0.lock().unwrap();
        assert!(
            rings.is_empty(),
            "a deleted run has nothing to ring about: {rings:?}"
        );
    }

    /// **A removed run's files go with it**, whether one run is deleted or ended runs are
    /// pruned (minor 50); a run that is not removed keeps its own.
    #[tokio::test]
    async fn a_removed_runs_uploads_go_with_it() {
        let daemon = test_daemon();
        let root = crate::uploads::root();
        let pruned = "01K1B3XQ8ZC0DE5FGH7JKMNPR2";
        let kept = "01K1B3XQ8ZC0DE5FGH7JKMNKP3";
        let deleted = "01K1B3XQ8ZC0DE5FGH7JKMNDT5";
        seed_session(&daemon, deleted, "cc-1", Lifecycle::Exited);
        seed_session(&daemon, pruned, "cc-2", Lifecycle::Exited);
        seed_session(&daemon, kept, "cc-3", Lifecycle::Live);
        // An upload's id is the device's, one per file whatever the run.
        for (uid, id) in [(deleted, "u-d"), (pruned, "u-p"), (kept, "u-k")] {
            assert_eq!(
                daemon.uploads.put(&root, 1, uid, id, "a", 1, 0, b"a"),
                Ok(1)
            );
        }
        let outcome = daemon.delete_exited_session(deleted).await.unwrap();
        assert!(matches!(
            outcome,
            protocol::ws::DeleteSessionResult::Deleted { .. }
        ));
        assert!(!root.join(deleted).exists());
        daemon.prune_ended_sessions(false).await.unwrap();
        assert!(!root.join(pruned).exists());
        assert!(root.join(kept).join("u-k").join("a").exists());
        daemon.uploads.remove_session(&root, kept);
    }

    /// **A push admitted for a run that is already gone never rings.**
    ///
    /// The ticket catches an eviction that lands *after* admission. It cannot
    /// catch one that lands just before: an admission already under way mints
    /// its ticket against the epoch the eviction had already bumped — so the
    /// ticket agrees with the world and the push looks perfectly valid. What
    /// settles it is asking, at the moment of ringing, whether the run is still
    /// there.
    #[tokio::test]
    async fn a_push_admitted_for_a_run_that_is_already_gone_never_rings() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        open_a_decision(&daemon).await;

        // Nothing has ever filed a row under this uid, which is the state a
        // deleted run leaves behind.
        let ticket = daemon
            .push_gate
            .admit_decision(TEST_UID, "r-gone")
            .expect("a fresh run admits");
        daemon.dispatch_push(TEST_UID.into(), 1, ticket, false, None);
        hold.end();
        rung_after(&capture, 0).await;
        assert!(
            capture.0.lock().unwrap().is_empty(),
            "a run that is not there has nothing to ring about"
        );

        // The control: the same push for a run that does exist does ring, so
        // the silence above is the check and not the harness.
        hosted_run(&daemon, TEST_UID, "/tmp");
        let ticket = daemon
            .push_gate
            .admit_decision(TEST_UID, "r-there")
            .expect("a new card is news");
        daemon.dispatch_push(TEST_UID.into(), 1, ticket, false, None);
        hold.end();
        rung_after(&capture, 1).await;
        assert_eq!(
            capture.0.lock().unwrap().len(),
            1,
            "a run that is still there rings"
        );
    }

    /// A card that closes inside its doorbell's grace never rings: the ring is
    /// for a decision still open.
    #[tokio::test]
    async fn a_card_retired_before_its_doorbell_rang_does_not_ring() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        hosted_card(
            &daemon,
            TEST_UID,
            1,
            "toolu_first",
            "Bash",
            json!({"command": "echo one"}),
        )
        .await;
        assert!(daemon
            .host_card_withdrawn(TEST_UID, HOST, 2, "toolu_first")
            .await
            .unwrap());
        hosted_card(
            &daemon,
            TEST_UID,
            3,
            "toolu_second",
            "Bash",
            json!({"command": "echo two"}),
        )
        .await;

        hold.end();
        rung_after(&capture, 1).await;
        let rung = format!("{:?}", capture.0.lock().unwrap());
        let count = capture.0.lock().unwrap().len();
        assert_eq!(
            count, 1,
            "only the decision that is still open rings: {rung}"
        );
        let open: Vec<String> = daemon
            .inner
            .lock()
            .await
            .pending
            .keys()
            .map(|(_, request)| request.clone())
            .collect();
        assert_eq!(open, ["toolu_second"], "and it is the one still open");
    }

    // ------------------------------- rings wait while someone is at the Mac

    /// The owner's two minutes, written out rather than read from `presence`, so
    /// a changed constant fails these tests instead of moving them with it.
    const AWAY: Duration = Duration::from_secs(120);

    /// Every path that rings a phone, each raised on its own daemon so no latch
    /// or grace couples one to another.
    #[derive(Debug, Clone, Copy)]
    enum Ring {
        ClaudeApproval,
        ClaudeQuestion,
        CodexApproval,
        CodexQuestion,
        CodexCompleted,
    }

    /// The agent host every hosted card in these tests comes from.
    const HOST: protocol::proc_identity::ProcessIdentity =
        protocol::proc_identity::ProcessIdentity {
            pid: 4242,
            birth: protocol::proc_identity::BirthIdentity {
                start_sec: 1,
                start_usec: 2,
            },
        };

    /// The hosted Claude run `uid` in `cwd`, begun and given its host [`HOST`], as a
    /// start does.
    fn hosted_run(daemon: &Arc<Daemon>, uid: &str, cwd: &str) {
        let now = protocol::time::now_rfc3339();
        let row = SessionRow {
            session_uid: uid.into(),
            session_id: uid.into(),
            cwd: cwd.into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: Lifecycle::Live,
            created_at: now.clone(),
            updated_at: now,
            agent: protocol::agent::AgentKind::Claude,
            codex_thread_id: None,
        };
        let request = format!("start-{uid}");
        assert_eq!(
            daemon.store.begin_run(&row, &request, None, None).unwrap(),
            None
        );
        daemon.store.record_host(uid, &HOST, None).unwrap();
    }

    /// **A listing built while a message moves a run offers its settings, however the move goes on meanwhile** (306,
    /// gate 7): the run its switch ended, the message moving it holding its gate, is listed while the move ends (the
    /// run live again, its mark gone, the gate let go) before the listing is built; it reads with its menus.
    #[tokio::test]
    async fn a_listing_built_across_a_moves_end_offers_its_settings() {
        let daemon = test_daemon();
        hosted_run(&daemon, TEST_UID, "/tmp");
        daemon
            .store
            .keep_hosted_models(
                TEST_UID,
                &serde_json::json!({"models": [
                    {"value": "default", "displayName": "Default", "description": "Opus"}
                ]}),
            )
            .unwrap();
        let moving = daemon.move_gate(TEST_UID).await.lock_owned().await;
        daemon
            .store
            .set_switch_state(TEST_UID, Some(crate::store::Store::SWITCH_MOVED))
            .unwrap();
        daemon
            .store
            .set_lifecycle(TEST_UID, Lifecycle::Exited)
            .unwrap();
        let held = daemon.inner.lock().await;
        let listing = tokio::spawn({
            let daemon = Arc::clone(&daemon);
            async move { daemon.sessions().await.unwrap() }
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        daemon
            .store
            .set_lifecycle(TEST_UID, Lifecycle::Live)
            .unwrap();
        daemon.store.set_switch_state(TEST_UID, None).unwrap();
        drop(moving);
        drop(held);
        let listed = listing.await.unwrap();
        let run = listed
            .iter()
            .find(|run| run.session_uid == TEST_UID)
            .unwrap();
        assert!(run.menus.is_some(), "its settings offered: {run:?}");
    }

    /// The hosted Claude run `uid` asking `tool_name` with `input` through its agent host,
    /// message `seq`: the engine's own path for a card ([`Daemon::host_card_asked`]). A
    /// run not yet begun is begun in `/tmp`.
    async fn hosted_card(
        daemon: &Arc<Daemon>,
        uid: &str,
        seq: u64,
        request_id: &str,
        tool_name: &str,
        input: serde_json::Value,
    ) {
        if daemon.store.hosted_link(uid).unwrap().is_none() {
            hosted_run(daemon, uid, "/tmp");
        }
        let asked = protocol::host::PermissionRequest {
            tool_use_id: request_id.into(),
            tool_name: tool_name.into(),
            input,
            options: json!({}),
        };
        assert!(
            daemon.host_card_asked(uid, HOST, seq, asked).await.unwrap(),
            "the card is filed"
        );
    }

    /// Raise `ring` the way its agent does.
    async fn raise_ring(daemon: &Arc<Daemon>, ring: Ring) {
        match ring {
            Ring::ClaudeApproval => {
                hosted_card(
                    daemon,
                    TEST_UID,
                    1,
                    "toolu_away",
                    "Bash",
                    json!({"command": "echo one"}),
                )
                .await;
            }
            Ring::ClaudeQuestion => {
                hosted_card(
                    daemon,
                    TEST_UID,
                    1,
                    "toolu_away_question",
                    protocol::host::ASK_USER_QUESTION,
                    json!({"questions": [{"question": "Which?", "options": [{"label": "A"}, {"label": "B"}]}]}),
                )
                .await;
            }
            Ring::CodexApproval => {
                seed_codex_session(daemon, TEST_UID, "cc-1");
                assert!(
                    daemon
                        .raise_codex_approval(
                            &SessionKey::new(TEST_UID, "cc-1"),
                            codex_command_card("rq-away"),
                            "01a06db1-1f8e-7db2-8fd5-10f13af55d1b",
                            "01a06db1-2223-7ed0-8be5-1d7e3ccdeaee",
                            "exec-d2700ed3-c69d-4915-a620-36c001e7f577",
                            "commandExecution",
                        )
                        .await,
                    "the card is filed"
                );
            }
            Ring::CodexQuestion => {
                seed_codex_session(daemon, TEST_UID, "cc-1");
                let asked: serde_json::Value = serde_json::from_str(include_str!(
                    "../../../fixtures/codex/plan-question-0.167.0.json"
                ))
                .unwrap();
                let params = &asked["params"];
                let card = crate::codex_approval::Approval::read(
                    crate::codex_approval::Family::Question,
                    params,
                    None,
                )
                .expect("the production parser reads the measured Plan question")
                .card("rq-away-question".into(), 1);
                assert!(
                    daemon
                        .raise_codex_approval(
                            &SessionKey::new(TEST_UID, "cc-1"),
                            card,
                            params["threadId"].as_str().unwrap(),
                            params["turnId"].as_str().unwrap(),
                            params["itemId"].as_str().unwrap(),
                            crate::codex_approval::Family::Question.as_str(),
                        )
                        .await,
                    "the card is filed"
                );
            }
            Ring::CodexCompleted => {
                let uid = protocol::uid::new().unwrap();
                seed_codex_session(daemon, &uid, "cc-9");
                let session = SessionKey::new(&uid, "cc-9");
                let done = codex_turn_terminal(daemon, &session, "th:turn:1").await;
                daemon.push_turn_complete(&session, &done).await;
            }
        }
    }

    fn rung(capture: &CaptureSender) -> Vec<crate::apns::PushKind> {
        capture
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(hint, _)| hint.kind)
            .collect()
    }

    /// What has rung once `count` rings are in, waiting at most a few seconds.
    /// A ring goes when the release pass gets to it, which a busy daemon
    /// delays, so a test waits for it rather than for a fixed time; then a
    /// little longer, so a ring that should not follow has had its chance.
    async fn rung_after(capture: &CaptureSender, count: usize) -> Vec<crate::apns::PushKind> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while rung(capture).len() < count && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(crate::presence::POLL * 3).await;
        rung(capture)
    }

    /// **The phone rings only once the owner has left the Mac.** Every ring, on
    /// both agents, raised while someone is at the Mac waits — no ring after its
    /// dispatch grace — and rings once, as what it is, as soon as the Mac has
    /// had no keyboard or mouse input for two minutes. A Claude approval or
    /// question and a Codex approval or question ring as themselves; a quiet
    /// occasion rings only with a decision open elsewhere, and then as that decision.
    #[tokio::test]
    async fn every_ring_waits_while_someone_is_at_the_mac_and_rings_once_they_have_left() {
        use crate::apns::PushKind;
        for (ring, kind, decision_elsewhere) in [
            (Ring::ClaudeApproval, PushKind::Approval, false),
            (Ring::ClaudeQuestion, PushKind::Question, false),
            (Ring::CodexApproval, PushKind::Approval, false),
            (Ring::CodexQuestion, PushKind::Question, false),
            (Ring::CodexCompleted, PushKind::Approval, true),
        ] {
            let (daemon, capture) = capture_daemon();
            if decision_elsewhere {
                open_a_decision(&daemon).await;
            }
            crate::presence::set_idle(Some(Duration::ZERO));
            raise_ring(&daemon, ring).await;
            tokio::time::sleep(Duration::from_millis(700)).await;
            assert_eq!(rung(&capture), vec![], "{ring:?} rang at the Mac");

            crate::presence::set_idle(Some(AWAY));
            assert_eq!(
                rung_after(&capture, 1).await,
                vec![kind],
                "{ring:?} once the Mac was left"
            );
        }
    }

    /// **A doorbell rings only for a decision the phone can answer.** The
    /// owner's phone was rung by runs going idle or finishing, and every such
    /// tap opened onto nothing to answer. A finished Codex turn rings nothing
    /// while nothing the phone can answer is open — nor while the only open
    /// card is one the computer alone can answer. With a decision the phone can
    /// answer open, it rings, and is about that decision: often the first the
    /// owner hears of a card whose own doorbell was held while he was at the
    /// computer.
    #[tokio::test]
    async fn a_quiet_occasion_rings_only_for_a_decision_the_phone_can_answer() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        raise_ring(&daemon, Ring::CodexCompleted).await;
        hold.end();
        assert_eq!(rung_after(&capture, 0).await, vec![], "with nothing open");

        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        daemon.inner.lock().await.pending.insert(
            ("other-run".into(), "r-at-the-computer".into()),
            pending_card("other-run", "Ledger"),
        );
        raise_ring(&daemon, Ring::CodexCompleted).await;
        hold.end();
        assert_eq!(
            rung_after(&capture, 0).await,
            vec![],
            "with only a card the computer alone can answer"
        );

        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        open_a_decision(&daemon).await;
        raise_ring(&daemon, Ring::CodexCompleted).await;
        hold.end();
        assert_eq!(
            rung_after(&capture, 1).await,
            vec![crate::apns::PushKind::Approval],
            "with a decision the phone can answer open"
        );
        let doorbell = newest_doorbell(&capture);
        assert_eq!(
            (doorbell.decisions, doorbell.project_label.as_str()),
            (1, "Ledger"),
            "it rings about the decision, named by its own run"
        );
    }

    /// The newest doorbell a capture daemon handed its sender.
    fn newest_doorbell(capture: &CaptureSender) -> crate::apns::PushHint {
        capture
            .0
            .lock()
            .unwrap()
            .last()
            .map(|(hint, _)| hint.clone())
            .expect("a doorbell rang")
    }

    /// A Codex run other than [`TEST_UID`], so a Claude run can sit beside it.
    const MIXED_CODEX_UID: &str = "01K1B3XQ8ZC0DE5FGH7JKMNPQS";

    /// File one Codex approval card on [`MIXED_CODEX_UID`].
    async fn raise_mixed_codex_card(daemon: &Arc<Daemon>) {
        seed_codex_session(daemon, MIXED_CODEX_UID, "cc-2");
        assert!(
            daemon
                .raise_codex_approval(
                    &SessionKey::new(MIXED_CODEX_UID, "cc-2"),
                    codex_command_card("rq-mixed"),
                    "01a06db1-1f8e-7db2-8fd5-10f13af55d1b",
                    "01a06db1-2223-7ed0-8be5-1d7e3ccdeaee",
                    "exec-d2700ed3-c69d-4915-a620-36c001e7f577",
                    "commandExecution",
                )
                .await,
            "the card is filed"
        );
    }

    /// **The phone's one notification says what the whole fleet needs, whichever
    /// agent rang last.**
    ///
    /// Every doorbell shares one collapse id, a device's queue replaces a waiting
    /// doorbell with the newer one, and the badge is the count the doorbell
    /// carries (`aps.badge` direct, `blocked_count` through the relay). So the
    /// newest doorbell is the phone's whole picture. A Codex turn that finishes
    /// while a Claude decision waits must still say a decision waits, with the
    /// fleet's count; and with one decision of each agent waiting, the count is
    /// two.
    ///
    /// **Mutation:** count only the ringing agent's cards in the dispatch and
    /// the finished Codex turn reads `Completed` with a count of 0.
    #[tokio::test]
    async fn a_finished_codex_turn_does_not_replace_a_waiting_claude_decision() {
        use crate::apns::PushKind;
        let (daemon, capture) = capture_daemon();
        crate::presence::set_idle(Some(AWAY));
        raise_ring(&daemon, Ring::ClaudeApproval).await;
        assert_eq!(rung_after(&capture, 1).await, vec![PushKind::Approval]);

        raise_ring(&daemon, Ring::CodexCompleted).await;
        assert_eq!(
            rung_after(&capture, 2).await.len(),
            2,
            "the Codex turn end rang"
        );
        let after_codex_turn = newest_doorbell(&capture);
        assert_eq!(
            (after_codex_turn.kind, after_codex_turn.decisions),
            (PushKind::Approval, 1),
            "a finished Codex turn must not replace the waiting Claude decision with \
             a finished turn and a badge of 0: {after_codex_turn:?}"
        );
        assert_eq!(
            crate::apns::alert(&after_codex_turn).1,
            PushKind::Approval.sentence()
        );

        raise_mixed_codex_card(&daemon).await;
        assert_eq!(
            rung_after(&capture, 3).await.len(),
            3,
            "the Codex approval rang"
        );
        let both_waiting = newest_doorbell(&capture);
        assert_eq!(
            (both_waiting.kind, both_waiting.decisions),
            (PushKind::Approval, 2),
            "one Claude and one Codex decision are two agents waiting: {both_waiting:?}"
        );
        assert_eq!(crate::apns::alert(&both_waiting).1, "2 waiting for you");
    }

    /// A [`CaptureSender`] whose FIRST send stands still until the test lets it
    /// go — the shape of a sender's synchronous registry read stalling between a
    /// dispatch's count and its enqueue.
    struct StallingSender {
        capture: CaptureSender,
        entered: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }
    impl crate::apns::PushSender for StallingSender {
        fn send(&self, hint: &crate::apns::PushHint, excluded: &[String]) {
            // Taken in a statement of its own: an `if let` would hold this lock
            // through the stall and queue every other send behind it.
            let first = self.entered.lock().unwrap().take();
            if let Some(entered) = first {
                let _ = entered.send(());
                let release = self.release.lock().unwrap().take().expect("one stall");
                // Off the runtime's books while it stands still, so the other
                // dispatches go on running beside it as they would in production.
                tokio::task::block_in_place(|| {
                    let _ = release.recv_timeout(Duration::from_secs(10));
                });
            }
            self.capture.send(hint, excluded);
        }
        fn retire(&self, device_id: &str) {
            self.capture.retire(device_id);
        }
    }

    /// **What a doorbell checked before waiting its turn is checked again after.**
    ///
    /// A doorbell waits for the ordering step behind any send still in it (held
    /// here directly, as a stalled send holds it). In that wait its card can
    /// close and the owner can come back to the Mac. The first makes it a
    /// doorbell about the past, dropped; the second holds it until the owner
    /// leaves again.
    ///
    /// **Mutation:** check the card and the owner's presence only before the
    /// wait, and both legs ring the moment the ordering step is free.
    #[tokio::test]
    async fn a_doorbell_that_waited_its_turn_is_checked_again() {
        use crate::apns::PushKind;

        // The card closes while its doorbell waits: dropped.
        let (daemon, capture) = capture_daemon();
        crate::presence::set_idle(Some(AWAY));
        let stalled = daemon.ring_order.lock().await;
        raise_ring(&daemon, Ring::ClaudeApproval).await;
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert!(daemon
            .host_card_withdrawn(TEST_UID, HOST, 2, "toolu_away")
            .await
            .unwrap());
        drop(stalled);
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert_eq!(rung(&capture), vec![], "the card closed while it waited");

        // The owner comes back while it waits: held, then rung once they leave.
        let (daemon, capture) = capture_daemon();
        crate::presence::set_idle(Some(AWAY));
        let stalled = daemon.ring_order.lock().await;
        raise_ring(&daemon, Ring::ClaudeApproval).await;
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        crate::presence::set_idle(Some(Duration::ZERO));
        drop(stalled);
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert_eq!(rung(&capture), vec![], "the owner is back: it holds");
        crate::presence::set_idle(Some(AWAY));
        assert_eq!(
            rung_after(&capture, 1).await,
            vec![PushKind::Approval],
            "and it rings once they have left again"
        );
    }

    /// **A held ring reads the Mac's idle time about once a second, not ten times.** Idle time
    /// only grows as time passes, so a ring held for someone who used the Mac a moment ago cannot
    /// go for two minutes; reading it every 100 ms woke the daemon ten times a second for as long as
    /// they worked. Held for three seconds, it reads at most a few times.
    #[tokio::test]
    async fn a_held_ring_reads_the_idle_time_about_once_a_second() {
        let (daemon, capture) = capture_daemon();
        crate::presence::set_idle(Some(Duration::ZERO));
        raise_ring(&daemon, Ring::ClaudeApproval).await;
        let before = crate::presence::idle_reads();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let reads = crate::presence::idle_reads() - before;
        assert_eq!(rung(&capture), vec![], "held while someone is at the Mac");
        assert!(
            reads < 10,
            "{reads} reads of the idle time in three held seconds"
        );
        crate::presence::set_idle(Some(AWAY));
        assert_eq!(
            rung_after(&capture, 1).await,
            vec![crate::apns::PushKind::Approval],
            "and it rings once they have left"
        );
    }

    /// **A doorbell counted earlier is never posted after one counted later.**
    ///
    /// Every doorbell shares one collapse slot and a device's queue keeps the one
    /// that arrives last, so arrival order has to be the order the counts were
    /// taken in. Here one run's decision is counted alone and its sender then
    /// stalls before enqueueing; meanwhile another run raises a decision, counted
    /// as 2. Released, the stale count of 1 must not end up as the phone's newest
    /// notification.
    ///
    /// **Mutation:** take the ordering lock out of the dispatch and the stalled
    /// `Approval, 1` lands last.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_earlier_count_never_lands_after_a_later_one() {
        use crate::apns::PushKind;
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let sender = Arc::new(StallingSender {
            capture: CaptureSender::new(),
            entered: std::sync::Mutex::new(Some(entered_tx)),
            release: std::sync::Mutex::new(Some(release_rx)),
        });
        let daemon = Daemon::new(
            Config::default(),
            shared_store(),
            Arc::clone(&sender) as Arc<dyn crate::apns::PushSender>,
            Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
        );
        crate::presence::set_idle(Some(AWAY));

        // Run A raises a decision, counted alone; its send stalls after the count.
        raise_ring(&daemon, Ring::ClaudeApproval).await;
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .expect("run A's doorbell reached its sender");

        // Run B raises a decision while A is stalled, and has every chance to ring.
        raise_mixed_codex_card(&daemon).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;

        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sender.capture.0.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let rung: Vec<(PushKind, usize)> = sender
            .capture
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(hint, _)| (hint.kind, hint.decisions))
            .collect();
        assert_eq!(
            rung.last(),
            Some(&(PushKind::Approval, 2)),
            "the newest doorbell must count both decisions, with a badge of 2: {rung:?}"
        );
    }

    /// **A direction override in a run's directory reaches the alert title as a
    /// replacement character.** The title of a direct APNs alert is the project
    /// label of the run holding the decision, drawn by the lock screen as it
    /// arrives.
    #[tokio::test]
    async fn a_direction_override_in_a_cwd_reaches_the_alert_title_replaced() {
        let (daemon, capture) = capture_daemon();
        crate::presence::set_idle(Some(AWAY));
        hosted_run(&daemon, TEST_UID, "/work/abc\u{202E}gpj.exe");
        raise_ring(&daemon, Ring::ClaudeApproval).await;
        assert_eq!(rung_after(&capture, 1).await.len(), 1, "the decision rang");
        let title = crate::apns::alert(&newest_doorbell(&capture)).0;
        assert_eq!(title, "abc\u{FFFD}gpj.exe");
    }

    /// **Two minutes without input, and not a moment less.**
    #[tokio::test]
    async fn a_ring_waits_for_two_full_minutes_without_input() {
        let (daemon, capture) = capture_daemon();
        crate::presence::set_idle(Some(AWAY - Duration::from_millis(1)));
        raise_ring(&daemon, Ring::ClaudeApproval).await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(rung(&capture), vec![], "a breath short of two minutes");

        crate::presence::set_idle(Some(AWAY));
        assert_eq!(
            rung_after(&capture, 1).await,
            vec![crate::apns::PushKind::Approval]
        );
    }

    /// **Who already knows is read when the held ring goes, not when it was
    /// raised.** A phone whose live socket received the fact while the owner sat
    /// at the Mac has seen it, and is not rung about it on their way out.
    #[tokio::test]
    async fn a_phone_that_saw_the_fact_while_its_ring_was_held_is_not_rung() {
        let (daemon, capture) = capture_daemon();
        crate::presence::set_idle(Some(Duration::ZERO));
        raise_ring(&daemon, Ring::ClaudeApproval).await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        daemon
            .push_gate
            .note_delivered("watching-phone", TEST_UID, 1_000);

        crate::presence::set_idle(Some(AWAY));
        rung_after(&capture, 1).await;
        let rings = capture.0.lock().unwrap();
        assert_eq!(rings.len(), 1, "{rings:?}");
        assert_eq!(rings[0].1, vec!["watching-phone".to_string()]);
    }

    /// **A held ring rings only if it still matters when the owner leaves.**
    /// The run moving on (a new Codex turn, the run ending), the decision
    /// closing, or the run being deleted while the owner sat there each mean
    /// there is nothing left to tell the phone.
    #[tokio::test]
    async fn a_held_ring_that_stopped_mattering_never_rings() {
        async fn leave_and_listen(capture: &CaptureSender, case: &str) {
            crate::presence::set_idle(Some(AWAY));
            // Longer than the held ring's longest wait, a second, so a ring that still mattered
            // would have gone.
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            assert_eq!(rung(capture), vec![], "{case}");
        }

        async fn codex_turn_held(daemon: &Arc<Daemon>) -> String {
            open_a_decision(daemon).await;
            let uid = protocol::uid::new().unwrap();
            seed_codex_session(daemon, &uid, "cc-9");
            let session = SessionKey::new(&uid, "cc-9");
            let done = codex_turn_terminal(daemon, &session, "th:turn:1").await;
            daemon.push_turn_complete(&session, &done).await;
            tokio::time::sleep(Duration::from_millis(700)).await;
            uid
        }

        let (daemon, capture) = capture_daemon();
        crate::presence::set_idle(Some(Duration::ZERO));
        let uid = codex_turn_held(&daemon).await;
        daemon.note_turn_running(&uid);
        leave_and_listen(&capture, "a new Codex turn started").await;

        let (daemon, capture) = capture_daemon();
        crate::presence::set_idle(Some(Duration::ZERO));
        let uid = codex_turn_held(&daemon).await;
        daemon.store.record_host(&uid, &HOST, None).unwrap();
        assert!(
            daemon
                .end_hosted_run(
                    &uid,
                    HOST,
                    None,
                    Lifecycle::Live,
                    Lifecycle::Exited,
                    json!({"exit_code": 0})
                )
                .await
                .unwrap(),
            "the premise: the run ended"
        );
        leave_and_listen(&capture, "the run ended").await;

        for ring in [Ring::ClaudeApproval, Ring::ClaudeQuestion] {
            let (daemon, capture) = capture_daemon();
            crate::presence::set_idle(Some(Duration::ZERO));
            raise_ring(&daemon, ring).await;
            tokio::time::sleep(Duration::from_millis(700)).await;
            let request = daemon
                .inner
                .lock()
                .await
                .pending
                .keys()
                .map(|(_, request)| request.clone())
                .next()
                .expect("the premise: an open decision");
            assert!(daemon
                .host_card_withdrawn(TEST_UID, HOST, 2, &request)
                .await
                .unwrap());
            leave_and_listen(&capture, &format!("{ring:?}: the card closed")).await;
        }

        let (daemon, capture) = capture_daemon();
        crate::presence::set_idle(Some(Duration::ZERO));
        open_a_decision(&daemon).await;
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Exited);
        let ticket = daemon
            .push_gate
            .admit_decision(TEST_UID, "r-held")
            .expect("a new card is news");
        daemon.dispatch_push(TEST_UID.into(), 1, ticket, false, None);
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(matches!(
            daemon.delete_exited_session(TEST_UID).await,
            Ok(protocol::ws::DeleteSessionResult::Deleted { .. })
        ));
        leave_and_listen(&capture, "the run was deleted").await;
    }

    /// The per-device floor: a second test inside the window is refused with
    /// the wait, a different device is not, and the window expires.
    #[tokio::test]
    async fn the_test_push_gate_is_per_device_and_timed() {
        let daemon = test_daemon();
        assert_eq!(
            daemon.test_push_gate("dev-a").await,
            None,
            "first send passes"
        );
        let wait = daemon.test_push_gate("dev-a").await;
        assert!(
            wait.is_some_and(|s| (1..=30).contains(&s)),
            "second is floored: {wait:?}"
        );
        assert_eq!(
            daemon.test_push_gate("dev-b").await,
            None,
            "another device is unaffected"
        );

        // Expiry, proven rather than trusted: age the recorded send past the
        // floor by editing the record, not by sleeping 30 wall seconds.
        {
            let mut inner = daemon.inner.lock().await;
            let aged = std::time::Instant::now() - std::time::Duration::from_secs(31);
            inner.test_pushes.insert("dev-a".into(), aged);
        }
        assert_eq!(
            daemon.test_push_gate("dev-a").await,
            None,
            "a window that has passed refuses nobody"
        );
    }

    // ------------------------------------------------------------ the end gate

    /// **The gate is per run.** A second hold for the same run waits for the
    /// first, and another run's does not.
    #[tokio::test]
    async fn the_registration_gate_serializes_one_sessions_lifecycle() {
        let daemon = test_daemon();
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
        let gate = daemon.registration_gate(uid).await;
        let held = gate.lock().await;

        // A second hold for the SAME run cannot start while one is held.
        let other = daemon.registration_gate(uid).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), other.lock())
                .await
                .is_err(),
            "a second hold for the same run waits for the first"
        );
        // A different session is unaffected — the gate is per-uid, not global.
        let elsewhere = daemon.registration_gate("01K1B3XQ8ZC0DE5FGH7JKMNP99").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), elsewhere.lock())
                .await
                .is_ok(),
            "another session's lifecycle must not queue behind this one"
        );
        drop(held);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), other.lock())
                .await
                .is_ok(),
            "and the next sequence proceeds once the first releases"
        );
    }

    /// A finished-turn fact, filed the way the live link files one.
    async fn codex_turn_terminal(daemon: &Arc<Daemon>, session: &SessionKey, key: &str) -> Event {
        daemon
            .ingest(
                PendingEvent::new(
                    session,
                    EventKind::TurnComplete,
                    json!({"status": "completed"}),
                    protocol::event::Source::Codex,
                )
                .with_source_event_id(key.to_string()),
            )
            .await
            .expect("the append succeeds")
            .expect("a fresh key files a fact")
    }

    // ------------------------------- codex push admission

    /// **A Codex turn-complete rings the same doorbell Claude's does, and carries
    /// the agent that decides who hears it.**
    #[tokio::test]
    async fn a_codex_turn_complete_rings_once_per_turn() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        open_a_decision(&daemon).await;
        let uid = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &uid, "cc-9");
        let session = SessionKey::new(&uid, "cc-9");
        let first = codex_turn_terminal(&daemon, &session, "th:turn:1").await;
        daemon.push_turn_complete(&session, &first).await;
        hold.end();
        rung_after(&capture, 1).await;
        {
            let rings = capture.0.lock().unwrap();
            assert_eq!(rings.len(), 1, "one finished turn, one ring: {rings:?}");
            assert_eq!(rings[0].0.kind, crate::apns::PushKind::Approval);
            assert_eq!(rings[0].0.session_uid, uid, "about the run that finished");
        }

        // **The second turn is second news.** Claude's `agent_completed` re-fires on
        // a timer and the ambient latch collapses the repeats; `turn/completed`
        // fires exactly once per turn, so there is nothing to collapse and every
        // terminal must ring on its own. Far enough apart to be two doorbells —
        // what the dispatch grace does to two turns closer together than that is
        // `two_turns_inside_one_dispatch_grace_ring_once_on_both_paths`.
        let second = codex_turn_terminal(&daemon, &session, "th:turn:2").await;
        daemon.push_turn_complete(&session, &second).await;
        hold.end();
        assert_eq!(
            rung_after(&capture, 2).await.len(),
            2,
            "a second turn finishing is a second fact, not a repeat of the first"
        );
    }

    /// **Two turns inside one dispatch grace are one ring.**
    ///
    /// The 400 ms grace exists so a doorbell describes the world at the moment it
    /// rings rather than the moment it was admitted, and the price of that is
    /// exactly this: a turn superseded before its push left the building does not
    /// ring, and the reader is told the later, truer thing once. The alternative —
    /// two banners 300 ms apart about the same open decisions — is one fact said
    /// twice.
    #[tokio::test]
    async fn two_turns_inside_one_dispatch_grace_ring_once() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        open_a_decision(&daemon).await;
        let uid = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &uid, "cc-9");
        let session = SessionKey::new(&uid, "cc-9");
        let first = codex_turn_terminal(&daemon, &session, "th:turn:1").await;
        daemon.push_turn_complete(&session, &first).await;
        let second = codex_turn_terminal(&daemon, &session, "th:turn:2").await;
        daemon.push_turn_complete(&session, &second).await;
        hold.end();
        assert_eq!(
            rung_after(&capture, 1).await.len(),
            1,
            "the first turn's doorbell is superseded by the second, which rings"
        );
    }

    /// **A turn that merely STARTS inside the grace cancels the doorbell too.**
    ///
    /// The parity test above covers the second turn *finishing*. This is the other
    /// half, and it was the live hole: turn A completes, turn B starts 200 ms later,
    /// and A's completion doorbell was still valid when its grace expired — so a
    /// phone was told "finished a turn" about a run that was mid-turn. Codex's turn
    /// start is `turn/started` on the wire, and this is what reads it; a hosted Claude
    /// run's is the start its host files, read the same way in `claude_host`'s
    /// `a_hosted_turn_starting_inside_the_grace_silences_the_last_ones_doorbell`.
    #[tokio::test]
    async fn a_turn_starting_inside_the_grace_cancels_the_last_turns_doorbell() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        open_a_decision(&daemon).await;
        let uid = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &uid, "cc-9");
        let session = SessionKey::new(&uid, "cc-9");
        let first = codex_turn_terminal(&daemon, &session, "th:turn:1").await;
        daemon.push_turn_complete(&session, &first).await;
        // The next turn begins inside the grace. No terminal, no fact, no event — just
        // the run going back to work.
        daemon.note_turn_running(&uid);
        hold.end();
        assert_eq!(
            rung_after(&capture, 0).await.len(),
            0,
            "a completion doorbell must not ring about a run that is mid-turn again"
        );

        // And the latch is open, not stuck: the turn that started then finishes,
        // and *that* is news.
        let second = codex_turn_terminal(&daemon, &session, "th:turn:2").await;
        daemon.push_turn_complete(&session, &second).await;
        hold.end();
        assert_eq!(
            rung_after(&capture, 1).await.len(),
            1,
            "the turn that superseded the first one rings when it ends"
        );
    }

    // ===================================================== deleting one run

    /// The guard the store cannot hold, and the reason it is here.
    ///
    /// Recording a run's end writes the lifecycle first and closes the run's cards
    /// after its gate is released, so for that moment a card is still in
    /// `inner.pending` while the row it belongs to reads `Exited` and the phone
    /// will happily offer a swipe on it. Delete the rows underneath that approval
    /// and whatever resolves it then writes events for a session that no longer
    /// exists — there is no foreign key to stop it.
    ///
    /// `prune_ended_sessions` has refused exactly this since it was written. A
    /// swipe must not be the way around it.
    #[tokio::test]
    async fn an_ended_run_with_an_approval_still_open_is_not_deletable() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        seed_codex_session(&daemon, uid, "cc-1");
        assert!(raise_codex_card(&daemon, uid, "req-open", "item-open").await);

        // The end's own write, without the closing that follows it.
        daemon.store.set_lifecycle(uid, Lifecycle::Exited).unwrap();
        assert_eq!(lifecycle_of(&daemon, uid), Lifecycle::Exited);
        assert_eq!(
            daemon.inner.lock().await.pending.len(),
            1,
            "the premise: the card is still open while the row reads Exited"
        );

        assert_eq!(
            daemon.delete_exited_session(uid).await.unwrap(),
            protocol::ws::DeleteSessionResult::StillRunning,
            "the daemon still holds live state for it, whatever the column says"
        );
        assert!(
            daemon.store.get_session(uid).unwrap().is_some(),
            "and the row is still there to be held"
        );
    }

    /// The control for the test above: nothing held, so it goes.
    #[tokio::test]
    async fn an_ended_run_nobody_is_holding_is_deletable() {
        let daemon = test_daemon();
        let uid = "01KYZ5E56X0D1RT7ZVRYK1ZEF9";
        seed_session(&daemon, uid, "cc-9", Lifecycle::Exited);

        assert_eq!(
            daemon.delete_exited_session(uid).await.unwrap(),
            protocol::ws::DeleteSessionResult::Deleted { events: 0 }
        );
        assert!(daemon.store.get_session(uid).unwrap().is_none());
    }

    // ------------------------------------------------------------ risk

    #[tokio::test]
    async fn approval_cards_carry_a_risk_class() {
        let daemon = test_daemon();
        hosted_card(
            &daemon,
            TEST_UID,
            1,
            "toolu_rm",
            "Bash",
            json!({"command": "rm -rf /tmp/build"}),
        )
        .await;

        let event = daemon
            .store
            .events_after(TEST_UID, 0, 10)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EventKind::ApprovalRequest)
            .expect("an approval must be logged");
        assert_eq!(event.payload["card"]["risk"]["class"], "high");
        assert_eq!(event.payload["card"]["risk"]["matched_pattern"], "rm -rf");
    }

    #[tokio::test]
    async fn a_read_only_tool_is_classified_low() {
        let daemon = test_daemon();
        hosted_card(
            &daemon,
            TEST_UID,
            1,
            "toolu_read",
            "Read",
            json!({"file_path": "/tmp/x"}),
        )
        .await;
        let event = daemon
            .store
            .events_after(TEST_UID, 0, 10)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EventKind::ApprovalRequest)
            .unwrap();
        assert_eq!(event.payload["card"]["risk"]["class"], "low");
        assert!(event.payload["card"]["risk"]["matched_pattern"].is_null());
    }

    // ------------------------------------------------- local resolution

    // ======================================================== prompt identity

    // =================================================== ordering and publish

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn append_and_publish_are_serialised_per_run() {
        // Assigning a seq and publishing it are two steps, and without a gate
        // around *both* a task holding seq 1 can be overtaken by one holding
        // seq 2 — after which the socket has seen 2 and can never accept 1.
        // The gate is what makes "the order they were numbered" and "the order
        // they were sent" the same sentence.
        //
        // Held from the outside here, which is the only way to assert its
        // existence rather than its luck: while the gate is held, an ingest for
        // that run must not be able to complete.
        let daemon = test_daemon();
        let key = test_key();
        seed_session(&daemon, &key.uid, &key.name, Lifecycle::Live);
        let gate = daemon.publish_gate(&key.uid).await;
        let held = gate.lock().await;

        let ingest = {
            let daemon = Arc::clone(&daemon);
            let key = key.clone();
            tokio::spawn(async move {
                daemon
                    .ingest(
                        PendingEvent::new(&key, EventKind::ToolCall, json!({}), Source::Hook)
                            .with_source_event_id("blocked"),
                    )
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !ingest.is_finished(),
            "an ingest must not be able to number or publish an event while the run's \
             gate is held"
        );
        assert_eq!(
            daemon.store.max_seq(&key.uid).unwrap(),
            0,
            "and it must not have reached the store either"
        );

        drop(held);
        assert!(ingest.await.unwrap().unwrap().is_some());
        assert_eq!(daemon.store.max_seq(&key.uid).unwrap(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_ingests_reach_a_subscriber_in_sequence_order() {
        // The property the gate buys, under the load that used to break it.
        // Every subscriber must see 1, 2, 3 … with no reordering, because a
        // socket that receives a higher seq first has no way to accept the
        // lower one afterwards.
        let daemon = test_daemon();
        let key = test_key();
        seed_session(&daemon, &key.uid, &key.name, Lifecycle::Live);
        let mut events = daemon.events_tx.subscribe();

        let mut tasks = Vec::new();
        for i in 0..64u32 {
            let daemon = Arc::clone(&daemon);
            let key = key.clone();
            tasks.push(tokio::spawn(async move {
                daemon
                    .ingest(
                        PendingEvent::new(&key, EventKind::ToolCall, json!({"i": i}), Source::Hook)
                            .with_source_event_id(format!("e{i}")),
                    )
                    .await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            seen.push(event.seq);
        }
        assert_eq!(
            seen,
            (1..=64u64).collect::<Vec<_>>(),
            "published order must be assignment order"
        );
    }

    // ===================================================== payload truncation

    #[tokio::test]
    async fn an_oversized_payload_is_cut_on_a_character_boundary() {
        // `String::truncate` panics on a non-boundary byte, and a tool response
        // is exactly the payload that carries multi-byte text — a path with an
        // accent, a diff with an emoji. The cap was also being reported *after*
        // the cut, so the recorded original size was the truncated one.
        let daemon = daemon_with(Config {
            max_payload_bytes: 4096,
            ..Config::default()
        });
        // Emoji are four bytes each, so the cap lands mid-character for three
        // of every four possible caps; this one is chosen to land inside one.
        let text = "🔥".repeat(4096);
        let mut pending = PendingEvent::new(
            &test_key(),
            EventKind::ToolResult,
            json!({ "stdout": text }),
            Source::Hook,
        );
        let original = pending.payload.to_string().len();
        assert!(original > 4096);

        daemon.truncate_payload(&mut pending);

        assert_eq!(pending.payload["_codeconnect_truncated"], true);
        assert_eq!(
            pending.payload["_original_bytes"].as_u64().unwrap() as usize,
            original,
            "the recorded original size must be the size before the cut"
        );
        let written = pending.payload.to_string().len();
        assert!(written <= 4096, "the replacement is {written} bytes");
        assert!(written > 4096 / 2, "the preview keeps half the room");
        // It survives the round trip an event has to make.
        let encoded = serde_json::to_string(&pending.payload).unwrap();
        let _: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    }

    /// **A cut payload keeps only the cut's own keys.** Nothing places a Codex item read
    /// back late any more, so a `belongs_before` it carried goes with its content.
    #[tokio::test]
    async fn a_cut_payload_keeps_only_the_cut() {
        let daemon = daemon_with(Config {
            max_payload_bytes: 4096,
            ..Config::default()
        });
        let mut pending = PendingEvent::new(
            &test_key(),
            EventKind::ToolResult,
            json!({
                "aggregated_output": "x".repeat(8192),
                "belongs_before": "01a1777e-f38f:item:msg_0de00084643b",
            }),
            Source::Codex,
        );

        daemon.truncate_payload(&mut pending);

        let keys: Vec<&str> = pending
            .payload
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["_codeconnect_truncated", "_original_bytes", "_preview"]
        );
    }

    #[tokio::test]
    async fn a_payload_under_the_cap_is_untouched() {
        let daemon = test_daemon();
        let mut pending = PendingEvent::new(
            &test_key(),
            EventKind::ToolResult,
            json!({"stdout": "ok"}),
            Source::Hook,
        );
        let before = pending.payload.clone();
        daemon.truncate_payload(&mut pending);
        assert_eq!(pending.payload, before);
    }

    /// Two typed prompts with a pasted image each, recorded from Claude Code
    /// 2.1.308 (`fixtures/transcript/big-image-prompt-2.1.308.jsonl`), with
    /// images small enough to commit: against a 4 KiB limit the first line fits
    /// and the second does not, as the recorded 368,774- and 677,762-byte lines
    /// stood against the default 512 KiB.
    const BIG_IMAGE_PROMPTS: &str =
        include_str!("../../../fixtures/transcript/big-image-prompt-2.1.308.jsonl");
    const FITTING_PROMPT: &str = "b845702c-3e73-4e8d-b734-3349f64dddf8";
    const OVERSIZED_PROMPT: &str = "761181c6-1c42-4173-bf8b-8e4f89bd553f";

    fn recorded_prompt(uuid: &str) -> serde_json::Value {
        BIG_IMAGE_PROMPTS
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|line| line["uuid"] == uuid)
            .unwrap()
    }

    /// The preview object, within the limit once written and over half of it:
    /// the preview takes half the room, and written again inside the
    /// replacement it at most doubles.
    fn is_replaced(payload: &serde_json::Value, original_bytes: usize, limit: usize) -> bool {
        let keys: Vec<&String> = payload.as_object().unwrap().keys().collect();
        let written = payload.to_string().len();
        keys == ["_codeconnect_truncated", "_original_bytes", "_preview"]
            && payload["_codeconnect_truncated"] == true
            && payload["_original_bytes"] == original_bytes
            && written <= limit
            && written > limit / 2
    }

    /// **A replaced payload fits the limit once written.** The preview is the
    /// payload's own JSON, escaped again inside the replacement: a backslash or
    /// a quote costs two bytes there, so a control character, `\u0001` in the
    /// payload, costs seven. Cut at the limit's byte count instead, a
    /// backslash-heavy payload was a 1,048,622-byte replacement against the
    /// default 524,288, past the 1 MiB a phone takes in one frame.
    ///
    /// **Mutation:** cut the preview at `max_payload_bytes` again and every
    /// case is over the limit.
    #[tokio::test]
    async fn a_replaced_payload_fits_the_limit_once_written() {
        for limit in [4096, Config::default().max_payload_bytes] {
            let daemon = daemon_with(Config {
                max_payload_bytes: limit,
                ..Config::default()
            });
            for text in [
                "\\".repeat(limit),
                "\"".repeat(limit),
                "é".repeat(limit),
                "\u{1}".repeat(limit),
            ] {
                let payload = json!({ "stdout": text });
                let encoded = payload.to_string();
                let mut pending =
                    PendingEvent::new(&test_key(), EventKind::ToolResult, payload, Source::Hook);
                daemon.truncate_payload(&mut pending);
                assert!(
                    is_replaced(&pending.payload, encoded.len(), limit),
                    "{} bytes against {limit}",
                    pending.payload.to_string().len()
                );
                assert!(encoded.starts_with(pending.payload["_preview"].as_str().unwrap()));
            }
        }
    }

    /// **A typed prompt over the limit keeps everything but its image.**
    ///
    /// Replaced whole, the prompt lost `promptSource`, `origin`, `uuid` and its
    /// words: the phone drew no YOU row for it and read the session as done
    /// while Claude worked on it — 6 typed prompts with images in a real
    /// history. The image is reduced to the block that names it, and the
    /// markers say the line was cut and from what size.
    ///
    /// **Mutation:** take the prompt arm out of `truncate_payload` and the
    /// payload comes back as the preview object.
    #[tokio::test]
    async fn a_typed_prompt_over_the_limit_keeps_its_fields_and_words_and_loses_its_image() {
        let daemon = daemon_with(Config {
            max_payload_bytes: 4096,
            ..Config::default()
        });
        let line = recorded_prompt(OVERSIZED_PROMPT);
        let original = line.to_string().len();
        assert!(original > 4096);
        let mut pending = PendingEvent::new(
            &test_key(),
            EventKind::UserMessage,
            line.clone(),
            Source::Transcript,
        );

        daemon.truncate_payload(&mut pending);

        let mut expected = line.clone();
        expected["message"]["content"][1] = json!({"type": "image"});
        expected["_codeconnect_truncated"] = json!(true);
        expected["_original_bytes"] = json!(original);
        assert_eq!(pending.payload, expected);
        assert_eq!(pending.payload["promptSource"], "typed");
        assert_eq!(
            pending.payload["message"]["content"][0]["text"],
            "[Image #2] SLOW and this bigger one"
        );
        assert!(pending.payload.to_string().len() <= 4096);

        // The prompt whose image fits keeps it.
        let line = recorded_prompt(FITTING_PROMPT);
        assert!(line.to_string().len() <= 4096);
        let mut pending = PendingEvent::new(
            &test_key(),
            EventKind::UserMessage,
            line.clone(),
            Source::Transcript,
        );
        daemon.truncate_payload(&mut pending);
        assert_eq!(pending.payload, line);
    }

    /// **A prompt that does not fit even without its images is replaced, as any
    /// payload is** — whether its words are too long, or it is the bare string a
    /// prompt with nothing attached is recorded as. Cut, the words would be a
    /// different prompt on the phone.
    ///
    /// **Mutation:** drop the size check on the reduced prompt and the long one
    /// is filed over the limit.
    #[tokio::test]
    async fn a_prompt_too_large_without_its_images_is_replaced_like_any_payload() {
        let daemon = daemon_with(Config {
            max_payload_bytes: 4096,
            ..Config::default()
        });
        let mut long_words = recorded_prompt(OVERSIZED_PROMPT);
        long_words["message"]["content"][0]["text"] = json!("é".repeat(4096));
        let mut bare = recorded_prompt(FITTING_PROMPT);
        bare["message"]["content"] = json!("é".repeat(4096));
        for line in [long_words, bare] {
            let original = line.to_string().len();
            let mut pending = PendingEvent::new(
                &test_key(),
                EventKind::UserMessage,
                line,
                Source::Transcript,
            );
            daemon.truncate_payload(&mut pending);
            assert!(
                is_replaced(&pending.payload, original, 4096),
                "{}",
                pending.payload
            );
        }
    }

    /// **Only a prompt is kept.** A tool's result over the limit is still
    /// replaced: the phone draws it as "(output too large; the daemon truncated
    /// it)" (`Timeline.swift`), where a result reduced to the blocks that name
    /// it would draw as a tool that printed nothing. Claude's own lines are
    /// replaced as before, a `Write` of a large file among them. Hook and Codex
    /// cards are bounded where they are built; see the card-limit tests.
    ///
    /// **Mutation:** keep a line of any kind and both are filed reduced.
    #[tokio::test]
    async fn a_tool_result_or_a_reply_over_the_limit_is_still_replaced() {
        let daemon = daemon_with(Config {
            max_payload_bytes: 4096,
            ..Config::default()
        });
        let image = recorded_prompt(OVERSIZED_PROMPT)["message"]["content"][1].clone();
        // `Read` of an image: Claude records the picture twice, in the result
        // and in `toolUseResult`.
        let result = json!({
            "type": "user", "uuid": "u-read",
            "message": {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "toolu_read", "content": [image.clone()],
            }]},
            "toolUseResult": {"type": "image", "file": {
                "base64": image["source"]["data"], "type": "image/jpeg",
            }},
        });
        let reply = json!({
            "type": "assistant", "uuid": "a-write",
            "message": {"role": "assistant", "content": [{
                "type": "tool_use", "id": "toolu_write", "name": "Write",
                "input": {"file_path": "/scratch/work/big.txt", "content": "x".repeat(8192)},
            }]},
        });
        for (kind, line) in [
            (EventKind::ToolResult, result),
            (EventKind::AgentMessage, reply),
        ] {
            let original = line.to_string().len();
            let mut pending = PendingEvent::new(&test_key(), kind, line, Source::Transcript);
            daemon.truncate_payload(&mut pending);
            assert!(
                is_replaced(&pending.payload, original, 4096),
                "{}",
                pending.payload
            );
        }
    }

    // ========================================================== send_text

    #[tokio::test]
    async fn send_text_refuses_an_identity_that_does_not_match_its_own_text() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        seed_session(&daemon, uid, "cc-1", Lifecycle::Live);
        let honest = protocol::hash::send_text_hash(uid, "deploy to prod", true);

        // A hash for different text: a captured frame replayed with new content
        // under an id the ledger already trusts.
        let swapped = daemon
            .send_text(
                uid,
                "rm -rf /".to_string(),
                Some("st-1"),
                Some(&honest),
                true,
                0,
                &[],
            )
            .await;
        assert!(
            matches!(&swapped, SendTextResult::Refused { reason } if reason.contains("payload_hash")),
            "{swapped:?}"
        );

        // An id with no hash cannot say what it is a retry *of*.
        let unbound = daemon
            .send_text(uid, "hi".to_string(), Some("st-2"), None, true, 0, &[])
            .await;
        assert!(
            matches!(unbound, SendTextResult::Refused { .. }),
            "{unbound:?}"
        );
    }

    #[tokio::test]
    async fn send_text_is_bounded_and_only_a_run_codeconnect_started_takes_one() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        seed_session(&daemon, uid, "cc-1", Lifecycle::Live);

        let huge = "x".repeat(protocol::ws::MAX_SEND_TEXT_BYTES + 1);
        let refused = daemon.send_text(uid, huge, None, None, true, 0, &[]).await;
        assert!(
            matches!(&refused, SendTextResult::Refused { reason } if reason.contains("ceiling")),
            "{refused:?}"
        );
    }

    #[test]
    fn constant_time_eq_behaves_like_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    // ======================================= the still-shared ledger tables

    /// The four tables a rolled-back v0.6.0 daemon reads — and writes — without
    /// walking a session row, which is why splitting sessions by agent did not
    /// hide them and why [`Daemon::shared_ledgers_admit`] exists.
    ///
    /// `store.rs` holds the same line one layer down, in
    /// `no_codex_row_reaches_a_table_a_rolled_back_daemon_sweeps_globally`. That
    /// test constructs a bare `Store` and so cannot see the three producers; it
    /// stays green while a producer leaks, which is what the three tests below
    /// are for. They drive the real entry points against a real Codex row.
    const SHARED_LEDGERS: [&str; 4] = [
        "text_mutations",
        "pending_approvals",
        "answer_claims",
        "answers",
    ];

    /// What each of the four holds for `session_uid`, asked of SQLite on a
    /// connection of our own. Deliberately not asked through `Store`: its readers
    /// answer for the rows it knows how to make, and the claim here is the
    /// stronger "no row of any shape landed", which only the table can settle.
    fn ledger_rows(db: &std::path::Path, session_uid: &str) -> Vec<(&'static str, i64)> {
        let conn = rusqlite::Connection::open(db).expect("the daemon's own database file");
        SHARED_LEDGERS
            .into_iter()
            .map(|table| {
                let count = conn
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {table} WHERE session_uid = ?1"),
                        [session_uid],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap_or_else(|err| panic!("counting {table}: {err}"));
                (table, count)
            })
            .collect()
    }

    fn assert_no_ledger_rows(db: &std::path::Path, session_uid: &str) {
        let rows = ledger_rows(db, session_uid);
        assert!(
            rows.iter().all(|(_, count)| *count == 0),
            "a Codex run must own no row in the tables a rolled-back daemon sweeps \
             globally: {rows:?}"
        );
    }

    /// **A durable audit of insert *attempts*, because counting rows afterwards
    /// cannot tell a gate from a tidy-up.**
    ///
    /// Most of these producers end in a refusal whether or not they are gated,
    /// and those ungated shapes clean up after themselves: `send_text` claims a
    /// `text_mutations` row, finds no agent host to send it to, and *releases*
    /// the claim; `answer` claims, and when nothing actuates it settles. The
    /// exceptions do the opposite and prove the same point: an ungated
    /// `PermissionRequest` persists a card, and a responder-backed `answer`
    /// records an answer row — durable rows the final-state assertions DO see.
    /// For the cleaning shapes the final table contents read zero either way,
    /// and a gate moved to after the claim would leave those tests green while
    /// restoring
    /// the exact window the rollback story is about — a daemon killed between
    /// the claim and the release leaves the `applying` row behind, and a
    /// rolled-back v0.6.0 rewrites every one it finds.
    ///
    /// An `AFTER INSERT` trigger per table settles it. It fires inside the
    /// producer's own transaction, so its audit row commits when the claim
    /// commits — and the release that follows deletes the claim, not the audit.
    /// The question the audit answers is therefore "did a row ever exist", which
    /// is the question the rollback cares about, rather than "does one exist
    /// now".
    ///
    /// Installed on a connection of our own after the store is open; SQLite's
    /// schema cookie makes the daemon's connections pick them up.
    fn arm_ledger_tripwire(db: &std::path::Path) {
        let conn = rusqlite::Connection::open(db).expect("the daemon's own database file");
        conn.execute_batch(
            "CREATE TABLE ledger_insert_audit(table_name TEXT NOT NULL, session_uid TEXT);",
        )
        .expect("arming the audit table");
        for table in SHARED_LEDGERS {
            conn.execute_batch(&format!(
                "CREATE TRIGGER audit_{table} AFTER INSERT ON {table}
                 BEGIN
                     INSERT INTO ledger_insert_audit(table_name, session_uid)
                     VALUES('{table}', NEW.session_uid);
                 END;"
            ))
            .unwrap_or_else(|err| panic!("arming the audit trigger on {table}: {err}"));
        }
    }

    /// What the tripwire caught. Empty is the only acceptable answer for a
    /// Codex run.
    fn assert_no_insert_was_attempted(db: &std::path::Path, session_uid: &str) {
        let conn = rusqlite::Connection::open(db).expect("the daemon's own database file");
        let caught: Vec<String> = conn
            .prepare("SELECT table_name FROM ledger_insert_audit WHERE session_uid = ?1")
            .unwrap()
            .query_map([session_uid], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            caught.is_empty(),
            "the gate let a durable write BEGIN before refusing: rows were inserted into \
             {caught:?} for a Codex run and then cleaned up again. A daemon killed in that \
             window leaves them behind, and a rolled-back v0.6.0 rewrites every one it finds"
        );
    }

    fn six_nodes() -> protocol::tailnet::Tailnet {
        protocol::tailnet::Tailnet::parse(include_str!(
            "../../../fixtures/tailscale/status-six-nodes.json"
        ))
        .unwrap()
    }

    fn refusal(status: protocol::ipc::MailStatus) -> String {
        match status {
            protocol::ipc::MailStatus {
                state: protocol::ipc::MailState::Refused,
                reason: Some(reason),
                ..
            } => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// **Two sessions on one computer message each other without Tailscale.** With no reading this computer's runs
    /// are addressed `<run id>@this.computer`; a reply reaches its sender here whichever name its address carries and
    /// whether Tailscale came up or went down since; an ended run is told apart from Tailscale; a message filed just
    /// before a stop is delivered by the next sweep. Another computer still needs Tailscale, a known computer's name
    /// still goes to that computer, and a tagged computer still takes part in no messages.
    #[tokio::test]
    async fn sessions_on_one_computer_message_each_other_without_tailscale() {
        use protocol::ipc::MailState;
        let daemon = test_daemon();
        let tailnet = six_nodes();
        let (a, b) = (protocol::uid::new().unwrap(), protocol::uid::new().unwrap());
        seed_codex_session(&daemon, &a, "cc-1");
        daemon.mail_run_live(&a).await;
        seed_codex_session(&daemon, &b, "cc-2");
        daemon.mail_run_live(&b).await;
        let ended = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &ended, "cc-3");
        let read = |run: &str, offer: &str| {
            daemon
                .store
                .offer_mail(run, offer, protocol::time::now_unix_ms())
                .unwrap()
                .messages
        };

        let (you, agents, unreachable) = daemon
            .list_agents(&a)
            .await
            .expect("listed without Tailscale");
        assert_eq!(you.address, format!("{a}@this.computer"));
        let listed: Vec<_> = agents.iter().map(|e| e.address.clone()).collect();
        assert_eq!(listed, [format!("{b}@this.computer")]);
        assert_eq!(unreachable, ["other computers (Tailscale is unavailable)"]);

        // Sent with no reading, answered with one.
        let sent = daemon
            .send_agent_message_on(None, &a, &format!("{b}@this.computer"), "one", None)
            .await;
        assert_eq!(sent.state, MailState::Waiting, "{sent:?}");
        let got = read(&b, "o1");
        assert_eq!(
            (got[0].from.as_str(), got[0].text.as_str()),
            (format!("{a}@this.computer").as_str(), "one")
        );
        let reply = daemon
            .send_agent_message_on(
                Some(&tailnet),
                &b,
                &got[0].from,
                "two",
                Some(&got[0].message_id),
            )
            .await;
        assert_eq!(reply.state, MailState::Waiting, "{reply:?}");
        assert_eq!(read(&a, "o2")[0].text, "two");

        // Sent with a reading, answered with none.
        let sent = daemon
            .send_agent_message_on(Some(&tailnet), &a, &format!("{b}@studio"), "three", None)
            .await;
        assert_eq!(sent.state, MailState::Waiting, "{sent:?}");
        let got = read(&b, "o3");
        assert_eq!(got[0].from, format!("{a}@studio"));
        let reply = daemon
            .send_agent_message_on(None, &b, &got[0].from, "four", Some(&got[0].message_id))
            .await;
        assert_eq!(reply.state, MailState::Waiting, "{reply:?}");
        assert_eq!(read(&a, "o4")[0].text, "four");

        // An ended run here is not blamed on Tailscale, with a reading or without.
        for reading in [None, Some(&tailnet)] {
            let status = daemon
                .send_agent_message_on(reading, &a, &format!("{ended}@this.computer"), "x", None)
                .await;
            assert!(
                refusal(status).contains("not a running session"),
                "{reading:?}"
            );
        }

        // Filed just before a stop: the next sweep with no reading delivers it.
        let filed = crate::store::SentMail {
            message_id: protocol::uid::new().unwrap(),
            context_id: protocol::uid::new().unwrap(),
            in_reply_to: None,
            session_uid: a.clone(),
            recipient: format!("{b}@this.computer"),
            recipient_computer: String::new(),
            text: "five".into(),
        };
        daemon
            .store
            .file_sent_mail(&filed, protocol::time::now_unix_ms())
            .unwrap();
        daemon.sweep_mail_on(None).await;
        assert_eq!(
            read(&b, "o5")
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            ["five"]
        );

        // Must-keeps.
        let elsewhere = protocol::uid::new().unwrap();
        let status = daemon
            .send_agent_message_on(None, &a, &format!("{elsewhere}@laptop"), "x", None)
            .await;
        assert!(refusal(status).contains("Tailscale"));
        let status = daemon
            .send_agent_message_on(Some(&tailnet), &a, &format!("{b}@linux-box"), "x", None)
            .await;
        assert!(
            refusal(status).contains("linux-box is offline"),
            "a known computer's name goes to that computer"
        );
        assert!(read(&b, "o6").is_empty());
    }

    /// **The refusals a session is shown speak of sessions and messages**: a pair over its hourly limit, and a store
    /// that cannot be read, whether for a reply, a read, or another computer saying where a message stands.
    #[tokio::test]
    async fn a_pair_over_its_limit_and_an_unreadable_store_are_refused_in_sessions_words() {
        use protocol::peer::{PeerReply, PeerRequest};
        let (store, db) = shared_store_on_disk();
        let daemon = daemon_on(store, Config::default());
        let tailnet = six_nodes();
        let (sender, codex) = (protocol::uid::new().unwrap(), protocol::uid::new().unwrap());
        seed_codex_session(&daemon, &sender, "cc-2");
        daemon.mail_run_live(&sender).await;
        seed_codex_session(&daemon, &codex, "cc-1");
        daemon.mail_run_live(&codex).await;
        let to = format!("{codex}@studio");
        let send = |reply_to: Option<&str>| {
            let (daemon, tailnet, sender, to) = (
                Arc::clone(&daemon),
                tailnet.clone(),
                sender.clone(),
                to.clone(),
            );
            let reply_to = reply_to.map(str::to_string);
            async move {
                daemon
                    .send_agent_message_on(Some(&tailnet), &sender, &to, "hi", reply_to.as_deref())
                    .await
            }
        };
        for _ in 0..crate::mail::LIMITS.per_pair_per_hour {
            assert_eq!(send(None).await.state, protocol::ipc::MailState::Waiting);
        }
        assert!(refusal(send(None).await)
            .contains("the most CodeConnect takes from one session for another"));

        rusqlite::Connection::open(db.path())
            .unwrap()
            .execute_batch("DROP TABLE mail; DROP TABLE mail_sent;")
            .unwrap();
        let reply_to = protocol::uid::new().unwrap();
        assert_eq!(
            refusal(send(Some(&reply_to)).await),
            "CodeConnect could not read its messages"
        );
        assert_eq!(
            daemon.offer_agent_messages(&codex).await.unwrap_err(),
            "CodeConnect could not read its messages"
        );
        let asked = PeerRequest::PeerMailState {
            message_id: reply_to,
            state: protocol::ipc::MailState::Read,
        };
        assert_eq!(
            daemon
                .serve_peer_on(Some(&tailnet), "100.64.0.2".parse().unwrap(), asked)
                .await,
            PeerReply::PeerRefused {
                reason: "the receiving computer could not read its messages".into()
            }
        );
    }

    /// **What an agent's message is told, case by case**, on this computer as the
    /// recorded tailnet names it (`studio`; `desktop` online, `linux-box` offline).
    #[tokio::test]
    async fn an_agents_message_is_refused_or_waits() {
        let daemon = test_daemon();
        let tailnet = six_nodes();
        let sender = protocol::uid::new().unwrap();
        let codex = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &sender, "cc-2");
        daemon.mail_run_live(&sender).await;
        seed_codex_session(&daemon, &codex, "cc-1");
        daemon.mail_run_live(&codex).await;
        let send = |to: String, text: String| {
            let daemon = Arc::clone(&daemon);
            let tailnet = tailnet.clone();
            let sender = sender.clone();
            async move {
                daemon
                    .send_agent_message_on(Some(&tailnet), &sender, &to, &text, None)
                    .await
            }
        };
        let hi = || "hi".to_string();
        let to = |computer: &str| format!("{codex}@{computer}");

        assert!(refusal(send("cc-1@studio".into(), hi()).await).contains("<run id>@computer"));
        assert!(refusal(send("cc-1@studio".into(), hi()).await).contains("as msg_sessions"));
        assert!(refusal(send(format!("{codex}@"), hi()).await).contains("<run id>@computer"));
        assert!(refusal(send(to("studio"), String::new()).await).contains("empty"));
        assert!(refusal(send(to("studio"), "a\u{1b}[2Jb".into()).await).contains("control"));
        let long = "x".repeat(protocol::ipc::MAIL_TEXT_BYTES + 1);
        assert!(refusal(send(to("studio"), long).await).contains("too long"));
        let stranger = format!("{}@nowhere", protocol::uid::new().unwrap());
        assert!(refusal(send(stranger, hi()).await).contains("not a computer"));
        assert!(refusal(send(to("linux-box"), hi()).await).contains("offline"));
        for device in ["phone", "desktop"] {
            assert!(
                refusal(send(to(device), hi()).await).contains("CodeConnect does not run on"),
                "{device}"
            );
        }
        let never = protocol::uid::new().unwrap();
        assert!(
            refusal(send(format!("{never}@studio"), hi()).await).contains("not a running session")
        );
        assert!(
            refusal(send(format!("{sender}@STUDIO"), hi()).await).contains("own address"),
            "its own address, whatever the case of the computer's name"
        );
        assert!(refusal(
            daemon
                .send_agent_message_on(None, &sender, &format!("{never}@studio"), "hi", None)
                .await
        )
        .contains("Tailscale"));
        assert!(refusal(
            daemon
                .send_agent_message_on(Some(&tailnet), "nobody", &to("studio"), "hi", None)
                .await
        )
        .contains("not known"));
        assert!(refusal(
            daemon
                .send_agent_message_on(Some(&tailnet), &sender, &to("studio"), "hi", Some("m0"))
                .await
        )
        .contains("not a message you were sent"));

        for text in ["please review", "and this"] {
            let status = send(to("Studio"), text.into()).await;
            assert_eq!(
                status.state,
                protocol::ipc::MailState::Waiting,
                "{status:?}"
            );
        }
        let offer = daemon
            .store
            .offer_mail(&codex, "o1", protocol::time::now_unix_ms())
            .unwrap();
        let from = format!("{sender}@studio");
        let read: Vec<(&str, &str, &str)> = offer
            .messages
            .iter()
            .map(|m| (m.from.as_str(), m.label.as_str(), m.text.as_str()))
            .collect();
        assert_eq!(
            read,
            [
                (from.as_str(), "cc-2@studio", "please review"),
                (from.as_str(), "cc-2@studio", "and this")
            ]
        );
    }

    /// **A run is addressed as itself, never by its label.** Two runs under one
    /// label are two entries and two mailboxes; once a run is gone, mail to it is
    /// refused even while another run carries its label, and the other never reads
    /// what was left for it.
    #[tokio::test]
    async fn a_run_is_addressed_as_itself_never_by_its_label() {
        let daemon = test_daemon();
        let tailnet = six_nodes();
        let sender = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &sender, "cc-2");
        daemon.mail_run_live(&sender).await;
        let first = protocol::uid::new().unwrap();
        seed_session(&daemon, &first, "cc-1", Lifecycle::Live);
        daemon.store.record_host(&first, &HOST, None).unwrap();
        daemon.mail_run_live(&first).await;
        let second = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &second, "cc-1");
        daemon.mail_run_live(&second).await;

        let (_, agents) = daemon
            .list_agents_on(Some(&tailnet), &sender, Vec::new())
            .await
            .unwrap();
        let mut listed: Vec<(String, String)> =
            agents.into_iter().map(|a| (a.address, a.label)).collect();
        listed.sort();
        let mut expected = vec![
            (format!("{first}@studio"), "cc-1@studio".to_string()),
            (format!("{second}@studio"), "cc-1@studio".to_string()),
        ];
        expected.sort();
        assert_eq!(listed, expected, "two runs, one label, two addresses");

        let send = |run: &str, text: &str| {
            let daemon = Arc::clone(&daemon);
            let (tailnet, sender, to, text) = (
                tailnet.clone(),
                sender.clone(),
                format!("{run}@studio"),
                text.to_string(),
            );
            async move {
                daemon
                    .send_agent_message_on(Some(&tailnet), &sender, &to, &text, None)
                    .await
            }
        };
        assert_eq!(
            send(&first, "for the first").await.state,
            protocol::ipc::MailState::Waiting
        );
        assert_eq!(
            send(&second, "for the second").await.state,
            protocol::ipc::MailState::Waiting
        );
        let texts = |run: &str| -> Vec<String> {
            daemon
                .store
                .offer_mail(
                    run,
                    &protocol::uid::new().unwrap(),
                    protocol::time::now_unix_ms(),
                )
                .unwrap()
                .messages
                .into_iter()
                .map(|m| m.text)
                .collect()
        };
        assert_eq!(texts(&first), ["for the first"]);
        assert_eq!(texts(&second), ["for the second"]);

        // The first run ends; a new run takes its label.
        assert!(daemon
            .end_hosted_run(
                &first,
                HOST,
                None,
                Lifecycle::Live,
                Lifecycle::Exited,
                json!({"exit_code": 0})
            )
            .await
            .unwrap());
        let replacement = protocol::uid::new().unwrap();
        seed_session(&daemon, &replacement, "cc-1", Lifecycle::Live);
        daemon.mail_run_live(&replacement).await;
        assert!(refusal(send(&first, "too late").await).contains("not a running session"));
        assert!(
            texts(&replacement).is_empty(),
            "nothing of the old run's reaches the new one"
        );
    }

    /// **A reply stays in its conversation, and each sender sees its own message
    /// read**, once, in its next `msg_read`, by way of the offer its session was
    /// given.
    #[tokio::test]
    async fn a_reply_keeps_its_conversation_and_the_sender_sees_its_message_read() {
        let daemon = test_daemon();
        let tailnet = six_nodes();
        let (asker, answerer) = (protocol::uid::new().unwrap(), protocol::uid::new().unwrap());
        seed_codex_session(&daemon, &asker, "cc-2");
        daemon.mail_run_live(&asker).await;
        seed_codex_session(&daemon, &answerer, "cc-1");
        daemon.mail_run_live(&answerer).await;

        let question = daemon
            .send_agent_message_on(
                Some(&tailnet),
                &asker,
                &format!("{answerer}@studio"),
                "status?",
                None,
            )
            .await;
        assert_eq!(question.state, protocol::ipc::MailState::Waiting);
        assert_eq!(
            question.reason.as_deref(),
            shown_only(),
            "the sender is told only what this computer's switch says"
        );
        let (offer, batch) = daemon.offer_agent_messages(&answerer).await.unwrap();
        let received = batch.messages[0].clone();
        assert_eq!(received.message_id, question.message_id);
        assert_eq!(received.from, format!("{asker}@studio"));
        daemon.read_offered_mail(&offer).await;

        let answer = daemon
            .send_agent_message_on(
                Some(&tailnet),
                &answerer,
                &received.from,
                "green",
                Some(&question.message_id),
            )
            .await;
        assert_eq!(answer.state, protocol::ipc::MailState::Waiting);
        let (offer, batch) = daemon.offer_agent_messages(&asker).await.unwrap();
        let reply = &batch.messages[0];
        assert_eq!(
            reply.context_id, received.context_id,
            "the same conversation"
        );
        assert_eq!(
            reply.in_reply_to.as_deref(),
            Some(question.message_id.as_str())
        );
        assert_eq!(
            batch
                .updates
                .iter()
                .map(|u| (u.message_id.as_str(), u.state))
                .collect::<Vec<_>>(),
            [(question.message_id.as_str(), protocol::ipc::MailState::Read)]
        );
        daemon.read_offered_mail(&offer).await;
        let (_, again) = daemon.offer_agent_messages(&asker).await.unwrap();
        assert!(again.messages.is_empty() && again.updates.is_empty());
    }

    /// **Another computer's daemon is served only when it is one of the owner's
    /// computers**, read from this computer's own tailnet: its message is taken as a
    /// local one is, from a run of the computer the connection came from, once per
    /// message id, with its labels as data; and only the computer a message went to
    /// may say where it stands.
    #[tokio::test]
    async fn another_computers_daemon_is_served_only_from_the_owners_computers() {
        use protocol::peer::{PeerReply, PeerRequest};
        let daemon = test_daemon();
        let tailnet = six_nodes();
        let codex = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &codex, "cc-1");
        daemon.mail_run_live(&codex).await;
        let desktop: std::net::IpAddr = "100.64.0.2".parse().unwrap();
        let remote = protocol::uid::new().unwrap();
        let (m1, m2, m3) = (
            protocol::uid::new().unwrap(),
            protocol::uid::new().unwrap(),
            protocol::uid::new().unwrap(),
        );
        let message =
            |id: &str, from_uid: &str, project: &str, to: &str, text: &str| PeerRequest::PeerMail {
                message_id: id.into(),
                context_id: id.into(),
                in_reply_to: None,
                from_uid: from_uid.into(),
                from: "cc-7".into(),
                agent: "Codex".into(),
                project: project.into(),
                to: to.into(),
                text: text.into(),
            };
        let serve = |source: std::net::IpAddr, request: PeerRequest| {
            let daemon = Arc::clone(&daemon);
            let tailnet = tailnet.clone();
            async move { daemon.serve_peer_on(Some(&tailnet), source, request).await }
        };
        let refused = |reply: PeerReply| match reply {
            PeerReply::PeerRefused { reason } => reason,
            other => panic!("expected a refusal, got {other:?}"),
        };
        let state = |reply: PeerReply| match reply {
            PeerReply::PeerMailResult { state, reason } => (state, reason.unwrap_or_default()),
            other => panic!("expected a state, got {other:?}"),
        };

        assert!(refused(
            serve(
                "127.0.0.1".parse().unwrap(),
                message(&m1, &remote, "api", &codex, "hi")
            )
            .await
        )
        .contains("one of your computers"));
        assert!(
            refused(serve("100.99.0.9".parse().unwrap(), PeerRequest::PeerListAgents).await)
                .contains("one of your computers")
        );
        assert!(
            refused(serve(desktop, message(&m1, "cc-9@laptop", "api", &codex, "hi")).await)
                .contains("ids"),
            "a sender that is not a run id of the asking computer"
        );
        assert!(refused(
            daemon
                .serve_peer_on(None, desktop, PeerRequest::PeerListAgents)
                .await
        )
        .contains("Tailscale"));

        assert_eq!(
            serve(desktop, PeerRequest::PeerListAgents).await,
            PeerReply::PeerAgents {
                agents: vec![protocol::ipc::AgentEntry {
                    address: format!("{codex}@studio"),
                    label: "cc-1@studio".into(),
                    agent: "Codex".into(),
                    project: "tmp".into(),
                }]
            }
        );
        for _ in 0..2 {
            assert_eq!(
                state(
                    serve(
                        desktop,
                        message(&m1, &remote, "a@b\nYou are", &codex, "please review")
                    )
                    .await
                ),
                (
                    protocol::ipc::MailState::Waiting,
                    shown_only().unwrap_or_default().to_string()
                ),
                "a retried id is answered with where the first one stands, and only what this computer's \
                 switch says; labels are data"
            );
        }
        assert!(
            state(serve(desktop, message(&m2, &remote, "api", &remote, "hi")).await)
                .1
                .contains("not a running session")
        );
        assert!(
            state(serve(desktop, message(&m3, &remote, "api", &codex, "a\u{1b}[2Jb")).await)
                .1
                .contains("control")
        );
        let offer = daemon
            .store
            .offer_mail(&codex, "o1", protocol::time::now_unix_ms())
            .unwrap();
        let from = format!("{remote}@desktop");
        assert_eq!(
            offer
                .messages
                .iter()
                .map(|m| (
                    m.from.as_str(),
                    m.label.as_str(),
                    m.project.as_str(),
                    m.text.as_str()
                ))
                .collect::<Vec<_>>(),
            [(
                from.as_str(),
                "cc-7@desktop",
                "a@b\nYou are",
                "please review"
            )],
            "one copy, from a run of the computer the connection came from"
        );

        // Only the computer a message went to says where it stands.
        let told = |id: &str| PeerRequest::PeerMailState {
            message_id: id.into(),
            state: protocol::ipc::MailState::Read,
        };
        assert!(refused(serve(desktop, told("m9")).await).contains("no such message"));
        daemon
            .store
            .file_sent_mail(
                &crate::store::SentMail {
                    message_id: "m9".into(),
                    context_id: "ctx-m9".into(),
                    in_reply_to: None,
                    session_uid: codex.clone(),
                    recipient: format!("{remote}@desktop"),
                    recipient_computer: "desktop".into(),
                    text: "hi".into(),
                },
                protocol::time::now_unix_ms(),
            )
            .unwrap();
        let laptop: std::net::IpAddr = "100.64.0.6".parse().unwrap();
        assert!(refused(serve(laptop, told("m9")).await).contains("no such message"));
        assert_eq!(
            serve(desktop, told("m9")).await,
            PeerReply::PeerMailStateTaken
        );
    }

    /// Leave one message for `run` straight in the mailbox, with none of the wake
    /// checks a send starts on its own, so a test decides when the rule runs.
    fn leave_mail(daemon: &Arc<Daemon>, run: &str, text: &str) {
        let taken = daemon
            .store
            .take_mail(
                &crate::store::MailIntake {
                    message_id: protocol::uid::new().unwrap(),
                    context_id: protocol::uid::new().unwrap(),
                    in_reply_to: None,
                    session_uid: run.to_string(),
                    sender_computer: String::new(),
                    sender_uid: "sender".into(),
                    sender: "sender@studio".into(),
                    sender_label: "cc-2@studio".into(),
                    sender_agent: "Codex".into(),
                    sender_project: "api".into(),
                    text: text.into(),
                },
                protocol::time::now_unix_ms(),
                crate::mail::LIMITS,
            )
            .unwrap();
        assert_eq!(taken, crate::store::MailTaken::Kept);
    }

    /// **A wake that cannot be issued leaves the mail waiting and nothing
    /// outstanding** (M-12): a Codex run whose `codex` cannot be run is tried again
    /// at the next trigger, never handed the mail any other way.
    #[tokio::test]
    async fn a_wake_that_cannot_be_issued_leaves_nothing_outstanding() {
        let daemon = test_daemon();
        let codex = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &codex, "cc-1");
        daemon.mail_run_live(&codex).await;
        leave_mail(&daemon, &codex, "hi");
        daemon.wake_if_due(&codex).await;
        assert!(
            daemon
                .store
                .claim_mail_wake(&codex, protocol::time::now_unix_ms(), false)
                .unwrap(),
            "no wake left outstanding"
        );
    }

    /// **A message written down before the daemon stopped still goes** (round 1,
    /// B-38): one to a session on this computer, kept in the outbox and never taken
    /// because the daemon stopped in between, is taken by the next sweep, once.
    #[tokio::test]
    async fn a_local_message_the_daemon_stopped_before_taking_is_taken_by_the_sweep() {
        let daemon = test_daemon();
        let tailnet = six_nodes();
        let sender = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &sender, "cc-2");
        daemon.mail_run_live(&sender).await;
        let receiver = protocol::uid::new().unwrap();
        seed_session(&daemon, &receiver, "cc-1", Lifecycle::Live);
        daemon.mail_run_live(&receiver).await;
        let mail = crate::store::SentMail {
            message_id: protocol::uid::new().unwrap(),
            context_id: protocol::uid::new().unwrap(),
            in_reply_to: None,
            session_uid: sender.clone(),
            recipient: format!("{receiver}@studio"),
            recipient_computer: String::new(),
            text: "written down, then the daemon stopped".into(),
        };
        daemon
            .store
            .file_sent_mail(&mail, protocol::time::now_unix_ms())
            .unwrap();
        for _ in 0..2 {
            daemon.sweep_mail_on(Some(&tailnet)).await;
        }
        let (_, batch) = daemon.offer_agent_messages(&receiver).await.unwrap();
        assert_eq!(
            batch
                .messages
                .iter()
                .map(|m| (m.message_id.as_str(), m.from.clone()))
                .collect::<Vec<_>>(),
            [(mail.message_id.as_str(), format!("{sender}@studio"))],
            "taken once, from its sender"
        );
    }

    /// **A person's prompt with mail waiting is told of it**, in the fixed words
    /// and nothing of the message, and a run with none is told nothing (M-09, M-11).
    #[tokio::test]
    async fn a_persons_prompt_is_told_of_waiting_mail_in_the_fixed_words() {
        let daemon = test_daemon();
        let claude = protocol::uid::new().unwrap();
        seed_session(&daemon, &claude, "cc-1", Lifecycle::Live);
        daemon.mail_run_live(&claude).await;
        assert_eq!(crate::mail::prompt_submitted(&daemon, &claude).await, None);
        daemon
            .store
            .mail_turn_marks(&claude, None, Some(protocol::time::now_unix_ms()))
            .unwrap();
        leave_mail(&daemon, &claude, "MARKER-7731");
        let told = crate::mail::prompt_submitted(&daemon, &claude).await;
        let told = told.expect("told of its mail");
        assert!(protocol::ipc::is_mail_nudge(told), "{told}");
        assert!(!told.contains("MARKER"));
    }

    /// **A run's mail is ended the moment the run exits** (M23-c), not at the next
    /// sweep: its sender reads `ended` at once.
    #[tokio::test]
    async fn a_runs_unread_mail_is_ended_the_moment_it_exits() {
        let daemon = test_daemon();
        let tailnet = six_nodes();
        let sender = protocol::uid::new().unwrap();
        seed_codex_session(&daemon, &sender, "cc-2");
        daemon.mail_run_live(&sender).await;
        let run = protocol::uid::new().unwrap();
        seed_session(&daemon, &run, "cc-1", Lifecycle::Live);
        daemon.store.record_host(&run, &HOST, None).unwrap();
        daemon.mail_run_live(&run).await;
        let status = daemon
            .send_agent_message_on(
                Some(&tailnet),
                &sender,
                &format!("{run}@studio"),
                "hi",
                None,
            )
            .await;
        assert_eq!(status.state, protocol::ipc::MailState::Waiting);
        assert!(daemon
            .end_hosted_run(
                &run,
                HOST,
                None,
                Lifecycle::Live,
                Lifecycle::Exited,
                json!({"exit_code": 0})
            )
            .await
            .unwrap());
        let (_, mine) = daemon.offer_agent_messages(&sender).await.unwrap();
        assert_eq!(
            mine.updates
                .iter()
                .map(|u| (u.message_id.as_str(), u.state))
                .collect::<Vec<_>>(),
            [(status.message_id.as_str(), protocol::ipc::MailState::Ended)]
        );
    }

    /// **The agents a session can message**: every other run on this computer
    /// that was launched with mail, each by its own address with its label beside
    /// it, and never itself; the asker is told its own.
    #[tokio::test]
    async fn an_agent_lists_the_other_running_sessions_on_this_computer() {
        let daemon = test_daemon();
        let tailnet = six_nodes();
        let (asker, codex) = (protocol::uid::new().unwrap(), protocol::uid::new().unwrap());
        seed_codex_session(&daemon, &asker, "cc-2");
        daemon.mail_run_live(&asker).await;
        seed_codex_session(&daemon, &codex, "cc-1");
        daemon.mail_run_live(&codex).await;
        let claude = protocol::uid::new().unwrap();
        seed_session(&daemon, &claude, "cc-3", Lifecycle::Live);
        daemon.mail_run_live(&claude).await;
        seed_session(
            &daemon,
            &protocol::uid::new().unwrap(),
            "cc-4",
            Lifecycle::Live,
        );
        let (you, agents) = daemon
            .list_agents_on(Some(&tailnet), &asker, Vec::new())
            .await
            .unwrap();
        assert_eq!(
            (you.address, you.label),
            (format!("{asker}@studio"), "cc-2@studio".to_string()),
            "the asker is told its own address"
        );
        let mut listed: Vec<(String, String, String)> = agents
            .into_iter()
            .map(|a| (a.address, a.label, a.agent))
            .collect();
        listed.sort();
        let mut expected = vec![
            (
                format!("{codex}@studio"),
                "cc-1@studio".to_string(),
                "Codex".to_string(),
            ),
            (
                format!("{claude}@studio"),
                "cc-3@studio".to_string(),
                "Claude".to_string(),
            ),
        ];
        expected.sort();
        assert_eq!(
            listed, expected,
            "a run not launched with mail is not listed"
        );
        let (_, unread) = daemon
            .list_agents_on(None, &asker, Vec::new())
            .await
            .unwrap();
        assert!(unread.iter().all(|a| a.address.ends_with("@this.computer")));
        let mut status: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/tailscale/status-six-nodes.json"
        ))
        .unwrap();
        status["Self"]["Tags"] = serde_json::json!(["tag:server"]);
        let tagged = protocol::tailnet::Tailnet::parse(&status.to_string()).unwrap();
        assert!(
            daemon
                .list_agents_on(Some(&tagged), &asker, Vec::new())
                .await
                .unwrap_err()
                .contains("tagged"),
            "a tagged computer lists no one"
        );
        assert!(refusal(
            daemon
                .send_agent_message_on(
                    Some(&tagged),
                    &asker,
                    &format!("{codex}@studio"),
                    "hi",
                    None
                )
                .await
        )
        .contains("tagged"));
    }

    #[tokio::test]
    async fn send_text_to_a_codex_session_is_refused_before_it_claims_anything() {
        let (store, db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        // The run is begun before the tripwire is armed. Beginning it writes
        // `codex_sessions` and `run_hosts`, which are none of the four, but scoping
        // the audit to the call under test is what makes an empty answer mean
        // something about the gate rather than about the fixture.
        seed_codex_session(&daemon, uid, "cc-1");
        arm_ledger_tripwire(&db);

        let text = "deploy";
        let hash = protocol::hash::send_text_hash(uid, text, true);
        let result = daemon
            .send_text(
                uid,
                text.to_string(),
                Some("st-1"),
                Some(&hash),
                true,
                0,
                &[],
            )
            .await;

        // **The reason is asserted, and it is the load-bearing assertion.**
        // Without the gate this call still ends in a refusal — the run has no host
        // to deliver it — so the count below reads zero either way, and only the
        // reason says it was the agent gate that refused it.
        match &result {
            SendTextResult::Refused { reason } => assert!(
                reason.contains("codex session") && reason.contains("nothing was sent"),
                "the refusal must name the agent it refused, before the claim: {reason}"
            ),
            other => panic!("a Codex session must not be typed into: {other:?}"),
        }
        assert_no_ledger_rows(&db, uid);
        assert_no_insert_was_attempted(&db, uid);
    }

    /// A real command approval, read by the production parser so the option table
    /// on the card is the one the wire and `Family::labels` actually produce.
    fn codex_command_card(request_id: &str) -> protocol::ws::ApprovalCard {
        codex_command_approval().card(request_id.to_string(), 1)
    }

    /// The same approval, before it is carded — for a caller that needs the
    /// **derived** request id, which is a function of the item rather than
    /// something a test may name.
    fn codex_command_approval() -> crate::codex_approval::Approval {
        let params = json!({
            "kind": "command",
            "threadId": "01a06db1-1f8e-7db2-8fd5-10f13af55d1b",
            "turnId": "01a06db1-2223-7ed0-8be5-1d7e3ccdeaee",
            "itemId": "exec-d2700ed3-c69d-4915-a620-36c001e7f577",
            "environmentId": "local",
            "reason": "May I create the requested probe file?",
            "command": "/bin/zsh -lc 'touch /tmp/marker.txt'",
            "cwd": "/tmp",
            "proposedExecpolicyAmendment": ["touch"],
            "availableDecisions": [
                "accept",
                {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["touch"]}},
                "cancel"
            ]
        });
        crate::codex_approval::Approval::read(crate::codex_approval::Family::Command, &params, None)
            .expect("the production parser reads a measured command approval")
    }

    /// **One Codex approval rings one doorbell, and a re-delivery rings none.**
    ///
    /// The app-server re-delivers an outstanding request to a reconnecting leg,
    /// with the same item id and therefore the same derived request id, so "the
    /// same approval filed twice" is an ordinary event rather than a fault. The
    /// doorbell must not be an ordinary event twice: a phone buzzing again for a
    /// question it is already holding is the fleet telling the same news twice.
    ///
    /// **Two independent layers hold it, and this asserts them together**, which
    /// is deliberate and was measured: breaking either one alone still rings
    /// once, so a test that named only one of them would be satisfied by a build
    /// that had lost it.
    ///
    /// * The doorbell hangs off the FILED event, and a re-delivery files none —
    ///   the event carries `perm:{request_id}`, and the events table refuses a
    ///   second row under the same source id, so `dispatch_push` is not reached.
    /// * The push gate admits one decision per `(session, request_id)` for the
    ///   life of the run, so even an event that were filed twice rings once.
    ///
    /// A third layer sits below both, in the per-device queue that replaces a
    /// waiting doorbell rather than joining it; that one is asserted in
    /// `apns_sender`, because it is about a device rather than about a question.
    ///
    /// The hint is asserted whole, because what it says is what the fan-out then
    /// authorizes: `agent: Codex` is what narrows the fleet to phones that can
    /// open a Codex run, and `kind: Approval` is what makes a tap open the
    /// decision list rather than the timeline.
    ///
    /// **Mutation:** make the event's source id unique per raise AND make
    /// `PushGate::admit_decision` always hand back a ticket. Either alone leaves
    /// this green — which is the point of asserting them together — and both
    /// together ring the phone twice for one question.
    #[tokio::test]
    async fn a_codex_approval_rings_one_doorbell_and_a_redelivery_rings_none() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        let uid = TEST_UID;
        seed_codex_session(&daemon, uid, "cc-1");
        let session = SessionKey::new(uid, "cc-1");
        let card = codex_command_card("rq-ring-once");
        let raise = || {
            let card = card.clone();
            let session = session.clone();
            let daemon = Arc::clone(&daemon);
            async move {
                daemon
                    .raise_codex_approval(
                        &session,
                        card,
                        "01a06db1-1f8e-7db2-8fd5-10f13af55d1b",
                        "01a06db1-2223-7ed0-8be5-1d7e3ccdeaee",
                        "exec-d2700ed3-c69d-4915-a620-36c001e7f577",
                        "commandExecution",
                    )
                    .await
            }
        };

        assert!(raise().await, "the first delivery files the card");
        // Byte-identical, under the same item id: what a reconnecting leg is
        // handed. It rebinds onto the card already standing.
        assert!(
            raise().await,
            "the re-delivery rebinds rather than refusing"
        );
        assert_eq!(
            open_cards(&daemon, uid).len(),
            1,
            "the premise: one question, one card"
        );

        // The dispatch grace: a doorbell describes the world at the moment it
        // rings, not the moment it was admitted, so it is read after that window.
        hold.end();
        rung_after(&capture, 1).await;
        let rings: Vec<(crate::apns::PushKind, String)> = capture
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(hint, _)| (hint.kind, hint.session_uid.clone()))
            .collect();
        assert_eq!(
            rings,
            vec![(crate::apns::PushKind::Approval, uid.to_string())],
            "one approval, one doorbell, about the run that raised it"
        );
    }

    /// Take the link handle away, leaving the registration standing.
    ///
    /// The state a daemon is in when the wrapper is gone but the run is not: an
    /// answer has nowhere to go, and the honest report is that nothing was sent.
    /// The cards this run still has open, read from the store rather than from
    /// memory: a card that is only in memory is not one a restart would restore.
    fn open_cards(daemon: &Arc<Daemon>, uid: &str) -> Vec<crate::store::CodexPendingApprovalRow> {
        daemon.store.codex_pending_approvals(uid).unwrap()
    }

    /// **An option the card never offered is refused before anything is claimed.**
    ///
    /// `decline` is in the app-server's own decision grammar and is deliberately in
    /// neither family's table — the TUI does not render it — so it is the exact
    /// shape of a decision that is wire-legal and still not this card's to give.
    ///
    /// **Mutation:** have `wire_decision` fall back to `Value::String(option_id)`
    /// when the id is not in the options and this goes red: a phone could name any
    /// decision the app-server understands, whatever the card showed.
    #[tokio::test]
    async fn an_option_the_card_never_offered_is_refused_before_the_claim() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");
        // **No link to ask.** A build that let this option through refuses for the
        // WRONG reason, at once — a fast, legible failure instead of a test that
        // hangs waiting for a reply nobody was going to give.
        assert!(raise_codex_card(&daemon, uid, "rq-decline", "exec-decline").await);
        let card: ApprovalCard = serde_json::from_str(&open_cards(&daemon, uid)[0].card).unwrap();

        let result = daemon
            .answer(
                &card.request_id,
                &card.payload_hash,
                AnswerDecision::OptionId {
                    option_id: "decline".into(),
                },
                Some(uid),
                protocol::ws::AnsweredOn::Iphone,
            )
            .await;
        match &result {
            AnswerResult::Rejected { reason } => assert!(
                reason.contains("not one of the options") && reason.contains("nothing was sent"),
                "the refusal must name what was wrong with it: {reason}"
            ),
            other => panic!("a decision this card never offered must not be sent: {other:?}"),
        }
        assert!(
            store
                .unsettled_claims(crate::store::OPERATION_ANSWER)
                .unwrap()
                .is_empty(),
            "a refusal that happens before the ask leaves no claim to recover"
        );
        assert_eq!(
            open_cards(&daemon, uid).len(),
            1,
            "the question still stands, so a correct answer can still be given"
        );
    }

    /// **A stale `payload_hash` is refused, and refused before anything is
    /// claimed.**
    ///
    /// The hash is taken over the card's whole preimage, `tool_input.options`
    /// included, so it is what ties an answer to the exact option table the phone
    /// was shown. A phone answering a card that has since been re-delivered with a
    /// different question would otherwise be picking an option out of a list nobody
    /// is looking at any more.
    ///
    /// **Mutation:** drop the `card.payload_hash != payload_hash` gate and this
    /// goes red — the answer would be written against whatever the card now says.
    #[tokio::test]
    async fn a_stale_payload_hash_is_refused_before_anything_is_claimed() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");
        assert!(raise_codex_card(&daemon, uid, "rq-stale", "exec-stale").await);
        let card: ApprovalCard = serde_json::from_str(&open_cards(&daemon, uid)[0].card).unwrap();

        let result = daemon
            .answer(
                &card.request_id,
                "sha256:a-hash-this-card-was-never-displayed-under",
                AnswerDecision::OptionId {
                    option_id: "accept".into(),
                },
                Some(uid),
                protocol::ws::AnsweredOn::Iphone,
            )
            .await;
        match &result {
            AnswerResult::Rejected { reason } => assert!(
                reason.contains("stale payload_hash"),
                "the refusal must say the card is out of date: {reason}"
            ),
            other => panic!("an answer to a card nobody is showing must not be sent: {other:?}"),
        }
        assert!(
            store
                .unsettled_claims(crate::store::OPERATION_ANSWER)
                .unwrap()
                .is_empty(),
            "the gate is before the claim, so a stale answer leaves nothing behind"
        );
        assert_eq!(open_cards(&daemon, uid).len(), 1);
    }

    /// **An answer with no link to write it on leaves no claim and no terminal.**
    ///
    /// This is the case the claim ordering exists for. Claiming first would leave
    /// an `applying` row for an answer that provably went nowhere, and the next
    /// restart's recovery would retire a perfectly answerable card as `Unknown` —
    /// so the operator loses the question because the link was briefly down.
    ///
    /// **Mutation:** take the claim in `Daemon::answer_codex` before asking the
    /// link and this goes red at `unsettled_answer_claims`.
    #[tokio::test]
    async fn an_answer_with_no_link_leaves_no_claim_and_the_card_standing() {
        let (store, db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");
        assert!(raise_codex_card(&daemon, uid, "rq-nolink", "item-nolink").await);
        let card: protocol::ws::ApprovalCard =
            serde_json::from_str(&open_cards(&daemon, uid)[0].card).unwrap();

        let result = daemon
            .answer(
                &card.request_id,
                &card.payload_hash,
                AnswerDecision::OptionId {
                    option_id: "accept".into(),
                },
                Some(uid),
                protocol::ws::AnsweredOn::Iphone,
            )
            .await;
        match &result {
            AnswerResult::Rejected { reason } => assert_eq!(
                reason,
                crate::codex_refusals::ENGINE_NO_CONNECTION,
                "the refusal must say what stopped it, and that nothing went out"
            ),
            other => panic!("with nothing to write on, nothing can be sent: {other:?}"),
        }
        assert!(
            store
                .unsettled_claims(crate::store::OPERATION_ANSWER)
                .unwrap()
                .is_empty(),
            "an answer that provably actuated nothing leaves no claim: the operator's \
             next tap is a first attempt and not a duplicate"
        );
        assert_eq!(open_cards(&daemon, uid).len(), 1);
        assert_no_ledger_rows(&db, uid);
    }

    /// **A terminal `unknown` is never answered again.**
    ///
    /// The card is deliberately still there in the case this covers: a bounce
    /// re-delivers the request, the app-server is genuinely still waiting, and the
    /// keyboard can still answer it. What must not happen is the phone writing a
    /// second response to a request that accepts exactly one — so the refusal comes
    /// from the ledger and the card is left alone rather than suppressed.
    ///
    /// **Mutation:** let `claim_mutation` replace a terminal row (`INSERT OR
    /// REPLACE`, or dropping the `status = 'applying'` guard on the settle) and the
    /// second answer below is admitted.
    #[tokio::test]
    async fn a_terminal_unknown_answer_is_never_sent_again() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");
        assert!(raise_codex_card(&daemon, uid, "rq-terminal", "item-terminal").await);
        let card: protocol::ws::ApprovalCard =
            serde_json::from_str(&open_cards(&daemon, uid)[0].card).unwrap();
        let now = protocol::time::now_rfc3339();
        let material = crate::store::ClaimedMaterial {
            thread_id: "01a06db1-1f8e-7db2-8fd5-10f13af55d1b".into(),
            generation: 1,
            route: "accept".into(),
            target_turn_id: Some("01a06db1-2223-7ed0-8be5-1d7e3ccdeaee".into()),
            claimed_hash: "hash".into(),
            question_decision: None,
        };
        store
            .claim_mutation(
                crate::store::OPERATION_ANSWER,
                uid,
                &card.request_id,
                &material,
                &now,
            )
            .unwrap();
        assert!(store
            .settle_mutation_indeterminate(
                crate::store::OPERATION_ANSWER,
                uid,
                &card.request_id,
                &now
            )
            .unwrap());

        let result = daemon
            .answer(
                &card.request_id,
                &card.payload_hash,
                AnswerDecision::OptionId {
                    option_id: "accept".into(),
                },
                Some(uid),
                protocol::ws::AnsweredOn::Iphone,
            )
            .await;
        match &result {
            AnswerResult::Rejected { reason } => assert!(
                reason.contains("not known") && reason.contains("will not be sent again"),
                "the operator has to be told why, or the card just looks broken: {reason}"
            ),
            other => panic!("a terminal unknown must never be re-answered: {other:?}"),
        }
        assert_eq!(
            open_cards(&daemon, uid).len(),
            1,
            "the server is still waiting, so the keyboard can still answer it"
        );
    }

    /// **A session this build has no word for is refused before any lock or
    /// claim.**
    ///
    /// `AgentKind::Unsupported` is a run some future build wrote, and its contract
    /// is that every actuation site fails closed. "Not Claude" is not evidence of
    /// Codex: routing it down the Codex path would enter the claim machinery for a
    /// wire this daemon cannot speak.
    ///
    /// **Mutation:** restore the `!shared_ledgers_admit(..)` test in place of the
    /// explicit `AgentKind` match and this goes red — the answer reaches the Codex
    /// path and refuses for the wrong reason.
    #[tokio::test]
    async fn an_unsupported_agent_is_refused_before_any_lock_or_claim() {
        let (store, db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");
        arm_ledger_tripwire(&db);
        // A row of the shape a later build writes, read by this one.
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE codex_sessions SET agent = 'gemini' WHERE session_uid = ?1",
                [uid],
            )
            .unwrap();

        let result = daemon
            .answer(
                "rq-unsupported",
                "hash",
                AnswerDecision::OptionId {
                    option_id: "accept".into(),
                },
                Some(uid),
                protocol::ws::AnsweredOn::Iphone,
            )
            .await;
        match &result {
            AnswerResult::Rejected { reason } => assert!(
                reason.contains("gemini") && reason.contains("nothing was sent"),
                "the refusal must name the agent it refused: {reason}"
            ),
            other => panic!("an unknown agent must fail closed: {other:?}"),
        }
        assert_no_ledger_rows(&db, uid);
        assert_no_insert_was_attempted(&db, uid);
        assert!(store
            .unsettled_claims(crate::store::OPERATION_ANSWER)
            .unwrap()
            .is_empty());
    }

    /// The reason a second answer, from an iPhone, is refused with when the card's answer
    /// claim is already settled `outcome`.
    ///
    /// The card is a real one, raised through the observer's own producer, and the ledger
    /// row is settled through the store's own primitive, so this is the replay path and
    /// not a hand-built string. The claim stores no device, so the same row stands for an
    /// answer from a phone, from the Mac app, or from a client on the static token.
    async fn second_answer_after_a_settled_claim(outcome: &'static str) -> String {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");
        assert!(raise_codex_card(&daemon, uid, "req-settled", "item-settled").await);
        let held = open_cards(&daemon, uid)
            .into_iter()
            .find(|row| row.request_id == "req-settled")
            .expect("the producer filed the card");
        let card: ApprovalCard = serde_json::from_str(&held.card).expect("the card decodes");

        // The exact durable state the first answer left: a claim under this card's key,
        // settled `outcome`; a `lost` one leaves the card standing for the winner's own
        // terminal.
        let claimed = crate::store::ClaimedMaterial {
            thread_id: "th-1".into(),
            generation: 1,
            route: "accept".into(),
            target_turn_id: Some("tu-1".into()),
            claimed_hash: card.payload_hash.clone(),
            question_decision: None,
        };
        let now = protocol::time::now_rfc3339();
        assert_eq!(
            daemon
                .db
                .claim_answer_mutation(uid.to_string(), "req-settled".into(), claimed, now.clone())
                .await
                .expect("the claim"),
            crate::store::MutationClaim::Claimed
        );
        assert!(daemon
            .db
            .settle_answer_mutation(uid.to_string(), "req-settled".into(), outcome, now)
            .await
            .expect("the settle"));

        let again = daemon
            .answer(
                "req-settled",
                &card.payload_hash,
                AnswerDecision::OptionId {
                    option_id: "accept".into(),
                },
                Some(uid),
                protocol::ws::AnsweredOn::Iphone,
            )
            .await;

        let AnswerResult::Rejected { reason } = again else {
            panic!("a terminal claim may never actuate again: {again:?}");
        };
        reason
    }

    /// **A ledger replay says what the ledger actually recorded, and `lost` is not
    /// "an answer reached Codex".**
    ///
    /// `Settled` is the ledger's word for *terminal*, and it carries two very different
    /// outcomes. `delivered` is an answer the broker confirmed went upstream. `lost` is
    /// the opposite fact: that answer forwarded ZERO bytes, something else settled the
    /// request, and the card was left standing for that other terminal. Replaying both as
    /// "already answered" tells the operator their tap was applied when the whole record
    /// says it was not — and it is the sentence a second tap on a lost card gets, which is
    /// exactly the moment they are asking.
    ///
    /// The sentence is pinned whole: the claim stores no device, so any wording that names
    /// one is a guess. **Mutation:** put "a phone answer" back at the head of the `lost`
    /// sentence and the assertion goes red.
    #[tokio::test]
    async fn a_lost_answer_replays_as_a_loss_and_not_as_a_phone_answer() {
        let reason = second_answer_after_a_settled_claim(crate::store::ANSWER_LOST).await;
        assert_eq!(
            reason,
            "an earlier answer to this card lost the race — something else answered it \
             first — so nothing was sent again"
        );
    }

    /// **A second answer to a card that was already answered is not told a phone answered
    /// it.**
    ///
    /// Two devices answer one Codex card. The Mac app's answer is delivered first, and the
    /// iPhone's comes second. The claim the first answer left stores no device, so the
    /// refusal can say the card was answered and nothing was sent again, but not by whom:
    /// "from a phone" was a guess, wrong whenever the Mac app answered.
    ///
    /// The sentence is pinned whole, so a wording that names any device fails. **Mutation:**
    /// put "from a phone" (or "from the Mac app") in the `delivered` sentence and the
    /// assertion goes red.
    #[tokio::test]
    async fn a_delivered_answer_replays_without_naming_a_device() {
        let reason = second_answer_after_a_settled_claim(crate::store::ANSWER_DELIVERED).await;
        assert_eq!(
            reason,
            "this card was already answered; nothing was sent again"
        );
    }

    // ------------------------------------------------ stopping a turn from a phone

    /// The hash the phone computes and the daemon re-derives. Spelled through the
    /// production function, so a test cannot pass by agreeing with itself.
    fn interrupt_hash(session_ref: &str, turn_id: &str) -> String {
        protocol::hash::interrupt_hash(session_ref, turn_id)
    }

    /// **Every refusal that must happen before a claim, happens before a claim.**
    ///
    /// The ledger is the thing being protected. An interrupt that leaves an
    /// `applying` row behind is one a restart has to make terminal, and one the phone
    /// can never re-ask under the same id — so a refusal that wrote a row would turn
    /// "we did not send this" into "we may have sent this" for every case below.
    ///
    /// **Mutation:** move the agent branch below the claim and the ledger
    /// assertion at the end goes red.
    #[tokio::test]
    async fn every_interrupt_refusal_leaves_the_ledger_untouched() {
        use protocol::ws::InterruptResult;
        let (store, db) = shared_store_on_disk();
        let daemon = daemon_on(Arc::clone(&store), Config::default());

        let gemini = "01K1B3XQ8ZC0DE5FGH7JKMNGEM";
        seed_session(&daemon, gemini, "cc-gemini", Lifecycle::Live);
        // A row of the shape a later build writes, read by this one.
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE sessions SET agent = 'gemini' WHERE session_uid = ?1",
                [gemini],
            )
            .unwrap();
        seed_codex_session(&daemon, "01K1B3XQ8ZC0DE5FGH7JKMNCDX", "cc-1");

        let ledger_rows = || {
            store
                .unsettled_claims(crate::store::OPERATION_INTERRUPT)
                .unwrap()
                .len()
        };

        // **A run some future build wrote.** "Not Claude" is not evidence of Codex.
        let refused = daemon
            .interrupt(
                "cc-gemini",
                "req-2",
                "turn-1",
                &interrupt_hash("cc-gemini", "turn-1"),
            )
            .await;
        let InterruptResult::Rejected { reason } = &refused else {
            panic!("an unsupported agent must be refused: {refused:?}");
        };
        assert!(reason.contains("gemini"), "the refusal names it: {reason}");

        // **A hash that does not bind the turn it names.** The phone saw one turn and
        // this request names another, which is the substitution the hash exists for.
        let refused = daemon
            .interrupt(
                "cc-1",
                "req-3",
                "turn-1",
                &interrupt_hash("cc-1", "turn-OTHER"),
            )
            .await;
        let InterruptResult::Rejected { reason } = &refused else {
            panic!("a mismatched hash must be refused: {refused:?}");
        };
        assert!(reason.contains("stale payload_hash"), "{reason}");

        // **No link at all** — an engine run whose link is not attached.
        let refused = daemon
            .interrupt("cc-1", "req-4", "turn-1", &interrupt_hash("cc-1", "turn-1"))
            .await;
        assert_eq!(
            refused,
            InterruptResult::Rejected {
                reason: crate::codex_refusals::ENGINE_NO_CONNECTION.into()
            }
        );

        assert_eq!(
            ledger_rows(),
            0,
            "not one of these refusals may leave a claim behind: a durable row is a \
             record that something was actuated, and nothing was"
        );
    }

    /// **A duplicate interrupt replays what the first one recorded, and sends
    /// nothing.**
    ///
    /// The ledger's whole job. Each of the three settled outcomes reads back
    /// differently, because they are three different things to have happened and a
    /// phone told "already stopped" about a turn that ended on its own has been told
    /// something false.
    ///
    /// **Mutation:** make the `Settled` arm return `Duplicate` for every outcome and
    /// the `turn_ended`/`refused` cases stop being refusals — an operator would be
    /// told their tap worked when the record says it did not.
    #[tokio::test]
    async fn a_duplicate_interrupt_replays_the_recorded_outcome() {
        use protocol::ws::InterruptResult;
        let (store, _db) = shared_store_on_disk();
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        let uid = TEST_UID;
        seed_codex_session(&daemon, uid, "cc-1");

        // **Staged as the daemon itself would write it**, which means the checksum is
        // over the RUN and not over the name the phone used to reach it: a row seeded
        // with the caller's own spelling would be material this daemon never produces,
        // and the replay it is here to test would compare against a fiction.
        let settle = |request_id: &str, outcome: &'static str| {
            let material = crate::store::ClaimedMaterial {
                thread_id: "th-1".into(),
                generation: 2,
                route: crate::store::INTERRUPT_ROUTE.into(),
                target_turn_id: Some("turn-1".into()),
                claimed_hash: interrupt_hash(uid, "turn-1"),
                question_decision: None,
            };
            let now = protocol::time::now_rfc3339();
            assert_eq!(
                store
                    .claim_mutation(
                        crate::store::OPERATION_INTERRUPT,
                        uid,
                        request_id,
                        &material,
                        &now
                    )
                    .unwrap(),
                crate::store::MutationClaim::Claimed
            );
            assert!(store
                .settle_mutation(
                    crate::store::OPERATION_INTERRUPT,
                    uid,
                    request_id,
                    outcome,
                    &now
                )
                .unwrap());
        };

        // The turn stopped. Asking again is a duplicate, not a refusal: the thing the
        // operator wanted has happened.
        settle("req-aborted", crate::store::INTERRUPT_ABORTED);
        assert_eq!(
            daemon
                .interrupt(
                    "cc-1",
                    "req-aborted",
                    "turn-1",
                    &interrupt_hash("cc-1", "turn-1")
                )
                .await,
            InterruptResult::Duplicate {
                turn_id: "turn-1".into()
            }
        );

        // The turn ended on its own. Nothing was stopped, and the sentence says so.
        settle("req-ended", crate::store::INTERRUPT_TURN_ENDED);
        let replayed = daemon
            .interrupt(
                "cc-1",
                "req-ended",
                "turn-1",
                &interrupt_hash("cc-1", "turn-1"),
            )
            .await;
        let InterruptResult::Rejected { reason } = &replayed else {
            panic!("a turn that ended on its own was not stopped from here: {replayed:?}");
        };
        assert!(reason.contains("ended on its own"), "{reason}");

        // The write was refused. Same again: nothing was stopped.
        settle("req-refused", crate::store::INTERRUPT_REFUSED);
        let replayed = daemon
            .interrupt(
                "cc-1",
                "req-refused",
                "turn-1",
                &interrupt_hash("cc-1", "turn-1"),
            )
            .await;
        let InterruptResult::Rejected { reason } = &replayed else {
            panic!("a refused write is not a stop: {replayed:?}");
        };
        assert!(reason.contains("refused"), "{reason}");

        // A claim nobody settled — the shape a daemon killed after the write leaves —
        // is terminal and never sent again.
        let material = crate::store::ClaimedMaterial {
            thread_id: "th-1".into(),
            generation: 2,
            route: crate::store::INTERRUPT_ROUTE.into(),
            target_turn_id: Some("turn-1".into()),
            claimed_hash: interrupt_hash(uid, "turn-1"),
            question_decision: None,
        };
        store
            .claim_mutation(
                crate::store::OPERATION_INTERRUPT,
                uid,
                "req-unknown",
                &material,
                &protocol::time::now_rfc3339(),
            )
            .unwrap();
        store
            .settle_mutation_indeterminate(
                crate::store::OPERATION_INTERRUPT,
                uid,
                "req-unknown",
                &protocol::time::now_rfc3339(),
            )
            .unwrap();
        let replayed = daemon
            .interrupt(
                "cc-1",
                "req-unknown",
                "turn-1",
                &interrupt_hash("cc-1", "turn-1"),
            )
            .await;
        let InterruptResult::Indeterminate { reason } = &replayed else {
            panic!("an unprovable interrupt is never retried: {replayed:?}");
        };
        assert!(reason.contains("not known"), "{reason}");
    }

    /// **Every kind of claim the ledger has is made terminal at the next start.**
    ///
    /// The two kinds a start recovers, an interrupt and a compose, are written by
    /// different paths, so they are recovered by two functions rather than one
    /// loop. What they must not
    /// differ about is being recovered at all: a claim left `applying` is one no
    /// phone can ever re-ask under the same id, and the second kind spent a while
    /// with half a recovery precisely because "which kinds are covered" lived only in
    /// the reader's head.
    ///
    /// **Mutation:** remove either recovery call from the start path and this goes
    /// red, naming the kind that was dropped.
    #[tokio::test]
    async fn every_kind_of_claim_is_made_terminal_at_the_next_start() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let kinds = [
            crate::store::OPERATION_INTERRUPT,
            crate::store::OPERATION_COMPOSE,
        ];
        {
            let daemon = daemon_on(Arc::clone(&store), Config::default());
            seed_codex_session(&daemon, uid, "cc-1");
        }
        for kind in kinds {
            store
                .claim_mutation(
                    kind,
                    uid,
                    &format!("req-{kind}"),
                    &crate::store::ClaimedMaterial {
                        thread_id: "th-1".into(),
                        generation: 1,
                        route: "accept".into(),
                        target_turn_id: Some("turn-1".into()),
                        claimed_hash: "h".into(),
                        question_decision: None,
                    },
                    &protocol::time::now_rfc3339(),
                )
                .unwrap();
        }

        let daemon = daemon_on(Arc::clone(&store), Config::default());
        daemon.recover().await;

        for kind in kinds {
            assert_eq!(
                store
                    .mutation_status(kind, uid, &format!("req-{kind}"))
                    .unwrap()
                    .map(|state| state.status),
                Some(crate::store::AnswerStatus::Indeterminate),
                "a {kind} claim left in flight must be terminal after a restart"
            );
            assert!(
                store.unsettled_claims(kind).unwrap().is_empty(),
                "and no {kind} claim is left for a later start to reason about"
            );
        }
    }

    /// **One id, two turns: a conflict, and nothing is sent.**
    ///
    /// A phone reuses a request id — a retry counter that wrapped, a client that
    /// mints one per screen rather than per ask — and the second ask names the turn
    /// that is running NOW. The ledger's own law is that a duplicate replays the
    /// material the first attempt was claimed with; a reply that reads the outcome
    /// column alone cannot apply that law, and the answer it gives is the worst one
    /// available: `Duplicate` naming the running turn, which tells the operator the
    /// turn in front of them has already been stopped when nothing has touched it.
    ///
    /// The comparison is over what the CLIENT bound — the turn it named and the hash
    /// binding this ask to it. The visit generation is the daemon's own bookkeeping
    /// and stays a live pre-write check, because a person who reused an id has not
    /// done anything wrong about a visit they cannot see.
    ///
    /// **Mutation:** compare the outcome alone and the second ask becomes a
    /// `Duplicate` of a turn nobody stopped.
    #[tokio::test]
    async fn a_settled_interrupt_id_naming_another_turn_is_a_conflict_not_a_duplicate() {
        use protocol::ws::InterruptResult;
        let (store, _db) = shared_store_on_disk();
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        let uid = TEST_UID;
        seed_codex_session(&daemon, uid, "cc-1");
        let (answers, _) = crate::codex_host::answer_channel();
        let (commands, mut asks) = tokio::sync::mpsc::unbounded_channel();
        daemon
            .host_link_attached(uid, HostLink::Codex(answers, commands))
            .await;

        // `req-1` stopped turn-1, and that is durably recorded.
        let material = crate::store::ClaimedMaterial {
            thread_id: "th-1".into(),
            generation: 2,
            route: crate::store::INTERRUPT_ROUTE.into(),
            target_turn_id: Some("turn-1".into()),
            claimed_hash: interrupt_hash(uid, "turn-1"),
            question_decision: None,
        };
        let now = protocol::time::now_rfc3339();
        store
            .claim_mutation(
                crate::store::OPERATION_INTERRUPT,
                uid,
                "req-1",
                &material,
                &now,
            )
            .unwrap();
        assert!(store
            .settle_mutation(
                crate::store::OPERATION_INTERRUPT,
                uid,
                "req-1",
                crate::store::INTERRUPT_ABORTED,
                &now
            )
            .unwrap());

        // The same id, aimed at turn-2, with turn-2's own valid hash.
        let reused = daemon
            .interrupt("cc-1", "req-1", "turn-2", &interrupt_hash("cc-1", "turn-2"))
            .await;
        let InterruptResult::Rejected { reason } = &reused else {
            panic!(
                "turn-2 was never stopped, so nothing may report it as already \
                 stopped: {reused:?}"
            );
        };
        assert!(
            reason.contains("different turn"),
            "the operator is told which of the two facts is wrong: {reason}"
        );
        assert!(
            asks.try_recv().is_err(),
            "a conflict is decided from the record, so nothing reaches the link"
        );

        // And the honest duplicate still replays, so the conflict is not a blanket
        // refusal of every retry.
        assert_eq!(
            daemon
                .interrupt("cc-1", "req-1", "turn-1", &interrupt_hash("cc-1", "turn-1"))
                .await,
            InterruptResult::Duplicate {
                turn_id: "turn-1".into()
            }
        );
        assert!(
            asks.try_recv().is_err(),
            "and it too is decided from the record"
        );
    }

    /// **A claim this daemon was in the middle of when it stopped becomes terminal at
    /// the next start, and is never sent again.**
    ///
    /// The kill-after-write shape, without a kill: an `applying` row is exactly what a
    /// process death between the claim and the terminal leaves, and what recovery does
    /// with it is the whole contract.
    ///
    /// **Mutation:** drop the `recover_codex_interrupts` call from the start path and
    /// the row stays `applying` — a phone could then be told "not applied" for an
    /// interrupt that may well have stopped the turn.
    #[tokio::test]
    async fn an_interrupt_in_flight_across_a_restart_becomes_one_terminal_unknown() {
        use protocol::ws::InterruptResult;
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        {
            let daemon = daemon_on(Arc::clone(&store), Config::default());
            seed_codex_session(&daemon, uid, "cc-1");
        }
        let material = crate::store::ClaimedMaterial {
            thread_id: "th-1".into(),
            generation: 2,
            route: crate::store::INTERRUPT_ROUTE.into(),
            target_turn_id: Some("turn-1".into()),
            claimed_hash: interrupt_hash(uid, "turn-1"),
            question_decision: None,
        };
        store
            .claim_mutation(
                crate::store::OPERATION_INTERRUPT,
                uid,
                "req-1",
                &material,
                &protocol::time::now_rfc3339(),
            )
            .unwrap();
        assert_eq!(
            store
                .unsettled_claims(crate::store::OPERATION_INTERRUPT)
                .unwrap()
                .len(),
            1
        );

        // The next daemon start.
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        daemon.recover().await;
        assert!(
            store
                .unsettled_claims(crate::store::OPERATION_INTERRUPT)
                .unwrap()
                .is_empty(),
            "recovery must leave no claim for a later start to reason about"
        );
        assert_eq!(
            store
                .mutation_status(crate::store::OPERATION_INTERRUPT, uid, "req-1")
                .unwrap()
                .map(|state| state.status),
            Some(crate::store::AnswerStatus::Indeterminate)
        );

        let replayed = daemon
            .interrupt("cc-1", "req-1", "turn-1", &interrupt_hash("cc-1", "turn-1"))
            .await;
        let InterruptResult::Indeterminate { reason } = &replayed else {
            panic!("a recovered claim is terminal and never retried: {replayed:?}");
        };
        assert!(reason.contains("will not be sent again"), "{reason}");
    }

    /// **An interrupt is recovered even when the approval cards cannot be read.**
    ///
    /// The answer recovery genuinely depends on the cards being back: settling an
    /// answer retires its card, and retiring one the restore never read files a
    /// resolution against something that is not there. An interrupt has no card, so
    /// none of that reasoning is about it — and standing inside the same gate bought
    /// nothing while costing the phone a whole extra restart before it could be told
    /// what became of its tap, and before the id could be used again.
    ///
    /// The fault is a real one rather than an injected flag: a second connection to
    /// the same file takes the approvals table away, which is the shape a disk error
    /// or a half-applied migration has at this boundary.
    ///
    /// **Mutation:** put the interrupt recovery back inside `if cards_are_back` and
    /// this goes red, with the claim still applying.
    #[tokio::test]
    async fn an_interrupt_is_recovered_even_when_the_cards_cannot_be_read() {
        let (store, db_path) = shared_store_on_disk();
        let uid = TEST_UID;
        {
            let daemon = daemon_on(Arc::clone(&store), Config::default());
            seed_codex_session(&daemon, uid, "cc-1");
        }
        store
            .claim_mutation(
                crate::store::OPERATION_INTERRUPT,
                uid,
                "req-1",
                &crate::store::ClaimedMaterial {
                    thread_id: "th-1".into(),
                    generation: 2,
                    route: crate::store::INTERRUPT_ROUTE.into(),
                    target_turn_id: Some("turn-1".into()),
                    claimed_hash: interrupt_hash(uid, "turn-1"),
                    question_decision: None,
                },
                &protocol::time::now_rfc3339(),
            )
            .unwrap();

        // Take the cards away. The restore this start makes will fail, and the answer
        // recovery behind it will correctly decline to run.
        rusqlite::Connection::open(&db_path)
            .expect("the same file")
            .execute_batch("DROP TABLE pending_approvals;")
            .expect("the table exists until now");

        let daemon = daemon_on(Arc::clone(&store), Config::default());
        daemon.recover().await;

        assert_eq!(
            store
                .mutation_status(crate::store::OPERATION_INTERRUPT, uid, "req-1")
                .unwrap()
                .map(|state| state.status),
            Some(crate::store::AnswerStatus::Indeterminate),
            "an interrupt has no card, so a failed card read decides nothing about it"
        );
    }

    /// **The two Claude-only operations are refused for a Codex run, by the agent and
    /// not by an accident.**
    ///
    /// The catalog's old refusal was a missing `claude_bin`, which is a fact about a
    /// registration field; the terminal had no agent check at all. Both now read the
    /// row and say what they found.
    ///
    /// **Mutation:** delete the `refuse_unless_claude` call from `command_catalog` and
    /// the reason reverts to the binary sentence — which this asserts is gone.
    #[tokio::test]
    async fn the_claude_only_operations_are_refused_for_a_codex_run() {
        let (store, _db) = shared_store_on_disk();
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        let uid = TEST_UID;
        seed_codex_session(&daemon, uid, "cc-1");

        let catalog = daemon.command_catalog("cc-1").await;
        let protocol::ws::CommandCatalogResult::Unavailable { reason } = &catalog else {
            panic!("a Codex run has no Claude to ask: {catalog:?}");
        };
        assert!(
            reason.contains("Codex session"),
            "the refusal names the agent, not a missing field: {reason}"
        );
        assert!(
            !reason.contains("Claude Code binary"),
            "the old accidental refusal described a registration field rather than the \
             truth: {reason}"
        );

        // The gate the terminal attach reads, asked directly — the attach path itself
        // is driven over a real WebSocket in `ws_server`'s own suite.
        let crate::state::ClaudeOnly::WrongAgent(refused) =
            daemon.refuse_unless_claude(uid, "the terminal").await
        else {
            panic!("a Codex run may not be typed into through a terminal")
        };
        assert!(refused.contains("Codex session"), "{refused}");
        assert!(refused.contains("terminal"), "{refused}");

        // **A reference this daemon cannot vouch for is refused, not admitted.** This
        // is the gate's whole contract, and the terminal attach is the one call site
        // with no later existence check behind it — everything after that line takes
        // the lease and opens a client. A row the store does not have is not evidence
        // that the run is Claude's.
        //
        // **Mutation:** admit `Ok(None)` and this goes red.
        assert!(
            matches!(
                daemon
                    .refuse_unless_claude("a-run-this-mac-has-never-heard-of", "the terminal")
                    .await,
                crate::state::ClaudeOnly::Unknown(_)
            ),
            "an unreadable or absent row fails closed"
        );

        // And a Claude run is untouched by the gate.
        //
        // **The uid is a well-formed one, and that is load-bearing here.** `L` is not
        // in the ULID alphabet, so the id this leg used to carry was never a uid the
        // resolver would look up: it fell through to a name lookup that matched
        // nothing, and the gate answered about a row it had not found. That answer
        // happened to be the one the assertion wanted, so the leg was green while
        // proving nothing — which is the same fail-open the gate itself had.
        let claude = "01K1B3XQ8ZC0DE5FGH7JKMNCJ2";
        seed_session(&daemon, claude, "cc-claude", Lifecycle::Live);
        assert_eq!(
            daemon.refuse_unless_claude(claude, "the terminal").await,
            crate::state::ClaudeOnly::Admitted,
            "the gate is about the agent and nothing else"
        );
    }

    /// **The two free-text takeovers refuse each other's agent, by name.**
    ///
    /// `send_text` types at the Mac's TTY and is Claude's; `compose` speaks the
    /// app-server's own `turn/start`/`turn/steer` and is Codex's. Neither silently does
    /// the other's job, and each refusal names the one that works — which is the
    /// difference between an operator who knows what to press next and one who does not.
    ///
    /// Every refusal here is taken BEFORE any durable claim, so a refused ask leaves the
    /// ledger empty. That is asserted rather than assumed: the whole reason the interrupt
    /// has a local gate is a live run that left a row recording an actuation that never
    /// happened.
    ///
    /// **Mutation:** delete the `AgentKind::Claude` arm from `Daemon::compose` and the
    /// first half goes red — a Claude run would fall through to the link lookup and be
    /// refused for having no Codex link, which is a true sentence about the wrong thing.
    #[tokio::test]
    async fn compose_is_codexs_and_send_text_is_claudes_and_each_refuses_the_other() {
        let (store, _db) = shared_store_on_disk();
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        let codex = TEST_UID;
        seed_codex_session(&daemon, codex, "cc-1");

        let claude = "01K1B3XQ8ZC0DE5FGH7JKMNCJ3";
        seed_session(&daemon, claude, "cc-claude", Lifecycle::Live);

        // A Claude session refuses the Codex compose, and says where its text goes.
        let text = "please stop and summarise".to_string();
        let refused = daemon
            .compose(
                claude,
                "c-1",
                text.clone(),
                &protocol::hash::compose_hash(claude, &text),
                0,
                &[],
            )
            .await;
        let protocol::ws::ComposeResult::Rejected { reason } = &refused else {
            panic!("a Claude session has no Codex thread to speak to: {refused:?}")
        };
        assert!(reason.contains("Claude session"), "{reason}");
        assert!(
            reason.contains("send_text"),
            "the refusal names the operation that works: {reason}"
        );

        // And the Codex session refuses the Claude takeover, which is the same statement
        // read from the other end.
        let typed = daemon
            .send_text(codex, "hello".into(), None, None, true, 0, &[])
            .await;
        let protocol::ws::SendTextResult::Refused { reason } = &typed else {
            panic!("a Codex session is not typed into: {typed:?}")
        };
        assert!(
            reason.contains("only sends messages to Claude sessions"),
            "{reason}"
        );

        // **A refused compose leaves the ledger empty.** Both of these are taken before
        // any claim: the agent branch above, and the link lookup below for a Codex run
        // with no live link.
        let none = daemon
            .compose(
                codex,
                "c-2",
                text.clone(),
                &protocol::hash::compose_hash(codex, &text),
                0,
                &[],
            )
            .await;
        assert_eq!(
            none,
            protocol::ws::ComposeResult::Rejected {
                reason: crate::codex_refusals::ENGINE_NO_CONNECTION.into()
            },
            "a Codex run with no link cannot be spoken to"
        );
        for (uid, id) in [(claude, "c-1"), (codex, "c-2")] {
            assert_eq!(
                store
                    .mutation_status(crate::store::OPERATION_COMPOSE, uid, id)
                    .unwrap(),
                None,
                "a refusal taken before the claim leaves no row: {uid}/{id}"
            );
        }

        // The checksum, and the ceiling. Both are refusals about the request rather than
        // about the session, and both are taken before the link is even consulted.
        let bad_hash = daemon
            .compose(codex, "c-3", text.clone(), "not-the-hash", 0, &[])
            .await;
        assert!(matches!(
            bad_hash,
            protocol::ws::ComposeResult::Rejected { .. }
        ));
        let huge = "x".repeat(protocol::ws::MAX_COMPOSE_BYTES + 1);
        let too_long = daemon
            .compose(
                codex,
                "c-4",
                huge.clone(),
                &protocol::hash::compose_hash(codex, &huge),
                0,
                &[],
            )
            .await;
        let protocol::ws::ComposeResult::Rejected { reason } = &too_long else {
            panic!("a message over the ceiling is refused: {too_long:?}")
        };
        assert!(reason.contains("ceiling"), "{reason}");
        // An empty message says nothing and is refused as such.
        let empty = daemon
            .compose(
                codex,
                "c-5",
                String::new(),
                &protocol::hash::compose_hash(codex, ""),
                0,
                &[],
            )
            .await;
        assert!(matches!(
            empty,
            protocol::ws::ComposeResult::Rejected { .. }
        ));
    }

    /// **A Codex message's attachments are claimed before anything is said** (minor 50). Files
    /// alone are not an empty message; a file this computer does not hold, or another
    /// connection's, refuses the message before the link is looked for, and a refused message
    /// leaves the ledger empty.
    #[tokio::test]
    async fn a_codex_message_claims_its_files_before_it_is_sent() {
        let (store, _db) = shared_store_on_disk();
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        let codex = "01K1B3XQ8ZC0DE5FGH7JKMNCP4";
        seed_codex_session(&daemon, codex, "cc-1");
        let root = crate::uploads::root();
        let compose = |id: &'static str, connection: u64, ids: Vec<String>| {
            let daemon = Arc::clone(&daemon);
            async move {
                let hash = protocol::hash::compose_hash_with(codex, "", &ids);
                daemon
                    .compose(codex, id, String::new(), &hash, connection, &ids)
                    .await
            }
        };

        let unknown = compose("c-1", 3, vec!["u-none".into()]).await;
        let protocol::ws::ComposeResult::Rejected { reason } = &unknown else {
            panic!("{unknown:?}")
        };
        assert!(reason.contains("u-none"), "{reason}");

        assert_eq!(
            daemon
                .uploads
                .put(&root, 3, codex, "u-1", "a.txt", 1, 0, b"a"),
            Ok(1)
        );
        let theirs = compose("c-2", 4, vec!["u-1".into()]).await;
        let protocol::ws::ComposeResult::Rejected { reason } = &theirs else {
            panic!("{theirs:?}")
        };
        assert!(reason.contains("another connection"), "{reason}");

        // Its own file, with no words: past the claim, to the link this run does not have.
        let own = compose("c-3", 3, vec!["u-1".into()]).await;
        assert_eq!(
            own,
            protocol::ws::ComposeResult::Rejected {
                reason: crate::codex_refusals::ENGINE_NO_CONNECTION.into()
            }
        );
        for id in ["c-1", "c-2", "c-3"] {
            assert_eq!(
                store
                    .mutation_status(crate::store::OPERATION_COMPOSE, codex, id)
                    .unwrap(),
                None,
                "{id}"
            );
        }
        // Claimed: the connection's close leaves it, the run's removal takes it.
        daemon.uploads.close_connection(&root, 3);
        assert!(root.join(codex).join("u-1").join("a.txt").exists());
        daemon.uploads.remove_session(&root, codex);
        assert!(!root.join(codex).exists());
    }

    /// **A finished compose is replayed while the link is gone, and the link's absence is
    /// not the answer.**
    ///
    /// The phone that retries an id is, almost always, the phone that never heard the first
    /// answer — and the reason it never heard it is usually that the link went down. So the
    /// state this test puts the daemon in is not a corner: it is the ordinary one in which
    /// a retry happens. What the Mac wrote down about a mutation that already ran outranks
    /// what its socket is doing right now.
    ///
    /// Four rows, all with the same dead link:
    ///
    ///   * a settled row with the same words replays the turn that heard them;
    ///   * a settled row reached with DIFFERENT words is the conflict, named plainly,
    ///     because replaying the first turn's id would tell somebody their new message
    ///     reached the model when it never left this machine;
    ///   * an unsettled `applying` claim with the same words is reported unknown, as
    ///     startup records it, and never sent again;
    ///   * and no row at all is the sentence about the missing connection.
    ///
    /// **Mutation:** drop the [`Daemon::settled_compose`] call and each settled row
    /// becomes the sentence about the missing connection — safe, and false.
    #[tokio::test]
    async fn a_finished_compose_is_replayed_even_when_the_link_is_gone() {
        let (store, _db) = shared_store_on_disk();
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        let uid = TEST_UID;
        seed_codex_session(&daemon, uid, "cc-1");

        let said = "say the word amber".to_string();
        let hash = protocol::hash::compose_hash(uid, &said);
        let now = protocol::time::now_rfc3339();
        let claim = |request_id: &str, claimed_hash: &str| {
            store
                .claim_mutation(
                    crate::store::OPERATION_COMPOSE,
                    uid,
                    request_id,
                    &crate::store::ClaimedMaterial {
                        thread_id: "th-1".into(),
                        generation: 1,
                        route: crate::store::COMPOSE_ROUTE_START.into(),
                        target_turn_id: None,
                        claimed_hash: claimed_hash.to_string(),
                        question_decision: None,
                    },
                    &now,
                )
                .unwrap()
        };

        // The baseline the fix has to preserve: nothing recorded, so the link is all there
        // is to say.
        let cold = daemon
            .compose(uid, "say-0", said.clone(), &hash, 0, &[])
            .await;
        assert_eq!(
            cold,
            protocol::ws::ComposeResult::Rejected {
                reason: crate::codex_refusals::ENGINE_NO_CONNECTION.into()
            },
            "an unrecorded ask with no link is refused"
        );

        // A compose that landed: claimed, then settled with the turn that heard it.
        claim("say-1", &hash);
        store
            .settle_mutation(
                crate::store::OPERATION_COMPOSE,
                uid,
                "say-1",
                &crate::store::compose_outcome(crate::store::COMPOSE_ROUTE_START, "turn-1"),
                &now,
            )
            .unwrap();
        assert_eq!(
            daemon
                .compose(uid, "say-1", said.clone(), &hash, 0, &[])
                .await,
            protocol::ws::ComposeResult::Duplicate {
                turn_id: "turn-1".into(),
                started: true,
            },
            "the record names the turn these words began, and the dead link does not \
             change that"
        );

        // A message that landed with a file, asked again by a daemon whose uploads hold none of
        // it, as after a restart: the record answers, not the missing file.
        let files = vec!["u-sent".to_string()];
        let with_files = protocol::hash::compose_hash_with(uid, &said, &files);
        claim("say-9", &with_files);
        store
            .settle_mutation(
                crate::store::OPERATION_COMPOSE,
                uid,
                "say-9",
                &crate::store::compose_outcome(crate::store::COMPOSE_ROUTE_START, "turn-9"),
                &now,
            )
            .unwrap();
        assert_eq!(
            daemon
                .compose(uid, "say-9", said.clone(), &with_files, 0, &files)
                .await,
            protocol::ws::ComposeResult::Duplicate {
                turn_id: "turn-9".into(),
                started: true,
            }
        );

        // The same id, different words. Not a duplicate — two mutations under one key.
        let other = "say the word umber instead".to_string();
        let conflict = daemon
            .compose(
                uid,
                "say-1",
                other.clone(),
                &protocol::hash::compose_hash(uid, &other),
                0,
                &[],
            )
            .await;
        let protocol::ws::ComposeResult::Rejected { reason } = &conflict else {
            panic!("an id reused for other words is refused, not replayed: {conflict:?}")
        };
        assert_eq!(reason, crate::codex_refusals::COMPOSE_ID_REUSED);

        // Written, and what became of it never learned.
        claim("say-2", &hash);
        store
            .settle_mutation_indeterminate(crate::store::OPERATION_COMPOSE, uid, "say-2", &now)
            .unwrap();
        let unknown = daemon
            .compose(uid, "say-2", said.clone(), &hash, 0, &[])
            .await;
        let protocol::ws::ComposeResult::Indeterminate { reason } = &unknown else {
            panic!("an unprovable compose is never re-sent: {unknown:?}")
        };
        assert_eq!(reason, crate::codex_refusals::COMPOSE_ALREADY_SENT_UNKNOWN);

        // An `applying` claim with no link: sent, and never sent again.
        claim("say-3", &hash);
        let in_flight = daemon
            .compose(uid, "say-3", said.clone(), &hash, 0, &[])
            .await;
        let protocol::ws::ComposeResult::Indeterminate { reason } = &in_flight else {
            panic!("an unsettled claim is not sent again: {in_flight:?}")
        };
        assert_eq!(reason, crate::codex_refusals::COMPOSE_ALREADY_SENT_UNKNOWN);
    }

    /// **Answering a Codex card writes nothing a rolled-back daemon can reach —
    /// on the refusing path AND on the succeeding one.**
    ///
    /// `shared_ledgers_admit` is a fork rather than a dead end, so the question
    /// this test asks is not "is a Codex answer refused" but "does a Codex
    /// answer, refused or not, stay out of the four
    /// tables a rolled-back v0.6.0 daemon rewrites globally". Both decisions are
    /// fired here for that reason — a `Deny` the Codex path has no words for, and
    /// an `option_id` it does — and the tripwire has to stay silent for both.
    ///
    /// The tripwire, not a row count, for the reason [`arm_ledger_tripwire`]
    /// gives: a path that inserts and then cleans up leaves the same zero behind
    /// as one that never inserted, and a daemon killed in that window does not.
    ///
    /// **Mutation:** point the answer path's durable claim at `claim_answer`
    /// instead of `claim_mutation` — or read `answers` instead of the mutation
    /// ledger in step 1 — and `assert_no_insert_was_attempted` goes red.
    ///
    /// **It fires on the second call, and only there, which is a property of the
    /// staging rather than a weakness in it.** The first call carries a decision
    /// the Codex path has no words for, so it is refused by the grammar check
    /// before any claim is reached and touches nothing either way — that half is
    /// asserting the refusal happens *before* the ledger, which is its own claim.
    /// The `option_id` call is the one that walks the whole path (ledger read,
    /// stored card, hash gate, option table, claimed material, ask), so it is the
    /// one a widened fork would show up in.
    #[tokio::test]
    async fn answering_a_codex_card_is_refused_before_the_claim() {
        let (store, db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        // Begun as the engine begins a run. The card is still placed by hand below;
        // what beginning the run buys is that `answer`'s gate is reading the agent off
        // a row this daemon produced.
        seed_codex_session(&daemon, uid, "cc-1");
        arm_ledger_tripwire(&db);

        // The card is placed in memory by hand: `answer` has to hold for a card
        // that got into memory some other way — a `pending_approvals` projection
        // written by a build without this scaffolding and restored by `recover`,
        // or a Codex producer.
        let request_id = "toolu_codex_1";
        let tool_input = json!({ "command": "touch /tmp/a" });
        let payload_hash = approval_payload_hash("Bash", &tool_input);
        let card = ApprovalCard {
            request_id: request_id.to_string(),
            payload_hash: payload_hash.clone(),
            tool_name: "Bash".into(),
            tool_input: tool_input.clone(),
            display_text: approval_payload_text("Bash", &tool_input),
            permission_suggestions: None,
            prompt_id: Some("p1".into()),
            permission_mode: None,
            risk: None,
            generation: 1,
            identity_bound: false,
            question_hold: None,
            allow_for_session: false,
        };
        daemon.inner.lock().await.pending.insert(
            (uid.to_string(), request_id.to_string()),
            PendingApproval {
                card,
                session: SessionKey::new(uid, "cc-1"),
                // The tripwire's whole point is a Codex card that got into
                // memory some other way, so it is a Codex card here too.
                agent: protocol::agent::AgentKind::Codex,
                raise: 0,
                claimed: false,
                tool_ran: false,
                project_label: "cc-1".into(),
                fork: false,
                link: false,
                question: false,
            },
        );

        let result = daemon
            .answer(
                request_id,
                &payload_hash,
                AnswerDecision::Allow,
                Some(uid),
                protocol::ws::AnsweredOn::Iphone,
            )
            .await;

        match &result {
            AnswerResult::Rejected { reason } => assert!(
                reason.contains("naming one of the options") && reason.contains("nothing was sent"),
                "a Codex card names its options; a bare allow/deny is not one of them: {reason}"
            ),
            other => panic!("a decision this card never offered must not be sent: {other:?}"),
        }
        assert_no_ledger_rows(&db, uid);
        assert_no_insert_was_attempted(&db, uid);

        // **And the decision the Codex path DOES accept, on a card the producer
        // really raised.**
        //
        // A hand-placed card would refuse at the store lookup — "unknown or
        // already-resolved request" — which is the honest answer for a request no
        // Codex card stands behind, and is exactly why it proves nothing here: the
        // shared ledgers are never reached because the function returns before
        // them. So this half raises a real card through the observer's own
        // producer, and the answer runs the whole validating path on it: the
        // ledger read, the store card, the hash gate, the option table, the
        // claimed material, and the ask. Every one of those is a place a widened
        // fork would have touched a shared table, and the tripwire has to stay
        // silent through all of it.
        assert!(raise_codex_card(&daemon, uid, "derived-tripwire", "exec-tripwire").await);
        let real = open_cards(&daemon, uid)
            .into_iter()
            .find(|row| row.request_id == "derived-tripwire")
            .expect("the producer filed the card");
        let real_card: ApprovalCard =
            serde_json::from_str(&real.card).expect("the stored card decodes");
        let by_option = daemon
            .answer(
                &real.request_id,
                &real_card.payload_hash,
                AnswerDecision::OptionId {
                    option_id: "accept".into(),
                },
                Some(uid),
                protocol::ws::AnsweredOn::Iphone,
            )
            .await;
        match &by_option {
            // Refused at the link, which is the last gate on the path and the
            // furthest a daemon with no live connection can get. What matters is
            // where it got to before it was refused, and the two assertions below
            // are what say so.
            AnswerResult::Rejected { reason } => assert!(
                reason.contains("nothing was sent"),
                "the refusal must say that nothing went out: {reason}"
            ),
            other => panic!("with no connection to write on, nothing can be sent: {other:?}"),
        }
        assert_no_ledger_rows(&db, uid);
        assert_no_insert_was_attempted(&db, uid);
    }

    /// Raise one ordinary Codex command card, the way the link does.
    async fn raise_codex_card(
        daemon: &Arc<Daemon>,
        uid: &str,
        request_id: &str,
        item_id: &str,
    ) -> bool {
        let approval = crate::codex_approval::Approval::read(
            crate::codex_approval::Family::Command,
            &json!({
                "threadId": "th-1",
                "turnId": "tu-1",
                "itemId": item_id,
                "environmentId": "local",
                "command": "/bin/zsh -lc 'touch /tmp/a'",
                "cwd": "/work",
                "availableDecisions": ["accept", "cancel"],
            }),
            None,
        )
        .expect("a command approval");
        let card = approval.card(request_id.to_string(), 1);
        daemon
            .raise_codex_approval(
                &SessionKey::new(uid, "cc-1"),
                card,
                &approval.thread_id,
                &approval.turn_id,
                &approval.item_id,
                "commandExecution",
            )
            .await
    }

    /// **A Codex approval's two events carry what the phone needs to act on the
    /// card: which turn it belongs to, and which card the terminal ends.**
    ///
    /// Neither fact was on the wire, and each hole had a different bad answer.
    ///
    ///   * **The turn.** `ClientMessage::Interrupt` requires a `turn_id` and the
    ///     card event carried none, so a Stop offered from a card had to name a turn
    ///     read off some *earlier* event's envelope. "The approval always follows its
    ///     item's `tool_call`" is an ordering the phone would have been relying on,
    ///     not a contract — and it is false for a link that rebound mid-turn, which
    ///     cards a command whose `item/started` it never saw.
    ///   * **The card.** The resolution payload named nothing, so the only
    ///     correlation was stripping `"resolved:"` off `source_event_id`. A prefix
    ///     parse that misses is silent, and its symptom is an answered card standing
    ///     on somebody's phone for ever.
    ///
    /// Driven through the production path — the observer's own `raise_codex_approval`
    /// and the terminal's own `retire_codex_approval` — so what is asserted is what a
    /// subscriber is sent, not what a struct can be built to look like.
    ///
    /// **Mutation:** drop the `with_turn_id` in `raise_codex_approval` and the first
    /// half goes red; serialize the bare `CodexResolution` again in
    /// `retire_codex_answered` and the second does.
    #[tokio::test]
    async fn a_codex_approval_names_its_turn_and_its_terminal_names_its_card() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");
        assert!(raise_codex_card(&daemon, uid, "derived-1", "exec-1").await);

        let raised = approval_events(&daemon, uid, EventKind::ApprovalRequest);
        assert_eq!(raised.len(), 1, "one card, one request event");
        assert_eq!(
            raised[0].turn_id.as_deref(),
            Some("tu-1"),
            "the turn the approval's own request named, on the envelope field every \
             other Codex event carries it on — this is what a Stop from the card names"
        );

        assert_eq!(
            daemon
                .retire_codex_approval(
                    &SessionKey::new(uid, "cc-1"),
                    "derived-1",
                    protocol::ws::CodexResolution::Cleared {
                        cause: protocol::ws::ClearCause::ItemCompleted,
                    },
                )
                .await,
            Retirement::Retired
        );
        let resolved = approval_events(&daemon, uid, EventKind::ApprovalResolved);
        assert_eq!(resolved.len(), 1, "one terminal, one resolution event");
        assert_eq!(
            resolved[0].payload["request_id"].as_str(),
            Some("derived-1"),
            "the payload names the card, where Claude's `AnswerOutcome` has named \
             one since minor 0"
        );
        assert_eq!(
            resolved[0].payload["request_id"].as_str(),
            raised[0].payload["card"]["request_id"].as_str(),
            "and it is the SAME id the card was raised under — the whole point of \
             the field is that a phone joins the two without parsing a string"
        );

        // **Additive, proven on the daemon's own output.** The payload still decodes
        // as a bare `CodexResolution` — which is what every reader in this crate, and
        // any phone written against minor 18, does with it.
        assert_eq!(
            serde_json::from_value::<protocol::ws::CodexResolution>(resolved[0].payload.clone())
                .expect("a minor-18 decoder still reads this payload"),
            protocol::ws::CodexResolution::Cleared {
                cause: protocol::ws::ClearCause::ItemCompleted,
            }
        );
        // The prefix parse the field replaces is still there, and still agrees. It is
        // asserted rather than removed: a client that already ships it must not be
        // broken by the addition.
        assert_eq!(
            resolved[0].source_event_id.as_deref(),
            Some("resolved:derived-1")
        );
    }

    /// **Every sentence a phone can be told a Codex stop or message did not
    /// happen, as this build says it.**
    ///
    /// `fixtures/codex/refusal-sentences.json` exists because the phone reads
    /// these sentences with `String.contains`. `InterruptResult::Rejected` is a
    /// bare reason on the wire — there is no code — so the iOS side decides
    /// whether to grey Stop and Compose for a few seconds by matching the
    /// clauses that describe a link which is coming back. That match is
    /// correct for every sentence today and would go silently wrong the first
    /// time somebody reworded one: nothing on this side would fail, because the
    /// daemon would still be saying something true.
    ///
    /// So [`crate::codex_refusals`] is where each sentence is written, exactly
    /// once, and this test asserts what the phone reads against the
    /// **committed** bytes, then re-derives those bytes from this build and
    /// requires them to be identical. `ios/` cannot be compiled here, so the
    /// file is the only place the two sides can meet.
    ///
    /// **Nothing is normalised, because nothing in it is read from anything.**
    /// The interpolated sentences are assembled by calling the very formatters
    /// production calls, with the fixture's own `{uid}`/`{n}` tokens as their
    /// arguments — so a template here cannot drift from the code, there being no
    /// second copy of it to drift.
    ///
    /// **There is no `{err}` token, and there must never be one again.** Five
    /// sentences were assembled from this Mac's own store error — an anyhow chain
    /// whose open paths name absolute filesystem locations — and a phone read the
    /// lot. What this file adds is that the phone's own splitter stops carrying a
    /// token that can no longer appear.
    ///
    /// **Mutation:** edit one sentence in the checked-in file and the byte
    /// comparison fails, naming the file and the command that regenerates it.
    #[test]
    fn the_refusal_sentences_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/codex/refusal-sentences.json");
        const REGENERATE: &str =
            "cargo test -p ccd --bin ccd -- --ignored regenerate_the_refusal_sentences_fixture";
        let committed: serde_json::Value =
            serde_json::from_str(FIXTURE).expect("the fixture must be JSON");

        // Gate one: what the phone matches on, asserted on the committed bytes.
        let sentences = committed["sentences"]
            .as_array()
            .expect("the sentences are a list");
        let mut link_state = Vec::new();
        for row in sentences {
            for key in ["id", "verb", "outcome", "category", "text"] {
                assert!(
                    row[key].as_str().is_some_and(|v| !v.is_empty()),
                    "every row carries a non-empty `{key}`: {row}"
                );
            }
            assert!(
                matches!(row["verb"].as_str(), Some("interrupt" | "compose" | "both")),
                "`verb` names a control the phone offers: {row}"
            );
            assert!(
                matches!(row["outcome"].as_str(), Some("rejected" | "indeterminate")),
                "`outcome` is the wire variant the reason rides in: {row}"
            );
            assert!(
                matches!(
                    row["category"].as_str(),
                    Some("link_state" | "permanent" | "transient_local" | "wire_code")
                ),
                "`category` is one this file documents: {row}"
            );
            if row["category"] == json!("link_state") {
                link_state.push(row["text"].as_str().expect("a text").to_string());
            }
        }
        assert_eq!(
            committed["counts"]["total"].as_u64(),
            Some(sentences.len() as u64),
            "the count the phone reads is the number of rows it gets"
        );
        // **The link-state family is named, not merely counted.** It is the run's
        // link not being attached, and the phone's whole grey-for-ten-seconds rule
        // is a statement about exactly this set. A sentence joining or leaving it
        // is the change worth seeing in a diff.
        assert_eq!(
            link_state,
            [crate::codex_refusals::ENGINE_NO_CONNECTION],
            "the link-state family"
        );
        // The wire's refusal passes a code and never the refuser's message.
        let wire: Vec<&str> = sentences
            .iter()
            .filter(|row| row["category"] == json!("wire_code"))
            .map(|row| row["text"].as_str().expect("a text"))
            .collect();
        assert_eq!(wire.len(), 2, "one for the stop, one for the message");
        assert!(
            wire.iter().all(|text| text.contains("(code {n})")),
            "the code is the part that is passed on: {wire:?}"
        );

        // Gate two: the file is what this build produces. Compared as a value
        // first, because that is the comparison whose failure is readable — it
        // names the row that moved — and then as BYTES, because the phone reads
        // the file and not a parse of it, and a fixture that may be reformatted
        // by hand is one whose diff stops being the record of a reword.
        let derived = crate::codex_refusals::fixture();
        assert_eq!(
            derived, committed,
            "the committed refusal sentences no longer match what this build \
             emits. Regenerate them: {REGENERATE}"
        );
        assert_eq!(
            FIXTURE,
            crate::codex_refusals::fixture_bytes(),
            "the committed refusal sentences say what this build emits but are \
             not written the way it writes them. Regenerate them: {REGENERATE}"
        );
    }

    /// Regenerate `fixtures/codex/refusal-sentences.json`. Run with
    /// `cargo test -p ccd --bin ccd -- --ignored regenerate_the_refusal_sentences_fixture`.
    ///
    /// **It writes the file itself**, rather than printing it for a shell to
    /// redirect. libtest owns the same stdout, so `--nocapture > file` wrote the
    /// harness's own "running 1 test" lines into the JSON and produced a fixture
    /// that did not parse; `CC_REFUSAL_FIXTURE_OUT` names another path when the
    /// tree should not be touched.
    #[test]
    #[ignore = "generator, not a gate"]
    fn regenerate_the_refusal_sentences_fixture() {
        let path = crate::codex_refusals::write_fixture();
        println!("wrote {}", path.display());
    }

    fn approval_events(daemon: &Daemon, uid: &str, kind: EventKind) -> Vec<Event> {
        daemon
            .store
            .events_after(uid, 0, 50)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .collect()
    }

    /// **A card the store refuses files no fact about it.**
    ///
    /// The unique index on `(session_uid, thread_id, item_id)` is the schema
    /// saying one wire item is one card, and it catches the one failure the
    /// primary key cannot: a *broken derivation*, which produces a different
    /// request id for the same item. The event used to be appended first, so
    /// that refusal left an `ApprovalRequest` in the log with no card anywhere
    /// and no terminal that could ever retire it — a question the timeline shows
    /// forever. One transaction is what makes the refusal total.
    ///
    /// **Mutation:** append the event before the row (or in a second
    /// transaction) and the second assertion goes red with two request events
    /// for one item.
    #[tokio::test]
    async fn a_card_the_schema_refuses_leaves_no_request_event_behind() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");

        assert!(raise_codex_card(&daemon, uid, "derived-1", "exec-1").await);
        // The same item under a second derived id: the derivation broke, and the
        // index is what says so.
        assert!(
            !raise_codex_card(&daemon, uid, "derived-1-forked", "exec-1").await,
            "one wire item must not become two cards"
        );

        assert_eq!(
            approval_events(&daemon, uid, EventKind::ApprovalRequest).len(),
            1,
            "the refused card must file no request event: an ApprovalRequest with no \
             card behind it can never be resolved"
        );
        let pending = daemon.inner.lock().await;
        assert_eq!(pending.pending.len(), 1);
        assert!(pending
            .pending
            .contains_key(&(uid.to_string(), "derived-1".to_string())));
    }

    /// **A re-delivery that changed the question is refused, not merged.**
    ///
    /// The silent-refresh path moved one half of the representation. The stored
    /// row and the in-memory card would take the new content while the
    /// already-filed `ApprovalRequest` — the fact every connected client holds,
    /// and the fact a phone answer is checked against — kept the old one. No event,
    /// no ring, nothing anywhere saying the question had changed under a card a
    /// human is looking at.
    ///
    /// The measured re-delivery is byte-identical (the live bounce capture), so
    /// this shape is one the wire has never produced; the honest answer is to
    /// refuse it loudly and leave the card the phone is holding alone, rather
    /// than invent a reconciliation for an input nothing can currently generate.
    ///
    /// **Mutation:** restore `ON CONFLICT … DO UPDATE SET card, turn_id` and the
    /// stored-card assertion goes red while the request-event count stays at 1 —
    /// which is exactly the two halves disagreeing.
    #[tokio::test]
    async fn a_redelivery_that_changed_the_question_is_refused_and_the_card_stands() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");

        assert!(raise_codex_card(&daemon, uid, "derived-1", "exec-1").await);
        let filed = daemon.store.codex_pending_approvals(uid).unwrap();
        assert_eq!(filed.len(), 1);
        let stored = filed[0].card.clone();

        // The same item and the same derived id, carrying a different command.
        let approval = crate::codex_approval::Approval::read(
            crate::codex_approval::Family::Command,
            &json!({
                "threadId": "th-1",
                "turnId": "tu-1",
                "itemId": "exec-1",
                "environmentId": "local",
                "command": "/bin/zsh -lc 'rm -rf /'",
                "cwd": "/work",
                "availableDecisions": ["accept", "cancel"],
            }),
            None,
        )
        .expect("a command approval");
        let swapped = approval.card("derived-1".to_string(), 1);
        assert_ne!(
            swapped.payload_hash, filed[0].card,
            "the premise: this is a different question"
        );
        assert!(
            !daemon
                .raise_codex_approval(
                    &SessionKey::new(uid, "cc-1"),
                    swapped,
                    &approval.thread_id,
                    &approval.turn_id,
                    &approval.item_id,
                    "commandExecution",
                )
                .await,
            "a re-delivery that changed the question is not a rebind"
        );

        let after = daemon.store.codex_pending_approvals(uid).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].card, stored,
            "the stored card must still be the one the filed request event describes"
        );
        assert_eq!(
            approval_events(&daemon, uid, EventKind::ApprovalRequest).len(),
            1,
            "and no second request event was filed for it either"
        );
        // The in-memory card is the stored one too, so nothing on this daemon
        // holds the swapped question.
        let held = daemon.inner.lock().await;
        let entry = held
            .pending
            .get(&(uid.to_string(), "derived-1".to_string()))
            .expect("the original card is still open");
        assert_eq!(serde_json::to_string(&entry.card).unwrap(), stored);
    }

    /// **A card that cannot be persisted is not raised at all.**
    ///
    /// The old arm called an I/O failure "not evidence of anything about the
    /// card", kept the in-memory half and rang the phone — a card that exists
    /// only in this process, that a restart forgets, and that the rebind path
    /// can never find because the store is its only witness. For Codex that is
    /// the whole guarantee: nothing is blocked on this daemon's answer, so a
    /// card's only value is that it is durable. Refusing loudly leaves the
    /// question where it already is — in front of the operator at the keyboard.
    ///
    /// **Mutation:** restore the `Err` arm that logs and falls through, and all
    /// three assertions go red.
    #[tokio::test]
    async fn a_card_that_cannot_be_persisted_is_refused_rather_than_raised_in_memory() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");
        store.break_codex_card_writes_for_tests();

        assert!(!raise_codex_card(&daemon, uid, "derived-1", "exec-1").await);
        assert!(
            approval_events(&daemon, uid, EventKind::ApprovalRequest).is_empty(),
            "no card, so no fact claiming there is one"
        );
        assert!(
            daemon.inner.lock().await.pending.is_empty(),
            "an in-memory-only card is a card the rebind path can never find"
        );
        // And the doorbell was never admitted: the gate's key for this request is
        // still free, which it would not be if a push had been dispatched.
        assert!(
            daemon.push_gate.admit_decision(uid, "derived-1").is_some(),
            "a refused card must not have rung"
        );
    }

    /// **A retirement that cannot commit keeps the card, and the SAME daemon
    /// finishes it.**
    ///
    /// Deleting the row and filing the resolution were two writes with the claim
    /// given up between them. A failed delete plus a filed resolution left a row
    /// recovery restores — an answered question back on a phone after a restart.
    /// A successful delete plus a failed append lost the only terminal the card
    /// will ever have. One transaction removes both, and putting the claim back
    /// is what keeps the failure retryable: a terminal that neither retired the
    /// card nor consumed the right to must not be the winner.
    ///
    /// **The retry is on the same daemon, the same store and the same claim**,
    /// because that is the only version of the question worth asking. An earlier
    /// draft of this test "retried" by building a fresh store, a fresh daemon and
    /// a fresh card — which exercises a first retirement, not a retry, and would
    /// have passed with the restored claim thrown away. The failure is therefore
    /// injected reversibly (`hide_codex_cards_for_tests`) so the retry meets the
    /// card the first terminal left behind.
    ///
    /// **Mutation:** drop the `pending.insert(id, claimed)` on the error arm and
    /// the retry finds `AlreadyGone` — the card is gone from memory with no
    /// resolution anywhere and its row still in the store.
    #[tokio::test]
    async fn a_retirement_that_cannot_commit_is_retried_by_the_next_terminal() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");
        assert!(raise_codex_card(&daemon, uid, "derived-1", "exec-1").await);

        // The row is unreachable, so the delete half cannot land. Reversible, so
        // the retry below is a real retry.
        store.hide_codex_cards_for_tests(true);
        let cleared = protocol::ws::CodexResolution::Cleared {
            cause: protocol::ws::ClearCause::TurnAborted,
        };
        assert_eq!(
            daemon
                .retire_codex_approval(&SessionKey::new(uid, "cc-1"), "derived-1", cleared.clone())
                .await,
            Retirement::Failed,
            "a failed commit must not report itself as a settled card"
        );
        assert!(
            approval_events(&daemon, uid, EventKind::ApprovalResolved).is_empty(),
            "a resolution filed against a card still in the store is what resurrects \
             it on the next restart"
        );
        assert!(
            daemon
                .inner
                .lock()
                .await
                .pending
                .contains_key(&(uid.to_string(), "derived-1".to_string())),
            "the claim goes back, or this terminal has consumed the right to retire \
             the card without retiring it"
        );

        // The store answers again, and the next terminal — same daemon, same
        // claim, same card — finishes what the first one could not.
        store.hide_codex_cards_for_tests(false);
        assert_eq!(
            daemon
                .retire_codex_approval(&SessionKey::new(uid, "cc-1"), "derived-1", cleared)
                .await,
            Retirement::Retired,
            "the restored claim is what makes the next terminal able to retire it"
        );
        assert_eq!(
            approval_events(&daemon, uid, EventKind::ApprovalResolved).len(),
            1
        );
        assert!(daemon
            .store
            .codex_pending_approvals(uid)
            .unwrap()
            .is_empty());
        assert!(daemon.inner.lock().await.pending.is_empty());

        // And a third terminal for the same card is `AlreadyGone`, not a second
        // failure and not a second resolution.
        assert_eq!(
            daemon
                .retire_codex_approval(
                    &SessionKey::new(uid, "cc-1"),
                    "derived-1",
                    protocol::ws::CodexResolution::Cleared {
                        cause: protocol::ws::ClearCause::ItemCompleted,
                    },
                )
                .await,
            Retirement::AlreadyGone
        );
        assert_eq!(
            approval_events(&daemon, uid, EventKind::ApprovalResolved).len(),
            1,
            "first terminal wins, and the loser files nothing"
        );
    }

    /// **A resolved card leaves no row for recovery to resurrect.**
    ///
    /// Recovery restores every open card in `all_pending_approvals` whose
    /// request has no `answers` row — and nothing writes a Codex row into
    /// `answers`, by design, because that table is one of the four a rolled-back
    /// v0.6.0 daemon rewrites globally. So for a Codex card the SQL terminal
    /// check can never fire, and the *only* thing standing between an answered
    /// question and a second appearance on the phone is that its row is gone.
    /// The atomic retirement is what guarantees that, and this is the assertion
    /// that says so end to end.
    ///
    /// **Mutation:** delete the row and file the resolution as two writes, break
    /// the delete, and the restored count comes back 1.
    #[tokio::test]
    async fn recovery_does_not_resurrect_a_codex_card_that_was_already_resolved() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");
        assert!(raise_codex_card(&daemon, uid, "derived-1", "exec-1").await);
        assert_eq!(
            daemon
                .retire_codex_approval(
                    &SessionKey::new(uid, "cc-1"),
                    "derived-1",
                    protocol::ws::CodexResolution::Answered {
                        by: protocol::ws::ResolutionActor::Phone,
                        decision: None,
                        answered_on: None,
                    },
                )
                .await,
            Retirement::Retired
        );

        // A fresh daemon over the same store is exactly what a restart is.
        let restarted = daemon_on(Arc::clone(&store), Config::default());
        restarted.recover().await;
        assert!(
            restarted.inner.lock().await.pending.is_empty(),
            "a question that was answered before the restart must not come back \
             asking to be answered again"
        );
    }

    /// The largest card `codex_approval`'s bounds can produce: every field at
    /// its ceiling, and the file list at `MAX_CHANGES` with maximal diffs.
    fn worst_case_codex_approval() -> crate::codex_approval::Approval {
        let started = json!({
            "type": "fileChange",
            "id": "exec-worst",
            "changes": (0..64)
                .map(|n| json!({
                    "path": format!("/work/{}/file{n}.txt", "ünïcodé".repeat(64)),
                    "kind": {"type": "update", "move_path": serde_json::Value::Null},
                    "diff": "→".repeat(64 * 1024),
                }))
                .collect::<Vec<serde_json::Value>>(),
        });
        crate::codex_approval::Approval::read(
            crate::codex_approval::Family::FileChange,
            &json!({
                "threadId": "01a06db1-b6a5-7500-8956-6c35b83b32d2",
                "turnId": "01a06db1-ce36-79d0-a0c1-9eecc7865ac0",
                "itemId": "exec-worst",
            }),
            Some(&started),
        )
        .expect("a worst-case file change is still readable")
    }

    /// **A worst-case card survives the daemon's own truncation, intact.**
    ///
    /// This is the assertion the per-field bounds were believed to buy and did
    /// not. `Daemon::truncate_payload` does not trim an oversized card — it
    /// REPLACES it with `{_codeconnect_truncated, _original_bytes, _preview}`,
    /// and a phone handed that decodes no card, so both buttons are gone. The
    /// old ceilings multiplied out to 512 KiB of diffs *before* counting that
    /// every field rides the payload twice (once as `tool_input`, once
    /// JSON-escaped inside `display_text`): a 32-file patch of maximal diffs
    /// measured 1,056,469 bytes against the 524,288-byte default, and the cliff
    /// was at **sixteen** files, not thirty-two.
    ///
    /// So the bound that matters is the aggregate one, it is measured on the
    /// serialised card, and it is checked here — through the real `ingest`, at
    /// the real configured limit, not against a constant this test also owns.
    ///
    /// **Mutation:** restore the per-file `MAX_DIFF_BYTES` in place of the
    /// shared `MAX_TOTAL_DIFF_BYTES` split and the filed payload comes back
    /// replaced by the preview object.
    #[tokio::test]
    async fn the_largest_card_the_bounds_admit_survives_the_daemons_truncation() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");

        let approval = worst_case_codex_approval();
        let card = approval.card("codex-worst-case".into(), 1);
        assert!(
            daemon
                .raise_codex_approval(
                    &SessionKey::new(uid, "cc-1"),
                    card,
                    &approval.thread_id,
                    &approval.turn_id,
                    &approval.item_id,
                    "fileChange",
                )
                .await,
            "the worst case the bounds admit must still be cardable"
        );

        let event = daemon
            .store
            .events_after(uid, 0, 50)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EventKind::ApprovalRequest)
            .expect("the approval must be logged");
        assert!(
            event.payload["_codeconnect_truncated"].is_null(),
            "the daemon replaced its own card with a preview object; the phone \
             decodes nothing from that. Payload was {} bytes against a {}-byte limit.",
            event.payload.to_string().len(),
            daemon.config.max_payload_bytes
        );
        // And it is still the card the phone verifies, after the round trip.
        let filed: ApprovalCard =
            serde_json::from_value(event.payload["card"].clone()).expect("a decodable card");
        assert_eq!(
            filed.payload_hash,
            protocol::hash::sha256_hex(filed.display_text.as_bytes())
        );
    }

    /// **And a card that cannot fit is refused rather than filed dead.**
    ///
    /// The per-field bounds make the aggregate check rare; they cannot make it
    /// unreachable, because nothing on the wire bounds a path, an execpolicy
    /// amendment, or a field a later codex adds — and `max_payload_bytes` is an
    /// operator setting that clamps as low as 4 KiB. When the total does not
    /// fit, the honest answer is no card: the question is being asked at the
    /// keyboard and is still answerable there. A filed event whose payload the
    /// daemon replaced is a permanent unanswerable card.
    ///
    /// **Mutation:** delete the size check in `raise_codex_approval` and the
    /// event is filed with `_codeconnect_truncated`, in memory and on the phone.
    #[tokio::test]
    async fn a_card_too_large_for_the_payload_limit_is_refused_rather_than_filed_dead() {
        let (store, _db) = shared_store_on_disk();
        let uid = TEST_UID;
        let daemon = daemon_on(
            Arc::clone(&store),
            Config {
                max_payload_bytes: 4096,
                ..Config::default()
            },
        );
        seed_codex_session(&daemon, uid, "cc-1");

        let approval = worst_case_codex_approval();
        let card = approval.card("codex-too-large".into(), 1);
        assert!(
            !daemon
                .raise_codex_approval(
                    &SessionKey::new(uid, "cc-1"),
                    card,
                    &approval.thread_id,
                    &approval.turn_id,
                    &approval.item_id,
                    "fileChange",
                )
                .await,
            "a card the daemon would have to replace is not a card"
        );
        assert!(
            daemon
                .store
                .events_after(uid, 0, 50)
                .unwrap()
                .into_iter()
                .all(|e| e.kind != EventKind::ApprovalRequest),
            "a refused card files no request event"
        );
        assert!(
            daemon.inner.lock().await.pending.is_empty(),
            "and leaves nothing in memory to ring about"
        );
    }

    // --------------------------------------------- AskUserQuestion (minor 21)

    /// A live 2.1.286 session's hooks for one 4-question `AskUserQuestion`.
    const QUESTION_HOOKS: &str = include_str!("../../../fixtures/claude/askuq-4q-2.1.286.jsonl");

    fn question_hook(event: &str) -> serde_json::Value {
        QUESTION_HOOKS
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|hook| hook["hook_event_name"] == event)
            .unwrap()
    }

    fn question_input() -> serde_json::Value {
        question_hook("PermissionRequest")["tool_input"].clone()
    }

    /// The `tool_use_id` the fixture's `PreToolUse` names, which the card is
    /// joined to and the `PostToolUse` names again.
    const QUESTION_ID: &str = "toolu_fake_7440824";

    /// A question's doorbell says it is a question, not an approval.
    #[tokio::test]
    async fn a_question_rings_as_a_question() {
        let (daemon, capture) = capture_daemon();
        let hold = daemon.push_gate.hold_grace();
        raise_ring(&daemon, Ring::ClaudeQuestion).await;
        hold.end();
        rung_after(&capture, 1).await;
        let rings = capture.0.lock().unwrap();
        assert_eq!(rings.len(), 1, "{rings:?}");
        assert_eq!(rings[0].0.kind, crate::apns::PushKind::Question);
    }

    /// **A run's open cards close when it ends.** Nobody is left to answer an
    /// approval or a question held for the phone, so each is closed as its host's card,
    /// the session ending: no answer is recorded and nothing reads as typed, the one a
    /// restart could not link included.
    #[tokio::test]
    async fn a_runs_open_cards_close_when_it_ends() {
        let daemon = test_daemon();
        hosted_card(
            &daemon,
            TEST_UID,
            1,
            "toolu_approval",
            "Bash",
            json!({"command": "touch a.txt"}),
        )
        .await;
        hosted_card(
            &daemon,
            TEST_UID,
            2,
            QUESTION_ID,
            protocol::host::ASK_USER_QUESTION,
            question_input(),
        )
        .await;
        assert_eq!(
            daemon.inner.lock().await.pending.len(),
            2,
            "the premise: two open cards"
        );
        // As recovery leaves a card whose run's host it could not read.
        daemon
            .inner
            .lock()
            .await
            .pending
            .get_mut(&(TEST_UID.to_string(), QUESTION_ID.to_string()))
            .unwrap()
            .link = false;

        assert!(
            daemon
                .end_hosted_run(
                    TEST_UID,
                    HOST,
                    None,
                    Lifecycle::Live,
                    Lifecycle::Exited,
                    json!({"exit_code": 0})
                )
                .await
                .unwrap(),
            "the premise: the run ended"
        );

        assert!(daemon.inner.lock().await.pending.is_empty());
        assert!(daemon.store.list_pending_approvals().unwrap().is_empty());
        let mut closed: Vec<String> = Vec::new();
        for event in approval_events(&daemon, TEST_UID, EventKind::ApprovalResolved) {
            let outcome: AnswerOutcome = serde_json::from_value(event.payload).unwrap();
            assert_eq!(outcome.resolved_by, ResolvedBy::Timeout);
            assert_eq!(outcome.detail.as_deref(), Some(RUN_ENDED));
            assert_eq!(outcome.applied_via, AnswerPath::HookReturn);
            assert_eq!(
                daemon
                    .store
                    .get_answer(TEST_UID, &outcome.request_id)
                    .unwrap(),
                None
            );
            closed.push(outcome.request_id);
        }
        closed.sort();
        assert_eq!(closed, ["toolu_approval", QUESTION_ID]);
    }

    /// The Codex card a run leaves when it ends, retired in Codex's own store.
    fn assert_retired_as_ended(daemon: &Daemon, uid: &str, request_id: &str) {
        assert!(daemon.inner.try_lock().unwrap().pending.is_empty());
        assert!(daemon
            .store
            .codex_pending_approvals(uid)
            .unwrap()
            .is_empty());
        assert!(daemon.store.list_pending_approvals().unwrap().is_empty());
        let resolved = approval_events(daemon, uid, EventKind::ApprovalResolved);
        assert_eq!(resolved.len(), 1, "one terminal for the card");
        assert_eq!(resolved[0].payload["request_id"], request_id);
        assert_eq!(resolved[0].payload["status"], "cleared");
        assert_eq!(resolved[0].payload["cause"], "turn_aborted");
    }

    /// A Codex run's open card closes when the run ends, as Claude's do: nobody
    /// is left to answer it.
    ///
    /// **Mutation:** keep `after_the_end` to Claude's cards and the card stays
    /// open, its run listed as blocked, for as long as the row exists.
    #[tokio::test]
    async fn a_codex_runs_open_card_closes_when_it_ends() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        seed_codex_session(&daemon, uid, "cc-1");
        daemon.store.record_host(uid, &HOST, None).unwrap();
        assert!(raise_codex_card(&daemon, uid, "req-ended", "item-ended").await);

        assert!(daemon
            .end_hosted_run(
                uid,
                HOST,
                None,
                Lifecycle::Live,
                Lifecycle::Exited,
                json!({"exit_code": 0})
            )
            .await
            .unwrap());

        assert_eq!(lifecycle_of(&daemon, uid), Lifecycle::Exited);
        assert_retired_as_ended(&daemon, uid, "req-ended");
    }

    /// A card an ended run left open, as a daemon before this one left it, is
    /// closed at the next start.
    ///
    /// **Mutation:** drop the closing at the end of `recover` and the card is
    /// restored for ever: the run is never seen ending again.
    #[tokio::test]
    async fn a_card_an_ended_run_left_open_is_closed_at_the_next_start() {
        let store = shared_store();
        let uid = TEST_UID;
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_codex_session(&daemon, uid, "cc-1");
        assert!(raise_codex_card(&daemon, uid, "req-left", "item-left").await);
        store.set_lifecycle(uid, Lifecycle::Exited).unwrap();

        let restarted = daemon_on(store, Config::default());
        restarted.recover().await;

        assert_retired_as_ended(&restarted, uid, "req-left");
    }

    /// **ExitPlanMode's plan, as this build emits the card.** `fixtures/claude/
    /// exit-plan-input.json` is one ExitPlanMode `tool_input` — a plan and its
    /// `planFilePath` — asked by a hosted run's agent host, the path a real plan
    /// card takes. The daemon builds `display_text` and `payload_hash`; only clock
    /// reads are normalised. R2T1-F2: the phone must draw the plan, not this
    /// `{tool}\n{input}`. The phone's copy under `ios/` is byte-identical
    /// (`fixtures/check-duplicated-fixtures.sh`).
    #[tokio::test]
    async fn the_exit_plan_card_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/claude/exit-plan-card-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_exit_plan_card().await,
            committed,
            "the committed ExitPlanMode card no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_exit_plan_card_fixture, then copy it over its copy under \
             ios/ (fixtures/check-duplicated-fixtures.sh names it)"
        );
    }

    /// Rewrite `fixtures/claude/exit-plan-card-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_exit_plan_card_fixture() {
        let wire = serde_json::to_string_pretty(&derived_exit_plan_card().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/exit-plan-card-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    async fn derived_exit_plan_card() -> serde_json::Value {
        const INPUT: &str = include_str!("../../../fixtures/claude/exit-plan-input.json");
        let input: serde_json::Value = serde_json::from_str(INPUT).unwrap();
        let uid = TEST_UID;
        let daemon = test_daemon();
        hosted_card(&daemon, uid, 1, QUESTION_ID, "ExitPlanMode", input).await;
        let mut events = approval_events(&daemon, uid, EventKind::ApprovalRequest);
        assert_eq!(events.len(), 1, "one ExitPlanMode card");
        let mut out = json!({ "exit_plan": serde_json::to_value(events.remove(0)).unwrap() });
        fn normalise(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, slot) in map.iter_mut() {
                        if matches!(key.as_str(), "ts" | "resolved_at") {
                            let was = slot.as_str().unwrap_or_default();
                            assert!(
                                was.len() == 24 && was.ends_with('Z'),
                                "{key} was expected to be a clock read, and is {slot}"
                            );
                            *slot = json!("2026-10-03T00:00:00.000Z");
                        } else {
                            normalise(slot);
                        }
                    }
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(normalise),
                _ => {}
            }
        }
        normalise(&mut out);
        out
    }

    // ----------------------------------------- summary refs and facts (minor 23)

    /// The Codex approval every test below raises: its thread, turn and item.
    const CODEX_THREAD: &str = "01a06db1-1f8e-7db2-8fd5-10f13af55d1b";
    const CODEX_TURN: &str = "01a06db1-2223-7ed0-8be5-1d7e3ccdeaee";
    const CODEX_ITEM: &str = "exec-d2700ed3-c69d-4915-a620-36c001e7f577";

    /// A card's `approval_request` event, read back from the log by its dedup id.
    fn request_event(daemon: &Arc<Daemon>, uid: &str, request_id: &str) -> Event {
        let id = format!("perm:{request_id}");
        daemon
            .store
            .events_after(uid, 0, 100_000)
            .unwrap()
            .into_iter()
            .find(|event| {
                event.kind == EventKind::ApprovalRequest
                    && event.source_event_id.as_deref() == Some(id.as_str())
            })
            .expect("the card's request event is in the log")
    }

    async fn summary_of(daemon: &Arc<Daemon>, uid: &str) -> SessionSummary {
        daemon
            .sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|summary| summary.session_uid == uid)
            .expect("the run is listed")
    }

    fn fork_permission_request() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/claude/askuq-fork-permissionrequest-2.1.286.json"
        ))
        .unwrap()
    }

    /// **A pending ref names the seq of its own request event**, for a Claude
    /// card (asked by its agent host) and a Codex card (filed with its row,
    /// carrying its turn).
    #[tokio::test]
    async fn a_pending_ref_names_the_seq_of_its_request_event_for_both_agents() {
        let daemon = test_daemon();
        hosted_run(&daemon, TEST_UID, "/tmp");
        for n in 0..3 {
            tool_call(&daemon, TEST_UID, TEST_UID, &format!("toolu_{n}")).await;
        }
        let request_id = "toolu_card".to_string();
        hosted_card(
            &daemon,
            TEST_UID,
            1,
            &request_id,
            "Bash",
            json!({"command": "touch /tmp/a"}),
        )
        .await;
        let request = request_event(&daemon, TEST_UID, &request_id);
        assert_eq!(
            request.seq, 4,
            "the premise: the request is not the first event"
        );
        let summary = summary_of(&daemon, TEST_UID).await;
        assert_eq!(summary.blocked_on, vec![request_id.clone()]);
        assert_eq!(
            summary.pending,
            vec![protocol::event::PendingRef {
                request_id,
                seq: request.seq,
                hold: serde_json::from_value(request.payload["card"]["question_hold"].clone())
                    .unwrap(),
                turn_id: None,
            }]
        );

        let codex = "01K1B3XQ8ZC0DE5FGH7JKMNPQS";
        seed_codex_session(&daemon, codex, "cc-2");
        let session = SessionKey::new(codex, "cc-2");
        tool_call(&daemon, "cc-2", codex, "toolu_codex").await;
        assert!(
            daemon
                .raise_codex_approval(
                    &session,
                    codex_command_card("codex-req-1"),
                    CODEX_THREAD,
                    CODEX_TURN,
                    CODEX_ITEM,
                    "commandExecution",
                )
                .await
        );
        let request = request_event(&daemon, codex, "codex-req-1");
        assert_eq!(request.source, Source::Daemon);
        assert!(
            request.seq > 1,
            "the premise: the request is not the first event"
        );
        let summary = summary_of(&daemon, codex).await;
        assert_eq!(
            summary.pending,
            vec![protocol::event::PendingRef {
                request_id: "codex-req-1".into(),
                seq: request.seq,
                hold: None,
                turn_id: Some(CODEX_TURN.into()),
            }]
        );
    }

    /// **Three open cards of 600 KB each cost the session list nothing**: a
    /// `sessions` reply names them by reference, stays under 64 KiB, and decodes.
    #[tokio::test]
    async fn a_sessions_reply_with_three_huge_cards_stays_small_and_decodes() {
        // Under a payload limit that lets such a card through whole.
        let daemon = daemon_with(Config {
            max_payload_bytes: 4 * 1024 * 1024,
            ..Config::default()
        });
        let big = "x".repeat(600 * 1024);
        for n in 0..3u64 {
            hosted_card(
                &daemon,
                TEST_UID,
                n + 1,
                &format!("toolu_{n}"),
                "Bash",
                json!({ "command": format!("{big}{n}") }),
            )
            .await;
        }
        let sessions = daemon.sessions().await.unwrap();
        assert_eq!(
            sessions[0].blocked_on.len(),
            3,
            "the premise: three open cards"
        );
        let encoded =
            serde_json::to_string(&protocol::ws::ServerMessage::Sessions { sessions }).unwrap();
        let decoded: protocol::ws::ServerMessage = serde_json::from_str(&encoded).unwrap();
        let protocol::ws::ServerMessage::Sessions { sessions } = decoded else {
            panic!("a sessions reply decodes as one");
        };
        assert_eq!(sessions[0].pending.len(), 3, "{encoded}");
        assert!(
            encoded.len() < 64 * 1024,
            "a sessions reply of {} bytes carries the cards' bodies",
            encoded.len()
        );
    }

    /// **A quiet run's facts are right after a restart**, rebuilt from the log an
    /// earlier build wrote: the model a `/model` receipt set after the
    /// `SessionStart`, and the mode a transcript line set after the main agent's
    /// hook — and a background agent's later hook changes neither, before the
    /// restart or after it.
    #[tokio::test]
    async fn a_quiet_sessions_facts_are_rebuilt_from_the_log_after_a_restart() {
        let store = shared_store();
        let daemon = daemon_on(Arc::clone(&store), Config::default());
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Live);
        let key = test_key();
        let filed = |id: &str, kind: EventKind, source: Source, payload: serde_json::Value| {
            PendingEvent::new(&key, kind, payload, source).with_source_event_id(id.to_string())
        };
        let mut fork_call = fork_permission_request();
        fork_call["hook_event_name"] = json!("PreToolUse");
        fork_call["tool_use_id"] = json!("toolu_fork");
        for event in [
            filed(
                "start",
                EventKind::SessionStart,
                Source::Hook,
                json!({
                    "hook_event_name": "SessionStart",
                    "cwd": "/tmp",
                    "source": "startup",
                    "model": "claude-opus-5[1m]",
                    "permission_mode": "default",
                }),
            ),
            filed(
                "toolu_1",
                EventKind::ToolCall,
                Source::Hook,
                json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "toolu_1"}),
            ),
            filed(
                "u1",
                EventKind::UserMessage,
                Source::Transcript,
                json!({"type": "user", "uuid": "u1", "message": {"role": "user",
                    "content": "<local-command-stdout>Set model to claude-sonnet-5 and saved as your default for new sessions</local-command-stdout>"}}),
            ),
            filed(
                "u2",
                EventKind::Other(protocol::facts::TRANSCRIPT_PERMISSION_MODE.into()),
                Source::Transcript,
                json!({"type": "permission-mode", "uuid": "u2", "permissionMode": "acceptEdits"}),
            ),
            filed(
                "u3",
                EventKind::UserMessage,
                Source::Transcript,
                json!({"type": "user", "uuid": "u3", "message": {"role": "user", "content": "thanks"}}),
            ),
            filed("toolu_fork", EventKind::ToolCall, Source::Hook, fork_call),
        ] {
            daemon.ingest(event).await.unwrap().expect("a new event");
        }

        let log = store.events_after(TEST_UID, 0, 100).unwrap();
        let by_id = |id: &str| {
            log.iter()
                .find(|event| event.source_event_id.as_deref() == Some(id))
                .unwrap()
                .clone()
        };
        let (receipt, mode) = (by_id("u1"), by_id("u2"));
        let model = Some(protocol::event::Fact {
            value: "claude-sonnet-5".into(),
            seq: receipt.seq,
        });
        let permission_mode = Some(protocol::event::Fact {
            value: "acceptEdits".into(),
            seq: mode.seq,
        });
        assert!(
            log.last().unwrap().seq > mode.seq,
            "the premise: newer events follow both deciding ones"
        );

        let live = summary_of(&daemon, TEST_UID).await;
        assert_eq!(
            (&live.model, &live.permission_mode),
            (&model, &permission_mode)
        );

        let restarted = daemon_on(Arc::clone(&store), Config::default());
        restarted.recover().await;
        let rebuilt = summary_of(&restarted, TEST_UID).await;
        assert_eq!(
            (&rebuilt.model, &rebuilt.permission_mode),
            (&model, &permission_mode)
        );
    }

    /// **A Codex card is in `pending` before anyone can see its request event** —
    /// in the log, where a replay or a page reads it, or on the broadcast.
    ///
    /// Proved at the commit itself: the test holds the database's write lock from
    /// a connection of its own, so the raise stalls inside its commit with the
    /// request event not yet in the log. The card must already be pending there.
    /// A raise that inserts the card after its commit — whatever locks it takes on
    /// the way — cannot have, because its commit cannot finish while the lock is
    /// held.
    #[tokio::test]
    async fn a_codex_card_is_pending_before_its_request_event_is_visible() {
        let (store, path) = shared_store_on_disk();
        let daemon = daemon_on(store, Config::default());
        seed_codex_session(&daemon, TEST_UID, "cc-1");
        let id = (TEST_UID.to_string(), "codex-req-1".to_string());
        let mut events = daemon.events_tx.subscribe();
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let raising = {
            let daemon = Arc::clone(&daemon);
            tokio::spawn(async move {
                daemon
                    .raise_codex_approval(
                        &test_key(),
                        codex_command_card("codex-req-1"),
                        CODEX_THREAD,
                        CODEX_TURN,
                        CODEX_ITEM,
                        "commandExecution",
                    )
                    .await
            })
        };
        // Within the store's five-second busy timeout, so the stalled commit is
        // still waiting rather than refused.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !daemon.inner.lock().await.pending.contains_key(&id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the raise reached its commit with no card pending"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !daemon
                .store
                .events_after(TEST_UID, 0, 100)
                .unwrap()
                .iter()
                .any(|event| event.kind == EventKind::ApprovalRequest),
            "the premise: the request event is not in the log yet"
        );
        writer.execute_batch("COMMIT").unwrap();

        loop {
            let event = events.recv().await.unwrap();
            if event.kind == EventKind::ApprovalRequest {
                break;
            }
        }
        assert!(daemon.inner.lock().await.pending.contains_key(&id));
        assert!(raising.await.unwrap(), "the card was raised");
    }

    /// **A Codex raise aborted while it waits leaves no card behind** — a card
    /// with no row and no event is one nothing can expire, retire or delete. A
    /// link teardown aborts the task that raises, at whatever await it is on;
    /// here it is aborted while waiting for the run's publish gate, and nothing of
    /// it lands once the gate is free.
    #[tokio::test]
    async fn an_aborted_codex_raise_leaves_no_card_without_its_row_and_event() {
        let daemon = test_daemon();
        seed_codex_session(&daemon, TEST_UID, "cc-1");
        let id = (TEST_UID.to_string(), "codex-req-1".to_string());
        let gate = daemon.publish_gate(TEST_UID).await;
        let held = gate.lock().await;
        let raising = {
            let daemon = Arc::clone(&daemon);
            tokio::spawn(async move {
                daemon
                    .raise_codex_approval(
                        &test_key(),
                        codex_command_card("codex-req-1"),
                        CODEX_THREAD,
                        CODEX_TURN,
                        CODEX_ITEM,
                        "commandExecution",
                    )
                    .await
            })
        };
        // The raise has the gate in hand and is waiting for it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&gate) < 3 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the raise never reached the gate"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        raising.abort();
        assert!(raising.await.unwrap_err().is_cancelled());
        assert!(
            !daemon.inner.lock().await.pending.contains_key(&id),
            "an aborted raise left a card with no row and no event"
        );

        drop(held);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let card = daemon.inner.lock().await.pending.contains_key(&id);
        let event = daemon
            .store
            .events_after(TEST_UID, 0, 100)
            .unwrap()
            .iter()
            .any(|event| event.kind == EventKind::ApprovalRequest);
        let row = !daemon
            .store
            .codex_pending_approvals(TEST_UID)
            .unwrap()
            .is_empty();
        assert_eq!(
            (card, event),
            (row, row),
            "a card, its event and its row go together"
        );
    }

    /// Raise the test's Codex card in a task of its own, so the test can abort it.
    fn raise_in_a_task(daemon: &Arc<Daemon>) -> tokio::task::JoinHandle<bool> {
        let daemon = Arc::clone(daemon);
        tokio::spawn(async move {
            daemon
                .raise_codex_approval(
                    &test_key(),
                    codex_command_card("codex-req-1"),
                    CODEX_THREAD,
                    CODEX_TURN,
                    CODEX_ITEM,
                    "commandExecution",
                )
                .await
        })
    }

    /// The card in memory, its request event in the log, its row in the table.
    async fn card_event_row(daemon: &Arc<Daemon>) -> (bool, bool, bool) {
        let id = (TEST_UID.to_string(), "codex-req-1".to_string());
        let card = daemon.inner.lock().await.pending.contains_key(&id);
        let event = daemon
            .store
            .events_after(TEST_UID, 0, 100)
            .unwrap()
            .iter()
            .any(|event| event.kind == EventKind::ApprovalRequest);
        let row = !daemon
            .store
            .codex_pending_approvals(TEST_UID)
            .unwrap()
            .is_empty();
        (card, event, row)
    }

    /// Wait until the commit a raise handed to the blocking pool has finished, on
    /// a pool thread, so a take-back handed to a task meanwhile still runs.
    async fn wait_out_the_writer(daemon: &Arc<Daemon>) {
        let store = Arc::clone(&daemon.store);
        tokio::task::spawn_blocking(move || store.wait_out_the_writer_for_tests())
            .await
            .unwrap();
    }

    /// Start a raise against a database another connection holds the write lock
    /// of, and wait until it is stalled inside its commit with its card pending.
    async fn a_raise_stalled_in_its_commit(daemon: &Arc<Daemon>) -> tokio::task::JoinHandle<bool> {
        let id = (TEST_UID.to_string(), "codex-req-1".to_string());
        let raising = raise_in_a_task(daemon);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !daemon.inner.lock().await.pending.contains_key(&id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the raise reached its commit with no card pending"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        raising
    }

    /// **A raise aborted inside a commit that then fails leaves nothing.** The
    /// test holds its write lock until the commit the aborted raise handed to the
    /// blocking pool has given up, so that commit is refused.
    #[tokio::test]
    async fn a_raise_aborted_inside_a_commit_that_fails_leaves_nothing() {
        let (store, path) = shared_store_on_disk();
        let daemon = daemon_on(store, Config::default());
        seed_codex_session(&daemon, TEST_UID, "cc-1");
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let raising = a_raise_stalled_in_its_commit(&daemon).await;
        raising.abort();
        assert!(raising.await.unwrap_err().is_cancelled());
        wait_out_the_writer(&daemon).await;
        writer.execute_batch("COMMIT").unwrap();
        assert_eq!(
            card_event_row(&daemon).await,
            (false, false, false),
            "(card, event, row) after an aborted raise whose commit failed"
        );
    }

    /// **A raise aborted inside a commit that then succeeds leaves no card in
    /// memory**; what is durable is the commit the blocking pool finished — the
    /// row and the request event, which this daemon's next start restores as a
    /// card. Nothing is broadcast: the raise that would have was dropped.
    #[tokio::test]
    async fn a_raise_aborted_inside_a_commit_that_succeeds_leaves_only_the_durable_half() {
        let (store, path) = shared_store_on_disk();
        let daemon = daemon_on(store, Config::default());
        seed_codex_session(&daemon, TEST_UID, "cc-1");
        let mut events = daemon.events_tx.subscribe();
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let raising = a_raise_stalled_in_its_commit(&daemon).await;
        raising.abort();
        assert!(raising.await.unwrap_err().is_cancelled());
        writer.execute_batch("COMMIT").unwrap();
        // The commit the pool was running lands once the lock is free.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while daemon
            .store
            .codex_pending_approvals(TEST_UID)
            .unwrap()
            .is_empty()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the commit never landed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            card_event_row(&daemon).await,
            (false, true, true),
            "(card, event, row) after an aborted raise whose commit succeeded"
        );
        let broadcast = std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| event.kind == EventKind::ApprovalRequest);
        assert!(!broadcast, "a dropped raise published its event");
    }

    /// A commit that fails with nobody aborting: the caller is told `false` and
    /// the card it put in is taken back.
    #[tokio::test]
    async fn a_raise_whose_commit_fails_takes_its_card_back() {
        let (store, path) = shared_store_on_disk();
        let daemon = daemon_on(store, Config::default());
        seed_codex_session(&daemon, TEST_UID, "cc-1");
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let raised = raise_in_a_task(&daemon).await.unwrap();
        writer.execute_batch("COMMIT").unwrap();
        assert!(!raised);
        assert_eq!(card_event_row(&daemon).await, (false, false, false));
    }

    /// **The same, with the state lock held by someone else at the moment of the
    /// abort**, so the take-back cannot be done on the spot and is handed on.
    #[tokio::test]
    async fn a_raise_aborted_while_the_state_lock_is_held_still_leaves_nothing() {
        let (store, path) = shared_store_on_disk();
        let daemon = daemon_on(store, Config::default());
        seed_codex_session(&daemon, TEST_UID, "cc-1");
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let raising = a_raise_stalled_in_its_commit(&daemon).await;
        let held = daemon.inner.lock().await;
        raising.abort();
        assert!(raising.await.unwrap_err().is_cancelled());
        drop(held);
        wait_out_the_writer(&daemon).await;
        writer.execute_batch("COMMIT").unwrap();
        assert_eq!(
            card_event_row(&daemon).await,
            (false, false, false),
            "(card, event, row) after an aborted raise whose commit failed"
        );
    }

    /// **A raise takes back only its own card.** A card put in its place since —
    /// here one recovery would restore, inserted by no raise — survives the
    /// raise's take-back on every path: a refused commit, an abort with the
    /// state lock free, and an abort while it is held, when the take-back is
    /// handed on.
    #[tokio::test]
    async fn a_raise_takes_back_only_the_card_it_put_there() {
        let id = (TEST_UID.to_string(), "codex-req-1".to_string());
        for path in ["refused commit", "abort", "abort under the state lock"] {
            let (store, db) = shared_store_on_disk();
            let daemon = daemon_on(store, Config::default());
            seed_codex_session(&daemon, TEST_UID, "cc-1");
            let writer = rusqlite::Connection::open(&db).unwrap();
            writer.execute_batch("BEGIN IMMEDIATE").unwrap();
            let raising = a_raise_stalled_in_its_commit(&daemon).await;
            let mut held = daemon.inner.lock().await;
            let mut replaced = held.pending.remove(&id).unwrap();
            replaced.raise = 0;
            held.pending.insert(id.clone(), replaced);
            match path {
                "refused commit" => {
                    drop(held);
                    assert!(
                        !raising.await.unwrap(),
                        "the premise: the commit is refused"
                    );
                }
                "abort" => {
                    drop(held);
                    raising.abort();
                    assert!(raising.await.unwrap_err().is_cancelled());
                }
                _ => {
                    raising.abort();
                    assert!(raising.await.unwrap_err().is_cancelled());
                    drop(held);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                daemon.inner.lock().await.pending.contains_key(&id),
                "{path}: the raise took back a card it did not put there"
            );
            writer.execute_batch("COMMIT").unwrap();
        }
    }
}
