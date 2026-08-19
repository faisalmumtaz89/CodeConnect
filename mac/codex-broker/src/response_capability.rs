//! The one-use response-capability fanout registry (A4).
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
//!   server-request ids are per-thread small integers from 0, reused across families and
//!   threads (plan line 159), so the registry **cannot key on the bare id**. It is
//!   disambiguated by a **per-leg view** ([`LegCapabilities`]): each connection records the
//!   `serverRequest`s observed on *its own* upstream, so on that connection an id resolves
//!   to exactly one `(thread_id, grant, generation)` — **as long as that id is observed at
//!   most once on the leg.**
//! * **The instant a bare id is observed twice on a leg it is permanently ambiguous.** The
//!   wire carries no provenance, so once id=0 has bound one request and is then reused (or
//!   collides) the broker can no longer prove which request a `{id:0,...}` response answers.
//!   Each bare id is therefore a **per-leg 3-state** ([`IdState`]): `Unseen → Bound →
//!   Tombstoned`. The FIRST observation binds; **any SECOND observation of any kind
//!   tombstones the id for the life of the leg** — it never rebinds, never evicts, and
//!   never resurrects. A `Tombstoned` (or `Unseen`) id **cannot authorize**, so *even the
//!   original binding becomes unanswerable* (fail closed). This is what closes the three
//!   bare-id alias routes a security review found:
//!   * **reverse alias** — bind phone-capable A(id=0), leave it live, then observe TUI-only
//!     B(id=0): the collision tombstones id=0, so a later `{id:0}` response can no longer
//!     resolve to A's (CcdAndTui) capability;
//!   * **miss/late alias** — a first approval that was *observed* (see below for the ones
//!     that are not) then reused at the same id tombstones, so a delayed response to the
//!     first cannot consume the reused slot;
//!   * a **duplicate after consume** — authorizing consumes the arbiter slot but leaves the
//!     view `Bound`; a subsequent observe of the same id still tombstones it, and the
//!     arbiter's one-use already blocks re-authorizing the consumed slot.
//!
//!   In the legitimate fanout each leg observes a given `(thread,id)` approval exactly once
//!   (the app-server sends one copy per leg), so tombstoning on a second observation never
//!   fires on a single genuine approval — only on a bare-id reuse, which is exactly the
//!   ambiguous case. A legit app-server *retry* of the identical frame would also tombstone
//!   (over-refusal); that is the deliberate pre-2e cost the D4 generation seam lifts.
//! * **The same approval fans out to both legs** (ccd and TUI each on their own upstream)
//!   and **the first response from either wins.** A **shared arbiter**
//!   ([`ResponseArbiter`], one `Arc` across all legs) holds the one-use slot keyed by the
//!   `(thread_id, request_id, generation)` upstream-request key. The first `consume`
//!   wins and records the winner role (provenance); every sibling — the losing-fanout
//!   leg, a duplicate on the same leg — loses and forwards **zero upstream bytes** (a
//!   normal race — [`crate::refusal::classify_response`] turns a losing authorize into
//!   `DropLogKeepOpen`).
//! * **Only the two phone-supported families grant a ccd response.** Command-execution
//!   and file-change approvals grant **both** a ccd and a TUI capability; every other
//!   observed `serverRequest` (permissions, and — fail-closed default — anything not a
//!   known phone-supported approval) grants **TUI only**. A ccd response to a TUI-only
//!   family forwards zero bytes.
//!
//! ## Observation: every id-bearing server-request frame occupies its bare id
//!
//! The invariant is: **every id-bearing server-request frame occupies its bare id** — `Bound`
//! if it is a clean answerable family, else `Tombstoned` — **notifications occupy nothing,
//! and only a clean `Bound` authorizes.** This is what makes the bare-id space *fully*
//! fail-closed: no id-bearing server-request frame is ever silently skipped, so a
//! later/earlier `Bound` at the same id can never be aliased through a frame the observer
//! failed to occupy. Server-request ids are per-thread integers from 0, **shared across
//! families** (plan line 159), so any two server→client requests at the same `(leg, bare id)`
//! must never be conflated — occupying every one is the only sound rule.
//!
//! Concretely, for a frame within the size cap that parses (NoDup ok) and carries a usable
//! top-level `id`:
//!
//! * **Clean answerable server-request** — has a `method` string, a top-level `id`, **no**
//!   `result`/`error`, and (approval family) a `params.threadId`. The two phone approval
//!   methods grant `Bound(CcdAndTui)`; any other `*/requestApproval` grants `Bound(TuiOnly)`.
//! * **Occupy-but-never-answer ⇒ `Tombstoned`** — a *second* occupant of an already-tracked
//!   id (any method); or a *first* occupant that is **not** clean-answerable: a hybrid (also
//!   carries `result`/`error`), an approval missing/over-long `threadId`, or a method that is
//!   not a confirmed answerable family (a non-`/requestApproval` id-bearing request — e.g.
//!   `requestUserInput`/elicitation, whose exact server-request strings are not confirmable
//!   in 0.147). It occupies the id so it can never be aliased, but never authorizes.
//! * **Notification (`method`, no top-level `id`) ⇒ occupies nothing** — no client response is
//!   solicited, so it is ignored. Likewise a plain method-less `{id, result|error}` response
//!   (the server answering a *client* request) occupies no server-request id and is ignored.
//!
//! An escaped method name (`"requestApproval"`) defeats a raw substring guard, so the
//! observer does **not** substring-filter as a security gate: every in-cap frame is parsed
//! (via the NoDup [`crate::message::parse_no_dup_value`]) and classified on its **decoded**
//! method, so a JSON-escaped `requestApproval` is still seen and registered/collided.
//!
//! ## The unclassifiable-frame remedy: poison the leg (closes the last miss-alias)
//!
//! An s2c frame that CANNOT be classified — it exceeds [`MAX_OBSERVE_FRAME_BYTES`] (so it
//! cannot be parsed to know whether it is a request or which bare id it occupies), OR it is
//! ≤ cap but NoDup-rejected (malformed / duplicate member ⇒ method and id cannot be trusted)
//! — is the one remaining under-refusal route: if such a frame were actually an id-bearing
//! request, silently skipping it would leave its bare id `Unseen`, so a same-id `Bound`
//! (before or after) could be aliased by a response meant for the unclassifiable request.
//! The remedy (codex's "poison the leg when a frame cannot be classified") is to set a
//! per-leg [`LegCapabilities::poisoned`] flag on exactly these two branches; once set,
//! [`LegCapabilities::authorize`] fails closed (checked FIRST, before any view lookup) for
//! **every** id on the leg. This closes the last miss-alias **unconditionally**: no
//! unclassifiable frame can leave a bare id unoccupied and aliasable.
//!
//! This is safe *and* non-breaking for real traffic because the size cap is set (8 MiB) above
//! the largest legitimate s2c frame (a ~5.76 MiB `plugin/list`/`app/list/updated`
//! notification from the Phase-0 spikes), so every real frame PARSES and is classified
//! normally — big notifications carry no top-level id ⇒ occupy nothing ⇒ are ignored after
//! parse, never poisoning. The poison triggers only on a frame that is oversized (> 8 MiB) or
//! malformed (duplicate JSON members), which the pinned, identity-verified codex 0.147
//! app-server (the s2c PRODUCER, enforced at launch in Phase 2a) never emits — so it is dead
//! in practice. If it ever fires it is a **SAFE over-refusal**: the leg's approvals become
//! unanswerable (the leg can reconnect), never an under-refusal. A frame that PARSES cleanly
//! but is merely a notification (no top-level id) or a plain response (no method) is
//! classified and occupies nothing, exactly as before — it does NOT poison.
//!
//! ## Documented residuals (fail-closed, out of the practical threat model)
//!
//! * **Cross-leg divergent legs.** The tombstone is **per leg**. Two *different* approvals
//!   that happen to share a bare id on two different legs (e.g. thread-A id=0 on the TUI
//!   leg, thread-B id=0 on the ccd leg) are each a clean `Bound` on their own leg and both
//!   answerable — which is correct, they are genuinely different approvals. The *same
//!   logical approval* fanned to both legs is the arbiter's job (one winner across legs).
//!   A canonical cross-leg incarnation of a bare id is the D4/2e generation seam.
//!   **Finding 7 (2e dependency, NOT solved here):** full cross-leg soundness additionally
//!   requires a single continuously-lived [`LegCapabilities`] per leg across leg recreation
//!   (so a recreated leg does not forget its tombstones), a sealed switch, and a shared
//!   arbiter identity with convergent cross-leg fanout keys (the D2/D3/2e wiring). Leg
//!   recreation forgetting tombstones and divergent fanout keys are out of this bundle; they
//!   are the explicit Phase-2e dependency for cross-leg soundness.
//!
//! ## Bounded retention
//!
//! The per-leg view is capped at [`MAX_TRACKED_IDS`] distinct ids (a new id beyond it is
//! not inserted, so it stays `Unseen` ⇒ unanswerable — fail closed); the shared arbiter's
//! winners map is capped at [`MAX_WINNERS`] (beyond it `consume` fails closed). Ambiguity
//! (tombstone / id-cap) log lines are rate-limited by [`AMBIGUITY_LOG_BUDGET`] so a flood
//! of colliding ids cannot unbounded-log.
//!
//! ## Labeled Phase-2e seams (deliberately NOT built here)
//!
//! * **Generation stamping (D4) — the seam that LIFTS the tombstone / over-refusal.** The
//!   upstream-request key carries a `generation` so an A→B→A revisit's stale answer can
//!   never pose as the current visit's. Until 2e stamps a live epoch there is exactly
//!   **one** generation ([`GENERATION_UNSTAMPED`]); *within* a generation `(thread_id,
//!   request_id)` is unique and complete, so this sub-chunk is correct as-is. Pre-2e, **any
//!   ambiguous bare id is permanently fail-closed** (tombstoned ⇒ unanswerable). Phase-2e
//!   generation stamping makes the key `(thread, id, generation)` unique *per visit* and,
//!   with per-incarnation id disambiguation (per codex), makes a reused id **answerable
//!   again as a distinct incarnation** — that is what lifts the tombstone and the
//!   over-refusal. Until then exactly one approval per `(leg, bare-id)` is answerable, and
//!   the ccd command path is gated, so the over-refusal has no live impact. No
//!   switch/revisit/generation-transition logic is built here.
//! * **Winner-signal injection.** The winner role is recorded and logged (provenance),
//!   but the broker→ccd side-channel that *injects* it is a 2e seam — the relay has no
//!   injection path yet, so nothing is injected here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::allowlist::Role;
use crate::message::RequestId;
use crate::relay::EventSink;

