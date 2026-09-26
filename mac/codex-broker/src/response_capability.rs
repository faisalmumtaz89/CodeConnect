//! The one-use response-capability fanout registry.
//!
//! A method-less response (`{id, result}` **or** `{id, error}`) is an *approval
//! answer*: a durable policy-widening frame the client sends server→client to resolve
//! a server-initiated `serverRequest`. Because the classifier can never recognize it
//! by method (it has none — see [`crate::message`]), it is authorized against a
//! **one-use capability** granted when the broker *observes* the upstream
//! `serverRequest` that solicited it.
//!
//! ## What makes this hard (and how the design answers it)
//!
//! * **A Response frame carries only the bare `id`, no thread, no provenance.** Upstream
//!   server-request ids come from one monotonic counter per app-server process, shared by
//!   every family and thread and not reused across threads (measured: an approval on one
//!   thread was id 0, the same approval on a `/new` thread id 1; upstream's
//!   `OutgoingMessageSender` holds one process-wide `AtomicI64`). The response still names no thread, so the registry **cannot key on the
//!   bare id alone**. It is disambiguated by a
//!   **per-leg view** ([`LegCapabilities`]): each connection records the `serverRequest`s
//!   observed on *its own* upstream, so on that connection an id resolves to exactly one
//!   `(thread_id, generation)` — **as long as that id is observed at most once on the leg.**
//! * **The instant a bare id is observed twice on a leg it is permanently ambiguous.** The
//!   wire carries no provenance, so once id=0 has bound one request and is then reused (or
//!   collides) the broker can no longer prove which request a `{id:0,...}` response answers.
//!   Each bare id is therefore a **per-leg 3-state** ([`IdState`]): `Unseen → Bound →
//!   Tombstoned`. The FIRST observation binds; **any SECOND observation of any kind
//!   tombstones the id for the life of the leg** — it never rebinds, never evicts, and
//!   never resurrects. A `Tombstoned` (or `Unseen`) id **cannot authorize**, so *even the
//!   original binding becomes unanswerable* (fail closed). This closes the bare-id alias
//!   routes: a reverse alias (a live approval at id=0 followed by another request at id=0),
//!   a late answer to a reused id, and a duplicate after consume.
//!
//!   In the legitimate fanout each leg observes a given `(thread,id)` approval exactly once
//!   (the app-server sends one copy per leg), so tombstoning on a second observation never
//!   fires on a single genuine approval — only on a bare-id reuse, which is exactly the
//!   ambiguous case.
//! * **Every server request goes to every subscribed connection, and the FIRST answer from
//!   any of them resolves it** (MEASURED on codex 0.155.1 with two clients on one
//!   app-server: an `item/tool/requestUserInput` reached both, and the first answer was
//!   followed by `serverRequest/resolved`). So:
//!   * **the keyboard leg is handed every server request and answers what it likes.** Its
//!     view registers the phone-family approvals, so its answer to one takes the
//!     arbitration slot ([`LegCapabilities::arbitrate_tui`]); its answer to anything else
//!     forwards untouched;
//!   * **the phone leg is handed only what it may answer**: a command-execution or
//!     file-change approval on the head. Everything else is withheld — neither delivered
//!     nor answered — because any answer from this leg, even a refusal, would settle the
//!     keyboard's question for it;
//!   * **one approval, two answerers, one winner.** A **shared arbiter**
//!     ([`ResponseArbiter`], one `Arc` across all legs) holds the one-use slot keyed by
//!     `(thread_id, request_id, generation)`. The first answer reserves it and records the
//!     winner role; a later answer from either leg forwards **zero upstream bytes**. That is
//!     also what keeps a keyboard answer that lost to the phone off the wire: the request
//!     is already resolved, and a second answer to it is never sent upstream.

//! ## Observation: every id-bearing server-request frame occupies its bare id
//!
//! The invariant is: **every id-bearing server-request frame occupies its bare id** — `Bound`
//! if it is a clean phone-family approval, else `Tombstoned` — **notifications occupy
//! nothing, and only a clean `Bound` authorizes.** No id-bearing server-request frame is
//! ever silently skipped, so a later/earlier `Bound` at the same id can never be aliased
//! through a frame the observer failed to occupy.
//!
//! Concretely, for a frame that parses (NoDup ok) and carries a usable top-level `id`:
//!
//! * **Clean phone-family approval** — [`COMMAND_EXEC_APPROVAL`] or
//!   [`FILE_CHANGE_APPROVAL`], a top-level `id`, **no** `result`/`error`, and a
//!   `params.threadId` ⇒ `Bound`.
//! * **Anything else ⇒ `Tombstoned`** — a *second* occupant of an already-tracked id; a
//!   hybrid (also carries `result`/`error`); an approval missing/over-long `threadId`; any
//!   other request method. It occupies the id so it can never be aliased, but never
//!   authorizes.
//! * **Notification (`method`, no top-level `id`) ⇒ occupies nothing.** Likewise a plain
//!   method-less `{id, result|error}` response.
//!
//! An escaped method name (`"requestApproval"`) defeats a raw substring guard, so the
//! observer does **not** substring-filter as a security gate: every frame is parsed
//! (via the NoDup [`crate::message::parse_no_dup_value`]) and classified on its **decoded**
//! method, so a JSON-escaped `requestApproval` is still seen and registered/collided.
//!
//! ## The unclassifiable-frame remedy: poison the leg (closes the last miss-alias)
//!
//! An s2c frame that CANNOT be classified — NoDup-rejected (malformed / duplicate member ⇒ method
//! and id cannot be trusted) — is the one remaining under-refusal route: if such a frame were
//! actually an id-bearing request, silently skipping it would leave its bare id `Unseen`, so a
//! same-id `Bound` (before or after) could be aliased by a response meant for the unclassifiable
//! request. The remedy (codex's "poison the leg when a frame cannot be classified") is to set a
//! per-leg [`LegCapabilities::poisoned`] flag on exactly that branch; once set,
//! [`LegCapabilities::authorize`] fails closed (checked FIRST, before any view lookup) for
//! **every** id on the leg. This closes the last miss-alias **unconditionally**: no unclassifiable
//! frame can leave a bare id unoccupied and aliasable.
//!
//! On the phone leg a poisoned view closes the leg. On the keyboard leg it is logged and
//! the leg goes on delivering: its answers then forward without taking a slot, so a
//! phone could answer the same approval too — accepted, because the producer is the
//! identity-pinned app-server, which has never sent such a frame.
//!
//! This is safe *and* non-breaking for real traffic because every frame is parsed whatever its size
//! — the upstream connection already refuses any message above [`crate::upstream::ws_config`]'s
//! bound — so real frames, including a 10.4 MB `plugin/list` reply, are classified normally. The
//! poison triggers only on a malformed frame (duplicate JSON members), which the identity-verified
//! app-server (the s2c PRODUCER, enforced at launch) never emits. If it ever fires it is a **SAFE
//! over-refusal**: the leg's approvals become unanswerable (the leg can reconnect), never an
//! under-refusal. A frame that PARSES cleanly but is merely a notification (no top-level id) or a
//! plain response (no method) is classified and occupies nothing, exactly as before — it does NOT
//! poison.
//!
//! ## Documented residuals (fail-closed, out of the practical threat model)
//!
//! * **Cross-leg divergent legs.** The tombstone is **per leg**. Two *different* approvals
//!   that happen to share a bare id on two different legs (e.g. thread-A id=0 on the TUI
//!   leg, thread-B id=0 on the ccd leg) are each a clean `Bound` on their own leg and both
//!   answerable — which is correct, they are genuinely different approvals. The *same
//!   logical approval* fanned to both legs is the arbiter's job (one winner across legs).
//!   A canonical cross-leg incarnation of a bare id is the generation seam (below).
//!   **Full cross-leg soundness is NOT solved here:** it additionally requires a single
//!   continuously-lived [`LegCapabilities`] per leg across leg recreation (so a recreated
//!   leg does not forget its tombstones), a sealed switch, and a shared arbiter identity
//!   with convergent cross-leg fanout keys. Leg recreation forgetting tombstones and
//!   divergent fanout keys are not handled by this module; they are what cross-leg
//!   soundness still depends on.
//!
//! ## Bounded retention
//!
//! The per-leg view is capped at [`MAX_TRACKED_IDS`] distinct ids (a new id beyond it is
//! not inserted, so it stays `Unseen` ⇒ unanswerable — fail closed); the shared arbiter's
//! winners map is capped at [`MAX_WINNERS`] (beyond it `consume` fails closed). Ambiguity
//! (tombstone / id-cap) log lines are rate-limited by [`AMBIGUITY_LOG_BUDGET`] so a flood
//! of colliding ids cannot unbounded-log.
//!
//! ## Seams (deliberately NOT built here)
//!
//! * **Generation stamping — the seam that LIFTS the tombstone / over-refusal.** The
//!   upstream-request key carries a `generation` so an A→B→A revisit's stale answer can
//!   never pose as the current visit's. Until a live epoch is stamped there is exactly
//!   **one** generation ([`GENERATION_UNSTAMPED`]); *within* a generation `(thread_id,
//!   request_id)` is unique and complete, so this module is correct as-is. Until then,
//!   **any ambiguous bare id is permanently fail-closed** (tombstoned ⇒ unanswerable).
//!   Generation stamping makes the key `(thread, id, generation)` unique *per visit* and,
//!   with per-incarnation id disambiguation, makes a reused id **answerable
//!   again as a distinct incarnation** — that is what lifts the tombstone and the
//!   over-refusal. Until then exactly one approval per `(leg, bare-id)` is answerable, and
//!   the ccd command path is gated, so the over-refusal has no live impact. No
//!   switch/revisit/generation-transition logic is built here.
//! * **Winner-signal injection.** The winner role is recorded and logged (provenance),
//!   but the broker→ccd side-channel that *injects* it is not built — the relay has no
//!   injection path yet, so nothing is injected here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::allowlist::Role;
use crate::message::RequestId;
use crate::relay::EventSink;
use crate::session::ThreadBinding;

/// The two phone-supported approval `serverRequest` methods (server→client requests),
/// confirmed against the captured 0.147 frames in `fixtures/codex/` (see this module's
/// tests). Either leg may answer one; the first answer wins.
pub const COMMAND_EXEC_APPROVAL: &str = "item/commandExecution/requestApproval";
/// File-change approval — the second phone-supported family (see [`COMMAND_EXEC_APPROVAL`]).
pub const FILE_CHANGE_APPROVAL: &str = "item/fileChange/requestApproval";

/// The visit generation until a live connection/delivery epoch is stamped. Exactly one
/// generation exists today, so this constant is the seam generation stamping replaces.
pub const GENERATION_UNSTAMPED: u64 = 0;

