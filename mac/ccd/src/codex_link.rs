//! The ccd **control link**: one live app-server connection per Codex session.
//!
//! `codex_adapter.rs` is the pure half of the Codex observation path — frames in,
//! [`PendingEvent`]s out, no socket anywhere. This module is the other half and
//! nothing more: it **holds the connection**. What it deliberately does *not* do is
//! reconcile a resume response against live state — see the pre-2e-4b contract
//! below for why that would be a guess today, and whose it is instead. Per Codex session it dials the
//! broker's ccd leg (WS-over-UDS on the run dir's `ccd.sock`), completes the ccd
//! role's allowlisted handshake, binds the session's thread, stamps every inbound
//! frame with its ingress attribution, and hands the admitted ones to the
//! adapter. What comes back lands through [`Daemon::ingest`] — the same call the
//! hook and transcript paths make — so a Codex fact is deduplicated by
//! `(session_uid, source, source_event_id)` exactly as a Claude fact is, and a
//! re-observed frame costs a row that is never written rather than a duplicate.
//!
//! ## What ccd is allowed to say
//!
//! The ccd leg is attach-only and refuse-by-default (`codex-broker/src/allowlist.rs`,
//! `Role::Ccd`). Six requests forward at all: `initialize`, the four read-only
//! census reads (`thread/read`, `thread/loaded/list`, `thread/turns/list`,
//! `thread/items/list`), and `thread/resume` — which the broker additionally binds
//! to the thread set it learned from its own server→client stream
//! (`codex-broker/src/session.rs`), so a resume ccd invents for a thread that does
//! not belong to this launch is refused before a byte leaves the broker. Exactly
//! one notification forwards: `initialized`. This module sends `initialize`,
//! `initialized` and `thread/resume`, and nothing else — the census reads are on
//! the leg but this chunk has no use for them, and `turn/start`/`thread/start`/
//! `thread/fork` are refused to ccd by role.
//!
//! ## The pre-2e-4b contract, and why it is this strict
//!
//! **Turns now run.** `turn/start` is `FingerprintThenHeadCheck` on the TUI leg and
//! the head-check is implemented (`codex-broker/src/refusal.rs`): a turn naming the
//! session's one bound thread is forwarded, and a real TUI completes real turns
//! through the broker — `codex_link_live`'s claim 4 asserts it.
//!
//! **This link is not subscribed to any of them, and that is measured.** Turn frames
//! are delivered only to the connection whose `thread/resume` succeeded. This
//! link's resume does not succeed — see below — so across a whole turn the only
//! turn-correlated frames it is handed are `thread/status/changed`, which
//! `codex_adapter.rs` drops as observation noise. Measured on the live gate: zero
//! `turn/*` and zero `item/*` frames on a connection in exactly this position,
//! against seven on the subscribed one.
//!
//! What HAS changed is the resume answer. After a turn, `thread/resume` returns a
//! `result` whose `thread.turns` carries the turn that ran
//! (`fixtures/codex/resume-populated-answer.json`) — and that is precisely the
//! answer `settle_resume` refuses to guess at. It is not `item/started` replay, it
//! is not a validated snapshot, and nothing in this chunk was designed against its
//! completeness, its keying, or the cross-resume id stability D15 warns about. So
//! the link reports it and reconnects, which is a loop rather than an attach — the
//! honest cost of not inventing a reconciliation, and 2e-4b's to close.
//!
//! This link therefore implements the **total** contract for the world it was built
//! for, and fails closed at its edge rather than guessing past it:
//!
//!   * **Binding: `thread/started` on this connection, and nothing else.** A thread
//!     id from the registration, or carried over from a previous connection, is a
//!     **resume target** — it says what to attach to, never that this connection is
//!     bound. A frame naming a thread this connection did not watch start is
//!     neither ingested nor buffered: it is counted, logged, and dropped, and the
//!     leg carries on.
//!   * **Reconnect:** send `thread/resume`, and accept **exactly one** answer — the
//!     measured `no rollout found` error for the thread that was asked about
//!     (retry with backoff; A1/D3, and the only answer a thread that has not yet
//!     run a turn can give). *Everything* else, success shapes included, is
//!     reported with a STOP-AND-AMEND log and reconnected. A success is not
//!     tolerated even though the live wire now produces one after a turn:
//!     normalizing a response whose completeness and keying nothing here has
//!     validated is exactly the guess this chunk refuses to make, and a reported
//!     anomaly plus a reconnect loop is the cost of refusing it.
//!   * **An announcement discharges an outstanding attach**, and redirects a target
//!     that names a different thread: the wire's own announcement is better
//!     evidence than any resume answer could be.
//!
//! ### 2e-4b owns the reconciliation
//!
//! Every one of those fail-closed edges is a marker, not a wall. Turns run now, so
//! resume responses carry real `turns[]`, open items can survive a disconnect, and
//! completions can happen while the link is down — and the design for reconciling
//! them belongs to **2e-4b, grounded in that evidence**: whether `turns[]` is
//! complete for the turns it reports, whether item ids key uniquely across a resume
//! (D15 says they do not, inside an interrupted turn), and what a partial snapshot
//! must therefore be trusted for. Building it here would have meant choosing those
//! answers before anything could check them; the evidence now exists, captured at
//! `fixtures/codex/first-turn.jsonl` and
//! `fixtures/codex/resume-populated-answer.json`.
//!
//! The live gate carries the tripwire for exactly that moment: it runs a real turn
//! and asserts this link records **nothing** across it, because it is not
//! subscribed. When 2e-4b lands and the link accepts the populated answer, it
//! becomes subscribed and that assertion breaks — which is the signal that this
//! contract has been given its successor.
//!
//! One fact from A2 survives into the simple contract and is worth keeping in view:
//! `thread/read` can overtake `thread/resume` on the same connection, so anything
//! depending on the attach is sound only strictly after its response. This link has
//! exactly one request outstanding at a time and issues nothing between sending a
//! resume and seeing its answer.
//!
//! ## Ingress attribution (D4), and the single-generation reality of this chunk
//!
//! Every inbound frame is stamped at ingress with `(generation, upstream_epoch)` and
//! admitted by [`Visit::admits`]. A generation identifies a **visit**, not a thread,
//! and filtering retires a delivery generation, never a `thread_id`.
//!
//! **Exactly one of those terms decides anything today, and the honest statement is
//! sharper than "not yet".** This link has one connection and one registration, so
//! the stamp is built from the very [`Visit`] it is then compared against: at the
//! single call site the epoch and generation comparisons are a value against
//! itself, structurally — not a case that happens to be unreachable. They are the
//! *shape* the D2/D4 switch chunk fills in, once a second visit or a superseded
//! upstream can deliver a frame at all, and they are unit-tested as a pure function
//! over stamps this chunk cannot produce.
//!
//! D4 also names a `receive_seq` alongside them. It is **deliberately not here**:
//! nothing in this chunk orders anything by it, and a field stamped and discarded is
//! scaffolding rather than attribution. The switch chunk that linearizes on it will
//! add exactly what it consumes.
//!
//! The term that *is* live is the thread: a frame for a thread this link is not
//! bound to is dropped rather than normalized into this session's timeline — and
//! while it is unbound, so is *every* named frame, because a thread nothing has
//! announced is a claim this link cannot check.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use protocol::event::SessionKey;
use protocol::ipc::RegisterSession;
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

use crate::codex_adapter::CodexAdapter;
use crate::state::Daemon;

/// How long a dial of `ccd.sock` may take before it counts as a failed attempt.
const CONNECT_BUDGET: Duration = Duration::from_secs(10);

/// How long the WebSocket upgrade may take once the socket is connected. Separate
/// from the dial: `connect()` returning only means something accepted, and a peer
/// that accepts and then never speaks would otherwise hold this task for ever.
const UPGRADE_BUDGET: Duration = Duration::from_secs(15);

/// How long any single write to the leg may block. A peer that stops reading
/// applies backpressure, and an unbounded write parks the task where no other
/// deadline can reach it.
const SEND_BUDGET: Duration = Duration::from_secs(15);

/// How long the `initialize` handshake may take. The app-server answers it
/// immediately; a connection that does not is not a usable observation path.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(20);

/// How long one `thread/resume` may stay outstanding.
///
/// Shortened under `cfg(test)` so a test can tell **discharge** from **timeout**:
/// with the production budget, a scripted scenario that finishes in seconds could
/// never have timed out, so "no second resume" would prove nothing about which of
/// the two happened. At this budget a resume that was *not* discharged does time
/// out inside the observation window, and the two outcomes are distinguishable.
/// The live gate is unaffected in practice — the app-server answers in
/// milliseconds.
#[cfg(not(test))]
const RESUME_BUDGET: Duration = Duration::from_secs(60);
#[cfg(test)]
const RESUME_BUDGET: Duration = Duration::from_millis(1500);

/// Reconnect backoff bounds. A daemon restart or a wrapper that is still coming up
/// must cost a bounded retry, never a hot loop on a socket that is not there yet.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(250);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(15);

/// Attach-retry backoff bounds, for the *retryable* attach failure A1/D3 predicts:
/// a thread has no rollout until its first turn, and until then every resume fails.
/// The ceiling is what keeps a session whose operator has not typed yet from
/// costing a request per second for as long as it is open.
const ATTACH_BACKOFF_MIN: Duration = Duration::from_millis(500);
const ATTACH_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Byte-based WebSocket bounds. D8: a single notification reaches multi-MB
/// (`plugin/list` at 5.76 MB is the worst case measured), so the ceiling is on
/// bytes and generous enough that a legitimate large frame is never rejected.
fn ws_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(64 << 20),
        max_frame_size: Some(64 << 20),
        ..Default::default()
    }
}

/// The per-session **control-link fact** a Codex registration carries: where the
/// broker's ccd leg is, which visit the registration speaks for, and the thread it
/// was launched against.
///
/// Read off the wire by [`ControlLink::from_registration`], which is the fail-closed
/// gate: a Codex registration that does not carry a socket *and* a generation
/// cannot be observed and is refused, exactly as a registration for an agent this
/// daemon cannot host is refused. Claude carries none of these and gets `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlLink {
    /// The broker's ccd leg, under the launch's runtime dir.
    pub socket: PathBuf,
    /// The monotonic thread generation (the **visit**) this registration speaks
    /// for. Stamped onto every frame this link admits.
    pub generation: u64,
    /// The thread the launch bound this session to, when the supervisor knew it.
    /// Absent is legitimate on a first attach — `thread/started` is broadcast to a
    /// merely-initialized connection and carries the whole Thread (A1/D1/D2) — but
    /// a link with no id and no broadcast has nothing to resume, and says so.
    pub thread_id: Option<String>,
}

impl ControlLink {
    /// Read the control-link fact off a registration, failing closed.
    ///
    /// `Ok(None)` for Claude, which has no control link and must carry none (the
    /// caller has already refused a Claude frame carrying Codex identity). For
    /// Codex the socket and the generation are both **required**: this daemon's
    /// only structured observation of a Codex session is the connection they name,
    /// and a registration that omits either would install a session the daemon can
    /// record but never watch — a run that looks live in the fleet and reports
    /// nothing. That is the state the refusal exists to prevent.
    pub fn from_registration(info: &RegisterSession) -> Result<Option<ControlLink>> {
        if !matches!(info.agent, protocol::agent::AgentKind::Codex) {
            return Ok(None);
        }
        let socket = match info.codex_socket.as_deref() {
            Some(path) if !path.trim().is_empty() => PathBuf::from(path),
            _ => bail!(
                "refusing to register {}: a Codex registration carries no control-link socket, \
                 so the session could never be observed",
                info.session_id
            ),
        };
        let Some(generation) = info.codex_generation else {
            bail!(
                "refusing to register {}: a Codex registration carries no thread generation, \
                 so its frames could not be attributed to a visit",
                info.session_id
            )
        };
        Ok(Some(ControlLink {
            socket,
            generation,
            thread_id: info
                .codex_thread_id
                .as_deref()
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string),
        }))
    }
}

/// The ingress attribution stamped on one inbound frame (D4).
///
/// Recorded at the only place it can be known: the moment the frame is read off a
/// specific connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ingress {
    pub generation: u64,
    pub upstream_epoch: u64,
}

/// The **visit** this link is currently bound to: a generation, the connection
/// delivering it, and the thread whose frames belong to this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    pub generation: u64,
    pub upstream_epoch: u64,
    /// `None` until `thread/started` (or the registration) names the thread. Until
    /// then every frame is admitted: there is nothing yet to disagree with, and
    /// dropping the very broadcast that carries the identity would deadlock the
    /// binding.
    pub thread_id: Option<String>,
}

