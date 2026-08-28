//! The ccd **control link**: one live app-server connection per Codex session.
//!
//! `codex_adapter.rs` is the pure half of the Codex observation path — frames and
//! resume answers in, [`PendingEvent`]s out, no socket anywhere. This module is the
//! other half: it **holds the connection**. Per Codex session it dials the broker's ccd
//! leg (WS-over-UDS on the run dir's `ccd.sock`), completes the ccd role's allowlisted
//! handshake, attaches to the session's thread — by watching it start, or by resuming
//! it and reconciling the answer — stamps every inbound frame with its ingress
//! attribution, and hands the admitted ones to the adapter. What comes back lands through [`Daemon::ingest`] — the same call the
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
//! ## The attach contract, and what each half of it is grounded in
//!
//! **Turns run, and this link observes them.** `turn/start` is forwarded on the TUI leg
//! by a head-check that discharges the real TUI's sandbox deferral
//! (`codex-broker/src/refusal.rs`), and — measured in 2e-4a — the app-server delivers
//! `turn/*` and `item/*` frames **only to the connection whose `thread/resume`
//! succeeded**. A merely-initialized connection is handed none of them. So for this
//! link, accepting the resume answer and observing turns are not two features: they are
//! one, and the acceptance is what buys the other.
//!
//! The contract is total over the three things that can happen:
//!
//!   * **Binding: `thread/started` on this connection, or a resume answer this link
//!     accepted.** Those are the two pieces of evidence that say which thread is this
//!     session's, and they are evidence of the same strength — one is the app-server
//!     announcing the thread to us, the other is the app-server accepting our
//!     subscription to it. A thread id from the registration, or carried over from a
//!     previous connection, is neither: it is a **resume target**, and until one of the
//!     two lands, a frame naming a thread is a claim this link cannot check and is
//!     counted, logged and dropped.
//!
//!     **Bound is not subscribed**, and the two must not be confused. Binding says
//!     *whose* frames these are; subscription is whether the app-server sends any. A
//!     link that binds from the broadcast still has to ask.
//!   * **Reconnect: send `thread/resume`, and accept exactly two answers.** The measured
//!     `no rollout found` error for the thread that was asked about (retry with backoff;
//!     A1/D3, the only answer a thread that has not yet run a turn can give), or a
//!     **populated result this build can read whole** — about that same thread, with
//!     every turn in one of the two states this wire was measured to report.
//!     Everything else is reported with a STOP-AND-AMEND log and reconnected.
//!   * **An announcement redirects the target. It does not discharge the attach.** The
//!     thread a `thread/started` names is this session's, so it becomes what every
//!     later attach addresses — better evidence than a registration hint. What it does
//!     *not* do is settle anything. Until 2e-4b it did: a connection that watched the
//!     thread start was taken to hold the whole stream already, so resuming would only
//!     ask for a replay of what it had. That was sound while `thread/resume` could
//!     never succeed, and the measurement retired it — turn frames reach only the
//!     **resume-subscribed** connection, so an announced, never-resumed link is handed
//!     the thread's identity and then nothing else for the life of the session. The
//!     resume was never about recovery. It is what subscribes.
//!
//! ### What acceptance does, in the order it does it
//!
//! [`Connection::attach_from_seed`] carries the ordering, and it is the part that was
//! got wrong once and deleted rather than patched:
//!
//!   1. **Record before rebuild.** Every fact the answer describes goes through
//!      [`Daemon::ingest`] — the ordinary dedup path — *before* the adapter is allowed
//!      to forget what it had open. A fact observed live and the same fact recovered
//!      from an answer carry the same key and the same payload, so they collapse to one
//!      row rather than doubling.
//!   2. **A failed insert fails the attach.** The seed is not applied, so no state
//!      moved; the connection ends and the next one plans the identical seed.
//!   3. **Never fabricate.** An open item the answer does not confirm still running is
//!      dropped **without** a terminal.
//!   4. **Merge, never replace.** An item whose turn the answer reports still running
//!      keeps everything this link watched happen to it.
//!
//! The rules in 3 and 4 are not taste. A `thread/resume` answer reports the **real** ids
//! for a turn that has finished — measured byte-identical to the live wire's, and stable
//! across repeated resumes — and **placeholder** ids (`item-1`, `item-2`) for a turn
//! that is still running, measured on the same turn resumed twice. That is D15, wider
//! than the plan recorded it. [`CodexAdapter::plan_resume_seed`] is where those
//! measurements are written down and where every uncaptured shape is refused.
//!
//! One fact from A2 survives and is worth keeping in view: `thread/read` can overtake
//! `thread/resume` on the same connection, so anything depending on the attach is sound
//! only strictly after its response. This link has exactly one request outstanding at a
//! time and issues nothing between sending a resume and seeing its answer.
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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use protocol::event::SessionKey;
use protocol::ipc::RegisterSession;
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

use crate::codex_adapter::{CodexAdapter, ResumeSeed};
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

/// **How long a followed switch waits for the broker to ADOPT the new thread**
/// (round-1 P5), and at what interval.
///
/// The window being waited on is one `thread/start` round trip: the link re-targets off the
/// `thread/started` BROADCAST, and the broker makes that thread resumable when it correlates
/// and verifies the creation RESPONSE — the very next frame on the TUI's leg. Milliseconds,
/// in other words. It deliberately does NOT ride the general attach ladder, whose ceiling is
/// a minute away: that ladder exists for a thread with no rollout yet, which legitimately
/// takes ~0.9 s and can take longer, and borrowing it here would make a switch that will
/// never be adopted take a minute to say so.
///
/// Five tries at 250 ms is ~1.25 s — three orders of magnitude over the round trip it is
/// waiting for, and short enough that a switch which is never adopted is reported while the
/// operator is still looking at the screen.
const ADOPTION_RETRY_DELAY: Duration = Duration::from_millis(250);
const ADOPTION_RETRIES: u32 = 5;

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

/// What [`Connection::note_switch_candidate`] made of one frame — **two separate
/// facts, which a boolean conflated** (round-10 F2).
///
/// The caller asks two questions of a `thread/started`, and they have different
/// answers. *Must I ingest this frame myself?* is about the fact: the ordinary
/// visit filter rejects a frame naming any thread but the bound one, so an
/// announcement consumed here has to be handed to the adapter by hand or its
/// content is dropped by the very rule it exists to satisfy. *Did somebody move?*
/// is about the operator: it is true only for an announcement that actually
/// changed where this link is headed, which is what the push gate's doorbell is
/// cancelled on.
///
/// A single `bool` answered the first and was read as the second. Splitting them
/// costs one enum and removes the only way to answer "a person pressed `/new`" for
/// a frame that repeats what this link already knew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Switch {
    /// Not an announcement this method consumed — a different method, an unusable
    /// thread id, or the first bind, which binds outright and lets the ordinary
    /// filter admit the frame. The caller carries on down its normal path.
    Passed,
    /// Consumed, and its fact must be ingested, but **nothing about this link
    /// moved**: the frame re-announces the thread already sitting in the candidate
    /// slot. The slot is latest-wins and is applied by the loop rather than here,
    /// so a second announcement of a thread already held changes neither the visit
    /// nor the slot. One key was pressed, and it was already accounted for.
    Held,
    /// Consumed, and the candidate slot moved to a thread this link was neither
    /// bound to nor already holding: the operator pressed `/new`.
    Novel,
}

impl Switch {
    /// Did this method take the frame off the ordinary path, making its ingest the
    /// caller's obligation? True for everything but [`Switch::Passed`].
    fn consumed(self) -> bool {
        self != Switch::Passed
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

/// The `status` a live `turn/started` reports its turn under — `"inProgress"` on
/// all 11 captured frames (round-4 F7; see [`Connection::note_turn_start`], which
/// carries the full measurement).
///
/// Spelled here rather than reached for in [`crate::codex_adapter`], which holds
/// the same string for a `thread/resume` answer's turn. They are two independent
/// measurements of two different wire shapes that happen to agree today; sharing
/// one constant would let a future re-measurement of either silently redefine the
/// other, and the whole point of both is that they are what was *observed*.
const LIVE_TURN_IN_PROGRESS: &str = "inProgress";

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
/// These are the keys the acceptance rule reads, so their presence or absence is the
/// fact a human needs when it refused; anything else in the answer is counted, not named.
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
         {requested_thread} answered with something this build cannot read. Two \
         answers are accepted: the not-ready error a thread with no rollout gives, \
         and a populated result about that same thread whose every turn is one of \
         the two states this wire was measured to report (finished, or still \
         running). This was neither — an unmeasured turn state, a partial item \
         list, another thread, or a shape nobody has captured — and reading past it \
         would be guessing, which is the one thing this link will not do. \
         Reconnecting. The answer, described rather than quoted: {shape}.{repeats}"
    )
}

/// Where the attach stands on the connection now in hand.
#[derive(Debug)]
enum Attach {
    /// **Nothing to attach to yet**: no thread is known — no registration hint, and no
    /// `thread/started` on this connection so far. The moment either arrives this
    /// leaves `Unbound` for [`Attach::due_now`].
    ///
    /// It is deliberately *not* where a bound connection rests. An announcement used to
    /// land here, on the reasoning that a link holding the thread's stream from its
    /// first frame has no history to recover — but recovery was never what the resume
    /// bought. Subscription is, and only an accepted resume grants it.
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
        /// **This request asked about a thread that is no longer this session's.**
        ///
        /// Set when a `thread/started` announces a different thread while this resume
        /// is still outstanding. The request cannot be recalled, so its answer is
        /// received and **thrown away**: not settled, not seeded, and above all not
        /// allowed to bind. Without this, a stale resume that happens to succeed would
        /// overwrite the announced binding with the thread the link asked about before
        /// the wire corrected it — and every fact after that would be filed under a
        /// thread this session is not.
        superseded: bool,
    },
    /// A retryable attach failure is being waited out while observation continues.
    Backoff {
        until: Instant,
        next_delay: Duration,
    },
    /// **Attached.** A `thread/resume` was answered with a populated result this build
    /// read whole, its facts were recorded, and this connection is now **subscribed**
    /// to the thread — so `turn/*` and `item/*` frames flow to it. A resting state
    /// like [`Attach::Unbound`], and for the same reason: there is nothing left to ask
    /// for.
    Attached,
    /// **A recovery-only resume is outstanding** (round-3 P7): an on-demand ask about a
    /// thread this link has already switched AWAY from, purely to record the items that
    /// finished before it subscribed.
    ///
    /// Deliberately its own state rather than a reuse of `Awaiting`: its answer must NOT
    /// bind the visit, must NOT move the attach target, and must NOT adopt anything. The
    /// link is on the new thread and stays there; this only pays a debt.
    ///
    /// **It is nonetheless a resume OUTSTANDING, and A2 applies to it** (round-8 F1).
    /// Being its own variant cost it the `Awaiting` tests every other site makes, and the
    /// candidate block was one: a `/new` landing here re-targeted the attach and sent the
    /// next resume beside this unanswered one. Every site that asks "may I send now?"
    /// must name both variants. There are exactly two ways out — an answer, and an
    /// expired deadline — and both go through [`leaving_recovery`].
    Recovering {
        id: i64,
        deadline: Instant,
        thread: String,
    },
    /// The answer was neither of the two this build accepts. Never a resting state: the
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

/// **Leaving [`Attach::Recovering`] — and not parking with a follow-up already
/// owed** (round-7 F2).
///
/// Both exits from `Recovering` land on a state that carries no deadline, and the
/// only thing that re-examines the owed set sits at the BOTTOM of the loop, past the
/// read. So a debt that became `Owed` *while* the recovery was outstanding — B's turn
/// terminalizing during the on-demand ask about A — was tested once against
/// `Recovering`, declined because every resume fires from `Attached`, and then never
/// tested again: the give-up falls through to a deadline-free `ws.next()`, and the
/// answer arm `continue`s straight past the scheduler onto the same read. On a quiet
/// socket B's follow-up is never sent, and the turn it would have completed keeps its
/// placeholders for the rest of the connection's life.
///
/// The question is asked here instead, where the state actually changes, because that
/// is the one place both exits share. `due_now` rather than a send: this is the
/// scheduler's own answer, and the `Backoff` block issues it on the next pass with
/// the fallback handling and the debt bookkeeping the send site already owns.
///
/// # A HELD SWITCH CANDIDATE IS APPLIED HERE TOO (round-8 F1)
///
/// The same argument, about the other thing the loop only reconsiders past the read.
/// `Recovering` used to count as settled at the candidate block, so a `/new` arriving
/// during the on-demand ask about A applied the candidate on the spot and pipelined
/// `resume(C)` against A's outstanding request — losing A's only copy. Excluding
/// `Recovering` there fixes the pipelining and creates the mirror-image stranding:
/// the candidate would then sit un-applied while both exits land on a deadline-free
/// read, and on a quiet socket the link would stay on a thread the user has left.
///
/// So this is the ONE place `Recovering` ends, and it ends by asking both questions —
/// what is owed, and where the user went. The candidate is applied *before* the
/// scheduling decision because applying it schedules its own attach, which supersedes
/// a follow-up on a thread the link is leaving anyway.
fn leaving_recovery(
    conn: &mut Connection<'_>,
    carried: &Arc<Mutex<Carried>>,
    resume_target: &mut Option<String>,
) -> Attach {
    let mut attach = if conn.follow_up_owed() {
        Attach::due_now()
    } else {
        Attach::Attached
    };
    apply_held_candidate(conn, carried, resume_target, &mut attach);
    attach
}

/// **Discharge the carried recovery debt — the one place it is cleared** (round-8 F2).
///
/// Named rather than owed: the slot holds at most one thread, and a switch applied
/// while a recovery was outstanding writes the NEXT thread's debt into it. Clearing it
/// blind at the end of an ask about A would then throw away the obligation just
/// recorded on B, which nothing else names either.
///
/// **The slot's single occupancy is unchanged, and is stated rather than fixed.** One
/// compound case ends with an unsettled debt overwritten: A's recovery answer fails to
/// write, and the same exit applies a candidate retiring a thread that also owes, whose
/// debt takes the slot. That is strictly better than what it replaces — the previous
/// rule emptied the slot at the send, losing A's debt on every path rather than on this
/// one — and it is the same "latest wins" the candidate slot already documents: the link
/// carries the obligation of the thread it left most recently, and an older one costs
/// the pre-subscription items of a thread two switches back.
fn discharge_recovery(carried: &Arc<Mutex<Carried>>, thread: &str) {
    let mut c = carried.lock().expect("the carried link state");
    if c.owed_recovery.as_deref() == Some(thread) {
        c.owed_recovery = None;
    }
}

/// **Follow a switch this connection has been holding** — the one place a candidate
/// becomes the visit (round-8 F1).
///
/// Extracted from the loop's candidate block so that the block and [`leaving_recovery`]
/// cannot drift apart. They are the same act at two moments: the block applies a
/// candidate noticed while the link was resting, and `leaving_recovery` applies one
/// noticed while a recovery was outstanding — which the block must decline for, because
/// applying it there would pipeline a resume against the recovery.
///
/// Does nothing when no candidate is held, or when the candidate is already the thread
/// the visit is on; otherwise it moves the visit, persists what a reconnect needs, and
/// schedules the attach that subscribes to the new thread.
fn apply_held_candidate(
    conn: &mut Connection<'_>,
    carried: &Arc<Mutex<Carried>>,
    resume_target: &mut Option<String>,
    attach: &mut Attach,
) {
    if conn.switch_candidate.is_none() {
        return;
    }
    let Some(retired) = conn.apply_switch_candidate() else {
        return;
    };
    let target = conn.bound().unwrap_or_default().to_string();
    crate::log_info!(
        "codex link for {}: re-targeting the attach from {retired} to {target} \
         (attach was {})",
        conn.session.name,
        match &attach {
            Attach::Unbound => "Unbound",
            Attach::Awaiting { .. } => "Awaiting",
            Attach::Backoff { .. } => "Backoff",
            Attach::Attached => "Attached",
            Attach::Recovering { .. } => "Recovering",
            Attach::Refused => "Refused",
        }
    );
    // **The fallback** (round-1 P5). The announcement said which thread the TUI
    // moved to; it did NOT say the broker has adopted it. If the new thread
    // never verifies, every resume of it is policy-refused — and the previous
    // thread, which this link CAN still read, is remembered so the retry loop
    // can fall back to it rather than stranding the session on a thread that
    // was never a session thread.
    //
    // Both halves already live OUTSIDE this connection — persisted when the
    // candidate was NOTED. This only sharpens the fallback to the thread
    // actually being retired, which is the most precise answer available.
    {
        let mut c = carried.lock().expect("the carried link state");
        c.fallback = Some(retired.clone());
        c.pending_candidate = Some(target.clone());
        // **ROUND-3 P7.** If the thread being left still owes a
        // pre-subscription recovery — this link attached to it MID-TURN and its
        // follow-up never landed — that obligation must outlive the switch and
        // the connection. Nothing else will ever ask about that thread again.
        if conn.owes_recovery_on(&retired) {
            c.owed_recovery = Some(retired.clone());
        }
    }
    *resume_target = Some(target);
    *attach = Attach::due_now();
}

/// **Where a Codex session's control link stands — the addressee an inbound,
/// session-scoped request resolves to.**
///
/// A phone's request names a *session*; what has to receive it is a *connection*,
/// and the two are not the same thing. A registered session whose link is dialling,
/// backing off, or bound to a thread it has not been allowed to subscribe to has no
/// addressee at all, and a request handed to it would be accepted into silence. So
/// the resolver answers with the connection's own state rather than with the
/// session's, and every caller has to name which states it will act on.
///
/// **`Bound` is not `Subscribed`, and collapsing them is the mistake this type
/// exists to make unmakeable.** Binding says *whose* frames these are; subscription
/// is whether the app-server sends any (see the module doc, and `Attach::Attached`).
/// A link that learned its thread from the `thread/started` broadcast is bound and
/// receives nothing until its `thread/resume` is accepted — so a `Bound` link is a
/// live connection that is nonetheless not yet an addressee for anything scoped to
/// its thread.
///
/// Read by [`crate::state::Daemon::resolve_codex_inbound`]. Phase 3 (approval
/// answering) and Phase 4 (steer/interrupt) are the verbs that will act on it; what
/// consumes it today is the session list, which reports the thread the link has
/// actually **adopted** in preference to the one the registration claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexAddressee {
    /// **No control link belongs to this uid's current registration.** A Claude
    /// run, a uid that names nothing, a session whose supervisor has disconnected,
    /// and the interval in which a fresh registration has published its epoch and
    /// not yet installed its link — from the resolver's side these are one answer,
    /// because none of them has a Codex connection this registration can address.
    ///
    /// **It is not a lifecycle claim.** `mark_exited` writes the row and stops the
    /// tail; it does not retire the link handle, so a run marked `Exited` by the
    /// liveness sweep while its supervisor is still connected keeps reporting
    /// whatever its link is doing. The retirement happens on the supervisor's
    /// disconnect, and that — not the lifecycle column — is what this answer
    /// tracks. A caller that needs the run to be alive reads the row's `lifecycle`,
    /// which is the field that means it.
    ///
    /// Never published by a link: it is the daemon's conclusion about the slot,
    /// which is why [`LinkPresence`] starts at [`CodexAddressee::Offline`] instead.
    /// A link that exists but has not dialled yet is emphatically not the same fact
    /// as no link at all.
    NoLink,
    /// **A link is running, and holds no established connection right now**:
    /// dialling, awaiting the WebSocket upgrade, mid-handshake, or sitting out its
    /// reconnect backoff. The session is registered and will be observed again; it
    /// is not being observed at this instant.
    ///
    /// **It carries the last thread this link ADOPTED, because the link still knows
    /// it.** A reconnect does not move a session to another thread — the link
    /// resumes the same one — so publishing a threadless `Offline` would throw away
    /// knowledge the link is holding, and the fleet would fall back to the
    /// registration's launch-time claim for the whole of every backoff. After a
    /// `/new` switch that claim names a retired thread, so the staleness the
    /// adoption exists to fix would return on the first dropped connection and stay
    /// until the next one succeeded.
    ///
    /// `None` is a link that has adopted nothing yet: its first connection has not
    /// come up, or came up and never had a resume accepted.
    Offline { thread_id: Option<String> },
    /// **Connected and handshaken, bound to no thread on THIS connection.** No
    /// `thread/started` has arrived on it and no resume has been accepted, so there
    /// is no subject a request could be scoped to *here*.
    ///
    /// **It carries the last thread this link ADOPTED, for the same reason
    /// [`CodexAddressee::Offline`] does**, and closing the same hole from the other
    /// side. A reconnect publishes `Offline { B }` while it backs off and then, the
    /// instant it dials, was publishing a threadless `Unbound` — so the fleet
    /// regressed to the registration's launch-time claim for the whole of the
    /// handshake-and-resume round trip, which after a `/new` names a thread the
    /// session left. The link knows better the whole time: `B` is the target of the
    /// `thread/resume` it has just sent.
    ///
    /// `None` is a link that has adopted nothing yet: its first connection, or one
    /// that came up and never had a resume accepted.
    Unbound { adopted: Option<String> },
    /// **Connected and bound to `thread_id`, but not subscribed to it.** The wire
    /// (or a carried target) named the thread, and the resume that would subscribe
    /// this connection has not been accepted: no `turn/*` or `item/*` frame reaches
    /// it, and it is the state a link sits in for the whole of a thread's life
    /// before its first turn creates the rollout.
    Bound { thread_id: String },
    /// **Connected and subscribed to `thread_id`.** A `thread/resume` was accepted,
    /// its facts were recorded, and this connection receives the thread's frames.
    /// The one state that is an addressee.
    Subscribed { thread_id: String },
}

impl CodexAddressee {
    /// The thread this link is on, bound or subscribed — and `None` when it is on
    /// none.
    ///
    /// Deliberately does **not** distinguish the two: a caller asking "which thread
    /// is this session on?" is asking about the binding, which is what names the
    /// session's current subject whether or not frames are flowing. A caller that
    /// needs the stronger fact matches on [`CodexAddressee::Subscribed`] and is made
    /// to say so.
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            CodexAddressee::Bound { thread_id } | CodexAddressee::Subscribed { thread_id } => {
                Some(thread_id)
            }
            // The last adopted thread, which is where the session was and where the
            // reconnect is heading. Not a binding — nothing is connected, or nothing
            // on this connection is — but the question this answers is which thread
            // the session is *on*, and a link that is dialling, backing off or
            // waiting out its resume has no better answer and no reason to forget
            // the one it has.
            CodexAddressee::Offline { thread_id } => thread_id.as_deref(),
            CodexAddressee::Unbound { adopted } => adopted.as_deref(),
            CodexAddressee::NoLink => None,
        }
    }

    /// Whether frames for this session's thread actually reach the daemon right now.
    ///
    /// The distinction Phase 3 and Phase 4 will act on — an answer or a steer sent
    /// to a merely-`Bound` connection is accepted into silence. Nothing on the wire
    /// is a Codex actuation yet, so today it is the shape the resolver's own tests
    /// assert on.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_subscribed(&self) -> bool {
        matches!(self, CodexAddressee::Subscribed { .. })
    }
}

/// **The one cell a link publishes its connection state into, and the daemon reads.**
///
/// Owned by the daemon's `codex_links` slot and cloned into [`run`], so its lifetime
/// is the *handle's* rather than the task's. That settles one half of the epoch
/// story on its own: a link released or parked by a newer registration takes its
/// cell with it, so a stale task still winding down writes into a cell nothing can
/// look up, and can never be mistaken for the link that replaced it.
///
/// **The other half is not free, and is checked.** A registration publishes its
/// supervisor epoch before the gated transaction retires the incumbent link, so for
/// that interval the slot holds a handle — and a cell — belonging to the previous
/// registration. `Daemon::codex_addressee_locked` compares the handle's epoch with
/// the session's current owner and answers `NoLink` when they differ.
///
/// A `std::sync::Mutex` and not an async one, deliberately: every critical section
/// here is a move of a small enum, the daemon reads it while already holding
/// `Inner`, and an async lock would make a trivially-uncontended read into an await
/// point on the session-list path.
#[derive(Clone)]
pub struct LinkPresence(Arc<Mutex<CodexAddressee>>);

impl Default for LinkPresence {
    /// A link that has been created and has not dialled yet — emphatically not the
    /// same fact as no link at all, and holding no thread because it has adopted
    /// none.
    fn default() -> LinkPresence {
        LinkPresence(Arc::new(Mutex::new(CodexAddressee::Offline {
            thread_id: None,
        })))
    }
}

impl LinkPresence {
    pub fn new() -> LinkPresence {
        LinkPresence::default()
    }

    /// What the link last published. See [`LinkPresence::publish`] for *when* that
    /// was — this is a state, not a subscription, and it is never newer than the
    /// link's last idle moment.
    pub fn get(&self) -> CodexAddressee {
        self.0.lock().expect("the codex link presence").clone()
    }

    /// **Published at the link's idle moment — the only write any LINK makes.**
    ///
    /// The cell has one other writer, and it is not a link:
    /// [`LinkPresence::publish_seed`], which the daemon uses to state what the cell says
    /// before the link exists. Once the link is spawned this is the only site.
    ///
    /// The publish site is the point in the connection loop where every mutation of
    /// the previous frame *and* every request this iteration was going to issue have
    /// happened, and the task is about to block on the wire. Publishing from each
    /// transition instead would be a dozen call sites to keep in step, and would
    /// expose half-applied states — a visit moved to a switched thread a moment
    /// before the adoption that goes with it. One site, after the dust settles,
    /// says what a reader can act on.
    fn publish(&self, state: CodexAddressee) {
        *self.0.lock().expect("the codex link presence") = state;
    }

    /// **What this cell says before the link it belongs to has run at all.**
    ///
    /// The daemon's one write, made while it still holds the cell alone — after minting
    /// it and before the task that will own it is spawned, under the same `Inner` lock,
    /// so no link has yet published anything and none can until this returns. It exists
    /// because a fresh cell's `Offline { thread_id: None }` sends the fleet to the
    /// session row, and after a `/new` the row names the thread the session left —
    /// for the whole of the replacement's first dial, upgrade and handshake.
    ///
    /// The install can still be declined (`Inner::spawn_codex_link_if_owner` returns
    /// without running its spawn closure when the session has changed hands), which
    /// leaves a cell that was seeded and never installed. Harmless: nothing holds it,
    /// and it is dropped with the transaction.
    ///
    /// Separate from [`LinkPresence::publish`] rather than the same method under
    /// another name: that one is the link's idle-moment publication and is documented
    /// as having exactly one call site. This is a seed, it happens before there is a
    /// link to publish anything, and keeping the two apart is what keeps that claim
    /// true.
    pub fn publish_seed(&self, state: CodexAddressee) {
        self.publish(state);
    }

    /// **Test-only.** Put a link in a given state without standing one up.
    ///
    /// The states a resolver has to tell apart are all reachable from a real link,
    /// and `codex_link`'s own suite drives one to reach them. What this is for is
    /// the *daemon* side — that the registry, the session list and the resolver read
    /// this cell and act on what it says — which is a different claim and should not
    /// need a socket to make.
    #[cfg(test)]
    pub fn publish_for_tests(&self, state: CodexAddressee) {
        self.publish(state);
    }
}

/// **The link's own [`Carried`] state, in a cell the daemon holds** — the second
/// cell on the handle, and deliberately not the first.
///
/// [`LinkPresence`] is a *projection*: what the fleet may be told about this
/// connection. It is lossy on purpose — [`CodexAddressee::Bound`] names a thread
/// nobody has confirmed, so it must not be reported as the thread the session is
/// on. That is right for a reader and wrong for **retention**, which is not a
/// reader: it is the moment a link's whole memory is about to be destroyed, and it
/// has to save what the link *knows*, not what the link was willing to say.
///
/// Reading retention off the projection lost two different facts at once. During an
/// `A → /new → B` chase the projection is `Bound { B }`, which names no adopted
/// thread at all — so `A`, which the link is still holding, was dropped; and the
/// pending candidate `B` was never carryable either, though `thread/started` is
/// broadcast once and never replayed, so aborting the task was the last moment it
/// existed anywhere. The projection is also *stale*: it is written at the
/// connection's idle moment, so an adoption is real in [`Carried`] before the next
/// idle moment publishes it.
///
/// So the daemon holds this cell for the same reason it holds the presence one, and
/// `Inner::retain_codex_carry` copies out of it. Same lifetime rule:
/// the cell travels with the handle, so a stale task winding down writes into
/// something no lookup can reach — and retention takes a **snapshot**, never a
/// share, so nothing the dead task writes afterwards can be read as the survivor's.
///
/// A `std::sync::Mutex` for the same reason as the presence cell: every critical
/// section is a handful of `Option<String>` moves, and the daemon reads it while
/// already holding `Inner`.
#[derive(Clone, Default)]
pub struct LinkCarry(Arc<Mutex<Carried>>);

impl LinkCarry {
    pub fn new() -> LinkCarry {
        LinkCarry::default()
    }

    /// The cell itself, for [`run`] and the connection loop that writes it.
    fn shared(&self) -> Arc<Mutex<Carried>> {
        Arc::clone(&self.0)
    }

    /// **What this link knows, taken by value.** The one thing retention reads.
    pub fn snapshot(&self) -> Carried {
        self.0.lock().expect("the carried link state").clone()
    }

    /// **Start this link from what its predecessor knew.**
    ///
    /// The whole of `retained` and not a chosen field, because a replacement link is
    /// the same link continuing: the slots mean what they meant, the priority
    /// [`Carried::first_target`] applies is the priority that applied, and the
    /// fallback that keeps a never-adopted candidate from stranding the link is the
    /// fallback that was already keeping it from stranding. Picking fields here
    /// would be re-deriving those rules a second time, in a second place.
    ///
    /// The retained hint comes along and is then **overwritten by [`run`]**, which
    /// writes this link's own `ControlLink::thread_id` into it as its first act. That
    /// is the single writer for the hint, and it has to be the link rather than this:
    /// the hint is what *this* registration was told, so seeding it here would be a
    /// second source for a fact the frame already carries.
    pub fn resume_from(&self, retained: &Carried) {
        *self.0.lock().expect("the carried link state") = retained.clone();
    }

    /// The thread a link started from this cell would resume first — asked before
    /// [`run`] writes the hint, so it answers only out of what was retained.
    pub fn first_target(&self) -> Option<String> {
        self.0
            .lock()
            .expect("the carried link state")
            .first_target()
    }

    /// **Test-only.** Put a link's carry in a given state without standing one up —
    /// the [`LinkPresence::publish_for_tests`] argument, for the same reason.
    ///
    /// `pending` sets the fallback too, because that is what starting a chase does
    /// (`note_thread_started`): the last adopted thread is kept so a candidate that
    /// never becomes a session thread cannot strand the link. A fixture that set the
    /// candidate alone would describe a state no connection reaches.
    #[cfg(test)]
    pub fn set_for_tests(&self, adopted: Option<&str>, pending: Option<&str>) {
        let mut carried = self.0.lock().expect("the carried link state");
        carried.adopted = adopted.map(str::to_string);
        carried.pending_candidate = pending.map(str::to_string);
        carried.fallback = pending.and(carried.adopted.clone());
    }

    /// **Test-only.** The three thread slots, so a test can assert what a link
    /// started from a retained carry actually holds — `adopted`, the pending
    /// candidate, and the fallback that keeps the chase from stranding it.
    #[cfg(test)]
    pub fn slots_for_tests(&self) -> (Option<String>, Option<String>, Option<String>) {
        let carried = self.0.lock().expect("the carried link state");
        (
            carried.adopted.clone(),
            carried.pending_candidate.clone(),
            carried.fallback.clone(),
        )
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
pub async fn run(
    daemon: Arc<Daemon>,
    session: SessionKey,
    link: ControlLink,
    presence: LinkPresence,
    carry: LinkCarry,
) {
    let mut adapter = CodexAdapter::new(session.clone());
    // **The chase survives a reconnect** (round-2 P6b) — see [`Carried`] — and, in
    // this cell, a *registration* too: the daemon owns it, seeded it from whatever
    // the previous link knew, and reads it back when this one is retired. See
    // [`LinkCarry`].
    let carried = carry.shared();
    // **The registration's claim goes in here, and only here.** Whatever the daemon
    // seeded the cell with is what a *previous* link learned; the hint is what this
    // registration was told, and [`Carried::first_target`] already ranks it last.
    carried.lock().expect("the carried link state").hint = link.thread_id.clone();
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
            &carried,
            &mut adapter,
            &mut amend,
            &presence,
        )
        .await;
        // **The connection is over; say so before anything waits.** A resolver that
        // went on reporting the last connection's `Subscribed` across a reconnect
        // would name an addressee that cannot receive anything — the precise error
        // this type exists to prevent — and the backoff below is exactly when a
        // reader is most likely to ask.
        //
        // The *thread* survives, because the link does. `Carried::adopted` is the
        // one slot that is provably readable and it outlives the connection by
        // design; the next connection resumes it. Dropping it here would send the
        // fleet back to the registration's launch-time claim for the length of
        // every backoff — and after a `/new` switch that claim names a thread the
        // session left.
        presence.publish(CodexAddressee::Offline {
            thread_id: carried
                .lock()
                .expect("the carried link state")
                .adopted
                .clone(),
        });
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

/// **The link state that survives a reconnect** (round-2 P6b, round-3 P7/P9).
///
/// A connection is disposable; these are not. The app-server broadcasts `thread/started`
/// exactly once per thread, to the connections it has at that instant, so a switch this
/// link learned about and has not finished chasing could never be rediscovered if it died
/// with the connection that heard it.
///
/// **The broker now replays the HEAD to a fresh `ccd` connection, and that narrows this
/// without replacing it** (`codex_broker::relay::deliver_head`). The replay names
/// the one thread the broker has verified as the session's binding — where the user is —
/// so a link whose connection died mid-chase would learn the head again on its next
/// connection. What the replay does not carry is everything else in here: the thread this
/// link *adopted* (which may be the one it is owed a recovery on rather than the head), the
/// fallback, and the unpaid pre-subscription debt. Those are facts about this link's own
/// history, and no announcement can restate them.
///
/// **The three thread slots are DISTINCT, and the distinction is load-bearing** (round-3
/// P9). An earlier form collapsed them into one "carried target", which then had to answer
/// two different questions at once — "what should I resume?" and "what have I adopted?" —
/// and got the second wrong: a merely-PENDING thread was handed to the next connection
/// labelled as adopted, so it was treated as retired-and-readable when it had never been
/// verified by anything.
///
/// **Held in a cell the daemon owns** ([`LinkCarry`]), so it survives the abort of
/// the task that filled it: a replacement link is seeded from the last one's copy of
/// this, and a registration boundary costs the session nothing a reconnect would not
/// have cost it.
#[derive(Clone, Debug, Default)]
pub struct Carried {
    /// **Adopted**: a thread whose `thread/resume` this link ACCEPTED. The only slot that
    /// may be published outward, and the only one a fallback may point at — it is the one
    /// state that is provably readable. Written at exactly one site.
    adopted: Option<String>,
    /// **Pending**: a thread the wire announced and this link is chasing. Not adopted, not
    /// readable, not a fallback. Resumed first on a fresh connection, because it is where
    /// the user is.
    ///
    /// Written at the two moments the wire names a thread, both of them synchronous with
    /// reading the frame that named it, because `thread/started` is broadcast once and
    /// never replayed: the first bind ([`Connection::bind_or_follow`]) and a `/new` switch
    /// ([`Connection::note_switch_candidate`]). Cleared by the single adoption site and by
    /// the fallback, which are the two ways a chase ends.
    ///
    /// **"Not adopted" describes what it is FOR, not an invariant it enforces.** On the
    /// reconnect path (P6c) the wire re-announces the thread this link is already
    /// carrying, the bind writes it here, and it briefly equals `adopted`. Nothing reads
    /// the two together: [`Carried::first_target`] returns the same thread either way,
    /// the fallback reads only `fallback`, and the adoption site clears this
    /// unconditionally. Worth naming rather than tightening — the write is what makes a
    /// wire-named thread survive a registration, and that is the property that matters.
    pending_candidate: Option<String>,
    /// **Fallback**: the last ADOPTED thread, kept while a chase is in flight so a
    /// never-adopted candidate cannot strand the link.
    fallback: Option<String>,
    /// **The registration's CLAIM, and nothing else.** Not evidence: it is what the daemon
    /// was told at registration, and the wire's own announcement outranks it. Used only
    /// until something is adopted.
    ///
    /// [`run`] is its single writer. That is what makes [`Carried::is_informative`]'s
    /// exclusion of it correct rather than lossy: a replacement link is told the claim
    /// too, so retaining it would carry nothing across. Wire evidence used to be mirrored
    /// in here as well, which quietly made the exclusion drop a fact only this link had.
    hint: Option<String>,
    /// **An unrecovered pre-subscription debt** (round-3 P7): a thread this link attached
    /// to MID-TURN and then switched away from before its follow-up resume could land. The
    /// items that finished before the subscription existed are reachable from nowhere else
    /// — no live frame carries them, and the link will never be asked about that thread
    /// again — so the obligation outlives the connection and is paid once, on demand, after
    /// the new thread is adopted.
    owed_recovery: Option<String>,
}

impl Carried {
    /// **Whether this link learned anything a replacement could not learn again.**
    ///
    /// The hint is excluded, and that is the whole of the question: it is what the
    /// registration said, so a replacement is told it too. Everything else here was
    /// read off a wire that does not repeat itself.
    ///
    /// Retention consults this rather than overwriting unconditionally: a link
    /// retired before it heard anything has nothing to say about where the session
    /// is, and forgetting on its behalf is the same regression by a slower route.
    pub fn is_informative(&self) -> bool {
        self.adopted.is_some()
            || self.pending_candidate.is_some()
            || self.fallback.is_some()
            || self.owed_recovery.is_some()
    }

    /// **The thread this link ADOPTED**, for the daemon to seed a replacement's
    /// presence with. The one slot that may be published outward, which is why this
    /// reader exists rather than the daemon reaching for `first_target`.
    pub fn adopted(&self) -> Option<String> {
        self.adopted.clone()
    }

    /// What a fresh connection should resume first: the chase if there is one, then the
    /// thread we adopted, then the registration's claim.
    fn first_target(&self) -> Option<String> {
        self.pending_candidate
            .clone()
            .or_else(|| self.adopted.clone())
            .or_else(|| self.hint.clone())
    }
}

/// One connection, end to end: dial, handshake, attach, observe until it ends.
///
/// **Eight lent things, and they do not group.** Each parameter is a distinct thing
/// [`run`] owns and this connection borrows for its lifetime — the daemon, the run's
/// identity, the link's address, this attempt's epoch, the reconnect-surviving chase,
/// the adapter's own state, the amend throttle, and the outward-facing presence.
/// Bundling some subset of them into a struct would move the list rather than shorten
/// it, and would invent a grouping that says nothing true about how they are used.
#[allow(clippy::too_many_arguments)]
async fn serve_connection(
    daemon: &Arc<Daemon>,
    session: &SessionKey,
    link: &ControlLink,
    upstream_epoch: u64,
    carried: &Arc<Mutex<Carried>>,
    adapter: &mut CodexAdapter,
    amend: &mut AmendThrottle,
    presence: &LinkPresence,
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
    // A pending candidate carried in from a previous connection outranks the published
    // target: it is the thread the user moved to, and `thread/started` will not be
    // broadcast again for it (round-2 P6b).
    // The round-3 P7 debt this connection must pay once the new thread is adopted.
    let mut owed_recovery: Option<String> = None;
    let (mut resume_target, carried_adopted, carried_candidate) = {
        let c = carried.lock().expect("the carried link state");
        (
            c.first_target(),
            c.adopted.clone(),
            c.pending_candidate.clone(),
        )
    };
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
        debts: std::collections::BTreeMap::new(),
        unadopted_retries: 0,
        fallback_due: false,
        // **Seeded from the carried candidate, and the chase depends on it.**
        // `thread/started` is broadcast once and never replayed, so a candidate whose
        // connection died can be re-learned from nowhere: this slot and
        // `resume_target` (which prefers `pending_candidate`) are the two halves of
        // carrying it. `resume_target` says which thread to ASK about; this says which
        // thread to move the VISIT to once the ask settles — and without it a refusal
        // would settle onto a connection with no candidate to apply, leaving the visit
        // on A while the link asks about B.
        //
        // The consequence for what the fleet is told is deliberate and pinned by
        // `a_carried_candidate_names_the_adopted_thread_only_until_it_is_applied`: this
        // connection publishes `Unbound { adopted: A }` until the first settle applies
        // the candidate, and `Bound { B }` after it.
        switch_candidate: carried_candidate,
        // **Adopted-only** (round-3 P9): a merely-PENDING thread is not something this link
        // has verified, so it must not be handed to the candidate rule as though it were.
        carried_target: carried_adopted,
        carried: Arc::clone(carried),
        adopted_thread: None,
    };

    // The handshake can bind (a `thread/started` may arrive before the `initialize`
    // answer), so a binding is published outwards even when the handshake then
    // fails — losing it would send the next connection back to square one. Only
    // ever **set**, never cleared: `clone_from` on a `None` would wipe a target
    // this link carried across a reconnect, which is the one thing the next
    // connection needs.
    let handshook = conn.handshake(&mut ws).await;
    if let Some(bound) = conn.bound() {
        // Only the LOCAL target. The carry was written by the binding itself — see
        // [`Connection::bind_or_follow`] — so the announcement is already in the slot
        // a reconnect reads, and was in it before this connection could be parked.
        // Mirroring it again here is what used to put wire evidence into
        // [`Carried::hint`], the one slot retention deliberately ignores.
        resume_target = Some(bound.to_string());
    }
    handshook?;

    // **Anything with a target attaches — being announced the thread is not enough.**
    //
    // Until 2e-4b this read `conn.bound().is_none() && …`: an announcement was taken to
    // discharge the attach, on the reasoning that a connection which watched the thread
    // start already holds its stream from the first frame and has nothing to recover.
    // That reasoning was sound in a world where `thread/resume` could never succeed,
    // and it is **wrong now**. The measurement is that `turn/*` and `item/*` frames are
    // delivered only to the connection whose resume succeeded — so an announced,
    // never-resumed connection is handed the thread's identity and then nothing else,
    // for ever. Recovery was never what the resume was for; **subscription** is.
    //
    // So an announcement binds and names the target, and the link still asks. Before
    // the thread's first turn the ask fails with the measured not-ready error and the
    // existing backoff loop carries it; the turn creates the rollout, the next ask is
    // answered, and acceptance subscribes the connection that has been watching all
    // along.
    let mut attach = if resume_target.is_some() {
        Attach::due_now()
    } else {
        Attach::Unbound
    };

    loop {
        // The one place a request is issued. Reached only when no request is
        // outstanding, which is what keeps the resume unpipelined (A2).
        // **PAY THE ROUND-3 P7 DEBT, ONCE.** The link has adopted the new thread and owes a
        // pre-subscription recovery on one it left. Fired from `Attached` only — any other
        // state has an attach in flight or scheduled, and this must never pipeline against
        // it (A2: a resume is never pipelined).
        if matches!(attach, Attach::Attached) {
            if let Some(thread) = owed_recovery.take() {
                let id = conn.send_resume(&mut ws, &thread).await?;
                // **THE CARRIED SLOT IS NOT CLEARED HERE** (round-8 F2), and the reason
                // is the one the local `take()` above already answers for.
                //
                // Round 7 moved the clear from where the debt was noticed to just after
                // the write, because a debt discharged against a state that could not
                // issue a request is a debt lost. That was half the rule. The other half
                // is that a request on the wire is not a recovery: the answer still has to
                // arrive, be readable, and be WRITTEN, and every one of those can fail
                // while the request cannot be un-sent. What is in this slot is the only
                // surviving name for the thread — `fallback` is cleared by the very
                // adoption that makes the debt payable — so clearing it here traded the
                // one copy of A's pre-subscription items for the fact that a question had
                // been asked.
                //
                // So the slot now survives the send and is discharged by
                // [`discharge_recovery`] at the two places the ask ENDS: an answer
                // consumed to completion, and a budget that expired without one. The
                // `take()` above still bounds this connection to one ask; a connection
                // that dies mid-answer leaves the slot set, and its successor re-arms from
                // it on adopting and asks again — free, because the log dedups every fact
                // the second answer re-describes.
                crate::log_info!(
                    "codex link for {}: asking once more about {thread}, which this link \
                     attached to mid-turn and then switched away from — its \
                     pre-subscription items are reachable from nowhere else",
                    session.name
                );
                attach = Attach::Recovering {
                    id,
                    deadline: Instant::now() + RESUME_BUDGET,
                    thread,
                };
            }
        }

        if let Attach::Backoff { until, next_delay } = &attach {
            if Instant::now() >= *until {
                let next_delay = *next_delay;
                // **THE FALLBACK FIRES HERE** (round-1 P5). A followed switch re-targets
                // off a broadcast; only the broker's verification makes the new thread
                // resumable. `settle_resume` retries a not-yet-adopted refusal with
                // backoff, and the backoff is capped — so when it reaches the ceiling
                // without ever being accepted, the link stops asking about a thread that
                // may never become a session thread and goes back to the one it could
                // demonstrably read. The switch is not "undone": the visit stays on the new
                // thread, so nothing of the new thread would be mis-filed; only the ATTACH
                // target reverts, which is what keeps the session observable instead of
                // silent.
                if conn.fallback_due {
                    conn.fallback_due = false;
                    let previous = carried
                        .lock()
                        .expect("the carried link state")
                        .fallback
                        .take();
                    if let Some(previous) = previous {
                        crate::log_info!(
                            "codex link for {}: {} was announced but never became a \
                             session thread within the attach budget; falling back to \
                             {previous}, which this link could read",
                            session.name,
                            resume_target.as_deref().unwrap_or("?")
                        );
                        // **THE VISIT GOES BACK TOO.** Moving only the target left the
                        // visit on the un-adopted thread, so the redirect pulled the target
                        // straight back and the link looped: chase, refuse, fall back,
                        // chase. Reverting both is what makes giving up mean giving up.
                        conn.revert_visit_to(&previous);
                        resume_target = Some(previous.clone());
                        // The chase is over — the candidate is not carried to the next
                        // connection, because it was tried and never adopted. The published
                        // id is untouched: it is still the last ADOPTED thread (P6a).
                        {
                            let mut c = carried.lock().expect("the carried link state");
                            c.pending_candidate = None;
                            // **The fallback IS the recovery** (closing S11). Going back to
                            // the thread we could read means resuming it — and that resume
                            // returns its whole history, which is exactly what the owed
                            // recovery would have asked for. Leaving the debt set would buy
                            // a second, redundant resume of the same thread moments later.
                            if c.owed_recovery.as_deref() == Some(previous.as_str()) {
                                c.owed_recovery = None;
                            }
                        }
                    }
                }
                match resume_target.clone() {
                    Some(target) => {
                        // The deadline starts when the attach BEGINS, not when the
                        // write returns: a `send_resume` that itself blocks is part
                        // of the time the attach has taken, and starting the clock
                        // afterwards would grant it a fresh budget on top.
                        let deadline = Instant::now() + RESUME_BUDGET;
                        let id = conn.send_resume(&mut ws, &target).await?;
                        // The request is on the wire, so whatever it owes is paid. Done
                        // here rather than where the debt was noticed: a debt cleared
                        // against a state that could not issue a request is a debt lost.
                        conn.launch_follow_up(&target);
                        attach = Attach::Awaiting {
                            id,
                            deadline,
                            next_delay,
                            target,
                            superseded: false,
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
        // **AN UNANSWERED RECOVERY GIVES UP; IT DOES NOT SPIN, AND IT DOES NOT BAIL.**
        //
        // `Recovering` contributes its deadline to the `timeout_at` below but had no arm
        // anywhere that consumed an elapsed one. The timeout therefore returned `Err`,
        // `continue`d to the top, found the same expired deadline, and returned `Err`
        // again — a hot loop for the rest of the connection's life. Only an answer could
        // break it (`Attach::Recovering` is settled on the response, below), and the case
        // this state exists for is precisely the one where the answer may not come.
        //
        // **What the spin costs was measured, and it is not what it looks like.** The
        // link goes on reading and recording perfectly well: `timeout_at` polls the inner
        // future before it checks its own deadline, so frames keep arriving and keep
        // being filed. What the link can never do again is ASK — every resume site in
        // this loop fires from `Attach::Attached`, so a connection holding an expired
        // `Recovering` never issues another request for the rest of its life. The
        // follow-up that finishes a mid-turn attach is exactly such a request, so the
        // damage is a thread whose history silently stops completing itself, on a
        // connection that looks healthy.
        //
        // `Attached` and not a `bail!`, for the reason the response arm already gives:
        // this debt is asked once, the link is on the NEW thread and stays there, and a
        // recovery that goes unanswered has cost the session the pre-subscription items
        // of a thread nobody is on — never the connection observing the thread the user
        // is actually using. The debt is already discharged: the request went out.
        if let Attach::Recovering {
            deadline, thread, ..
        } = &attach
        {
            if Instant::now() >= *deadline {
                let thread = thread.clone();
                crate::log_warn!(
                    "codex link for {}: the recovery resume for {thread} went unanswered \
                     within {RESUME_BUDGET:?}; giving it up — its pre-subscription items \
                     stay unrecorded, and this connection goes on observing the thread the \
                     session is on",
                    session.name
                );
                // **Given up is a discharge** (round-8 F2). The debt is asked once; an
                // app-server that answered nothing within the budget is not going to
                // answer a repeat of the same question, and leaving the slot set would
                // buy every future connection a resume nobody will reply to.
                discharge_recovery(carried, &thread);
                attach = leaving_recovery(&mut conn, carried, &mut resume_target);
            }
        }

        // **The idle moment — the one place the link publishes what it is.**
        //
        // Everything the previous frame changed has been applied, and every request
        // this iteration was going to issue has been sent; the next statement blocks
        // on the wire. A reader asking now gets a settled state rather than a
        // half-applied one — see [`LinkPresence::publish`].
        presence.publish(conn.addressee());

        let deadline = match &attach {
            Attach::Awaiting { deadline, .. } | Attach::Recovering { deadline, .. } => {
                Some(*deadline)
            }
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
            && matches!(&attach, Attach::Awaiting { id, .. } | Attach::Recovering { id, .. }
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

        // **A binding redirects the target, schedules an attach, and supersedes a stale
        // one.** It does not settle anything.
        //
        // The thread a `thread/started` names is this session's — better evidence than a
        // registration hint, which is only a claim. So it becomes what every later
        // attach addresses, both on this connection (`resume_target`) and on the next
        // one (`thread_id`, published outward). What it does NOT do is end the attach:
        // this connection is bound and still unsubscribed, and only an accepted resume
        // changes that.
        // **An announcement REDIRECTS the target. It does not discharge the attach.**
        //
        // The thread it names is this session's — better evidence than any registration
        // hint — so it becomes what every later attach addresses. What it emphatically
        // does not do is settle anything: this connection is bound but **unsubscribed**,
        // and only an accepted `thread/resume` changes that.
        // Owned, so the block may also mutate `conn` (an immutable borrow held across a
        // mutable one would otherwise refuse to compile).
        if let Some(bound) = conn.bound().map(str::to_string) {
            let bound = bound.as_str();
            if resume_target.as_deref() != Some(bound) {
                if let Some(stale) = resume_target.as_deref() {
                    crate::log_info!(
                        "codex link for {}: {bound} was announced on this connection; \
                         the attach will target it rather than {stale}",
                        session.name
                    );
                }
                resume_target = Some(bound.to_string());
                // **Published outward, unless that would DISCARD an adopted thread**
                // (round-2 P6a).
                //
                // Two cases, and only one of them defers:
                //
                // * **Nothing adopted yet.** The published id is a registration HINT — a
                //   claim — and the announcement is the wire's own evidence. There is no
                //   readable thread to preserve, so publishing at once is strictly better:
                //   a reconnect should ask about the thread the wire named, not the one the
                //   registration guessed.
                // * **A thread was adopted and a switch announced another.** Publishing B
                //   now would hand the next connection a target the broker may never adopt
                //   while discarding A, which this link demonstrably could read. Deferred
                //   until B's resume is ACCEPTED; the chase itself survives the reconnect
                //   through `pending_candidate`/`fallback_target` (P6b).
                //
                // **`Carried::adopted` is NOT written here** (round-2 d2, resolved as
                // polish in round 3): it has exactly ONE write site — `attach_from_seed`,
                // where a resume is accepted — so "what is published is a thread we
                // adopted" is a property of the type's single writer rather than a
                // condition this site must remember to apply.
                //
                // **Nothing is written to the CARRY here either**, and that is the
                // round-6 correction. This site used to mirror the binding into
                // [`Carried::hint`] — wire evidence in the slot reserved for the
                // registration's claim, which retention ignores and which [`run`]
                // overwrites. The binding writes its own slot now, at the instant it
                // happens: see [`Connection::bind_or_follow`].
            }
            // A binding with nothing scheduled means this connection learned its thread
            // from the broadcast and has not asked yet. Ask now.
            if matches!(attach, Attach::Unbound) {
                attach = Attach::due_now();
            }
            // **A FOLLOWED SWITCH RE-ARMS THE ATTACH, from ANY state** (2e-4c).
            //
            // `Unbound` above is not enough, and the state it misses is the only one that
            // matters in practice: a link that has been happily watching thread A is
            // `Attached`, which is a RESTING state carrying no deadline. Left alone it
            // would block on the next read for ever — bound to B, subscribed to A,
            // recording nothing.
            //
            // MEASURED, and this is why re-arming is sufficient rather than merely
            // hopeful: `thread/resume` on an already-subscribed connection ADDS a
            // subscription; it does not replace one and it does not error. The very same
            // connection that resumed A can resume B and immediately begin receiving B's
            // `turn/*` and `item/*` frames (proven live: an observer resumed A, watched
            // A's turn, followed a `/new` to B, resumed B on that same socket, and
            // received B's whole turn stream). No reconnect, no second connection.
            //
            // The residual subscription to A is deliberately left in place. Nothing can
            // produce a frame on A any more — only the TUI starts turns, the TUI is on B,
            // and the `/resume` affordance that could send it back is refused by the
            // broker — so an unsubscribe would be machinery with no observable effect.
            // Should a stale A frame arrive anyway, the thread term of [`Visit::admits`]
            // drops it, which is the D4 filter doing precisely the job it exists for.
            // **An outstanding resume for a DIFFERENT thread is superseded.** It cannot
            // be recalled, so it is marked here and its answer is discarded when it
            // arrives. Skipped during a fallback (round-2 P5): there the outstanding
            // request names the old thread ON PURPOSE. The announcement is the wire telling us which thread is this
            // session's; a request issued before that correction must not be allowed to
            // win by answering later.
            if let Attach::Awaiting {
                id,
                target,
                superseded,
                ..
            } = &mut attach
            {
                if target.as_str() != bound && !*superseded {
                    crate::log_info!(
                        "codex link for {}: thread/resume (id {id}) asked about {target}, \
                         which {bound} has just superseded; its answer will be discarded",
                        session.name
                    );
                    *superseded = true;
                }
            }
        }

        // **A RECOVERY ANSWER RECORDS AND NOTHING ELSE** (round-3 P7). It is about a thread
        // this link has already left, so it may not bind the visit, move the target or
        // adopt anything — the link is on the new thread and stays there. Whatever the
        // answer is, the attach leaves `Recovering`: this debt is paid once, and a second
        // ask would be a loop over a thread nobody is on.
        //
        // The `continue` is why this site needs [`leaving_recovery`] as much as the
        // give-up above does: it jumps over the bottom-of-loop scheduler, and lands on
        // the same deadline-free read.
        if is_our_response {
            if let Attach::Recovering { thread, .. } = &attach {
                let thread = thread.clone();
                // **THE DEBT IS DISCHARGED BY DURABILITY, NEVER BY THE ASK** (round-8 F2).
                //
                // `recover_from` says whether this answer was CONSUMED TO COMPLETION —
                // every fact it described filed, or the answer proved to be one no repeat
                // could improve on. Only that clears the slot. A write that failed
                // recovered nothing, and a slot cleared on the strength of the request
                // having been *sent* discharged the obligation against a promise: the
                // remaining facts have no other copy and no other name, so a database
                // error, or this task being parked or aborted between the send and the
                // last insert, lost them for good.
                //
                // Retry is safe because every recovered fact goes through the log's
                // `(session_uid, source, source_event_id)` dedup, so a re-ask of a
                // partially-written answer files the remainder and re-files nothing.
                let settled = conn.recover_from(&frame, &thread).await;
                if settled {
                    discharge_recovery(carried, &thread);
                }
                attach = leaving_recovery(&mut conn, carried, &mut resume_target);
                continue;
            }
        }

        // The only response this link can receive is the answer to its own resume.
        if is_our_response {
            if let Attach::Awaiting {
                next_delay,
                target,
                superseded,
                ..
            } = &attach
            {
                let (next_delay, target, superseded) = (*next_delay, target.clone(), *superseded);
                attach = if superseded {
                    // **Received, logged, thrown away.** Not settled: a superseded answer
                    // is evidence about a thread this session has moved off, and reading
                    // it — even to refuse it — would let it bind, seed or report. The
                    // next attach targets the announced thread.
                    crate::log_debug!(
                        "codex link for {}: discarded the answer to a superseded \
                         thread/resume for {target}",
                        session.name
                    );
                    Attach::due_now()
                } else {
                    let has_fallback = carried
                        .lock()
                        .expect("the carried link state")
                        .fallback
                        .is_some();
                    let settled = conn
                        .settle_resume(&frame, &target, next_delay, has_fallback)
                        .await?;
                    // An answer that did not ATTACH recovered nothing, so any debt this
                    // request was paying is owed again (round-3 P7).
                    if !matches!(settled, Attach::Attached) {
                        conn.unsettle_debts(&target);
                    }
                    settled
                };
                // **ADOPTION ENDS THE CHASE** (round-2 P5/P6), checked AFTER the settle —
                // `attach_from_seed` is what records the adoption, so reading it before
                // would only ever see the previous one.
                //
                // The connection accepted a resume for `target`, so that thread is the one
                // to publish outward and to inherit, and neither the candidate nor the
                // fallback is owed anything further. This is the ONLY place the published
                // id moves to a switched-to thread (P6a).
                if conn.adopted_thread.as_deref() == Some(target.as_str()) {
                    // `attach_from_seed` already wrote `Carried::adopted` and cleared the
                    // chase (round-3 P8/P9). What is left for the loop is the LOCAL target,
                    // and the round-3 P7 debt: if this link owes a pre-subscription
                    // recovery on a thread it switched away from, adopting the new one is
                    // the moment that debt becomes payable.
                    owed_recovery = carried
                        .lock()
                        .expect("the carried link state")
                        .owed_recovery
                        .clone();
                }
            }
        }
        // **P7 — AN UNREADABLE ANSWER ENDS THE LEG BEFORE ANY CANDIDATE IS APPLIED.**
        //
        // This bail used to sit at the very bottom, after the candidate block, so a switch
        // announcement arriving on the same pass could apply a candidate, re-arm the attach
        // and leave `Refused` behind — SUPPRESSING the loud stop that an answer this build
        // cannot read is supposed to produce. The candidate is not lost by moving the bail:
        // it is carried to the next connection by `pending_candidate`/`fallback_target`
        // (round-2 P6b), which live outside this function precisely so a reconnect resumes
        // the chase rather than forgetting it.
        if matches!(attach, Attach::Refused) {
            bail!("thread/resume answered with a shape this build cannot read; reconnecting");
        }

        // **APPLY A HELD SWITCH CANDIDATE — after settling, never before** (round-1 P5/P6).
        //
        // Position is the whole point. By here, an outstanding `thread/resume` of the
        // thread the link is leaving has had its answer settled and RECORDED on this very
        // pass, so the recovery it carried — potentially the only copy of the items that
        // finished before this link subscribed — is durable before the link moves on. An
        // announcement applied where it arrived would have marked that request superseded
        // and thrown its answer away.
        //
        // Nothing is applied while a resume is still outstanding — of EITHER kind
        // (round-8 F1). `Awaiting` alone was the test, and `Recovering` is a resume
        // outstanding too: a `/new` for C landing during the on-demand ask about A applied
        // the candidate here, replaced `Recovering` with `due_now()`, and let `resume(C)`
        // go out beside A's unanswered request. A's answer then matched no active id and
        // was dropped as unroutable — while its debt had already been discharged — so the
        // one copy of A's pre-subscription items was lost by the pipelining A2 forbids.
        // The candidate is not stranded by waiting: [`leaving_recovery`] applies it at the
        // instant the recovery ends, which is the only other way out of that state.
        if !matches!(attach, Attach::Awaiting { .. } | Attach::Recovering { .. }) {
            apply_held_candidate(&mut conn, carried, &mut resume_target, &mut attach);
        }

        // **The follow-up attach, checked LAST.**
        //
        // A turn this link joined mid-flight has just finished, so its real item ids are
        // now describable — one more resume settles what the placeholders hid. Fired only
        // from `Attached`: any other state already has an attach in flight or scheduled,
        // which will carry the same recovery.
        //
        // The position in the loop is load-bearing and was wrong once. Checked before the
        // response is settled, this sees the state as it was *entering* the iteration —
        // so a debt recorded while a resume was outstanding is tested against `Awaiting`,
        // declines to fire, and then the loop blocks on the next read with nothing
        // scheduled to wake it. `Attached` carries no deadline, so that block is
        // permanent. Settling first and asking afterwards means the answer that returns
        // this connection to `Attached` is the same pass that notices what is still owed.
        if conn.follow_up_owed() && matches!(attach, Attach::Attached) {
            attach = Attach::due_now();
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
    /// Counted and dropped: this link is bound to exactly one thread — the one it
    /// watched start, or the one it resumed — so a frame for any other is not this
    /// session's fact.
    filtered: u64,
    next_id: i64,
    /// The STOP-AND-AMEND report's throttle, lent by [`run`] so it outlives this
    /// connection — which is the only way it can throttle a reconnect loop.
    amend: &'a mut AmendThrottle,
    /// **What this link still owes each turn it attached across.**
    ///
    /// A turn that was still running when the answer described it contributed no items:
    /// its ids were placeholders. Whatever of it had already finished before this link
    /// subscribed is therefore missing, and no live frame will ever carry it — those
    /// items completed before the subscription existed. The one thing that can recover
    /// them is a later answer describing the same turn **finished**, with the real ids.
    ///
    /// **One map, not three sets, and that is the fix rather than the tidying.** These
    /// were three collections kept disjoint by discipline, and the discipline broke in
    /// the one place it was hardest to see: a turn moved awaiting → owed, the answer
    /// already in flight still reported it running and put it back into awaiting, and
    /// the launch then moved it owed → settled and left the stale awaiting entry behind.
    /// A replayed terminal for that turn found it awaiting again and bought a fourth
    /// resume nobody owed. A single map makes a turn's state one value, so "in two
    /// states at once" is not something the code can express.
    ///
    /// **Keyed by (THREAD, turn)** (round-1 P7). Turn ids are unique per thread, not per
    /// session, and 2e-4c made one connection carry more than one thread's worth of them:
    /// the link follows a `/new` switch on the socket it already has, so debts recorded
    /// under thread A now share a map with turns of thread B. A bare turn-id key would let
    /// a terminal on B settle a debt owed on A — and, worse, let a resume of B be counted
    /// as paying it, so A's unrecoverable items would be marked recovered and never asked
    /// for again.
    debts: std::collections::BTreeMap<(String, String), TurnDebt>,
    /// How many times a resume of a followed-to thread has been refused as not-yet-adopted
    /// (round-1 P5). Reset when an attach is accepted or a fresh candidate is applied.
    unadopted_retries: u32,
    /// Set when the adoption budget is spent and a fallback target exists; read by the loop
    /// at its one send site.
    fallback_due: bool,
    /// **A `thread/started` for a different thread, not yet applied** (round-1 P5/P6).
    ///
    /// Set by [`Connection::note_switch_candidate`] and consumed by
    /// [`apply_held_candidate`] once no resume of the CURRENT thread is outstanding — of
    /// either kind (round-8 F1: `Recovering` is an outstanding resume too, and applying a
    /// candidate against it pipelines the next ask over an unanswered one). Holding it as
    /// a candidate — rather than re-pointing the visit where the frame arrives — is what
    /// keeps an in-flight recovery of the old thread from being thrown away, and what
    /// keeps the link from committing to a thread the broker has not adopted.
    /// The thread the SUBSCRIPTION should chase, once any outstanding recovery for the
    /// current one is settled. Latest announcement wins (round-2 P8); the announcement's
    /// own fact is recorded when it arrives, so a replaced candidate costs nothing.
    switch_candidate: Option<String>,
    /// The target this connection inherited from a previous one **that the link had
    /// actually ADOPTED** (round-2 P6c). A carried adopted target means "not unbound,
    /// merely unannounced": an announcement naming a DIFFERENT thread gets candidate
    /// treatment rather than binding instantly, exactly as it would on a connection that
    /// had been announced.
    ///
    /// **A registration HINT is deliberately not carried here**, and the distinction is
    /// the module's existing one rather than a new rule: a hint is a CLAIM, an announcement
    /// is the WIRE's own evidence, and evidence beats a claim immediately. Treating a
    /// never-verified hint as something to fall back to would delay the bind by a pass —
    /// and that pass is exactly where the outstanding stale request gets marked superseded,
    /// so its answer would be read and this session's timeline filed under a thread the
    /// wire had already corrected.
    carried_target: Option<String>,
    /// **The reconnect-surviving state, shared with [`run`]** (round-3 P8).
    ///
    /// Held by handle rather than mirrored by the loop, because the mirror had a window:
    /// a candidate noted during the HANDSHAKE — before the main loop runs at all — was not
    /// persisted, and a connection that died there took it with it. `thread/started` is
    /// broadcast once and never replayed, so that candidate was gone for good. Writing it
    /// where it is noted removes the window rather than narrowing it.
    carried: Arc<Mutex<Carried>>,
    /// The thread whose resume this connection ACCEPTED. Only an adopted thread is
    /// published outward for the next connection to inherit (round-2 P6a).
    adopted_thread: Option<String>,
}

/// Where one turn stands in the follow-up settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnDebt {
    /// Attached across while it was running; its terminal has not been seen yet.
    AwaitingTerminal,
    /// Its terminal has been seen. A follow-up is owed and has **not** been launched.
    Owed,
    /// A follow-up has been launched for it. **Terminal state** — one per turn, ever,
    /// which is what makes the follow-up a settlement rather than a loop.
    Settled,
}

/// **The binding and the subscription, resolved into one answer.**
///
/// A free function, and that is the point: the three states it distinguishes are the
/// whole contract of [`CodexAddressee`], and taking three `Option<&str>` lets every
/// one of them be asserted exhaustively without standing up a connection. See
/// [`Connection::addressee`] for why these fields — and not [`Attach`] — are what
/// decide it.
///
/// `carried` is the thread a **previous** connection adopted. It decides nothing
/// about this connection's state — an unbound connection is unbound whatever the
/// link remembers — and is carried only so an unbound one can still say which thread
/// the session is on. Deliberately not consulted in the bound arms: a carried
/// adoption compared against this connection's binding would report `Subscribed` for
/// a resume this connection has not had accepted, which is the one thing the
/// bound/subscribed split exists to prevent.
fn addressee_of(
    bound: Option<&str>,
    adopted: Option<&str>,
    carried: Option<&str>,
) -> CodexAddressee {
    match (bound, adopted) {
        (Some(bound), Some(adopted)) if bound == adopted => CodexAddressee::Subscribed {
            thread_id: bound.to_string(),
        },
        (Some(bound), _) => CodexAddressee::Bound {
            thread_id: bound.to_string(),
        },
        (None, _) => CodexAddressee::Unbound {
            adopted: carried.map(str::to_string),
        },
    }
}

/// **Is this fact a finished turn — the one thing a Codex run rings a phone about?**
///
/// A `TurnComplete` whose status is `completed`. The narrowing away from
/// `interrupted`/`failed` is argued where the push is composed
/// ([`Daemon::push_codex_turn_complete`]): a human stopped one, and the other has no
/// honest sentence.
///
/// **Deliberately its own predicate, not shared with
/// [`Connection::note_terminal`]**, which happens to test the same two fields. That
/// one narrows to `completed` because a resume answer describing any other state is
/// refused, so asking again would buy nothing; this one narrows because of what a
/// lock screen may claim. Two reasons that agree today and have no obligation to
/// keep agreeing — folding them together would make a change to either silently
/// move the other.
fn rings_the_doorbell(pending: &protocol::event::PendingEvent) -> bool {
    pending.kind == protocol::event::EventKind::TurnComplete
        && pending.payload.get("status").and_then(Value::as_str) == Some("completed")
}

/// Is this answer the BROKER's own policy refusal, rather than the app-server's?
///
/// Matched on the broker's synthetic code (`crate::codex_broker`'s `E_POLICY_REFUSED`,
/// -32001) — a code the app-server does not emit, which is what makes the two
/// distinguishable at all. The message text is deliberately not matched: it is the
/// broker's own wording and pinning it here would couple two crates through prose.
///
/// Narrow on purpose. It gates ONE behaviour — retry instead of reconnect — and every
/// other unmeasured answer still takes the STOP-AND-AMEND branch.
fn is_broker_policy_refusal(frame: &Value) -> bool {
    frame.get("result").is_none()
        && frame
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_i64)
            == Some(BROKER_POLICY_REFUSED)
}

/// The broker's synthetic policy-refusal code. Mirrors `codex_broker::refusal::E_POLICY_REFUSED`;
/// ccd does not depend on that crate, so the constant is restated with the reason it must
/// not drift.
const BROKER_POLICY_REFUSED: i64 = -32001;

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

    /// Consume a `thread/resume` answer. **Exactly two shapes are acceptable.**
    ///
    /// 1. The measured not-ready error, for the thread we asked about — a thread that
    ///    has not yet run a turn has no rollout (A1/D3), and this is the only answer it
    ///    can give. Retried with backoff.
    /// 2. A **populated result this build can read whole**: it is about the thread we
    ///    asked about, its `turns[]` is readable, and every turn in it is one of the
    ///    two states measured on this wire. Accepted — which reconciles it and
    ///    **subscribes this connection** (see [`Connection::attach_from_seed`]).
    ///
    /// Everything else — a success of an unmeasured shape, an answer about another
    /// thread, another error — takes the STOP-AND-AMEND branch, which is
    /// **described, never quoted** ([`describe_resume_answer`]) and **throttled**
    /// ([`AmendThrottle`]): a populated answer is the session's content, and a
    /// refusal recurs every reconnect for as long as the condition lasts.
    ///
    /// The shape check lives in [`CodexAdapter::plan_resume_seed`], which is where the
    /// measurements behind it are written down. This function owns only the two things
    /// the *link* knows: which thread it asked about, and that a frame carrying both a
    /// `result` and an `error` is not a response at all.
    async fn settle_resume(
        &mut self,
        frame: &Value,
        requested_thread: &str,
        next_delay: Duration,
        has_fallback: bool,
    ) -> Result<Attach> {
        if is_measured_not_ready(frame, requested_thread) {
            crate::log_debug!(
                "codex link for {}: {requested_thread} has no rollout yet; retrying the \
                 attach in {next_delay:?}",
                self.session.name
            );
            return Ok(Attach::Backoff {
                until: Instant::now() + next_delay,
                next_delay: (next_delay * 2).min(ATTACH_BACKOFF_MAX),
            });
        }
        // **The broker has not adopted this thread YET — retryable, not fatal**
        // (round-1 P5). A followed switch re-targets off a `thread/started` BROADCAST,
        // which announces that the TUI created a thread; only the broker's own correlated
        // verification of that creation makes it a session thread, and until then a
        // `thread/resume` naming it is policy-refused on the ccd leg.
        //
        // That window is ordinary and short — it is the round trip of the very
        // `thread/start` whose broadcast we just saw — so treating the refusal as a
        // STOP-AND-AMEND would end the connection and reconnect on every switch, which is
        // both noisy and, because the new connection asks the same question, a loop. It
        // backs off exactly like the measured no-rollout answer instead. If the thread is
        // NEVER adopted, the caller's fallback (see `fallback_target` in [`run`]) is what
        // stops the retrying being forever.
        if is_broker_policy_refusal(frame) {
            if self.unadopted_retries < ADOPTION_RETRIES {
                self.unadopted_retries += 1;
                crate::log_debug!(
                    "codex link for {}: the broker has not yet adopted {requested_thread} \
                     (try {}/{ADOPTION_RETRIES}); retrying in {ADOPTION_RETRY_DELAY:?}",
                    self.session.name,
                    self.unadopted_retries
                );
                return Ok(Attach::Backoff {
                    until: Instant::now() + ADOPTION_RETRY_DELAY,
                    next_delay,
                });
            }
            // The budget is spent. If this link has a thread it could demonstrably read,
            // go back to it rather than keep asking about one that may never be adopted.
            if has_fallback {
                self.unadopted_retries = 0;
                self.fallback_due = true;
                crate::log_info!(
                    "codex link for {}: {requested_thread} was announced but never became \
                     a session thread; falling back to the thread this link could read",
                    self.session.name
                );
                return Ok(Attach::Backoff {
                    until: Instant::now() + ADOPTION_RETRY_DELAY,
                    next_delay,
                });
            }
            // No fallback: this is an ordinary policy refusal of a thread that is simply
            // not this session's, and it keeps the LOUD disposition it always had — end the
            // connection and report — reached after a bounded delay instead of instantly.
        }
        // **Response exclusivity, before anything is read out of it.** A frame carrying
        // both a `result` and an `error` is not a JSON-RPC response, and reading its
        // result would be believing one half of a contradiction — the same test
        // [`is_measured_not_ready`] makes from the other side.
        if frame.get("error").is_none() {
            if let Some(result) = frame.get("result") {
                if let Some(seed) = self.adapter.plan_resume_seed(result, requested_thread) {
                    self.attach_from_seed(seed, requested_thread).await?;
                    return Ok(Attach::Attached);
                }
            }
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
        Ok(Attach::Refused)
    }

    /// **Record what a recovery answer describes, and change nothing else** (round-3 P7).
    ///
    /// The full attach path binds the visit, seeds the adapter and marks the connection
    /// adopted. None of that may happen here: this answer is about a thread the link has
    /// already switched away from, asked purely so the items that finished before the link
    /// subscribed are not lost with it.
    ///
    /// The same acceptance rule applies — an answer this build cannot read whole is
    /// refused, not guessed at — and a refusal costs only the recovery, never the
    /// connection: the link is attached to a different thread and healthy.
    ///
    /// # What the answer decides: whether the carried debt is discharged (round-8 F2)
    ///
    /// `true` means **this ask is over**, which is two different situations the caller
    /// treats alike because nothing distinguishes them from outside:
    ///
    /// * every fact the answer described was FILED, so the thread's pre-subscription
    ///   items are durable and there is nothing left to recover; or
    /// * the answer was one no repeat could improve on — an error, or a shape this
    ///   build does not read. Asking the same app-server the same question again would
    ///   get the same reply, so the gap is legible and permanent rather than pending.
    ///
    /// `false` is the case worth carrying, and there are two of them:
    ///
    /// * the answer was READABLE and the write failed. The facts exist, this link
    ///   had them in its hands, and the log does not hold them — so the debt is
    ///   still owed and the next connection asks again. The log's dedup key makes
    ///   that retry cost only the rows that did not land;
    /// * the answer reports the thread's turn **still running** (round-9 F1). Read
    ///   what that answer contains: an `inProgress` turn contributes no item fact
    ///   whatsoever, because its `items[]` carry placeholder ids rather than the
    ///   ones the live wire emitted — the measurement written down at
    ///   [`CodexAdapter::plan_resume_seed`]. So the ask succeeded, everything it
    ///   described was filed, and *the owed facts still do not exist anywhere*: the
    ///   only thing that can ever name them is a later answer reporting that same
    ///   turn finished. The answer is not evidence that the debt is paid; it is the
    ///   app-server saying, in as many words, that it cannot be paid yet.
    ///
    ///   That case is reachable rather than theoretical: attach to A mid-turn,
    ///   switch to B carrying A's debt, and recover A while its turn is still going.
    ///   A's terminal is then filtered — the link is on B — so nothing later on this
    ///   connection settles it, and a debt discharged here loses A's
    ///   pre-subscription items for good.
    ///
    /// The debts of the thread are settled either way, on every path above. They
    /// bound how many times THIS connection asks — one ask per turn — and the
    /// obligation that outlives the connection is the carried slot, not these. A
    /// carried slot that stays set is re-armed by the next connection to adopt a
    /// thread, which is what makes "ask again later" a bounded retry rather than a
    /// loop.
    ///
    /// **What an unpayable debt costs, stated:** a thread the user abandons mid-turn
    /// never produces the answer that would settle it, so its slot stays set and each
    /// later adoption spends one `thread/resume` on it. That is one request per
    /// adoption, on a link that is otherwise idle, against the alternative of
    /// silently dropping the only claim on facts that exist nowhere else — and it is
    /// the same cost the round-8 unanswered case already accepted.
    async fn recover_from(&mut self, frame: &Value, thread: &str) -> bool {
        let Some(result) = frame.get("result").filter(|_| frame.get("error").is_none()) else {
            crate::log_info!(
                "codex link for {}: the recovery ask about {thread} was not answered with a \
                 readable result; its pre-subscription items stay a legible gap rather \
                 than a guess",
                self.session.name
            );
            self.settle_debts_on(thread);
            return true;
        };
        let Some(mut seed) = self.adapter.plan_resume_seed(result, thread) else {
            crate::log_info!(
                "codex link for {}: the recovery answer for {thread} is a shape this build \
                 does not read; recording nothing",
                self.session.name
            );
            self.settle_debts_on(thread);
            return true;
        };
        // Read before the events are taken out, because it is a property of the
        // answer rather than of what the answer happened to contain.
        let still_running = seed.running_turn_ids().len();
        let events = seed.take_events();
        let described = events.len();
        let mut wrote_them_all = true;
        for pending in events {
            // Recorded through the ordinary dedup path: every one of these keys may
            // already exist (the answer re-describes the whole thread), and a duplicate
            // costing nothing is exactly what makes an on-demand recovery safe to fire.
            //
            // **The loop does not stop at the first failure**, and that is the difference
            // between this and `attach_from_seed`, which fails the attach on one. There
            // is no state rebuilt around these facts — the link is on another thread —
            // so every fact that CAN be written is worth writing: a retry then has less
            // to do, and a permanent failure still leaves the log as complete as this
            // answer could make it.
            if self.record(pending).await.is_err() {
                wrote_them_all = false;
            }
        }
        self.settle_debts_on(thread);
        if !wrote_them_all {
            crate::log_warn!(
                "codex link for {}: the recovery answer for {thread} described {described} \
                 fact(s) and at least one could not be written; the obligation stays owed \
                 so a later connection asks again — the facts have no other copy",
                self.session.name
            );
            return false;
        }
        // **A RUNNING TURN OWES FACTS THIS ANSWER COULD NOT CARRY** (round-9 F1).
        // Checked after the writes, not instead of them: an answer may describe
        // several turns, and the finished ones' facts are worth filing whatever the
        // running one is doing.
        if still_running > 0 {
            crate::log_info!(
                "codex link for {}: the recovery answer for {thread} filed {described} \
                 fact(s) and reports {still_running} turn(s) still running — a running \
                 turn's items are described with placeholder ids, so its real ones are \
                 still owed and only an answer reporting that turn finished can name \
                 them; a later connection asks again",
                self.session.name
            );
            return false;
        }
        crate::log_info!(
            "codex link for {}: recovered {described} fact(s) from {thread} on demand; the \
             link remains on its current thread",
            self.session.name
        );
        true
    }

    /// One ask per turn, on the thread that was asked about. Called on every exit from
    /// [`Connection::recover_from`] including a failed write: what these bound is how
    /// often THIS connection asks, and re-asking a thread whose answer was in hand would
    /// be the loop the debts exist to prevent.
    fn settle_debts_on(&mut self, thread: &str) {
        for ((t, _), debt) in self.debts.iter_mut() {
            if t == thread {
                *debt = TurnDebt::Settled;
            }
        }
    }

    /// Turn an accepted answer into an attach: **subscribe, weigh the movement,
    /// record, rebuild, then remember what is owed.**
    ///
    /// The order is the whole safety property, and each step is a constraint a review
    /// round paid for:
    ///
    ///   * **The movement is weighed BEFORE anything is persisted** (round-4 F6).
    ///     Deciding "this answer says the agent went back to work" is a pure read of
    ///     the answer plus one question to the log; cancelling a doomed doorbell on
    ///     the strength of it costs nothing and cannot fail. Doing it *after* the
    ///     recovery writes — which is where it used to sit — made it hostage to them
    ///     twice over, and both hostages fired:
    ///
    ///     **A failed write skipped it entirely.** The ingest loop below returns on
    ///     its first error, so an answer that demonstrably found the run mid-turn
    ///     left the stale completion ticket *valid* and the doorbell rang about a
    ///     turn that was already over. The write failing says nothing whatever about
    ///     whether the agent is working.
    ///
    ///     **A large recovery outran the grace.** Every recovered fact is one await
    ///     on the DB executor, and the window this cancellation lives inside is the
    ///     400 ms dispatch grace — a many-event answer could spend it and land the
    ///     cancel after the push had already gone. Moving the cancel ahead of the
    ///     writes bounds it by the single `turn_terminal_filed` read instead.
    ///
    ///     What it buys, then, is that the cancellation depends only on the evidence
    ///     it is *about*: the answer, and what the log already knows of the turns it
    ///     names. Nothing downstream can withhold it or delay it.
    ///   * **Record before rebuild.** Every fact the answer describes goes through
    ///     [`Daemon::ingest`] — the same call the hook, transcript and live-frame paths
    ///     make, deduplicated by `(session_uid, source, source_event_id)` — *before*
    ///     [`CodexAdapter::apply_resume_seed`] is allowed to forget what it had open.
    ///   * **A failed insert fails the attach.** Not "logs and carries on", which is
    ///     what the live-frame path does: there, a lost fact costs one row, while here
    ///     it would be a lost fact *plus* a state rebuild performed around its absence.
    ///     So the error propagates, `serve_connection` ends the connection, and the
    ///     reconnect asks again. Nothing was applied, so the retry plans the identical
    ///     seed; the facts that did land cost rows that are never written twice, which
    ///     is precisely what makes the retry free.
    ///   * **Acceptance is what subscribes, so the admit filter opens FIRST.** Measured
    ///     in 2e-4a: `turn/*` and `item/*` frames are delivered only to the connection
    ///     whose `thread/resume` succeeded. The app-server may consider this connection
    ///     subscribed from the moment it *sends* the answer, so live frames for this
    ///     thread can be on the wire before the seed has finished being written — and
    ///     `Visit::admits` rejects every named frame while `thread_id` is `None`.
    ///     Opening the filter before the writes means such a frame is admitted rather
    ///     than dropped.
    ///
    ///     This is **not** a violation of record-before-rebuild, and the distinction is
    ///     the point: that rule protects the adapter's *open-item state*, which must
    ///     not be rebuilt around facts that were never written. The admit filter is a
    ///     different thing — it decides which frames belong to this session — and a
    ///     frame admitted early is not a risk, because a live frame and the seed's
    ///     description of the same fact carry the same key and collapse. That identity
    ///     is the whole design, and here it is what makes the ordering safe.
    async fn attach_from_seed(&mut self, mut seed: ResumeSeed, thread: &str) -> Result<()> {
        let (described, terminal, running) = (
            seed.described_turns(),
            seed.terminal_turns(),
            seed.running_turns(),
        );
        // Subscribed as of the answer: admit this thread's frames from here on. Set
        // before the writes, and deliberately not undone if they fail — the visit is
        // per-connection, and a failed attach ends the connection anyway.
        self.visit.thread_id = Some(thread.to_string());
        // **THE RUN IS MID-TURN, AND THIS IS THE ONLY WITNESS THERE IS** (round-3
        // P4). A completion admitted moments ago is waiting out its dispatch grace;
        // if the link dropped and the next turn began while it was down, no
        // `turn/started` frame exists to see — the app-server broadcasts it once and
        // never replays it — and this answer is what says the agent went back to
        // work. Ringing anyway announces a completion the reader can no longer act
        // on.
        //
        // **Gated on a turn that is NEWS, and news is a question for the LOG**
        // (round-4 F5). It used to be a question for `self.debts`, and that map
        // cannot answer it: it gains an entry in exactly two places — the seeding
        // below, and `note_terminal`, which only *transitions* one that is already
        // there — and it is built empty per `Connection`. So a turn watched
        // completing NORMALLY, live and admitted, is not in it at all, and a
        // vacancy test read that turn as brand new. An app-server snapshot taken
        // before that completion then describes the same turn `inProgress`, and the
        // gate opened for it: a stale answer cancelled a doorbell about a
        // completion that had really happened, on one connection with no reconnect
        // at all (a mid-connection `launch_follow_up`), and doubly so across one.
        //
        // The log is the right scope twice over: it is what actually watched the
        // turn finish, and it is the only memory of a turn that outlives a
        // connection — which is precisely the moment a resume answer arrives. So a
        // turn whose terminal this daemon has already filed is a stale regression
        // and says nothing; a turn the log holds no terminal for, and that this
        // connection is not already awaiting, is the run moving.
        //
        // **A log that will not answer is not a log that says "no terminal".** The
        // read is fallible, and the two failure directions are not symmetric: a
        // spurious cancel silences a doorbell about news that is still current and
        // is unrecoverable, while a missed cancel costs one push that turns out to
        // be stale. So an error is treated as "not news" and logged — the same
        // fail-toward-not-cancelling rule the shape guards in `note_turn_start`
        // follow.
        //
        // Recovery may silence and may not ring — see
        // [`Daemon::note_codex_turn_running`] for why those are the same rule.
        //
        // **The read is on the cancel's critical path, and its cost was measured
        // rather than assumed** (round-5 F4). It is a point query on the dedup
        // index over WAL — p50 24 µs, p99 36 µs, worst sample 0.39 ms against a
        // 2,000-event log, and p99 46 µs with the executor under concurrent write
        // load — against the 400 ms this is racing. The cancel is nonetheless
        // issued before the ingest loop below (round-4 F6), which is the part that
        // does scale with the answer; see [`Daemon::dispatch_push`], where the
        // window is documented as best-effort and both measurements are recorded.
        //
        // **The FIRST novel turn is the whole answer** (round-6 F8). The gate is a
        // single per-session flag — one turn moving and five turns moving are the same
        // cancel — so a loop that went on querying after the first `Ok(false)` was
        // buying nothing and spending the one budget that matters. Nothing bounds how
        // many turns an answer may report `inProgress` (`plan_resume_seed` pushes one
        // per turn and caps nothing), and each further read can block on a busy
        // connection from the store's four-reader pool, so the tail it was holding the
        // cancel behind is unbounded in exactly the direction the grace cannot afford.
        // The cancel is issued the moment the evidence is decisive, and the loop stops
        // there.
        for turn in seed.running_turn_ids() {
            if self.debts.contains_key(&(thread.to_string(), turn.clone())) {
                // Already awaiting, owed or settled here: this connection has known
                // about the turn since it seeded it, so the answer is re-describing
                // its own attach rather than reporting movement.
                continue;
            }
            // **Asked with the THREAD, because a turn id names a turn only inside
            // one.** The debt map above is keyed `(thread, turn)` for that very
            // reason, and the log has to be asked the same way: thread A's settled
            // turn and thread B's running one can wear the same id, and a
            // session-wide ask reads B's movement as A's stale snapshot and
            // declines to cancel — the unrecoverable direction.
            match self
                .daemon
                .db
                .turn_terminal_filed(
                    self.session.uid.clone(),
                    crate::codex_adapter::turn_terminal_source_event_id(thread, turn),
                )
                .await
            {
                // Decisive: the log holds no terminal for a turn this connection was
                // not already awaiting, so the run has moved. Cancel HERE — before the
                // remaining queries, and before the ingest loop below, which is the
                // part that scales with the answer.
                Ok(false) => {
                    self.daemon.note_codex_turn_running(&self.session.uid);
                    break;
                }
                Ok(true) => crate::log_info!(
                    "codex link for {}: the thread/resume answer for {thread} reports \
                     turn {turn} still running, but this daemon already filed its \
                     terminal — a snapshot taken before that completion is stale, not \
                     movement, so it silences nothing",
                    self.session.name
                ),
                Err(err) => crate::log_warn!(
                    "codex link for {}: could not ask the log whether turn {turn} had \
                     already finished ({err:#}); reading that as movement would cancel \
                     a doorbell on the strength of an answer nobody gave, so this turn \
                     counts as no news",
                    self.session.name
                ),
            }
        }
        let events = seed.take_events();
        let recorded = events.len();
        for pending in events {
            self.daemon.ingest(pending).await.with_context(|| {
                format!(
                    "recording a fact the thread/resume answer for {thread} described \
                     (the attach fails rather than rebuilding state around a fact that \
                     was never written)"
                )
            })?;
        }
        self.adapter.apply_resume_seed(&seed);
        // The attach was ACCEPTED, so whatever adoption budget an earlier switch spent is
        // returned: the next switch gets a full one (round-1 P5).
        self.unadopted_retries = 0;
        self.fallback_due = false;
        // ADOPTED (round-2 P6a). Only now may this thread be published outward as the
        // target a future connection should inherit — the single write site for
        // [`Carried::adopted`], which is what makes "published implies adopted" structural
        // rather than a condition to be maintained (round-2 d2, now polish).
        self.adopted_thread = Some(thread.to_string());
        {
            let mut carried = self.carried.lock().expect("the carried link state");
            carried.adopted = Some(thread.to_string());
            carried.pending_candidate = None;
            carried.fallback = None;
        }
        // A turn that was running when this answer described it owes items this link can
        // never be sent: they finished before it subscribed. Remember it until its
        // terminal is observed, then ask once more.
        //
        // **The vacancy still governs the DEBT, and only the debt** (round-4 F5). A
        // turn whose terminal this link has already seen — or already asked about —
        // keeps the state it is in: an answer that is merely stale about a turn
        // cannot rewind that turn's settlement, and re-seeding it `AwaitingTerminal`
        // would buy a second follow-up for a turn already settled. That is a sound
        // rule about *this connection's* obligations, which is exactly why it is the
        // wrong rule for the cancellation above — the connection's obligations and
        // what the run has actually done are different questions, and one map was
        // being asked both.
        for turn in seed.running_turn_ids() {
            self.debts
                .entry((thread.to_string(), turn.clone()))
                .or_insert(TurnDebt::AwaitingTerminal);
        }
        crate::log_info!(
            "codex link for {}: attached to thread {thread} by resume. The answer \
             described {described} turn(s) ({terminal} finished, {running} still \
             running); {recorded} described fact(s) went through the ordinary dedup \
             path. This connection is subscribed.",
            self.session.name
        );
        Ok(())
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
        //
        // A SWITCH announcement is NOT applied here, though (round-1 P5/P6). It is
        // recorded as a CANDIDATE and applied by the loop, after the loop has had a
        // chance to settle an outstanding resume of the thread being left. See
        // [`Connection::note_switch_candidate`] and the loop's candidate block.
        //
        // **Its FACT, however, is recorded immediately** (round-2 P8) — see that method.
        //
        // **And a followed switch is MOVEMENT** (round-9 F4). A `thread/started`
        // naming a thread other than the one this connection is on is the operator
        // pressing `/new` — a person acting on this session, which ends the quiet
        // state any completion doorbell still sitting out its dispatch grace is
        // describing.
        //
        // It has to be said HERE rather than inferred from the new thread's first
        // turn, and that is measured: the old subscription receives the announcement
        // and then only `thread/status/changed` for the new thread's turn — the
        // `turn/started` [`Connection::note_turn_start`] reads goes to the TUI's own
        // connection (`fixtures/codex/thread-switch.jsonl`). A link waiting for that
        // frame waits for one it is never sent, and the stale ring goes out. See
        // [`Daemon::note_codex_switch`].
        //
        // **But only a NOVEL announcement this build could READ is that person**
        // (round-10 F2). Two shapes reach the cancel that are not somebody moving,
        // and both used to fire it:
        //
        //   * [`Switch::Held`] — the candidate slot already held this thread. The
        //     slot is applied by the loop, so a repeat announcement moves neither
        //     the visit nor the slot; one key was pressed and it was already
        //     accounted for. A doorbell admitted *after* the switch describes the
        //     state the switch left behind, and re-cancelling kills it.
        //   * A body the adapter refuses. [`frame_thread_id`] answers from
        //     `params.thread.id` alone, while the identity fact needs `cwd`, `path`
        //     and `cliVersion` too — so an announcement carrying an id and nothing
        //     else is one this build declines to record. A doorbell must not die on
        //     evidence the fact store rejected.
        //
        // So the READING happens first and the cancel reads its answer — but the
        // cancel does not wait for the FILING (round-11 F1). Those are two halves of
        // an ingest and only the first one is on this question's critical path:
        // [`CodexAdapter::ingest`] is a synchronous read of the frame, so whether it
        // minted anything is known the instant it returns, while writing what it
        // minted is an await on the DB executor whose worst case is the store's
        // five-second `busy_timeout` — a second process holding the write lock —
        // against the 400 ms dispatch grace this is racing
        // ([`Daemon::dispatch_push`]). An ingest awaited to completion before the
        // cancel therefore put a five-second hostage in front of a 400 ms window,
        // and the stale ring went out while the operator was already on the new
        // thread. The same reordering, for the same reason, is why the resume
        // cancel above is issued before its own recovery writes (round-4 F6).
        //
        // The two halves are split, and the order is safe both ways round. The
        // cancel is [`crate::push_gate::PushGate::note_progress`] — in memory, no
        // database, nothing to fail — so nothing about it wants to be behind a
        // write. And a `thread/started` mints exactly one kind of fact, the session
        // identity, and never a turn terminal, so nothing inside the filing can ring
        // a doorbell that the cancellation ought to have preceded.
        //
        // Live-only by the same call-site rule as everything else in this function:
        // `observe_notification` is reached only from a frame read off the wire.
        // Same call site and the same two conditions as before; only the moment
        // within it moved earlier.
        let switch = self.note_switch_candidate(frame);
        if switch.consumed() {
            let minted = self.normalize_frame(frame);
            if switch == Switch::Novel && !minted.is_empty() {
                self.daemon.note_codex_switch(&self.session.uid);
            }
            self.record_minted(minted).await;
            return;
        }
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

    /// **A turn beginning on this thread, which is the run moving.**
    ///
    /// `turn/started` mints no fact — the neutral model has no "turn started" event,
    /// and [`CodexAdapter::ingest`] maps it to nothing — but it is still the one
    /// frame that says the agent has gone back to work. The push gate needs exactly
    /// that: a completion doorbell admitted moments ago is waiting out its dispatch
    /// grace, and a turn starting inside that window means the state it announces is
    /// over. See [`Daemon::note_codex_turn_running`].
    ///
    /// Read off the raw frame rather than off an event, because there is no event to
    /// read; the *call site* is what keeps it live-only, exactly as it does for the
    /// completion doorbell below.
    ///
    /// # The frame must NAME the thread it claims to be about
    ///
    /// The visit filter admits a frame that names no thread at all, because a
    /// connection-scoped notification belongs to whoever is on the connection. That
    /// is right for the notifications it was written for and wrong for this one: a
    /// `turn/started` is thread-scoped by nature, so one arriving without a usable
    /// `threadId` — missing, empty, or not a string — is not connection news but a
    /// frame whose subject cannot be established. Admitting it as "ours" let a
    /// schema-invalid frame cancel this run's doorbell while the run was in fact
    /// finished, which is the one thing the D4 filter exists to stop for every
    /// *named* frame.
    ///
    /// So this asks the stronger question the filter deliberately does not: the
    /// frame must name a thread, and it must be the thread this connection is bound
    /// to. Anything else is somebody else's movement, or nobody's.
    ///
    /// # And it must carry the MEASURED `turn/started` BODY (round-4 F7)
    ///
    /// Naming the bound thread was not enough, because the resolver that read the
    /// name — [`frame_thread_id`] — is deliberately *generous*: it accepts both the
    /// flat `params.threadId` of the turn/item families **and** the nested
    /// `params.thread.id` that only `thread/started` carries, and it is documented
    /// as generous because its other callers need that. Reading this frame through
    /// it meant a `turn/started` with no `params.turn` at all, or one wearing
    /// `thread/started`'s Thread object, silenced a real completion merely by
    /// spelling the bound thread's id. That is the same class of hole as the
    /// unnamed frame above — a subject this build never measured, admitted as
    /// movement — one shape further in.
    ///
    /// **What the wire actually always carries**, measured over every captured
    /// `turn/started` in the repo — 11 frames across `fixtures/codex/`'s
    /// `first-turn.jsonl`, `interrupt.jsonl` and `thread-switch.jsonl`:
    ///
    /// ```text
    /// params keys : exactly {"threadId", "turn"}          11/11 — never "thread"
    /// params.threadId : non-empty string                   11/11
    /// params.turn : object {completedAt, durationMs, error,
    ///                       id, items, itemsView, startedAt, status}
    /// params.turn.id : non-empty string                    11/11
    /// params.turn.status : "inProgress"                    11/11
    /// ```
    ///
    /// So three things are required, and no fourth: the **flat** `threadId` naming
    /// the bound thread, a `turn` object with a non-empty `id`, and that turn
    /// reported running. Each is universal on the measured wire, so requiring it
    /// cannot cost a real cancellation; anything beyond them — the exact key set,
    /// the absence of `thread`, the timestamps — is a shape this build has no
    /// reason to depend on, and a requirement the live wire does not satisfy would
    /// be the regression rather than the guard.
    ///
    /// The nested shape needs no explicit rejection: reading `threadId` flatly is
    /// what excludes it, and a frame carrying *both* fields in contradiction never
    /// reaches here at all — [`Visit::admits`] rejects [`FrameThread::Conflicted`]
    /// outright, bound or not.
    ///
    /// Parsed **locally** rather than by tightening [`frame_thread_id`]: that
    /// resolver's generosity is load-bearing for the admit filter and the switch
    /// announcement, and narrowing it to satisfy this one caller would break the
    /// two paths it was written for.
    fn note_turn_start(&self, frame: &Value) {
        if frame.get("method").and_then(Value::as_str) != Some("turn/started") {
            return;
        }
        let named = frame
            .get("params")
            .and_then(|params| params.get("threadId"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let Some(id) = named else {
            crate::log_debug!(
                "codex link for {}: a turn/started naming no thread is not this run's \
                 movement; it cancels nothing",
                self.session.name
            );
            return;
        };
        if self.visit.thread_id.as_deref() != Some(id) {
            return;
        }
        let turn = frame.pointer("/params/turn");
        let names_a_turn = turn
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty());
        let reports_running = turn
            .and_then(|turn| turn.get("status"))
            .and_then(Value::as_str)
            == Some(LIVE_TURN_IN_PROGRESS);
        if !(names_a_turn && reports_running) {
            crate::log_debug!(
                "codex link for {}: a turn/started for {id} carrying no measured turn \
                 body (id present: {names_a_turn}, running: {reports_running}) is a \
                 shape this build has never seen the wire produce; it cancels nothing",
                self.session.name
            );
            return;
        }
        self.daemon.note_codex_turn_running(&self.session.uid);
    }

    /// Normalize one admitted frame and record whatever it turned into.
    ///
    /// Two halves, kept separable because one caller needs to act between them: see
    /// [`Connection::normalize_frame`] and [`Connection::record_minted`], and the
    /// switch cancellation in [`Connection::observe_notification`] that reads the
    /// first half's answer without waiting for the second.
    async fn ingest_frame(&mut self, frame: &Value) {
        let minted = self.normalize_frame(frame);
        self.record_minted(minted).await;
    }

    /// **The reading half: what does this frame say, and what does it turn into?**
    ///
    /// Synchronous, and deliberately so. Everything here is a read of the frame in
    /// hand — the movement it announces, and the facts the adapter derives from it —
    /// so the answer is available the moment the frame is, with no database between
    /// the question and it. The filing that follows is where the waiting lives, and
    /// [`Connection::observe_notification`]'s switch cancel exists on the strength of
    /// exactly that difference.
    ///
    /// The returned events are the adapter's **verdict on the frame**, which is not
    /// the same question as whether the store kept them. A fact the log already holds
    /// is filed as `None` by `record` and the frame was still perfectly well-formed;
    /// a frame the adapter refuses mints nothing at all. The caller that weighs this
    /// answer is asking about the frame, so it gets the adapter's verdict rather than
    /// dedup's.
    fn normalize_frame(&mut self, frame: &Value) -> Vec<protocol::event::PendingEvent> {
        self.note_turn_start(frame);
        self.adapter.ingest(frame)
    }

    /// **The filing half: put what the frame turned into into the log, and ring for
    /// whatever the log actually kept.**
    ///
    /// Every iteration is an await on the DB executor, which is why this is the half
    /// no cancellation is allowed to wait behind.
    async fn record_minted(&mut self, minted: Vec<protocol::event::PendingEvent>) {
        for pending in minted {
            self.note_terminal(&pending);
            let rings = rings_the_doorbell(&pending);
            // A write that failed is logged inside `record` and is nothing this path can
            // act on: the frame has been consumed, and a doorbell for a fact the log does
            // not hold would announce something no reader can open.
            let filed = self.record(pending).await.ok().flatten();
            // **The doorbell hangs off the FILED event, and only here.**
            //
            // `record` answers `None` for a fact the log already held, so a turn
            // terminal that some earlier `thread/resume` already described rings
            // nothing when the live frame re-derives it — the reader was told when
            // it happened, and a re-attach is not a second piece of news.
            //
            // Live-only is a property of *this call site* rather than of the fact:
            // this function is reached only from a frame read off the wire, while
            // the two recovery paths write through `record` and `Daemon::ingest`
            // and stop. See `Daemon::push_codex_turn_complete`, which is where the
            // reasoning is written down.
            if let (true, Some(event)) = (rings, filed.as_ref()) {
                self.daemon
                    .push_codex_turn_complete(self.session, event)
                    .await;
            }
        }
    }

    /// **Does this fact settle a turn this link attached across?**
    ///
    /// If so, the debt described on [`Connection::awaiting_terminal`] can now be paid:
    /// the turn is finished, so an answer about it will carry its real item ids, and one
    /// more `thread/resume` recovers whatever completed before this link subscribed.
    ///
    /// Only a **completed** terminal triggers it, and that is a deliberate narrowing. An
    /// `interrupted` or `failed` turn is a state no resume answer has ever been captured
    /// reporting, so [`CodexAdapter::plan_resume_seed`] refuses it — asking again would
    /// buy nothing and would turn an ordinary interrupt into a refuse-and-reconnect loop.
    /// The debt on such a turn stays unpaid and visible rather than chased.
    fn note_terminal(&mut self, pending: &protocol::event::PendingEvent) {
        if pending.kind != protocol::event::EventKind::TurnComplete {
            return;
        }
        if pending.payload.get("status").and_then(Value::as_str) != Some("completed") {
            return;
        }
        let Some(turn) = pending.turn_id.as_deref() else {
            return;
        };
        // The thread this terminal belongs to. A fact only reaches here after the D4
        // filter admitted it, so it is this visit's thread by construction — but the key is
        // built from the CURRENT binding rather than assumed, so a debt can never be
        // recorded under a thread the link is not on (round-1 P7).
        let Some(thread) = self.bound().map(str::to_string) else {
            return;
        };
        let key = (thread.clone(), turn.to_string());
        // Only a turn still awaiting its terminal transitions. A turn already owed has
        // nothing to add; one already settled has had its ask, and a replayed terminal
        // for it must not buy another.
        if self.debts.get(&key) == Some(&TurnDebt::AwaitingTerminal) {
            // Owed, NOT settled: a debt is discharged by a request reaching the wire,
            // and nothing here can issue one.
            self.debts.insert(key, TurnDebt::Owed);
            crate::log_info!(
                "codex link for {}: turn {turn} finished, and this link attached while it \
                 was running — asking once more so the items it could not name are \
                 recovered",
                self.session.name
            );
        }
    }

    /// Is a follow-up owed? **Peeked, never consumed** — see [`Connection::owed`]. The
    /// debt is cleared by [`Connection::launch_follow_up`], and only once the request
    /// that pays it is actually on the wire.
    /// **Does this link owe `thread` a pre-subscription recovery?** (round-3 P7.)
    ///
    /// True when a turn on that thread was seeded while RUNNING and its follow-up resume
    /// has not been launched. Those items — the ones that finished before this link
    /// subscribed — are reachable from nowhere else: no live frame carries them, and once
    /// the link switches away nothing will ever ask about that thread again. The obligation
    /// therefore has to outlive the switch.
    fn owes_recovery_on(&self, thread: &str) -> bool {
        self.debts
            .iter()
            .any(|((t, _), debt)| t == thread && *debt != TurnDebt::Settled)
    }

    /// Is a follow-up owed **for the thread this link is currently on**?
    ///
    /// Thread-scoped (round-1 P7): a resume can only ever ask about ONE thread, so a debt
    /// owed on a thread the link has switched away from cannot be paid by the request this
    /// would schedule. Counting it would fire a resume of the CURRENT thread and then mark
    /// the OTHER thread's debt settled — retiring a recovery that never happened.
    fn follow_up_owed(&self) -> bool {
        let Some(thread) = self.bound() else {
            return false;
        };
        self.debts
            .iter()
            .any(|((t, _), debt)| t == thread && *debt == TurnDebt::Owed)
    }

    /// A resume has just been sent. Every debt outstanding at this moment is settled by
    /// it: one answer describes the whole thread, so a single request recovers all of
    /// them at once.
    ///
    /// A debt recorded *after* this point stays owed and fires when the connection is
    /// attached again — which is what keeps at most one follow-up in flight without
    /// letting the next one fall on the floor.
    /// **A follow-up went out but did not RECOVER anything** (round-3 P7).
    ///
    /// `launch_follow_up` marks a debt `Settled` at the send site, which is right for the
    /// purpose it was written for — one ask per turn, never a loop. But "asked" is not
    /// "recovered": if that ask comes back not-ready, refused, or unreadable, the items it
    /// was for are still missing, and a debt left `Settled` would say they had been found.
    ///
    /// Rolling it back to `Owed` is what lets the switch see a genuine outstanding
    /// obligation and carry it. The one-ask-per-turn bound still holds: the roll-back only
    /// happens on an answer that recovered nothing.
    fn unsettle_debts(&mut self, target: &str) {
        for ((thread, _), debt) in self.debts.iter_mut() {
            if thread == target && *debt == TurnDebt::Settled {
                *debt = TurnDebt::Owed;
            }
        }
    }

    fn launch_follow_up(&mut self, target: &str) {
        // Settles only the debts of the thread the request ACTUALLY names (round-1 P7).
        // One answer describes one whole thread, so it pays every debt of that thread at
        // once — and none of any other's.
        for ((thread, _), debt) in self.debts.iter_mut() {
            if thread == target && *debt == TurnDebt::Owed {
                *debt = TurnDebt::Settled;
            }
        }
    }

    /// Record a fact, and say what the log made of it.
    ///
    /// `Some(event)` is a fact that was genuinely filed — the caller may act on it.
    /// `None` covers both a key the log already held (a recovery re-describing what
    /// is there, which is the ordinary case and costs nothing) and a write that
    /// failed. Neither is something to announce: one is not new, and the other did
    /// not happen.
    /// **The failure is reported as well as logged** (round-8 F2). The live path has
    /// nothing to do with a fact it could not write — the frame is gone and no state
    /// depends on it — but the recovery path does: whether its answer was written is
    /// exactly what decides whether the debt it was paying is discharged or owed again.
    async fn record(
        &self,
        pending: protocol::event::PendingEvent,
    ) -> Result<Option<protocol::event::Event>> {
        let filed = self.daemon.ingest(pending).await;
        if let Err(err) = &filed {
            crate::log_error!(
                "codex link for {}: could not record a fact: {err:#}",
                self.session.name
            );
        }
        filed
    }

    /// Bind this connection to a thread from the **`thread/started` it watched
    /// arrive**. The other thing that binds is an accepted resume answer
    /// ([`Connection::attach_from_seed`]); see the module doc's attach contract.
    /// Bind to the announced thread — or, if one is already bound, **FOLLOW THE SWITCH**.
    ///
    /// # The switch signal is the announcement, and that is measured
    ///
    /// 2e-4c spike, real codex 0.147 TUI driven through `/new`: `thread/started` for the
    /// new thread is broadcast to **every initialized connection**, whatever it is
    /// subscribed to and whether it is subscribed at all (proven on three connections at
    /// once: one resume-subscribed to the old thread, one subscribed to a different
    /// thread, and one merely initialized). It is therefore the ONE signal a link that is
    /// watching thread A reliably receives when the operator moves to thread B.
    ///
    /// It is also the ONLY one. The same capture shows a resume-subscribed connection
    /// receives **nothing else** about the new thread — no `turn/*`, no `item/*`, not one
    /// frame of B's turn reached A's subscription — so a link that ignored the
    /// announcement would sit on a thread nobody is using, recording nothing, for the life
    /// of the session. That is exactly what this build did before 2e-4c.
    ///
    /// # What following costs, and what it does not
    ///
    /// * **The generation is bumped.** D4: a generation identifies a VISIT, not a thread.
    ///   The old visit is retired here and the new one begins, which is what makes the two
    ///   distinguishable in the log, in [`Ingress`], and to anything downstream that
    ///   attributes a frame to a visit.
    ///
    ///   Be precise about what that bump does *at this call site*, because overstating it
    ///   would be the kind of claim this module does not make: `observe_notification`
    ///   builds its stamp out of this same `Visit`, so the generation term of
    ///   [`Visit::admits`] compares a value with itself and cannot reject anything. **The
    ///   term that actually retires stale-A frames here is the THREAD term** — once
    ///   `thread_id` moves to B, a frame naming A is not this session's fact and is
    ///   filtered. The generation becomes load-bearing the moment a frame can arrive
    ///   stamped with a generation other than the live one, i.e. when a second delivery
    ///   connection exists (Phase 3's fanout). Until then it is honest bookkeeping, and it
    ///   is kept because a visit counter that only starts counting when it is needed has
    ///   no history to count.
    /// * **The thread is re-pointed**, which is what the caller's redirect block reads to
    ///   move `resume_target` and to supersede an outstanding resume for A.
    /// * **Nothing is unrecorded.** A's facts are already durable and stay so; A's own
    ///   `<A>:thread_started` identity key is thread-namespaced, so B's announcement mints
    ///   its own and neither shadows the other under first-wins.
    ///
    /// Returns the thread that was retired, iff this announcement was a switch.
    fn bind_or_follow(&mut self, frame: &Value) -> Option<String> {
        if frame.get("method").and_then(Value::as_str) != Some("thread/started") {
            return None;
        }
        let FrameThread::Named(id) = frame_thread_id(frame) else {
            return None;
        };
        match self.visit.thread_id.as_deref() {
            // The first announcement: an ordinary bind.
            None => {
                crate::log_info!(
                    "codex link for {}: bound to thread {id}, announced on this connection",
                    self.session.name
                );
                self.visit.thread_id = Some(id.to_string());
                // **Carried HERE, in the same breath as the binding.**
                //
                // `thread/started` is broadcast once and never replayed, so the moment
                // this frame is read is the only moment the id exists anywhere. Two
                // things used to be between that moment and the carry:
                //
                //   * an `.await` — the caller ingests the frame next
                //     ([`Connection::observe_notification`]), and a registration that
                //     parked this link mid-ingest snapshotted a cell that did not yet
                //     name the thread the link had just bound to;
                //   * the wrong SLOT — the loop mirrored the binding into
                //     [`Carried::hint`], which [`Carried::is_informative`] excludes
                //     (rightly: a hint is the registration's claim, and a replacement is
                //     told it too) and which [`run`] overwrites with the incoming
                //     registration's own claim. A first wire-derived thread was
                //     therefore dropped at every registration boundary.
                //
                // `pending_candidate` is the slot that already means this: announced by
                // the wire, not adopted, and the thing to resume first. The single
                // adoption site clears it, and so does the fallback.
                self.carried
                    .lock()
                    .expect("the carried link state")
                    .pending_candidate = Some(id.to_string());
                None
            }
            // A re-announcement of the SAME thread. The app-server broadcasts one
            // `thread/started` per creation, so this is not a shape the wire has shown —
            // but it is trivially idempotent and must not be read as a switch, or a
            // duplicate would retire a live visit.
            Some(bound) if bound == id => None,
            // A DIFFERENT thread: the operator pressed `/new`.
            Some(retired) => {
                let retired = retired.to_string();
                self.visit.generation += 1;
                self.visit.thread_id = Some(id.to_string());
                crate::log_info!(
                    "codex link for {}: thread {id} was announced while bound to \
                     {retired} — following the switch; the visit is now generation {} \
                     and {retired} is retired (its recorded facts stand; frames naming \
                     it are no longer this session's)",
                    self.session.name,
                    self.visit.generation
                );
                Some(retired)
            }
        }
    }

    /// Apply a held switch candidate: bump the generation, retire the visit, re-point the
    /// thread. Returns the thread that was retired, or `None` if there is no candidate (or
    /// the candidate is already the bound thread).
    ///
    /// The state change is exactly what [`Connection::bind_or_follow`] used to do inline;
    /// only WHEN it happens moved (round-1 P5/P6).
    /// **Give up on the chased thread and go back** (round-2 P5).
    ///
    /// A fallback that moved only the ATTACH target was not a fallback at all: the visit
    /// stayed on the un-adopted thread, so the bound-target redirect — whose whole job is
    /// to make the target agree with the visit — pulled the target straight back, and the
    /// link chased, was refused, fell back, chased again, for ever. The suppression flag
    /// that used to guard this was papering over the missing half.
    ///
    /// Giving up means the link IS back on the previous thread: it never subscribed to the
    /// chased one, that thread's only fact (its announcement) is already recorded, and its
    /// history stays resumable on demand. The generation advances because this is a new
    /// VISIT of the old thread (D4), which is what keeps any late frame stamped under the
    /// abandoned visit distinguishable.
    fn revert_visit_to(&mut self, previous: &str) {
        if self.visit.thread_id.as_deref() == Some(previous) {
            return;
        }
        self.visit.generation += 1;
        self.visit.thread_id = Some(previous.to_string());
        self.switch_candidate = None;
    }

    fn apply_switch_candidate(&mut self) -> Option<String> {
        let candidate = self.switch_candidate.take()?;
        let retired = self
            .visit
            .thread_id
            .clone()
            .or_else(|| self.carried_target.clone())?;
        if retired == candidate {
            return None;
        }
        self.visit.generation += 1;
        self.visit.thread_id = Some(candidate.clone());
        // A FRESH switch gets a fresh adoption budget.
        self.unadopted_retries = 0;
        self.fallback_due = false;
        crate::log_info!(
            "codex link for {}: following the switch from {retired} to {candidate}; the \
             visit is now generation {} and {retired} is retired (its recorded facts \
             stand; frames naming it are no longer this session's)",
            self.session.name,
            self.visit.generation
        );
        Some(retired)
    }

    /// Record a `thread/started` that names a DIFFERENT thread as a switch CANDIDATE,
    /// without touching the visit (round-1 P5/P6).
    ///
    /// # Why the announcement is not applied where it arrives
    ///
    /// Two reasons, both about what would be lost if it were:
    ///
    /// * **P6 — an in-flight recovery is the only copy of some facts.** When the switch
    ///   lands, this link may have a `thread/resume` of the OLD thread outstanding. That
    ///   answer can be the only thing that will ever name the items which finished before
    ///   this link subscribed: no live frame carries them, and once the link has moved on
    ///   it will never ask about that thread again. Re-pointing the visit here marks the
    ///   request superseded, so its answer is received and thrown away. The loop settles it
    ///   FIRST and follows afterwards.
    /// * **P5 — the announcement is not an adoption.** `thread/started` is a broadcast; the
    ///   broker has not necessarily verified the new thread's creation yet, and it may
    ///   never (a creation response can prove failure, or prove nothing at all). Until it
    ///   does, the new thread is not a session thread and a `thread/resume` naming it is
    ///   policy-refused. Committing the visit to it irrevocably would strand the link on a
    ///   thread that was never adopted, with the thread it *could* still read forgotten.
    ///
    /// Returns what the frame turned out to be, in the caller's terms: whether its
    /// ingest is now the caller's obligation (round-2 P8), and — separately — whether
    /// this announcement is a person moving. See [`Switch`] for why those two are not
    /// one answer.
    ///
    /// # Why a switch announcement's FACT is recorded at once
    ///
    /// The candidate slot holds ONE thread. With `/new` pressed twice while a recovery of A
    /// is still outstanding, C overwrites B and B is dropped — so what, exactly, is lost?
    ///
    /// From the measurements: a link that never subscribed to B receives **nothing about B
    /// but its announcement**. No `turn/*`, no `item/*` — those reach only the
    /// resume-subscribed connection. And B's history is not lost either: a retired thread
    /// stays fully resumable, so it can be recovered on demand later.
    ///
    /// So the only thing a dropped candidate can cost is the announcement's own fact — and
    /// that fact needs no candidate at all. It is verified broadcast content from this
    /// session's own app-server, describing a thread that session just created, so it is
    /// recorded the moment it arrives. The candidate then carries only the SUBSCRIPTION
    /// decision, and the latest announcement wins it: **the link subscribes to where the
    /// user IS**. Queueing candidates would chase threads the user has already left.
    ///
    /// (The frame is ingested by the CALLER rather than here because ingestion is async and
    /// this is the sync bind/filter path. Returning the obligation keeps the one ingest
    /// site — and therefore the one dedup path — intact.)
    fn note_switch_candidate(&mut self, frame: &Value) -> Switch {
        if frame.get("method").and_then(Value::as_str) != Some("thread/started") {
            return Switch::Passed;
        }
        let FrameThread::Named(id) = frame_thread_id(frame) else {
            return Switch::Passed;
        };
        // The FIRST bind is not a switch and needs no deferral: there is no outstanding
        // recovery to lose and no previous thread to fall back to. It binds, and the
        // ordinary filter then admits the frame — so the caller must NOT ingest it twice.
        if self.visit.thread_id.is_none() && self.carried_target.is_none() {
            let _ = self.bind_or_follow(frame);
            return Switch::Passed;
        }
        let current = self
            .visit
            .thread_id
            .clone()
            .or_else(|| self.carried_target.clone());
        if current.as_deref() == Some(id) {
            // A re-announcement of the thread we are already on (or carrying). If we are
            // BOUND to it the ordinary filter admits the frame; if we are merely carrying
            // it as a target, bind now — this is the reconnect case (P6c) where the
            // announcement finally names the thread we were already asking about.
            if self.visit.thread_id.is_none() {
                let _ = self.bind_or_follow(frame);
            }
            return Switch::Passed;
        }
        // Whether the slot MOVES is read before it is written, because that — not the
        // frame's arrival — is what makes this a person acting on the session.
        let moved = self.switch_candidate.as_deref() != Some(id);
        if moved {
            crate::log_info!(
                "codex link for {}: thread {id} was announced while on {} — held as a \
                 switch candidate until any outstanding recovery for the current thread \
                 is settled; its own fact is recorded now",
                self.session.name,
                current.as_deref().unwrap_or("?")
            );
        }
        // Latest wins. A candidate this replaces cost nothing but its announcement, which
        // was recorded when it arrived.
        self.switch_candidate = Some(id.to_string());
        // **Persisted HERE, at note time, in every path** (round-3 P8) — including the
        // handshake, where the main loop has not begun and a mirror would never run.
        {
            let mut carried = self.carried.lock().expect("the carried link state");
            carried.pending_candidate = Some(id.to_string());
            // The thread to fall back to is the last ADOPTED one — the only slot that is
            // provably readable (round-3 P9). Never overwritten by a second switch.
            //
            // **Honest status: REDUNDANT, and deliberately kept** (closing S10).
            //
            // Deleting it changes no test, and the earlier claim that its case was merely
            // unstageable was wrong: the apply site sets the fallback from the thread being
            // retired, and every path that reaches a fallback passes through one of the two
            // writers. This one is genuinely redundant with that.
            //
            // It stays because the two writers answer different questions and the redundancy
            // is fail-closed: the apply site knows which thread is being RETIRED, this one
            // knows which was last ADOPTED, and on the un-applied path — a candidate noted
            // whose connection dies before the loop can act — only this one has run. A
            // redundant write on a fail-closed slot costs nothing; a missing one strands the
            // link on a thread that may never be adopted.
            if carried.fallback.is_none() {
                carried.fallback = carried.adopted.clone();
            }
        }
        if moved {
            Switch::Novel
        } else {
            Switch::Held
        }
    }

    /// The thread this connection is bound to, if it has been announced.
    fn bound(&self) -> Option<&str> {
        self.visit.thread_id.as_deref()
    }

    /// **What this connection is, for a resolver — derived, never tracked.**
    ///
    /// Read off the two fields that already decide it, so there is no third
    /// representation of the link's state to fall out of step with them:
    ///
    ///   * `visit.thread_id` is the **binding** — whose frames these are. Several
    ///     things write it (an announcement, a switch, a fallback, an accepted
    ///     resume), which is exactly why it alone cannot answer the question.
    ///   * `adopted_thread` is the **subscription**, and it has *one* writer:
    ///     `attach_from_seed`, where a `thread/resume` is accepted. So "subscribed"
    ///     is a property of that single write site rather than a flag this function
    ///     has to be trusted to maintain.
    ///
    /// Subscribed therefore means the two **agree**, and the states where they do
    /// not are the ones worth naming. Bound to a thread nothing has adopted: a link
    /// waiting out the not-ready error before a thread's first turn. Adopted a
    /// thread the visit has since left: a `/new` switch, where the new thread is
    /// bound and the old one's subscription is no longer what this session is about
    /// — the honest answer is `Bound`, and it becomes `Subscribed` when the new
    /// thread's resume is accepted.
    ///
    /// **Deliberately not read off [`Attach`].** A subscribed connection re-enters
    /// `Backoff`/`Awaiting` to fire the follow-up resume it owes (the round-3
    /// mid-turn debt) while frames keep flowing to it, so an `Attach`-derived answer
    /// would drop that connection to `Bound` for the length of a round trip and
    /// report a live addressee as unreachable.
    ///
    /// # A held switch candidate is deliberately invisible here
    ///
    /// While a resume of A is outstanding, an announcement of B is parked in
    /// `switch_candidate` and applied only once that answer settles — which is what
    /// keeps A's recovery from being thrown away, and which can last as long as the
    /// resume budget. Through all of it this reports `Subscribed { A }`, and that
    /// is the honest answer rather than a stale one: the connection *is* subscribed
    /// to A, A's frames are still arriving, and a decision raised on A is answered
    /// on A.
    ///
    /// What it does not say is that a newer thread has been announced — and it must
    /// not, because an announcement is not an adoption. B has not been resumed and
    /// may never be: a thread with no rollout is policy-refused on every attempt,
    /// and the link falls back to A. Publishing B would name a thread nothing has
    /// verified as readable, which is the mistake `Carried`'s three distinct slots
    /// were separated to prevent (round-3 P9). So the *announced* head is carried
    /// where a chase can act on it, and the *adopted* head is what is published.
    ///
    /// # How long the gap really lasts
    ///
    /// [`RESUME_BUDGET`] bounds the **hold**, and only the hold. The candidate is
    /// parked until A's outstanding resume settles, and an unanswered resume ends the
    /// connection at that budget, so `Subscribed { A }` cannot outlast it. An earlier
    /// version of this note stopped there and called it the bound on the whole gap,
    /// which was the overstatement: applying the candidate is where the chase
    /// *starts*, not where it ends.
    ///
    /// What follows the hold has two shapes, and neither is a round trip:
    ///
    ///   * **Applied on this connection.** The visit moves to B and the answer
    ///     becomes `Bound { B }` — the wire has named B, nothing has read it. That
    ///     stands for as long as B is refused, which is the *fallback's* budget:
    ///     [`ADOPTION_RETRIES`] asks with backoff, after which the link gives up on
    ///     B, clears the candidate and reverts to the thread it can read.
    ///   * **Carried past a dead connection.** `thread/started` is broadcast once and
    ///     never replayed, so a candidate that died with its connection could never
    ///     be rediscovered; it lives in [`Carried::pending_candidate`], is what the
    ///     next connection resumes first ([`Carried::first_target`]), and is seeded
    ///     into that connection's `switch_candidate`. So the sequence has **two**
    ///     parts, not one: the new connection has bound nothing, so it opens on
    ///     `Unbound { adopted: A }` and the fleet names A; then the first settle —
    ///     B's first policy refusal is one — applies the candidate and the answer
    ///     becomes `Bound { B }`, on the same terms as the shape above.
    ///
    ///     An earlier version of this note said the carried chase published
    ///     `Unbound { adopted: A }` for the whole of it. That was wrong about this
    ///     code, and the test named below is what pins the real sequence.
    ///
    /// Pinned by
    /// `a_switch_target_that_is_never_adopted_falls_back_and_the_fallback_completes`
    /// for the first shape, and by
    /// `a_carried_candidate_names_the_adopted_thread_only_until_it_is_applied`
    /// (published states) with `a_reconnecting_link_never_publishes_less_than_it_knows`
    /// (the reconnect that opens it) for the second.
    ///
    /// A caller that must not act on a superseded thread needs a state this type does
    /// not have, and inventing one now would be a shape no consumer exists to read —
    /// Phase 3 answers decisions (correctly scoped to A) and Phase 4 steers, which is
    /// where the distinction first earns its keep.
    fn addressee(&self) -> CodexAddressee {
        addressee_of(
            self.visit.thread_id.as_deref(),
            self.adopted_thread.as_deref(),
            // **What the LINK knows, for the one state the connection does not.**
            // A fresh connection has bound nothing and adopted nothing, but the link
            // it belongs to is resuming a thread it adopted on an earlier one — see
            // [`CodexAddressee::Unbound`]. Read from `carried_target`, which is the
            // adopted-only slot (round-2 P6c): a merely-pending candidate is not
            // something this link has verified, and must not be named.
            self.carried_target.as_deref(),
        )
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
            exit_replay: false,
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
    /// The turn it runs.
    const LIFECYCLE_TURN: &str = "01a0127a-d9cd-7461-84d7-6eea6d0b98a5";
    /// The real 0.147 notification stream for one message turn.
    const LIFECYCLE: &str = include_str!("../../../fixtures/codex/lifecycle.jsonl");
    /// The real 0.147 stream for the first turn of a fresh thread. Read here for one
    /// thing only: it is the nearest committed capture of a `turn/started` frame, and
    /// round-4 F7's guard is a claim about that frame's shape. `lifecycle.jsonl`
    /// begins after the turn has started and carries none.
    const FIRST_TURN: &str = include_str!("../../../fixtures/codex/first-turn.jsonl");

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
        /// A result whose `turns[]` sits at the TOP level rather than under
        /// `thread` — so there is no `result.thread.turns` to read at all.
        TurnsNotUnderThread,
        /// **The real thing.** The committed populated answer, retargeted onto the
        /// thread and the turn the lifecycle capture is about, so the answer and the
        /// notification stream beside it describe the SAME facts. Accepted.
        Populated,
        /// The same answer, and then **silence** — the leg replays nothing. What lands
        /// in the store therefore came from the ANSWER and from nowhere else.
        PopulatedNoReplay,
        /// The same answer, and then the **app-server goes away for good**: the leg
        /// closes the connection and unlinks its socket, so every later dial fails
        /// outright. The link is left permanently in its reconnect backoff, which is
        /// what makes a between-connections state observable without racing a timer.
        PopulatedThenGone,
        /// The same answer, and then a **reconnect that handshakes and goes deaf**:
        /// the leg closes act one, accepts every later dial, answers `initialize`,
        /// and never answers the `thread/resume` that follows. That holds the link
        /// in the one state the between-connections script cannot reach — connected,
        /// bound to nothing on THIS connection, its resume in flight — for the whole
        /// of [`RESUME_BUDGET`], which is where a threadless `Unbound` used to send
        /// the fleet back to the registration's launch-time claim.
        PopulatedThenDeaf,
        /// The same answer with its turn reported `inProgress` and its items carrying
        /// the **placeholder ids** a running turn is measured to report (`item-1`, …).
        /// Accepted, and it must contribute no item fact whatsoever.
        PopulatedInProgress,
        /// A turn state this wire has never reported. Refused.
        UnmeasuredTurnStatus,
        /// A turn whose `itemsView` says its item list is partial. Refused.
        PartialItemsView,
        /// **The mid-turn attach, and what settles it.** The first resume answers with
        /// the turn still RUNNING, and the leg then replays only the TAIL of that turn —
        /// the userMessage frames are withheld, standing for items that finished before
        /// this link subscribed and which no live frame will ever carry. The follow-up
        /// resume the link owes itself is answered with the turn FINISHED, carrying both
        /// items under their real ids, which is the only thing that can recover them.
        MidTurnThenCompleted,
        /// **Two debts, and the second falls due while the first is in flight.**
        ///
        /// The answer seeds TWO turns running. Turn A terminalizes, so a follow-up
        /// fires; while that request is outstanding turn B terminalizes too. The second
        /// answer still reports B running, so only a third resume can recover it — which
        /// happens if and only if B's debt survived being noticed at a moment when no
        /// request could be issued.
        TwoMidTurnDebts,
        /// **The stale request is answered, and must lose.** The first resume (for a
        /// stale hint) goes unanswered until the leg has announced the real thread; only
        /// then does it answer — with a perfectly valid populated result about the stale
        /// thread. The announcement supersedes that request, so its answer must bind
        /// nothing, seed nothing and report nothing.
        AnnounceThenAnswerStale,
        /// A well-formed answer whose one RUNNING turn carries an item with no
        /// routing identity. Refused.
        ///
        /// The turn is running deliberately. A *finished* turn's items are read one by
        /// one, so a malformed one is refused by that read whatever else is in place —
        /// which mutation testing showed makes it useless for pinning the up-front
        /// check. A running turn's items are never read at all, so the up-front check
        /// is the only thing standing between this answer and an attach.
        ItemWithoutType,
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
        /// **A RECOVERY ANSWER CROSSING THE SWITCH BOUNDARY** (round-1 P6).
        ///
        /// The first resume is answered with the turn still RUNNING, so the link owes
        /// itself a follow-up; the leg replays only the TAIL, standing for items that
        /// finished before the link subscribed. The turn then terminalizes, the link fires
        /// its follow-up — and the leg announces a SWITCH while that request is in flight,
        /// answering it only afterwards with the turn FINISHED and its real ids.
        ///
        /// That answer is the ONLY thing that can ever name those items: no live frame
        /// carries them, and once the link moves to the new thread it will never ask about
        /// the old one again. It must be CONSUMED before the switch is followed.
        RecoveryCrossingASwitch,
        /// **THE FOLLOWED-TO THREAD IS NOT ADOPTED YET** (round-1 P5).
        ///
        /// The link is announced a new thread and re-targets to it — but the broker has not
        /// finished verifying that thread's creation, so the first resumes of it come back
        /// as the broker's own policy refusal (`-32001`). That window is ordinary and short
        /// (one `thread/start` round trip), so it must be RETRIED IN PLACE rather than
        /// treated as an unreadable answer: ending the connection would reconnect, ask the
        /// same question, and be refused again — a loop, on every switch.
        ///
        /// After a few refusals the leg relents and answers properly, and the link must be
        /// attached to the new thread on the connection it has had all along.
        SwitchThenNotAdoptedYet,
        /// **THE HANDSHAKE ANNOUNCEMENT MUST SUPERSEDE A STALE HINT** (closing S8).
        ///
        /// The link carries a registration hint. The leg announces the real thread at
        /// `initialized` — and then drops before anything is adopted. Nothing will ever
        /// announce that thread again, so the reconnect can only reach it if the
        /// announcement replaced the hint in the SLOT a reconnect reads.
        ///
        /// The leg refuses the stale hint outright, so a reconnect that still asks for it
        /// gets nowhere: the observable is which thread the second connection names.
        AnnouncedThenDroppedBeforeAdopting,
        /// **A CROSSING RECOVERY THAT NEVER LANDS BEFORE THE SWITCH** (round-3 P7).
        ///
        /// The link attaches to A MID-TURN, so it owes a follow-up: A's earlier items
        /// finished before the subscription existed and are reachable from nowhere else.
        /// The turn terminalizes, the follow-up goes out — and the leg announces B and
        /// answers that follow-up with the measured NOT-READY error, so the recovery never
        /// lands. The link follows the switch and adopts B.
        ///
        /// A's withheld item must still be recovered: the obligation outlives the switch,
        /// and is paid once, on demand, after B is adopted. That is the on-demand recovery
        /// the P8 doctrine promises, made real rather than asserted.
        SwitchWithACrossingRecoveryOwed,
        /// **THE APP-SERVER GOES AWAY AS THE ON-DEMAND RECOVERY IS WRITTEN** (round-3 P7).
        ///
        /// The same arc as [`ResumeAnswer::SwitchWithACrossingRecoveryOwed`] up to the point
        /// where B is adopted — and then B's answer is the leg's **last act**: it sends it
        /// and drops the socket, so by the time the link has ingested that answer, noticed
        /// the debt it now owes on A and reached for the wire, the peer is gone and
        /// `send_resume` fails.
        ///
        /// The socket path is unlinked before this connection is even served (the
        /// [`ResumeAnswer::PopulatedThenGone`] mechanism), so no reconnect can be served
        /// either. That is what makes the carry a **settled** state a test can read rather
        /// than a moment it has to catch: nothing can come along afterwards and pay the
        /// debt, so whatever the carry holds at the end is what the failed write left in it.
        ///
        /// Making the *write* fail is the honest reach of this harness. A scripted leg
        /// cannot inject an error into the link's own socket writes; dropping the far end at
        /// the moment the recovery is issued is the same failure by the route the wire
        /// actually produces it.
        SwitchThenTheLegVanishesAsTheRecoveryIsWritten,
        /// **THE ON-DEMAND RECOVERY IS NEVER ANSWERED** (round-3 P7).
        ///
        /// The same arc again — A attached mid-turn, the follow-up refused, B announced and
        /// adopted — and then the leg simply **says nothing** to the recovery resume for A.
        /// That is not a contrived silence: the case an on-demand recovery exists for is a
        /// thread nobody is on any more, and an app-server that never answers about one is
        /// precisely the failure the state has to survive.
        ///
        /// **B is joined mid-turn as well**, with its userMessage withheld, and B's turn
        /// terminalizes **while the recovery is still outstanding** — then the leg goes
        /// quiet and stays quiet. That ordering is the whole of the case, and an earlier
        /// version of this leg had it the other way round: sending B's terminal *after*
        /// [`RESUME_BUDGET`] had elapsed meant the frame that made B's follow-up due also
        /// woke the loop that could schedule it, so a link that never re-checked its debts
        /// on leaving `Recovering` passed anyway.
        ///
        /// Delivered during the recovery instead, the debt is recorded against a state the
        /// bottom-of-loop scheduler declines for, and nothing arrives afterwards to make it
        /// ask again: an elapsed deadline does not stop the link READING — `timeout_at`
        /// polls the socket before it checks its own deadline — but every site that issues a
        /// resume fires from `Attach::Attached`, and a quiet socket produces no further
        /// pass. Only a link that re-checks what is owed at the moment it leaves
        /// `Recovering` ([`leaving_recovery`]) asks for B's follow-up, and only that answer
        /// can name B's withheld item.
        SwitchThenTheRecoveryIsNeverAnswered,
        /// **THE RECOVERY IS ANSWERED AND THE LEG THEN SAYS NOTHING** (round-8 F5).
        ///
        /// The sibling of [`ResumeAnswer::SwitchThenTheRecoveryIsNeverAnswered`], and the
        /// reason it needs one: that script leaves `Recovering` through the TIMEOUT arm,
        /// so it pins only one of the two exits. Replacing the ANSWERED arm's
        /// [`leaving_recovery`] with a bare `Attach::Attached` passes it untouched.
        ///
        /// The arc is identical up to the on-demand ask about A, and then diverges in the
        /// one way that matters: B's turn terminalizes **during** the recovery — so B's
        /// follow-up is recorded against `Recovering`, which the bottom-of-loop scheduler
        /// declines for — and the recovery is then ANSWERED, readably, after which the leg
        /// is silent for good.
        ///
        /// The answered arm `continue`s to the top of the loop, past that scheduler, onto
        /// a `ws.next()` with no deadline. On a socket that will not speak again there is
        /// no later pass at all, so B's withheld userMessage can be named only if the
        /// answered exit itself re-checks what is owed.
        SwitchThenTheRecoveryIsAnsweredAndTheLegGoesQuiet,
        /// **A THIRD THREAD IS ANNOUNCED WHILE THE ON-DEMAND RECOVERY IS OUTSTANDING**
        /// (round-8 F1).
        ///
        /// The same arc again — A joined mid-turn, its follow-up refused, B announced and
        /// adopted, the on-demand ask about A on the wire — and then the operator presses
        /// `/new` a second time. `thread/started` for C is broadcast to every initialized
        /// connection, so it lands on this one with A's recovery unanswered.
        ///
        /// `Recovering` is a resume OUTSTANDING. A candidate applied against it re-targets
        /// the attach and schedules `resume(C)`, which goes out beside A's request — the
        /// pipelining A2 forbids — and A's answer, when it arrives, matches no active id
        /// and is dropped as unroutable. That answer is the only thing that will ever name
        /// A's pre-subscription items: the leg answers it with A's real ids **after** the
        /// announcement, so whether the link consumed it or threw it away is directly
        /// readable in the log.
        SwitchThenAThirdIsAnnouncedDuringTheRecovery,
        /// **THE LEG VANISHES AFTER THE RECOVERY IS ASKED, BEFORE IT IS ANSWERED**
        /// (round-8 F2).
        ///
        /// The near-twin of [`ResumeAnswer::SwitchThenTheLegVanishesAsTheRecoveryIsWritten`],
        /// and the difference is the whole finding: there the WRITE fails, so the ask never
        /// happened; here the write succeeds, the request is genuinely on the wire, and the
        /// leg then drops the socket without answering.
        ///
        /// A slot cleared on the strength of the send is empty at exactly that moment, and
        /// nothing else names A — the fallback is cleared by the adoption of B that made
        /// the debt payable. So the reconnect adopts B, finds nothing owed, and A's
        /// pre-subscription items are unreachable for the rest of the session.
        ///
        /// The socket path is **not** unlinked, unlike `PopulatedThenGone`: the reconnect
        /// is the observable. Connection 2 adopts B and must then ask about A.
        SwitchThenTheLegVanishesAfterTheRecoveryIsAsked,
        /// **THE ON-DEMAND RECOVERY IS ANSWERED, AND THE ANSWER SAYS THE TURN IS STILL
        /// RUNNING** (round-9 F1).
        ///
        /// The near-twin of the two above, and the difference is the whole finding: the
        /// request is sent, the answer arrives, it is perfectly readable, and every fact
        /// it describes is filed — and none of those facts is the one that is owed. A
        /// turn reported `inProgress` describes its items with **placeholder** ids, so
        /// the answer contributes no item fact at all and the real ids stay unreachable
        /// until some later answer reports that turn finished.
        ///
        /// The link is on B, so A's own terminal is filtered when it arrives: nothing on
        /// this connection can settle the debt afterwards. A discharge on the strength of
        /// "the answer was consumed" therefore loses A's pre-subscription items exactly
        /// as a discharge on the strength of the send did.
        ///
        /// The leg drops after that answer and serves the reconnect, whose ask about A
        /// **is** answered with the turn finished and its real ids — which is what makes
        /// "the debt survived and was later settled" one observable rather than two
        /// hopes.
        SwitchThenTheRecoveryAnswersARunningTurn,
        /// **THE CANDIDATE MUST SURVIVE A CONNECTION THAT DIES BEFORE APPLYING IT**
        /// (round-2 P6b).
        ///
        /// Connection 1: the resume of A is answered and A's stream replays, so A is
        /// ADOPTED. The leg then announces B **and immediately drops the connection**,
        /// before the link's next pass can apply the candidate.
        ///
        /// `thread/started` is broadcast once and never replayed, so nothing will ever tell
        /// this link about B again. If the candidate died with the connection, the link
        /// would sit on A for the rest of the session while the user works in B. The
        /// reconnect must therefore ask about B — with A still the thing to fall back to.
        CandidateAnnouncedThenTheLegDrops,
        /// **A RECONNECT CARRYING AN ADOPTED TARGET IS ANNOUNCED A DIFFERENT THREAD**
        /// (round-2 P6c).
        ///
        /// Connection 1 adopts A and drops. Connection 2 carries A and is announced B as
        /// its FIRST announcement. That must get candidate treatment — announcement is not
        /// an instant repoint, uniformly — with A as the fallback, exactly as it would on a
        /// connection that had been announced A earlier. Binding B instantly would discard
        /// a thread this link demonstrably could read in favour of one the broker may never
        /// adopt.
        ///
        /// The leg refuses B for ever, so the fallback to A is what the test observes.
        ReconnectCarryingAdoptedThenAnnouncedAnother,
        /// **TWO SWITCHES WHILE A RECOVERY IS OUTSTANDING** (round-2 P8).
        ///
        /// The link attaches mid-turn (so it owes a follow-up), and while that request is
        /// in flight the leg announces B and then C. The single candidate slot holds one
        /// thread, so B is replaced — and what B costs must be nothing but its
        /// announcement, which is recorded the moment it arrives. The SUBSCRIPTION chases
        /// C, because C is where the user is.
        TwoSwitchesDuringRecovery,
        /// **AN UNREADABLE ANSWER ARRIVES WITH A SWITCH ANNOUNCEMENT** (round-2 P7).
        ///
        /// The leg announces a switch and answers the outstanding resume with a shape this
        /// build cannot read, on the same pass. The unreadable answer must still END the
        /// leg loudly — a candidate must never suppress that stop — and the candidate must
        /// survive to the next connection, since `thread/started` is broadcast once and
        /// never replayed.
        UnreadableAnswerBesideASwitch,
        /// **THE SWITCH TARGET IS NEVER ADOPTED, AND THE FALLBACK MUST COMPLETE**
        /// (round-2 P5).
        ///
        /// The first resume (A) is answered and A's stream replays; the leg announces B and
        /// then refuses EVERY resume of B, for ever. Once the adoption budget is spent the
        /// link must fall back to A — and the fallback has to actually finish, which is the
        /// part that was structurally impossible before: the bound-target redirect rewrote
        /// the target back to B and marked the A-request superseded on the very next pass,
        /// so the link retargeted B, was refused, fell back, and looped.
        ///
        /// Scripted honestly: the leg refuses B more times than [`ADOPTION_RETRIES`], so
        /// the fallback is genuinely reached rather than short-circuited by a script that
        /// relented first.
        SwitchNeverAdoptedThenFallback,
        /// **THE MEASURED `/new` SWITCH, on one connection** (2e-4c).
        ///
        /// The first resume — for the announced thread A — is answered populated and A's
        /// captured turn stream replays, so this link is genuinely SUBSCRIBED to A. The
        /// leg then broadcasts a `thread/started` for a SECOND thread, which is exactly
        /// what the real app-server sends to every initialized connection when the
        /// operator presses `/new`, and is the ONLY thing it sends a connection about the
        /// new thread. The link must follow: re-target, re-resume on this same socket,
        /// and start recording B.
        ///
        /// B's stream carries the SAME item and turn ids as A's, retargeted. That is
        /// deliberate and it is what makes "zero cross-thread contamination" provable
        /// rather than incidental: if the dedup key were not thread-namespaced, B's facts
        /// would collide with A's and be silently dropped as duplicates.
        SwitchToSecondThread,
        /// **The first resume goes unanswered and the thread is announced instead.**
        /// The announcement redirects the target and settles nothing, so the stale
        /// attach times out, the connection ends, and the reconnect asks under the
        /// announced thread — which is how the redirect is observed.
        AnnounceLate,
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

    /// Whether the late-announcing script has already broadcast its thread, **for the
    /// life of the leg** rather than for the life of one connection.
    ///
    /// A reconnect to a thread that is already running gets no `thread/started`: the
    /// announcement happened once, on a connection that is gone. Scoping this per
    /// connection re-announced on every reconnect, which both misrepresents the wire and
    /// left the not-ready branch behind the announcement unreachable.
    type AnnouncedOnce = Arc<std::sync::atomic::AtomicBool>;

    /// **Which scripts stage an app-server that does not come back.**
    ///
    /// The path is unlinked before the connection is even served, so the dial the link
    /// makes after that connection drops fails outright rather than hanging on a backlog
    /// nobody is accepting from. That is what turns "the link is between connections"
    /// into a settled state a test can read instead of a moment it has to catch — and,
    /// for the carry, what makes "whatever is in the slot at the end" mean "what the
    /// dying connection left there" rather than "what some later connection got round to
    /// paying". A named predicate rather than a `matches!` repeated at both sites,
    /// because a script added to one and not the other is a leg that keeps accepting
    /// dials it is supposed to have stopped accepting.
    fn the_leg_never_comes_back(answer: ResumeAnswer) -> bool {
        matches!(
            answer,
            ResumeAnswer::PopulatedThenGone
                | ResumeAnswer::SwitchThenTheLegVanishesAsTheRecoveryIsWritten
        )
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
            let announced: AnnouncedOnce = Arc::default();
            let server = tokio::spawn({
                let seen = Arc::clone(&seen);
                let connections = Arc::clone(&connections);
                let socket = path.clone();
                async move {
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            return;
                        };
                        let nth = connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        // **The app-server that does not come back.** The path is
                        // unlinked before this connection is even served, so the dial
                        // the link makes after it drops fails outright rather than
                        // hanging on a backlog nobody is accepting from — which is what
                        // makes "the link is between connections" a settled state a
                        // test can read instead of a moment it has to catch.
                        if the_leg_never_comes_back(answer) {
                            let _ = std::fs::remove_file(&socket);
                        }
                        let seen = Arc::clone(&seen);
                        let announced = Arc::clone(&announced);
                        tokio::spawn(async move {
                            let _ = serve_scripted(
                                stream,
                                nth,
                                answer,
                                announce_thread,
                                seen,
                                announced,
                            )
                            .await;
                        });
                        if the_leg_never_comes_back(answer) {
                            return;
                        }
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

    /// The captured turn stream this leg replays **after a resume that subscribes**.
    ///
    /// The capture's own `thread/started` is always withheld: an announcement is a
    /// separate act on this leg (it happens at `initialized`, to a connection that has
    /// not resumed), and leaving it in here would let a subscription hand out a binding
    /// the real wire delivers by broadcast.
    fn capture() -> Vec<String> {
        lifecycle_frames()
            .into_iter()
            .filter(|line| {
                serde_json::from_str::<Value>(line)
                    .ok()
                    .and_then(|v| v.get("method").and_then(Value::as_str).map(str::to_string))
                    != Some("thread/started".to_string())
            })
            .collect()
    }

    /// The captured turn stream **without the userMessage**, standing for a turn joined
    /// after some of its items had already finished.
    ///
    /// Those items are unrecoverable from the live wire by construction — they completed
    /// before the subscription existed — and unrecoverable from the answer while the turn
    /// is still running, because a running turn reports placeholder ids. Only the answer
    /// that describes the turn finished can name them.
    fn capture_tail() -> Vec<String> {
        capture()
            .into_iter()
            .filter(|line| {
                serde_json::from_str::<Value>(line)
                    .ok()
                    .and_then(|v| v["params"]["item"]["type"].as_str().map(str::to_string))
                    != Some("userMessage".to_string())
            })
            .collect()
    }

    /// The captured turn's `userMessage` item — the one `capture_tail()` WITHHOLDS, so it
    /// can only ever reach the store through a resume answer that describes the turn
    /// finished. Read from `fixtures/codex/lifecycle.jsonl`'s own `item/completed`.
    const LIFECYCLE_USER_ITEM: &str = "01a0127a-dbdd-7d11-b925-5bb0c2dac319";

    /// A THIRD thread, for the two-switches-in-a-row case (round-2 P8).
    const THIRD_THREAD: &str = "01a039a6-d67c-7bb1-9050-071265c77067";

    /// The thread the `/new` switch moves TO. A second real-shaped thread id.
    const SWITCHED_THREAD: &str = "01a0399e-7504-79f3-8740-233aa08ca259";

    /// The captured turn stream retargeted onto `thread` — the same frames, the same item
    /// and turn ids, a different `threadId`.
    ///
    /// A plain string substitution would be wrong (it would rewrite an id that merely
    /// looked like the thread's), so the retarget is done on the parsed frame, at the one
    /// key that names the thread.
    fn capture_on(thread: &str) -> Vec<String> {
        capture()
            .into_iter()
            .map(|line| {
                let mut v: Value = serde_json::from_str(&line).expect("a captured frame");
                v["params"]["threadId"] = json!(thread);
                v.to_string()
            })
            .collect()
    }

    /// [`capture_tail`] retargeted onto `thread` — a turn joined after some of its items
    /// had already finished, on a thread other than the capture's own.
    ///
    /// Built out of the two existing notions rather than restating either: what "the tail"
    /// means is [`capture_tail`]'s business, and what retargeting means is [`capture_on`]'s.
    /// A script needs both at once the moment the thread it joins mid-turn is the thread it
    /// SWITCHED to.
    fn capture_tail_on(thread: &str) -> Vec<String> {
        capture_tail()
            .into_iter()
            .map(|line| {
                let mut v: Value = serde_json::from_str(&line).expect("a captured frame");
                v["params"]["threadId"] = json!(thread);
                v.to_string()
            })
            .collect()
    }

    /// A second turn, invented because the capture has only one — with real ids, its own
    /// items, and a later `startedAt` so the answer is ordered oldest-first.
    const TURN_B: &str = "01a0127a-bbbb-7000-0000-00000000000b";
    const TURN_B_USER: &str = "01a0127a-bbbb-7000-0000-0000000000u1";
    const TURN_B_AGENT: &str = "msg_bbbb0000000000000000000000000000000000000000000000";

    fn turn_b(status: &str) -> Value {
        let running = status == "inProgress";
        json!({
            "id": TURN_B,
            // A running turn reports PLACEHOLDER ids — the measured behaviour this whole
            // follow-up exists because of.
            "items": if running {
                json!([{"type": "userMessage", "id": "item-1",
                        "content": [{"type": "text", "text": "second"}]}])
            } else {
                json!([
                    {"type": "userMessage", "id": TURN_B_USER,
                     "content": [{"type": "text", "text": "second"}]},
                    {"type": "agentMessage", "id": TURN_B_AGENT, "text": "two"},
                ])
            },
            "itemsView": "full",
            "status": status,
            "error": null,
            "startedAt": 1_787_617_900_i64,
            "completedAt": if running { Value::Null } else { json!(1_787_617_902_i64) },
            "durationMs": if running { Value::Null } else { json!(2_000) },
        })
    }

    /// The two-turn answer: turn A in whatever state, turn B in whatever state.
    fn two_turn_answer(id: i64, thread: &str, a_running: bool, b_running: bool) -> Value {
        let mut answer = if a_running {
            lifecycle_answer_in_progress(id, thread)
        } else {
            lifecycle_answer(id, thread, |_| {})
        };
        let turn_a = answer["result"]["thread"]["turns"][0].clone();
        answer["result"]["thread"]["turns"] = json!([
            turn_a,
            turn_b(if b_running { "inProgress" } else { "completed" }),
        ]);
        answer
    }

    /// A live `turn/completed` for one turn, as the wire sends it.
    fn turn_terminal(thread: &str, turn: &str) -> String {
        json!({
            "method": "turn/completed",
            "params": {"threadId": thread, "turn": {
                "id": turn, "items": [], "itemsView": "summary", "status": "completed",
                "error": null, "startedAt": 1_787_617_900_i64,
                "completedAt": 1_787_617_902_i64, "durationMs": 2_000,
            }}
        })
        .to_string()
    }

    /// The `thread/started` that binds a connection, for `thread`.
    ///
    /// **Carries `cliVersion`, because the real broadcast does.** This helper used to
    /// omit it, and that abbreviation stopped being free the moment the identity fact's
    /// immutable fields were validated rather than defaulted: a Thread object without it
    /// mints no `thread_started` at all. The fix is to make the stand-in faithful, not to
    /// let the validation tolerate a shape the wire never sends —
    /// `fixtures/codex/lifecycle.jsonl` and `first-turn.jsonl` both carry all three.
    fn announce(thread: &str) -> String {
        json!({
            "method": "thread/started",
            "params": {"thread": {"id": thread, "path": "/r/t.jsonl", "cwd": "/work",
                                  "cliVersion": "0.147.0", "turns": []}}
        })
        .to_string()
    }

    /// The committed populated answer, retargeted onto the thread and the turn the
    /// lifecycle capture is about.
    ///
    /// Built out of **both** committed fixtures rather than written by hand here. The
    /// envelope — every top-level key, the whole Thread object, the policy fields — is
    /// `fixtures/codex/resume-populated-answer.json` exactly as captured. The turn it
    /// describes is rebuilt from `fixtures/codex/lifecycle.jsonl`'s own frames: the turn
    /// object its `turn/completed` carried, carrying the items its `item/completed`s
    /// carried, under the `itemsView:"full"` a resume answer is measured to report.
    ///
    /// That arrangement is the point. The answer and the notification stream the leg
    /// replays beside it describe the same turn with the same ids, which is the only
    /// way dedup **across the attach boundary** can be exercised at all: a hand-written
    /// answer would mint facts the capture never produces, and every key would be
    /// unique for the boring reason.
    fn lifecycle_answer(id: i64, thread: &str, mutate: impl FnOnce(&mut Value)) -> Value {
        let parsed: Vec<Value> = lifecycle_frames()
            .iter()
            .map(|line| serde_json::from_str(line).expect("a captured frame is JSON"))
            .collect();
        let mut turn = parsed
            .iter()
            .find(|f| f["method"] == "turn/completed")
            .expect("the capture completes a turn")["params"]["turn"]
            .clone();
        // `turn/completed` carries `itemsView:"summary"` and a PARTIAL item list; a
        // resume answer carries the whole one. Measured, and the difference is exactly
        // what `RESUME_ITEMS_VIEW_FULL` guards.
        turn["items"] = Value::Array(
            parsed
                .iter()
                .filter(|f| f["method"] == "item/completed")
                .map(|f| f["params"]["item"].clone())
                .collect(),
        );
        turn["itemsView"] = json!("full");

        let mut answer: Value = populated_answer();
        answer["id"] = json!(id);
        let result = answer
            .get_mut("result")
            .expect("the captured answer has a result");
        result["thread"]["id"] = json!(thread);
        result["thread"]["sessionId"] = json!(thread);
        result["thread"]["turns"] = json!([turn]);
        mutate(result);
        answer
    }

    /// The lifecycle answer with its one turn still running — and therefore with the
    /// **placeholder** item ids a running turn is measured to report, in place of the
    /// real ones it reports once the turn finishes.
    fn lifecycle_answer_in_progress(id: i64, thread: &str) -> Value {
        lifecycle_answer(id, thread, |result| {
            let turn = &mut result["thread"]["turns"][0];
            turn["status"] = json!("inProgress");
            turn["completedAt"] = Value::Null;
            turn["durationMs"] = Value::Null;
            let placeholders: Vec<Value> = turn["items"]
                .as_array()
                .expect("items")
                .iter()
                .enumerate()
                .map(|(n, item)| {
                    let mut item = item.clone();
                    item["id"] = json!(format!("item-{}", n + 1));
                    item
                })
                .collect();
            turn["items"] = Value::Array(placeholders);
        })
    }

    /// The same answer, with its running turn wearing a turn id **this daemon has
    /// never filed a terminal for** — which is what "the agent went back to work
    /// while the link was down" actually looks like on the wire.
    ///
    /// [`lifecycle_answer_in_progress`] cannot express that on its own: it reuses the
    /// capture's one turn id, so an answer built from it after that turn has been
    /// recorded finished is a *stale snapshot* of a settled turn, not news. The two
    /// helpers are the two halves of the round-4 F5 distinction, and a test needs
    /// both to show the gate telling them apart.
    fn lifecycle_answer_novel_turn(id: i64, thread: &str, turn_id: &str) -> Value {
        let mut answer = lifecycle_answer_in_progress(id, thread);
        answer["result"]["thread"]["turns"][0]["id"] = json!(turn_id);
        answer
    }

    /// Make every `events` insert fail, and leave every read working.
    ///
    /// The asymmetry is the whole point of the probe: round-4 F6 is about a
    /// cancellation that used to be hostage to the recovery WRITES, and separating it
    /// from them can only be shown by a log that refuses to be written to while still
    /// answering [`crate::store::Store::turn_terminal_filed`]. Closing the database,
    /// or revoking the file, would break both halves and prove nothing.
    ///
    /// A `BEFORE INSERT ... RAISE(ABORT)` trigger is the narrowest thing that does
    /// it: it is schema, so it survives for the test's lifetime, it aborts the
    /// enclosing transaction exactly as a real write failure does, and `SELECT` never
    /// touches it.
    fn refuse_event_writes(db: &TempDb) {
        rusqlite::Connection::open(db.path())
            .expect("a second handle on the test database")
            .execute_batch(
                "CREATE TRIGGER refuse_events BEFORE INSERT ON events
                 BEGIN SELECT RAISE(ABORT, 'the log is refusing writes'); END;",
            )
            .expect("installing the refusal");
    }

    /// Is this the SECOND ask about the fallback thread? It counts as a side effect, which
    /// is why it is a named function rather than a block inside an `if` condition.
    fn is_second_fallback_ask(seen: &mut usize) -> bool {
        *seen += 1;
        *seen == 2
    }

    async fn serve_scripted(
        stream: tokio::net::UnixStream,
        nth: usize,
        answer: ResumeAnswer,
        announce_thread: bool,
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
        announced: AnnouncedOnce,
    ) -> Result<()> {
        let mut ws = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config())).await?;
        // Which scripts force a reconnect: only the one whose subject IS the reconnect
        // arc. Every other script holds its connection, so "no further resume" cannot be
        // a reconnect in disguise.
        let drops_after_retry = matches!(answer, ResumeAnswer::NoRollout) && announce_thread;
        // How many resumes THIS connection has answered. The "act one drops" scripts
        // close on the second, which is what makes a retry-on-the-same-connection
        // observable before the reconnect that follows it.
        let mut resumes_answered = 0usize;
        // Asks about a thread OTHER than the switch target. `resumes_answered` counts every
        // reply the shared tail sends — refusals included — so it cannot be used to say
        // "this is the second time the link asked about the fallback thread".
        let mut fallback_asks = 0usize;
        // Asks about the ORIGINAL thread, for the round-3 P7 script.
        let mut a_asks = 0usize;
        // Asks about the SWITCHED-TO thread. Separate from `a_asks` for the same reason
        // that one is separate from `resumes_answered`: the never-answered-recovery
        // script has to answer B's first ask and B's follow-up differently, and the two
        // are interleaved with asks about A.
        let mut b_asks = 0usize;
        while let Some(Ok(msg)) = ws.next().await {
            let Message::Text(text) = msg else { continue };
            let frame: Value = serde_json::from_str(&text)?;
            let method = frame
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let id = frame.get("id").and_then(Value::as_i64).unwrap_or(-1);
            // The app-server answers about the thread the request named; a leg that
            // always answered about one hard-coded thread could not stage a test whose
            // evidence has to be selectable by thread id.
            let asked_about = frame
                .pointer("/params/threadId")
                .and_then(Value::as_str)
                .unwrap_or(LIFECYCLE_THREAD)
                .to_string();
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
                    // **The announcement, and NOTHING ELSE.**
                    //
                    // This is the measured wire, and an earlier version of this leg got
                    // it wrong in a way that mattered: it replayed the whole captured
                    // turn stream to a connection that had merely handshaked. On the
                    // real wire that connection is handed the thread's identity, its
                    // status changes, and **zero** `turn/*` or `item/*` frames — turn
                    // frames go only to a connection whose `thread/resume` succeeded
                    // (2e-4a, and `codex_link_live`'s claim 5 counts it). A leg that
                    // handed them out for free let an announced-but-unsubscribed link
                    // look like it was observing, which is exactly the defect P1 fixes.
                    //
                    // **Act one only.** A reconnect to a thread that is already running
                    // gets no `thread/started` — the announcement happened once, on a
                    // connection that is gone. Re-broadcasting it would hand the second
                    // connection a binding the real wire does not give it.
                    let announces_late = matches!(answer, ResumeAnswer::AnnounceLate);
                    if announce_thread && nth == 0 && !announces_late {
                        ws.send(Message::Text(announce(LIFECYCLE_THREAD))).await?;
                    }
                    // P6c: the RECONNECT — which is carrying an ADOPTED target — is
                    // announced a DIFFERENT thread as its first announcement.
                    if matches!(
                        answer,
                        ResumeAnswer::ReconnectCarryingAdoptedThenAnnouncedAnother
                    ) && nth > 0
                    {
                        ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                    }
                }
                "thread/resume" => {
                    // **Act two onwards is deaf.** The connection is up and
                    // handshaken and this request will never be answered, so the link
                    // sits unbound-with-a-resume-in-flight until its budget expires.
                    if matches!(answer, ResumeAnswer::PopulatedThenDeaf) && nth > 0 {
                        continue;
                    }
                    let reply = match answer {
                        // A1/D3, verbatim off the live wire. The noisy-handshake
                        // script answers the same way: its subject is the handshake,
                        // and the reconnect arc behind it should behave normally.
                        ResumeAnswer::NoRollout | ResumeAnswer::NoisyHandshake => {
                            json!({"id": id, "error": {
                                "code": -32600,
                                "message":
                                    format!("no rollout found for thread id {asked_about}")
                            }})
                        }
                        // **Each of these is the REAL captured envelope with exactly
                        // one thing wrong.** An earlier version wrote them out by hand
                        // as bare `{"thread": {...}, "turns": [...]}` objects, and
                        // mutation testing showed what that cost: with the identity
                        // check or the empty-turns check deleted, they were still
                        // refused — for the incidental reason that a hand-written
                        // answer has no `thread.turns` to read at all. They named a
                        // defect they did not isolate. Built from the fixture, each one
                        // now fails for its own reason and for no other.
                        ResumeAnswer::EmptyTurns => lifecycle_answer(id, &asked_about, |result| {
                            result["thread"]["turns"] = json!([]);
                        }),
                        ResumeAnswer::TurnsNotUnderThread => {
                            lifecycle_answer(id, &asked_about, |result| {
                                let turns = result["thread"]["turns"].take();
                                result["turns"] = turns;
                            })
                        }
                        // The broker's own refusal, verbatim (`refusal.rs`).
                        ResumeAnswer::Refused => json!({"id": id, "error": {
                            "code": -32001,
                            "message": "resume refused: target thread is not bound to this session"
                        }}),
                        ResumeAnswer::NoTurnsArray => {
                            lifecycle_answer(id, &asked_about, |result| {
                                result["thread"]
                                    .as_object_mut()
                                    .expect("the captured thread is an object")
                                    .remove("turns");
                            })
                        }
                        ResumeAnswer::Populated
                        | ResumeAnswer::PopulatedNoReplay
                        | ResumeAnswer::PopulatedThenGone
                        | ResumeAnswer::PopulatedThenDeaf => {
                            lifecycle_answer(id, &asked_about, |_| {})
                        }
                        ResumeAnswer::PopulatedInProgress => {
                            lifecycle_answer_in_progress(id, &asked_about)
                        }
                        // First ask: still running. Follow-up: finished, real ids.
                        ResumeAnswer::MidTurnThenCompleted => {
                            if resumes_answered == 0 {
                                lifecycle_answer_in_progress(id, &asked_about)
                            } else {
                                lifecycle_answer(id, &asked_about, |_| {})
                            }
                        }
                        // Two turns running; A terminalizes, then B terminalizes while
                        // the follow-up for A is still outstanding.
                        ResumeAnswer::TwoMidTurnDebts => match resumes_answered {
                            // Both running. Then A finishes on the wire.
                            0 => {
                                let answer = two_turn_answer(id, &asked_about, true, true);
                                ws.send(Message::Text(answer.to_string())).await?;
                                resumes_answered += 1;
                                ws.send(Message::Text(turn_terminal(&asked_about, LIFECYCLE_TURN)))
                                    .await?;
                                continue;
                            }
                            // The follow-up for A. B's terminal is sent FIRST and the
                            // answer is held back, so B's debt is noticed while this very
                            // request is in flight — the moment a flag would have been
                            // consumed against a state that could not act on it.
                            1 => {
                                ws.send(Message::Text(turn_terminal(&asked_about, TURN_B)))
                                    .await?;
                                tokio::time::sleep(Duration::from_millis(300)).await;
                                let answer = two_turn_answer(id, &asked_about, false, true);
                                ws.send(Message::Text(answer.to_string())).await?;
                                resumes_answered += 1;
                                continue;
                            }
                            // The follow-up for B. Only now is B describable.
                            _ => two_turn_answer(id, &asked_about, false, false),
                        },
                        // A follow-up recovery answer racing a switch announcement.
                        ResumeAnswer::RecoveryCrossingASwitch => {
                            match resumes_answered {
                                // Attach MID-TURN: the turn is running, so its items carry
                                // placeholder ids and contribute nothing.
                                0 => {
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                    // Only the TAIL: the userMessage is withheld, standing
                                    // for what finished before the subscription existed.
                                    for line in capture_tail() {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                }
                                // The FOLLOW-UP. Announce the switch FIRST, so the
                                // announcement is in flight beside this answer, then answer
                                // with the turn finished and its REAL ids.
                                _ => {
                                    ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                    tokio::time::sleep(Duration::from_millis(150)).await;
                                    let answer = lifecycle_answer(id, &asked_about, |_| {});
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                }
                            }
                            continue;
                        }
                        // Announce the real thread, then drop before anything is adopted.
                        ResumeAnswer::AnnouncedThenDroppedBeforeAdopting => {
                            // Connection 0 has been ANNOUNCED the real thread at
                            // `initialized`, so it asks about it — and the leg drops
                            // WITHOUT answering. Nothing is adopted, and nothing will ever
                            // announce that thread again.
                            if nth == 0 {
                                tokio::time::sleep(Duration::from_millis(120)).await;
                                ws.close(None).await?;
                                return Ok(());
                            }
                            // On the reconnect the stale hint is refused outright, so the
                            // link can only get anywhere if the announcement replaced it in
                            // the slot a reconnect reads.
                            if asked_about == LIFECYCLE_THREAD {
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                resumes_answered += 1;
                                for line in capture_on(&asked_about) {
                                    ws.send(Message::Text(line)).await?;
                                }
                                continue;
                            }
                            json!({"id": id, "error": {
                                "code": -32001,
                                "message": "resume refused: target thread is not bound to this session"
                            }})
                        }
                        // Attach to A mid-turn; the follow-up is refused; B is announced
                        // and adopted; A's recovery must still be paid afterwards.
                        ResumeAnswer::SwitchWithACrossingRecoveryOwed => {
                            if asked_about == SWITCHED_THREAD {
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                resumes_answered += 1;
                                for line in capture_on(&asked_about) {
                                    ws.send(Message::Text(line)).await?;
                                }
                                continue;
                            }
                            a_asks += 1;
                            match a_asks {
                                // Attach MID-TURN: placeholder ids, tail only. The
                                // userMessage is withheld — that is what must be recovered.
                                1 => {
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                    for line in capture_tail() {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    ws.send(Message::Text(turn_terminal(
                                        &asked_about,
                                        LIFECYCLE_TURN,
                                    )))
                                    .await?;
                                    continue;
                                }
                                // The FOLLOW-UP. Announce the switch, then answer it with
                                // the measured not-ready error: the recovery never lands.
                                2 => {
                                    ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    resumes_answered += 1;
                                    json!({"id": id, "error": {
                                        "code": -32600,
                                        "message":
                                            format!("no rollout found for thread id {asked_about}")
                                    }})
                                }
                                // The ON-DEMAND recovery, fired after B was adopted. NOW
                                // the answer is complete, with A's real ids.
                                _ => {
                                    let answer = lifecycle_answer(id, &asked_about, |_| {});
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                    // **And then a fresh B frame.** The link must still be
                                    // on B: a recovery is a read of a thread it left, not
                                    // a return to it. If the recovery re-bound the visit,
                                    // the D4 filter drops this and the fact never lands.
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    ws.send(Message::Text(turn_terminal(SWITCHED_THREAD, TURN_B)))
                                        .await?;
                                    continue;
                                }
                            }
                        }
                        // The same arc, with the leg VANISHING as B's answer goes out — so
                        // the on-demand recovery it makes payable cannot be written.
                        ResumeAnswer::SwitchThenTheLegVanishesAsTheRecoveryIsWritten => {
                            if asked_about == SWITCHED_THREAD {
                                // **B's answer is the last thing this leg ever does.**
                                // Adopting B is what makes A's debt payable, so the link
                                // reaches for the wire on the very next pass — after
                                // parsing this answer, normalizing it and committing its
                                // facts to SQLite, all of which this drop beats by orders
                                // of magnitude. The socket is gone; the write fails.
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                return Ok(());
                            }
                            a_asks += 1;
                            match a_asks {
                                // Attach MID-TURN: placeholder ids, tail only. The
                                // userMessage is withheld — that is the debt.
                                1 => {
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                    for line in capture_tail() {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    ws.send(Message::Text(turn_terminal(
                                        &asked_about,
                                        LIFECYCLE_TURN,
                                    )))
                                    .await?;
                                    continue;
                                }
                                // The FOLLOW-UP. Announce the switch, then answer it
                                // not-ready: the recovery never lands, so the obligation
                                // crosses the switch and becomes owed on demand.
                                _ => {
                                    ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    resumes_answered += 1;
                                    json!({"id": id, "error": {
                                        "code": -32600,
                                        "message":
                                            format!("no rollout found for thread id {asked_about}")
                                    }})
                                }
                            }
                        }
                        // The same arc, with the on-demand recovery met by SILENCE — and B
                        // attached mid-turn, so the link owes a follow-up it can only fire
                        // once it has given that silence up.
                        ResumeAnswer::SwitchThenTheRecoveryIsNeverAnswered => {
                            if asked_about == SWITCHED_THREAD {
                                b_asks += 1;
                                resumes_answered += 1;
                                if b_asks == 1 {
                                    // **B is joined MID-TURN too**, and only the TAIL of its
                                    // turn is replayed: B's userMessage is withheld exactly
                                    // as A's was, and the running turn reports placeholder
                                    // ids. The one thing that can ever name it is a later
                                    // answer describing that turn FINISHED — which the link
                                    // can only ask for from `Attached`.
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    for line in capture_tail_on(&asked_about) {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                    continue;
                                }
                                // **The follow-up on B, and it is the whole point.** Reached
                                // only by a link that gave the unanswered recovery up: every
                                // site that issues a resume fires from `Attached`, so a link
                                // left holding an expired `Recovering` can never ask this,
                                // however many frames it goes on reading.
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                continue;
                            }
                            a_asks += 1;
                            match a_asks {
                                1 => {
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                    for line in capture_tail() {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    ws.send(Message::Text(turn_terminal(
                                        &asked_about,
                                        LIFECYCLE_TURN,
                                    )))
                                    .await?;
                                    continue;
                                }
                                2 => {
                                    ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    resumes_answered += 1;
                                    json!({"id": id, "error": {
                                        "code": -32600,
                                        "message":
                                            format!("no rollout found for thread id {asked_about}")
                                    }})
                                }
                                // **The ON-DEMAND recovery, and nothing comes back.** Not an
                                // error, not a refusal, not a close: silence, which is the
                                // one answer no arm can settle.
                                //
                                // B's turn terminalizes FIRST — while the recovery is still
                                // outstanding — and then the leg goes quiet for good. The
                                // frame that makes B's follow-up due therefore arrives
                                // against `Attach::Recovering`, which the bottom-of-loop
                                // scheduler declines for, and no later frame comes along to
                                // give the loop a second chance to notice. Sending it after
                                // the budget instead, as this leg used to, handed the link
                                // exactly that second chance and hid the defect.
                                _ => {
                                    ws.send(Message::Text(turn_terminal(
                                        SWITCHED_THREAD,
                                        LIFECYCLE_TURN,
                                    )))
                                    .await?;
                                    continue;
                                }
                            }
                        }
                        // The same arc, with the on-demand recovery ANSWERED and the leg
                        // silent afterwards — the answered exit, on its own.
                        ResumeAnswer::SwitchThenTheRecoveryIsAnsweredAndTheLegGoesQuiet => {
                            if asked_about == SWITCHED_THREAD {
                                b_asks += 1;
                                resumes_answered += 1;
                                if b_asks == 1 {
                                    // B is joined MID-TURN, its userMessage withheld and
                                    // its running turn reporting placeholder ids — exactly
                                    // as A was. Only an answer describing that turn
                                    // FINISHED can ever name the item.
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    for line in capture_tail_on(&asked_about) {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                    continue;
                                }
                                // **B's follow-up, and it is the whole point.** Reached
                                // only by a link that re-checked its debts as it LEFT
                                // `Recovering` by the answered door: the socket goes quiet
                                // after this, so there is no later pass to notice.
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                continue;
                            }
                            a_asks += 1;
                            match a_asks {
                                1 => {
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                    for line in capture_tail() {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    ws.send(Message::Text(turn_terminal(
                                        &asked_about,
                                        LIFECYCLE_TURN,
                                    )))
                                    .await?;
                                    continue;
                                }
                                2 => {
                                    ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    resumes_answered += 1;
                                    json!({"id": id, "error": {
                                        "code": -32600,
                                        "message":
                                            format!("no rollout found for thread id {asked_about}")
                                    }})
                                }
                                // **The ON-DEMAND recovery. B's terminal FIRST, then the
                                // answer, then silence.**
                                //
                                // The ordering is the test, for the reason the
                                // never-answered script writes down: a terminal delivered
                                // after the answer would be a second knock on the socket,
                                // and the bottom-of-loop scheduler would fire on it with
                                // the re-check deleted. Delivered during the recovery, the
                                // debt is recorded against `Recovering` and declined, and
                                // the answer below is the last frame this leg ever sends.
                                _ => {
                                    ws.send(Message::Text(turn_terminal(
                                        SWITCHED_THREAD,
                                        LIFECYCLE_TURN,
                                    )))
                                    .await?;
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    resumes_answered += 1;
                                    let answer = lifecycle_answer(id, &asked_about, |_| {});
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    continue;
                                }
                            }
                        }
                        // The same arc, with a SECOND switch announced while the on-demand
                        // recovery is outstanding.
                        ResumeAnswer::SwitchThenAThirdIsAnnouncedDuringTheRecovery => {
                            if asked_about == THIRD_THREAD || asked_about == SWITCHED_THREAD {
                                resumes_answered += 1;
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                continue;
                            }
                            a_asks += 1;
                            match a_asks {
                                1 => {
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                    for line in capture_tail() {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    ws.send(Message::Text(turn_terminal(
                                        &asked_about,
                                        LIFECYCLE_TURN,
                                    )))
                                    .await?;
                                    continue;
                                }
                                2 => {
                                    ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    resumes_answered += 1;
                                    json!({"id": id, "error": {
                                        "code": -32600,
                                        "message":
                                            format!("no rollout found for thread id {asked_about}")
                                    }})
                                }
                                // **The ON-DEMAND recovery, and C is announced while it is
                                // outstanding.** The answer carries A's REAL item ids and
                                // is sent afterwards, so a link that let the announcement
                                // re-target the attach has thrown away the only copy of
                                // A's pre-subscription items — readable in the log as the
                                // absence of one key.
                                _ => {
                                    ws.send(Message::Text(announce(THIRD_THREAD))).await?;
                                    tokio::time::sleep(Duration::from_millis(120)).await;
                                    resumes_answered += 1;
                                    let answer = lifecycle_answer(id, &asked_about, |_| {});
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    continue;
                                }
                            }
                        }
                        // The same arc, with the leg vanishing after the recovery is asked.
                        ResumeAnswer::SwitchThenTheLegVanishesAfterTheRecoveryIsAsked => {
                            if asked_about == SWITCHED_THREAD {
                                resumes_answered += 1;
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                continue;
                            }
                            a_asks += 1;
                            // **Connection 2 answers A properly**, which is the whole
                            // observable: it is reached only by a link that still had a
                            // name for A after the first connection died mid-recovery.
                            if nth > 0 {
                                resumes_answered += 1;
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                continue;
                            }
                            match a_asks {
                                1 => {
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                    for line in capture_tail() {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    ws.send(Message::Text(turn_terminal(
                                        &asked_about,
                                        LIFECYCLE_TURN,
                                    )))
                                    .await?;
                                    continue;
                                }
                                2 => {
                                    ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    resumes_answered += 1;
                                    json!({"id": id, "error": {
                                        "code": -32600,
                                        "message":
                                            format!("no rollout found for thread id {asked_about}")
                                    }})
                                }
                                // **The ON-DEMAND recovery is received and the socket
                                // closes.** The request was written — `send_resume`
                                // returned — so nothing about the ASK failed. What has not
                                // happened is any part of the recovery.
                                _ => {
                                    ws.close(None).await?;
                                    return Ok(());
                                }
                            }
                        }
                        // The same arc, with the recovery ANSWERED — and the answer
                        // reporting A's turn still running.
                        ResumeAnswer::SwitchThenTheRecoveryAnswersARunningTurn => {
                            if asked_about == SWITCHED_THREAD {
                                resumes_answered += 1;
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                continue;
                            }
                            a_asks += 1;
                            // **Connection 2 answers A with the turn FINISHED**, which is
                            // the only frame in this whole script that can name A's
                            // withheld item — and it is reached only by a link that still
                            // had a name for A after an answer that settled nothing.
                            if nth > 0 {
                                resumes_answered += 1;
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                continue;
                            }
                            match a_asks {
                                1 => {
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                    for line in capture_tail() {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    ws.send(Message::Text(turn_terminal(
                                        &asked_about,
                                        LIFECYCLE_TURN,
                                    )))
                                    .await?;
                                    continue;
                                }
                                2 => {
                                    ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    resumes_answered += 1;
                                    json!({"id": id, "error": {
                                        "code": -32600,
                                        "message":
                                            format!("no rollout found for thread id {asked_about}")
                                    }})
                                }
                                // **The ON-DEMAND recovery, answered with the turn still
                                // RUNNING.** Nothing failed: the request was written, the
                                // answer is readable, and every fact it describes is
                                // filed. What it describes is a turn whose items wear
                                // placeholder ids, so the facts that are owed are not
                                // among them. The socket then closes, so this connection
                                // can do nothing further about it.
                                _ => {
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    tokio::time::sleep(Duration::from_millis(120)).await;
                                    ws.close(None).await?;
                                    return Ok(());
                                }
                            }
                        }
                        // Adopt A, announce B, then drop before the link can act.
                        ResumeAnswer::CandidateAnnouncedThenTheLegDrops => {
                            if nth == 0 && resumes_answered == 0 {
                                // Attach MID-TURN so the link owes itself a follow-up. That
                                // follow-up is what makes a resume OUTSTANDING when the
                                // announcement lands — which is the window where a
                                // candidate is held UN-APPLIED and would die with the
                                // connection.
                                let answer = lifecycle_answer_in_progress(id, &asked_about);
                                ws.send(Message::Text(answer.to_string())).await?;
                                resumes_answered += 1;
                                for line in capture_tail() {
                                    ws.send(Message::Text(line)).await?;
                                }
                                // Terminalize it: the link now fires its follow-up.
                                tokio::time::sleep(Duration::from_millis(80)).await;
                                ws.send(Message::Text(turn_terminal(&asked_about, LIFECYCLE_TURN)))
                                    .await?;
                                continue;
                            }
                            if nth == 0 {
                                // The follow-up is OUTSTANDING. Announce, and drop before
                                // answering it — so the candidate can never be applied on
                                // this connection.
                                ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                tokio::time::sleep(Duration::from_millis(120)).await;
                                ws.close(None).await?;
                                return Ok(());
                            }
                            if asked_about == SWITCHED_THREAD {
                                // **B is never adopted.** The reconnect therefore needs a
                                // FALLBACK — and the only place it could have been recorded
                                // is when the candidate was NOTED, because the apply block
                                // never ran on the connection that heard the announcement.
                                json!({"id": id, "error": {
                                    "code": -32001,
                                    "message":
                                        "resume refused: target thread is not bound to this session"
                                }})
                            } else {
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                resumes_answered += 1;
                                for line in capture_on(&asked_about) {
                                    ws.send(Message::Text(line)).await?;
                                }
                                continue;
                            }
                        }
                        // Adopt A and drop; then announce B to the RECONNECT and refuse it.
                        ResumeAnswer::ReconnectCarryingAdoptedThenAnnouncedAnother => {
                            if asked_about == SWITCHED_THREAD {
                                json!({"id": id, "error": {
                                    "code": -32001,
                                    "message":
                                        "resume refused: target thread is not bound to this session"
                                }})
                            } else {
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                resumes_answered += 1;
                                for line in capture_on(&asked_about) {
                                    ws.send(Message::Text(line)).await?;
                                }
                                // Connection 0 adopts A and drops. Connection 1 is
                                // announced B, chases it, falls back to A — and then drops
                                // too, so connection 2's FIRST ask reveals what was
                                // PUBLISHED across all of it (round-2 P6a).
                                if nth <= 1 {
                                    tokio::time::sleep(Duration::from_millis(120)).await;
                                    ws.close(None).await?;
                                    return Ok(());
                                }
                                continue;
                            }
                        }
                        // Two announcements while the follow-up is in flight.
                        ResumeAnswer::TwoSwitchesDuringRecovery => {
                            match resumes_answered {
                                0 => {
                                    let answer = lifecycle_answer_in_progress(id, &asked_about);
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                    for line in capture_tail() {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                }
                                // The follow-up is now outstanding: announce B, then C,
                                // and only then answer it.
                                1 => {
                                    ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    ws.send(Message::Text(announce(THIRD_THREAD))).await?;
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    let answer = lifecycle_answer(id, &asked_about, |_| {});
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                }
                                _ => {
                                    let answer = lifecycle_answer(id, &asked_about, |_| {});
                                    ws.send(Message::Text(answer.to_string())).await?;
                                    resumes_answered += 1;
                                    for line in capture_on(&asked_about) {
                                        ws.send(Message::Text(line)).await?;
                                    }
                                }
                            }
                            continue;
                        }
                        // An unreadable answer arriving beside a switch announcement.
                        ResumeAnswer::UnreadableAnswerBesideASwitch => {
                            if asked_about == SWITCHED_THREAD {
                                // On the reconnect the carried candidate is asked about and
                                // answered properly, which is how its survival is observed.
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                resumes_answered += 1;
                                for line in capture_on(&asked_about) {
                                    ws.send(Message::Text(line)).await?;
                                }
                                continue;
                            }
                            // First connection: announce the switch, then answer the
                            // outstanding resume with a shape this build refuses.
                            ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                            tokio::time::sleep(Duration::from_millis(80)).await;
                            resumes_answered += 1;
                            lifecycle_answer(id, &asked_about, |result| {
                                result["thread"]["turns"][0]["status"] = json!("interrupted");
                            })
                        }
                        // The switch, then B is refused for ever; A stays answerable.
                        ResumeAnswer::SwitchNeverAdoptedThenFallback => {
                            if asked_about == SWITCHED_THREAD {
                                // B is never adopted. Not once.
                                json!({"id": id, "error": {
                                    "code": -32001,
                                    "message":
                                        "resume refused: target thread is not bound to this session"
                                }})
                            } else if is_second_fallback_ask(&mut fallback_asks) {
                                // **The first FALLBACK ask gets the measured not-ready
                                // error**, so the fallback needs a SECOND ask to complete.
                                // That second ask is what reads `resume_target` again — and
                                // therefore what exposes a redirect that quietly rewrote it
                                // back to B while the fallback was in flight.
                                json!({"id": id, "error": {
                                    "code": -32600,
                                    "message":
                                        format!("no rollout found for thread id {asked_about}")
                                }})
                            } else {
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                resumes_answered += 1;
                                for line in capture_on(&asked_about) {
                                    ws.send(Message::Text(line)).await?;
                                }
                                if resumes_answered == 1 {
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                    ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                }
                                continue;
                            }
                        }
                        // The switch, then a few not-yet-adopted refusals of the new
                        // thread, then a proper answer.
                        ResumeAnswer::SwitchThenNotAdoptedYet => {
                            if resumes_answered == 0 {
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                resumes_answered += 1;
                                for line in capture_on(&asked_about) {
                                    ws.send(Message::Text(line)).await?;
                                }
                                tokio::time::sleep(Duration::from_millis(100)).await;
                                ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                                continue;
                            }
                            // The next three resumes of the NEW thread are refused as
                            // not-yet-adopted; the fourth is answered.
                            if resumes_answered <= 3 {
                                resumes_answered += 1;
                                json!({"id": id, "error": {
                                    "code": -32001,
                                    "message":
                                        "resume refused: target thread is not bound to this session"
                                }})
                            } else {
                                resumes_answered += 1;
                                let answer = lifecycle_answer(id, &asked_about, |_| {});
                                ws.send(Message::Text(answer.to_string())).await?;
                                for line in capture_on(&asked_about) {
                                    ws.send(Message::Text(line)).await?;
                                }
                                continue;
                            }
                        }
                        // The `/new` switch, all on ONE connection.
                        ResumeAnswer::SwitchToSecondThread => {
                            let answer = lifecycle_answer(id, &asked_about, |_| {});
                            ws.send(Message::Text(answer.to_string())).await?;
                            resumes_answered += 1;
                            // Subscribe: replay the turn stream for whichever thread was
                            // just resumed, so BOTH visits produce live facts.
                            for line in capture_on(&asked_about) {
                                ws.send(Message::Text(line)).await?;
                            }
                            if resumes_answered == 1 {
                                // ...and then the operator presses `/new`. The broadcast
                                // reaches this connection whatever it is subscribed to.
                                tokio::time::sleep(Duration::from_millis(100)).await;
                                ws.send(Message::Text(announce(SWITCHED_THREAD))).await?;
                            }
                            continue;
                        }
                        // Announce first, THEN answer the now-superseded request.
                        ResumeAnswer::AnnounceThenAnswerStale => {
                            if resumes_answered == 0 {
                                ws.send(Message::Text(announce(LIFECYCLE_THREAD))).await?;
                                lifecycle_answer(id, &asked_about, |_| {})
                            } else {
                                json!({"id": id, "error": {
                                    "code": -32600,
                                    "message":
                                        format!("no rollout found for thread id {asked_about}")
                                }})
                            }
                        }
                        ResumeAnswer::UnmeasuredTurnStatus => {
                            lifecycle_answer(id, &asked_about, |result| {
                                result["thread"]["turns"][0]["status"] = json!("interrupted");
                            })
                        }
                        ResumeAnswer::PartialItemsView => {
                            lifecycle_answer(id, &asked_about, |result| {
                                result["thread"]["turns"][0]["itemsView"] = json!("summary");
                            })
                        }
                        ResumeAnswer::ItemWithoutType => {
                            let mut answer = lifecycle_answer_in_progress(id, &asked_about);
                            answer["result"]["thread"]["turns"][0]["items"][0]
                                .as_object_mut()
                                .expect("an item is an object")
                                .remove("type");
                            answer
                        }
                        // A whole, well-formed, readable answer — about somebody
                        // else's thread. Nothing but the identity check stands between
                        // this and another session's timeline under our uid.
                        ResumeAnswer::WrongThread => {
                            lifecycle_answer(id, "th_SOME_OTHER_THREAD", |_| {})
                        }
                        // The same, with a second field naming a third thread.
                        ResumeAnswer::ConflictingIdentity => {
                            lifecycle_answer(id, &asked_about, |result| {
                                result["threadId"] = json!("a-different-thread");
                            })
                        }
                        // **The first resume is never answered; the thread is
                        // announced instead.** The announcement redirects the target —
                        // and, since 2e-4b, settles nothing: the outstanding attach for
                        // the stale target times out, the connection ends, and the
                        // reconnect asks again under the ANNOUNCED thread, which is what
                        // makes the redirect observable. Later resumes answer not-ready.
                        ResumeAnswer::AnnounceLate => {
                            // Announce once for the whole leg, and never answer THAT
                            // resume. Every later one — on this connection or on the
                            // reconnect the timeout forces — answers not-ready, which is
                            // the branch that proves the redirect took.
                            if !announced.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                resumes_answered += 1;
                                ws.send(Message::Text(announce(LIFECYCLE_THREAD))).await?;
                                continue;
                            }
                            json!({"id": id, "error": {
                                "code": -32600,
                                "message":
                                    format!("no rollout found for thread id {asked_about}")
                            }})
                        }
                    };
                    ws.send(Message::Text(reply.to_string())).await?;
                    resumes_answered += 1;

                    // The answer is on the wire and the app-server is gone. Returning
                    // here closes the socket; the listener has already unlinked the
                    // path, so the link adopts what it was just told and then has
                    // nothing left to dial.
                    if matches!(answer, ResumeAnswer::PopulatedThenGone) {
                        return Ok(());
                    }
                    // The link has adopted the thread. Held open just long enough
                    // for it to reach its idle moment and publish that — the
                    // transition this leg is about starts from `Subscribed`, and a
                    // socket closed in the same breath as the answer would skip it.
                    // Then act one ends, and the reconnect is met with silence.
                    if matches!(answer, ResumeAnswer::PopulatedThenDeaf) {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        return Ok(());
                    }

                    // **Turn frames follow only an answer that SUBSCRIBES.** A1: they
                    // follow the response and have no end marker, and they are on the
                    // wire while the link is still writing its seed — which is the
                    // ordering the admit filter has to survive. Every one of them
                    // describes the very turn the answer just described, so each must
                    // cost a row that is never written; that is the dedup claim across
                    // the attach boundary. `PopulatedNoReplay` withholds them so a test
                    // can see what the ANSWER alone recovers.
                    let subscribes = matches!(
                        answer,
                        ResumeAnswer::Populated | ResumeAnswer::PopulatedInProgress
                    );
                    if subscribes {
                        for line in capture() {
                            ws.send(Message::Text(line)).await?;
                        }
                    }
                    // **A duplicate terminal for a turn whose debt is already settled.**
                    // The wire replays after a resume (A1, no end marker), so a terminal
                    // this link has already acted on can and does come round again. It
                    // must buy nothing: the turn has had its one ask.
                    if matches!(answer, ResumeAnswer::TwoMidTurnDebts) && resumes_answered == 3 {
                        ws.send(Message::Text(turn_terminal(&asked_about, TURN_B)))
                            .await?;
                    }
                    // The mid-turn script replays only the TAIL, and only once: the
                    // withheld userMessage is what the follow-up has to recover, and a
                    // second replay would hand it over for free.
                    if matches!(answer, ResumeAnswer::MidTurnThenCompleted) && resumes_answered == 1
                    {
                        for line in capture_tail() {
                            ws.send(Message::Text(line)).await?;
                        }
                    }
                    // Act one drops after answering a SECOND resume, so a test sees the
                    // retry on one connection and then the reconnect. Only the scripts
                    // that need a reconnect arc do this; the rest hold the connection so
                    // "no further resume" cannot be a reconnect in disguise.
                    if nth == 0 && drops_after_retry && resumes_answered >= 2 {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        ws.close(None).await?;
                        return Ok(());
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
        let (daemon, db, _) = linked_daemon_watching_pushes(session);
        (daemon, db)
    }

    /// The same daemon, keeping the push sender so a test can see what it rang
    /// about. [`crate::apns::LoggingPushSender`] records the last hint it was
    /// handed, which is exactly the probe a doorbell test needs and is already the
    /// sender these tests run with.
    fn linked_daemon_watching_pushes(
        session: &SessionKey,
    ) -> (Arc<Daemon>, TempDb, Arc<crate::apns::LoggingPushSender>) {
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
        let push = Arc::new(crate::apns::LoggingPushSender::new());
        let daemon = Daemon::new(
            protocol::config::Config::default(),
            store,
            Arc::clone(&push) as Arc<dyn crate::apns::PushSender>,
            crate::state::Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
            tail_tx,
        );
        (daemon, db, push)
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
        let (events, connections, resumes, frames, _, _) =
            drive_watching_presence(answer, hint, announce_thread, settle).await;
        (events, connections, resumes, frames)
    }

    /// **The scripted-leg serializer.** See the note inside
    /// [`drive_watching_presence`]: every leg holds sockets, tasks and a SQLite file
    /// for its whole settle budget, and a dozen at once lands as load on the rest of
    /// the workspace rather than on this module.
    static ONE_LEG_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// The same drive, keeping **every distinct state the link published**, in order.
    ///
    /// Split out rather than folded into every caller's tuple: what a link
    /// *publishes* is one property, and the dozen tests about what it *records* have
    /// no business restating it.
    ///
    /// **A sequence and not a final value.** The states worth asserting are ones the
    /// link passes *through* — a reconnect's `Unbound` lasts one resume budget, a
    /// chase's `Bound` lasts an adoption budget — so sampling only where the settle
    /// budget happens to expire would test a coin flip. Distinct-in-order because the
    /// claim is about the sequence; a hundred consecutive `Offline`s say nothing one
    /// does not. `last()` is the final state, for the tests that want only that.
    async fn drive_watching_presence(
        answer: ResumeAnswer,
        hint: Option<&str>,
        announce_thread: bool,
        settle: Duration,
    ) -> (
        Vec<protocol::event::Event>,
        usize,
        Vec<Value>,
        Vec<Value>,
        Vec<CodexAddressee>,
        Option<crate::apns::PushHint>,
    ) {
        // **One scripted leg at a time, process-wide.**
        //
        // Each of these stands up a real unix-socket server and a real link that reconnects
        // against it for the whole of its settle budget, holding sockets, tasks and a SQLite
        // file open throughout. Run concurrently — which is what `cargo test` does by
        // default — a dozen of them are a genuine resource load, and it lands on the rest of
        // the workspace rather than on itself: the `apns`/`relay_sender` suites read the
        // system trust store, and under that load the read intermittently fails with "no
        // trust anchors available", failing tests that have nothing to do with any of this.
        //
        // Observed directly: `cargo test --workspace` failed with 29 unrelated failures on
        // one run and passed clean on the next, while the same suites were green on a tree
        // without these tests. Serializing caps the peak at one leg, which costs this
        // module wall-clock time it already spends waiting anyway.
        let _serialized = ONE_LEG_AT_A_TIME.lock().await;

        let leg = ScriptedLeg::start(answer, announce_thread);
        let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-1");
        let (daemon, _db, push) = linked_daemon_watching_pushes(&session);
        let uid = session.uid.clone();
        let presence = LinkPresence::new();
        let task = tokio::spawn(run(
            Arc::clone(&daemon),
            session.clone(),
            ControlLink {
                socket: leg.path.clone(),
                generation: 1,
                thread_id: hint.map(str::to_string),
            },
            presence.clone(),
            LinkCarry::new(),
        ));
        // Sampled rather than slept through: see the note on this function.
        let mut published: Vec<CodexAddressee> = Vec::new();
        let until = Instant::now() + settle;
        while Instant::now() < until {
            let now = presence.get();
            if published.last() != Some(&now) {
                published.push(now);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let out = (
            daemon.store.events_after(&uid, 0, 1000).unwrap(),
            leg.connections.load(std::sync::atomic::Ordering::SeqCst),
            leg.requests("thread/resume"),
            leg.seen.lock().unwrap().clone(),
            published,
        );
        task.abort();
        let _ = task.await;
        // **The leg, the link and the serialization guard are all released first,
        // and the doorbell is read after.**
        //
        // `dispatch_push` waits out a grace before it rings, so a probe taken at
        // `settle` would report silence for a push that was merely still in flight
        // and every live-only claim would pass for the wrong reason. The wait has
        // to happen — but it must not happen while this leg is still holding
        // sockets, tasks and a SQLite file, because that window is the load the
        // guard above exists to cap, and lengthening it makes an unrelated
        // trust-store flake elsewhere in the workspace more likely rather than
        // less. Nothing here needs the leg: the push was dispatched onto its own
        // task before the link was aborted, and it reaches the sender either way.
        drop(leg);
        drop(_serialized);
        tokio::time::sleep(Duration::from_millis(700)).await;
        let (events, connections, resumes, frames, presence) = out;
        (events, connections, resumes, frames, presence, push.last())
    }

    /// **The same drive, keeping the CARRY — and never joining the link task.**
    ///
    /// Both halves are what separate this from [`drive_watching_presence`], and neither is
    /// that function's business.
    ///
    /// The **carry** is the observable for a debt whose write failed. `Carried::owed_recovery`
    /// is projected nowhere a reader can see — deliberately, it names a thread nobody is on —
    /// so a test about what survives a failed `send_resume` has to look at the slot itself.
    ///
    /// The **join is bounded**, and that is not tidiness either. Joining at all is what keeps
    /// an aborted link from overlapping the next leg's budget, so it is worth doing — but the
    /// failure these two tests report is a link stuck on a deadline no arm consumes, and a
    /// task that stops reaching an await point never observes an abort. Measured, the round-3
    /// P7 spin does keep yielding, so the join returns at once; the bound is there so that if
    /// it ever does not, the test says what went wrong instead of hanging while it tidies up.
    ///
    /// Returns `(events recorded, connections accepted, the thread each resume asked
    /// about, what the link carried at the end)`.
    async fn drive_holding_the_carry(
        answer: ResumeAnswer,
        settle: Duration,
    ) -> (Vec<protocol::event::Event>, usize, Vec<String>, Carried) {
        // One scripted leg at a time, process-wide — see [`drive_watching_presence`].
        let _serialized = ONE_LEG_AT_A_TIME.lock().await;
        let leg = ScriptedLeg::start(answer, true);
        let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-1");
        let (daemon, _db) = linked_daemon(&session);
        let uid = session.uid.clone();
        let carry = LinkCarry::new();
        let task = tokio::spawn(run(
            Arc::clone(&daemon),
            session.clone(),
            ControlLink {
                socket: leg.path.clone(),
                generation: 1,
                thread_id: None,
            },
            LinkPresence::new(),
            carry.clone(),
        ));
        tokio::time::sleep(settle).await;
        let out = (
            daemon.store.events_after(&uid, 0, 1000).unwrap(),
            leg.connections.load(std::sync::atomic::Ordering::SeqCst),
            leg.requests("thread/resume")
                .iter()
                .filter_map(|r| r["params"]["threadId"].as_str().map(str::to_string))
                .collect(),
            carry.snapshot(),
        );
        task.abort();
        let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
        out
    }

    /// **A RECOVERY WHOSE WRITE FAILS LEAVES THE THREAD STILL NAMED** (round-3 P7).
    ///
    /// The debt used to be cleared from the carry at the send site but **ahead of the
    /// send**. The site was right — a debt discharged where it was noticed is a debt lost,
    /// because the noticing state cannot issue a request — and the ordering was wrong:
    /// `send_resume` is itself an await, so a write error, or a registration parking the
    /// task inside it, discharged the debt against a request that had never been written.
    ///
    /// What that costs is total. `Carried::fallback` is cleared by the very adoption that
    /// makes this debt payable, and `pending_candidate` with it, so `owed_recovery` is the
    /// only slot left holding A's name. Emptied by a send that did not happen, the
    /// successor connection has no name for A at all — nothing will ever announce it
    /// again, and the items that finished before this link subscribed to A are reachable
    /// from nowhere.
    ///
    /// **How the write is made to fail.** This harness cannot inject an error into the
    /// link's own socket writes, so the far end is dropped at the moment the recovery is
    /// issued instead: the leg sends B's answer — the thing that makes the debt payable —
    /// and vanishes in the same breath, so `send_resume` finds no peer. That is the same
    /// failure by the route the wire actually produces it. The absent fourth resume is the
    /// evidence it really did fail: the leg never saw the recovery go by.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recovery_whose_write_fails_leaves_the_thread_it_owes_still_named_in_the_carry() {
        let (_, connections, targets, carried) = drive_holding_the_carry(
            ResumeAnswer::SwitchThenTheLegVanishesAsTheRecoveryIsWritten,
            Duration::from_millis(1500),
        )
        .await;

        // The arc got where it had to get to: A attached mid-turn, the follow-up refused,
        // B announced and ADOPTED — which is the moment the debt becomes payable.
        assert_eq!(
            carried.adopted.as_deref(),
            Some(SWITCHED_THREAD),
            "the switch must have been followed and B adopted, or nothing here is about a \
             payable debt at all: {targets:?}"
        );
        // The recovery never reached the wire: the leg saw A, A's follow-up, and B, and
        // then it was gone.
        assert_eq!(
            targets,
            vec![
                LIFECYCLE_THREAD.to_string(),
                LIFECYCLE_THREAD.to_string(),
                SWITCHED_THREAD.to_string()
            ],
            "the leg vanishes with B's answer, so the recovery resume must never have \
             been written: {targets:?}"
        );
        // And nothing could come along afterwards and pay it: the socket is unlinked, so
        // every later dial fails outright.
        assert_eq!(
            connections, 1,
            "the app-server does not come back, so what the carry holds is what the dying \
             connection left in it"
        );

        // **The claim.** The debt is still named, so the successor has something to ask
        // about. Cleared here, A would be unreachable for the rest of the session.
        assert_eq!(
            carried.owed_recovery.as_deref(),
            Some(LIFECYCLE_THREAD),
            "a recovery whose write FAILED recovered nothing, so the obligation is still \
             owed — and this slot is the only surviving name for the thread it is owed on"
        );
    }

    /// **AN UNANSWERED RECOVERY IS GIVEN UP, AND THE LINK GOES ON OBSERVING** (round-3 P7).
    ///
    /// `Attach::Recovering` contributed its deadline to the connection's `timeout_at` and no
    /// arm anywhere consumed an elapsed one. The timeout returned `Err`, the loop `continue`d
    /// to the top, found the same expired deadline and returned `Err` again — a hot loop for
    /// the rest of the connection's life. Only an answer could break it, and the case the
    /// state exists for is precisely the one where the answer does not come.
    ///
    /// So the leg answers the ordinary resumes and meets the on-demand recovery with
    /// **silence**. The claim is OBSERVABLE PROGRESS rather than a log line, and it is
    /// chosen to be progress the spin genuinely cannot make: reading is not it — measured
    /// directly, `tokio::time::timeout_at` polls the socket before it checks its elapsed
    /// deadline, so a spinning link goes on recording frames and a test that asserted only
    /// that would pass with the give-up deleted. What the spin cannot do is **ask**. Every
    /// site that issues a resume fires from `Attached`, so a link left holding an expired
    /// `Recovering` never sends another request as long as the connection lives.
    ///
    /// So B is joined mid-turn as well, with its userMessage withheld, and B's turn
    /// terminalizes **while the recovery is still outstanding**; after that the leg is
    /// silent for good. Only a link that re-checks what it owes at the moment it leaves
    /// `Recovering` asks B's follow-up, and only that answer lands the item.
    ///
    /// **The ordering is the test** (round-7 F2). Sending B's terminal after the budget
    /// had elapsed, as this leg first did, meant the frame that made the follow-up due was
    /// also a frame that ran the loop to its bottom-of-loop scheduler — which by then saw
    /// `Attached` and fired. That passed with the re-check deleted, because the socket was
    /// obliging enough to knock twice. Delivered during the recovery, the debt is recorded
    /// against `Recovering`, the scheduler declines, and the only remaining chance to ask
    /// is the state change itself: on a quiet socket there is no later pass at all.
    ///
    /// And giving up is not a `bail!`: the recovery has cost the session the
    /// pre-subscription items of a thread nobody is on, which is not a reason to tear down
    /// the connection observing the thread the user is using. So the connection count stays
    /// at one, and A's withheld item stays missing — the second of which also pins that the
    /// progress below came from B's follow-up and not from a recovery that quietly landed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recovery_that_is_never_answered_is_given_up_and_the_link_keeps_observing() {
        let (events, connections, targets, _) = drive_holding_the_carry(
            ResumeAnswer::SwitchThenTheRecoveryIsNeverAnswered,
            Duration::from_millis(3000),
        )
        .await;

        // **The claim, in what the link ASKED.** A, A's follow-up, B, the on-demand
        // recovery for A that is never answered — and then, only if that recovery was
        // given up, B's own follow-up.
        assert_eq!(
            targets,
            vec![
                LIFECYCLE_THREAD.to_string(),
                LIFECYCLE_THREAD.to_string(),
                SWITCHED_THREAD.to_string(),
                LIFECYCLE_THREAD.to_string(),
                SWITCHED_THREAD.to_string()
            ],
            "an unanswered recovery must be given up AND what it left owed re-checked as \
             it goes: every resume fires from `Attached`, so a link still holding the \
             expired deadline can never ask this fifth question — and one that reaches \
             `Attached` without looking at its debts blocks on a socket that will not \
             speak again, which cannot ask it either: {targets:?}"
        );

        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        // **The claim, in what the link RECORDED.** B's userMessage finished before this
        // link subscribed to B and the running turn reported placeholder ids, so the only
        // thing that can ever name it is the follow-up above.
        assert!(
            ids.contains(&format!("{SWITCHED_THREAD}:item:{LIFECYCLE_USER_ITEM}")),
            "the link must go on observing the thread it IS on after an unanswered \
             recovery expires — including recovering what that thread still owes it: \
             {ids:?}"
        );
        // Given up, not bailed: the connection observing B is not the thing at fault.
        assert_eq!(
            connections, 1,
            "an unanswered recovery costs a retired thread's history, never the \
             connection watching the thread the session is on"
        );
        // And it really did go unanswered: A's withheld item is still missing, so the
        // progress above is B's follow-up and not a recovery that landed after all.
        assert!(
            !ids.contains(&format!("{LIFECYCLE_THREAD}:item:{LIFECYCLE_USER_ITEM}")),
            "nothing answered the recovery, so A's pre-subscription item must still be \
             the legible gap the warning describes: {ids:?}"
        );
    }

    /// **THE ANSWERED EXIT FROM `Recovering` RE-CHECKS WHAT IS OWED — ON ITS OWN**
    /// (round-8 F5).
    ///
    /// `leaving_recovery` is one rule applied at two exits, and until this test only the
    /// TIMEOUT exit was pinned: the never-answered script reaches
    /// [`leaving_recovery`] through the expired deadline, so replacing the ANSWERED
    /// arm's call with a bare `Attach::Attached` left it green. That is a masking
    /// residual the round-7 report stated rather than hid, and this closes it.
    ///
    /// The arc is the same up to the on-demand ask about A, and then: B's turn
    /// terminalizes **while the recovery is outstanding**, so B's follow-up debt is
    /// recorded against `Recovering` and the bottom-of-loop scheduler declines it; the
    /// recovery is then ANSWERED, readably; and the leg says nothing further.
    ///
    /// The answered arm `continue`s past that scheduler onto a deadline-free `ws.next()`.
    /// A quiet socket produces no later pass, so the only remaining chance to ask for
    /// B's follow-up is the state change itself — which is what this asserts, in the one
    /// thing a link that never asks cannot produce: B's withheld userMessage.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_answered_recovery_exit_asks_for_what_became_owed_during_it() {
        let (events, connections, targets, _) = drive_holding_the_carry(
            ResumeAnswer::SwitchThenTheRecoveryIsAnsweredAndTheLegGoesQuiet,
            Duration::from_millis(2500),
        )
        .await;

        assert_eq!(
            targets,
            vec![
                LIFECYCLE_THREAD.to_string(),
                LIFECYCLE_THREAD.to_string(),
                SWITCHED_THREAD.to_string(),
                LIFECYCLE_THREAD.to_string(),
                SWITCHED_THREAD.to_string()
            ],
            "the fifth question is B's follow-up, and it exists only if the ANSWERED \
             exit from Recovering re-checked the debts: the socket is silent from the \
             recovery answer onwards, so nothing else can wake this loop: {targets:?}"
        );

        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        assert!(
            ids.contains(&format!("{SWITCHED_THREAD}:item:{LIFECYCLE_USER_ITEM}")),
            "B's userMessage finished before this link subscribed to B and its running \
             turn reported placeholder ids, so the follow-up above is the only thing that \
             can ever name it: {ids:?}"
        );
        // The recovery genuinely landed — which is what makes this the ANSWERED exit and
        // not the give-up the sibling test covers.
        assert!(
            ids.contains(&format!("{LIFECYCLE_THREAD}:item:{LIFECYCLE_USER_ITEM}")),
            "the premise is that A's recovery was answered and consumed; without A's \
             withheld item this test is exercising the timeout arm again: {ids:?}"
        );
        assert_eq!(
            connections, 1,
            "nothing here is a reason to tear down the connection observing B"
        );
    }

    /// **A `/new` DURING THE ON-DEMAND RECOVERY DOES NOT PIPELINE OVER IT** (round-8 F1).
    ///
    /// `Recovering` is a resume outstanding, and the candidate block tested only for
    /// `Awaiting`. So the third exit from `Recovering` was the candidate block itself:
    /// an announcement for C replaced the state with `due_now()`, `resume(C)` went out
    /// beside A's unanswered request — the pipelining A2 forbids — and A's answer then
    /// matched no active id and was dropped as unroutable.
    ///
    /// # The observable is the QUESTIONS, and that is a correction worth stating
    ///
    /// The obvious claim would be A's withheld item: the recovery answer is the only
    /// thing that can name it, so a dropped answer loses it. **Measured, that is no
    /// longer true on this tree, and the test says so rather than claiming a kill it
    /// does not make.** Round-8 F2 keeps the carried debt until the answer is
    /// *persisted*, so the dropped answer leaves the obligation owed, adopting C re-arms
    /// it, and the item is recovered on the retry. The two fixes compose: F2 repairs what
    /// F1 breaks, which is the right relationship and the reason neither substitutes for
    /// the other.
    ///
    /// What the pipelining cannot hide is the SEQUENCE. The retry is a SIXTH question
    /// about A that a link honouring A2 never asks, and it is asked because a resume went
    /// out while one was already outstanding. So the resume targets are the claim, in
    /// their exact order and length, and A's item is asserted beside them as the
    /// premise — the answer really did carry it.
    ///
    /// **Mutation:** restore `!matches!(attach, Attach::Awaiting { .. })` as the whole
    /// condition on the candidate block. Left gains a sixth ask about A; right has five.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_switch_announced_during_a_recovery_waits_for_it() {
        let (events, connections, targets, carried) = drive_holding_the_carry(
            ResumeAnswer::SwitchThenAThirdIsAnnouncedDuringTheRecovery,
            Duration::from_millis(2500),
        )
        .await;

        // **The claim.** Five questions, in this order: A, A's follow-up, B, the
        // on-demand recovery for A, then C. A resume issued against `Recovering` adds a
        // sixth — the retry the dropped answer makes necessary — and that extra ask is
        // the pipelining, visible.
        assert_eq!(
            targets,
            vec![
                LIFECYCLE_THREAD.to_string(),
                LIFECYCLE_THREAD.to_string(),
                SWITCHED_THREAD.to_string(),
                LIFECYCLE_THREAD.to_string(),
                THIRD_THREAD.to_string()
            ],
            "a candidate applied against Recovering re-targets the attach and pipelines \
             resume(C) beside A's unanswered request; A's answer then matches no active \
             id and is dropped, and the link has to ask a sixth time to get it. The \
             candidate must also not be STRANDED by waiting — C is asked about, once, at \
             the instant the recovery ends: {targets:?}"
        );

        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        // The premise: the answer this test is about really did carry A's withheld item,
        // so "consumed" and "thrown away" are distinguishable at all.
        assert!(
            ids.contains(&format!("{LIFECYCLE_THREAD}:item:{LIFECYCLE_USER_ITEM}")),
            "A was joined mid-turn with its userMessage withheld, and the recovery answer \
             is what names it: without this the sequence above proves nothing: {ids:?}"
        );
        assert_eq!(
            carried.adopted.as_deref(),
            Some(THIRD_THREAD),
            "C's resume was accepted, so C is the thread a reconnect inherits"
        );
        assert_eq!(
            connections, 1,
            "none of this is a reason to end a healthy connection"
        );
    }

    /// **A RECOVERY THAT WAS ASKED AND NEVER ANSWERED KEEPS ITS DEBT** (round-8 F2).
    ///
    /// The debt used to be discharged the moment `send_resume` returned. Round 7 moved it
    /// there from the noticing site for a good reason — a debt cleared against a state
    /// that could not issue a request is a debt lost — but a request on the wire is not a
    /// recovery. The answer still has to arrive, be readable, and be WRITTEN, and the
    /// request cannot be un-sent when any of those fails.
    ///
    /// This stages the plainest of those failures: the leg receives the recovery ask and
    /// closes the socket. `send_resume` succeeded, so nothing about the ASK failed —
    /// which is exactly what separates this from the sibling case where the write itself
    /// does. `Carried::owed_recovery` is the only surviving name for A (`fallback` is
    /// cleared by the very adoption of B that made the debt payable), so a slot emptied
    /// at the send site leaves the reconnect with nothing to ask about.
    ///
    /// The observable is the reconnect's own question, and the fact only its answer can
    /// name. Retry is free: every recovered key goes through the log's dedup.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recovery_asked_and_left_unanswered_by_a_dead_leg_is_asked_again() {
        let (events, connections, targets, carried) = drive_holding_the_carry(
            ResumeAnswer::SwitchThenTheLegVanishesAfterTheRecoveryIsAsked,
            Duration::from_millis(3000),
        )
        .await;

        assert!(
            connections >= 2,
            "the premise is a reconnect after the leg dropped mid-recovery: {connections}"
        );
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        // **The claim.** Connection 2 adopted B, re-armed from the carry, and asked about
        // A — the only route by which this key can exist.
        assert!(
            ids.contains(&format!("{LIFECYCLE_THREAD}:item:{LIFECYCLE_USER_ITEM}")),
            "a debt discharged by the SEND is gone when the answer never comes: the \
             successor adopts B, finds nothing owed, and A's pre-subscription items are \
             unreachable for the rest of the session: {ids:?}"
        );
        assert!(
            targets.iter().filter(|t| *t == LIFECYCLE_THREAD).count() >= 4,
            "connection 1 asked about A three times (attach, follow-up, recovery); the \
             fourth is the successor asking again: {targets:?}"
        );
        // And the debt is discharged once it is genuinely paid, so a third connection
        // would not ask a fourth time.
        assert_eq!(
            carried.owed_recovery, None,
            "the recovery was answered and written on the second connection, which is \
             what discharges it — the slot must not stay set for ever"
        );
    }

    /// **AN ANSWER THAT SAYS THE TURN IS STILL RUNNING HAS PAID NOTHING** (round-9 F1).
    ///
    /// The third way an on-demand recovery can fail to be one, and the only one where
    /// nothing goes wrong at all: the request is written, the answer arrives, it is
    /// readable, and every fact it describes is filed. None of those facts is the one
    /// that is owed. A turn reported `inProgress` describes its items with placeholder
    /// ids — measured, and the reason `plan_resume_seed` contributes no item fact for
    /// one — so the real ids remain reachable from nowhere, and only a later answer
    /// reporting that turn *finished* can name them.
    ///
    /// The link is on B by then, so A's own terminal is filtered when it arrives and
    /// nothing on that connection settles the debt afterwards. A discharge on the
    /// strength of "the answer was consumed to completion" therefore loses A's
    /// pre-subscription items exactly as a discharge on the strength of the send did —
    /// the failure round 8 fixed, arriving through the one door it left open.
    ///
    /// The observable is A's withheld `userMessage`. Only connection 2's answer carries
    /// it, and connection 2 asks about A only if the debt survived an answer this
    /// build reads perfectly well.
    ///
    /// **Mutation:** delete the `still_running > 0` arm in `recover_from` — so a
    /// readable, fully written answer discharges the debt whatever it reports — and the
    /// item assertion fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recovery_answered_with_the_turn_still_running_keeps_its_debt() {
        let (events, connections, targets, carried) = drive_holding_the_carry(
            ResumeAnswer::SwitchThenTheRecoveryAnswersARunningTurn,
            Duration::from_millis(3000),
        )
        .await;

        assert!(
            connections >= 2,
            "the premise is a reconnect after the leg answered and dropped: {connections}"
        );
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        assert!(
            ids.contains(&format!("{LIFECYCLE_THREAD}:item:{LIFECYCLE_USER_ITEM}")),
            "the answer that discharged the debt described A's items with placeholder \
             ids and filed none of them; the successor then adopts B, finds nothing \
             owed, and A's pre-subscription items are unreachable for the rest of the \
             session: {ids:?}"
        );
        assert!(
            targets.iter().filter(|t| *t == LIFECYCLE_THREAD).count() >= 4,
            "connection 1 asked about A three times (attach, follow-up, recovery); the \
             fourth is the successor asking again because the third answer settled \
             nothing: {targets:?}"
        );
        assert_eq!(
            carried.owed_recovery, None,
            "and an answer reporting the turn FINISHED does discharge it — the debt \
             survives what cannot pay it, not everything"
        );
    }

    /// **A RECOVERY WHOSE WRITE FAILS STAYS OWED** (round-8 F2, the other half).
    ///
    /// The scripted-leg case above proves the debt survives an answer that never comes.
    /// This proves the other thing [`Connection::recover_from`] now decides: an answer
    /// that DID come, was readable, and could not be persisted is not a recovery either.
    /// `record` logs a failed insert and returns — the live path has nothing to do with
    /// one — so before this the whole answer could fail to land and the debt would still
    /// be reported as settled.
    ///
    /// The failure is a real one rather than an injected flag: a second connection to the
    /// same SQLite file drops the `events` table, so every `append_event` returns "no
    /// such table" from inside the blocking pool. That is the shape a disk error takes at
    /// this boundary, and it needs no test-only branch in the store to stage.
    ///
    /// **Mutation:** make `recover_from` return `true` unconditionally, and the first
    /// assertion fails.
    #[tokio::test]
    async fn a_recovery_whose_facts_cannot_be_written_is_not_settled() {
        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000002".into(),
            name: "cc-1".into(),
        };
        let (daemon, db) = linked_daemon(&session);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let answer = lifecycle_answer(1, LIFECYCLE_THREAD, |_| {});
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 1,
                upstream_epoch: 1,
                thread_id: Some(SWITCHED_THREAD.to_string()),
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };

        // The healthy answer first, so the failure below is plainly the write and not the
        // answer: this is the same frame, read by the same code, against a log that works.
        assert!(
            conn.recover_from(&answer, LIFECYCLE_THREAD).await,
            "a readable answer whose facts all land is a recovery, and discharges the debt"
        );

        // Now take the log away. A second connection is what another process is; the
        // daemon's own connection sees the schema change on its next statement.
        let saboteur = rusqlite::Connection::open(db.path()).expect("the same file");
        saboteur
            .execute_batch("DROP TABLE events;")
            .expect("the table exists until now");

        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 1,
                upstream_epoch: 1,
                thread_id: Some(SWITCHED_THREAD.to_string()),
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };
        assert!(
            !conn.recover_from(&answer, LIFECYCLE_THREAD).await,
            "the answer was in this link's hands and the log does not hold it, so the \
             obligation is still owed — a debt discharged here is the facts lost"
        );
    }

    /// **What an announced-but-unsubscribed link records: the announcement, once.**
    ///
    /// The measured wire hands such a connection the thread's identity and no `turn/*`
    /// or `item/*` frame at all, so this is the whole of its timeline until an accepted
    /// resume subscribes it.
    fn assert_only_the_announcement_recorded(events: &[protocol::event::Event]) {
        let keys: Vec<String> = events
            .iter()
            .map(|e| e.source_event_id.clone().unwrap_or_default())
            .collect();
        assert_eq!(
            keys,
            vec![format!("{LIFECYCLE_THREAD}:thread_started")],
            "a link that has not attached is handed the announcement and nothing else, \
             so anything beyond it here is a fact the wire never delivered: {events:?}"
        );
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

    /// **The pre-turn arc, end to end**: bind from the `thread/started` this connection
    /// watched, **still resume it**, meet the measured not-ready error, retry on the
    /// same connection, survive an EOF, and carry the target across the reconnect.
    ///
    /// The resume is the part that moved. Until 2e-4b an announcement discharged the
    /// attach and this link would never have asked at all — which, on the measured
    /// wire, is a link that is bound and permanently unsubscribed. Now it asks, and
    /// before the thread's first turn the honest answer is that there is no rollout
    /// yet, so it keeps asking.
    ///
    /// The second connection gets **no announcement**, because a reconnect to a running
    /// thread does not get one: the announcement happened on a connection that is gone.
    /// So it stays unbound and its only business is the attach.
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
        assert_only_the_announcement_recorded(&events);
    }

    /// **Every answer outside the two accepted shapes fails closed.**
    ///
    /// Two families here, and the second is the one that earns its keep. The shapeless
    /// ones — an empty `turns[]`, a `turns[]` in the wrong place, no turns array, a
    /// policy refusal, another thread, two disagreeing identities — would be caught by
    /// almost any reader. The other four are the **real captured answer with exactly
    /// one measured particular changed**: a turn state this wire has never reported, an
    /// item list the answer itself flags as partial, an item missing a routing
    /// identity. A rule that had quietly widened would take all four while still
    /// rejecting the shapeless ones, and the suite would look green.
    ///
    /// Each is asserted per connection: the leg answers, the link reconnects, and
    /// the count grows every time — a link that quietly accepted any of them would
    /// settle on one connection and stop.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_answer_outside_the_two_accepted_shapes_fails_closed() {
        for answer in [
            ResumeAnswer::EmptyTurns,
            ResumeAnswer::TurnsNotUnderThread,
            ResumeAnswer::Refused,
            ResumeAnswer::NoTurnsArray,
            ResumeAnswer::WrongThread,
            ResumeAnswer::ConflictingIdentity,
            // The four that are the REAL answer with one thing wrong. These are the
            // ones worth having: each is a well-formed populated result that differs
            // from the accepted shape in exactly one measured particular, so a rule
            // that had quietly widened would accept them while the shapeless ones
            // above still failed.
            ResumeAnswer::UnmeasuredTurnStatus,
            ResumeAnswer::PartialItemsView,
            ResumeAnswer::ItemWithoutType,
        ] {
            // **A policy refusal is now reported after a BOUNDED delay, not instantly**
            // (round-1 P5). A followed switch re-targets off a broadcast and the broker
            // adopts the new thread one round trip later, so a `-32001` for a thread that
            // is simply "not adopted YET" is retried a few times before it is believed.
            // The disposition is unchanged — end the connection and report loudly — but it
            // now costs `ADOPTION_RETRIES * ADOPTION_RETRY_DELAY` first, so this budget
            // covers three connections' worth of that instead of three instant refusals.
            let budget = if matches!(answer, ResumeAnswer::Refused) {
                4 * ADOPTION_RETRIES * ADOPTION_RETRY_DELAY
            } else {
                Duration::from_secs(3)
            };
            let (_, connections, resumes, _) = drive(answer, None, true, budget).await;
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

    /// **THE LINK FOLLOWS THE SWITCH** (2e-4c) — the whole deliverable, on one socket.
    ///
    /// Before this chunk the link bound once and never again: `bind_if_unbound` returned
    /// early while bound, so a `thread/started` for a new thread was learned by nobody and
    /// then dropped by the thread filter for naming a thread the link was not on. A link
    /// watching thread A stayed on thread A for the life of the session while the operator
    /// worked on B, recording nothing. This test is what fails if that returns.
    ///
    /// Four claims, and each is one of the spike's measurements:
    ///
    /// 1. **The announcement is the switch signal.** It is the only frame about B this
    ///    connection receives, so the link must act on it or act on nothing.
    /// 2. **The attach is re-targeted from `Attached`.** That is a resting state with no
    ///    deadline; a re-arm that only fired from `Unbound` would leave the link blocked
    ///    on a read for ever.
    /// 3. **One connection, two subscriptions.** `thread/resume` ADDS a subscription
    ///    rather than replacing one, so following the switch costs no reconnect. A
    ///    `connections == 1` assertion is what proves the link did not simply die and come
    ///    back — which is how this could pass while being completely broken.
    /// 4. **Zero cross-thread contamination.** B's replayed stream carries A's very item
    ///    and turn ids, so every fact is distinguishable only by its thread namespace.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_link_follows_a_new_switch_on_the_same_connection() {
        let (events, connections, resumes, _) = drive(
            ResumeAnswer::SwitchToSecondThread,
            None,
            true,
            Duration::from_secs(5),
        )
        .await;

        // CLAIM 2/3 — re-targeted, on the connection that was already attached.
        let targets: Vec<&str> = resumes
            .iter()
            .filter_map(|r| r["params"]["threadId"].as_str())
            .collect();
        assert!(
            targets.contains(&LIFECYCLE_THREAD) && targets.contains(&SWITCHED_THREAD),
            "the link must resume BOTH the original thread and the switched-to one: \
             {targets:?}"
        );
        assert!(
            targets.iter().position(|t| *t == LIFECYCLE_THREAD)
                < targets.iter().position(|t| *t == SWITCHED_THREAD),
            "it must resume the original FIRST and follow the switch afterwards: {targets:?}"
        );
        assert_eq!(
            connections, 1,
            "following a switch costs NO reconnect — a resume adds a subscription to the \
             connection that already has one (measured). {connections} connections means \
             the link died and recovered, which is not following a switch."
        );

        // CLAIM 1/4 — both threads produced facts, and every fact names exactly one of
        // them. The ids are shared between the two streams, so this can only pass if the
        // dedup key is thread-namespaced.
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        let from_a = ids
            .iter()
            .filter(|i| i.starts_with(LIFECYCLE_THREAD))
            .count();
        let from_b = ids
            .iter()
            .filter(|i| i.starts_with(SWITCHED_THREAD))
            .count();
        assert!(from_a > 0, "the original visit's facts must stand: {ids:?}");
        assert!(
            from_b > 0,
            "the switched-to thread's turn must be OBSERVED, which is the entire point: \
             {ids:?}"
        );
        assert_eq!(
            from_a + from_b,
            ids.len(),
            "every recorded fact must be namespaced to one of the two session threads, \
             and to no other: {ids:?}"
        );
        // A's timeline is INTACT: the switch retired a visit, it did not retract a fact.
        assert!(
            ids.iter()
                .any(|i| i == &format!("{LIFECYCLE_THREAD}:turn:{LIFECYCLE_TURN}")),
            "the pre-switch turn terminal must survive the switch: {ids:?}"
        );
        assert!(
            ids.iter()
                .any(|i| i == &format!("{SWITCHED_THREAD}:turn:{LIFECYCLE_TURN}")),
            "and the post-switch turn must be recorded under the NEW thread: {ids:?}"
        );
        // **The ANNOUNCEMENT ITSELF is a fact, and deferring the bind must not lose it.**
        //
        // `thread/started` carries the whole Thread object and mints the new thread's
        // session-identity row. Holding it as a candidate means the D4 filter — still on
        // the OLD thread when the frame arrives — would drop it, so it is replayed through
        // the ordinary ingest path once the visit moves.
        //
        // The KEY's presence proves nothing on its own: the resume answer that follows
        // mints the same first-wins key. The recorded PAYLOAD is what tells the two apart —
        // `announce()` carries `path: "/r/t.jsonl"`, the committed resume answer carries
        // the fixture's own rollout path, and first-wins means the store holds whichever
        // arrived first. Without the replay an unresumable switch would leave the thread
        // with no identity row at all.
        let identity = events
            .iter()
            .find(|e| {
                e.source_event_id.as_deref()
                    == Some(format!("{SWITCHED_THREAD}:thread_started").as_str())
            })
            .unwrap_or_else(|| {
                panic!(
                    "the switch announcement must be RECORDED for the thread it names: \
                     {ids:?}"
                )
            });
        assert_eq!(
            identity.payload["rollout_path"].as_str(),
            Some("/r/t.jsonl"),
            "the new thread's identity row must come from the ANNOUNCEMENT, which arrives \
             first — not from the later resume answer, whose presence would mean the \
             replay is missing"
        );
    }

    /// **AN IN-FLIGHT RECOVERY IS CONSUMED BEFORE THE SWITCH IS FOLLOWED** (round-1 P6).
    ///
    /// The defect this closes: the link attached mid-turn, so it owes itself one follow-up
    /// resume — the only thing that can ever name the items which finished before it
    /// subscribed. If a `thread/started` for a new thread re-points the visit the moment it
    /// arrives, that outstanding request is marked superseded and its answer is received,
    /// logged and THROWN AWAY. Those items are then unreachable from both directions for
    /// ever: no live frame carries them, and the link will never ask about that thread
    /// again.
    ///
    /// So the announcement is held as a CANDIDATE and applied only after the loop has
    /// settled the answer in flight.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recovery_answer_crossing_a_switch_is_recorded_before_the_link_follows() {
        let (events, connections, resumes, _) = drive(
            ResumeAnswer::RecoveryCrossingASwitch,
            None,
            true,
            Duration::from_secs(6),
        )
        .await;

        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        // The withheld userMessage — the fact ONLY the crossing answer could carry.
        let recovered = format!("{LIFECYCLE_THREAD}:item:{LIFECYCLE_USER_ITEM}");
        assert!(
            ids.contains(&recovered),
            "the recovery answer that crossed the switch boundary must be RECORDED before \
             the link follows; without it the item that finished before this link \
             subscribed is lost for ever. ids={ids:?} resumes={resumes:?}"
        );
        // ...and the link did follow, on the same connection.
        let targets: Vec<&str> = resumes
            .iter()
            .filter_map(|r| r["params"]["threadId"].as_str())
            .collect();
        assert!(
            targets.contains(&SWITCHED_THREAD),
            "the switch must still be followed once the recovery is settled: {targets:?}"
        );
        assert_eq!(connections, 1, "and without a reconnect");
    }

    /// **A NOT-YET-ADOPTED SWITCH TARGET IS RETRIED, NOT ABANDONED** (round-1 P5).
    ///
    /// The link re-targets off a `thread/started` BROADCAST; only the broker's own
    /// correlated verification of that creation makes the thread resumable. In between, a
    /// `thread/resume` naming it is policy-refused — an ordinary, short window that must
    /// not be read as "this answer is unreadable, end the connection". Doing so reconnects,
    /// asks the identical question and is refused again: a loop on every single switch.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_switch_target_the_broker_has_not_adopted_yet_is_retried_in_place() {
        let (events, connections, resumes, _) = drive(
            ResumeAnswer::SwitchThenNotAdoptedYet,
            None,
            true,
            Duration::from_secs(8),
        )
        .await;

        assert_eq!(
            connections, 1,
            "a not-yet-adopted refusal must be retried ON THIS CONNECTION; a reconnect \
             would ask the same question and be refused again — a loop on every switch"
        );
        let targets: Vec<&str> = resumes
            .iter()
            .filter_map(|r| r["params"]["threadId"].as_str())
            .collect();
        assert!(
            targets.iter().filter(|t| **t == SWITCHED_THREAD).count() >= 3,
            "the link must keep asking about the new thread across the refusals: {targets:?}"
        );
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        assert!(
            ids.iter().any(|i| i.starts_with(SWITCHED_THREAD)),
            "and must end up ATTACHED to it once the broker adopts it: {ids:?}"
        );
    }

    /// **A HANDSHAKE ANNOUNCEMENT SUPERSEDES THE CARRIED HINT** (closing S8).
    ///
    /// A hint is the registration's claim; a `thread/started` is the wire's own evidence.
    /// The announcement is broadcast ONCE and never repeated, so if it does not replace the
    /// hint in the slot a reconnect reads, a connection that dies before adopting anything
    /// comes back asking about the thread the registration guessed — for ever.
    ///
    /// Written even when the local target already agrees, because it is the SLOT that
    /// survives the connection, not the local.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_announcement_supersedes_the_carried_hint_across_a_reconnect() {
        let (_, connections, resumes, _) = drive(
            ResumeAnswer::AnnouncedThenDroppedBeforeAdopting,
            Some("th_STALE_HINT"),
            true,
            Duration::from_secs(10),
        )
        .await;
        assert!(connections >= 2, "the leg drops, so there is a reconnect");
        let targets: Vec<&str> = resumes
            .iter()
            .filter_map(|r| r["params"]["threadId"].as_str())
            .collect();
        // Connection 0 asked about the announced thread and was never answered; the
        // reconnect gets no announcement at all. So every target from the reconnect onward
        // comes from the carried slots, and it must be the ANNOUNCED thread rather than the
        // registration's stale claim.
        // The hint is asked ONCE, legitimately: on connection 0, before the announcement
        // that corrects it has arrived. Everything after that must be the announced thread.
        let first_announced = targets
            .iter()
            .position(|t| *t == LIFECYCLE_THREAD)
            .unwrap_or_else(|| panic!("the announced thread must be asked about: {targets:?}"));
        assert!(
            !targets[first_announced..].contains(&"th_STALE_HINT"),
            "once the wire has named the thread, the registration's stale claim must never \
             be asked about again — least of all by a reconnect, which gets no announcement \
             to correct it a second time: {targets:?}"
        );
        assert_eq!(
            targets.last(),
            Some(&LIFECYCLE_THREAD),
            "the reconnect must resume the ANNOUNCED thread: {targets:?}"
        );
    }

    /// **A CROSSING RECOVERY IS PAID AFTER THE SWITCH, ON DEMAND** (round-3 P7).
    ///
    /// The link attaches to A mid-turn, so A owes it a follow-up — the items that finished
    /// before the subscription existed are reachable from nowhere else. The follow-up goes
    /// out, a switch is announced, and the follow-up comes back NOT-READY: the recovery
    /// never lands. The link follows the switch and adopts B.
    ///
    /// Round-2 stopped there, and the honest reading of that is that A's history was simply
    /// lost — the P8 doctrine says "retired threads are recovered on demand", and nothing
    /// was demanding. Now the obligation outlives the switch and the connection, and is paid
    /// once after B is adopted.
    ///
    /// The assertion is the WITHHELD item, not a count: `capture_tail()` omits the
    /// userMessage precisely so that only a completed answer about A can supply it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recovery_owed_on_the_thread_left_behind_is_paid_after_the_switch() {
        let (events, connections, resumes, _) = drive(
            ResumeAnswer::SwitchWithACrossingRecoveryOwed,
            None,
            true,
            Duration::from_secs(14),
        )
        .await;

        let targets: Vec<&str> = resumes
            .iter()
            .filter_map(|r| r["params"]["threadId"].as_str())
            .collect();
        // B was adopted...
        assert!(
            targets.contains(&SWITCHED_THREAD),
            "the switch must be followed: {targets:?}"
        );
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        assert!(
            ids.iter().any(|i| i.starts_with(SWITCHED_THREAD)),
            "and its turn observed: {ids:?}"
        );
        // ...and A was asked about AGAIN, after the switch.
        let first_b = targets
            .iter()
            .position(|t| *t == SWITCHED_THREAD)
            .expect("B is asked about");
        assert!(
            targets[first_b..].contains(&LIFECYCLE_THREAD),
            "the debt owed on the thread left behind must be paid AFTER the switch — \
             nothing else will ever ask about it: {targets:?}"
        );
        // **The withheld item is there.** Only a completed answer about A can supply it:
        // no live frame carries it (it finished before the subscription existed) and the
        // mid-turn answer reported placeholder ids.
        assert!(
            ids.contains(&format!("{LIFECYCLE_THREAD}:item:{LIFECYCLE_USER_ITEM}")),
            "A's pre-subscription item must be RECOVERED, not lost with the switch: {ids:?}"
        );
        // **The link is STILL ON B.** A recovery reads a thread the link has left; it does
        // not return to it. The leg sends a fresh B terminal after answering the recovery,
        // and it can only be recorded if the visit is still on B — a recovery that re-bound
        // the visit would have the D4 filter drop it.
        assert!(
            ids.contains(&format!("{SWITCHED_THREAD}:turn:{TURN_B}")),
            "after paying the recovery the link must still be on B, recording B's frames: \
             {ids:?}"
        );
        // And the recovery cost no reconnect: it is an on-demand ask on the live link.
        assert_eq!(connections, 1, "paid on the connection that owes it");
        // It is paid ONCE. A second ask would be a loop over a thread nobody is on.
        assert_eq!(
            targets[first_b..]
                .iter()
                .filter(|t| **t == LIFECYCLE_THREAD)
                .count(),
            1,
            "the debt is paid exactly once: {targets:?}"
        );
    }

    /// **A CANDIDATE OUTLIVES THE CONNECTION THAT HEARD IT** (round-2 P6b).
    ///
    /// The link adopts A; the leg announces B and drops before the link's next pass can
    /// apply the candidate. `thread/started` is broadcast once and never replayed, so if
    /// the candidate died with that connection nothing would ever mention B again and the
    /// link would sit on A while the user works in B.
    ///
    /// Persisting the chase only at APPLY time was not enough, and this is the window that
    /// proved it: a candidate is held un-applied for as long as a resume is outstanding.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_candidate_survives_the_connection_that_heard_it() {
        let (_, connections, resumes, _) = drive(
            ResumeAnswer::CandidateAnnouncedThenTheLegDrops,
            None,
            true,
            Duration::from_secs(12),
        )
        .await;
        assert!(connections >= 2, "the leg drops, so there is a reconnect");
        let targets: Vec<&str> = resumes
            .iter()
            .filter_map(|r| r["params"]["threadId"].as_str())
            .collect();
        assert!(
            targets.contains(&SWITCHED_THREAD),
            "the reconnect must resume the thread announced on the connection that died — \
             nothing will ever announce it again: {targets:?}"
        );
        let first_b = targets
            .iter()
            .position(|t| *t == SWITCHED_THREAD)
            .expect("B is asked about");
        assert!(
            targets[..first_b].contains(&LIFECYCLE_THREAD),
            "and only after having adopted the old one: {targets:?}"
        );
        // **The candidate was never APPLIED on the connection that heard it.** The
        // announcement lands while the link's follow-up resume is outstanding, and the
        // connection dies before that answer arrives — so the apply block never runs. A
        // chase persisted only at apply time is lost exactly here, and the link would sit
        // on the old thread for the rest of the session.
        // **And the FALLBACK was recorded when the candidate was NOTED.** B is refused for
        // ever, so the reconnect can only get back to the thread it adopted if a fallback
        // was carried alongside the candidate. The apply block never ran on the connection
        // that heard the announcement, so note-time is the only place it could come from.
        assert!(
            targets[first_b..].contains(&LIFECYCLE_THREAD),
            "an un-applied candidate must carry its FALLBACK too, or a never-adopted \
             switch strands the link: {targets:?}"
        );
        assert_eq!(
            connections, 2,
            "and the return trip is a FALLBACK on connection 2, not another reconnect: \
             {targets:?}"
        );
    }

    /// **A RECONNECT CARRYING AN ADOPTED TARGET TREATS AN ANNOUNCEMENT AS A CANDIDATE**
    /// (round-2 P6c).
    ///
    /// Uniformity: an announcement is never an instant repoint. On a fresh connection the
    /// link is technically unbound, but a target it ADOPTED is not a guess — it is a thread
    /// this link demonstrably could read. Binding an announced thread instantly there would
    /// discard it in favour of one the broker may never adopt, with no way back.
    ///
    /// (A registration HINT is deliberately different: it is a claim, the announcement is
    /// the wire's own evidence, and evidence beats a claim at once. That case is covered by
    /// `an_announcement_redirects_the_target_and_never_settles_the_attach`.)
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reconnect_carrying_an_adopted_target_treats_an_announcement_as_a_candidate() {
        let (_, connections, resumes, _) = drive(
            ResumeAnswer::ReconnectCarryingAdoptedThenAnnouncedAnother,
            None,
            true,
            Duration::from_secs(14),
        )
        .await;
        let targets: Vec<&str> = resumes
            .iter()
            .filter_map(|r| r["params"]["threadId"].as_str())
            .collect();
        assert!(
            targets.contains(&SWITCHED_THREAD),
            "the announced thread must be chased: {targets:?}"
        );
        // ...and because it is never adopted, the link must be able to come BACK to the
        // thread it had adopted. That return trip is only possible if the announcement was
        // treated as a candidate with a fallback rather than as an instant repoint.
        let first_b = targets
            .iter()
            .position(|t| *t == SWITCHED_THREAD)
            .expect("B is asked about");
        assert!(
            targets[first_b..].contains(&LIFECYCLE_THREAD),
            "an announcement that is never adopted must leave the link able to return to \
             the thread it had ADOPTED; an instant repoint would have discarded it: \
             {targets:?}"
        );
        // **The return trip is a FALLBACK, not a reconnect** — which is what isolates this
        // to the candidate treatment. An instant repoint records no fallback at all, so the
        // link can only get back to A by exhausting its budget, taking the loud
        // unreadable-answer path, and reconnecting onto its published target. Counting
        // connections is what tells those two stories apart.
        assert_eq!(
            connections, 3,
            "conn 0 adopts A, conn 1 chases B and FALLS BACK to A on the same connection, \
             conn 2 verifies what was published. A fourth connection means the link had to \
             reconnect to reach A, i.e. no fallback was ever recorded: {targets:?}"
        );
        // **And what was PUBLISHED is the ADOPTED thread, never the chased one** (P6a).
        // Connection 2 starts from the published target, so its first ask is the evidence.
        let last_b = targets
            .iter()
            .rposition(|t| *t == SWITCHED_THREAD)
            .expect("B is asked about");
        assert_eq!(
            targets.get(last_b + 1),
            Some(&LIFECYCLE_THREAD),
            "after the chase failed, the next connection must start from the thread this \
             link ADOPTED — publishing the chased thread would hand a reconnect a target \
             the broker never adopted, with the readable one discarded: {targets:?}"
        );
    }

    /// **TWO SWITCHES IN A ROW: B's FACT SURVIVES, C WINS THE SUBSCRIPTION** (round-2 P8).
    ///
    /// The candidate slot holds ONE thread, so a second `/new` while a recovery is
    /// outstanding replaces the first. The question is what that costs, and the
    /// measurements answer it: a link that never subscribed to B receives **nothing about B
    /// but its announcement** — `turn/*` and `item/*` reach only the resume-subscribed
    /// connection — and B's history is not lost either, because a retired thread stays
    /// fully resumable and can be recovered on demand.
    ///
    /// So the announcement's fact is recorded the moment it arrives (it is verified
    /// broadcast content from this session's own app-server), and only the SUBSCRIPTION
    /// chases the latest candidate. The link subscribes to where the user IS; queueing
    /// candidates would chase threads the user has already left.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_switches_in_a_row_keep_both_facts_and_chase_the_latest() {
        let (events, connections, resumes, _) = drive(
            ResumeAnswer::TwoSwitchesDuringRecovery,
            None,
            true,
            Duration::from_secs(8),
        )
        .await;

        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        // **B's announcement fact is there even though B was never subscribed to.**
        assert!(
            ids.contains(&format!("{SWITCHED_THREAD}:thread_started")),
            "the REPLACED candidate's announcement must still be recorded — it is the only \
             thing a dropped candidate could otherwise cost: {ids:?}"
        );
        assert!(
            ids.contains(&format!("{THIRD_THREAD}:thread_started")),
            "and so must the latest one's: {ids:?}"
        );
        // **The subscription chased C, not B.**
        let targets: Vec<&str> = resumes
            .iter()
            .filter_map(|r| r["params"]["threadId"].as_str())
            .collect();
        assert!(
            targets.contains(&THIRD_THREAD),
            "the link must chase the LATEST announcement — where the user is: {targets:?}"
        );
        assert!(
            !targets.contains(&SWITCHED_THREAD),
            "and must not chase the one the user had already left: {targets:?}"
        );
        // The recovery that was in flight when the switches landed still got recorded.
        assert!(
            ids.contains(&format!("{LIFECYCLE_THREAD}:item:{LIFECYCLE_USER_ITEM}")),
            "the in-flight recovery must survive TWO announcements, not just one: {ids:?}"
        );
        assert_eq!(connections, 1, "and none of it costs a reconnect");
    }

    /// **AN UNREADABLE ANSWER STILL ENDS THE LEG, AND THE CANDIDATE SURVIVES** (round-2
    /// P7 + P6b).
    ///
    /// The `Refused` bail used to sit at the very bottom of the loop, after the candidate
    /// block — so a switch announcement arriving on the same pass applied a candidate,
    /// re-armed the attach, and left `Refused` behind: the loud stop that an unreadable
    /// answer is supposed to produce was SUPPRESSED by an unrelated event.
    ///
    /// Moving the bail earlier does not lose the candidate, and that is the other half of
    /// this test: `thread/started` is broadcast once and never replayed, so a candidate that
    /// died with the connection could never be rediscovered. It is carried across the
    /// reconnect and asked about there.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreadable_answer_ends_the_leg_and_the_candidate_survives_the_reconnect() {
        let (events, connections, resumes, _) = drive(
            ResumeAnswer::UnreadableAnswerBesideASwitch,
            None,
            true,
            Duration::from_secs(8),
        )
        .await;

        assert!(
            connections >= 2,
            "an answer this build cannot read must END the leg, even when a switch \
             announcement arrives on the same pass: {connections} connections"
        );
        let targets: Vec<&str> = resumes
            .iter()
            .filter_map(|r| r["params"]["threadId"].as_str())
            .collect();
        assert!(
            targets.contains(&SWITCHED_THREAD),
            "the candidate must SURVIVE the reconnect — a thread/started is broadcast once \
             and never replayed, so a candidate lost with its connection is lost for \
             ever: {targets:?}"
        );
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        assert!(
            ids.iter().any(|i| i.starts_with(SWITCHED_THREAD)),
            "and the link must end up observing it: {ids:?}"
        );
    }

    /// **THE FALLBACK COMPLETES** (round-2 P5).
    ///
    /// A followed switch whose target the broker never adopts must not strand the session.
    /// The link spends its adoption budget on B, gives up, and goes back to A — and the
    /// going-back has to WORK. It could not before: the bound-target redirect fires on the
    /// pass after the fallback, rewrites the target from A back to B (the visit is on B),
    /// and marks the outstanding A-request superseded, so its answer is discarded and the
    /// cycle repeats for ever. The redirect is suppressed for the duration of a fallback.
    ///
    /// Scripted with B refused FOR EVER, so the fallback is genuinely reached — a script
    /// that relented after a few refusals would let this pass without the fallback ever
    /// running, which is precisely how the round-1 version masked the defect.
    ///
    /// It also carries the measurement that keeps [`Carried`] in memory: the last
    /// assertion pins the state in which the durable log's newest identity fact and the
    /// thread a replacement link should resume are DIFFERENT threads. See the comment
    /// there.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_switch_target_that_is_never_adopted_falls_back_and_the_fallback_completes() {
        let (events, connections, resumes, _, published, _) = drive_watching_presence(
            ResumeAnswer::SwitchNeverAdoptedThenFallback,
            None,
            true,
            // Long enough to see what the link asks about AFTER the fallback's first ask
            // is answered not-ready — which is the window a fallback that failed to bring
            // the VISIT back would show up in — and no longer. These scripted legs hold
            // real sockets open for their whole budget, and an over-generous one is not
            // free: it starves other suites in the same `cargo test --workspace` run of
            // file descriptors, which surfaces as unrelated tests failing to read the
            // system trust store.
            Duration::from_secs(15),
        )
        .await;

        let targets: Vec<&str> = resumes
            .iter()
            .filter_map(|r| r["params"]["threadId"].as_str())
            .collect();
        // The budget was genuinely spent on B...
        assert!(
            targets.iter().filter(|t| **t == SWITCHED_THREAD).count() > ADOPTION_RETRIES as usize,
            "the adoption budget must be spent before the fallback — otherwise the \
             fallback is never reached and this test proves nothing: {targets:?}"
        );
        // ...and then the link went BACK to the thread it could read.
        //
        // Anchored on the FIRST fallback, not the last B. Anchoring on the last B was
        // self-defeating: a retarget back to B simply moved the anchor, so the very defect
        // this asserts against slid out of the window it was measured in.
        let first_b = targets
            .iter()
            .position(|t| *t == SWITCHED_THREAD)
            .expect("B was asked about");
        let first_fallback = targets[first_b..]
            .iter()
            .position(|t| *t == LIFECYCLE_THREAD)
            .map(|i| first_b + i)
            .unwrap_or_else(|| {
                panic!(
                    "after the budget is spent the link must fall back to the thread it \
                     could read: {targets:?}"
                )
            });
        // **And the fallback COMPLETED**: from the first fallback onward, the link never
        // went back to chasing B. A single retarget to B here means the redirect was not
        // suppressed, and the fallback can never structurally complete — it would retarget
        // B, be refused, fall back, and loop for ever.
        assert!(
            !targets[first_fallback..].contains(&SWITCHED_THREAD),
            "once falling back, the redirect must stay suppressed; a retarget to B after \
             the fallback began means the fallback cannot complete: {targets:?}"
        );
        assert_eq!(connections, 1, "all of it on one connection");
        // A's facts are there, and nothing was filed under the never-adopted B beyond its
        // announcement (which is verified broadcast content).
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        assert!(
            ids.iter().any(|i| i.starts_with(LIFECYCLE_THREAD)),
            "the fallback thread's facts must be recorded: {ids:?}"
        );
        let from_b: Vec<&String> = ids
            .iter()
            .filter(|i| i.starts_with(SWITCHED_THREAD))
            .collect();
        assert_eq!(
            from_b,
            vec![&format!("{SWITCHED_THREAD}:thread_started")],
            "a thread this link never subscribed to can contribute exactly its \
             announcement and nothing else"
        );

        // ------------- why the carry is a CELL and not a query (round-6)
        //
        // It is a fair question whether [`Carried`] needs to exist at all, given that
        // the daemon already files a durable identity fact for every thread the wire
        // names — synchronously, before anything can observe it, surviving restarts
        // the in-memory cell does not, and scoped to the session. If the newest such
        // fact always named the thread a replacement link should resume, the cell and
        // everything that retains it could be deleted and the answer read from the log.
        //
        // **This leg is the counter-example, and it is the ordinary one.** The log
        // records when a thread was first DESCRIBED; the carry records what the link
        // has PROVEN it can read. Those come apart exactly here: B was announced after
        // A, so B's fact is the newest one in the log — and B is the thread the broker
        // refused on every ask, which is why the link is back on A. Re-resuming A files
        // nothing new, because `<A>:thread_started` is already there and the dedup key
        // is first-wins, so nothing will ever move the log's newest fact back to A.
        //
        // A store-derived seed would therefore hand the replacement the one thread this
        // link demonstrably could not read, and hand it over WITHOUT the fallback that
        // rescued this one — `Carried::fallback` has no representation in the log at
        // all, because "adopted" is a fact about a resume being accepted and the log
        // only ever saw an announcement. Asserted rather than argued, so a later round
        // that reaches for the tidier design meets the measurement first.
        let newest_identity = events
            .iter()
            .rfind(|e| e.kind == protocol::event::EventKind::SessionStart)
            .and_then(|e| e.source_event_id.clone())
            .expect("both threads announced themselves");
        assert_eq!(
            newest_identity,
            format!("{SWITCHED_THREAD}:thread_started"),
            "the newest identity fact in the log names B — the thread that was never \
             adopted — while the link is correctly back on A. The log cannot answer \
             'where should a replacement resume', and this is the shape that proves it."
        );

        // ------------- what the FLEET was told while all that was happening
        //
        // **The hold outlasts one resume budget, and the note used to say it could
        // not.** `Connection::addressee` claimed the window in which the fleet does
        // not yet name B was bounded by `RESUME_BUDGET`. That bounds the *hold* —
        // the candidate parked while A's resume is outstanding — and nothing more:
        // applying the candidate is where the chase starts, and this leg's chase
        // spends the whole adoption budget asserted above. Read off the same leg
        // rather than a second one: another 15 s of held sockets would buy nothing
        // and costs the rest of the workspace real load.
        assert!(
            !published.iter().any(|s| matches!(
                s,
                CodexAddressee::Subscribed { thread_id } if thread_id == SWITCHED_THREAD
            )),
            "B is refused on every ask, so nothing may ever report it readable: \
             {published:?}"
        );
        assert!(
            published.iter().any(|s| matches!(
                s,
                CodexAddressee::Bound { thread_id } if thread_id == SWITCHED_THREAD
            )),
            "once the candidate is APPLIED the wire has named B and the visit is on \
             it, and `Bound` is exactly that fact — announced, not readable: \
             {published:?}"
        );
        assert_eq!(
            published.last().and_then(CodexAddressee::thread_id),
            Some(LIFECYCLE_THREAD),
            "and the bound that does exist is reached: the link gives up on B and is \
             back on the thread it can read, not stranded: {published:?}"
        );
    }

    /// **A DEBT IS KEYED BY (THREAD, TURN)** (round-1 P7).
    ///
    /// Turn ids are unique per thread, not per session, and one connection now carries more
    /// than one thread's turns. A bare turn-id key lets a resume of thread B settle a debt
    /// owed on thread A — marking A's unrecoverable items recovered when nothing recovered
    /// them.
    #[test]
    fn a_debt_is_scoped_to_its_thread() {
        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000000".into(),
            name: "cc-1".into(),
        };
        let (daemon, _db) = linked_daemon(&session);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 1,
                upstream_epoch: 0,
                thread_id: Some(LIFECYCLE_THREAD.to_string()),
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };
        // The SAME turn id, owed on two different threads.
        let turn = "01a0-turn".to_string();
        conn.debts
            .insert((LIFECYCLE_THREAD.to_string(), turn.clone()), TurnDebt::Owed);
        conn.debts
            .insert((SWITCHED_THREAD.to_string(), turn.clone()), TurnDebt::Owed);

        // While on thread A, only A's debt is visible...
        assert!(conn.follow_up_owed(), "A owes a follow-up");
        // ...and a request naming A settles ONLY A's.
        conn.launch_follow_up(LIFECYCLE_THREAD);
        assert_eq!(
            conn.debts.get(&(SWITCHED_THREAD.to_string(), turn.clone())),
            Some(&TurnDebt::Owed),
            "a resume of A must NOT settle a debt owed on B — nothing recovered it"
        );
        assert_eq!(
            conn.debts
                .get(&(LIFECYCLE_THREAD.to_string(), turn.clone())),
            Some(&TurnDebt::Settled)
        );
        assert!(!conn.follow_up_owed(), "A is settled");

        // Follow the switch: now B's debt is the visible one.
        conn.switch_candidate = Some(SWITCHED_THREAD.to_string());
        assert_eq!(
            conn.apply_switch_candidate().as_deref(),
            Some(LIFECYCLE_THREAD)
        );
        assert!(
            conn.follow_up_owed(),
            "on B, B's debt is owed and must still be payable"
        );
    }

    /// A re-announcement of the thread the link is ALREADY on is not a switch.
    ///
    /// It is not a shape the wire has shown — the app-server broadcasts one
    /// `thread/started` per creation — but reading one as a switch would retire a live
    /// visit and bump a generation for nothing, so the idempotence is pinned rather than
    /// assumed.
    #[test]
    fn a_re_announcement_of_the_bound_thread_is_not_a_switch() {
        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000000".into(),
            name: "cc-1".into(),
        };
        let (daemon, _db) = linked_daemon(&session);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 7,
                upstream_epoch: 0,
                thread_id: None,
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };
        let first: Value = serde_json::from_str(&announce(LIFECYCLE_THREAD)).unwrap();
        conn.note_switch_candidate(&first);
        assert_eq!(
            conn.visit.thread_id.as_deref(),
            Some(LIFECYCLE_THREAD),
            "the FIRST announcement binds immediately — there is no outstanding recovery \
             to lose and no previous thread to fall back to"
        );
        assert_eq!(
            conn.visit.generation, 7,
            "a bind does not bump the generation"
        );
        conn.note_switch_candidate(&first);
        assert!(
            conn.switch_candidate.is_none(),
            "a re-announcement of the bound thread is not a candidate"
        );
        assert_eq!(conn.visit.generation, 7, "and still does not");
        assert_eq!(conn.visit.thread_id.as_deref(), Some(LIFECYCLE_THREAD));

        // A DIFFERENT thread is a switch — but it is HELD as a candidate first
        // (round-1 P5/P6), so nothing about the visit moves until the loop applies it.
        let second: Value = serde_json::from_str(&announce(SWITCHED_THREAD)).unwrap();
        conn.note_switch_candidate(&second);
        assert_eq!(
            conn.visit.thread_id.as_deref(),
            Some(LIFECYCLE_THREAD),
            "a candidate must NOT re-point the visit: an outstanding recovery for the \
             current thread would be thrown away, and the new thread may never be adopted"
        );
        assert_eq!(conn.visit.generation, 7, "nor bump the generation");

        // **The broker's head delivery cannot walk this link back** (2e-7b round-2
        // F1's D4 interaction, verified here rather than assumed). The broker
        // delivers the head it has VERIFIED, which lags the announcement — so the
        // frame that can arrive while a candidate is held is one naming the thread
        // the visit is still bound to. It must move neither: it names the bound
        // thread, so it is `Passed` to the ordinary filter and the candidate stands.
        assert_eq!(
            conn.note_switch_candidate(&first),
            Switch::Passed,
            "a late announcement of the BOUND thread is not a switch"
        );
        assert_eq!(
            conn.switch_candidate.as_deref(),
            Some(SWITCHED_THREAD),
            "and it must not displace the candidate the person actually moved to"
        );
        assert_eq!(
            conn.visit.thread_id.as_deref(),
            Some(LIFECYCLE_THREAD),
            "nor re-point the visit"
        );
        assert_eq!(conn.visit.generation, 7, "nor bump the generation");

        assert_eq!(
            conn.apply_switch_candidate().as_deref(),
            Some(LIFECYCLE_THREAD),
            "applying it names what it retired"
        );
        assert_eq!(
            conn.visit.generation, 8,
            "a switch retires the visit — D4: a generation identifies a VISIT"
        );
        assert_eq!(conn.visit.thread_id.as_deref(), Some(SWITCHED_THREAD));
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

    /// **An announcement REDIRECTS the target, and never settles the attach.**
    ///
    /// This test used to assert the opposite — that an announcement *discharged* an
    /// outstanding resume — and that rule was retired by measurement, not by taste. A
    /// connection that has not resumed is handed no `turn/*` frame at all, so treating
    /// the announcement as "nothing left to ask for" left a perfectly healthy link
    /// bound, silent and permanently unsubscribed. Recovery was never what the resume
    /// bought; subscription is.
    ///
    /// So: the link is given a stale hint and attaches to it. The leg never answers that
    /// resume and announces the real thread instead. What must follow is that the
    /// announcement **redirects** the target, the stale attach is left to time out
    /// (RESUME_BUDGET is shortened under `cfg(test)` precisely so this is
    /// distinguishable from a discharge), the connection ends, and every later attach
    /// addresses the ANNOUNCED thread.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_announcement_redirects_the_target_and_never_settles_the_attach() {
        let (events, connections, resumes, _) = drive(
            ResumeAnswer::AnnounceLate,
            Some("th_STALE_TARGET"),
            true,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            resumes[0]["params"]["threadId"].as_str(),
            Some("th_STALE_TARGET"),
            "the hint addresses the first attach: {resumes:?}"
        );
        // **Not discharged.** A discharge would leave exactly one resume for ever. The
        // attach instead times out, the connection ends, and the link asks again.
        assert!(
            resumes.len() >= 2,
            "an announcement must not settle an outstanding attach — the link has to \
             keep asking, because only an accepted answer subscribes it: {resumes:?}"
        );
        assert!(
            connections >= 2,
            "the unanswered attach must time the connection out: {connections}"
        );
        for resume in &resumes[1..] {
            assert_eq!(
                resume["params"]["threadId"].as_str(),
                Some(LIFECYCLE_THREAD),
                "every attach after the announcement targets the ANNOUNCED thread, \
                 never the stale one it replaced: {resume}"
            );
        }
        // The announcement itself is still a fact, and still exactly one.
        assert_only_the_announcement_recorded(&events);
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
    /// thread, and nothing may be recorded under the noise thread. The link is not
    /// subscribed here (its resume answers not-ready), so the announcement is the whole
    /// of what it legitimately records.
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
        assert_only_the_announcement_recorded(&events);
    }

    /// This link says only the three things it claims to, **and says them with the
    /// params those three document and nothing else**. Not a check of the broker's
    /// allowlist — `codex-broker` owns and tests that table — but of this module's
    /// own contract, so a stray request added here, or a stray *field* on one, has
    /// to be added to the module doc too.
    ///
    /// The params half is the ownership guard. The ccd leg is the one the broker's
    /// allowlist trusts to be attach-only, so an `approvalPolicy`, `sandboxPolicy`
    /// or `model` riding along on a request from here is an ownership escape and
    /// not a style slip — and `send_resume`'s privilege-free exemption is exactly
    /// the claim that the resume carries no such field. Asserted as a key SET
    /// against what each method is permitted, never as a list of forbidden names:
    /// the field somebody invents tomorrow is the one a denylist misses.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_link_sends_nothing_outside_its_documented_surface() {
        let (_, _, _, seen) =
            drive(ResumeAnswer::NoRollout, None, true, Duration::from_secs(2)).await;
        assert!(!seen.is_empty());
        for frame in &seen {
            let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
            // The documented surface, method and params together: being named here
            // is what entitles a method to send params at all, and the slice beside
            // it is the COMPLETE set of keys that method may carry. None of the
            // three is unconstrained.
            //
            // `initialize` carries client identity; the interior of `clientInfo` is
            // this daemon's own name and version, and an ownership field would be a
            // params key of its own, which is the level asserted here.
            let allowed: &[&str] = match method {
                "initialize" => &["clientInfo"],
                "initialized" => &[],
                "thread/resume" => &["threadId"],
                _ => panic!(
                    "the link sent {method}; its documented surface is initialize, \
                     initialized and thread/resume"
                ),
            };
            let Some(params) = frame.get("params").and_then(Value::as_object) else {
                panic!("{method} carries a params object on this wire; sent {frame}");
            };
            for key in params.keys() {
                assert!(
                    allowed.contains(&key.as_str()),
                    "the link sent {method} carrying the params key {key:?}, which is \
                     outside the {allowed:?} this method documents; a field added here \
                     leaves the ccd leg on the wire the broker trusts to be attach-only"
                );
            }
            assert_eq!(
                params.len(),
                allowed.len(),
                "{method} sends its documented params exactly — {allowed:?} — and this \
                 one sent {params:?}"
            );
        }
    }

    // ------------------------------------------- the attach, and what it recovers

    /// The five facts of the capture, whatever route each of them took.
    fn fact_keys(events: &[protocol::event::Event]) -> Vec<String> {
        events
            .iter()
            .map(|e| e.source_event_id.clone().unwrap_or_default())
            .collect()
    }

    /// **A populated answer is ACCEPTED, and what it describes is recovered — from the
    /// answer and from nothing else.**
    ///
    /// The leg answers the attach with the real captured shape and then says nothing at
    /// all: no `thread/started`, no replay. So every fact in the store arrived by being
    /// read out of the resume answer, which is the whole capability this chunk adds.
    /// The link was never announced a thread, so before the answer it was unbound and
    /// dropped the named frames it was sent — which is what makes the recovery, rather
    /// than the observation, the thing under test.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_populated_answer_is_accepted_and_its_facts_are_recovered() {
        let (events, connections, resumes, _) = drive(
            ResumeAnswer::PopulatedNoReplay,
            Some(LIFECYCLE_THREAD),
            false,
            Duration::from_secs(3),
        )
        .await;
        // **ONE connection and ONE resume.** A refusal ends the connection, so a link
        // that had not accepted this answer would be several connections deep by now
        // and would have asked again on each. This is the discriminator the live gate
        // uses too, and it is the difference between attaching and looping.
        assert_eq!(
            connections, 1,
            "an accepted answer ATTACHES: the connection is kept and never reconnects. \
             More than one connection means the answer was refused: {connections}"
        );
        assert_eq!(
            resumes.len(),
            1,
            "and nothing is re-asked once attached: {resumes:?}"
        );

        // What the answer described, and only that. The capture's `usage` fact is
        // deliberately absent: the token totals it is built from arrive as
        // notifications, this link was unbound when they went past, and the answer does
        // not carry them. Recovering a usage total nobody observed would be inventing
        // one — the recovery is honest about its own edges.
        let mut keys = fact_keys(&events);
        keys.sort();
        assert_eq!(
            keys,
            vec![
                format!("{LIFECYCLE_THREAD}:item:01a0127a-dbdd-7d11-b925-5bb0c2dac319"),
                format!(
                    "{LIFECYCLE_THREAD}:item:msg_0903e096e1c759f8016a83b4f7c4a481918c11f878e92d0c37"
                ),
                format!("{LIFECYCLE_THREAD}:thread_started"),
                format!("{LIFECYCLE_THREAD}:turn:01a0127a-d9cd-7461-84d7-6eea6d0b98a5"),
            ],
            "the recovered facts are the session's identity, both items' terminals and \
             the turn's — keyed exactly as the live path keys them, which is what makes \
             them dedup rather than double: {events:?}"
        );
        // And they carry the content, not just the keys: a recovery that landed empty
        // rows would satisfy the keys above perfectly.
        let agent = events
            .iter()
            .find(|e| e.kind == protocol::event::EventKind::AgentMessage)
            .expect("the agent's reply is recovered");
        assert_eq!(agent.payload["text"], "pong");
        assert_eq!(
            agent.turn_id.as_deref(),
            Some("01a0127a-d9cd-7461-84d7-6eea6d0b98a5"),
            "a recovered item is attributed to its turn"
        );
        let turn = events
            .iter()
            .find(|e| e.kind == protocol::event::EventKind::TurnComplete)
            .expect("the turn terminal is recovered");
        assert_eq!(turn.payload["status"], "completed");
    }

    /// **The same turn, described by the answer AND replayed as frames, is ONE set of
    /// facts — and the frames that arrive while the seed is still being written are
    /// admitted, not dropped.**
    ///
    /// Two claims, and the second is the subscription-timing one. The leg sends the
    /// answer and then, immediately, the turn's real frames — which is the wire's own
    /// ordering (A1: replay follows the response, with no end marker). Those bytes are
    /// in flight while the link is writing its seed, so if the admit filter only opened
    /// *after* the writes finished, a named frame could be read against an unbound
    /// visit and dropped. The **usage** fact is what makes that visible: it exists only
    /// on the notification route — a recovered turn deliberately contributes none, since
    /// a held total is a mid-turn snapshot under a first-wins key — so its presence says
    /// the post-answer frames were really admitted, and its absence would say they were
    /// silently discarded.
    ///
    /// The dedup claim is a claim at all only because the scripted answer is built from
    /// the capture itself: the answer's turn id and item ids are the capture's own, so
    /// the two routes mint the same keys and the store has to collapse them.
    #[tokio::test(flavor = "multi_thread")]
    async fn facts_from_the_answer_and_facts_from_the_frames_dedup_to_one() {
        let (events, _, resumes, _) = drive(
            ResumeAnswer::Populated,
            Some(LIFECYCLE_THREAD),
            false,
            Duration::from_secs(3),
        )
        .await;
        // **An answer that owed nothing asks nothing more.** This turn was already
        // finished when the answer described it, so its items came back under their real
        // ids and there is no debt to settle — the follow-up attach must stay silent.
        // Firing on every terminal instead of only on the turns an attach joined
        // mid-flight would mean a resume per turn, for ever, on any busy session.
        assert_eq!(
            resumes.len(),
            1,
            "a completed turn leaves nothing for a follow-up to recover, so the link \
             must not ask again: {resumes:?}"
        );
        assert_capture_recorded_once(&events);
        assert!(
            events
                .iter()
                .any(|e| e.kind == protocol::event::EventKind::Usage),
            "the usage fact can only come from the frames the leg sent AFTER the \
             answer, so its absence means either the attach never subscribed this \
             connection or the admit filter was still closed while the seed was being \
             written — and a frame read against an unbound visit is dropped, not \
             queued: {events:?}"
        );
    }

    /// **A running turn contributes no item fact, and the attach still happens.**
    ///
    /// The answer reports its turn `inProgress`, which is measured to mean its item ids
    /// are placeholders — `item-1`, `item-2` — while the real ids are the ones on the
    /// wire. The link must attach on it and record **nothing** keyed to a placeholder:
    /// those keys are fabrications, and two running turns would collide on them.
    ///
    /// The replay that follows carries the same turn's REAL frames, so the facts that
    /// do land are the observed ones — which is exactly the point. A build that seeded
    /// the placeholders would show `…:item:item-1` beside them.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_turns_placeholder_item_ids_are_never_recorded() {
        let (events, connections, _, _) = drive(
            ResumeAnswer::PopulatedInProgress,
            Some(LIFECYCLE_THREAD),
            false,
            Duration::from_secs(3),
        )
        .await;
        assert_eq!(
            connections, 1,
            "an in-progress turn is a shape this build reads, so the answer attaches"
        );
        for key in fact_keys(&events) {
            assert!(
                !key.contains(":item:item-"),
                "a PLACEHOLDER item id reached the store as {key}. A running turn's \
                 item ids are measured not to be the real ones, so a fact keyed to one \
                 is invented — and every running turn's first item would alias onto it"
            );
        }
        // The turn is still running, so its terminal is not described either.
        assert!(
            !fact_keys(&events)
                .iter()
                .any(|k| k.starts_with(&format!("{LIFECYCLE_THREAD}:turn:"))
                    && events
                        .iter()
                        .any(|e| e.source_event_id.as_deref() == Some(k)
                            && e.kind == protocol::event::EventKind::TurnComplete
                            && e.payload["status"] == "inProgress")),
            "a running turn must never be recorded as a turn terminal: {events:?}"
        );
        // And the attach really did subscribe: the replayed frames landed.
        assert!(
            !events.is_empty(),
            "the attach must subscribe, so the replay that follows is observed"
        );
    }

    /// **A mid-turn attach FINISHES ITSELF once the turn terminalizes.**
    ///
    /// This is the hole the follow-up attach closes, and it is worth stating precisely
    /// because it is invisible from any single frame. A link that attaches while a turn
    /// is running is told that turn is `inProgress` — whose item ids are measured
    /// placeholders — so it recovers none of that turn's items. The ones that had
    /// already finished are then unreachable from **both** directions: no live frame
    /// will carry them, because they completed before the subscription existed, and no
    /// answer will name them, because a running turn does not report real ids. Without a
    /// follow-up the link sits `Attached` and perfectly quiescent with that gap
    /// permanent — nothing in the machine ever asks again.
    ///
    /// The leg stages exactly that: the answer reports the turn running, and the replay
    /// that follows carries only the turn's tail, the userMessage having "already
    /// happened". When the turn terminalizes the link must ask **once** more, and the
    /// answer — now describing a finished turn with real ids — must complete the
    /// timeline.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_mid_turn_attach_finishes_itself_when_the_turn_terminalizes() {
        let (events, connections, resumes, _) = drive(
            ResumeAnswer::MidTurnThenCompleted,
            Some(LIFECYCLE_THREAD),
            false,
            Duration::from_secs(4),
        )
        .await;
        assert_eq!(
            connections, 1,
            "the follow-up rides the SAME connection: the link is attached and \
             subscribed, and re-handshaking would throw that away"
        );
        assert_eq!(
            resumes.len(),
            2,
            "exactly ONE follow-up — the debt is settled, not chased. A third resume \
             would mean the link re-asks on every terminal it sees, which on a busy \
             session is a request per turn for ever: {resumes:?}"
        );
        // The whole turn, one copy of each fact — including the userMessage, which the
        // live replay withheld and which only the follow-up's answer could name.
        assert_capture_recorded_once(&events);
        assert!(
            events
                .iter()
                .any(|e| e.kind == protocol::event::EventKind::UserMessage),
            "the item that finished before this link subscribed is exactly what the \
             follow-up exists to recover; without it the attach left a permanent gap: \
             {events:?}"
        );
    }

    // ------------------------------------------------ the doorbell (2e-5)

    /// **Only a turn this link WATCHED finish rings a phone.**
    ///
    /// The two scripts are the same turn reaching the store two different ways, which
    /// is what makes this a controlled pair rather than two separate assertions:
    ///
    ///   * `MidTurnThenCompleted` seeds the turn **running** — a running turn
    ///     contributes no terminal — and then replays the rest of it live, so the
    ///     `turn/completed` the link records is one it observed arrive. News, and it
    ///     rings.
    ///   * `PopulatedNoReplay` answers with the turn already **finished** and then
    ///     stays silent. The identical fact, under the identical key, recovered
    ///     rather than witnessed. Whatever it was, it was over before this daemon
    ///     was listening, and a doorbell for it would be a doorbell about the past.
    ///
    /// Nothing about the two *facts* differs — that identity is what makes recovery
    /// dedup-safe, and is asserted in `codex_adapter`'s own suite — so this is
    /// necessarily a test about which call site produced it.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_turn_this_link_watched_finish_rings_a_phone() {
        let (_, _, _, _, _, watched) = drive_watching_presence(
            ResumeAnswer::MidTurnThenCompleted,
            Some(LIFECYCLE_THREAD),
            false,
            Duration::from_secs(4),
        )
        .await;
        let watched = watched.expect("a turn observed finishing live is news worth ringing about");
        assert_eq!(
            watched.kind,
            crate::apns::PushKind::Completed,
            "the doorbell says a turn finished and nothing else: {watched:?}"
        );
        assert_eq!(
            watched.agent,
            protocol::agent::AgentKind::Codex,
            "it must carry the agent it is about, or the fan-out cannot tell which \
             phones can open it: {watched:?}"
        );

        let (events, _, _, _, _, recovered) = drive_watching_presence(
            ResumeAnswer::PopulatedNoReplay,
            Some(LIFECYCLE_THREAD),
            false,
            Duration::from_secs(3),
        )
        .await;
        // The premise, asserted rather than assumed: this run really did record the
        // same terminal. Without it the silence below would be the silence of a turn
        // that never landed at all, which proves nothing about live-only.
        assert!(
            events.iter().any(|e| {
                e.kind == protocol::event::EventKind::TurnComplete
                    && e.payload.get("status").and_then(Value::as_str) == Some("completed")
            }),
            "the premise is that the recovered run holds the SAME finished turn: {events:?}"
        );
        assert_eq!(
            recovered, None,
            "a turn recovered from a resume answer finished before anyone was \
             watching; ringing about it would announce the past: {recovered:?}"
        );

        // **The third case, and the one that needs the log's own answer.** Here the
        // seed describes the finished turn AND the leg replays it, so a live
        // `turn/completed` for it really does reach `ingest_frame` — the call site
        // that rings. What keeps it silent is that the fact was already filed:
        // `ingest` answers `None`, and a re-attach must not re-ring a turn the
        // reader was told about when it happened.
        let (replayed_events, _, _, _, _, replayed) = drive_watching_presence(
            ResumeAnswer::Populated,
            Some(LIFECYCLE_THREAD),
            false,
            Duration::from_secs(3),
        )
        .await;
        // **The premise, and only one fact can establish it.** This case is worth
        // nothing unless the live frames really did reach `ingest_frame` — silence
        // from a connection that was handed nothing would pass just as well and
        // would prove the opposite of what is claimed. The token usage settles it:
        // a `thread/resume` answer carries no totals at all (they are held, and a
        // held total is deliberately DISCARDED by the seed), so a `Usage` fact can
        // only have come off the notification wire — which means the
        // `turn/completed` beside it did too, and the silence below is the log's
        // dedup answer rather than an absence of frames.
        assert!(
            replayed_events
                .iter()
                .any(|e| e.kind == protocol::event::EventKind::Usage),
            "the premise is that the live replay REACHED the link; without the usage \
             fact this case proves nothing: {replayed_events:?}"
        );
        assert_eq!(
            replayed, None,
            "the terminal was recovered first, so the live frame that follows is a \
             duplicate of a fact already reported — not a second piece of news: \
             {replayed:?}"
        );
    }

    /// The narrowing, stated as a table: the fact's kind and the turn's status are
    /// both load-bearing, and neither alone decides it.
    #[test]
    fn the_doorbell_rings_for_a_finished_turn_and_for_nothing_else() {
        let session = SessionKey::new("01K1B3XQ8ZC0DE5FGH7JKMNPQR", "cc-1");
        let fact = |kind, payload| {
            protocol::event::PendingEvent::new(
                &session,
                kind,
                payload,
                protocol::event::Source::Codex,
            )
        };
        use protocol::event::EventKind;
        assert!(rings_the_doorbell(&fact(
            EventKind::TurnComplete,
            json!({"status": "completed"})
        )));
        for status in ["interrupted", "failed"] {
            assert!(
                !rings_the_doorbell(&fact(EventKind::TurnComplete, json!({"status": status}))),
                "{status}: a human stopped it, or it has no honest sentence — see \
                 Daemon::push_codex_turn_complete"
            );
        }
        assert!(
            !rings_the_doorbell(&fact(EventKind::TurnComplete, json!({}))),
            "a terminal with no status is malformed; it must not be read as success"
        );
        assert!(
            !rings_the_doorbell(&fact(
                EventKind::AgentMessage,
                json!({"status": "completed"})
            )),
            "a message is not a turn, whatever its payload happens to carry"
        );
    }

    /// **A live `turn/started` reaches the push gate, and kills the doorbell the
    /// last turn admitted.**
    ///
    /// `turn/started` mints no fact, so nothing in the event path could carry this:
    /// the frame is read where it arrives. The hole it closes is the one shape the
    /// parity test could not reach — turn A completes, turn B *starts* inside A's
    /// 400 ms dispatch grace, and A's "finished a turn" doorbell went out while the
    /// agent was mid-turn.
    ///
    /// **Mutation:** drop the `note_turn_start` call from `normalize_frame` and the
    /// ticket below is still valid, which is the bug.
    #[tokio::test]
    async fn a_live_turn_start_cancels_the_doorbell_the_last_turn_admitted() {
        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000000".into(),
            name: "cc-1".into(),
        };
        let (daemon, _db) = linked_daemon(&session);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 1,
                upstream_epoch: 1,
                thread_id: Some(LIFECYCLE_THREAD.to_string()),
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };

        // A turn ended a moment ago; its doorbell is waiting out the grace.
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        assert!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1)
                .is_some(),
            "the premise: the doorbell is standing before the next turn begins"
        );

        let started: Value = serde_json::from_str(&format!(
            r#"{{"method":"turn/started","params":{{"threadId":"{LIFECYCLE_THREAD}",
                "turn":{{"id":"01a0399f-0059-7900-91b0-1e70229b582d","status":"inProgress",
                "items":[],"itemsView":"notLoaded"}}}}}}"#
        ))
        .unwrap();
        conn.observe_notification(&started).await;

        assert_eq!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1),
            None,
            "the run is working again, so the completion it announces is over"
        );

        // **Another thread's turn is not this session's movement.** The D4 filter
        // rejects the frame before anything reads it, so a doorbell for this run
        // survives a turn starting on a thread this connection is not bound to.
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        let elsewhere: Value = serde_json::from_str(&format!(
            r#"{{"method":"turn/started","params":{{"threadId":"{SWITCHED_THREAD}",
                "turn":{{"id":"t-elsewhere","status":"inProgress"}}}}}}"#
        ))
        .unwrap();
        conn.observe_notification(&elsewhere).await;
        assert!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1)
                .is_some(),
            "a frame this visit does not admit is not this run moving"
        );

        // **And a `turn/started` that names NO thread is nobody's movement.**
        //
        // The visit filter admits an unnamed frame as connection-scoped, which is
        // right for a notification that has no thread and wrong for this one: a
        // turn always belongs to a thread, so a `turn/started` that cannot say
        // which is a frame whose subject cannot be established. Every shape of
        // that — missing, empty, and not-a-string — must leave the doorbell
        // standing, or a schema-invalid frame silences a completion that really
        // happened.
        for params in [
            r#"{"turn":{"id":"t-anonymous","status":"inProgress"}}"#,
            r#"{"threadId":"","turn":{"id":"t-anonymous"}}"#,
            r#"{"threadId":7,"turn":{"id":"t-anonymous"}}"#,
        ] {
            let ticket = daemon.push_gate.admit_turn_end(&session.uid);
            let anonymous: Value =
                serde_json::from_str(&format!(r#"{{"method":"turn/started","params":{params}}}"#))
                    .unwrap();
            conn.observe_notification(&anonymous).await;
            assert!(
                daemon
                    .push_gate
                    .exclusions_if_valid(&session.uid, ticket, true, 1)
                    .is_some(),
                "{params}: a turn/started that names no thread cancels nothing"
            );
        }
    }

    /// **A `/new` INSIDE THE GRACE CANCELS THE DOORBELL** (round-9 F4).
    ///
    /// The hole the test above cannot reach, because it is not about turns at all.
    /// A turn on A completes and its doorbell waits out the 400 ms grace; the
    /// operator presses `/new`. What this connection then receives is measured, and
    /// it is one frame: `thread/started` for B. When a turn begins on B the old
    /// subscription is sent only `thread/status/changed` — B's `turn/started` goes
    /// to the TUI's own connection (`fixtures/codex/thread-switch.jsonl`, step 5) —
    /// so `note_turn_start` is never reached and nothing else in the link is
    /// watching. Worse, the link's own first resume of an idle B can come back
    /// no-rollout and enter a 500 ms backoff, which outlives the grace outright.
    /// The ticket validates and the phone is told a turn finished while the agent is
    /// demonstrably working on the thread the user just opened.
    ///
    /// So the switch itself is the movement, and it is: a person pressed a key on
    /// this session. Reading `thread/status/changed` into turn state instead would
    /// be building on semantics nobody has measured — this build has never
    /// interpreted that frame, and the finding is not a reason to start.
    ///
    /// The negative half is the narrowing, and it is what stops this from being "any
    /// `thread/started` cancels": a re-announcement of the thread this connection is
    /// already on is not a switch, nobody moved, and the doorbell stands.
    ///
    /// **Mutations:** drop the `note_codex_switch` call from `observe_notification`
    /// and the first half fails; move it above the `note_switch_candidate` test so
    /// every `thread/started` cancels, and the second does.
    #[tokio::test]
    async fn a_new_thread_announced_inside_the_grace_cancels_the_doorbell() {
        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000004".into(),
            name: "cc-1".into(),
        };
        let (daemon, _db) = linked_daemon(&session);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 1,
                upstream_epoch: 1,
                thread_id: Some(LIFECYCLE_THREAD.to_string()),
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };

        // A re-announcement of the thread this connection is on. Nobody moved.
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        let same: Value = serde_json::from_str(&announce(LIFECYCLE_THREAD)).unwrap();
        conn.observe_notification(&same).await;
        assert!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1)
                .is_some(),
            "the thread this link is already on being announced again is not the \
             operator going anywhere"
        );

        // `/new`. The operator is at the machine and has moved on.
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        assert!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1)
                .is_some(),
            "the premise: the doorbell is standing before the switch arrives"
        );
        let switched: Value = serde_json::from_str(&announce(SWITCHED_THREAD)).unwrap();
        conn.observe_notification(&switched).await;
        assert_eq!(
            conn.switch_candidate.as_deref(),
            Some(SWITCHED_THREAD),
            "the premise: this frame really was read as a switch"
        );
        assert_eq!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1),
            None,
            "the reader is at the machine on a new thread; a push announcing the turn \
             they have already left describes a wait that is over"
        );
    }

    /// **A switch cancels the doorbell once, and only when it is a switch the
    /// adapter can read** (round-10 F2).
    ///
    /// The witness above proves the positive half and one negative — a
    /// re-announcement of the thread this connection is BOUND to is nobody moving.
    /// It leaves two shapes untested, and both of them cancelled:
    ///
    ///   * **The same candidate announced twice.** The candidate slot is
    ///     latest-wins and is applied by the loop, not here, so a second
    ///     `thread/started` for a thread already sitting in that slot moves
    ///     nothing: the visit is where it was, the candidate is what it was, and
    ///     the operator pressed one key, not two. A doorbell admitted between the
    ///     two frames describes a wait that is still going on.
    ///   * **A Thread body the adapter refuses.** [`frame_thread_id`] is
    ///     deliberately generous and answers from `params.thread.id` alone, while
    ///     [`CodexAdapter::thread_identity_event`] mints nothing unless `cwd`,
    ///     `path` and `cliVersion` are all present and string-valued. A frame
    ///     between the two — an id and nothing else — used to cancel a standing
    ///     doorbell on the strength of an announcement this build then declined to
    ///     record. Cancelling on evidence the fact store rejected is the doorbell
    ///     believing something the log does not hold.
    ///
    /// **Mutations**, one per half of the guard in `observe_notification`: drop the
    /// novelty test and cancel on the adapter's answer alone, and the first leg
    /// cancels again; drop the adapter's answer and cancel on [`Switch::Novel`]
    /// alone, and the second does.
    #[tokio::test]
    async fn a_switch_cancels_once_and_only_when_the_adapter_reads_it() {
        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000005".into(),
            name: "cc-1".into(),
        };
        let (daemon, _db) = linked_daemon(&session);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 1,
                upstream_epoch: 1,
                thread_id: Some(LIFECYCLE_THREAD.to_string()),
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };

        // The premise: one `/new`, which really does cancel.
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        let switched: Value = serde_json::from_str(&announce(SWITCHED_THREAD)).unwrap();
        conn.observe_notification(&switched).await;
        assert_eq!(
            conn.switch_candidate.as_deref(),
            Some(SWITCHED_THREAD),
            "the premise: the candidate slot is holding the announced thread"
        );
        assert_eq!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1),
            None,
            "the premise: the first announcement of a new thread is the operator moving"
        );

        // Leg 1: that same thread announced again. The slot already holds it.
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        conn.observe_notification(&switched).await;
        assert!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1)
                .is_some(),
            "the candidate slot already held this thread, so nothing about this link \
             moved; a doorbell admitted after the switch is describing the state the \
             switch left behind"
        );

        // Leg 2: a third thread announced with an id and nothing else. The link can
        // read the id; the adapter cannot read the Thread object.
        const BODYLESS_THREAD: &str = "01a03a55-0000-7000-8000-0000000000ff";
        let incomplete = json!({
            "method": "thread/started",
            "params": {"thread": {"id": BODYLESS_THREAD}}
        });
        assert!(
            CodexAdapter::new(session.clone())
                .ingest(&incomplete)
                .is_empty(),
            "the premise: this Thread body mints no fact at all"
        );
        assert!(
            matches!(frame_thread_id(&incomplete), FrameThread::Named(id) if id == BODYLESS_THREAD),
            "the premise: the link nonetheless reads a thread id off it"
        );
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        conn.observe_notification(&incomplete).await;
        assert!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1)
                .is_some(),
            "the announcement was refused by the adapter and recorded nowhere; a \
             doorbell must not die on evidence the fact store declined to hold"
        );
    }

    /// **The switch cancel does not wait for the write** (round-11 F1).
    ///
    /// The two witnesses above both await the ingest to completion, so both prove
    /// only that the cancellation happens *eventually*. Eventually is not the
    /// property the doorbell needs. The grace is 400 ms
    /// ([`crate::state::Daemon::dispatch_push`]) and the store's `busy_timeout` is
    /// five seconds for a second process holding the write lock, so a cancel
    /// sequenced behind the announcement's own persistence can be more than ten
    /// graces late — and a cancel that lands after the grace has expired is a stale
    /// "finished a turn" already delivered to a reader who is demonstrably somewhere
    /// else.
    ///
    /// So the writer is genuinely blocked here rather than merely slow, by the
    /// second-connection idiom the store's own migration tests use: `BEGIN
    /// IMMEDIATE` held open for the length of the assertion. The claim is then made
    /// mid-flight — the ingest future is polled, observed still pending, and the gate
    /// asked while the write it is waiting on has not happened and will not for
    /// seconds. Nothing else can produce that ordering by accident.
    ///
    /// **Mutation:** move the `note_codex_switch` call below the `record_minted`
    /// await and this fails at the gate — the ticket is still valid, because the
    /// cancel is sitting behind a lock another process holds.
    #[tokio::test]
    async fn a_switch_cancels_inside_the_grace_while_the_log_is_blocked() {
        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000006".into(),
            name: "cc-1".into(),
        };
        let (daemon, db) = linked_daemon(&session);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 1,
                upstream_epoch: 1,
                thread_id: Some(LIFECYCLE_THREAD.to_string()),
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };

        // Another process takes the write lock and keeps it. The daemon's own writer
        // now waits out `busy_timeout` — five seconds — at its next `BEGIN IMMEDIATE`.
        let holder = rusqlite::Connection::open(db.path()).expect("the same file");
        holder
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("the write lock is free until now");

        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        let switched: Value = serde_json::from_str(&announce(SWITCHED_THREAD)).unwrap();
        {
            let ingest = conn.observe_notification(&switched);
            tokio::pin!(ingest);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), &mut ingest)
                    .await
                    .is_err(),
                "the premise: the log really is blocked, so this ingest has not \
                 finished — whatever is asserted next is asserted mid-flight"
            );
            assert_eq!(
                daemon
                    .push_gate
                    .exclusions_if_valid(&session.uid, ticket, true, 1),
                None,
                "the operator is on a new thread, and knowing it needed nothing from \
                 the log; a cancel held behind a five-second write outlives the 400 ms \
                 grace and the stale ring goes out"
            );

            // Let the writer through and drain, so no blocking thread is left holding
            // a lock the next test's database has nothing to do with.
            holder.execute_batch("ROLLBACK;").expect("the lock is ours");
            drop(holder);
            ingest.await;
        }
        assert_eq!(
            conn.switch_candidate.as_deref(),
            Some(SWITCHED_THREAD),
            "and the frame was still read for everything else it says: cancelling \
             early moved the cancel, not the ingest"
        );
    }

    /// **Only the MEASURED `turn/started` shape counts as this run's movement**
    /// (round-4 F7).
    ///
    /// The frame under test is the committed capture itself, not a hand-written
    /// stand-in, and every negative below is that same frame with one field changed.
    /// That is the only arrangement that can prove both halves at once: a guard is a
    /// regression the moment it rejects something the live wire really sends, so the
    /// positive leg has to be the wire's own bytes, and the negatives have to differ
    /// from them in exactly one measured way.
    ///
    /// What was wrong: `note_turn_start` read the subject through [`frame_thread_id`],
    /// which is *documented* as generous — it accepts `params.thread.id` as well as
    /// `params.threadId`, because the admit filter and the switch announcement need it
    /// to. So a bodyless `turn/started`, or one wearing `thread/started`'s Thread
    /// object, silenced a real completion merely by spelling the bound thread's id.
    ///
    /// The measurement the guard is cut to — all 11 `turn/started` frames in
    /// `fixtures/codex/` (`first-turn`, `interrupt`, `thread-switch`): `params` keys
    /// are exactly `{threadId, turn}` on 11/11 and never carry `thread`;
    /// `params.threadId` is a non-empty string on 11/11; `params.turn.id` is a
    /// non-empty string on 11/11; `params.turn.status` is `"inProgress"` on 11/11.
    /// Three requirements, each universal on that wire, and nothing beyond them.
    ///
    /// **Mutations**, one per half of the guard: drop the turn-body test and the
    /// bodyless leg cancels again; read the subject back through [`frame_thread_id`]
    /// instead of flatly, and the chimera leg does.
    #[tokio::test]
    async fn only_the_measured_turn_started_shape_is_this_runs_movement() {
        let captured: Value = FIRST_TURN
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).expect("a captured frame is JSON"))
            .find(|f| f["method"] == "turn/started")
            .expect("the capture starts a turn");
        let bound = captured["params"]["threadId"]
            .as_str()
            .expect("the captured frame names its thread")
            .to_string();

        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000003".into(),
            name: "cc-1".into(),
        };
        let (daemon, _db) = linked_daemon(&session);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 1,
                upstream_epoch: 1,
                thread_id: Some(bound.clone()),
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };

        // **The wire's own frame must still cancel.** This is the half that makes the
        // guard a guard rather than a regression.
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        conn.observe_notification(&captured).await;
        assert_eq!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1),
            None,
            "the captured turn/started is this run going back to work"
        );

        // Every one of these names the BOUND thread — the D4 filter admits them all,
        // which is exactly why the shape has to be asked about separately. None of
        // them is a shape the app-server has ever been observed sending, so none may
        // silence a completion that really happened.
        let mut bodyless = captured.clone();
        bodyless["params"]
            .as_object_mut()
            .expect("params is an object")
            .remove("turn");

        let nested = json!({
            "method": "turn/started",
            "params": {"thread": {"id": bound, "path": "/r/t.jsonl", "cwd": "/work",
                                  "cliVersion": "0.147.0", "turns": []}}
        });

        // **The chimera, and the only reason the name is read FLATLY.** Spell the
        // subject in `thread/started`'s dialect but hang the captured turn body off
        // it, and the body test alone is satisfied — this is the one shape that
        // separates reading `params.threadId` directly from asking the generous
        // [`frame_thread_id`]. Without it the flat read would be an unwitnessed
        // narrowing.
        let mut nested_with_a_turn = captured.clone();
        nested_with_a_turn["params"] = json!({
            "thread": {"id": bound, "path": "/r/t.jsonl", "cwd": "/work",
                       "cliVersion": "0.147.0", "turns": []},
            "turn": captured["params"]["turn"].clone(),
        });

        let mut finished = captured.clone();
        finished["params"]["turn"]["status"] = json!("completed");

        let mut unnamed_turn = captured.clone();
        unnamed_turn["params"]["turn"]
            .as_object_mut()
            .expect("the turn is an object")
            .remove("id");

        let mut empty_turn_id = captured.clone();
        empty_turn_id["params"]["turn"]["id"] = json!("");

        let mut turn_not_an_object = captured.clone();
        turn_not_an_object["params"]["turn"] = json!("01a03652-fe8e-79d2-99f8-2d5e445e6d8d");

        for (what, frame) in [
            ("no turn body at all", bodyless),
            ("thread/started's nested Thread object", nested),
            (
                "a nested subject under a real turn body",
                nested_with_a_turn,
            ),
            ("a turn reported finished", finished),
            ("a turn with no id", unnamed_turn),
            ("a turn with an empty id", empty_turn_id),
            ("a turn that is not an object", turn_not_an_object),
        ] {
            let ticket = daemon.push_gate.admit_turn_end(&session.uid);
            conn.observe_notification(&frame).await;
            assert!(
                daemon
                    .push_gate
                    .exclusions_if_valid(&session.uid, ticket, true, 1)
                    .is_some(),
                "{what}: a turn/started the wire has never been measured sending \
                 cancels nothing, however faithfully it names the bound thread"
            );
        }
    }

    /// **A resume answer that finds a NOVEL turn running cancels the doorbell — an
    /// answer that finds every turn finished does not, and neither does one that
    /// merely regresses a turn this daemon watched finish.**
    ///
    /// The hole the cancellation closes is the one a live frame cannot: `turn/started`
    /// is broadcast once and never replayed, so a link that was *down* when the next
    /// turn began has no frame to read. Meanwhile the completion admitted just before
    /// the drop is sitting out its 400 ms dispatch grace, and the reconnect backoff
    /// after a long-lived connection is 250 ms — so the answer that says "still
    /// running" can land while the doorbell is still cancellable.
    ///
    /// The other two legs are the boundary that keeps it from becoming a blanket
    /// "recovery silences everything", and the third of them is what round-4 F5
    /// bought. **Novelty is a question for the LOG, not for `self.debts`.** That map
    /// gains an entry in exactly two places — `attach_from_seed`'s own seeding, and
    /// `note_terminal`, which only transitions one already there — and it starts empty
    /// on every `Connection`. So a turn watched completing normally, live and
    /// admitted, is not in it at all, and the vacancy test this gate used to run read
    /// that turn as brand new: a stale app-server snapshot describing it `inProgress`
    /// cancelled a doorbell about a completion that had really happened.
    ///
    /// The first three legs below are exactly those three answers, in the order a real
    /// link meets them, and the middle one has to name a turn the log holds no terminal
    /// for — which is why [`lifecycle_answer_novel_turn`] exists.
    ///
    /// **The fourth leg is the log's SCOPE** (round-5 F3). Turn ids are unique per
    /// thread, and the ask has to be too: a session-wide question about a turn id reads
    /// thread A's settled turn as thread B's running one and stops cancelling.
    ///
    /// **Mutations**, each killing a different leg:
    ///
    ///   * drop the `turn_terminal_filed` check, leaving the debt-map vacancy alone →
    ///     the stale leg cancels again;
    ///   * make the gate unconditional (cancel on every arm, not only `Ok(false)`) →
    ///     the stale and quiet legs both cancel;
    ///   * drop the `note_codex_turn_running` call entirely → the novel leg stops
    ///     cancelling;
    ///   * ask the log by `turn_id` alone (drop the thread from the source-event id) →
    ///     the fourth leg stops cancelling.
    #[tokio::test]
    async fn a_resume_that_finds_a_novel_turn_running_cancels_the_pending_doorbell() {
        /// A turn id the capture never produces, so nothing has ever filed its
        /// terminal: the next turn, begun while the link was down.
        const NOVEL_TURN: &str = "01a039a2-585f-7040-9771-3b334eea3b36";

        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000001".into(),
            name: "cc-1".into(),
        };
        let (daemon, _db) = linked_daemon(&session);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            // A fresh connection: it has been told nothing yet, which is what a
            // reconnect looks like when it asks.
            visit: Visit {
                generation: 1,
                upstream_epoch: 1,
                thread_id: None,
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };

        // **QUIET.** An answer describing a finished thread is a re-attach, not news.
        // It is also what sets the stale leg up: this attach is the daemon watching
        // LIFECYCLE_TURN finish and filing its terminal.
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        let settled = lifecycle_answer(1, LIFECYCLE_THREAD, |_| {});
        let seed = conn
            .adapter
            .plan_resume_seed(&settled["result"], LIFECYCLE_THREAD)
            .expect("a readable answer");
        conn.attach_from_seed(seed, LIFECYCLE_THREAD).await.unwrap();
        assert!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1)
                .is_some(),
            "a resume that finds the thread quiet has not observed the run move"
        );
        assert!(
            daemon
                .db
                .turn_terminal_filed(
                    session.uid.clone(),
                    crate::codex_adapter::turn_terminal_source_event_id(
                        LIFECYCLE_THREAD,
                        LIFECYCLE_TURN,
                    ),
                )
                .await
                .unwrap(),
            "the premise of the stale leg: that attach filed the turn's terminal"
        );
        assert!(
            !conn
                .debts
                .contains_key(&(LIFECYCLE_THREAD.to_string(), LIFECYCLE_TURN.to_string())),
            "and the premise of the BUG: a turn seen finishing leaves no debt behind, \
             so the vacancy this gate used to read says 'never heard of it'"
        );

        // **NOVEL.** The same link, one reconnect later, and the answer names a turn
        // the log holds no terminal for: the agent went back to work while the link
        // was down, and this answer is the only witness there will ever be.
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        assert!(
            !daemon
                .db
                .turn_terminal_filed(
                    session.uid.clone(),
                    crate::codex_adapter::turn_terminal_source_event_id(
                        LIFECYCLE_THREAD,
                        NOVEL_TURN,
                    ),
                )
                .await
                .unwrap(),
            "the premise: this daemon has never watched that turn finish"
        );
        let running = lifecycle_answer_novel_turn(2, LIFECYCLE_THREAD, NOVEL_TURN);
        let seed = conn
            .adapter
            .plan_resume_seed(&running["result"], LIFECYCLE_THREAD)
            .expect("a readable answer");
        conn.attach_from_seed(seed, LIFECYCLE_THREAD).await.unwrap();
        assert_eq!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1),
            None,
            "the agent is mid-turn, so the completion the doorbell announces is over"
        );

        // **STALE.** One more answer, describing the turn whose terminal this daemon
        // already filed as though it were still running — an app-server snapshot taken
        // before that completion. It regresses a settled turn, which is not the run
        // moving, and a doorbell about a completion that really happened must survive
        // it. Reachable on ONE connection with no reconnect at all, through a
        // mid-connection `launch_follow_up`.
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        let stale = lifecycle_answer_in_progress(3, LIFECYCLE_THREAD);
        assert_eq!(
            stale["result"]["thread"]["turns"][0]["id"].as_str(),
            Some(LIFECYCLE_TURN),
            "the premise: this answer is about the turn that already finished"
        );
        let seed = conn
            .adapter
            .plan_resume_seed(&stale["result"], LIFECYCLE_THREAD)
            .expect("a readable answer");
        conn.attach_from_seed(seed, LIFECYCLE_THREAD).await.unwrap();
        assert!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1)
                .is_some(),
            "an answer that rewinds a turn this daemon watched finish is a stale \
             snapshot, not movement; it must not silence a live completion"
        );

        // **ANOTHER THREAD'S TURN, WEARING THE SAME ID.** Turn ids are unique per
        // THREAD, not per session — the debt map has keyed `(thread, turn)` since
        // it was written — so a turn id the log holds a terminal for under thread A
        // says nothing about the same id under thread B. Asking the log without the
        // thread reads B's genuinely running turn as A's settled one and refuses to
        // cancel, which is the one failure direction that is unrecoverable: the
        // doorbell announces a completion the reader can no longer act on.
        const OTHER_THREAD: &str = "01a0127a-c6f4-70d1-b3a3-0742f8fd0dff";
        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        let elsewhere = lifecycle_answer_novel_turn(4, OTHER_THREAD, LIFECYCLE_TURN);
        let seed = conn
            .adapter
            .plan_resume_seed(&elsewhere["result"], OTHER_THREAD)
            .expect("a readable answer");
        conn.attach_from_seed(seed, OTHER_THREAD).await.unwrap();
        assert_eq!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1),
            None,
            "the turn the log settled was {LIFECYCLE_THREAD}'s; this one is \
             {OTHER_THREAD}'s and is running, so the run IS moving"
        );
    }

    /// **The cancellation survives a recovery whose writes all fail** (round-4 F6).
    ///
    /// The cancel used to sit at the bottom of `attach_from_seed`, after an ingest
    /// loop that returns on its first error. So the one case where the evidence is
    /// strongest — an answer that demonstrably found the run mid-turn — was also a
    /// case where a single failed insert skipped the cancel entirely and let the stale
    /// completion ticket stay valid. Whether the log accepted a write says nothing
    /// about whether the agent is working.
    ///
    /// The same reordering bounds the *latency*: every recovered fact is an await on
    /// the DB executor, and this cancellation is racing a 400 ms dispatch grace.
    /// Weighing the movement first makes the cancel cost one `turn_terminal_filed`
    /// read instead of the whole recovery.
    ///
    /// The write failure is a `BEFORE INSERT` trigger rather than a broken database,
    /// because the novelty read must still work — see [`refuse_event_writes`].
    ///
    /// **Mutation:** move the `note_codex_turn_running` call back below the ingest
    /// loop and this fails — the attach returns `Err` before ever reaching it.
    #[tokio::test]
    async fn the_cancel_survives_a_recovery_whose_writes_fail() {
        const NOVEL_TURN: &str = "01a039a2-585f-7040-9771-3b334eea3b36";

        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000002".into(),
            name: "cc-1".into(),
        };
        let (daemon, db) = linked_daemon(&session);
        refuse_event_writes(&db);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 1,
                upstream_epoch: 1,
                thread_id: None,
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };

        let ticket = daemon.push_gate.admit_turn_end(&session.uid);
        let running = lifecycle_answer_novel_turn(1, LIFECYCLE_THREAD, NOVEL_TURN);
        let seed = conn
            .adapter
            .plan_resume_seed(&running["result"], LIFECYCLE_THREAD)
            .expect("a readable answer");
        let outcome = conn.attach_from_seed(seed, LIFECYCLE_THREAD).await;

        assert!(
            outcome.is_err(),
            "the premise: a fact the answer described could not be written, so the \
             attach fails rather than rebuilding state around it"
        );
        assert_eq!(
            daemon
                .push_gate
                .exclusions_if_valid(&session.uid, ticket, true, 1),
            None,
            "the answer still found the run mid-turn, and a failed insert is no \
             evidence at all about whether the agent is working"
        );
    }

    // --------------------------------------- the inbound resolver (2e-5)

    /// **Every state the resolver can report, and what separates them.**
    ///
    /// Exhaustive here rather than through a scripted leg, because `Bound` is a
    /// transient on every healthy link — it is what a connection *is* between the
    /// announcement and the accepted resume — and a test that raced a leg for it
    /// would be a flaky test of a permanent property.
    #[test]
    fn bound_is_not_subscribed_and_the_resolver_says_which() {
        assert_eq!(
            addressee_of(None, None, None),
            CodexAddressee::Unbound { adopted: None },
            "connected and told nothing, by a link that has never adopted anything: \
             there is not even a subject yet"
        );
        // **The reconnect's first moment.** The connection is unbound — it has been
        // told nothing and had nothing accepted — but the LINK adopted th-a and has
        // just sent the resume for it. A threadless answer here sent the fleet back
        // to the registration's launch claim for the length of the round trip.
        assert_eq!(
            addressee_of(None, None, Some("th-a")),
            CodexAddressee::Unbound {
                adopted: Some("th-a".into())
            },
        );
        assert_eq!(
            addressee_of(None, None, Some("th-a")).thread_id(),
            Some("th-a"),
            "which thread the session is on: the one this link is coming back to"
        );
        assert!(!addressee_of(None, None, Some("th-a")).is_subscribed());
        // A carried adoption decides nothing once this connection is bound: it is
        // the previous connection's evidence, and reading it beside this one's
        // binding would report Subscribed for a resume nobody here has had accepted.
        assert_eq!(
            addressee_of(Some("th-a"), None, Some("th-a")),
            CodexAddressee::Bound {
                thread_id: "th-a".into()
            },
            "bound on this connection, adopted on an earlier one — not subscribed"
        );
        assert_eq!(
            addressee_of(Some("th-a"), None, None),
            CodexAddressee::Bound {
                thread_id: "th-a".into()
            },
            "announced but never resumed — the position the measured wire hands no \
             turn frames at all"
        );
        assert_eq!(
            addressee_of(Some("th-a"), Some("th-a"), None),
            CodexAddressee::Subscribed {
                thread_id: "th-a".into()
            },
            "the binding and the accepted resume agree: frames flow"
        );
        assert_eq!(
            addressee_of(Some("th-b"), Some("th-a"), None),
            CodexAddressee::Bound {
                thread_id: "th-b".into()
            },
            "a /new switch: the session is on th-b, and the subscription this link \
             still holds is for a thread it has left. Reporting Subscribed here — or \
             reporting th-a — would name an addressee for the wrong thread"
        );
        // The two accessors, on the same table, so a caller reaching for the weaker
        // question cannot accidentally get the stronger one's answer.
        assert_eq!(
            addressee_of(Some("th-b"), Some("th-a"), None).thread_id(),
            Some("th-b"),
            "which thread the session is on is the BINDING"
        );
        assert!(!addressee_of(Some("th-b"), Some("th-a"), None).is_subscribed());
        assert!(addressee_of(Some("th-a"), Some("th-a"), None).is_subscribed());
        assert_eq!(CodexAddressee::NoLink.thread_id(), None);
        assert_eq!(
            CodexAddressee::Offline { thread_id: None }.thread_id(),
            None
        );
        assert!(!CodexAddressee::Offline { thread_id: None }.is_subscribed());
        // A link between connections keeps the thread it adopted: a reconnect is
        // not a move, and the fleet would otherwise fall back to the launch claim
        // for the whole of every backoff.
        assert_eq!(
            CodexAddressee::Offline {
                thread_id: Some("th-a".into())
            }
            .thread_id(),
            Some("th-a")
        );
        assert!(
            !CodexAddressee::Offline {
                thread_id: Some("th-a".into())
            }
            .is_subscribed(),
            "knowing where it is coming back to is not being reachable"
        );
    }

    /// **A HELD switch candidate is not part of the answer, and that is the rule.**
    ///
    /// The state the table above cannot express: bound and adopted on A while B has
    /// been announced and is parked in `switch_candidate`, waiting for A's
    /// outstanding resume to settle. The published answer stays `Subscribed { A }`
    /// for as long as that takes.
    ///
    /// Which is honest rather than stale — A's frames are arriving and a decision
    /// raised on A is answered on A — and the alternative is worse: an announcement
    /// is not an adoption, B has never been resumed, and a thread with no rollout is
    /// refused on every attempt and falls back to A. Publishing B would name a
    /// thread nothing has verified as readable. See [`Connection::addressee`] for
    /// the window this leaves and why no state is invented for it.
    #[test]
    fn a_held_switch_candidate_does_not_change_what_the_link_publishes() {
        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000000".into(),
            name: "cc-1".into(),
        };
        let (daemon, _db) = linked_daemon(&session);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 1,
                upstream_epoch: 0,
                thread_id: Some(LIFECYCLE_THREAD.to_string()),
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: None,
            carried_target: None,
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: Some(LIFECYCLE_THREAD.to_string()),
        };
        let subscribed_to_a = CodexAddressee::Subscribed {
            thread_id: LIFECYCLE_THREAD.to_string(),
        };
        assert_eq!(conn.addressee(), subscribed_to_a);

        let announced: Value = serde_json::from_str(&announce(SWITCHED_THREAD)).unwrap();
        assert_eq!(
            conn.note_switch_candidate(&announced),
            Switch::Novel,
            "a different thread while bound and adopted is held as a candidate"
        );
        assert_eq!(
            conn.switch_candidate.as_deref(),
            Some(SWITCHED_THREAD),
            "the premise: B is known and parked"
        );
        assert_eq!(
            conn.addressee(),
            subscribed_to_a,
            "and the published answer is still the thread this link is subscribed to"
        );

        // Applying it is what moves the answer — to `Bound`, because B has been
        // bound and not yet adopted.
        conn.apply_switch_candidate();
        assert_eq!(
            conn.addressee(),
            CodexAddressee::Bound {
                thread_id: SWITCHED_THREAD.to_string()
            },
            "an applied switch binds B and leaves the subscription behind on A"
        );
    }

    /// **The same chase, carried past a dead connection — and it does NOT stay on
    /// A.**
    ///
    /// `thread/started` is broadcast once and never replayed, so a candidate whose
    /// connection died is carried in [`Carried::pending_candidate`] and seeded into
    /// the next connection's `switch_candidate`. That connection has bound nothing,
    /// so it opens on `Unbound { adopted: A }` and the fleet keeps naming the
    /// thread this link can actually read.
    ///
    /// **It stops naming A at the first settle.** The candidate is applied as soon
    /// as no resume is outstanding — a policy refusal of B is such a settle — and
    /// from then on the answer is `Bound { B }`: the wire named B, nothing has read
    /// it, which is exactly what [`CodexAddressee::Bound`] means. An earlier note
    /// here claimed the carried chase published `Unbound { adopted: A }` throughout;
    /// it does not, and this pins the sequence rather than the claim.
    #[test]
    fn a_carried_candidate_names_the_adopted_thread_only_until_it_is_applied() {
        let session = SessionKey {
            uid: "01JQXV9K7B0000000000000000".into(),
            name: "cc-1".into(),
        };
        let (daemon, _db) = linked_daemon(&session);
        let mut adapter = CodexAdapter::new(session.clone());
        let mut amend = AmendThrottle::default();
        // A fresh connection, seeded exactly as the reconnect seeds one: the
        // candidate B carried in, the adopted thread A carried in, nothing bound.
        let mut conn = Connection {
            daemon: &daemon,
            session: &session,
            adapter: &mut adapter,
            visit: Visit {
                generation: 2,
                upstream_epoch: 0,
                thread_id: None,
            },
            filtered: 0,
            next_id: 1,
            amend: &mut amend,
            debts: std::collections::BTreeMap::new(),
            unadopted_retries: 0,
            fallback_due: false,
            switch_candidate: Some(SWITCHED_THREAD.to_string()),
            carried_target: Some(LIFECYCLE_THREAD.to_string()),
            carried: Arc::new(Mutex::new(Carried::default())),
            adopted_thread: None,
        };

        assert_eq!(
            conn.addressee(),
            CodexAddressee::Unbound {
                adopted: Some(LIFECYCLE_THREAD.to_string())
            },
            "before the candidate is applied the fleet names the thread this link \
             adopted, not the one it is chasing"
        );

        assert_eq!(
            conn.apply_switch_candidate().as_deref(),
            Some(LIFECYCLE_THREAD),
            "the thread retired by the carried switch is the adopted one"
        );
        assert_eq!(
            conn.addressee(),
            CodexAddressee::Bound {
                thread_id: SWITCHED_THREAD.to_string()
            },
            "and from the first settle onward the answer is B — bound, unread, and \
             said so"
        );
    }

    /// **A real link publishes what it is, and an accepted resume is what makes it
    /// an addressee.**
    ///
    /// The end-to-end half of the table above: a link that resumed and was accepted
    /// reports `Subscribed` on the thread it adopted, while one that never had a
    /// thread to bind to reports `Unbound` — and neither is inferred from the
    /// events, which is what a resolver would otherwise have had to do.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_link_publishes_the_connection_state_a_resolver_reads() {
        let (_, _, _, _, subscribed, _) = drive_watching_presence(
            ResumeAnswer::PopulatedNoReplay,
            Some(LIFECYCLE_THREAD),
            false,
            Duration::from_secs(3),
        )
        .await;
        assert_eq!(
            subscribed.last().cloned(),
            Some(CodexAddressee::Subscribed {
                thread_id: LIFECYCLE_THREAD.to_string()
            }),
            "an accepted answer subscribes this connection, and that is the one state \
             a request could be addressed to"
        );

        // No hint, no announcement: the link is connected and has nothing to be about.
        let (_, _, _, _, unbound, _) =
            drive_watching_presence(ResumeAnswer::NoRollout, None, false, Duration::from_secs(2))
                .await;
        assert_eq!(
            unbound.last(),
            Some(&CodexAddressee::Unbound { adopted: None }),
            "a link with no thread has no addressee, and says so rather than \
             reporting the registration's claim as though the wire had confirmed it"
        );
    }

    /// **A dropped connection ends the addressee, not the session's place.**
    ///
    /// The leg answers one resume — so the link adopts — and then vanishes for good,
    /// leaving the link permanently in its reconnect backoff. What it publishes there
    /// has to keep the thread it adopted: the link is holding it in `Carried`, the
    /// next connection will resume it, and a threadless answer would send
    /// `Daemon::sessions` back to the registration's launch-time claim for the whole
    /// of every outage — which after a `/new` names a thread the session has left.
    /// That is the staleness the adoption exists to report away, returning on the
    /// first dropped socket.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_link_between_connections_still_knows_which_thread_it_adopted() {
        let (_, connections, _, _, offline, _) = drive_watching_presence(
            ResumeAnswer::PopulatedThenGone,
            Some(LIFECYCLE_THREAD),
            false,
            Duration::from_secs(3),
        )
        .await;
        assert_eq!(
            connections, 1,
            "the premise: the leg served once and went away"
        );
        assert_eq!(
            offline.last(),
            Some(&CodexAddressee::Offline {
                thread_id: Some(LIFECYCLE_THREAD.to_string())
            }),
            "offline is not an addressee, and is emphatically not amnesia"
        );
    }

    /// **And it does not forget across the reconnect either.**
    ///
    /// The test above stops where the link goes offline. This one walks it through
    /// what comes next, which is where the knowledge used to be dropped: a fresh
    /// connection initializes its binding and its adoption to `None`, sends
    /// `resume(A)` — so it demonstrably knows A — and then published a **threadless**
    /// `Unbound` for the whole of the round trip. `Daemon::sessions` fell back to the
    /// registration's launch-time claim for that window, which after a `/new` names a
    /// thread the session has left; the staleness the adoption exists to report away
    /// came back on every reconnect.
    ///
    /// The leg answers act one and then goes deaf, so the `Unbound` window is a
    /// resume budget wide rather than a moment to be caught. Every distinct state is
    /// sampled, because the claim is about what the link publishes *throughout*, not
    /// about where the settle budget happens to expire.
    ///
    /// **Mutation:** make `Unbound` threadless again — or pass `None` for the carried
    /// target in `Connection::addressee` — and the sequence contains a state naming
    /// no thread while the link is on one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reconnecting_link_never_publishes_less_than_it_knows() {
        let (_, connections, _, _, states, _) = drive_watching_presence(
            ResumeAnswer::PopulatedThenDeaf,
            Some(LIFECYCLE_THREAD),
            false,
            Duration::from_secs(3),
        )
        .await;
        assert!(
            connections > 1,
            "the premise: the link adopted on act one and dialled again: {states:?}"
        );
        assert!(
            states.iter().any(|s| matches!(
                s,
                CodexAddressee::Subscribed { thread_id } if thread_id == LIFECYCLE_THREAD
            )),
            "the premise: act one's resume was accepted, so there IS an adoption to \
             carry: {states:?}"
        );
        assert!(
            states
                .iter()
                .any(|s| matches!(s, CodexAddressee::Unbound { .. })),
            "the premise: the reconnect handshakes and waits on a resume nobody \
             answers, which is the window: {states:?}"
        );
        // The claim itself. Once the link has adopted a thread, nothing it publishes
        // may name a different one or none at all — offline, dialling, handshaken,
        // waiting on an answer.
        let adopted = states
            .iter()
            .position(|s| s.is_subscribed())
            .expect("checked above");
        for state in &states[adopted..] {
            assert_eq!(
                state.thread_id(),
                Some(LIFECYCLE_THREAD),
                "a link that has adopted {LIFECYCLE_THREAD} published {state:?}, which \
                 sends the fleet back to the registration's launch-time claim: {states:?}"
            );
        }
    }

    /// **A second debt falling due while the first follow-up is in flight is not lost.**
    ///
    /// This is the failure a single `follow_up_due` flag made unrepresentable-looking and
    /// was not. The loop read-and-cleared that flag *before* checking whether a request
    /// could actually be issued, and `note_terminal` had already marked the turn
    /// followed-up — so a terminal arriving while an earlier follow-up was outstanding
    /// consumed the flag against a state that could not act on it, and nothing ever
    /// re-armed it. The turn's items were then unreachable for ever, exactly as if no
    /// follow-up existed at all.
    ///
    /// The leg stages the collision precisely: two turns seeded running; A finishes, so a
    /// follow-up goes out; B's terminal is delivered while that request is still
    /// unanswered; and the answer to it still reports B running, so **only a third
    /// resume** can name B's items. If B's debt survived, it fires once the connection is
    /// attached again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_debt_falling_due_mid_follow_up_still_gets_paid() {
        let (events, connections, resumes, _) = drive(
            ResumeAnswer::TwoMidTurnDebts,
            Some(LIFECYCLE_THREAD),
            false,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            connections, 1,
            "every follow-up rides the connection the link already has"
        );
        assert_eq!(
            resumes.len(),
            3,
            "three asks: the attach, the follow-up for the turn that finished first, and \
             the follow-up for the one that finished while that request was in flight. \
             TWO means the second debt was noticed at a moment nothing could act on it \
             and then dropped — the items of that turn are unreachable from the live wire \
             (they completed before this link subscribed) and from every future answer \
             (the turn is marked followed-up), so they are gone for good: {resumes:?}"
        );
        let items: Vec<String> = events.iter().filter_map(|e| e.item_id.clone()).collect();
        assert!(
            items.iter().any(|id| id == TURN_B_USER),
            "the second turn's items were never recovered: {items:?}"
        );
        assert!(
            items.iter().any(|id| id == TURN_B_AGENT),
            "the second turn's reply was never recovered: {items:?}"
        );
        // And no placeholder ever reached a fact along the way.
        for key in fact_keys(&events) {
            assert!(
                !key.contains(":item:item-"),
                "a placeholder id was recorded: {key}"
            );
        }
        // **Bounded against a REPLAYED terminal, which is the shape that broke it.**
        //
        // After the third answer the leg sends B's terminal again — the wire replays
        // after a resume, so this is not contrived. It must buy nothing. It used to: the
        // debt lived in three sets kept disjoint by hand, and the answer still in flight
        // during B's first terminal put B back into `awaiting` while the launch moved it
        // from `owed` to `settled`, leaving it in two states at once. The replayed
        // terminal then found it awaiting and asked a fourth time. One map, one state per
        // turn, and the overlap is not expressible.
        assert_eq!(
            resumes.len(),
            3,
            "a replayed terminal for a turn whose follow-up has already been launched \
             bought another ask. Each turn gets ONE, however many times its terminal \
             comes round: {resumes:?}"
        );
    }

    /// **A stale resume answer loses to the announcement, even when it succeeds.**
    ///
    /// The link attaches to a registration hint. Before that request is answered the
    /// wire announces the real thread — so the outstanding resume is now asking about a
    /// thread this session is not. The leg then answers it, and answers it *well*: a
    /// perfectly valid populated result, about the stale thread.
    ///
    /// Nothing about that answer may be acted on. Accepting it would bind the visit to
    /// the stale thread and seed its turns under this session's uid — every fact
    /// afterwards filed under a thread the wire has already corrected, and no later
    /// frame could undo it because the binding would then be doing the filtering.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_superseded_resume_answer_cannot_bind_or_seed() {
        let (events, _, resumes, _) = drive(
            ResumeAnswer::AnnounceThenAnswerStale,
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
        assert!(
            resumes.len() >= 2,
            "the superseded answer settles nothing, so the link must ask again — under \
             the announced thread: {resumes:?}"
        );
        for resume in &resumes[1..] {
            assert_eq!(
                resume["params"]["threadId"].as_str(),
                Some(LIFECYCLE_THREAD),
                "every attach after the announcement targets the ANNOUNCED thread: \
                 {resume}"
            );
        }
        // Nothing the stale answer described may exist, and the link may not have bound
        // to the thread it was about.
        for event in &events {
            let key = event.source_event_id.clone().unwrap_or_default();
            assert!(
                key.starts_with(LIFECYCLE_THREAD),
                "a fact was filed under the STALE thread: the superseded answer was \
                 read and acted on, so this session's timeline now belongs to a thread \
                 the wire had already corrected: {key}"
            );
        }
        assert_only_the_announcement_recorded(&events);
    }

    /// **A failed insert FAILS the attach — it does not attach anyway.**
    ///
    /// The lever is a session key with an empty uid, which `store::append_in_tx`
    /// refuses outright ("refusing to append an event with no session_uid"). It stands
    /// in for any storage failure, and it is the one this suite can produce
    /// deterministically **through the real `Daemon::ingest` path** rather than by
    /// mocking it.
    ///
    /// What must follow is that the link keeps reconnecting — and, crucially, that it
    /// does so **without** a STOP-AND-AMEND report. That is the whole discriminator: a
    /// refused answer and a failed attach both look like a reconnect loop from the
    /// outside, and only the absence of the report says the answer was understood and
    /// the *write* was what failed. Attaching regardless would leave the link
    /// subscribed with its state rebuilt around facts that were never written.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_ingest_failure_fails_the_attach_rather_than_attaching_anyway() {
        // Its own thread, because `log::capture` is process-global and every other
        // test in this suite is logging into it at the same time. Selecting this
        // link's lines by a thread id nothing else uses is what makes the assertion
        // about THIS link rather than about whatever else was running.
        const UNWRITABLE_THREAD: &str = "th_INGEST_FAILS_TO_WRITE";
        let leg = ScriptedLeg::start(ResumeAnswer::PopulatedNoReplay, false);
        // A key whose uid no append can accept.
        let session = SessionKey::new("", "cc-1");
        let (daemon, _db) = linked_daemon(&SessionKey::new(protocol::uid::new().unwrap(), "cc-1"));
        crate::log::capture::install();
        let task = tokio::spawn(run(
            Arc::clone(&daemon),
            session,
            ControlLink {
                socket: leg.path.clone(),
                generation: 1,
                thread_id: Some(UNWRITABLE_THREAD.to_string()),
            },
            LinkPresence::new(),
            LinkCarry::new(),
        ));
        tokio::time::sleep(Duration::from_secs(3)).await;
        let connections = leg.connections.load(std::sync::atomic::Ordering::SeqCst);
        let resumes = leg.requests("thread/resume").len();
        task.abort();
        let _ = task.await;
        let captured = crate::log::capture::drain();
        crate::log::capture::uninstall();

        assert!(
            connections >= 2 && resumes >= 2,
            "an attach whose facts could not be written must FAIL, which ends the \
             connection and re-attaches on the next one: {connections} connection(s), \
             {resumes} resume(s)"
        );
        assert!(
            !captured
                .iter()
                .any(|line| line.contains("STOP-AND-AMEND") && line.contains(UNWRITABLE_THREAD)),
            "the answer was readable — it was the WRITE that failed — so the refusal \
             report must not fire. Reporting one here would send an operator looking \
             for a wire change when the fault is storage:\n{}",
            captured.join("\n")
        );
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