/// Bounded retention: the max number of distinct bare ids tracked per leg. Server-request
/// ids are one counter per app-server process, so a real leg sees one id per server
/// request it is sent — a few per turn; this cap only
/// prevents unbounded growth under a hostile/broken upstream. A new id beyond the cap is
/// not inserted, so it stays `Unseen` ⇒ unanswerable (fail closed).
const MAX_TRACKED_IDS: usize = 4096;

/// Bounded retention for the shared arbiter: the max number of consumed winner slots per
/// session. One slot per genuine approval; approvals are user-driven and single-generation
/// today, so a session never approaches this. Beyond it, `consume` fails closed (no winner
/// recorded ⇒ zero bytes).
const MAX_WINNERS: usize = 64 * 1024;

/// How many per-leg ambiguity (tombstone / id-cap) log lines to emit before suppressing, so
/// a flood of colliding ids cannot unbounded-log. The last emitted line is marked
/// suppressed.
const AMBIGUITY_LOG_BUDGET: u32 = 64;

/// Bounded retention: the maximum stored `threadId` length. Real codex thread ids are
/// UUIDs (~36 chars), so this is orders of magnitude of slack; an approval whose
/// `threadId` exceeds it is **tombstoned** (the id is occupied but stores no string), so
/// per-entry memory stays bounded even under a hostile/broken upstream. This closes the
/// only unbounded per-entry field — the id key and grant are already fixed-size.
const MAX_THREAD_ID_BYTES: usize = 256;

/// Authorization oracle for a phone's method-less response (an approval answer).
///
/// The classifier consults this to decide whether a `{id, result|error}` response from the
/// phone leg matches a live one-use capability on the head. Anything else (unsolicited /
/// duplicate / another thread / losing-sibling / ambiguous-tombstoned) forwards zero bytes.
pub trait ResponseCapabilityRegistry {
    /// True iff `id` names a live phone-family approval on `head` — the thread the keyboard
    /// is on — that this answer may consume. Implementations MUST consume atomically. A
    /// `{id,error}` answer consumes the slot exactly like a `{id,result}` one.
    fn authorize(&self, id: &RequestId, head: Option<&str>) -> bool;
}

/// The deferred, fail-closed registry: no capability is ever authorized, so every
/// method-less response forwards zero upstream bytes. Retained for the unit tests that
/// do not exercise fanout (and as the `Env` default in the pure-core tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCapabilities;

impl ResponseCapabilityRegistry for NoCapabilities {
    fn authorize(&self, _id: &RequestId, _head: Option<&str>) -> bool {
        false
    }
}

/// What the relay must do with one observed server→client frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S2cDisposition {
    /// Relay it to the client unchanged.
    Deliver,
    /// **Phone leg only: do not deliver, and do not answer.** A server request the phone
    /// cannot answer — anything but a phone-family approval on the head. The app-server
    /// sends it to the keyboard's leg too, and the first answer from any connection
    /// resolves it, so this leg must neither be handed it nor answer it: the keyboard does.
    Withhold,
    /// **Phone leg only: the leg can service nothing further — close it.**
    ///
    /// Reached when an s2c frame could not be classified at all (a duplicate member), which
    /// POISONS the leg: it may have been an id-bearing request whose id could not be
    /// occupied, so from that moment [`LegCapabilities::authorize`] fails closed for every
    /// id here, and a later approval delivered to the phone would be one whose answer is
    /// guaranteed to be discarded. Also reached for an upstream frame in the broker's own
    /// `codeconnect/` namespace, which the phone's daemon would read as this broker's word.
    CloseLeg(&'static str),
}

/// The shared arbiter's one-use slot key. **Within one generation**, `(thread_id,
/// request_id)` uniquely and completely identifies an upstream server-request; the
/// `generation` field is the seam that keeps a stale revisit's answer from consuming
/// the current visit's slot (see the module docs). Private: only the arbiter and its
/// tests build one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UpstreamRequestKey {
    thread_id: String,
    request_id: RequestId,
    generation: u64,
}

/// The outcome of an atomic one-use consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Consume {
    /// This response is the first to consume the slot — its bytes forward.
    Won,
    /// The slot was already consumed (a losing-fanout sibling or a duplicate), or the
    /// arbiter is saturated and cannot safely record a winner — zero bytes.
    Lost,
}

/// **What a consumed slot has actually proven.**
///
/// The distinction that authorization alone cannot carry. Authorization happens before any
/// I/O, so a slot marked spent at that moment records "somebody was allowed to answer",
/// not "somebody answered" — and the two come apart exactly when the winner's upstream
/// write fails. Under the old single state a refused write consumed the approval for
/// ever: no answer had reached the app-server, so no `serverRequest/resolved` would ever
/// follow, and the keyboard in front of the same prompt could no longer answer it either.
///
/// So a slot is `Reserved` from the moment it is won and becomes `Confirmed` only when
/// the write behind it has been proven. The three transitions out of `Reserved` are the
/// three things the wire can do to a write, and they are deliberately not the same:
///
/// * proven written  → [`ResponseArbiter::confirm`] → `Confirmed`, nameable as the winner.
/// * proven refused  → [`ResponseArbiter::release`] → the slot is removed and the
///   approval is answerable again, because zero bytes left.
/// * neither         → the slot **stays `Reserved`**. A dropped receipt (the pump died or
///   was cancelled mid-write) proves only that nobody can say: tungstenite's `send` is a
///   feed plus a flush, so a partial write is a real state. Releasing on that would let a
///   second answer actuate a command the first may already have actuated, which is worse
///   than leaving the approval unanswerable.
///
/// A `Reserved` slot is occupied for [`ResponseArbiter::consume`] — mutual exclusion
/// holds for the whole duration of the write — but it is NOT a winner for
/// [`ResponseArbiter::winner`], which is what stops a losing sibling being told a name
/// nothing has proven yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    /// Won, write outstanding. Occupies the slot; names no winner.
    Reserved(Role),
    /// Won, and the upstream write for it completed.
    Confirmed(Role),
}

/// The shared, cross-leg fanout arbiter: the one-use slots and their winner provenance.
///
/// One `Arc<ResponseArbiter>` is shared by every connection task (like
/// [`crate::session::SessionThreads`]); the interior [`Mutex`] makes `consume` atomic
/// across the legs racing to answer the same fanned-out approval. This is the pure core
/// — no I/O, unit-testable in isolation.
#[derive(Debug, Default)]
pub struct ResponseArbiter {
    /// Consumed slots → what each consume has proven ([`SlotState`]). Presence marks the
    /// slot spent for mutual exclusion; only a `Confirmed` entry is a winner anyone may be
    /// told about (queryable via [`ResponseArbiter::winner`]). Bounded at [`MAX_WINNERS`]
    /// (fail closed beyond it).
    winners: Mutex<HashMap<UpstreamRequestKey, SlotState>>,
}

impl ResponseArbiter {
    pub fn new() -> ResponseArbiter {
        ResponseArbiter::default()
    }

    /// Atomically consume the slot for `key` on behalf of `role`, **as a reservation**.
    /// The first caller wins (the slot records `role` as [`SlotState::Reserved`]); every
    /// later caller loses. One `Mutex` section, so two legs racing the same fanned-out
    /// approval have exactly one winner. If the winners map is saturated
    /// ([`MAX_WINNERS`]) and this is a new key, no winner can be recorded, so the consume
    /// fails closed (`Lost`, zero bytes).
    ///
    /// A reservation is not yet an answer. The caller must follow it with
    /// [`ResponseArbiter::confirm`] once the write is proven, or
    /// [`ResponseArbiter::release`] once it is proven to have failed — and with neither
    /// when nothing can be proven. See [`SlotState`].
    fn consume(&self, key: &UpstreamRequestKey, role: Role) -> Consume {
        let mut winners = self.winners.lock().expect("arbiter mutex poisoned");
        // Already consumed (a losing sibling/duplicate), OR saturated so no winner can be
        // recorded: either way fail closed (zero bytes). Only a fresh key with room wins.
        //
        // **Saturation is a capacity residual, not a case the wire can reach.** It needs
        // [`MAX_WINNERS`] — 65,536 — distinct approvals recorded in ONE session, each of
        // them a decision a person made; the wire produces nothing like that, and the
        // branch exists only so the map cannot grow without bound under something broken.
        // Its report is deliberately the same as every other `Lost`: `delivered:false`
        // with NO named winner. That is not a shortfall being papered over. The absence of
        // a winner already carries exactly the right meaning — *something else settled
        // this and this broker cannot say what* — and a saturated arbiter genuinely cannot
        // say. A third disposition for it would add a wire case the daemon must branch on
        // to reach the same conclusion it reaches from the silence.
        if winners.contains_key(key) || winners.len() >= MAX_WINNERS {
            Consume::Lost
        } else {
            winners.insert(key.clone(), SlotState::Reserved(role));
            Consume::Won
        }
    }

    /// **The write behind a reservation completed: the reserver is now the winner.**
    ///
    /// Only the leg that reserved the slot holds the receipt for its write, so only it
    /// ever calls this, and it calls it for the one key it just won. Guarded on
    /// `Reserved(role)` all the same, so a confirmation can never overwrite somebody
    /// else's settled slot or resurrect one that was released.
    fn confirm(&self, key: &UpstreamRequestKey, role: Role) {
        let mut winners = self.winners.lock().expect("arbiter mutex poisoned");
        if let Some(slot @ SlotState::Reserved(_)) = winners.get(key) {
            if *slot == SlotState::Reserved(role) {
                winners.insert(key.clone(), SlotState::Confirmed(role));
            }
        }
    }

    /// **The write behind a reservation was refused: the approval is answerable again.**
    ///
    /// Zero bytes reached the app-server, so nothing was answered and nothing will
    /// resolve the request — the slot must go back, or the keyboard in front of the same
    /// prompt is locked out of an approval nobody answered.
    ///
    /// Same `Reserved(role)` guard as [`ResponseArbiter::confirm`], and for the sharper
    /// reason: a release that could remove a `Confirmed` entry would un-spend a slot whose
    /// answer really did actuate.
    fn release(&self, key: &UpstreamRequestKey, role: Role) {
        let mut winners = self.winners.lock().expect("arbiter mutex poisoned");
        if winners.get(key) == Some(&SlotState::Reserved(role)) {
            winners.remove(key);
        }
    }