impl Visit {
    /// Is this frame admitted to the projection path (D4)?
    ///
    /// Three terms, and the doc is exact about which of them can vary in this
    /// chunk:
    ///
    ///   * **epoch** — a frame delivered by a connection this link has already
    ///     superseded is retired.
    ///   * **generation** — a frame delivered under a retired visit is retired.
    ///     Filtering retires a *generation*, never a thread id.
    ///
    ///     Both of those are inert at this chunk's only call site, and inert
    ///     *structurally*: `Connection::observe_notification` builds the stamp out of its own
    ///     `Visit`, so it compares each value with itself. They are asserted here
    ///     as a pure function, over stamps the caller cannot yet construct, because
    ///     the switch chunk is what makes the caller able to.
    ///   * **thread** — a frame naming a thread this link is not bound to is not
    ///     this session's fact. This one is live now: one app-server can serve more
    ///     than one thread, and normalizing another thread's items into this
    ///     session's timeline would be a lie the dedup key cannot catch.
    pub fn admits(&self, stamp: Ingress, frame_thread: FrameThread<'_>) -> bool {
        if stamp.upstream_epoch != self.upstream_epoch || stamp.generation != self.generation {
            return false;
        }
        match (self.thread_id.as_deref(), frame_thread) {
            // A frame naming two threads names none this link can act on, and the
            // adapter would still read one of them. Rejected outright, bound or not.
            (_, FrameThread::Conflicted) => false,
            (Some(bound), FrameThread::Named(seen)) => bound == seen,
            // **Unbound admits no named frame.** Until a `thread/started` on this
            // connection says which thread is this session's, a frame naming one is
            // a claim this link cannot check — and normalizing it would write
            // another thread's items under this session's uid, durably. The
            // announcement itself is consumed *before* this check
            // (`Connection::observe_notification`), so binding is never blocked by it.
            (None, FrameThread::Named(_)) => false,
            // A frame naming no thread at all is connection-scoped, and belongs to
            // whoever is on the connection: us.
            (_, FrameThread::Unnamed) => true,
        }
    }
}

/// The thread a frame is about: `params.threadId` for the item/turn families,
/// `params.thread.id` for `thread/started`, which carries the whole Thread object.
///
/// What kind of JSON-RPC frame this is, decided **before** anything asks what it is
/// about. Both loops route on this, so the two agree by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    /// A method-bearing frame: a notification from the app-server.
    Notification,
    /// No `method` member: a response to somebody's request.
    Response,
    /// A `method` that is present but not a string — `null`, a number, an object.
    /// Not a notification and not a response; this build cannot act on it, so it is
    /// counted and dropped rather than half-read. (`get("method").is_some()` would
    /// route `"method": null` into the notification path, where the adapter would
    /// then find no method and silently do nothing — the same outcome by accident
    /// rather than by decision.)
    Malformed,
}

fn frame_kind(frame: &Value) -> FrameKind {
    match frame.get("method") {
        None => FrameKind::Response,
        Some(Value::String(_)) => FrameKind::Notification,
        Some(_) => FrameKind::Malformed,
    }
}

/// A frame's subject, as three distinct facts. [`FrameThread::Conflicted`] is not
/// the same as [`FrameThread::Unnamed`] and must not collapse into it: an unnamed
/// frame is connection-scoped and belongs to us, while a frame naming *two* threads
/// is one the adapter would still namespace by whichever field it happens to read.
/// Only rejecting it outright keeps the two layers from disagreeing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameThread<'a> {
    Unnamed,
    Named(&'a str),
    Conflicted,
}

/// The thread a frame is about: `params.threadId` for the item/turn families,
/// `params.thread.id` for `thread/started`, which carries the whole Thread object.
/// Both are read, and if both are present they must agree — picking one would be
/// choosing which contradiction to believe.
fn frame_thread_id(frame: &Value) -> FrameThread<'_> {
    let Some(params) = frame.get("params") else {
        return FrameThread::Unnamed;
    };
    let flat = params
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let nested = params
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    match (flat, nested) {
        (Some(a), Some(b)) if a != b => FrameThread::Conflicted,
        (Some(id), _) | (None, Some(id)) => FrameThread::Named(id),
        (None, None) => FrameThread::Unnamed,
    }
}

/// Is this frame **exactly** the measured not-ready answer for the thread we asked
/// about?
///
/// The resume answer a thread that has not yet run a turn gives (A1/D3, captured by
/// `codex_link_live`'s claim 3 at the last moment it is true):
///
/// ```text
/// {"id":102,"error":{"code":-32600,
///                    "message":"no rollout found for thread id 01a0333d-…"}}
/// ```
///
/// Matched precisely rather than by keyword, because this predicate is the *only*
/// thing standing between "the wire behaved as measured" and "something happened
/// that this build has never seen". A substring test would let an unrelated error
/// mentioning a rollout — or a hostile one quoting the phrase — read as routine.
///
/// Three things are checked, and all three matter:
///
///   * **Response exclusivity.** A frame carrying both `result` and `error` is not
///     a JSON-RPC response at all; it is certainly not this one.
///   * **The exact code**, `-32600`.
///   * **The exact message**, for **the thread we asked about** — so an answer
///     about some other thread cannot be mistaken for our own not-ready state.
pub fn is_measured_not_ready(frame: &Value, requested_thread: &str) -> bool {
    if frame.get("result").is_some() {
        return false;
    }
    let Some(error) = frame.get("error") else {
        return false;
    };
    if error.get("code").and_then(Value::as_i64) != Some(NOT_READY_CODE) {
        return false;
    }
    error.get("message").and_then(Value::as_str)
        == Some(&format!("{NOT_READY_PREFIX}{requested_thread}"))
}

/// The JSON-RPC code the app-server answers a rollout-less resume with.
const NOT_READY_CODE: i64 = -32600;
/// Its message, up to the thread id. Captured verbatim from codex 0.147.
const NOT_READY_PREFIX: &str = "no rollout found for thread id ";

/// A frame's fingerprint **as it may appear in a log a human reads**.
///
/// Two arms rather than one string, because the difference between them is the
/// security property. A digest of the frame is a confirmation oracle: it goes to
/// a log, and anyone who can guess what the frame said can hash their guess and
/// check it against the line. Keying it with a secret this process drew from the
/// CSPRNG and never writes down removes that — a candidate frame cannot be turned
/// into a candidate digest by anyone who does not hold the salt, and nobody does.
///
/// When there is no such secret there is **no weaker digest to fall back to**.
/// The field is absent instead, and the report says so in fixed vocabulary, so a
/// reader can tell the difference between a digest that is missing by design and
/// one that is missing by accident.
///
/// The salt is per-process, which is exactly the lifetime the throttle would need
/// if it rode this — it does not, deliberately; see [`frame_discriminator`]. Two
/// reports in one log are still comparable to each other. Across a restart the
/// digests change, so they carry nothing between runs — the point, not a
/// shortcoming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublicDigest {
    /// 16 hex characters of an HMAC tag under this process's CSPRNG salt.
    Keyed(String),
    /// The CSPRNG refused. Rendered as [`DIGEST_UNAVAILABLE`], never as a value.
    Unavailable,
}

/// What the report's `digest` field says when there is no digest to say. Fixed
/// vocabulary, like every other non-numeric word the report prints.
const DIGEST_UNAVAILABLE: &str = "unavailable";

impl std::fmt::Display for PublicDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keyed(hex) => formatter.write_str(hex),
            Self::Unavailable => formatter.write_str(DIGEST_UNAVAILABLE),
        }
    }
}

/// This frame's loggable digest, keyed by this process's salt if it has one.
///
/// HMAC-SHA256 from `ring`, which ccd already depends on for the APNs signature —
/// a keyed hash this build can spell without a new crate. `Value` serializes its
/// objects through a `BTreeMap`, so the text this signs is key-ordered.
// Crate-visible so the live gate can assert the exact line the daemon would emit,
// against the LIVE answer rather than only the fixture.
pub(crate) fn frame_digest(frame: &Value) -> PublicDigest {
    frame_digest_salted(digest_salt(), frame)
}

/// [`frame_digest`] against an explicit salt, so the test suite can prove the
/// digest is keyed by watching two independently-salted instances disagree about
/// the same frame — and can force the no-salt path, which on macOS is otherwise
/// unreachable because `getrandom` there does not fail.
///
/// **The two cases are separate arms on purpose.** A fallback that quietly keyed
/// the digest with something else would leave the log looking identical while the
/// oracle was back: the clock and the pid are low-entropy and searchable, so an
/// attacker who knows roughly when ccd started can enumerate candidate salts and
/// recompute the digest from a guessed frame. There is therefore no salt-shaped
/// value to substitute here — reaching the `sign` below requires a real key, and
/// restoring a fallback would mean writing one out of thin air in full view
/// rather than editing one branch of an `unwrap_or`.
fn frame_digest_salted(salt: Option<&ring::hmac::Key>, frame: &Value) -> PublicDigest {
    let Some(salt) = salt else {
        return PublicDigest::Unavailable;
    };
    let tag = ring::hmac::sign(salt, frame.to_string().as_bytes());
    PublicDigest::Keyed(
        tag.as_ref()[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// This process's digest salt, drawn once on first use — and `None` for the whole
/// life of a process whose CSPRNG refused, so the refusal is uniform rather than
/// per-call.
fn digest_salt() -> Option<&'static ring::hmac::Key> {
    static SALT: OnceLock<Option<ring::hmac::Key>> = OnceLock::new();
    SALT.get_or_init(mint_digest_salt).as_ref()
}

/// Draw one salt from the system CSPRNG.
///
/// `SystemRandom::fill` is fallible in the type system, and a logging path is the
/// wrong place to abort a daemon — so a failure is a `None` that costs the log its
/// digest field, and nothing else. It is never a weaker salt: see
/// [`frame_digest_salted`].
fn mint_digest_salt() -> Option<ring::hmac::Key> {
    let mut salt = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut salt).ok()?;
    Some(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &salt))
}

/// Tell two answers apart **inside this process**, for [`AmendThrottle`] only.
///
/// The throttle's whole question is "the same answer as last time?", and that
/// comparison never leaves the process — so it needs no secret, no keying, and
/// nothing that would survive a restart. A non-cryptographic hash answers it, and
/// answers it whether or not the CSPRNG produced a salt, which is why a missing
/// digest costs the log a field and does not cost the daemon its throttle.
///
/// **Compared, never rendered.** It is a `u64` and the report is built from a
/// [`PublicDigest`]: neither [`describe_resume_answer`] nor
/// [`stop_and_amend_report`] can see this value, so there is no edit to one of
/// them that puts it in a log line.
fn frame_discriminator(frame: &Value) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&frame.to_string(), &mut hasher);
    std::hash::Hasher::finish(&hasher)
}