/// The two phone-supported approval `serverRequest` methods (server→client requests),
/// confirmed against the captured 0.147 frames in `fixtures/codex/` (see this module's
/// tests). A response to either is authorized for **both** ccd and TUI.
pub const COMMAND_EXEC_APPROVAL: &str = "item/commandExecution/requestApproval";
/// File-change approval — the second phone-supported family (see [`COMMAND_EXEC_APPROVAL`]).
pub const FILE_CHANGE_APPROVAL: &str = "item/fileChange/requestApproval";

/// The visit generation until Phase 2e stamps a live connection/delivery epoch. Exactly
/// one generation exists in this sub-chunk, so this constant is the seam D4 replaces.
pub const GENERATION_UNSTAMPED: u64 = 0;

/// A generous cap on an observed s2c frame we will parse. It is set **above the largest
/// legitimate s2c frame** so that all real traffic PARSES and is classified normally — never
/// size-skipped. The largest known-legit frame observed in the Phase-0 spikes is a ~5.76 MiB
/// `plugin/list` / `app/list/updated` notification; 8 MiB covers that observed maximum with
/// comfortable margin, while real server→client REQUESTS (the two approval families, per
/// `fixtures/codex/*.jsonl`) are ≤ ~1 KiB. Because every legitimate frame parses, big
/// notifications occupy nothing (no top-level id) and are correctly ignored after parse,
/// rather than tripping the poison path below. A frame *above* this cap cannot be parsed to
/// learn whether it is a request or which id it occupies, so it is UNCLASSIFIABLE and poisons
/// the leg (see [`LegCapabilities::observe_server_frame`]); the pinned, identity-verified
/// codex 0.147 app-server (the s2c producer, enforced at launch in Phase 2a) never emits a
/// frame this large, so that path is dead in practice — and a SAFE over-refusal if it fires.
/// Every in-cap text frame is parsed (no substring pre-filter), so a JSON-escaped
/// `requestApproval` method name is still decoded and seen.
const MAX_OBSERVE_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Bounded retention: the max number of distinct bare ids tracked per leg. Server-request
/// ids are per-thread small integers from 0, so a real leg's id set is tiny; this cap only
/// prevents unbounded growth under a hostile/broken upstream. A new id beyond the cap is
/// not inserted, so it stays `Unseen` ⇒ unanswerable (fail closed).
const MAX_TRACKED_IDS: usize = 4096;