    /// The **confirmed** winner of a slot, if one has been proven.
    ///
    /// Read on the losing side: a leg whose answer forwarded zero bytes asks who did
    /// answer, so the broker can say so instead of leaving the daemon to guess. `None`
    /// for a slot nothing has consumed — including a saturated arbiter, which records no
    /// winner to report (see [`ResponseArbiter::consume`]).
    ///
    /// **`Reserved` names nobody.** A reservation is a leg that was allowed to answer and
    /// whose bytes may still be in flight; reporting it as the winner would tell a losing
    /// phone "the Mac answered this" on the strength of an authorization, which is the
    /// claim the reservation exists to stop making. Only a `Confirmed` slot has an
    /// answerer to name, and a loser told nothing keeps the meaning it can act on:
    /// *something else settled this and this broker cannot say what.*
    fn winner(&self, key: &UpstreamRequestKey) -> Option<Role> {
        match self
            .winners
            .lock()
            .expect("arbiter mutex poisoned")
            .get(key)
        {
            Some(SlotState::Confirmed(role)) => Some(*role),
            Some(SlotState::Reserved(_)) | None => None,
        }
    }
}

/// What a cleanly-bound (observed exactly once) server-request id maps to on **this**
/// connection.
#[derive(Debug, Clone)]
struct LegEntry {
    thread_id: String,
    generation: u64,
}

/// The per-leg lifecycle of one bare server-request id. A bare Response frame carries only
/// the id (no thread, no provenance), so the instant an id is observed twice on a leg the
/// broker can no longer prove which request a `{id,...}` response answers.
///
/// * **Unseen** — no entry in the view (absence); cannot authorize.
/// * **`Bound`** — observed exactly once; resolves to one `(thread, grant, generation)` and
///   can authorize a matching response (subject to the role gate + one-use arbiter).
/// * **`Tombstoned`** — occupied by a request this leg may not answer, or observed a SECOND
///   time (any reuse/collision). Never authorizes, never rebinds, never resurrects — even
///   an original binding becomes unanswerable (fail closed).
#[derive(Debug, Clone)]
enum IdState {
    Bound(LegEntry),
    Tombstoned,
}

/// How a method-bearing, id-bearing s2c frame OCCUPIES its bare response id. Every such
/// frame occupies the id — that is what makes the bare-id space fully fail-closed. Either it
/// is a clean, answerable server-request ([`Occupancy::Bind`]), or it is an id-bearing frame
/// we can never answer ([`Occupancy::Tombstone`]) — a hybrid (also carries `result`/`error`),
/// an approval missing/over-long `threadId`, or a method that is not a confirmed answerable
/// family — in which case it still occupies the id so a later/earlier `Bound` at the same id
/// can never be aliased through it.
enum Occupancy {
    Bind { thread_id: String },
    Tombstone,
}

/// A single leg's (connection's) response-capability view over the shared arbiter.
///
/// Owned by one connection task. It observes that leg's s2c stream
/// ([`LegCapabilities::observe_server_frame`]) to learn which server-requests are live on
/// this connection, then authorizes a c2s response against the shared [`ResponseArbiter`].
/// This is the transport-edge adapter around the pure arbiter: it holds the audit sink so
/// a won capability logs its winner-provenance.
pub struct LegCapabilities {
    arbiter: Arc<ResponseArbiter>,
    /// This leg's observed server-requests, keyed by bare id, each a 3-state [`IdState`].
    /// The first observation of an id `Bound`s it; **any second observation of any kind
    /// `Tombstone`s it for the life of the leg** — never rebind, never evict, never
    /// resurrect — so an ambiguous bare id is permanently unanswerable (fail closed).
    /// Bounded at [`MAX_TRACKED_IDS`] distinct ids. See the module docs for the generation seam.
    view: HashMap<RequestId, IdState>,
    log: EventSink,
    /// Remaining ambiguity (tombstone / id-cap) log lines before suppression.
    log_budget: u32,
    /// Leg-wide poison flag (default `false`). Set when an s2c frame is genuinely
    /// UNCLASSIFIABLE — NoDup-rejected (malformed / duplicate-member ⇒ method/id cannot be
    /// trusted). Once set, [`authorize`] fails closed
    /// for **all** ids on this leg (checked first), because such a frame may have been an
    /// id-bearing request whose bare id we could not occupy, leaving a same-id `Bound`
    /// aliasable. This closes the last miss-alias unconditionally; a clean notification or
    /// response (which classifies and occupies nothing) never sets it.
    ///
    /// [`authorize`]: LegCapabilities::authorize
    poisoned: bool,
}

impl LegCapabilities {
    /// Build a per-leg view over the shared arbiter, logging provenance through `log`.
    pub fn new(arbiter: Arc<ResponseArbiter>, log: EventSink) -> LegCapabilities {
        LegCapabilities {
            arbiter,
            view: HashMap::new(),
            log,
            log_budget: AMBIGUITY_LOG_BUDGET,
            poisoned: false,
        }
    }

    /// Observe one server→client frame on this leg (`role`) and, for **every** frame that
    /// occupies a bare response id, register how it occupies that id: a clean phone-family
    /// approval `Bind`s the id; any other id-bearing frame `Tombstone`s it.
    ///
    /// **The keyboard leg is handed everything** ([`S2cDisposition::Deliver`] always): its
    /// view only has to know which ids the arbiter keys.
    ///
    /// **The phone leg is handed only what it may answer.** A server request is delivered
    /// iff it is a clean phone-family approval on the head — the thread the keyboard is on,
    /// read from `threads` — and withheld otherwise. A poisoned view, or an upstream frame in
    /// the broker's own namespace, closes the leg.
    ///
    /// An UNCLASSIFIABLE frame (NoDup-rejected: unparseable, or a duplicate member ⇒ method/id
    /// untrustworthy) **poisons the view**: it could be an id-bearing request whose bare id
    /// could not be occupied, so from then on [`authorize`] and
    /// [`Self::arbitrate_tui`] take no slot on this leg. Every frame is parsed whatever its
    /// size: the upstream connection already refuses any message above
    /// [`crate::upstream::ws_config`]'s bound (a `plugin/list` reply measures 10.4 MB on codex
    /// 0.153.4).
    ///
    /// [`authorize`]: LegCapabilities::authorize
    pub fn observe_server_frame(
        &mut self,
        role: Role,
        threads: &crate::session::SessionThreads,
        text: &str,
    ) -> S2cDisposition {
        let keyboard = role == Role::Tui;
        if self.poisoned {
            return if keyboard {
                S2cDisposition::Deliver
            } else {
                S2cDisposition::CloseLeg("the capability leg is poisoned")
            };
        }
        // Parse with the same duplicate-member discipline the c2s classifier uses: a frame
        // with a duplicate member (at any nesting) is ambiguous between our parse and the
        // app-server's, so it yields no trustworthy method/id.
        let Some(v) = crate::message::parse_no_dup_value(text) else {
            self.poison("NoDup-rejected s2c frame (unparseable / duplicate member)");
            return if keyboard {
                S2cDisposition::Deliver
            } else {
                S2cDisposition::CloseLeg(
                    "an unparseable or duplicate-member s2c frame could not be classified",
                )
            };
        };
        // **The broker's own namespace, refused at the origin on the phone leg.** The whole
        // value of `codeconnect/` is that this broker is its only author, so an upstream frame
        // wearing the prefix would be read by `ccd` as this broker's own word about a request
        // it is still waiting on. The keyboard is a real Codex client and reads it as the
        // unknown method it is.
        if !keyboard
            && v.get("method")
                .and_then(|m| m.as_str())
                .is_some_and(|m| m.starts_with(crate::relay::CODECONNECT_NAMESPACE))
        {
            self.poison("s2c frame in the broker's reserved `codeconnect/` namespace");
            return S2cDisposition::CloseLeg(
                "an s2c frame claimed the broker's reserved `codeconnect/` namespace",
            );
        }
        // Only a frame that occupies a bare RESPONSE id concerns us: a top-level `id` AND a
        // `method` make it a server→client request (or an id-bearing hybrid). A notification
        // or a plain response occupies nothing.
        let (Some(id), Some(method)) = (
            v.get("id").and_then(RequestId::from_value),
            v.get("method").and_then(|m| m.as_str()),
        ) else {
            return S2cDisposition::Deliver;
        };
        let mut occupancy = Self::classify_request(&v, method);
        // **What the phone is not shown, it cannot answer.** An approval on a thread that is
        // not the head occupies its id on this leg without binding it, so a phone that
        // later names the id — after the keyboard moved to that thread — answers nothing.
        if !keyboard {
            if let Occupancy::Bind { thread_id } = occupancy {
                occupancy = if threads.bound_thread().as_deref() == Some(thread_id.as_str()) {
                    Occupancy::Bind { thread_id }
                } else {
                    Occupancy::Tombstone
                };
            }
        }
        self.register(id.clone(), occupancy, method);
        if keyboard {
            return S2cDisposition::Deliver;
        }
        // `register` tombstones a SECOND occupant of an id even when this frame classified
        // cleanly, so the view is what decides.
        if matches!(self.view.get(&id), Some(IdState::Bound(_))) {
            S2cDisposition::Deliver
        } else {
            (self.log)(&format!(
                "capability withheld from the phone (the keyboard answers it): id={id:?} \
                 method={method:?}"
            ));
            S2cDisposition::Withhold
        }
    }

    /// Classify a method-bearing, id-bearing s2c frame into how it occupies its bare id.
    /// Every such frame occupies the id; the only question is `Bind` vs `Tombstone`.
    ///
    /// Only the two phone-family approvals bind: they are the requests two parties may
    /// answer, and so the only ones with a slot to arbitrate. Every other server request —
    /// `item/tool/call`, `item/tool/requestUserInput`, `mcpServer/elicitation/request`, any
    /// other `*/requestApproval`, anything codex adds — is the keyboard's alone, and on this
    /// leg only occupies its id.
    fn classify_request(v: &serde_json::Value, method: &str) -> Occupancy {
        // Keyed in the arbiter by its `params.threadId`; one over the stored-id memory bound
        // is not stored.
        let thread = v
            .get("params")
            .and_then(|p| p.get("threadId"))
            .and_then(|t| t.as_str())
            .filter(|t| t.len() <= MAX_THREAD_ID_BYTES)
            .map(str::to_string);
        // A method-bearing frame that ALSO carries a response discriminant is a hybrid, not a
        // clean request. It still occupies the id, but it can never be answered.
        let hybrid = v.get("result").is_some() || v.get("error").is_some();
        match thread {
            Some(thread_id)
                if !hybrid
                    && (method == COMMAND_EXEC_APPROVAL || method == FILE_CHANGE_APPROVAL) =>
            {
                Occupancy::Bind { thread_id }
            }
            _ => Occupancy::Tombstone,
        }
    }