/// Describe a `thread/resume` answer down to the parts that cannot carry text.
///
/// **The frame itself may never reach the log, however useful the dump would be
/// while debugging.** Once turns run, the answer is a populated `result`: it
/// carries the user's prompt and the assistant's reply verbatim under
/// `thread.turns[].items[]`, the rollout file's path under `CODEX_HOME`, the
/// session's real `cwd`, and its workspace roots. That is the session's content,
/// and ccd's log is not where it goes. Anyone tempted to restore `Frame: {frame}`
/// here is restoring a content leak that a reconnect loop then repeats for the
/// life of the daemon.
///
/// What survives is structure, and it is chosen so that none of it can be a
/// carrier:
///
///   * whether the answer was a `result` or an `error`;
///   * for an error, its `code` — structural. The `message` is **not**: the
///     measured not-ready one already embeds a thread id, and a future one could
///     embed anything;
///   * how many turns a result claims, which is the fact that says *why* this
///     branch fired;
///   * **yes/no for each of the top-level `result` keys named in
///     [`DESCRIBED_RESULT_KEYS`]**, and a **count** of the others. Never their
///     names: a key name is peer-supplied text, unbounded in number and content
///     and drawn from no fixed vocabulary, so printing the frame's keys is
///     printing the frame. Every word of this section that is not a number comes
///     from the constant below;
///   * the digest, so two occurrences can be compared without either being read —
///     or, when this process has no salt to key one with, the fixed
///     [`DIGEST_UNAVAILABLE`] marker in its place. A [`PublicDigest`] rather than
///     a string, so the only fingerprint this function can render is the one
///     that is safe to render.
// Crate-visible so the live gate can assert the exact line the daemon would emit,
// against the LIVE answer rather than only the fixture.
pub(crate) fn describe_resume_answer(frame: &Value, digest: &PublicDigest) -> String {
    if let Some(result) = frame.get("result") {
        let Some(map) = result.as_object() else {
            return format!("a result (not an object; digest {digest})");
        };
        // Rendered for every allowlisted key whether present or not, so the line's
        // vocabulary is the same for every frame and says nothing about this one
        // beyond the yes/no this code chose to ask for.
        let flags = DESCRIBED_RESULT_KEYS
            .iter()
            .map(|key| {
                let present = if map.contains_key(*key) { "yes" } else { "no" };
                format!("{key}={present}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        let known = DESCRIBED_RESULT_KEYS
            .iter()
            .filter(|key| map.contains_key(**key))
            .count();
        let others = map.len() - known;
        let turns = match result.pointer("/thread/turns").and_then(Value::as_array) {
            Some(turns) => turns.len().to_string(),
            None => "none".to_string(),
        };
        return format!(
            "a result ({flags} turns={turns}; {others} other top-level keys; digest {digest})"
        );
    }
    if let Some(error) = frame.get("error") {
        let code = match error.get("code").and_then(Value::as_i64) {
            Some(code) => code.to_string(),
            None => "none".to_string(),
        };
        return format!("an error (code {code}; digest {digest})");
    }
    format!("neither a result nor an error (digest {digest})")
}

/// The only `result` key names that may ever be written to the log, spelled here
/// rather than read off the frame.
///
/// These are the keys 2e-4b will be written against, so their presence or absence
/// is the fact a human needs; anything else in the answer is counted, not named.
/// Sorted, so the same answer always renders the same line.
const DESCRIBED_RESULT_KEYS: [&str; 5] = ["approvalPolicy", "cwd", "model", "sandbox", "thread"];

/// How long the STOP-AND-AMEND report stays quiet after one has been emitted for
/// the same answer to the same thread.
///
/// The refusal **ends the connection**, so `run` reconnects and asks again — and
/// a thread that has run a turn answers with the same populated result every
/// cycle, at a reconnect backoff that tops out at 15s. Reporting every one of
/// them wrote six copies of the report in fifteen seconds on the live gate, and
/// would have kept doing it for as long as the daemon lived. One report per
/// window, with the swallowed count attached to the next, keeps a permanent
/// condition visible without the log becoming the thing that reports it.
const AMEND_REPORT_QUIET: Duration = Duration::from_secs(300);

/// The STOP-AND-AMEND report's own throttle.
///
/// Owned by [`run`] and lent to each connection, because the loop that repeats
/// the report **is** the reconnect loop: state scoped to one connection would be
/// rebuilt on every occurrence and would suppress nothing.
#[derive(Debug, Default)]
struct AmendThrottle {
    /// The last answer reported, and what has happened since.
    last: Option<AmendReport>,
}

/// One reported answer, held only as the things that identify it.
#[derive(Debug)]
struct AmendReport {
    thread: String,
    /// The [`frame_discriminator`], never the frame and never the loggable
    /// [`PublicDigest`] — this is compared in memory and has no path to a log.
    answer: u64,
    at: Instant,
    suppressed: u64,
}

impl AmendThrottle {
    /// Decide whether this occurrence is reported. `Some(n)` reports it, carrying
    /// how many occurrences were swallowed since the previous report; `None`
    /// suppresses it and counts it.
    ///
    /// A different thread, or a different answer for the same thread, is a new
    /// fact and is reported at once — the throttle only holds down a repeat of
    /// the thing it has already said.
    fn admit(&mut self, now: Instant, thread: &str, answer: u64) -> Option<u64> {
        let same = self
            .last
            .as_ref()
            .is_some_and(|last| last.thread == thread && last.answer == answer);
        if same {
            let last = self.last.as_mut().expect("matched just above");
            if now.duration_since(last.at) < AMEND_REPORT_QUIET {
                last.suppressed += 1;
                return None;
            }
        }
        let suppressed = match self.last.take() {
            Some(last) if same => last.suppressed,
            _ => 0,
        };
        self.last = Some(AmendReport {
            thread: thread.to_string(),
            answer,
            at: now,
            suppressed: 0,
        });
        Some(suppressed)
    }
}

/// The whole STOP-AND-AMEND report, prose and redacted shape, ready to log.
///
/// A free function so the test suite can assert on the exact line that reaches
/// stderr rather than on a piece of it — the redaction is only worth anything if
/// what is *logged* is what was checked.
// Crate-visible so the live gate can assert the exact line the daemon would emit,
// against the LIVE answer rather than only the fixture.
pub(crate) fn stop_and_amend_report(
    session: &str,
    requested_thread: &str,
    shape: &str,
    suppressed: u64,
) -> String {
    let repeats = match suppressed {
        0 => String::new(),
        n => format!(" (plus {n} suppressed since the previous report.)"),
    };
    format!(
        "codex link for {session}: STOP-AND-AMEND — thread/resume for \
         {requested_thread} answered with something this build cannot read. The \
         only answer it is designed for is the not-ready error a thread with no \
         rollout gives; a turn that has run answers with a populated turns[] \
         instead, and reconciling that is 2e-4b's, not this chunk's. Guessing at \
         it is exactly what this chunk refuses to do. Reconnecting. The answer, \
         described rather than quoted: {shape}.{repeats}"
    )
}

/// Where the attach stands on the connection now in hand.
#[derive(Debug)]
enum Attach {
    /// Nothing to attach: either no thread is known yet, or the one that is was
    /// **announced on this connection**. A link that arrives before its thread
    /// hears `thread/started` (broadcast to a merely-initialized connection, A1/D2)
    /// and holds the stream from the thread's first frame onwards, so there is no
    /// history to recover; resuming would only ask the app-server to replay what
    /// this connection already has. An announcement therefore *discharges* an
    /// outstanding attach — see `serve_connection`.
    Unbound,
    /// A `thread/resume` is outstanding. Nothing else is sent until its response is
    /// observed (A2: never pipelined). `next_delay` rides along so a retryable
    /// answer keeps the backoff it inherited rather than restarting it.
    Awaiting {
        id: i64,
        deadline: Instant,
        next_delay: Duration,
        /// The thread this resume asked about. Carried so the answer is checked
        /// against what was *requested* rather than against a binding that may have
        /// moved, and so a response naming no thread can be read as answering it.
        target: String,
    },
    /// A retryable attach failure is being waited out while observation continues.
    Backoff {
        until: Instant,
        next_delay: Duration,
    },
    /// The answer was not the measured not-ready one. Never a resting state: the
    /// connection ends and reconnects. The target is kept — it came from an
    /// announcement, and an answer this build cannot read is evidence about the
    /// answer, not about the thread.
    Refused,
}

impl Attach {
    /// An attach that is due immediately, at the floor of the retry backoff.
    fn due_now() -> Attach {
        Attach::Backoff {
            until: Instant::now(),
            next_delay: ATTACH_BACKOFF_MIN,
        }
    }
}

/// Hold one Codex session's control link for as long as the session is registered.
///
/// Never returns on its own: a lost connection is a reconnect, not an end. The
/// caller ends it by **aborting** the task (`Daemon::unregister_supervisor`) —
/// dropping a `JoinHandle` only detaches the task, so an abort is what actually
/// stops one. Teardown is bounded because nothing waits on the abort: the task
/// stops at its next await point, and every fact it produced was already durable
/// when it produced it, so a cancellation can cost a live subscriber the *push* of
/// a final event, never the event.
pub async fn run(daemon: Arc<Daemon>, session: SessionKey, link: ControlLink) {
    let mut adapter = CodexAdapter::new(session.clone());
    let mut thread_id = link.thread_id.clone();
    let mut upstream_epoch: u64 = 0;
    let mut backoff = RECONNECT_BACKOFF_MIN;
    // Lives out here rather than inside a connection: the STOP-AND-AMEND branch
    // ends the connection, so the repeat it has to throttle is this very loop.
    let mut amend = AmendThrottle::default();

    crate::log_info!(
        "codex link for {} ({}) attaching to {} at generation {}",
        session.name,
        session.uid,
        link.socket.display(),
        link.generation
    );

    loop {
        // Never reused, and incremented before the dial so a connection that fails
        // to establish still consumes its epoch — an epoch names an attempt at an
        // upstream, not a successful one.
        upstream_epoch += 1;
        let started = Instant::now();
        let outcome = serve_connection(
            &daemon,
            &session,
            &link,
            upstream_epoch,
            &mut thread_id,
            &mut adapter,
            &mut amend,
        )
        .await;
        match outcome {
            // A clean end is the wrapper going away, which the supervisor's own
            // disconnect will report; until it does, reattaching is correct.
            Ok(()) => crate::log_debug!("codex link for {}: connection closed", session.name),
            Err(err) => crate::log_debug!("codex link for {}: {err:#}", session.name),
        }
        // The backoff is reset by a connection that **lasted**, not by one that
        // merely ended cleanly. A leg the broker accepts and closes at once, or an
        // app-server that dies as it starts, ends cleanly every time — and treating
        // that as healthy would turn the floor into a permanent reconnect every
        // 250 ms for the life of the session.
        backoff = if started.elapsed() >= RECONNECT_BACKOFF_MAX {
            RECONNECT_BACKOFF_MIN
        } else {
            (backoff * 2).min(RECONNECT_BACKOFF_MAX)
        };
        tokio::time::sleep(backoff).await;
    }
}

/// One connection, end to end: dial, handshake, attach, observe until it ends.
async fn serve_connection(
    daemon: &Arc<Daemon>,
    session: &SessionKey,
    link: &ControlLink,
    upstream_epoch: u64,
    thread_id: &mut Option<String>,
    adapter: &mut CodexAdapter,
    amend: &mut AmendThrottle,
) -> Result<()> {
    let stream = tokio::time::timeout(CONNECT_BUDGET, UnixStream::connect(&link.socket))
        .await
        .with_context(|| format!("dialling {} timed out", link.socket.display()))?
        .with_context(|| format!("dialling {}", link.socket.display()))?;
    // ccd is the WebSocket *client* to the broker's ccd leg, the same handshake the
    // broker itself performs against the app-server. Bounded separately from the
    // dial: `connect()` returning only proves something accepted the socket, and a
    // peer that accepts and then never completes the upgrade would hold this task
    // open indefinitely.
    let (mut ws, _resp) = tokio::time::timeout(
        UPGRADE_BUDGET,
        tokio_tungstenite::client_async_with_config("ws://localhost/", stream, Some(ws_config())),
    )
    .await
    .map_err(|_| anyhow::anyhow!("the ccd leg's WebSocket upgrade stalled for {UPGRADE_BUDGET:?}"))?
    .context("the ccd leg refused the WebSocket handshake")?;

    // **The target and the binding are different things.** A thread id carried in
    // — from the registration's hint or from a previous connection — says what to
    // resume; it does not say this connection is bound. Only a `thread/started`
    // watched on THIS connection binds, so `visit.thread_id` starts empty and the
    // D4 filter rejects every NAMED frame until one arrives.
    let mut resume_target = thread_id.clone();
    let mut conn = Connection {
        daemon,
        session,
        adapter,
        visit: Visit {
            generation: link.generation,
            upstream_epoch,
            thread_id: None,
        },
        filtered: 0,
        next_id: 1,
        amend,
    };

    // The handshake can bind (a `thread/started` may arrive before the `initialize`
    // answer), so a binding is published outwards even when the handshake then
    // fails — losing it would send the next connection back to square one. Only
    // ever **set**, never cleared: `clone_from` on a `None` would wipe a target
    // this link carried across a reconnect, which is the one thing the next
    // connection needs.
    let handshook = conn.handshake(&mut ws).await;
    if let Some(bound) = conn.bound() {
        resume_target = Some(bound.to_string());
        *thread_id = Some(bound.to_string());
    }
    handshook?;

    // An announcement on this connection already gives the whole stream, so only a
    // target this connection did NOT watch start is attached to by resume.
    let mut attach = if conn.bound().is_none() && resume_target.is_some() {
        Attach::due_now()
    } else {
        Attach::Unbound
    };

    loop {
        // The one place a request is issued. Reached only when no request is
        // outstanding, which is what keeps the resume unpipelined (A2).
        if let Attach::Backoff { until, next_delay } = &attach {
            if Instant::now() >= *until {
                let next_delay = *next_delay;
                match resume_target.clone() {
                    Some(target) => {
                        // The deadline starts when the attach BEGINS, not when the
                        // write returns: a `send_resume` that itself blocks is part
                        // of the time the attach has taken, and starting the clock
                        // afterwards would grant it a fresh budget on top.
                        let deadline = Instant::now() + RESUME_BUDGET;
                        let id = conn.send_resume(&mut ws, &target).await?;
                        attach = Attach::Awaiting {
                            id,
                            deadline,
                            next_delay,
                            target,
                        };
                    }
                    // The id went away between states; nothing to resume.
                    None => attach = Attach::Unbound,
                }
            }
        }

        // Checked here rather than only in the timeout arm below: tokio polls the
        // inner future first, so a connection delivering frames continuously would
        // otherwise starve an elapsed resume deadline indefinitely.
        if let Attach::Awaiting { id, deadline, .. } = &attach {
            if Instant::now() >= *deadline {
                bail!("thread/resume (id {id}) went unanswered within {RESUME_BUDGET:?}");
            }
        }

        let deadline = match &attach {
            Attach::Awaiting { deadline, .. } => Some(*deadline),
            Attach::Backoff { until, .. } => Some(*until),
            _ => None,
        };

        let next = match deadline {
            // A deadline that expires is handled at the top of the next iteration:
            // an owed backoff sends its resume, an elapsed `Awaiting` bails.
            Some(at) => match tokio::time::timeout_at(at, ws.next()).await {
                Ok(next) => next,
                Err(_) => continue,
            },
            None => ws.next().await,
        };

        let msg = match next {
            Some(Ok(msg)) => msg,
            Some(Err(err)) => bail!("read error on the ccd leg: {err}"),
            None => return Ok(()),
        };
        let text = match msg {
            Message::Text(text) => text,
            Message::Close(_) => return Ok(()),
            // Control frames are terminated per hop; the app-server protocol has no
            // binary form, so one is not a frame this link can read.
            _ => continue,
        };

        // **Parse, then check the deadline, then act.** Splitting the parse from the
        // mutation is the whole point of the order: reading and normalizing a frame
        // is real work (D8: a single notification reached 4.0 MB on the live gate),
        // so a frame that arrives just inside the budget can finish parsing well
        // outside it — and a response or an announcement that crossed the deadline
        // must not be allowed to mutate anything on its way past.
        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            // Unparseable: the adapter's contract is that a malformed frame mutates
            // nothing, and that is the same rule one layer out.
            continue;
        };
        if let Attach::Awaiting { id, deadline, .. } = &attach {
            if Instant::now() >= *deadline {
                bail!("thread/resume (id {id}) went unanswered within {RESUME_BUDGET:?}");
            }
        }

        // **Kind before attribution.** A frame whose id matches the request this
        // link has outstanding is OUR response, and it is settled as one whatever
        // it does or does not say about a thread. Routing it through the named-frame
        // filter first would drop it while unbound — so the measured error would
        // time the attach out instead of retrying it, and an unmeasured one would
        // never reach the STOP-AND-AMEND log that exists to report it.
        let kind = frame_kind(&frame);
        let is_our_response = kind == FrameKind::Response
            && matches!(&attach, Attach::Awaiting { id, .. }
                if frame.get("id").and_then(Value::as_i64) == Some(*id));
        if !is_our_response {
            match kind {
                FrameKind::Notification => conn.observe_notification(&frame).await,
                // Not the answer this link is waiting on (a late one, or one from a
                // superseded attach), or not a frame at all. Neither goes near the
                // bind/filter/normalize path: that path is for notifications, and
                // this keeps the code saying what the contract says.
                FrameKind::Response | FrameKind::Malformed => conn.drop_unroutable(kind),
            }
        }

        // **A binding discharges an outstanding attach, and redirects the target.**
        //
        // The announcement is better evidence than any resume answer could be: this
        // connection now holds the thread's stream from its first frame, so there is
        // nothing left to recover and nothing left to ask for. And the thread it
        // names is this session's — a target naming a different one is stale, from a
        // registration hint or a thread the session has moved on from.
        //
        // The redirect is done by **publishing outward**, which is what the next
        // connection reads as its target. There is deliberately no local reassignment
        // beside it: this connection stops attaching here, so a local target would
        // never be read again, and code with no reachable effect is code that lies
        // about what it does.
        if let Some(bound) = conn.bound() {
            if thread_id.as_deref() != Some(bound) {
                if let Some(stale) = thread_id.as_deref() {
                    crate::log_info!(
                        "codex link for {}: {bound} was announced on this connection; \
                         the next attach will target it rather than {stale}",
                        session.name
                    );
                }
                // Only ever set, never cleared: a target that survived a reconnect is
                // still the best thing the next connection has to attach with.
                thread_id.clone_from(&conn.visit.thread_id);
            }
            if !matches!(attach, Attach::Unbound) {
                attach = Attach::Unbound;
            }
        }

        // The only response this link can receive is the answer to its own resume.
        if is_our_response {
            if let Attach::Awaiting {
                next_delay, target, ..
            } = &attach
            {
                let (next_delay, target) = (*next_delay, target.clone());
                attach = conn.settle_resume(&frame, &target, next_delay);
            }
        }
        // Any answer but the measured one ends the connection. The target is kept:
        // it came from an announcement or the registration hint, and a wire answer
        // this build cannot read is evidence about the answer, not about the thread.
        if matches!(attach, Attach::Refused) {
            bail!("thread/resume was not the measured not-ready answer; reconnecting");
        }
    }
}

/// Everything scoped to one connection: who to record for, what has been admitted,
/// and the ids this link has issued.
struct Connection<'a> {
    daemon: &'a Arc<Daemon>,
    session: &'a SessionKey,
    adapter: &'a mut CodexAdapter,
    visit: Visit,
    /// Frames dropped because they named a thread this connection is not bound to.
    /// Counted and dropped: pre-2e-4b the only thread this link can be bound to is
    /// one it watched start, so a frame for any other is not this session's.
    filtered: u64,
    next_id: i64,
    /// The STOP-AND-AMEND report's throttle, lent by [`run`] so it outlives this
    /// connection — which is the only way it can throttle a reconnect loop.
    amend: &'a mut AmendThrottle,
}

impl Connection<'_> {
    /// The ccd role's handshake: `initialize`, then the one notification the leg
    /// forwards. Every frame that arrives before the answer is a real fact and is
    /// observed on the way past rather than discarded.
    async fn handshake<S>(&mut self, ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            ws,
            json!({
                "id": id,
                "method": "initialize",
                "params": {"clientInfo": {
                    "name": "codeconnect-ccd",
                    "title": "CodeConnect daemon",
                    "version": env!("CARGO_PKG_VERSION"),
                }},
            }),
        )
        .await?;

        let deadline = Instant::now() + HANDSHAKE_BUDGET;
        let response = loop {
            // Checked against ELAPSED time at the top, not only inside the timeout:
            // `timeout_at` polls its inner future first, so a peer flooding
            // unsolicited frames — every one of which costs a parse — could
            // otherwise keep this loop fed past its deadline for ever. The budget
            // covers the parsing too, which is the work the flood is made of.
            if Instant::now() >= deadline {
                bail!("initialize went unanswered within {HANDSHAKE_BUDGET:?}");
            }
            let next = tokio::time::timeout_at(deadline, ws.next())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("initialize went unanswered within {HANDSHAKE_BUDGET:?}")
                })?;
            let msg = match next {
                Some(Ok(msg)) => msg,
                Some(Err(err)) => bail!("read error during the handshake: {err}"),
                None => bail!("the ccd leg closed during the handshake"),
            };
            let Message::Text(text) = msg else { continue };
            // Parse, then check the deadline, then act — the same order the main
            // loop uses, and for the same reason: parsing is work, and a frame that
            // finishes it past the deadline must not mutate anything.
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if Instant::now() >= deadline {
                bail!("initialize went unanswered within {HANDSHAKE_BUDGET:?}");
            }
            // Kind before attribution: our own answer is settled as a response
            // whatever it says about a thread.
            // Kind before attribution, exactly as in the main loop: only the
            // response to OUR initialize breaks the loop; other notifications are
            // observed; unmatched responses and malformed frames go nowhere near
            // the bind/filter/normalize path.
            let kind = frame_kind(&frame);
            if kind == FrameKind::Response && frame.get("id").and_then(Value::as_i64) == Some(id) {
                break frame;
            }
            match kind {
                FrameKind::Notification => self.observe_notification(&frame).await,
                FrameKind::Response | FrameKind::Malformed => self.drop_unroutable(kind),
            }
        };
        if let Some(error) = response.get("error") {
            bail!("initialize refused: {error}");
        }
        // A success is a **result object**, not merely the absence of an error: a
        // bare `{"id":1}` is not a handshake, and treating it as one would leave
        // this link observing a connection the app-server never accepted.
        if !response.get("result").is_some_and(Value::is_object) {
            bail!("initialize answered with neither an error nor a result object");
        }

        self.send(ws, json!({"method": "initialized", "params": {}}))
            .await?;
        crate::log_info!(
            "codex link for {}: initialized on the ccd leg (epoch {})",
            self.session.name,
            self.visit.upstream_epoch
        );
        Ok(())
    }

    /// Ask the broker to re-attach this link to its thread. The broker validates the
    /// target against the thread set it learned from its own server→client stream;
    /// this link emits no ownership fields, which `thread/resume` is exempt from
    /// requiring and which is what makes the attach path privilege-free.
    async fn send_resume<S>(
        &mut self,
        ws: &mut tokio_tungstenite::WebSocketStream<S>,
        thread_id: &str,
    ) -> Result<i64>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            ws,
            json!({"id": id, "method": "thread/resume", "params": {"threadId": thread_id}}),
        )
        .await?;
        Ok(id)
    }

    /// Consume a `thread/resume` answer. **Exactly one shape is acceptable.**
    ///
    /// The measured not-ready error, for the thread we asked about — and nothing
    /// else. Not a success with an empty `turns[]`, not a success of any shape, not
    /// another error. See the module doc: a thread that has not yet run a turn has
    /// no rollout, so the not-ready error is the only answer it can give, and it is
    /// the only one this build was designed against.
    ///
    /// Once a turn has run, the wire answers instead with a `result` carrying a
    /// populated `turns[]` — and that lands here too, on this branch, deliberately.
    /// Accepting it would be normalizing a response whose completeness and keying
    /// nothing in this chunk has validated, on a guess about what it means. The
    /// point of failing closed here is that the guess is never made — the answer is
    /// *reported*, loudly, with what a human needs in order to decide: its shape.
    /// 2e-4b is what turns that report into an attach.
    ///
    /// The report is **described, never quoted** ([`describe_resume_answer`]) and
    /// **throttled** ([`AmendThrottle`]) — a populated answer is the session's
    /// content, and this branch recurs every reconnect for as long as the
    /// condition lasts.
    fn settle_resume(
        &mut self,
        frame: &Value,
        requested_thread: &str,
        next_delay: Duration,
    ) -> Attach {
        if is_measured_not_ready(frame, requested_thread) {
            crate::log_debug!(
                "codex link for {}: {requested_thread} has no rollout yet; retrying the \
                 attach in {next_delay:?}",
                self.session.name
            );
            return Attach::Backoff {
                until: Instant::now() + next_delay,
                next_delay: (next_delay * 2).min(ATTACH_BACKOFF_MAX),
            };
        }
        // The throttle rides the in-process discriminator and the log rides the
        // keyed digest: two values, because one of them must never be logged and
        // the other must never be the reason the throttle stops working.
        let answer = frame_discriminator(frame);
        if let Some(suppressed) = self.amend.admit(Instant::now(), requested_thread, answer) {
            crate::log_error!(
                "{}",
                stop_and_amend_report(
                    &self.session.name,
                    requested_thread,
                    &describe_resume_answer(frame, &frame_digest(frame)),
                    suppressed,
                )
            );
        }
        Attach::Refused
    }

    /// Stamp one inbound frame at ingress, admit it or retire it, and — for a
    /// notification — normalize it and record whatever it turned into.
    ///
    /// Takes an already-parsed frame and returns nothing: the caller parses, checks
    /// its deadline, and routes by [`frame_kind`] before anything reaches here, so
    /// this only ever sees notifications.
    /// Count and drop a frame neither loop can route: a response to a request this
    /// link is not waiting on, or something that is not a JSON-RPC frame at all.
    fn drop_unroutable(&mut self, kind: FrameKind) {
        self.filtered += 1;
        crate::log_debug!(
            "codex link for {}: dropped an unroutable frame ({kind:?}; {} dropped so \
             far)",
            self.session.name,
            self.filtered
        );
    }

    /// Bind, filter and normalize one **notification**. Responses never reach here
    /// at all — correlated or not, the caller routes by frame kind first — so
    /// everything below may assume a method-bearing frame.
    async fn observe_notification(&mut self, frame: &Value) {
        // **The announcement is consumed before the filter, not after.** While
        // unbound the filter rejects every named frame, so a `thread/started`
        // checked first would be dropped by the very rule it exists to satisfy.
        self.bind_if_unbound(frame);
        let stamp = Ingress {
            generation: self.visit.generation,
            upstream_epoch: self.visit.upstream_epoch,
        };
        if !self.visit.admits(stamp, frame_thread_id(frame)) {
            // Counted, not ingested. The connection continues: another
            // thread's frame is not a reason to tear down an otherwise healthy leg,
            // and it is emphatically not this session's fact.
            self.filtered += 1;
            crate::log_debug!(
                "codex link for {}: filtered a frame at {stamp:?} ({} so far)",
                self.session.name,
                self.filtered
            );
            return;
        }
        self.ingest_frame(frame).await;
    }

    /// Normalize one admitted frame and record whatever it turned into.
    async fn ingest_frame(&mut self, frame: &Value) {
        for pending in self.adapter.ingest(frame) {
            self.record(pending).await;
        }
    }

    async fn record(&self, pending: protocol::event::PendingEvent) {
        if let Err(err) = self.daemon.ingest(pending).await {
            crate::log_error!(
                "codex link for {}: could not record a fact: {err:#}",
                self.session.name
            );
        }
    }

    /// Bind this connection to a thread, from the **`thread/started` it watched
    /// arrive**. That is the only thing that binds — see the module doc's
    /// pre-2e-4b contract.
    fn bind_if_unbound(&mut self, frame: &Value) {
        if self.visit.thread_id.is_some() {
            return;
        }
        if frame.get("method").and_then(Value::as_str) != Some("thread/started") {
            return;
        }
        let FrameThread::Named(id) = frame_thread_id(frame) else {
            return;
        };
        crate::log_info!(
            "codex link for {}: bound to thread {id}, announced on this connection",
            self.session.name
        );
        self.visit.thread_id = Some(id.to_string());
    }

    /// The thread this connection is bound to, if it has been announced.
    fn bound(&self) -> Option<&str> {
        self.visit.thread_id.as_deref()
    }

    async fn send<S>(
        &self,
        ws: &mut tokio_tungstenite::WebSocketStream<S>,
        frame: Value,
    ) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        // Bounded: a peer that has stopped reading applies backpressure through the
        // socket buffer, and an unbounded `send` would park this task there for
        // ever — a link that is neither observing nor reconnecting, and that no
        // timeout above it can reach.
        tokio::time::timeout(SEND_BUDGET, ws.send(Message::Text(frame.to_string())))
            .await
            .map_err(|_| anyhow::anyhow!("a write to the ccd leg blocked for {SEND_BUDGET:?}"))?
            .context("writing to the ccd leg")
    }
}