/// Bounded retention for the shared arbiter: the max number of consumed winner slots per
/// session. One slot per genuine approval; approvals are user-driven and single-generation
/// pre-2e, so a session never approaches this. Beyond it, `consume` fails closed (no winner
/// recorded ⇒ zero bytes).
const MAX_WINNERS: usize = 64 * 1024;

/// How many per-leg ambiguity (tombstone / id-cap) log lines to emit before suppressing, so
/// a flood of colliding ids cannot unbounded-log. The last emitted line is marked
/// suppressed.
const AMBIGUITY_LOG_BUDGET: u32 = 64;

/// Bounded retention (finding 5): the maximum stored `threadId` length. Real codex thread
/// ids are UUIDs (~36 chars), so this is orders of magnitude of slack; an approval whose
/// `threadId` exceeds it is **tombstoned** (the id is occupied but stores no string), so
/// per-entry memory stays bounded even under a hostile/broken upstream. This closes the
/// only unbounded per-entry field — the id key and grant are already fixed-size.
const MAX_THREAD_ID_BYTES: usize = 256;

/// Authorization oracle for a method-less response (an approval answer).
///
/// The classifier consults this to decide whether a `{id, result|error}` response
/// matches a live, authorized, one-use capability granted to this endpoint. Anything
/// unauthorized (unsolicited / duplicate / cross-thread / stale-visit / losing-sibling
/// / wrong-role / ambiguous-tombstoned) forwards zero bytes.
pub trait ResponseCapabilityRegistry {
    /// True iff `id` names a live capability granted to `role` that this response may
    /// consume. Implementations MUST consume atomically (a real one revokes siblings).
    /// `is_error` is ignored — a `{id,error}` answer consumes the slot exactly like a
    /// `{id,result}` one (A4).
    fn authorize(&self, role: Role, id: &RequestId, is_error: bool) -> bool;
}

/// The deferred, fail-closed registry: no capability is ever authorized, so every
/// method-less response forwards zero upstream bytes. Retained for the unit tests that
/// do not exercise fanout (and as the `Env` default in the pure-core tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCapabilities;

impl ResponseCapabilityRegistry for NoCapabilities {
    fn authorize(&self, _role: Role, _id: &RequestId, _is_error: bool) -> bool {
        false
    }
}

/// Which endpoint roles an observed `serverRequest` grants a one-use response capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grant {
    /// A phone-supported family (command execution, file change): both the ccd phone
    /// answer and the TUI keyboard answer are granted.
    CcdAndTui,
    /// A non-phone approval family (`item/permissions/requestApproval`) and the fail-closed
    /// default for any other/unknown `*/requestApproval`: only the TUI may answer. (Non-
    /// approval id-bearing requests — e.g. `requestUserInput`/elicitation — are not bound at
    /// all; the observer tombstones them, so they never reach `Grant`.)
    TuiOnly,
}

impl Grant {
    /// Classify an approval `serverRequest` method into its grant. Invoked only for
    /// `*/requestApproval` methods (the observer tombstones every other id-bearing request).
    /// Fail-closed: only the two confirmed phone-supported methods grant ccd; every other
    /// approval is TUI-only, so an unknown/future approval can never wrongly hand the phone a
    /// capability.
    fn for_method(method: &str) -> Grant {
        match method {
            COMMAND_EXEC_APPROVAL | FILE_CHANGE_APPROVAL => Grant::CcdAndTui,
            _ => Grant::TuiOnly,
        }
    }

    /// Does this grant authorize `role` to answer?
    fn grants(self, role: Role) -> bool {
        match (self, role) {
            (Grant::CcdAndTui, _) => true,
            (Grant::TuiOnly, Role::Tui) => true,
            (Grant::TuiOnly, Role::Ccd) => false,
        }
    }
}

/// The shared arbiter's one-use slot key. **Within one generation**, `(thread_id,
/// request_id)` uniquely and completely identifies an upstream server-request; the
/// `generation` field is the D4 seam that keeps a stale revisit's answer from consuming
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

/// The shared, cross-leg fanout arbiter: the one-use slots and their winner provenance.
///
/// One `Arc<ResponseArbiter>` is shared by every connection task (like
/// [`crate::session::SessionThreads`]); the interior [`Mutex`] makes `consume` atomic
/// across the legs racing to answer the same fanned-out approval. This is the pure core
/// — no I/O, unit-testable in isolation.
#[derive(Debug, Default)]
pub struct ResponseArbiter {
    /// Consumed slots → the role that won each. Presence marks the slot spent; the value
    /// is the winner-provenance (queryable via [`ResponseArbiter::winner`]). Bounded at
    /// [`MAX_WINNERS`] (fail closed beyond it).
    winners: Mutex<HashMap<UpstreamRequestKey, Role>>,
}

impl ResponseArbiter {
    pub fn new() -> ResponseArbiter {
        ResponseArbiter::default()
    }

    /// Atomically consume the slot for `key` on behalf of `role`. The first caller wins
    /// (the slot records `role` as the winner); every later caller loses. One `Mutex`
    /// section, so two legs racing the same fanned-out approval have exactly one winner.
    /// If the winners map is saturated ([`MAX_WINNERS`]) and this is a new key, no winner
    /// can be recorded, so the consume fails closed (`Lost`, zero bytes).
    fn consume(&self, key: &UpstreamRequestKey, role: Role) -> Consume {
        let mut winners = self.winners.lock().expect("arbiter mutex poisoned");
        // Already consumed (a losing sibling/duplicate), OR saturated so no winner can be
        // recorded: either way fail closed (zero bytes). Only a fresh key with room wins.
        if winners.contains_key(key) || winners.len() >= MAX_WINNERS {
            Consume::Lost
        } else {
            winners.insert(key.clone(), role);
            Consume::Won
        }
    }