    /// Apply one occupant to the 3-state view. A second occupant of an already-tracked id
    /// (any kind) → `Tombstoned` (permanent ambiguity). A first occupant → `Bound` if it is a
    /// clean answerable family, else `Tombstoned` (it still occupies the id). A new id beyond
    /// [`MAX_TRACKED_IDS`] is not inserted (stays `Unseen` ⇒ unanswerable), so the view is
    /// code-bounded.
    ///
    /// Exception safety: the `Tombstoned` state is committed to the view **before** the
    /// log/event sink runs, so a panicking sink can never leave the old `Bound` alive.
    fn register(&mut self, id: RequestId, occupancy: Occupancy, method: &str) {
        if self.view.contains_key(&id) {
            // A SECOND occupant of any kind is a collision: the bare id is now permanently
            // ambiguous on this leg. Install Tombstoned FIRST, then log.
            self.view.insert(id.clone(), IdState::Tombstoned);
            self.log_ambiguous("tombstoned (ambiguous bare id)", &id, method);
            return;
        }
        if self.view.len() >= MAX_TRACKED_IDS {
            // Bounded retention: refuse to grow. A new id past the cap stays Unseen ⇒
            // unanswerable (fail closed).
            self.log_ambiguous("skipped (per-leg id cap reached)", &id, method);
            return;
        }
        match occupancy {
            Occupancy::Bind { thread_id } => {
                self.view.insert(
                    id,
                    IdState::Bound(LegEntry {
                        thread_id,
                        generation: GENERATION_UNSTAMPED,
                    }),
                );
            }
            Occupancy::Tombstone => {
                // A first occupant that is not clean-answerable still OCCUPIES the id so it can
                // never be aliased. Install Tombstoned FIRST, then log.
                self.view.insert(id.clone(), IdState::Tombstoned);
                self.log_ambiguous("tombstoned (id-bearing non-answerable frame)", &id, method);
            }
        }
    }

    /// Emit one rate-limited ambiguity log line (Debug-escaped id/method so a control char in
    /// an observed field cannot inject into the audit stream). Suppressed once
    /// [`AMBIGUITY_LOG_BUDGET`] lines have been emitted; the last marks the suppression.
    fn log_ambiguous(&mut self, reason: &str, id: &RequestId, method: &str) {
        if self.log_budget == 0 {
            return;
        }
        self.log_budget -= 1;
        let suffix = if self.log_budget == 0 {
            " (further ambiguity logs suppressed)"
        } else {
            ""
        };
        (self.log)(&format!(
            "capability {reason}: id={id:?} method={method:?}{suffix}"
        ));
    }

    /// Poison the whole leg: an UNCLASSIFIABLE (NoDup-rejected) s2c frame was
    /// observed, so it may have been an id-bearing request whose bare id we could not occupy.
    /// After this, [`Self::authorize`] fails closed for every id on this leg (checked first),
    /// closing the last miss-alias unconditionally. Idempotent, and logs exactly once per leg
    /// (on the transition), so a flood of unclassifiable frames cannot unbounded-log. `reason`
    /// is a fixed code literal (no observed field), Debug-escaped for the audit stream.
    fn poison(&mut self, reason: &str) {
        if self.poisoned {
            return;
        }
        self.poisoned = true;
        (self.log)(&format!(
            "capability leg poisoned (fail closed): {reason:?}"
        ));
    }

    /// The clean `Bound` entry for `id`, or `None` if the id is `Unseen` or `Tombstoned`.
    #[cfg(test)]
    fn bound_entry(&self, id: &RequestId) -> Option<&LegEntry> {
        match self.view.get(id) {
            Some(IdState::Bound(entry)) => Some(entry),
            _ => None,
        }
    }

    /// Is `id` tombstoned (observed twice) on this leg?
    #[cfg(test)]
    fn is_tombstoned(&self, id: &RequestId) -> bool {
        matches!(self.view.get(id), Some(IdState::Tombstoned))
    }

    /// Has this leg been poisoned by an unclassifiable s2c frame (fail closed leg-wide)?
    #[cfg(test)]
    fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// The thread a cleanly-`Bound` id belongs to, for naming the request a
    /// disposition is about.
    ///
    /// **A read, and only of what this leg was already handed.** The bare id in a
    /// response identifies the request only on the connection that was handed it,
    /// so a disposition frame quoting the id back is ambiguous to a reader that
    /// files its cards by thread. This returns the thread from the very entry
    /// [`ResponseCapabilityRegistry::authorize`] just consulted, so the frame names
    /// the same request the answer did.
    ///
    /// Deliberately `Bound`-only: a `Tombstoned` or `Unseen` id has no single
    /// thread this leg can truthfully name, and `poisoned` means no id on the leg
    /// can be trusted to name one. Those cases return `None`, and the relay then
    /// says nothing rather than guessing — the same fail-closed shape as
    /// `authorize` itself.
    pub(crate) fn bound_thread(&self, id: &RequestId) -> Option<&str> {
        if self.poisoned {
            return None;
        }
        match self.view.get(id) {
            Some(IdState::Bound(entry)) => Some(entry.thread_id.as_str()),
            _ => None,
        }
    }

    /// **Who actually answered the request this bare id names**, if anyone did.
    ///
    /// A leg whose answer forwarded zero bytes is told `delivered:false`, and that bit
    /// alone conflates four different fates: it lost to the keyboard, it lost to another
    /// phone, it never held the capability, or the arbiter had no room to record a
    /// winner. Only the first two have an answerer to name, and the arbiter is the one
    /// place that knows which. This rebuilds the slot key from the very entry
    /// [`ResponseCapabilityRegistry::authorize`] consulted — the losing consume leaves
    /// the binding in place, so the key is exactly the one that was raced for — and
    /// reports the recorded role.
    ///
    /// `None` whenever no winner is recorded, and the caller says nothing rather than
    /// inventing one. Same `Bound`-only, poison-first discipline as [`Self::bound_thread`]:
    /// an id this leg cannot truthfully speak about names no slot to ask after either.
    pub(crate) fn recorded_winner(&self, id: &RequestId) -> Option<Role> {
        if self.poisoned {
            return None;
        }
        let Some(IdState::Bound(entry)) = self.view.get(id) else {
            return None;
        };
        self.arbiter.winner(&UpstreamRequestKey {
            thread_id: entry.thread_id.clone(),
            request_id: id.clone(),
            generation: entry.generation,
        })
    }

    /// **The upstream write for the answer at `id` completed: confirm this leg's win.**
    ///
    /// Called by the relay once the pump's receipt has resolved `true`, and only on the
    /// leg that forwarded — a leg that did not forward holds no receipt. Rebuilds the slot
    /// key from the same `Bound` entry [`ResponseCapabilityRegistry::authorize`]
    /// consulted, so it confirms exactly the reservation that was taken.
    pub(crate) fn confirm_write(&self, role: Role, id: &RequestId) {
        if let Some(key) = self.slot_key(id) {
            self.arbiter.confirm(&key, role);
            (self.log)(&format!(
                "capability confirmed: winner={role:?} id={id:?} thread={:?}",
                key.thread_id
            ));
        }
    }

    /// **The upstream write for the answer at `id` was refused: put the slot back.**
    ///
    /// Called by the relay only when the pump's receipt resolved `false` — the socket
    /// refused the write and zero bytes left, so nothing answered the request and nothing
    /// will resolve it. A dropped receipt is NOT this: it proves nothing, and the relay
    /// calls neither method for it. See [`SlotState`].
    pub(crate) fn release_write(&self, role: Role, id: &RequestId) {
        if let Some(key) = self.slot_key(id) {
            self.arbiter.release(&key, role);
            (self.log)(&format!(
                "capability released: role={role:?} id={id:?} thread={:?} (write refused)",
                key.thread_id
            ));
        }
    }

    /// The arbiter slot this leg's `Bound` entry for `id` names, or `None` when this leg
    /// can truthfully name none. Same poison-first, `Bound`-only discipline as
    /// [`Self::bound_thread`] and [`Self::recorded_winner`], for the same reason: an id
    /// this leg cannot speak about names no slot either.
    fn slot_key(&self, id: &RequestId) -> Option<UpstreamRequestKey> {
        if self.poisoned {
            return None;
        }
        let Some(IdState::Bound(entry)) = self.view.get(id) else {
            return None;
        };
        Some(UpstreamRequestKey {
            thread_id: entry.thread_id.clone(),
            request_id: id.clone(),
            generation: entry.generation,
        })
    }
}

impl LegCapabilities {
    /// **Take the one-use slot for `id` on behalf of `role`.** `None` when this leg names
    /// no slot for `id` (poisoned, `Unseen` or `Tombstoned`); otherwise whether `role` won.
    fn take_slot(&self, role: Role, id: &RequestId, head: Option<&str>) -> Option<bool> {
        let key = self.slot_key(id)?;
        if head.is_some_and(|head| head != key.thread_id) {
            return Some(false);
        }
        Some(match self.arbiter.consume(&key, role) {
            Consume::Won => {
                // Debug-escape the thread id (and the already-Debug id) so a control char
                // or newline in an observed thread id cannot inject into the audit line.
                //
                // **"won" here is the RESERVATION.** The write behind it has not been
                // attempted yet; `capability confirmed` (or `capability released`) is the
                // line that says what became of it. Live gates assert this exact substring.
                (self.log)(&format!(
                    "capability won: winner={role:?} thread={:?} id={id:?} gen={}",
                    key.thread_id, key.generation
                ));
                true
            }
            // A losing-fanout sibling, a duplicate on this leg, or arbiter saturation.
            Consume::Lost => false,
        })
    }

    /// **May the keyboard's answer at `id` go upstream?** TRUE unless it answers a
    /// phone-family approval whose slot is already taken — by the phone, or by the
    /// keyboard's own earlier answer. Every other keyboard answer forwards untouched: a tool
    /// call's result, a question's reply, an answer to an id this view could not
    /// disambiguate.
    ///
    /// Why the one exception exists: every server request goes to every leg and the first
    /// answer resolves it, so once the phone's answer has won, the keyboard's is a second
    /// answer to a resolved request and stays off the wire.
    pub fn arbitrate_tui(&self, id: &RequestId) -> bool {
        self.take_slot(Role::Tui, id, None).unwrap_or(true)
    }
}