/// A test database that removes itself, siblings and all.
///
/// SQLite leaves a `-wal` and a `-shm` beside the file it is given, so "delete the
/// db" has to mean all three. Shared with [`crate::codex_link_live`], and not
/// fussiness: a suite that leaves one database per run in the temp directory is how
/// a machine ends up with tens of gigabytes of them.
#[cfg(test)]
pub struct TempDb(PathBuf);

#[cfg(test)]
impl TempDb {
    pub fn new(stem: &str) -> TempDb {
        let path = std::env::temp_dir().join(format!("{stem}.db"));
        let db = TempDb(path);
        db.remove();
        db
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn remove(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.0.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(path));
        }
    }
}

#[cfg(test)]
impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::agent::AgentKind;

    fn registration(agent: AgentKind) -> RegisterSession {
        RegisterSession {
            session_id: "cc-1".into(),
            session_uid: Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR".into()),
            tmux_session: "cc-1".into(),
            tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
            cwd: "/tmp".into(),
            supervisor_pid: 4242,
            claude_bin: None,
            agent,
            agent_bin: None,
            codex_thread_id: None,
            codex_socket: None,
            codex_generation: None,
            started_at: protocol::time::now_rfc3339(),
            protocol_minor: protocol::PROTOCOL_MINOR,
        }
    }

    // ------------------------------------------------ the registration fact

    #[test]
    fn a_claude_registration_has_no_control_link() {
        assert_eq!(
            ControlLink::from_registration(&registration(AgentKind::Claude)).unwrap(),
            None
        );
    }

    #[test]
    fn a_codex_registration_missing_the_fact_is_refused() {
        // No socket and no generation.
        let bare = registration(AgentKind::Codex);
        assert!(ControlLink::from_registration(&bare).is_err(), "neither");

        // A generation with nowhere to observe it.
        let no_socket = RegisterSession {
            codex_generation: Some(1),
            ..registration(AgentKind::Codex)
        };
        assert!(
            ControlLink::from_registration(&no_socket).is_err(),
            "a generation with no socket"
        );

        // A socket with no visit to attribute its frames to.
        let no_generation = RegisterSession {
            codex_socket: Some("/tmp/cch.x/ccd.sock".into()),
            ..registration(AgentKind::Codex)
        };
        assert!(
            ControlLink::from_registration(&no_generation).is_err(),
            "a socket with no generation"
        );

        // A present-but-empty socket is an absent socket, not a path.
        let empty_socket = RegisterSession {
            codex_socket: Some("   ".into()),
            codex_generation: Some(1),
            ..registration(AgentKind::Codex)
        };
        assert!(
            ControlLink::from_registration(&empty_socket).is_err(),
            "an empty socket path"
        );
    }

    #[test]
    fn a_complete_codex_registration_yields_the_link() {
        let info = RegisterSession {
            codex_socket: Some("/tmp/cch.x/ccd.sock".into()),
            codex_generation: Some(7),
            codex_thread_id: Some("01a0-thread".into()),
            ..registration(AgentKind::Codex)
        };
        let link = ControlLink::from_registration(&info).unwrap().unwrap();
        assert_eq!(link.socket, PathBuf::from("/tmp/cch.x/ccd.sock"));
        assert_eq!(link.generation, 7);
        assert_eq!(link.thread_id.as_deref(), Some("01a0-thread"));

        // A generation of zero is a generation. It is `None` that is refused, and
        // reading absence off a sentinel value is how that distinction gets lost.
        let zero = RegisterSession {
            codex_generation: Some(0),
            ..info
        };
        assert_eq!(
            ControlLink::from_registration(&zero)
                .unwrap()
                .unwrap()
                .generation,
            0
        );
    }

    // ------------------------------------------------- ingress + retirement

    fn visit(thread: Option<&str>) -> Visit {
        Visit {
            generation: 3,
            upstream_epoch: 2,
            thread_id: thread.map(str::to_string),
        }
    }

    fn stamp(generation: u64, epoch: u64) -> Ingress {
        Ingress {
            generation,
            upstream_epoch: epoch,
        }
    }

    #[test]
    fn a_frame_from_the_live_visit_and_bound_thread_is_admitted() {
        let v = visit(Some("th_A"));
        assert!(v.admits(stamp(3, 2), FrameThread::Named("th_A")));
        // A connection-scoped notification names no thread and is still ours.
        assert!(v.admits(stamp(3, 2), FrameThread::Unnamed));
    }

    #[test]
    fn a_superseded_connection_is_retired() {
        let v = visit(Some("th_A"));
        assert!(
            !v.admits(stamp(3, 1), FrameThread::Named("th_A")),
            "an older upstream epoch is a connection this link has replaced"
        );
        assert!(
            !v.admits(stamp(3, 3), FrameThread::Named("th_A")),
            "an epoch this link has not reached cannot have delivered anything"
        );
    }

    #[test]
    fn a_retired_generation_is_filtered_and_a_thread_id_is_not() {
        let v = visit(Some("th_A"));
        // D4: filtering retires a *generation*. The same thread at an older visit
        // is retired...
        assert!(!v.admits(stamp(2, 2), FrameThread::Named("th_A")));
        // ...while the live generation admits that thread, which is exactly the
        // difference between retiring a visit and blacklisting a thread.
        assert!(v.admits(stamp(3, 2), FrameThread::Named("th_A")));
    }

    #[test]
    fn another_threads_frame_is_never_this_sessions_fact() {
        let v = visit(Some("th_A"));
        assert!(!v.admits(stamp(3, 2), FrameThread::Named("th_B")));
        // **Unbound admits NO named frame.** Until a `thread/started` says which
        // thread is this session's, a frame naming one is a claim this link cannot
        // check — and normalizing it would write another thread's items under this
        // session's uid. (The announcement itself is consumed before this check, so
        // binding is never blocked by it — see `Connection::observe_notification`.)
        assert!(!visit(None).admits(stamp(3, 2), FrameThread::Named("th_B")));
        // A frame naming no thread is connection-scoped and belongs to whoever is
        // on the connection.
        assert!(visit(None).admits(stamp(3, 2), FrameThread::Unnamed));
    }

    #[test]
    fn a_frame_naming_two_different_threads_names_none() {
        // Both fields present and disagreeing: the subject cannot be established,
        // so the frame is unnamed — dropped while unbound, and failing the
        // comparison while bound. Picking one would be choosing a contradiction.
        let conflicted = json!({
            "method": "item/started",
            "params": {"threadId": "th_A", "thread": {"id": "th_B"}}
        });
        assert_eq!(frame_thread_id(&conflicted), FrameThread::Conflicted);
        // Rejected whether or not this link is bound — including by a link bound to
        // one of the two threads it names.
        assert!(!visit(None).admits(stamp(3, 2), FrameThread::Conflicted));
        assert!(!visit(Some("th_A")).admits(stamp(3, 2), FrameThread::Conflicted));
    }

    #[test]
    fn the_thread_a_frame_is_about_is_read_from_both_shapes() {
        // `thread/started` carries the whole Thread (fixtures/codex/lifecycle.jsonl).
        let started = json!({
            "method": "thread/started",
            "params": {"thread": {"id": "01a0", "path": "/x", "cwd": "/work"}}
        });
        assert_eq!(frame_thread_id(&started), FrameThread::Named("01a0"));
        // Item/turn families carry a flat `threadId`.
        let item = json!({
            "method": "item/started",
            "params": {"threadId": "01a0", "turnId": "t1", "item": {"id": "i1"}}
        });
        assert_eq!(frame_thread_id(&item), FrameThread::Named("01a0"));
        // Absent, empty and unshaped all read as "names no thread" rather than as
        // an empty thread id, which would alias every unnamed frame together.
        assert_eq!(
            frame_thread_id(&json!({"method": "x"})),
            FrameThread::Unnamed
        );
        assert_eq!(
            frame_thread_id(&json!({"method": "x", "params": {"threadId": ""}})),
            FrameThread::Unnamed
        );
        assert_eq!(
            frame_thread_id(&json!({"method": "x", "params": {"thread": 7}})),
            FrameThread::Unnamed
        );
    }

    // ------------------------------ the STOP-AND-AMEND report carries no content

    /// The real populated answer the live gate captured: prompt, reply, rollout
    /// path and cwd, exactly as `thread/resume` returns them once a turn has run.
    const POPULATED: &str = include_str!("../../../fixtures/codex/resume-populated-answer.json");
    /// The thread that answer is about.
    const POPULATED_THREAD: &str = "01a03652-f207-76e2-b1f5-aece767a3081";

    fn populated_answer() -> Value {
        serde_json::from_str(POPULATED).expect("the captured answer is JSON")
    }

    /// The line that reaches stderr, built the way `settle_resume` builds it, but
    /// against an explicit salt so both entropy modes are reachable from a test.
    ///
    /// `None` **is** the production failure path: `digest_salt()` is `None` for the
    /// life of a process whose CSPRNG refused, and every caller reaches the digest
    /// through `frame_digest_salted`, so passing it `None` exercises exactly what
    /// that process would log.
    fn report_with_salt(salt: Option<&ring::hmac::Key>, frame: &Value, suppressed: u64) -> String {
        stop_and_amend_report(
            "cc-live",
            POPULATED_THREAD,
            &describe_resume_answer(frame, &frame_digest_salted(salt, frame)),
            suppressed,
        )
    }

    /// The line that reaches stderr, built the way `settle_resume` builds it.
    fn report_for(frame: &Value, suppressed: u64) -> String {
        report_with_salt(digest_salt(), frame, suppressed)
    }

    /// This process's salt, for a test that wants the with-digest mode explicitly.
    fn a_salt() -> ring::hmac::Key {
        mint_digest_salt().expect("the platform CSPRNG produces a salt")
    }

    /// The longest run of consecutive hex characters in `text` — the shape a digest
    /// renders as, whatever keyed it. Used to assert that the digest-unavailable
    /// report carries no digest *of any provenance*, not merely not the keyed one.
    fn longest_hex_run(text: &str) -> usize {
        let mut longest = 0;
        let mut run = 0;
        for character in text.chars() {
            run = if character.is_ascii_hexdigit() {
                run + 1
            } else {
                0
            };
            longest = longest.max(run);
        }
        longest
    }

    /// The report with the one thing in it that is legitimately hex — the thread id
    /// the operator needs — replaced, so an assertion about digest-shaped or
    /// number-shaped text is about the digest field and not about the thread id.
    fn without_the_thread_id(line: &str) -> String {
        line.replace(POPULATED_THREAD, "<thread>")
    }

    /// The two entropy modes, named for assertion messages. The redaction claims
    /// are asserted across both: losing the digest may cost the report a field and
    /// nothing else.
    fn both_modes(salt: &ring::hmac::Key) -> [(&'static str, Option<&ring::hmac::Key>); 2] {
        [("with a digest", Some(salt)), ("digest unavailable", None)]
    }

    #[test]
    fn the_stop_and_amend_report_describes_the_answer_instead_of_quoting_it() {
        let frame = populated_answer();
        let salt = a_salt();
        for (mode, salt) in both_modes(&salt) {
            let line = report_with_salt(salt, &frame, 0);

            // Nothing from inside the frame. These four are the session's content
            // and the reason the frame dump was removed: two of them are what the
            // user and the assistant said to each other, and two are real paths.
            for leaked in [
                "Reply with the single word ok",
                "\"ok\"",
                "rollout-2026-08-25T03-30-00",
                "/work/proj",
                "/work/.codex/sessions",
                "gpt-5.6-luna",
                // And the KEY names, which round 1 printed. A key name is peer text
                // from no fixed vocabulary; a future or hostile server can put
                // anything in one, so none of them may reach the log either.
                "itemsBackwardsCursor",
                "instructionSources",
                "activePermissionProfile",
                "runtimeWorkspaceRoots",
                "turnsBackwardsCursor",
                "initialTurnsPage",
            ] {
                assert!(
                    !line.contains(leaked),
                    "the report ({mode}) leaked {leaked:?} out of the frame:\n{line}"
                );
            }
            // Belt and braces: no substring of the report is a substring of the
            // frame long enough to be content. The prompt is the longest in there.
            assert!(
                !line.contains("nothing else."),
                "the report ({mode}) quoted the prompt:\n{line}"
            );

            // And it still says the things a human needs: which thread, what shape,
            // how many turns — the fact that says why this branch fired at all.
            assert!(
                line.contains(POPULATED_THREAD),
                "no thread id ({mode}):\n{line}"
            );
            assert!(line.contains("turns=1"), "no turn count ({mode}):\n{line}");
            // The allowlisted flags, all five, from this build's own constant — and
            // the eleven remaining keys as a number rather than as names.
            assert!(
                line.contains(
                    "a result (approvalPolicy=yes cwd=yes model=yes sandbox=yes thread=yes \
                     turns=1; 11 other top-level keys;"
                ),
                "wrong described shape ({mode}):\n{line}"
            );
            assert!(
                line.contains("STOP-AND-AMEND"),
                "no prose ({mode}):\n{line}"
            );
        }

        // With a salt the digest is present, and is 16 hex characters of an HMAC
        // tag rather than the frame.
        let line = report_with_salt(Some(&salt), &frame, 0);
        let PublicDigest::Keyed(hex) = frame_digest_salted(Some(&salt), &frame) else {
            panic!("a salted digest is keyed");
        };
        assert_eq!(hex.len(), 16, "digest {hex}");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{hex}");
        assert!(line.contains(&format!("digest {hex}")), "{line}");
    }

    #[test]
    fn a_csprng_failure_omits_the_digest_rather_than_substituting_a_searchable_one() {
        let frame = populated_answer();
        // The failure, injected where production reads it.
        assert_eq!(
            frame_digest_salted(None, &frame),
            PublicDigest::Unavailable,
            "an unsalted digest must be an absence, not a value"
        );
        let line = report_with_salt(None, &frame, 0);

        // The field is absent, and the report says so in fixed vocabulary so a
        // reader knows it is missing by design and not by accident.
        assert!(
            line.contains(&format!("digest {DIGEST_UNAVAILABLE}")),
            "the report does not mark the digest as unavailable:\n{line}"
        );

        // No digest of ANY provenance. The old fallback keyed the digest with the
        // clock and the pid and rendered the same 16 hex characters, so the log
        // looked identical while the confirmation oracle was back: those inputs are
        // low-entropy, and an attacker who knows roughly when ccd started can
        // enumerate candidate salts and recompute a digest from a guessed frame.
        let scrubbed = without_the_thread_id(&line);
        assert!(
            longest_hex_run(&scrubbed) < 16,
            "the digest-unavailable report carries digest-shaped text:\n{line}"
        );
        // And specifically none of the material that fallback was made of. A test
        // process's pid is several digits, and unix seconds are ten, so neither
        // collides with the report's own numbers (turn count, key count).
        let pid = std::process::id().to_string();
        assert!(
            !scrubbed.contains(&pid),
            "the report carries this process's pid ({pid}):\n{line}"
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after the epoch");
        for stamp in [
            now.as_secs().to_string(),
            now.as_millis().to_string(),
            format!("{:x}", now.as_secs()),
        ] {
            assert!(
                !scrubbed.contains(&stamp),
                "the report carries a clock-derived value ({stamp}):\n{line}"
            );
        }

        // Losing the digest costs the report the field and nothing else: the prose
        // and the shape a human acts on are still there.
        assert!(line.contains("STOP-AND-AMEND"), "{line}");
        assert!(line.contains(POPULATED_THREAD), "{line}");
        assert!(line.contains("turns=1"), "{line}");
    }

    #[test]
    fn an_error_answer_reports_its_code_and_never_its_message() {
        // A message is not structural: the measured not-ready one already embeds a
        // thread id, so a future one could embed anything.
        let frame = json!({
            "id": 2,
            "error": {"code": -32000, "message": "/work/proj is not a workspace"}
        });
        let line = report_for(&frame, 0);
        assert!(line.contains("an error (code -32000"), "{line}");
        assert!(
            !line.contains("/work/proj"),
            "the report leaked a path:\n{line}"
        );
        assert!(!line.contains("not a workspace"), "{line}");
    }

    #[test]
    fn an_unknown_top_level_key_is_counted_and_never_named() {
        // The key names in an answer are the peer's text. This one is the bait: it
        // may raise the "other" count and nothing else.
        let frame = json!({
            "id": 2,
            "result": {
                "thread": {"id": POPULATED_THREAD, "turns": []},
                "zzz_secret_key_name": "whatever",
                "another_unknown": 1
            }
        });
        let line = report_for(&frame, 0);
        assert!(
            !line.contains("zzz_secret_key_name"),
            "an unknown key reached the log by name:\n{line}"
        );
        assert!(!line.contains("another_unknown"), "{line}");
        assert!(
            !line.contains("whatever"),
            "a value reached the log:\n{line}"
        );
        assert!(
            line.contains("2 other top-level keys"),
            "the unknown keys were not counted:\n{line}"
        );
        // The allowlisted vocabulary is rendered whole either way, so an absent
        // key is a `no` rather than a silence that could be mistaken for one.
        assert!(
            line.contains("approvalPolicy=no cwd=no model=no sandbox=no thread=yes turns=0;"),
            "{line}"
        );
    }

    #[test]
    fn the_digest_is_keyed_by_a_per_process_salt_not_by_the_content() {
        let frame = populated_answer();
        let one = a_salt();
        let two = a_salt();

        // Stable under one salt: two reports in one log are comparable to each
        // other, which is the whole reason the field is there.
        assert_eq!(
            frame_digest_salted(Some(&one), &frame),
            frame_digest_salted(Some(&one), &frame)
        );
        // Different under another: nobody who guesses the frame can recompute the
        // digest a log line carries, because the salt is not in the log.
        assert_ne!(
            frame_digest_salted(Some(&one), &frame),
            frame_digest_salted(Some(&two), &frame),
            "the digest is content-derived — it is a confirmation oracle"
        );
        // Still a short hex fingerprint, and still telling two answers apart.
        let PublicDigest::Keyed(hex) = frame_digest_salted(Some(&one), &frame) else {
            panic!("a salted digest is keyed");
        };
        assert_eq!(hex.len(), 16, "digest {hex}");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{hex}");
        assert_ne!(
            frame_digest_salted(Some(&one), &frame),
            frame_digest_salted(Some(&one), &json!({"result": {}}))
        );
        // On a platform whose CSPRNG works — every platform ccd ships on — the
        // process-wide digest is present and stable, so the field is the normal
        // case and the absence is the exception.
        assert!(matches!(frame_digest(&frame), PublicDigest::Keyed(_)));
        assert_eq!(frame_digest(&frame), frame_digest(&frame));
    }

    #[test]
    fn the_line_that_actually_reaches_stderr_carries_no_frame() {
        // `log.rs` writes to `std::io::stderr()`, which the harness cannot capture,
        // so the redaction has never been asserted against the *emitted* text. The
        // test-only sink closes that gap: this is the real `log_error!` line.
        let frame = populated_answer();
        let salt = a_salt();
        for (mode, salt) in both_modes(&salt) {
            crate::log::capture::install();
            crate::log_error!("{}", report_with_salt(salt, &frame, 0));
            let captured = crate::log::capture::drain();
            crate::log::capture::uninstall();

            // Other tests log on other threads into the same process-global sink,
            // so the line this test means is selected by content.
            let line = captured
                .iter()
                .find(|line| line.contains("STOP-AND-AMEND"))
                .unwrap_or_else(|| panic!("the sink captured nothing ({mode}): {captured:?}"));
            assert!(line.starts_with("20") && line.contains(" ERROR "), "{line}");
            // The `Frame: {frame}` dump round 1 removed, asserted where it would
            // actually have appeared.
            assert!(!line.contains("Frame:"), "{mode}: {line}");
            assert!(
                !line.contains("Reply with the single word ok"),
                "{mode}: {line}"
            );
            assert!(!line.contains("itemsBackwardsCursor"), "{mode}: {line}");
            assert!(line.contains(POPULATED_THREAD), "{mode}: {line}");
            // The emitted message carries the pid of no process. Taken after
            // `log::emit`'s own level prefix, which is where the line's timestamp
            // lives — that stamp is the log's, not the digest's, so the clock
            // assertion belongs to the report and is made there.
            let message = line.split(" ERROR ").nth(1).expect("an ERROR line");
            assert!(
                !without_the_thread_id(message).contains(&std::process::id().to_string()),
                "{mode}: {line}"
            );
        }
    }

    #[test]
    fn the_report_is_throttled_and_says_how_many_it_swallowed() {
        let frame = populated_answer();
        let salt = a_salt();
        // Both modes. The throttle rides the in-process discriminator, not the
        // loggable digest, so losing the digest may not cost the daemon the one
        // thing that keeps a permanent condition from filling the log.
        for (mode, salt) in both_modes(&salt) {
            let mut throttle = AmendThrottle::default();
            let t0 = Instant::now();
            // Recomputed at every occurrence, exactly as `settle_resume` does — a
            // discriminator that were stable only within one call would suppress
            // nothing in the reconnect loop this throttle exists for.
            let answer = || frame_discriminator(&frame);

            // The first occurrence is reported in full, nothing suppressed yet.
            assert_eq!(
                throttle.admit(t0, POPULATED_THREAD, answer()),
                Some(0),
                "{mode}"
            );
            // Every repeat inside the window is swallowed — this is the reconnect
            // loop that wrote six copies of the frame in fifteen seconds.
            for tick in 1..=6 {
                assert_eq!(
                    throttle.admit(t0 + Duration::from_secs(tick), POPULATED_THREAD, answer()),
                    None,
                    "{mode}: repeat {tick} was not suppressed"
                );
            }
            // Once the window is out, one report — carrying the count it hid.
            let suppressed = throttle
                .admit(t0 + AMEND_REPORT_QUIET, POPULATED_THREAD, answer())
                .unwrap_or_else(|| panic!("{mode}: the quiet window is over"));
            assert_eq!(suppressed, 6, "{mode}");
            let line = report_with_salt(salt, &frame, suppressed);
            assert!(
                line.contains("plus 6 suppressed"),
                "{mode}: the count must reach the log:\n{line}"
            );
            // And the counter starts again from that report, not from the first.
            assert_eq!(
                throttle.admit(t0 + AMEND_REPORT_QUIET, POPULATED_THREAD, answer()),
                None,
                "{mode}"
            );

            // The discriminator itself is compared and never rendered: it is the
            // one fingerprint of the frame with no keying, so a log line carrying
            // it would be the oracle the digest's keying exists to remove.
            for spelling in [answer().to_string(), format!("{:x}", answer())] {
                assert!(
                    !without_the_thread_id(&line).contains(&spelling),
                    "{mode}: the throttle's discriminator reached the log:\n{line}"
                );
            }
        }
    }

    #[test]
    fn a_different_answer_or_a_different_thread_is_reported_at_once() {
        let mut throttle = AmendThrottle::default();
        let t0 = Instant::now();
        assert_eq!(throttle.admit(t0, "th_A", 0xAAAA), Some(0));
        // A new answer for the same thread is a new fact, not a repeat.
        assert_eq!(throttle.admit(t0, "th_A", 0xBBBB), Some(0));
        // As is the same answer for a different thread.
        assert_eq!(throttle.admit(t0, "th_B", 0xBBBB), Some(0));
        // The throttle holds down only what it last said.
        assert_eq!(throttle.admit(t0, "th_B", 0xBBBB), None);
    }

    #[test]
    fn the_discriminator_tells_answers_apart_without_being_the_digest() {
        let frame = populated_answer();
        // Stable, so a repeat is recognised as one.
        assert_eq!(frame_discriminator(&frame), frame_discriminator(&frame));
        // And distinguishing, so a genuinely new answer is reported at once.
        assert_ne!(
            frame_discriminator(&frame),
            frame_discriminator(&json!({"result": {}}))
        );
        // It is not the digest, in either mode: it is not derived from the salt,
        // which is exactly why the throttle survives the salt being absent.
        let salt = a_salt();
        let PublicDigest::Keyed(hex) = frame_digest_salted(Some(&salt), &frame) else {
            panic!("a salted digest is keyed");
        };
        assert_ne!(hex, format!("{:016x}", frame_discriminator(&frame)));
    }

    // -------------------------------- the link, against a scripted ccd leg

    /// A thread nothing in this session ever announces. Frames naming it are the
    /// bait: anything that binds to it, or records a fact under it, has routed a
    /// non-notification into the notification path.
    const NOISE_THREAD: &str = "th_NOISE_NOT_THIS_SESSION";
    /// The thread the lifecycle capture belongs to.
    const LIFECYCLE_THREAD: &str = "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86";
    /// The real 0.147 notification stream for one message turn.
    const LIFECYCLE: &str = include_str!("../../../fixtures/codex/lifecycle.jsonl");

    fn lifecycle_frames() -> Vec<String> {
        LIFECYCLE
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect()
    }

    /// How a scripted leg answers `thread/resume`. [`ResumeAnswer::NoRollout`] is
    /// the only answer the contract accepts; every other one must fail closed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ResumeAnswer {
        /// The measured not-ready error — what the live wire answers for a thread
        /// that has not yet run a turn, because it has no rollout (A1/D3).
        NoRollout,
        /// A success with an empty `turns[]`. **Never observed on the wire**, so it
        /// fails closed like everything else: accepting it would normalize a shape
        /// nobody has seen, which is the mistake this chunk exists to not make.
        EmptyTurns,
        /// A result describing a turn — the 2e-4b boundary.
        DescribesTurns,
        /// A policy refusal no retry can fix.
        Refused,
        /// A `result` object with no `turns[]` anywhere — neither an error nor a
        /// usable success.
        NoTurnsArray,
        /// A result describing a DIFFERENT thread than the one asked about.
        WrongThread,
        /// An identity present but unusable: two fields naming different threads.
        ConflictingIdentity,
        /// **Pre-initialize noise.** Before answering `initialize`, the leg sends an
        /// unmatched method-less response and a `"method": null` frame — both naming
        /// a thread. Neither is a notification, so neither may bind this connection
        /// or reach the adapter; and the handshake must still complete.
        NoisyHandshake,
        /// **Never answers.** The leg receives the resume and says nothing, then
        /// announces a thread — so the announcement arrives while the attach is
        /// still `Awaiting`, which is the discharge path. The connection is then
        /// **held open**, so "no second resume" means discharged rather than
        /// reconnected.
        AnnounceInstead,
        /// The same, but act one **drops** afterwards — so the reconnect's attach is
        /// observable, and the target it addresses is the proof the announcement
        /// redirected rather than merely silenced.
        AnnounceInsteadThenDrop,
    }

    /// A stand-in for the broker's ccd leg. Act one announces the thread and
    /// replays the captured stream, then drops; act two answers the reconnect's
    /// `thread/resume` per [`ResumeAnswer`] and then replays the same capture, so
    /// dedup is exercised on every run.
    struct ScriptedLeg {
        path: PathBuf,
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
        connections: Arc<std::sync::atomic::AtomicUsize>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for ScriptedLeg {
        fn drop(&mut self) {
            self.server.abort();
            let _ = std::fs::remove_file(&self.path);
        }
    }

    impl ScriptedLeg {
        fn start(answer: ResumeAnswer, announce_thread: bool) -> ScriptedLeg {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            // Short: a unix socket path is capped at SUN_LEN (103), and macOS's
            // temp dir is long enough to matter (A1/D7).
            let path = PathBuf::from(format!(
                "/tmp/ccd-link-{}-{}.sock",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path).expect("bind the scripted leg");
            let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
            let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let server = tokio::spawn({
                let seen = Arc::clone(&seen);
                let connections = Arc::clone(&connections);
                async move {
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            return;
                        };
                        let nth = connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let seen = Arc::clone(&seen);
                        tokio::spawn(async move {
                            let _ =
                                serve_scripted(stream, nth, answer, announce_thread, seen).await;
                        });
                    }
                }
            });
            ScriptedLeg {
                path,
                seen,
                connections,
                server,
            }
        }

        fn requests(&self, method: &str) -> Vec<Value> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|f| f.get("method").and_then(Value::as_str) == Some(method))
                .cloned()
                .collect()
        }
    }

    /// The captured stream this leg replays. With `announce` false the capture's
    /// own `thread/started` is withheld too — it is an announcement like any other,
    /// and leaving it in would bind the connection the caller wanted left unbound.
    fn capture(announce: bool) -> Vec<String> {
        lifecycle_frames()
            .into_iter()
            .filter(|line| {
                announce
                    || serde_json::from_str::<Value>(line)
                        .ok()
                        .and_then(|v| v.get("method").and_then(Value::as_str).map(str::to_string))
                        != Some("thread/started".to_string())
            })
            .collect()
    }

    /// The `thread/started` that binds a connection, for `thread`.
    fn announce(thread: &str) -> String {
        json!({
            "method": "thread/started",
            "params": {"thread": {"id": thread, "path": "/r/t.jsonl", "cwd": "/work",
                                  "turns": []}}
        })
        .to_string()
    }

    async fn serve_scripted(
        stream: tokio::net::UnixStream,
        nth: usize,
        answer: ResumeAnswer,
        announce_thread: bool,
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
    ) -> Result<()> {
        let mut ws = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config())).await?;
        while let Some(Ok(msg)) = ws.next().await {
            let Message::Text(text) = msg else { continue };
            let frame: Value = serde_json::from_str(&text)?;
            let method = frame
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let id = frame.get("id").and_then(Value::as_i64).unwrap_or(-1);
            seen.lock().unwrap().push(frame);

            match method.as_str() {
                "initialize" => {
                    if matches!(answer, ResumeAnswer::NoisyHandshake) {
                        // A response to somebody else's request, carrying a thread —
                        // routed as a notification it would bind this connection.
                        ws.send(Message::Text(
                            json!({"id": 9_999, "result": {"whatever": true},
                                   "params": {"thread": {"id": NOISE_THREAD}}})
                            .to_string(),
                        ))
                        .await?;
                        // `method: null` — method-BEARING to `get().is_some()`, and
                        // not a frame at all to anything that reads it properly.
                        ws.send(Message::Text(
                            json!({"method": Value::Null,
                                   "params": {"thread": {"id": NOISE_THREAD}}})
                            .to_string(),
                        ))
                        .await?;
                    }
                    ws.send(Message::Text(
                        json!({"id": id, "result": {"userAgent": "scripted",
                                                    "codexHome": "/tmp"}})
                        .to_string(),
                    ))
                    .await?;
                }
                "initialized" => {
                    // **Act one only.** A reconnect to a thread that is already
                    // running gets no `thread/started` — the announcement happened
                    // once, on a connection that is gone. Re-broadcasting it here
                    // would hand the second connection a binding the real wire does
                    // not give it, repairing the very thing under test.
                    let announces_late = matches!(
                        answer,
                        ResumeAnswer::AnnounceInstead | ResumeAnswer::AnnounceInsteadThenDrop
                    );
                    if announce_thread && nth == 0 && !announces_late {
                        ws.send(Message::Text(announce(LIFECYCLE_THREAD))).await?;
                    }
                    if !announces_late {
                        for line in capture(announce_thread && nth == 0) {
                            ws.send(Message::Text(line)).await?;
                        }
                    }
                    // Act one drops, so act two has a reconnect to attach on. Two
                    // scripts keep a single connection instead: the unbound one
                    // (nothing binds, so no target crosses a reconnect) and the
                    // late-announcing one (its whole subject is what happens to an
                    // attach that is already outstanding on THIS connection).
                    if nth == 0 && announce_thread && !announces_late {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        ws.close(None).await?;
                        return Ok(());
                    }
                }
                "thread/resume" => {
                    let reply = match answer {
                        // A1/D3, verbatim off the live wire. The noisy-handshake
                        // script answers the same way: its subject is the handshake,
                        // and the reconnect arc behind it should behave normally.
                        ResumeAnswer::NoRollout | ResumeAnswer::NoisyHandshake => {
                            json!({"id": id, "error": {
                                "code": -32600,
                                "message":
                                    format!("no rollout found for thread id {LIFECYCLE_THREAD}")
                            }})
                        }
                        ResumeAnswer::EmptyTurns => json!({"id": id, "result": {
                            "thread": {"id": LIFECYCLE_THREAD}, "turns": []
                        }}),
                        ResumeAnswer::DescribesTurns => json!({"id": id, "result": {
                            "thread": {"id": LIFECYCLE_THREAD},
                            "turns": [{"id": "turn_1", "status": "inProgress", "items": [
                                {"id": "exec-1", "type": "commandExecution",
                                 "status": "inProgress"}
                            ]}]
                        }}),
                        // The broker's own refusal, verbatim (`refusal.rs`).
                        ResumeAnswer::Refused => json!({"id": id, "error": {
                            "code": -32001,
                            "message": "resume refused: target thread is not bound to this session"
                        }}),
                        ResumeAnswer::NoTurnsArray => {
                            json!({"id": id, "result": {"thread": {"id": LIFECYCLE_THREAD}}})
                        }
                        ResumeAnswer::WrongThread => json!({"id": id, "result": {
                            "thread": {"id": "some-other-thread"}, "turns": []
                        }}),
                        ResumeAnswer::ConflictingIdentity => json!({"id": id, "result": {
                            "thread": {"id": LIFECYCLE_THREAD},
                            "threadId": "a-different-thread",
                            "turns": []
                        }}),
                        // No answer at all: the announcement is what settles the
                        // outstanding attach. The capture then goes out TWICE — both
                        // times under the freshly announced binding, so the second
                        // pass is a genuine replay over already-recorded facts and
                        // dedup is actually exercised rather than asserted over an
                        // empty set.
                        ResumeAnswer::AnnounceInstead | ResumeAnswer::AnnounceInsteadThenDrop => {
                            ws.send(Message::Text(announce(LIFECYCLE_THREAD))).await?;
                            for _ in 0..2 {
                                for line in capture(true) {
                                    ws.send(Message::Text(line)).await?;
                                }
                            }
                            // Act one only, and only for the dropping variant: this
                            // forces the reconnect that makes the REDIRECTED target
                            // observable. The holding variant keeps the connection so
                            // "no second resume" cannot be a reconnect in disguise.
                            if nth == 0 && matches!(answer, ResumeAnswer::AnnounceInsteadThenDrop) {
                                tokio::time::sleep(Duration::from_millis(150)).await;
                                ws.close(None).await?;
                                return Ok(());
                            }
                            continue;
                        }
                    };
                    ws.send(Message::Text(reply.to_string())).await?;
                    // A1: replay follows the response and has no end marker. Every
                    // frame here was already recorded on act one, so each must cost
                    // a row that is never written.
                    for line in capture(false) {
                        ws.send(Message::Text(line)).await?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A daemon on its own database, with the run already in the session table.
    ///
    /// The returned [`TempDb`] owns the file: hold it for the test's lifetime and
    /// the database (and its `-wal`/`-shm` siblings) go when it drops. Not fussiness
    /// — a suite that leaves one SQLite file per run behind is how a machine ends up
    /// with tens of gigabytes of them in its temp directory.
    fn linked_daemon(session: &SessionKey) -> (Arc<Daemon>, TempDb) {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let db = TempDb::new(&format!(
            "ccd-link-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            protocol::time::now_unix_ms()
        ));
        let store = Arc::new(crate::store::Store::open(db.path()).unwrap());
        let now = protocol::time::now_rfc3339();
        store
            .upsert_session(&crate::store::SessionRow {
                session_uid: session.uid.clone(),
                session_id: session.name.clone(),
                tmux_session: session.name.clone(),
                tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
                cwd: "/work".into(),
                claude_session_id: None,
                transcript_path: None,
                lifecycle: protocol::event::Lifecycle::Live,
                created_at: now.clone(),
                updated_at: now,
                agent: protocol::agent::AgentKind::Codex,
                codex_thread_id: None,
                codex_socket: None,
            })
            .unwrap()
            .assert_present();
        let (tail_tx, tail_rx) = tokio::sync::mpsc::unbounded_channel();
        // Kept alive: a dropped receiver would fail every tail registration, which
        // is not what these tests are about.
        Box::leak(Box::new(tail_rx));
        let daemon = Daemon::new(
            protocol::config::Config::default(),
            store,
            Arc::new(crate::apns::LoggingPushSender::new()),
            crate::state::Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
            tail_tx,
        );
        (daemon, db)
    }

    /// Drive a link against a scripted leg and report what landed.
    ///
    /// Returns `(events recorded, connections accepted, resumes seen, the frames the
    /// leg received)`.
    async fn drive(
        answer: ResumeAnswer,
        hint: Option<&str>,
        announce_thread: bool,
        settle: Duration,
    ) -> (Vec<protocol::event::Event>, usize, Vec<Value>, Vec<Value>) {
        let leg = ScriptedLeg::start(answer, announce_thread);
        let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-1");
        let (daemon, _db) = linked_daemon(&session);
        let uid = session.uid.clone();
        let task = tokio::spawn(run(
            Arc::clone(&daemon),
            session.clone(),
            ControlLink {
                socket: leg.path.clone(),
                generation: 1,
                thread_id: hint.map(str::to_string),
            },
        ));
        tokio::time::sleep(settle).await;
        let out = (
            daemon.store.events_after(&uid, 0, 1000).unwrap(),
            leg.connections.load(std::sync::atomic::Ordering::SeqCst),
            leg.requests("thread/resume"),
            leg.seen.lock().unwrap().clone(),
        );
        task.abort();
        let _ = task.await;
        out
    }

    /// The capture's five facts, one copy each, with no duplicate dedup key.
    fn assert_capture_recorded_once(events: &[protocol::event::Event]) {
        let mut keys = std::collections::HashSet::new();
        for event in events {
            let id = event
                .source_event_id
                .clone()
                .unwrap_or_else(|| panic!("every Codex fact carries a dedup key: {event:?}"));
            assert_eq!(event.source, protocol::event::Source::Codex);
            assert!(
                id.starts_with(LIFECYCLE_THREAD),
                "every fact is namespaced to the announced thread: {id}"
            );
            assert!(
                keys.insert((event.source.as_str().to_string(), id.clone())),
                "the replay duplicated {id}"
            );
        }
        assert_eq!(
            events.len(),
            5,
            "one copy of each captured fact, though every one arrived twice: {events:?}"
        );
    }

    /// **The whole arc the pre-2e-4b contract actually supports**: bind from the
    /// `thread/started` this connection watched, record the stream under it, survive
    /// an EOF, carry the target across the reconnect, and meet the measured
    /// not-ready error there — retrying, indefinitely and bounded.
    ///
    /// The second connection gets **no announcement**, because a reconnect to a
    /// running thread does not get one: the announcement happened on a connection
    /// that is gone. So it stays unbound, drops the replayed named frames, and its
    /// only business is the attach — which is exactly the pre-2e-4b contract.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_link_binds_reconnects_and_retries_the_measured_attach() {
        let (events, connections, resumes, _) =
            drive(ResumeAnswer::NoRollout, None, true, Duration::from_secs(3)).await;
        assert_eq!(
            connections, 2,
            "act one, then exactly ONE reconnect. A third connection would mean the \
             attach was REFUSED rather than retried — a STOP-AND-AMEND ends the \
             connection, so the count is how the two outcomes are told apart without \
             reading the log: {connections}"
        );
        assert!(
            resumes.len() >= 2,
            "the measured not-ready answer must be RETRIED — a second attempt on the \
             SAME connection, with the backoff doubling: {resumes:?}"
        );
        for resume in &resumes {
            assert_eq!(
                resume["params"]["threadId"].as_str(),
                Some(LIFECYCLE_THREAD),
                "the target survives the reconnect and addresses every attach: {resume}"
            );
        }
        assert_capture_recorded_once(&events);
    }

    /// **Every answer but the measured one fails closed** — including the success
    /// shapes. That is finding 12 applied: an empty `turns[]` has never been seen on
    /// this wire, and accepting it would normalize an unobserved wire change instead
    /// of reporting one. A populated `turns[]` is the same mistake with more
    /// consequences.
    ///
    /// Each is asserted per connection: the leg answers, the link reconnects, and
    /// the count grows every time — a link that quietly accepted any of them would
    /// settle on one connection and stop.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_answer_but_the_measured_one_fails_closed() {
        for answer in [
            ResumeAnswer::EmptyTurns,
            ResumeAnswer::DescribesTurns,
            ResumeAnswer::Refused,
            ResumeAnswer::NoTurnsArray,
            ResumeAnswer::WrongThread,
            ResumeAnswer::ConflictingIdentity,
        ] {
            let (_, connections, resumes, _) =
                drive(answer, None, true, Duration::from_secs(3)).await;
            assert!(
                connections >= 3,
                "{answer:?} must end its connection and reconnect, every time: \
                 {connections} connections"
            );
            assert!(
                resumes.len() >= 2,
                "{answer:?}: each reconnect re-attaches, so a refusal is answered \
                 once per connection: {resumes:?}"
            );
        }
    }

    /// **A registration hint targets a resume and binds nothing.**
    ///
    /// The link is handed a thread id it never watched start. Two things must
    /// follow, and the second is what the closed-unbound rule buys: the hint
    /// addresses the attach, and the named frames arriving while the link is still
    /// unbound are **dropped** rather than recorded. A hint is a claim from the
    /// registration; only the wire's own announcement is evidence.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_registration_hint_targets_a_resume_and_binds_nothing() {
        let (events, _, resumes, _) = drive(
            ResumeAnswer::NoRollout,
            Some("th_HINT_FROM_REGISTRATION"),
            false,
            Duration::from_secs(3),
        )
        .await;
        assert!(!resumes.is_empty(), "the hint must address an attach");
        for resume in &resumes {
            assert_eq!(
                resume["params"]["threadId"].as_str(),
                Some("th_HINT_FROM_REGISTRATION"),
                "the hint is what a resume is addressed to: {resume}"
            );
        }
        assert!(
            events.is_empty(),
            "a hint binds nothing, so every named frame arriving before an \
             announcement is dropped rather than recorded: {events:?}"
        );
    }

    /// **An announcement arriving mid-attach discharges it, and the replay that
    /// follows is deduplicated.**
    ///
    /// Two things this pins that nothing else did. The leg takes the reconnect's
    /// `thread/resume` and never answers it; instead it announces the thread. The
    /// announcement is better evidence than any resume could be — this connection
    /// now holds the stream from the thread's first frame — so the outstanding
    /// attach is **discharged** rather than left to time out, and no further resume
    /// is sent. And because the connection is now bound, the capture it replays is
    /// admitted and normalized for the second time, which is the only place dedup on
    /// a real replay is actually exercised: every fact must still exist exactly once.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_announcement_mid_attach_discharges_it_and_the_replay_deduplicates() {
        // A target is carried in so an attach actually fires: the subject here is
        // what happens to a resume that is ALREADY outstanding when the thread is
        // announced, which needs one to be outstanding.
        let (events, connections, resumes, _) = drive(
            ResumeAnswer::AnnounceInstead,
            Some(LIFECYCLE_THREAD),
            true,
            Duration::from_secs(4),
        )
        .await;
        // **Discharge, not timeout.** The scripted budget is 1.5s and the window is
        // 4s, so an attach that was *not* discharged would have timed out, torn the
        // connection down, and reconnected into a second resume. Exactly one resume
        // and exactly one connection is what tells the two apart.
        assert_eq!(
            resumes.len(),
            1,
            "the announcement discharges the attach, so no second resume is ever \
             sent: {resumes:?}"
        );
        assert_eq!(
            connections, 1,
            "and the connection is never torn down: a timed-out attach would have \
             ended it and reconnected"
        );
        // The capture was replayed twice over the SAME bound connection, so every
        // frame was normalized twice and every fact still exists once. Dedup on a
        // genuine replay, not on an empty set.
        assert_capture_recorded_once(&events);
    }

    /// **A stale target is redirected by an announcement.** The link is handed a
    /// hint it never watched start, attaches to it, and is then told by the wire
    /// which thread is actually this session's. The announced thread wins: the
    /// stale target is dropped, and the facts land under the announced thread.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_announcement_redirects_a_stale_resume_target() {
        let (events, _, resumes, _) = drive(
            ResumeAnswer::AnnounceInsteadThenDrop,
            Some("th_STALE_TARGET"),
            true,
            Duration::from_secs(4),
        )
        .await;
        assert_eq!(
            resumes[0]["params"]["threadId"].as_str(),
            Some("th_STALE_TARGET"),
            "the hint addresses the first attach: {resumes:?}"
        );
        // **The redirect is proven by the NEXT connection's target**, not merely by
        // the attaching stopping. Act one drops after announcing, so act two attaches
        // again — and it must address the announced thread, never the stale hint.
        assert!(
            resumes.len() >= 2,
            "the reconnect must attach again so the redirected target is observable: \
             {resumes:?}"
        );
        for resume in &resumes[1..] {
            assert_eq!(
                resume["params"]["threadId"].as_str(),
                Some(LIFECYCLE_THREAD),
                "every attach after the announcement targets the ANNOUNCED thread, \
                 never the stale one it replaced: {resume}"
            );
        }
        assert!(!events.is_empty());
        for event in &events {
            let id = event.source_event_id.clone().unwrap_or_default();
            assert!(
                id.starts_with(LIFECYCLE_THREAD) && !id.starts_with("th_STALE"),
                "facts land under the ANNOUNCED thread, never the stale target: {id}"
            );
        }
    }

    /// **P4: `method` must be a STRING.**
    ///
    /// `get("method").is_some()` calls `"method": null` method-bearing, which would
    /// route it into the notification path. Today that is survivable by accident —
    /// `bind_if_unbound` and the adapter both read the method with
    /// `.and_then(Value::as_str)` and no-op on a non-string — but "harmless because
    /// two other places also check" is not a contract, it is a coincidence waiting
    /// on whichever of them stops checking first. The classifier decides once, and
    /// this pins that decision.
    #[test]
    fn a_method_that_is_not_a_string_is_malformed_not_a_notification() {
        assert_eq!(
            frame_kind(&json!({"method": "thread/started", "params": {}})),
            FrameKind::Notification
        );
        assert_eq!(
            frame_kind(&json!({"id": 1, "result": {}})),
            FrameKind::Response
        );
        // The three shapes `is_some()` would have called notifications.
        assert_eq!(
            frame_kind(&json!({"method": Value::Null, "params": {}})),
            FrameKind::Malformed,
            "`method: null` is present but is not a method"
        );
        assert_eq!(frame_kind(&json!({"method": 7})), FrameKind::Malformed);
        assert_eq!(
            frame_kind(&json!({"method": {"nested": true}})),
            FrameKind::Malformed
        );
    }

    /// **M3: the handshake survives frames that are not notifications.**
    ///
    /// Before the `initialize` answer the leg sends an unmatched method-less
    /// response and a `"method": null` frame, both naming a thread this session
    /// never announces. The handshake must complete, the link must bind to the REAL
    /// thread, and nothing may be recorded under the noise thread.
    ///
    /// **Stated honestly: this pins the outcome, not the routing.** Both frames are
    /// discarded either way today — `bind_if_unbound` requires a *string* method
    /// equal to `thread/started`, and the adapter reads its method the same way — so
    /// reintroducing the old `is_some()` routing does not change what this observes,
    /// and I verified that by mutation rather than assuming otherwise. What the
    /// routing change buys is that the discard is now a *decision* made once, by
    /// [`frame_kind`] (pinned above), instead of an accident that holds only while
    /// two unrelated call sites keep checking. This test is the end-to-end
    /// companion to that: it proves the noise cannot reach the log by any path.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_noisy_handshake_binds_the_real_thread_and_records_no_noise() {
        let (events, _, _, _) = drive(
            ResumeAnswer::NoisyHandshake,
            None,
            true,
            Duration::from_secs(3),
        )
        .await;
        assert!(
            !events.is_empty(),
            "the handshake must survive the noise and the real stream must still be \
             observed"
        );
        for event in &events {
            let id = event.source_event_id.clone().unwrap_or_default();
            assert!(
                id.starts_with(LIFECYCLE_THREAD),
                "every fact belongs to the announced thread; a fact under \
                 {NOISE_THREAD} would mean a non-notification was normalized: {id}"
            );
        }
        assert_capture_recorded_once(&events);
    }

    /// This link says only the three things it claims to.    /// This link says only the three things it claims to. Not a check of the
    /// broker's allowlist — `codex-broker` owns and tests that table — but of this
    /// module's own contract, so a stray request added here has to be added to the
    /// module doc too.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_link_sends_nothing_outside_its_documented_surface() {
        let (_, _, _, seen) =
            drive(ResumeAnswer::NoRollout, None, true, Duration::from_secs(2)).await;
        assert!(!seen.is_empty());
        for frame in &seen {
            let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
            assert!(
                matches!(method, "initialize" | "initialized" | "thread/resume"),
                "the link sent {method}; its documented surface is initialize, \
                 initialized and thread/resume"
            );
        }
    }

    #[test]
    fn only_the_exact_measured_answer_is_the_not_ready_one() {
        const T: &str = "01a0333d-aa45-7f53-ac86-1082482e24c7";
        let not_ready =
            |msg: &str, code: i64| json!({"id": 1, "error": {"code": code, "message": msg}});

        // Verbatim off the live wire (codex 0.147, captured by `codex_link_live`).
        assert!(is_measured_not_ready(
            &not_ready(&format!("no rollout found for thread id {T}"), -32600),
            T
        ));

        // **Everything else is not it**, and each of these is a way a looser
        // predicate would have said yes:
        //
        // a substring match on the phrase, for another thread —
        assert!(!is_measured_not_ready(
            &not_ready("no rollout found for thread id some-other-thread", -32600),
            T
        ));
        // — the spike's older wording, which this build has NOT measured —
        assert!(!is_measured_not_ready(
            &not_ready(&format!("no rollout found for thread {T}"), -32600),
            T
        ));
        // — the right words under a different code —
        assert!(!is_measured_not_ready(
            &not_ready(&format!("no rollout found for thread id {T}"), -32603),
            T
        ));
        // — the phrase quoted inside something else —
        assert!(!is_measured_not_ready(
            &not_ready(
                &format!("internal error: no rollout found for thread id {T}"),
                -32600
            ),
            T
        ));
        // — a frame carrying BOTH a result and the error, which is not a response —
        assert!(!is_measured_not_ready(
            &json!({"id": 1, "result": {}, "error": {
                "code": -32600,
                "message": format!("no rollout found for thread id {T}")
            }}),
            T
        ));
        // — a success of any shape, including the empty-turns one this chunk
        //   deliberately does NOT accept (it has never been observed) —
        assert!(!is_measured_not_ready(
            &json!({"id": 1, "result": {"thread": {"id": T}, "turns": []}}),
            T
        ));
        // — and the shapeless ones.
        assert!(!is_measured_not_ready(&json!({"id": 1}), T));
        assert!(!is_measured_not_ready(&json!({"id": 1, "error": {}}), T));
    }
}