    /// The recorded winner of a slot, if it has been consumed (winner-provenance query).
    ///
    /// Production emits provenance on the [`LegCapabilities`] consume path (the audit
    /// log); this direct accessor is the inspection seam the tests use, and the query
    /// point the 2e winner-signal *injection* will build on. Test-only until then.
    #[cfg(test)]
    fn winner(&self, key: &UpstreamRequestKey) -> Option<Role> {
        self.winners
            .lock()
            .expect("arbiter mutex poisoned")
            .get(key)
            .copied()
    }
}

/// What a cleanly-bound (observed exactly once) server-request id maps to on **this**
/// connection.
#[derive(Debug, Clone)]
struct LegEntry {
    thread_id: String,
    grant: Grant,
    generation: u64,
}

/// The per-leg lifecycle of one bare server-request id. A bare Response frame carries only
/// the id (no thread, no provenance), so the instant an id is observed twice on a leg the
/// broker can no longer prove which request a `{id,...}` response answers.
///
/// * **Unseen** — no entry in the view (absence); cannot authorize.
/// * **`Bound`** — observed exactly once; resolves to one `(thread, grant, generation)` and
///   can authorize a matching response (subject to the role gate + one-use arbiter).
/// * **`Tombstoned`** — observed a SECOND time (any reuse/collision): permanently ambiguous
///   on this leg. Never authorizes, never rebinds, never resurrects — even the original
///   binding becomes unanswerable (fail closed). Only D4 generation stamping (2e) lifts it.
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
    Bind { thread_id: String, grant: Grant },
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
    /// Bounded at [`MAX_TRACKED_IDS`] distinct ids. See the module docs for the 2e seam.
    view: HashMap<RequestId, IdState>,
    log: EventSink,
    /// Remaining ambiguity (tombstone / id-cap) log lines before suppression.
    log_budget: u32,
    /// Leg-wide poison flag (default `false`). Set when an s2c frame is genuinely
    /// UNCLASSIFIABLE — above [`MAX_OBSERVE_FRAME_BYTES`] (cannot be parsed to know whether it
    /// is a request or which id it occupies) or ≤ cap but NoDup-rejected (malformed /
    /// duplicate-member ⇒ method/id cannot be trusted). Once set, [`authorize`] fails closed
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

    /// Observe one server→client frame and, for **every** frame that occupies a bare
    /// response id, register how it occupies that id: a clean answerable server-request
    /// `Bind`s the id; any other id-bearing frame `Tombstone`s it. This is the invariant
    /// that makes the bare-id space fully fail-closed — no id-bearing server-request frame is
    /// ever silently skipped, so a later/earlier `Bound` at the same id can never be aliased
    /// through a frame the observer failed to occupy (findings 1 & 2).
    ///
    /// Two UNCLASSIFIABLE pre-filters, both fail-closed by **poisoning the whole leg**: a frame
    /// above [`MAX_OBSERVE_FRAME_BYTES`] (cannot be parsed to know if it is a request / which id
    /// it occupies) and a NoDup-rejected frame (unparseable / duplicate member ⇒ method/id
    /// untrustworthy). Either could be an id-bearing request whose bare id we then fail to
    /// occupy, so instead of silently skipping (which would leave a same-id `Bound` aliasable —
    /// a real under-refusal) we set [`Self::poisoned`], after which [`authorize`] fails closed
    /// for every id on this leg. A frame that PARSES cleanly but carries no top-level id occupies
    /// no response id (a notification, or a plain method-less response) and is simply ignored —
    /// it does NOT poison. The size cap is set above the largest legitimate s2c frame, so real
    /// traffic (including multi-MB `plugin/list`/`app/list/updated` notifications) always parses
    /// and never reaches the poison path; the poison triggers only on a frame the identity-
    /// pinned codex 0.147 app-server never emits, and is a SAFE over-refusal if it fires.
    ///
    /// [`authorize`]: LegCapabilities::authorize
    pub fn observe_server_frame(&mut self, text: &str) {
        // Size gate, checked before any parse: it bounds worst-case parse+alloc AND marks the
        // frame unclassifiable. The cap sits above the largest legit s2c frame, so a frame over
        // it cannot be parsed to learn whether it is a request or which bare id it occupies —
        // it might be an id-bearing request we then fail to occupy, so poison the leg (fail
        // closed leg-wide) rather than silently skip. Forwarding is unaffected.
        if text.len() > MAX_OBSERVE_FRAME_BYTES {
            self.poison("oversized s2c frame (> MAX_OBSERVE_FRAME_BYTES)");
            return;
        }
        // Parse with the same duplicate-member discipline the c2s classifier uses: a frame
        // with a duplicate member (at any nesting) is ambiguous between our parse and the
        // app-server's, so it yields no trustworthy method/id. It too may be an id-bearing
        // request we cannot occupy precisely, so poison the leg rather than silently skip.
        let Some(v) = crate::message::parse_no_dup_value(text) else {
            self.poison("NoDup-rejected s2c frame (unparseable / duplicate member)");
            return;
        };
        // Only a frame that occupies a bare RESPONSE id concerns us: it must carry a usable
        // top-level `id`. No top-level id ⇒ a notification (method-only ⇒ no client response)
        // or a non-id-bearing frame ⇒ occupies nothing ⇒ ignore.
        let Some(id) = v.get("id").and_then(RequestId::from_value) else {
            return;
        };
        // id-bearing, but only a frame that ALSO carries a `method` is a server→client
        // request (or an id-bearing hybrid). A method-less `{id, result|error}` is a plain
        // response (the server answering a client request) — it does not occupy the
        // server-request id space and c2s responses go through `authorize`, so ignore it.
        let Some(method) = v.get("method").and_then(|m| m.as_str()) else {
            return;
        };
        // From here the frame OCCUPIES bare id `id`: it MUST be registered — `Bind` if it is
        // a clean answerable family, else `Tombstone` — never silently skipped.
        let occupancy = Self::classify_request(&v, method);
        self.register(id, occupancy, method);
    }

    /// Classify a method-bearing, id-bearing s2c frame into how it occupies its bare id.
    /// Every such frame occupies the id; the only question is `Bind` vs `Tombstone`.
    fn classify_request(v: &serde_json::Value, method: &str) -> Occupancy {
        // A method-bearing frame that ALSO carries a response discriminant is a hybrid, not a
        // clean request. It still occupies the id (it has a readable top-level id), but it can
        // never be answered ⇒ tombstone (fail closed).
        if v.get("result").is_some() || v.get("error").is_some() {
            return Occupancy::Tombstone;
        }
        // The only confirmed answerable server→client requests in codex 0.147 are the
        // approval family (`*/requestApproval`, per `fixtures/codex/*.jsonl` and
        // `ccd/src/codex_adapter.rs`). Any OTHER id-bearing request method (e.g. a
        // `requestUserInput`/elicitation whose exact string we cannot confirm) is not a
        // confirmed answerable family ⇒ tombstone (occupy the id, permanently unanswerable).
        // This is the finding-1 fix: a non-`/requestApproval` id-bearing request no longer
        // leaves its id unoccupied.
        if !method.ends_with("/requestApproval") {
            return Occupancy::Tombstone;
        }
        // An approval is a server→client *request* keyed in the arbiter by its
        // `params.threadId`. A missing threadId (no arbiter key) or an over-long one
        // (finding 5 memory bound) still occupies the id ⇒ tombstone rather than skip.
        match v
            .get("params")
            .and_then(|p| p.get("threadId"))
            .and_then(|t| t.as_str())
        {
            Some(thread_id) if thread_id.len() <= MAX_THREAD_ID_BYTES => Occupancy::Bind {
                thread_id: thread_id.to_string(),
                grant: Grant::for_method(method),
            },
            _ => Occupancy::Tombstone,
        }
    }

    /// Apply one occupant to the 3-state view. A second occupant of an already-tracked id
    /// (any kind) → `Tombstoned` (permanent ambiguity). A first occupant → `Bound` if it is a
    /// clean answerable family, else `Tombstoned` (it still occupies the id). A new id beyond
    /// [`MAX_TRACKED_IDS`] is not inserted (stays `Unseen` ⇒ unanswerable), so the view is
    /// code-bounded.
    ///
    /// Finding 6 (exception safety): the `Tombstoned` state is committed to the view **before**
    /// the log/event sink runs, so a panicking sink can never leave the old `Bound` alive.
    fn register(&mut self, id: RequestId, occupancy: Occupancy, method: &str) {
        if self.view.contains_key(&id) {
            // A SECOND occupant of any kind is a collision: the bare id is now permanently
            // ambiguous on this leg. Install Tombstoned FIRST (finding 6), then log.
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
            Occupancy::Bind { thread_id, grant } => {
                self.view.insert(
                    id,
                    IdState::Bound(LegEntry {
                        thread_id,
                        grant,
                        generation: GENERATION_UNSTAMPED,
                    }),
                );
            }
            Occupancy::Tombstone => {
                // A first occupant that is not clean-answerable still OCCUPIES the id so it can
                // never be aliased. Install Tombstoned FIRST (finding 6), then log.
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

    /// Poison the whole leg: an UNCLASSIFIABLE s2c frame (oversized, or NoDup-rejected) was
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
}

impl ResponseCapabilityRegistry for LegCapabilities {
    fn authorize(&self, role: Role, id: &RequestId, _is_error: bool) -> bool {
        // Poison gate FIRST, before any view lookup: if an unclassifiable s2c frame (oversized
        // or NoDup-rejected) was ever observed on this leg, it may have been an id-bearing
        // request whose bare id we could not occupy, leaving a same-id `Bound` aliasable. Fail
        // closed for ALL ids on this leg — this closes the last miss-alias unconditionally.
        if self.poisoned {
            return false;
        }
        // Only a CLEAN `Bound` state can authorize. `Tombstoned` (observed twice ⇒
        // permanently ambiguous) or `Unseen` (never observed) fails closed — zero bytes.
        // This is what closes the reverse alias: once id has ANY collision, even the
        // original binding is unanswerable, because a bare-id response carries no
        // provenance to prove which request it belongs to.
        let Some(IdState::Bound(entry)) = self.view.get(id) else {
            return false;
        };
        // Role gate: a ccd response to a TUI-only family is never granted.
        if !entry.grant.grants(role) {
            return false;
        }
        let key = UpstreamRequestKey {
            thread_id: entry.thread_id.clone(),
            request_id: id.clone(),
            generation: entry.generation,
        };
        match self.arbiter.consume(&key, role) {
            Consume::Won => {
                // Debug-escape the thread id (and the already-Debug id) so a control char
                // or newline in an observed thread id cannot inject into the audit line.
                (self.log)(&format!(
                    "capability won: winner={role:?} thread={:?} id={id:?} gen={}",
                    entry.thread_id, entry.generation
                ));
                true
            }
            // A losing-fanout sibling, a duplicate on this leg, or arbiter saturation.
            Consume::Lost => false,
        }
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

    // --- Family classifier -------------------------------------------------

    #[test]
    fn phone_supported_methods_grant_ccd_and_tui() {
        for m in [COMMAND_EXEC_APPROVAL, FILE_CHANGE_APPROVAL] {
            assert_eq!(Grant::for_method(m), Grant::CcdAndTui, "{m}");
            assert!(Grant::for_method(m).grants(Role::Ccd), "{m} grants ccd");
            assert!(Grant::for_method(m).grants(Role::Tui), "{m} grants tui");
        }
    }

    #[test]
    fn observe_only_and_unknown_approval_methods_are_tui_only() {
        // `Grant::for_method` is now only invoked for `*/requestApproval` methods (the
        // observer tombstones every OTHER id-bearing request rather than binding it), so this
        // exercises the approval-family grant default: the known observe-only permissions
        // family and any unknown future `*/requestApproval` are TUI-only (fail-closed).
        for m in [
            "item/permissions/requestApproval",
            "some/future/requestApproval",
        ] {
            let g = Grant::for_method(m);
            assert_eq!(g, Grant::TuiOnly, "{m}");
            assert!(g.grants(Role::Tui), "{m} grants tui");
            assert!(!g.grants(Role::Ccd), "{m} must NOT grant ccd");
        }
    }

    // --- Shared arbiter: atomic one-use + provenance -----------------------

    #[test]
    fn arbiter_is_atomic_one_use_and_records_the_winner() {
        let arb = ResponseArbiter::new();
        let k = key("thread-A", 0, GENERATION_UNSTAMPED);
        // First consume wins and records provenance; the sibling loses, provenance holds.
        assert_eq!(arb.consume(&k, Role::Ccd), Consume::Won);
        assert_eq!(arb.winner(&k), Some(Role::Ccd));
        assert_eq!(arb.consume(&k, Role::Tui), Consume::Lost, "sibling revoked");
        assert_eq!(arb.winner(&k), Some(Role::Ccd), "winner is unchanged");
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
        // The generation field also separates slots (the D4 seam) — a gen-1 slot does not
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
        ccd.observe_server_frame(&frame);
        tui.observe_server_frame(&frame);

        // ccd answers first → authorized; the TUI sibling then loses (revoked), zero bytes.
        assert!(ccd.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(!tui.authorize(Role::Tui, &RequestId::Int(0), false));
        // Winner-provenance is queryable on the shared arbiter.
        assert_eq!(
            arb.winner(&key("thread-A", 0, GENERATION_UNSTAMPED)),
            Some(Role::Ccd)
        );
    }

    #[test]
    fn observe_only_family_refuses_ccd_authorizes_tui() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut ccd = LegCapabilities::new(Arc::clone(&arb), silent());
        let mut tui = LegCapabilities::new(Arc::clone(&arb), silent());
        let frame = approval_frame("item/permissions/requestApproval", "thread-P", 0);
        ccd.observe_server_frame(&frame);
        tui.observe_server_frame(&frame);

        // ccd can never answer an observe-only family (zero bytes); the TUI still can.
        assert!(!ccd.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(tui.authorize(Role::Tui, &RequestId::Int(0), false));
    }

    #[test]
    fn unsolicited_response_id_is_unauthorized() {
        let arb = Arc::new(ResponseArbiter::new());
        let leg = LegCapabilities::new(arb, silent());
        // No serverRequest observed → nothing to consume.
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(7), false));
    }

    #[test]
    fn duplicate_on_the_same_leg_is_one_use() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0));
        assert!(
            leg.authorize(Role::Ccd, &RequestId::Int(0), false),
            "first wins"
        );
        assert!(
            !leg.authorize(Role::Ccd, &RequestId::Int(0), false),
            "second is spent"
        );
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
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0));
        leg.observe_server_frame(&approval_frame(
            "item/permissions/requestApproval",
            "thread-B",
            0,
        ));
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "collision tombstones"
        );
        // Neither role can answer the tombstoned id — the reverse alias is closed.
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), false));
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
        leg.observe_server_frame(&frame);
        leg.observe_server_frame(&frame); // duplicate observation ⇒ collision
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(
            !leg.authorize(Role::Ccd, &RequestId::Int(0), false),
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
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0));
        assert!(
            leg.authorize(Role::Ccd, &RequestId::Int(0), false),
            "clean Bound authorizes once"
        );
        // A later same-id observation tombstones the (already-consumed) id.
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "thread-B", 0));
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        // A post-consume duplicate response fails closed (tombstoned AND already spent).
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), false));
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
        leg.observe_server_frame(&approval_frame(
            "item/permissions/requestApproval",
            "th-P",
            0,
        ));
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-C", 0));
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(
            !leg.authorize(Role::Ccd, &RequestId::Int(0), false),
            "no phone upgrade"
        );
        assert!(
            !leg.authorize(Role::Tui, &RequestId::Int(0), false),
            "original tui answer is also closed once ambiguous"
        );
    }

    #[test]
    fn single_unknown_request_approval_registers_tui_only_then_reuse_tombstones() {
        // REWRITTEN from `unknown_request_approval_registers_tui_only_and_obeys_never_rebind`.
        // A single observe of an unknown `*/requestApproval` still registers as TuiOnly
        // (fail-closed default) — coverage kept. But a same-id reuse now TOMBSTONES it
        // (was: original stayed answerable), so the tui answer is also closed afterward.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(Arc::clone(&arb), silent());
        leg.observe_server_frame(&approval_frame("some/future/requestApproval", "th-U", 0));
        assert_eq!(
            leg.bound_entry(&RequestId::Int(0)).unwrap().grant,
            Grant::TuiOnly,
            "unknown approval registers as the fail-closed TUI-only default"
        );
        // A phone-family reuse of id=0 tombstones it rather than upgrading or preserving.
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-C", 0));
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), false));
    }

    #[test]
    fn escaped_request_approval_method_is_decoded_and_registered() {
        // An escaped method name (`l` for the final `l` of requestApproval) defeats a
        // raw substring guard but decodes to a real approval method. The size-only observe
        // gate parses it, so it is recognized and registered — NOT silently skipped.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(Arc::clone(&arb), silent());
        // The final `l` is JSON-escaped (l), so the raw bytes carry no literal
        // `requestApproval` marker but decode to COMMAND_EXEC_APPROVAL.
        let frame = escaped_approval_frame("th-A", 0);
        assert!(
            !frame.contains("requestApproval"),
            "the raw bytes do NOT contain the literal marker (the escape attack)"
        );
        leg.observe_server_frame(&frame);
        // It was decoded to the phone family and bound (treated as an approval).
        assert_eq!(
            leg.bound_entry(&RequestId::Int(0)).unwrap().grant,
            Grant::CcdAndTui,
            "the escaped approval is decoded and registered as the phone family"
        );
        assert!(leg.authorize(Role::Ccd, &RequestId::Int(0), false));
    }

    #[test]
    fn escaped_request_approval_method_collides_correctly() {
        // The escaped form is the SAME id as a following plain approval ⇒ a collision that
        // tombstones (it is not silently skipped, which would have left the plain one live).
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(Arc::clone(&arb), silent());
        let escaped = escaped_approval_frame("th-A", 0);
        assert!(!escaped.contains("requestApproval"));
        leg.observe_server_frame(&escaped);
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-B", 0));
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "the escaped approval participates in collision detection"
        );
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
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
        leg.observe_server_frame(&format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"id":1,"params":{{"threadId":"th-A"}}}}"#
        ));
        assert!(leg.is_poisoned(), "a NoDup-rejected frame poisons the leg");
        // Duplicate `method` also fails NoDup (idempotent poison, still registers nothing).
        leg.observe_server_frame(&format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","method":"x/requestApproval","id":2,"params":{{"threadId":"th-A"}}}}"#
        ));
        assert!(
            leg.view.is_empty(),
            "a duplicate-member frame must not register (malformed s2c)"
        );
        // A later CLEAN approval is unanswerable — the leg is fail closed leg-wide.
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-clean", 9));
        assert!(leg.bound_entry(&RequestId::Int(9)).is_some());
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(9), false));
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(9), false));
    }

    #[test]
    fn large_notification_under_cap_does_not_poison_and_keeps_later_approvals_answerable() {
        // The raised cap (8 MiB) is set ABOVE the largest legitimate s2c frame so real traffic
        // still works: a big `app/list/updated` NOTIFICATION (a `method` frame with NO
        // top-level id) comfortably over the OLD 1 MiB cap but under 8 MiB must PARSE, occupy
        // nothing, and NOT poison — proving real multi-MB plugin/list traffic is unaffected.
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        // ~2 MiB payload: over the old 1 MiB cap, well under the new 8 MiB cap.
        let pad = "Z".repeat(2 * 1024 * 1024);
        let big_notification =
            format!(r#"{{"method":"app/list/updated","params":{{"pad":"{pad}"}}}}"#);
        assert!(
            big_notification.len() > 1024 * 1024
                && big_notification.len() < MAX_OBSERVE_FRAME_BYTES,
            "the frame is over the OLD 1 MiB cap but under the NEW 8 MiB cap"
        );
        leg.observe_server_frame(&big_notification);
        assert!(
            !leg.is_poisoned(),
            "a large but in-cap notification parses and occupies nothing — no poison"
        );
        assert!(leg.view.is_empty(), "a notification occupies no id");
        // A later CLEAN phone approval is therefore still answerable (real traffic works).
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0));
        assert!(leg.bound_entry(&RequestId::Int(0)).is_some());
        assert!(
            leg.authorize(Role::Ccd, &RequestId::Int(0), false),
            "the leg is not poisoned, so the clean approval answers"
        );
    }

    #[test]
    fn poison_overrides_a_previously_bound_id() {
        // Poison-first ordering: an id is cleanly `Bound` first (and would authorize), THEN an
        // unclassifiable frame poisons the leg. Because `authorize` checks the poison flag
        // FIRST — before the view lookup — the previously-Bound id's response now forwards ZERO
        // bytes. Poison overrides an existing clean Bound.
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0));
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_some(),
            "id=0 binds cleanly first"
        );
        // Now poison via an oversized (unclassifiable) frame.
        let pad = "Z".repeat(MAX_OBSERVE_FRAME_BYTES + 1);
        leg.observe_server_frame(&format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":1,"params":{{"threadId":"th-B","pad":"{pad}"}}}}"#
        ));
        assert!(leg.is_poisoned());
        // The still-Bound id=0 is now unanswerable: poison overrides the clean Bound.
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_some(),
            "the view still holds the clean Bound"
        );
        assert!(
            !leg.authorize(Role::Ccd, &RequestId::Int(0), false),
            "but poison-first makes it forward zero bytes"
        );
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), false));
    }

    #[test]
    fn hybrid_result_bearing_frame_tombstones_the_id_and_never_authorizes() {
        // ADAPTED from `hybrid_result_bearing_approval_frame_is_not_registered`, which
        // asserted the view stayed EMPTY (the old skip). A hybrid (method-bearing frame that
        // also carries `result`/`error`) is not a clean serverRequest, but it has a readable
        // top-level id, so it OCCUPIES that id ⇒ Tombstoned (findings 1/2). It never grants a
        // capability, and a later same-id approval can never alias through it.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(&format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"params":{{"threadId":"th-A"}},"result":{{}}}}"#
        ));
        leg.observe_server_frame(&format!(
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
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), true));
        // A later clean approval reusing id=0 stays tombstoned (second occupant) —
        // unanswerable, so it can never alias through the hybrid.
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-B", 0));
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
    }

    #[test]
    fn oversized_id_bearing_frame_poisons_the_leg() {
        // ADAPTED from `oversized_approval_frame_is_not_registered`, which asserted only that
        // an oversized frame does not register (view stays empty). It still does not register —
        // but because a > cap frame cannot be parsed to know if it is a request / which id it
        // occupies, it is now UNCLASSIFIABLE ⇒ it POISONS the whole leg (fail closed leg-wide),
        // closing the last miss-alias. The old skip left a same-id `Bound` aliasable.
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        let pad = "Z".repeat(MAX_OBSERVE_FRAME_BYTES + 1);
        let big = format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"params":{{"threadId":"th-A","pad":"{pad}"}}}}"#
        );
        assert!(big.len() > MAX_OBSERVE_FRAME_BYTES);
        leg.observe_server_frame(&big);
        assert!(leg.is_poisoned(), "an oversized frame poisons the leg");
        assert!(leg.view.is_empty(), "and still registers nothing");
        // A subsequently-observed CLEAN phone approval at ANY id is a clean Bound in the view,
        // yet its response forwards ZERO bytes — the poison is leg-wide, overriding the Bound.
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-clean", 7));
        assert!(
            leg.bound_entry(&RequestId::Int(7)).is_some(),
            "the clean approval binds in the view"
        );
        assert!(
            !leg.authorize(Role::Ccd, &RequestId::Int(7), false),
            "but the poisoned leg forwards zero bytes for any id"
        );
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(7), false));
    }

    #[test]
    fn an_error_response_consumes_like_a_result() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0));
        // is_error = true still consumes the one-use slot (A4).
        assert!(leg.authorize(Role::Ccd, &RequestId::Int(0), true));
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), true));
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
        leg.observe_server_frame(&frame);
        assert!(leg.view.is_empty());
    }

    #[test]
    fn per_leg_id_cap_stops_growth_and_leaves_new_ids_unanswerable() {
        // Bounded retention: fill the view to the cap with distinct ids, then a NEW id is
        // not inserted (stays Unseen ⇒ unanswerable), while an EXISTING id still tombstones.
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        for i in 0..MAX_TRACKED_IDS as i64 {
            leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th", i));
        }
        assert_eq!(leg.view.len(), MAX_TRACKED_IDS);
        // A brand-new id past the cap is not tracked → unanswerable (fail closed).
        let over = MAX_TRACKED_IDS as i64;
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th", over));
        assert_eq!(
            leg.view.len(),
            MAX_TRACKED_IDS,
            "the view does not grow past the cap"
        );
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(over), false));
        // An already-tracked id can still transition to Tombstoned (no growth).
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th2", 0));
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
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0));
        for _ in 0..(AMBIGUITY_LOG_BUDGET as usize + 50) {
            leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-B", 0));
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
                w.insert(key("saturate", i, GENERATION_UNSTAMPED), Role::Tui);
            }
        }
        assert_eq!(
            arb.consume(&key("fresh", 0, GENERATION_UNSTAMPED), Role::Ccd),
            Consume::Lost,
            "a saturated arbiter fails closed on a new key"
        );
    }

    // --- Occupy every id-bearing server-request frame (findings 1, 2, 5, 6) ------

    /// A non-approval, id-bearing server→client REQUEST (a `method`+top-level-`id` frame that
    /// is NOT a `*/requestApproval`). This is the finding-1 shape: a valid server-request that
    /// occupies a bare id but is not an approval — e.g. `tool/requestUserInput`.
    fn request_frame(method: &str, id: i64) -> String {
        format!(r#"{{"method":"{method}","id":{id},"params":{{"prompt":"?"}}}}"#)
    }

    #[test]
    fn phone_then_request_user_input_same_id_tombstones_zero_bytes() {
        // THE EXACT FINDING-1 CASE. Phone approval A (thread-A, id=0) ⇒ Bound(CcdAndTui).
        // Then a valid `tool/requestUserInput` B (thread-B, id=0) — previously SKIPPED by the
        // `/requestApproval` suffix check, leaving A live and aliasable. Now B occupies id=0
        // ⇒ collision ⇒ tombstone, so a ccd/tui `{id:0,result}` intended for B can NOT
        // authorize through A: ZERO bytes.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(Arc::clone(&arb), silent());
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0));
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_some(),
            "A binds first"
        );
        leg.observe_server_frame(&request_frame("tool/requestUserInput", 0));
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "the non-approval request occupies id=0 ⇒ collision ⇒ tombstone"
        );
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), false));
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
        leg.observe_server_frame(&request_frame("tool/requestUserInput", 0));
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "an unconfirmed non-approval request tombstones on first observe"
        );
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0));
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), false));
    }

    #[test]
    fn unknown_non_approval_id_bearing_request_tombstones_the_id() {
        // A generic unknown non-`/requestApproval` id-bearing request occupies its id ⇒
        // Tombstoned (fail-closed default), so a later same-id approval is unanswerable.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(&request_frame("some/unknown/serverRequest", 0));
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(leg.bound_entry(&RequestId::Int(0)).is_none());
        // A later phone approval reusing id=0 cannot resurrect it.
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-C", 0));
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), false));
    }

    #[test]
    fn approval_missing_thread_id_tombstones_the_id() {
        // An approval-family frame (a `*/requestApproval` with a top-level id) but with NO
        // `params.threadId` has no arbiter key, yet it still occupies the id ⇒ Tombstoned
        // (occupy, permanently unanswerable) rather than being silently skipped.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(&format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"params":{{"itemId":"x"}}}}"#
        ));
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), false));
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
            r#"{"method":"thread/status/changed","params":{"threadId":"th-A"}}"#,
        );
        leg.observe_server_frame(
            r#"{"method":"serverRequest/resolved","params":{"threadId":"th-A","requestId":0}}"#,
        );
        assert!(leg.view.is_empty(), "notifications occupy no id");
        // A later approval at id=0 is a clean Bound and authorizes (not spuriously tombstoned).
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0));
        assert!(leg.bound_entry(&RequestId::Int(0)).is_some());
        assert!(leg.authorize(Role::Ccd, &RequestId::Int(0), false));
    }

    #[test]
    fn over_long_thread_id_approval_tombstones_and_stores_nothing() {
        // Finding 5 (memory bound): an approval whose threadId exceeds MAX_THREAD_ID_BYTES is
        // tombstoned (the id is occupied but no oversized string is retained), so per-entry
        // memory stays bounded. It is unanswerable.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        let long = "t".repeat(MAX_THREAD_ID_BYTES + 1);
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, &long, 0));
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "over-long threadId tombstones"
        );
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_none(),
            "no LegEntry (and so no oversized string) is stored"
        );
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), false));
        // A threadId exactly at the cap is still a clean Bound (boundary is inclusive).
        let at_cap = "t".repeat(MAX_THREAD_ID_BYTES);
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, &at_cap, 1));
        assert!(leg.bound_entry(&RequestId::Int(1)).is_some());
    }

    #[test]
    fn tombstone_is_installed_before_a_panicking_sink_runs() {
        // Finding 6 (exception safety): the Tombstoned state is committed to the view BEFORE
        // the log/event sink is invoked, so a panicking sink can never leave the old Bound
        // alive (resurrectable). A sink that panics on its first call models the hostile case.
        use std::panic::{catch_unwind, AssertUnwindSafe};
        let sink: EventSink = Arc::new(|_: &str| panic!("event sink panicked"));
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), sink);
        // First observe is a clean Bound — no ambiguity log fires, so the sink is not called.
        leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0));
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_some(),
            "first observe binds"
        );
        // Second observe collides ⇒ register installs Tombstoned, THEN logs (which panics).
        let result = catch_unwind(AssertUnwindSafe(|| {
            leg.observe_server_frame(&approval_frame(COMMAND_EXEC_APPROVAL, "thread-B", 0));
        }));
        assert!(result.is_err(), "the panicking sink unwinds");
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "the tombstone is committed before the sink runs — the old Bound is gone"
        );
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), false));
    }
}