impl ResponseCapabilityRegistry for LegCapabilities {
    fn authorize(&self, id: &RequestId, head: Option<&str>) -> bool {
        // Only a CLEAN `Bound` state can authorize, on a leg that is not poisoned.
        // `Tombstoned` (observed twice ⇒ permanently ambiguous) or `Unseen` (never observed)
        // fails closed — zero bytes: a bare-id response carries no provenance to prove which
        // request it belongs to. And the approval must be on the head: a thread the
        // keyboard has left is readable from the phone, never actuable.
        head.is_some() && self.take_slot(Role::Ccd, id, head) == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(thread: &str, id: i64, generation: u64) -> UpstreamRequestKey {
        UpstreamRequestKey {
            thread_id: thread.to_string(),
            request_id: RequestId::Int(id),
            generation,
        }
    }

    fn silent() -> EventSink {
        Arc::new(|_: &str| {})
    }

    /// A session with no head. What every test of the view alone wants: the keyboard's
    /// leg registers the same way whatever the head is.
    fn no_session() -> crate::session::SessionThreads {
        crate::session::SessionThreads::new()
    }

    /// A session whose head is `thread`, bound the only way a head binds.
    fn on_head(thread: &str) -> crate::session::SessionThreads {
        let threads = crate::session::SessionThreads::new();
        let keyboard = crate::session::ConnId(1);
        let start = r#"{"method":"thread/start","id":"s","params":{}}"#;
        threads.observe_tui_request(
            keyboard,
            &crate::message::classify_shape(&crate::message::WsPayload::Text(start.into())),
        );
        threads.observe_server_frame(
            keyboard,
            &serde_json::json!({"id": "s", "result": {"thread": {"id": thread}}}).to_string(),
        );
        threads
    }

    /// The phone answers `id` on the thread this leg bound it to, as though that thread
    /// were the head.
    fn phone_answers(leg: &LegCapabilities, id: i64) -> bool {
        let id = RequestId::Int(id);
        let head = leg.bound_thread(&id).map(str::to_string);
        leg.authorize(&id, Some(head.as_deref().unwrap_or("no-thread")))
    }

    // --- Family classifier -------------------------------------------------

    // --- Shared arbiter: atomic one-use + provenance -----------------------

    /// **Reserve, then confirm: a consume is atomic and one-use, and the winner is
    /// nameable only once the write behind it is proven.**
    ///
    /// The reservation excludes the sibling for the whole duration of the write — that is
    /// the one-use half, unchanged. The other half is that `winner` stays silent until
    /// `confirm`, so a losing sibling is never told a name on the strength of an
    /// authorization.
    ///
    /// **Mutation:** have `consume` insert `Confirmed` and the middle assertion below
    /// names a winner whose bytes have not been attempted yet.
    #[test]
    fn arbiter_is_atomic_one_use_and_records_the_winner() {
        let arb = ResponseArbiter::new();
        let k = key("thread-A", 0, GENERATION_UNSTAMPED);
        assert_eq!(arb.consume(&k, Role::Ccd), Consume::Won);
        assert_eq!(
            arb.winner(&k),
            None,
            "a reservation is not yet an answer, so it names nobody"
        );
        assert_eq!(arb.consume(&k, Role::Tui), Consume::Lost, "sibling revoked");
        arb.confirm(&k, Role::Ccd);
        assert_eq!(
            arb.winner(&k),
            Some(Role::Ccd),
            "the proven write is the winner"
        );
        assert_eq!(
            arb.consume(&k, Role::Tui),
            Consume::Lost,
            "and a confirmed slot stays spent"
        );
        assert_eq!(arb.winner(&k), Some(Role::Ccd), "winner is unchanged");
    }

    /// **A proven-refused write puts the slot back; nothing else does.**
    ///
    /// The stranding defect at its smallest: an approval whose only answer left
    /// zero bytes must still be answerable, or the keyboard in front of it is locked out
    /// of a question nobody answered. The three negatives are the guard rails —
    /// `release` may not un-spend a confirmed win, may not act for a role that did not
    /// reserve, and is not what a dropped receipt earns (the relay simply never calls it).
    ///
    /// **Mutation:** drop the `Reserved(role)` guard in `release` and the confirmed slot
    /// below is un-spent, so an answer that actuated can be sent a second time.
    #[test]
    fn only_a_proven_refusal_releases_a_reserved_slot() {
        let arb = ResponseArbiter::new();
        let k = key("thread-A", 0, GENERATION_UNSTAMPED);

        // A foreign role cannot release somebody else's reservation.
        assert_eq!(arb.consume(&k, Role::Ccd), Consume::Won);
        arb.release(&k, Role::Tui);
        assert_eq!(arb.consume(&k, Role::Tui), Consume::Lost, "still reserved");

        // The reserver's own proven refusal does release it.
        arb.release(&k, Role::Ccd);
        assert_eq!(
            arb.consume(&k, Role::Tui),
            Consume::Won,
            "a refused write leaves the approval answerable"
        );

        // A confirmed win is never released.
        arb.confirm(&k, Role::Tui);
        arb.release(&k, Role::Tui);
        assert_eq!(arb.consume(&k, Role::Ccd), Consume::Lost);
        assert_eq!(arb.winner(&k), Some(Role::Tui));
    }

    /// **A confirmation only ever confirms the reservation that was taken.**
    ///
    /// A slot nothing reserved, and a slot reserved by the other role, are both left
    /// exactly as they were — so a stray confirm can neither invent a winner nor rewrite
    /// one.
    ///
    /// **Mutation:** drop the `Reserved(role)` guard in `confirm` and the first
    /// assertion names a winner for a slot no leg ever consumed.
    #[test]
    fn a_confirmation_cannot_invent_or_rewrite_a_winner() {
        let arb = ResponseArbiter::new();
        let k = key("thread-A", 0, GENERATION_UNSTAMPED);

        arb.confirm(&k, Role::Ccd);
        assert_eq!(
            arb.winner(&k),
            None,
            "nothing was reserved, so nothing wins"
        );

        assert_eq!(arb.consume(&k, Role::Ccd), Consume::Won);
        arb.confirm(&k, Role::Tui);
        assert_eq!(
            arb.winner(&k),
            None,
            "a role that did not reserve cannot confirm the reservation"
        );
        arb.confirm(&k, Role::Ccd);
        assert_eq!(arb.winner(&k), Some(Role::Ccd));
    }

    #[test]
    fn arbiter_slots_are_independent_across_threads_and_generations() {
        let arb = ResponseArbiter::new();
        // Same bare id=0 on two different threads → two independent slots.
        let a = key("thread-A", 0, GENERATION_UNSTAMPED);
        let b = key("thread-B", 0, GENERATION_UNSTAMPED);
        assert_eq!(arb.consume(&a, Role::Tui), Consume::Won);
        assert_eq!(
            arb.consume(&b, Role::Ccd),
            Consume::Won,
            "other thread live"
        );
        // The generation field also separates slots — a gen-1 slot does not
        // collide with the gen-0 one for the same thread+id.
        let a_next = key("thread-A", 0, GENERATION_UNSTAMPED + 1);
        assert_eq!(arb.consume(&a_next, Role::Tui), Consume::Won);
    }

    // --- Per-leg authorize over the arbiter --------------------------------

    fn approval_frame(method: &str, thread: &str, id: i64) -> String {
        format!(
            r#"{{"method":"{method}","id":{id},"params":{{"threadId":"{thread}","itemId":"x"}}}}"#
        )
    }

    /// A [`COMMAND_EXEC_APPROVAL`] frame whose method's final `l` is JSON-escaped as the
    /// unicode escape `l`, so the raw bytes carry NO literal `requestApproval` marker
    /// (defeating a substring guard) yet decode to the real phone-family method. Built at
    /// runtime so the source has no fragile literal escape. `"\\u006c"` here is the six
    /// characters backslash-u-0-0-6-c.
    fn escaped_approval_frame(thread: &str, id: i64) -> String {
        let method = format!(
            "{}\\u006c",
            &COMMAND_EXEC_APPROVAL[..COMMAND_EXEC_APPROVAL.len() - 1]
        );
        format!(
            r#"{{"method":"{method}","id":{id},"params":{{"threadId":"{thread}","itemId":"x"}}}}"#
        )
    }

    #[test]
    fn phone_family_first_response_wins_sibling_revoked() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut ccd = LegCapabilities::new(Arc::clone(&arb), silent());
        let mut tui = LegCapabilities::new(Arc::clone(&arb), silent());
        // The same approval fans out to both legs (each observes its own upstream copy —
        // once per leg, so each leg holds a clean Bound, not a tombstone).
        let frame = approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0);
        ccd.observe_server_frame(Role::Tui, &no_session(), &frame);
        tui.observe_server_frame(Role::Tui, &no_session(), &frame);

        // ccd answers first → authorized; the TUI sibling then loses (revoked), zero bytes.
        assert!(phone_answers(&ccd, 0));
        assert!(!tui.arbitrate_tui(&RequestId::Int(0)));
        // Winner-provenance is queryable on the shared arbiter — once the write behind
        // the reservation has been proven, which is what the relay's receipt does.
        assert_eq!(
            arb.winner(&key("thread-A", 0, GENERATION_UNSTAMPED)),
            None,
            "the authorization alone names nobody"
        );
        ccd.confirm_write(Role::Ccd, &RequestId::Int(0));
        assert_eq!(
            arb.winner(&key("thread-A", 0, GENERATION_UNSTAMPED)),
            Some(Role::Ccd)
        );
    }

    /// **A poisoned phone leg CLOSES; it does not go on delivering.**
    ///
    /// Poisoning makes `authorize` fail closed for every id here — so a later clean
    /// approval would still register `Bound`, still be delivered, and then have its answer
    /// discarded: the phone would be handed a question whose answer is guaranteed to go
    /// nowhere.
    #[test]
    fn a_poisoned_leg_closes_instead_of_delivering_what_it_cannot_service() {
        // A duplicate member at any depth: ambiguous between our parse and the server's, so
        // it yields no trustworthy id.
        let poisoning = r#"{"id":0,"method":"item/commandExecution/requestApproval","params":{"threadId":"th-A","threadId":"th-B"}}"#;
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        assert!(
            matches!(
                leg.observe_server_frame(Role::Ccd, &no_session(), poisoning),
                S2cDisposition::CloseLeg(_)
            ),
            "an unclassifiable frame must close the leg"
        );
        assert!(leg.is_poisoned());

        // The frame that used to recreate the hang: a perfectly clean approval,
        // arriving after the poison. It must NOT be delivered.
        let clean = approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0);
        assert!(
            matches!(
                leg.observe_server_frame(Role::Ccd, &no_session(), &clean),
                S2cDisposition::CloseLeg(_)
            ),
            "a poisoned leg must not deliver an exchange whose answer it will discard"
        );
        // …and its answer would indeed have been discarded, which is the whole point.
        assert!(!phone_answers(&leg, 0));
    }

    /// **The reserved namespace is closed to upstream on the phone leg, whatever the method
    /// under it.**
    ///
    /// The gate is on the PREFIX, not on the one method the broker composes today: a
    /// namespace whose guarantee is "this broker is its only author" is worth nothing if
    /// the next method added to it arrives unguarded. Both frame shapes are covered
    /// because the response disposition is a notification — a frame that carries no
    /// top-level id and would otherwise be waved through before any id-bearing check
    /// runs.
    ///
    /// **Mutation:** match the exact `codeconnect/responseDisposition` instead of the
    /// prefix, and every other method in the namespace passes through unexamined.
    #[test]
    fn any_method_in_the_reserved_namespace_closes_the_phone_leg() {
        for forged in [
            // The forgery itself: the broker's own frame, notification-shaped.
            r#"{"method":"codeconnect/responseDisposition","params":{"threadId":"th-A","requestId":0,"delivered":true}}"#,
            // Any other method under the prefix, id-bearing this time.
            r#"{"id":0,"method":"codeconnect/anythingElse","params":{"threadId":"th-A"}}"#,
        ] {
            let arb = Arc::new(ResponseArbiter::new());
            let mut leg = LegCapabilities::new(arb, silent());
            assert!(
                matches!(
                    leg.observe_server_frame(Role::Ccd, &no_session(), forged),
                    S2cDisposition::CloseLeg(_)
                ),
                "an upstream frame in the reserved namespace must close the leg: {forged}"
            );
            assert!(leg.is_poisoned(), "and nothing on it may authorize again");
        }
    }

    /// The one frame the broker composes lives inside the prefix the gate defends. If
    /// these two ever drift apart the gate stops guarding the thing it exists for.
    #[test]
    fn the_composed_frame_lives_inside_the_reserved_namespace() {
        assert!(
            crate::relay::RESPONSE_DISPOSITION.starts_with(crate::relay::CODECONNECT_NAMESPACE),
            "the disposition method must sit under the namespace the s2c gate reserves"
        );
    }

    /// **The loser can name the winner — and a leg that can name nothing names no
    /// winner either.**
    ///
    /// The losing consume leaves the binding in place, which is what lets the losing leg
    /// rebuild the slot key it raced for and ask the arbiter who took it. A poisoned leg
    /// is the counterweight: no id on it resolves to a request it can truthfully speak
    /// about, so it reports no winner rather than one read out of a view it no longer
    /// trusts — the same discipline `bound_thread` and `authorize` already keep.
    ///
    /// **Mutation:** drop the poison gate in `recorded_winner` and a leg whose whole view
    /// is untrustworthy still names an answerer.
    #[test]
    fn the_losing_leg_names_the_winner_and_a_poisoned_leg_names_none() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut ccd = LegCapabilities::new(Arc::clone(&arb), silent());
        let mut tui = LegCapabilities::new(Arc::clone(&arb), silent());
        let solicits = approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0);
        ccd.observe_server_frame(Role::Tui, &no_session(), &solicits);
        tui.observe_server_frame(Role::Tui, &no_session(), &solicits);

        // Nothing has answered yet, so there is nobody to name.
        assert_eq!(ccd.recorded_winner(&RequestId::Int(0)), None);
        // The keyboard takes the slot; the phone lost it and can say so — but only once
        // the keyboard's write is proven. A reservation still in flight names nobody,
        // which is what stops a phone being told "the Mac answered" about a write that
        // may yet fail.
        assert!(tui.arbitrate_tui(&RequestId::Int(0)));
        assert!(!phone_answers(&ccd, 0));
        assert_eq!(
            ccd.recorded_winner(&RequestId::Int(0)),
            None,
            "the keyboard's bytes are still in flight"
        );
        tui.confirm_write(Role::Tui, &RequestId::Int(0));
        assert_eq!(ccd.recorded_winner(&RequestId::Int(0)), Some(Role::Tui));
        // An id this leg never observed resolves to no slot to ask after.
        assert_eq!(ccd.recorded_winner(&RequestId::Int(99)), None);

        ccd.poison("an unclassifiable frame");
        assert_eq!(
            ccd.recorded_winner(&RequestId::Int(0)),
            None,
            "a leg that can name no thread can name no winner either"
        );
    }

    /// The sibling census entry stays closed. `item/tool/requestUserInput` is an s2c
    /// request in the same pinned census and is a DIFFERENT capability (asking the user a
    /// question); no capture exercises it and nothing strands on it, so admitting the tool
    /// dispatch must not have admitted it by accident.
    #[test]
    fn the_other_non_approval_server_requests_are_still_tombstoned() {
        for method in [
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
            "attestation/generate",
        ] {
            let arb = Arc::new(ResponseArbiter::new());
            let mut tui = LegCapabilities::new(arb, silent());
            tui.observe_server_frame(Role::Tui, &no_session(), &format!(
                r#"{{"id":0,"method":"{method}","params":{{"threadId":"thread-A","namespace":"codex_tui"}}}}"#
            ));
            assert!(
                !phone_answers(&tui, 0),
                "{method} must stay unanswerable from the phone"
            );
            assert!(
                tui.arbitrate_tui(&RequestId::Int(0)),
                "and the keyboard's answer to it forwards"
            );
        }
    }

    #[test]
    fn unsolicited_response_id_is_unauthorized() {
        let arb = Arc::new(ResponseArbiter::new());
        let leg = LegCapabilities::new(arb, silent());
        // No serverRequest observed → nothing to consume.
        assert!(
            leg.arbitrate_tui(&RequestId::Int(7)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
    }

    #[test]
    fn duplicate_on_the_same_leg_is_one_use() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
        assert!(phone_answers(&leg, 0), "first wins");
        assert!(!phone_answers(&leg, 0), "second is spent");
    }

    // --- Tombstone state machine (the three bare-id alias routes) -----------

    #[test]
    fn reverse_alias_collision_makes_the_original_unanswerable() {
        // THE CRITICAL CASE. Observe phone-capable A(id=0) and do NOT answer it; then
        // observe TUI-only B reusing id=0. The second observation is a collision, so id=0
        // is tombstoned — permanently ambiguous. A ccd (or tui) response for id=0 now
        // forwards ZERO bytes: A is no longer answerable, because a bare-id response cannot
        // prove it belongs to A rather than B.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(Arc::clone(&arb), silent());
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame("item/permissions/requestApproval", "thread-B", 0),
        );
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "collision tombstones"
        );
        // Neither role can answer the tombstoned id — the reverse alias is closed.
        assert!(!phone_answers(&leg, 0));
        assert!(
            leg.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
        // No slot was ever consumed for A (it never authorized).
        assert_eq!(arb.winner(&key("thread-A", 0, GENERATION_UNSTAMPED)), None);
    }

    #[test]
    fn collision_then_original_response_forwards_zero_bytes() {
        // A second observation of the SAME (thread, id) is still a collision (any second
        // observation is), so the original is unanswerable afterward.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(Arc::clone(&arb), silent());
        let frame = approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0);
        leg.observe_server_frame(Role::Tui, &no_session(), &frame);
        leg.observe_server_frame(Role::Tui, &no_session(), &frame); // duplicate observation ⇒ collision
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(
            !phone_answers(&leg, 0),
            "original is unanswerable after a collision"
        );
    }

    #[test]
    fn consume_then_second_observe_then_duplicate_forwards_zero_bytes() {
        // Consume-vs-tombstone ordering: authorizing consumes the arbiter slot but leaves
        // the view Bound; a subsequent observe of the same id still tombstones it, so both
        // a post-consume duplicate AND any later same-id response fail closed.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(Arc::clone(&arb), silent());
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
        assert!(phone_answers(&leg, 0), "clean Bound authorizes once");
        // A later same-id observation tombstones the (already-consumed) id.
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-B", 0),
        );
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        // A post-consume duplicate response fails closed (tombstoned AND already spent).
        assert!(!phone_answers(&leg, 0));
        assert!(
            leg.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
    }

    #[test]
    fn observe_only_then_phone_same_bare_id_tombstones_both() {
        // REWRITTEN from `never_rebind_blocks_observe_only_then_phone_upgrade`, which
        // asserted the ORIGINAL tui grant still answered after a same-id reuse — that
        // never-rebind form left the original live and was the reverse-alias defect. Under
        // the tombstone model, an observe-only (TUI-only) request at id=0 followed by a
        // phone-family reuse of id=0 is a collision: id=0 is tombstoned, so BOTH ccd and tui
        // fail closed (no phone upgrade AND no original tui answer).
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(Arc::clone(&arb), silent());
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame("item/permissions/requestApproval", "th-P", 0),
        );
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-C", 0),
        );
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(!phone_answers(&leg, 0), "no phone upgrade");
        assert!(
            leg.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard's answer takes no slot on an ambiguous id: it forwards"
        );
    }

    #[test]
    fn escaped_request_approval_method_is_decoded_and_registered() {
        // An escaped method name (`l` for the final `l` of requestApproval) defeats a
        // raw substring guard but decodes to a real approval method. The observer parses
        // it, so it is recognized and registered — NOT silently skipped.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(Arc::clone(&arb), silent());
        // The final `l` is JSON-escaped (l), so the raw bytes carry no literal
        // `requestApproval` marker but decode to COMMAND_EXEC_APPROVAL.
        let frame = escaped_approval_frame("th-A", 0);
        assert!(
            !frame.contains("requestApproval"),
            "the raw bytes do NOT contain the literal marker (the escape attack)"
        );
        leg.observe_server_frame(Role::Tui, &no_session(), &frame);
        // It was decoded to the phone family and bound (treated as an approval).
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_some(),
            "the escaped approval is decoded and registered as the phone family"
        );
        assert!(phone_answers(&leg, 0));
    }

    #[test]
    fn escaped_request_approval_method_collides_correctly() {
        // The escaped form is the SAME id as a following plain approval ⇒ a collision that
        // tombstones (it is not silently skipped, which would have left the plain one live).
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(Arc::clone(&arb), silent());
        let escaped = escaped_approval_frame("th-A", 0);
        assert!(!escaped.contains("requestApproval"));
        leg.observe_server_frame(Role::Tui, &no_session(), &escaped);
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-B", 0),
        );
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "the escaped approval participates in collision detection"
        );
        assert!(!phone_answers(&leg, 0));
    }

    #[test]
    fn duplicate_member_frame_poisons_the_leg() {
        // ADAPTED from `duplicate_member_frame_is_not_registered`, which asserted only that a
        // NoDup-rejected frame does not register. It still registers nothing — but a NoDup-
        // rejected frame yields no trustworthy method/id, so it is UNCLASSIFIABLE and now
        // POISONS the leg (fail closed leg-wide), rather than being a silent skip that could
        // leave a same-id `Bound` aliasable.
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        // Duplicate top-level `id` — ambiguous between our parse and the app-server's.
        leg.observe_server_frame(Role::Ccd, &no_session(), &format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"id":1,"params":{{"threadId":"th-A"}}}}"#
        ));
        assert!(leg.is_poisoned(), "a NoDup-rejected frame poisons the leg");
        // Duplicate `method` also fails NoDup (idempotent poison, still registers nothing).
        leg.observe_server_frame(Role::Ccd, &no_session(), &format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","method":"x/requestApproval","id":2,"params":{{"threadId":"th-A"}}}}"#
        ));
        assert!(
            leg.view.is_empty(),
            "a duplicate-member frame must not register (malformed s2c)"
        );
        // A later CLEAN approval is not even DELIVERED: the leg closes. It used to bind in
        // the view and be relayed, and only its ANSWER was refused — which handed the
        // client an exchange whose reply was guaranteed to be discarded.
        assert!(matches!(
            leg.observe_server_frame(
                Role::Ccd,
                &no_session(),
                &approval_frame(COMMAND_EXEC_APPROVAL, "th-clean", 9),
            ),
            S2cDisposition::CloseLeg(_)
        ));
        assert!(leg.bound_entry(&RequestId::Int(9)).is_none());
        assert!(!phone_answers(&leg, 9));
        assert!(
            leg.arbitrate_tui(&RequestId::Int(9)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
    }

    /// A `plugin/list` reply measured at 10,371,547 bytes on codex 0.153.4 (4,620 plugins
    /// in the curated remote marketplace) is ordinary traffic: it classifies, occupies
    /// nothing answerable, and leaves the leg serving the phone's approvals.
    #[test]
    fn a_ten_megabyte_plugin_list_reply_classifies_and_keeps_the_leg_serving() {
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        let plugins = format!(
            "[{}]",
            vec![r#"{"name":"p","description":"d"}"#; 330_000].join(",")
        );
        let reply = format!(
            r#"{{"id":"plugin-list-1","result":{{"marketplaces":[{{"name":"openai-curated-remote","plugins":{plugins}}}]}}}}"#
        );
        assert!(
            reply.len() > 10_000_000,
            "the reply is the measured size: {}",
            reply.len()
        );
        assert!(matches!(
            leg.observe_server_frame(Role::Tui, &no_session(), &reply),
            S2cDisposition::Deliver
        ));
        assert!(!leg.is_poisoned());
        assert!(
            leg.view.is_empty(),
            "a reply to a client request occupies no id"
        );
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0),
        );
        assert!(phone_answers(&leg, 0));
    }

    #[test]
    fn poison_overrides_a_previously_bound_id() {
        // Poison-first ordering: an id is cleanly `Bound` first (and would authorize), THEN an
        // unclassifiable frame poisons the leg. Because `authorize` checks the poison flag
        // FIRST — before the view lookup — the previously-Bound id's response now forwards ZERO
        // bytes. Poison overrides an existing clean Bound.
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0),
        );
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_some(),
            "id=0 binds cleanly first"
        );
        // Now poison via an unclassifiable (duplicate-member) frame.
        leg.observe_server_frame(Role::Tui, &no_session(), &format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":1,"params":{{"threadId":"th-B","threadId":"th-C"}}}}"#
        ));
        assert!(leg.is_poisoned());
        // The still-Bound id=0 is now unanswerable: poison overrides the clean Bound.
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_some(),
            "the view still holds the clean Bound"
        );
        assert!(
            !phone_answers(&leg, 0),
            "but poison-first makes it forward zero bytes"
        );
        assert!(
            leg.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
    }

    #[test]
    fn hybrid_result_bearing_frame_tombstones_the_id_and_never_authorizes() {
        // ADAPTED from `hybrid_result_bearing_approval_frame_is_not_registered`, which
        // asserted the view stayed EMPTY (the old skip). A hybrid (method-bearing frame that
        // also carries `result`/`error`) is not a clean serverRequest, but it has a readable
        // top-level id, so it OCCUPIES that id ⇒ Tombstoned. It never grants a capability,
        // and a later same-id approval can never alias through it.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(Role::Tui, &no_session(), &format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"params":{{"threadId":"th-A"}},"result":{{}}}}"#
        ));
        leg.observe_server_frame(Role::Tui, &no_session(), &format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":1,"params":{{"threadId":"th-A"}},"error":{{"code":-1}}}}"#
        ));
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "result-hybrid occupies id 0"
        );
        assert!(
            leg.is_tombstoned(&RequestId::Int(1)),
            "error-hybrid occupies id 1"
        );
        assert!(!phone_answers(&leg, 0));
        assert!(
            leg.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
        // A later clean approval reusing id=0 stays tombstoned (second occupant) —
        // unanswerable, so it can never alias through the hybrid.
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-B", 0),
        );
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(!phone_answers(&leg, 0));
    }

    #[test]
    fn an_error_response_consumes_like_a_result() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
        // An error answer consumes the one-use slot like a result.
        assert!(phone_answers(&leg, 0));
        assert!(!leg.arbitrate_tui(&RequestId::Int(0)));
    }

    #[test]
    fn a_non_approval_notification_under_the_cap_occupies_no_id() {
        // ADAPTED comment: `app/list/updated` is a NOTIFICATION — it carries a `method` but
        // NO top-level `id`, so it occupies no response id and nothing registers (view stays
        // empty). This is the notification-vs-request distinction: only method+id frames
        // occupy an id. (An id-BEARING non-approval request tombstones instead — see
        // `unknown_non_approval_id_bearing_request_tombstones_the_id`.)
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        let frame = format!(
            r#"{{"method":"app/list/updated","params":{{"pad":"{}"}}}}"#,
            "Z".repeat(4096)
        );
        leg.observe_server_frame(Role::Tui, &no_session(), &frame);
        assert!(leg.view.is_empty());
    }

    #[test]
    fn per_leg_id_cap_stops_growth_and_leaves_new_ids_unanswerable() {
        // Bounded retention: fill the view to the cap with distinct ids, then a NEW id is
        // not inserted (stays Unseen ⇒ unanswerable), while an EXISTING id still tombstones.
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        for i in 0..MAX_TRACKED_IDS as i64 {
            leg.observe_server_frame(
                Role::Tui,
                &no_session(),
                &approval_frame(COMMAND_EXEC_APPROVAL, "th", i),
            );
        }
        assert_eq!(leg.view.len(), MAX_TRACKED_IDS);
        // A brand-new id past the cap is not tracked → unanswerable (fail closed).
        let over = MAX_TRACKED_IDS as i64;
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th", over),
        );
        assert_eq!(
            leg.view.len(),
            MAX_TRACKED_IDS,
            "the view does not grow past the cap"
        );
        assert!(!phone_answers(&leg, over));
        // An already-tracked id can still transition to Tombstoned (no growth).
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th2", 0),
        );
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert_eq!(leg.view.len(), MAX_TRACKED_IDS);
    }

    #[test]
    fn ambiguity_logging_is_rate_limited() {
        // A flood of colliding ids must not unbounded-log: exactly AMBIGUITY_LOG_BUDGET
        // lines are emitted, the last marked suppressed.
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink_lines = Arc::clone(&lines);
        let sink: EventSink =
            Arc::new(move |l: &str| sink_lines.lock().unwrap().push(l.to_string()));
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), sink);
        // Bind id=0, then collide it far more times than the budget.
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0),
        );
        for _ in 0..(AMBIGUITY_LOG_BUDGET as usize + 50) {
            leg.observe_server_frame(
                Role::Tui,
                &no_session(),
                &approval_frame(COMMAND_EXEC_APPROVAL, "th-B", 0),
            );
        }
        let logged = lines.lock().unwrap();
        assert_eq!(
            logged.len(),
            AMBIGUITY_LOG_BUDGET as usize,
            "ambiguity logs are capped at the budget"
        );
        assert!(
            logged
                .last()
                .unwrap()
                .contains("further ambiguity logs suppressed"),
            "the last emitted line marks suppression"
        );
    }

    #[test]
    fn arbiter_saturation_fails_closed() {
        // Beyond MAX_WINNERS distinct slots, consume cannot record a winner → Lost (zero
        // bytes), so a genuine new approval fails closed rather than growing unbounded.
        let arb = ResponseArbiter::new();
        {
            let mut w = arb.winners.lock().unwrap();
            for i in 0..MAX_WINNERS as i64 {
                w.insert(
                    key("saturate", i, GENERATION_UNSTAMPED),
                    SlotState::Confirmed(Role::Tui),
                );
            }
        }
        assert_eq!(
            arb.consume(&key("fresh", 0, GENERATION_UNSTAMPED), Role::Ccd),
            Consume::Lost,
            "a saturated arbiter fails closed on a new key"
        );
        // …and it has no winner to name, so the disposition omits the field rather than
        // inventing an answerer. See `ResponseArbiter::consume`.
        assert_eq!(
            arb.winner(&key("fresh", 0, GENERATION_UNSTAMPED)),
            None,
            "saturation records no winner, so there is nothing to report"
        );
    }

    // --- Occupy every id-bearing server-request frame ----------------------

    /// A non-approval, id-bearing server→client REQUEST (a `method`+top-level-`id` frame that
    /// is NOT a `*/requestApproval`). The shape that matters: a valid server-request that
    /// occupies a bare id but is not an approval — e.g. `tool/requestUserInput`.
    fn request_frame(method: &str, id: i64) -> String {
        format!(r#"{{"method":"{method}","id":{id},"params":{{"prompt":"?"}}}}"#)
    }

    #[test]
    fn phone_then_request_user_input_same_id_tombstones_zero_bytes() {
        // THE EXACT ALIASING CASE. Phone approval A (thread-A, id=0) ⇒ Bound(CcdAndTui).
        // Then a valid `tool/requestUserInput` B (thread-B, id=0) — previously SKIPPED by the
        // `/requestApproval` suffix check, leaving A live and aliasable. Now B occupies id=0
        // ⇒ collision ⇒ tombstone, so a ccd/tui `{id:0,result}` intended for B can NOT
        // authorize through A: ZERO bytes.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(Arc::clone(&arb), silent());
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_some(),
            "A binds first"
        );
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &request_frame("tool/requestUserInput", 0),
        );
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "the non-approval request occupies id=0 ⇒ collision ⇒ tombstone"
        );
        assert!(!phone_answers(&leg, 0));
        assert!(
            leg.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
        // A was never consumed — it never authorized (no under-refusal leak).
        assert_eq!(arb.winner(&key("thread-A", 0, GENERATION_UNSTAMPED)), None);
    }

    #[test]
    fn request_user_input_then_phone_same_id_tombstones_zero_bytes() {
        // Reverse ordering also aliases under the old skip. A `tool/requestUserInput` first
        // occupies id=0 ⇒ it is not a confirmed answerable family ⇒ Tombstoned immediately.
        // A later phone approval reusing id=0 is a second occupant ⇒ stays tombstoned, so the
        // phone answer forwards ZERO bytes.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &request_frame("tool/requestUserInput", 0),
        );
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "an unconfirmed non-approval request tombstones on first observe"
        );
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(!phone_answers(&leg, 0));
        assert!(
            leg.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
    }

    #[test]
    fn unknown_non_approval_id_bearing_request_tombstones_the_id() {
        // A generic unknown non-`/requestApproval` id-bearing request occupies its id ⇒
        // Tombstoned (fail-closed default), so a later same-id approval is unanswerable.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &request_frame("some/unknown/serverRequest", 0),
        );
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(leg.bound_entry(&RequestId::Int(0)).is_none());
        // A later phone approval reusing id=0 cannot resurrect it.
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-C", 0),
        );
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(!phone_answers(&leg, 0));
        assert!(
            leg.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
    }

    #[test]
    fn approval_missing_thread_id_tombstones_the_id() {
        // An approval-family frame (a `*/requestApproval` with a top-level id) but with NO
        // `params.threadId` has no arbiter key, yet it still occupies the id ⇒ Tombstoned
        // (occupy, permanently unanswerable) rather than being silently skipped.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &format!(r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"params":{{"itemId":"x"}}}}"#),
        );
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(!phone_answers(&leg, 0));
        assert!(
            leg.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
    }

    #[test]
    fn notification_without_id_occupies_nothing_and_leaves_a_later_approval_answerable() {
        // A notification (a `method` frame with NO top-level id) solicits no client response,
        // so it occupies nothing — it must NOT spuriously tombstone any id. A later phone
        // approval at id=0 is then a clean first occupant ⇒ Bound ⇒ answerable.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        // Real 0.147 notifications: one plain, one whose params carry a NESTED requestId (not
        // a top-level id) — neither must occupy a bare id.
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            r#"{"method":"thread/status/changed","params":{"threadId":"th-A"}}"#,
        );
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            r#"{"method":"serverRequest/resolved","params":{"threadId":"th-A","requestId":0}}"#,
        );
        assert!(leg.view.is_empty(), "notifications occupy no id");
        // A later approval at id=0 is a clean Bound and authorizes (not spuriously tombstoned).
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0),
        );
        assert!(leg.bound_entry(&RequestId::Int(0)).is_some());
        assert!(phone_answers(&leg, 0));
    }

    #[test]
    fn over_long_thread_id_approval_tombstones_and_stores_nothing() {
        // The memory bound: an approval whose threadId exceeds MAX_THREAD_ID_BYTES is
        // tombstoned (the id is occupied but no oversized string is retained), so per-entry
        // memory stays bounded. It is unanswerable.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        let long = "t".repeat(MAX_THREAD_ID_BYTES + 1);
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, &long, 0),
        );
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "over-long threadId tombstones"
        );
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_none(),
            "no LegEntry (and so no oversized string) is stored"
        );
        assert!(
            leg.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
        // A threadId exactly at the cap is still a clean Bound (boundary is inclusive).
        let at_cap = "t".repeat(MAX_THREAD_ID_BYTES);
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, &at_cap, 1),
        );
        assert!(leg.bound_entry(&RequestId::Int(1)).is_some());
    }

    #[test]
    fn tombstone_is_installed_before_a_panicking_sink_runs() {
        // Exception safety: the Tombstoned state is committed to the view BEFORE the
        // log/event sink is invoked, so a panicking sink can never leave the old Bound
        // alive (resurrectable). A sink that panics on its first call models the hostile case.
        use std::panic::{catch_unwind, AssertUnwindSafe};
        let sink: EventSink = Arc::new(|_: &str| panic!("event sink panicked"));
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), sink);
        // First observe is a clean Bound — no ambiguity log fires, so the sink is not called.
        leg.observe_server_frame(
            Role::Tui,
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_some(),
            "first observe binds"
        );
        // Second observe collides ⇒ register installs Tombstoned, THEN logs (which panics).
        let result = catch_unwind(AssertUnwindSafe(|| {
            leg.observe_server_frame(
                Role::Tui,
                &no_session(),
                &approval_frame(COMMAND_EXEC_APPROVAL, "thread-B", 0),
            );
        }));
        assert!(result.is_err(), "the panicking sink unwinds");
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "the tombstone is committed before the sink runs — the old Bound is gone"
        );
        assert!(!phone_answers(&leg, 0));
        assert!(
            leg.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard\'s answer takes no slot here: it forwards"
        );
    }

    // --- Who is handed which server request ---------------------------------

    /// Every server request family the wire has shown or the census declares, plus one
    /// nobody has named yet.
    fn every_family(thread: &str) -> Vec<(&'static str, String)> {
        [
            COMMAND_EXEC_APPROVAL,
            FILE_CHANGE_APPROVAL,
            "item/permissions/requestApproval",
            "item/tool/call",
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
            "some/future/serverRequest",
        ]
        .iter()
        .enumerate()
        .map(|(id, method)| (*method, approval_frame(method, thread, id as i64)))
        .collect()
    }

    /// **The keyboard is handed every frame**: every request family, and even a frame
    /// that poisons its view or wears the broker's own namespace. It is a real Codex
    /// client, and the app-server's first-answer-wins rule is what settles each request.
    #[test]
    fn the_keyboard_is_handed_every_server_request() {
        let mut tui = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        for (method, frame) in every_family("th-A") {
            assert_eq!(
                tui.observe_server_frame(Role::Tui, &no_session(), &frame),
                S2cDisposition::Deliver,
                "{method}"
            );
        }
        for frame in [
            r#"{"method":"codeconnect/responseDisposition","params":{"delivered":true}}"#,
            r#"{"id":50,"method":"item/commandExecution/requestApproval","params":{"threadId":"a","threadId":"b"}}"#,
            r#"{"id":51,"method":"item/commandExecution/requestApproval","params":{"threadId":"th-A"}}"#,
        ] {
            assert_eq!(
                tui.observe_server_frame(Role::Tui, &no_session(), frame),
                S2cDisposition::Deliver,
                "{frame}"
            );
        }
        assert!(
            tui.is_poisoned(),
            "the ambiguous frame still poisons the view"
        );
    }

    /// **The phone is handed a phone-family approval on the head, and nothing else it
    /// could answer.** An approval on another thread, and every other family, are
    /// withheld: an answer from this leg — even the broker's refusal — would settle the
    /// keyboard's question for it. Notifications and plain responses still pass.
    #[test]
    fn the_phone_is_handed_only_a_phone_family_approval_on_the_head() {
        let head = on_head("th-A");
        let mut ccd = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        for (method, frame) in every_family("th-A") {
            let want = if method == COMMAND_EXEC_APPROVAL || method == FILE_CHANGE_APPROVAL {
                S2cDisposition::Deliver
            } else {
                S2cDisposition::Withhold
            };
            assert_eq!(
                ccd.observe_server_frame(Role::Ccd, &head, &frame),
                want,
                "{method}"
            );
        }
        let mut ccd = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        assert_eq!(
            ccd.observe_server_frame(
                Role::Ccd,
                &head,
                &approval_frame(COMMAND_EXEC_APPROVAL, "th-other", 0)
            ),
            S2cDisposition::Withhold,
            "an approval on a thread the keyboard is not on"
        );
        assert_eq!(
            ccd.observe_server_frame(
                Role::Ccd,
                &no_session(),
                &approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 1)
            ),
            S2cDisposition::Withhold,
            "an approval while there is no head"
        );
        for passes in [
            r#"{"method":"thread/started","params":{"thread":{"id":"x"}}}"#,
            r#"{"id":9,"result":{"ok":true}}"#,
        ] {
            assert_eq!(
                ccd.observe_server_frame(Role::Ccd, &head, passes),
                S2cDisposition::Deliver,
                "{passes}"
            );
        }
    }

    /// **A phone's answer names the head.** An approval raised on a thread the
    /// keyboard has since left is readable history, not something the phone may answer.
    #[test]
    fn a_phone_answers_only_an_approval_on_the_head() {
        let mut ccd = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        ccd.observe_server_frame(
            Role::Ccd,
            &on_head("th-A"),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0),
        );
        let id = RequestId::Int(0);
        assert!(!ccd.authorize(&id, None), "no head");
        assert!(
            !ccd.authorize(&id, Some("th-B")),
            "the keyboard moved to another thread"
        );
        assert!(ccd.authorize(&id, Some("th-A")));
        assert!(!ccd.authorize(&id, Some("th-A")), "and it stays one-use");
    }

    /// **The keyboard's answer is dropped only when it lost a phone-family approval.**
    #[test]
    fn the_keyboard_loses_only_an_approval_already_answered() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut tui = LegCapabilities::new(Arc::clone(&arb), silent());
        let mut ccd = LegCapabilities::new(Arc::clone(&arb), silent());
        for (_, frame) in every_family("th-A") {
            tui.observe_server_frame(Role::Tui, &no_session(), &frame);
            ccd.observe_server_frame(Role::Ccd, &on_head("th-A"), &frame);
        }
        // The phone takes the command approval (id 0).
        assert!(ccd.authorize(&RequestId::Int(0), Some("th-A")));
        assert!(
            !tui.arbitrate_tui(&RequestId::Int(0)),
            "the keyboard's answer to an approval the phone already won must not go upstream"
        );
        // The keyboard takes the file-change approval (id 1); a second keyboard answer to
        // it is dropped too.
        assert!(tui.arbitrate_tui(&RequestId::Int(1)));
        assert!(!tui.arbitrate_tui(&RequestId::Int(1)));
        // Everything else is the keyboard's alone and forwards however often it answers.
        for id in 2..7 {
            assert!(tui.arbitrate_tui(&RequestId::Int(id)), "id {id}");
            assert!(tui.arbitrate_tui(&RequestId::Int(id)), "id {id} again");
        }
        assert!(
            tui.arbitrate_tui(&RequestId::Int(99)),
            "an id nobody asked about"
        );
    }

    /// **What the phone is not shown, it cannot answer**, even once the keyboard has moved
    /// to that thread.
    #[test]
    fn an_approval_withheld_from_the_phone_is_not_answerable_from_it() {
        let mut ccd = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        assert_eq!(
            ccd.observe_server_frame(
                Role::Ccd,
                &on_head("th-A"),
                &approval_frame(COMMAND_EXEC_APPROVAL, "th-B", 5)
            ),
            S2cDisposition::Withhold
        );
        assert!(!ccd.authorize(&RequestId::Int(5), Some("th-B")));
    }
}
