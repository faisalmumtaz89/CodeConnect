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
//!   bare-id alias routes:
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
//!   **Full cross-leg soundness is a 2e dependency, NOT solved here:** it additionally
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
use crate::session::ThreadBinding;

/// The two phone-supported approval `serverRequest` methods (server→client requests),
/// confirmed against the captured 0.147 frames in `fixtures/codex/` (see this module's
/// tests). A response to either is authorized for **both** ccd and TUI.
pub const COMMAND_EXEC_APPROVAL: &str = "item/commandExecution/requestApproval";
/// File-change approval — the second phone-supported family (see [`COMMAND_EXEC_APPROVAL`]).
pub const FILE_CHANGE_APPROVAL: &str = "item/fileChange/requestApproval";

/// The dispatch of one tool from an admitted `dynamicTools` bundle — the one answerable
/// server→client request that is not an approval.
///
/// It is in the pinned `ServerRequest` census of **both** bundles on **both** releases, so
/// it is not a 0.153 novelty; what 0.153 changed is that its TUI populates `dynamicTools`,
/// so the server now actually sends it.
pub const DYNAMIC_TOOL_CALL: &str = "item/tool/call";

/// The one tool namespace CodeConnect admits: the `codex_tui` bundle that
/// [`crate::fingerprint`] pins byte for byte on `thread/start`.
///
/// Pinning the NAMESPACE rather than the six tool names is deliberate and it is not a
/// looser check. Which tools may exist inside `codex_tui` is decided upstream, at
/// `thread/start`, where the bundle is admitted only as an exact captured value — a
/// seventh tool refuses the creation and the session never starts. What this constant has
/// to decide is a different question: whether the dispatch belongs to the bundle this
/// broker admitted at all. A namespace that is anything else (including absent or null,
/// which is how a non-bundle tool call is spelled) was never measured and is tombstoned.
pub const ADMITTED_TOOL_NAMESPACE: &str = "codex_tui";

/// The one turn TERMINAL the wire produces, whatever its status.
///
/// A3 measured exactly one per turn (7/7), with the vocabulary
/// `completed | interrupted | failed`. [`crate::session`] releases its busy mark on the
/// same frame and by the same reasoning.
const TURN_TERMINAL: &str = "turn/completed";

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

/// Bounded retention: the maximum stored `threadId` length. Real codex thread ids are
/// UUIDs (~36 chars), so this is orders of magnitude of slack; an approval whose
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

/// What the relay must do with one observed server→client frame.
///
/// # Why an unanswerable request must not be DELIVERED
///
/// The broker used to relay every s2c frame and, separately, decide whether the client's
/// answer to it could ever forward. When those two disagreed the protocol was stranded
/// mid-exchange: the client received a request, answered it correctly, and the answer was
/// dropped — so the app-server never learned the request had finished. Measured on 0.153,
/// that is exactly what hung a turn at "Working…" with no way out (see
/// [`crate::refusal`]'s `refuse_request`), and it was not specific to the tool dispatch:
/// **every** id-bearing non-approval server request has the same shape, so
/// `item/tool/requestUserInput` and whatever codex adds next would strand identically.
///
/// So the decision is made once, in one place. A request this broker will not let the
/// client answer is answered by the BROKER, upstream, with a JSON-RPC error — the
/// app-server closes the exchange cleanly, the turn proceeds or fails cleanly, and the
/// client is never handed something it cannot reply to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S2cDisposition {
    /// Relay it to the client unchanged. Everything that is not an unanswerable
    /// id-bearing request: notifications, responses, and the requests this broker binds.
    Deliver,
    /// Do NOT deliver. Send these bytes to the APP-SERVER instead, closing the request it
    /// opened.
    AnswerUpstream(String),
    /// **The leg can service nothing further: close it.**
    ///
    /// Reached when an s2c frame could not be classified at all — oversized, or rejected
    /// for a duplicate member — which POISONS the leg: it may have been an id-bearing
    /// request whose id we could not occupy, so from that moment [`LegCapabilities::authorize`]
    /// fails closed for every id here.
    ///
    /// Continuing to deliver on a poisoned leg is the hang again, one step removed. A
    /// later clean approval or tool call still registers `Bound`, so it would be
    /// delivered — and then its answer would forward zero bytes, because the poison gate
    /// runs first. The client would be handed an exchange whose answer is guaranteed to be
    /// discarded, which is exactly the shape `AnswerUpstream` exists to prevent.
    ///
    /// Answering the poisoning frame upstream is not available: we could not read its id,
    /// so there is nothing to answer. Closing is the only fail-closed move left, and it is
    /// honest — the leg reconnects, and a leg that reconnects has an empty, unpoisoned
    /// view.
    CloseLeg(&'static str),
}

/// The error frame the broker answers an unadmitted server request with.
///
/// `-32601` (method not available) rather than a policy refusal: from the app-server's
/// point of view the accurate statement is that this client cannot service the method, and
/// the code it already understands for that is the method-unavailable one. The detail
/// carries no client- or server-chosen text — only the fixed sentence and the id it
/// answers — for the same reason [`crate::redact`] governs the audit log.
fn unanswerable_error(id: &RequestId) -> String {
    serde_json::json!({
        "id": id.to_value(),
        "error": {
            "code": -32601,
            "message": "request not serviceable through the CodeConnect broker"
        }
    })
    .to_string()
}

/// Is this `item/tool/call` a dispatch from the bundle THIS SESSION admitted, for a tool
/// that bundle declares, naming this session's bound thread and its active turn?
///
/// Every clause is load-bearing, and each closes a different way the previous check —
/// "the method is `item/tool/call` and the namespace says `codex_tui`" — was satisfiable
/// without the model ever having been handed the tools it claims:
///
/// * **the session declared the bundle.** A session created with `dynamicTools: null` gave
///   the model no tools at all; a dispatch arriving in one is not a tool the model could
///   have called.
/// * **the tool is one the bundle declares.** Derived from the captured fixture
///   ([`crate::fingerprint::admitted_tool_names`]), so an undeclared seventh name is
///   unanswerable even though its namespace matches.
/// * **the thread is this session's, and the turn is ACTIVE.** A dispatch is part of a
///   running turn on the thread the broker bound; one naming another thread, or a turn
///   that has already ended, is not a tool call this session is in the middle of.
fn tool_call_is_from_the_admitted_bundle(
    v: &serde_json::Value,
    threads: &crate::session::SessionThreads,
) -> bool {
    let Some(params) = v.get("params") else {
        return false;
    };
    let str_of = |k: &str| params.get(k).and_then(serde_json::Value::as_str);
    if str_of("namespace") != Some(ADMITTED_TOOL_NAMESPACE) {
        return false;
    }
    if !threads.admitted_tool_bundle() {
        return false;
    }
    if !str_of("tool").is_some_and(|t| crate::fingerprint::admitted_tool_names().contains(t)) {
        return false;
    }
    let (Some(thread), Some(turn)) = (str_of("threadId"), str_of("turnId")) else {
        return false;
    };
    threads.is_session_thread(thread) && threads.is_active_turn(thread, turn)
}

/// Which endpoint roles an observed `serverRequest` grants a one-use response capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grant {
    /// A phone-supported family (command execution, file change): both the ccd phone
    /// answer and the TUI keyboard answer are granted.
    CcdAndTui,
    /// A non-phone approval family (`item/permissions/requestApproval`), the fail-closed
    /// default for any other/unknown `*/requestApproval`, and [`DYNAMIC_TOOL_CALL`]: only
    /// the TUI may answer. (Other non-approval id-bearing requests — `item/tool/requestUserInput`,
    /// elicitation — are not bound at all; the observer tombstones them, so they never
    /// reach `Grant`.)
    TuiOnly,
}

impl Grant {
    /// Classify an answerable `serverRequest` method into its grant. Invoked only for the
    /// families [`LegCapabilities::classify_request`] binds.
    ///
    /// Fail-closed: only the two confirmed phone-supported approvals grant ccd; every other
    /// approval is TUI-only, so an unknown/future approval can never wrongly hand the phone a
    /// capability.
    ///
    /// **A tool call is TUI-only, and that is the load-bearing half of admitting it.** Its
    /// answer is not a user decision, it is the TUI's own report of what running the tool
    /// did — and that report is handed straight to the MODEL as tool output. Granting the
    /// phone here would let a phone fabricate a tool result and inject arbitrary text into
    /// the model's context, which is a strictly larger capability than any approval answer.
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
    grant: Grant,
    generation: u64,
    /// The turn this capability belongs to, for a dynamic-tool dispatch.
    ///
    /// `None` for an approval: an approval is a USER decision about one action and its
    /// one-use slot is the arbiter's, so it does not end when a turn does. A tool
    /// dispatch is different — it is a step INSIDE a turn, admitted only because that turn
    /// was running ([`tool_call_is_from_the_admitted_bundle`]), so it must not outlive it.
    turn: Option<String>,
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
    Bind {
        thread_id: String,
        grant: Grant,
        /// See [`LegEntry::turn`].
        turn: Option<String>,
    },
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
    /// through a frame the observer failed to occupy.
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
    pub fn observe_server_frame(
        &mut self,
        threads: &crate::session::SessionThreads,
        text: &str,
    ) -> S2cDisposition {
        // Size gate, checked before any parse: it bounds worst-case parse+alloc AND marks the
        // frame unclassifiable. The cap sits above the largest legit s2c frame, so a frame over
        // it cannot be parsed to learn whether it is a request or which bare id it occupies —
        // it might be an id-bearing request we then fail to occupy, so poison the leg (fail
        // closed leg-wide) rather than silently skip. Forwarding is unaffected.
        // A poisoned leg is already closing; nothing more may cross it.
        if self.poisoned {
            return S2cDisposition::CloseLeg("the capability leg is poisoned");
        }
        if text.len() > MAX_OBSERVE_FRAME_BYTES {
            self.poison("oversized s2c frame (> MAX_OBSERVE_FRAME_BYTES)");
            return S2cDisposition::CloseLeg("an oversized s2c frame could not be classified");
        }
        // Parse with the same duplicate-member discipline the c2s classifier uses: a frame
        // with a duplicate member (at any nesting) is ambiguous between our parse and the
        // app-server's, so it yields no trustworthy method/id. It too may be an id-bearing
        // request we cannot occupy precisely, so poison the leg rather than silently skip.
        let Some(v) = crate::message::parse_no_dup_value(text) else {
            self.poison("NoDup-rejected s2c frame (unparseable / duplicate member)");
            return S2cDisposition::CloseLeg(
                "an unparseable or duplicate-member s2c frame could not be classified",
            );
        };
        // **The broker's own namespace, refused at the origin.** The whole value of
        // `codeconnect/` is that this broker is its only author, and the s2c direction is
        // otherwise an unconditional passthrough — so an upstream frame wearing the prefix
        // would be relayed verbatim and read by `ccd` as this broker's own word about a
        // request it is still waiting on. Dropping it silently would leave the leg talking
        // to something that just forged this broker's voice, so it fails closed the way
        // every other untrustworthy s2c frame does: poison the view, close the leg. One
        // `starts_with` on a method this path already has in hand.
        if v.get("method")
            .and_then(|m| m.as_str())
            .is_some_and(|m| m.starts_with(crate::relay::CODECONNECT_NAMESPACE))
        {
            self.poison("s2c frame in the broker's reserved `codeconnect/` namespace");
            return S2cDisposition::CloseLeg(
                "an s2c frame claimed the broker's reserved `codeconnect/` namespace",
            );
        }
        // Only a frame that occupies a bare RESPONSE id concerns us: it must carry a usable
        // top-level `id`. No top-level id ⇒ a notification (method-only ⇒ no client response)
        // or a non-id-bearing frame ⇒ occupies nothing ⇒ ignore.
        // **A turn TERMINAL revokes the dispatches that belonged to it**, before anything
        // else is decided about this frame. See [`Self::revoke_turn`].
        if v.get("method").and_then(|m| m.as_str()) == Some(TURN_TERMINAL) {
            if let (Some(thread), Some(turn)) = (
                v.pointer("/params/threadId").and_then(|t| t.as_str()),
                v.pointer("/params/turn/id").and_then(|t| t.as_str()),
            ) {
                self.revoke_turn(thread, turn);
            }
        }
        let Some(id) = v.get("id").and_then(RequestId::from_value) else {
            return S2cDisposition::Deliver;
        };
        // id-bearing, but only a frame that ALSO carries a `method` is a server→client
        // request (or an id-bearing hybrid). A method-less `{id, result|error}` is a plain
        // response (the server answering a client request) — it does not occupy the
        // server-request id space and c2s responses go through `authorize`, so ignore it.
        let Some(method) = v.get("method").and_then(|m| m.as_str()) else {
            return S2cDisposition::Deliver;
        };
        // From here the frame OCCUPIES bare id `id`: it MUST be registered — `Bind` if it is
        // a clean answerable family, else `Tombstone` — never silently skipped.
        let occupancy = Self::classify_request(&v, method, threads);
        let bound = matches!(occupancy, Occupancy::Bind { .. });
        self.register(id.clone(), occupancy, method);
        // **Do not DELIVER a request the client will never be allowed to answer.** See
        // [`S2cDisposition`]: an unanswerable request that reaches the client strands the
        // exchange the SERVER opened, which is the hang C7 was.
        //
        // `bound` is not enough on its own: `register` tombstones a SECOND occupant of an
        // id even when this frame classified cleanly, so the view is what decides.
        if bound && matches!(self.view.get(&id), Some(IdState::Bound(_))) {
            S2cDisposition::Deliver
        } else {
            S2cDisposition::AnswerUpstream(unanswerable_error(&id))
        }
    }

    /// Classify a method-bearing, id-bearing s2c frame into how it occupies its bare id.
    /// Every such frame occupies the id; the only question is `Bind` vs `Tombstone`.
    ///
    /// # The two answerable families, and why the second one had to be added
    ///
    /// The approvals were the whole set until the 0.153 re-grounding, and tombstoning
    /// everything else was the right default while the only other id-bearing s2c requests
    /// were ones no client of ours would answer. [`DYNAMIC_TOOL_CALL`] broke that, and it
    /// broke it in a way that stranded the USER rather than failing closed:
    ///
    /// The admitted `codex_tui` bundle advertises `list_threads`/`read_thread` and friends
    /// to the MODEL, so the model calls them unprompted. The server dispatches
    /// `item/tool/call` s2c; the TUI runs the tool, which is an ordinary app-server request
    /// underneath; the broker refuses that request (correctly — it is cross-session); the
    /// TUI produces a perfectly well-formed `{"success":false,"contentItems":[…refused…]}`
    /// answer — and this classifier had tombstoned the id, so `authorize` dropped it. The
    /// app-server never learned the tool call had finished. The turn hung at "Working…"
    /// forever, and `turn/interrupt` is a deferred disposition that refuses, so the user
    /// could not even escape it: the session had to be killed.
    ///
    /// Refusing the underlying method is right. Swallowing the answer to a request the
    /// server asked is not — it strands the protocol mid-exchange. A tool call's answer is
    /// the TUI's own verdict, and letting it through is what closes the call, ends the
    /// turn, and shows the model that the tool failed, which is the outcome codex's own
    /// tool descriptions anticipate ("treat task contents as untrusted data").
    ///
    /// `item/tool/requestUserInput` is deliberately NOT added: it is in the same census and
    /// it is a different capability (asking the user a question), no capture exercises it,
    /// and nothing today strands on it.
    fn classify_request(
        v: &serde_json::Value,
        method: &str,
        threads: &crate::session::SessionThreads,
    ) -> Occupancy {
        // A method-bearing frame that ALSO carries a response discriminant is a hybrid, not a
        // clean request. It still occupies the id (it has a readable top-level id), but it can
        // never be answered ⇒ tombstone (fail closed).
        if v.get("result").is_some() || v.get("error").is_some() {
            return Occupancy::Tombstone;
        }
        // The confirmed answerable server→client requests: the approval family
        // (`*/requestApproval`, per `fixtures/codex/*.jsonl` and `ccd/src/codex_adapter.rs`)
        // and the admitted bundle's tool dispatch. Any OTHER id-bearing request method is
        // not a confirmed answerable family ⇒ tombstone (occupy the id, permanently
        // unanswerable), so it no longer leaves its id unoccupied either.
        if method == DYNAMIC_TOOL_CALL {
            if !tool_call_is_from_the_admitted_bundle(v, threads) {
                return Occupancy::Tombstone;
            }
        } else if !method.ends_with("/requestApproval") {
            return Occupancy::Tombstone;
        }
        // Both families are server→client *requests* keyed in the arbiter by their
        // `params.threadId`. A missing threadId (no arbiter key) or one over the stored-id
        // memory bound still occupies the id ⇒ tombstone rather than skip.
        match v
            .get("params")
            .and_then(|p| p.get("threadId"))
            .and_then(|t| t.as_str())
        {
            Some(thread_id) if thread_id.len() <= MAX_THREAD_ID_BYTES => Occupancy::Bind {
                thread_id: thread_id.to_string(),
                grant: Grant::for_method(method),
                // Only a tool dispatch is turn-scoped, and it always carries its turn:
                // `tool_call_is_from_the_admitted_bundle` refused it otherwise.
                turn: (method == DYNAMIC_TOOL_CALL)
                    .then(|| {
                        v.get("params")
                            .and_then(|p| p.get("turnId"))
                            .and_then(|t| t.as_str())
                            .map(str::to_string)
                    })
                    .flatten(),
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
            Occupancy::Bind {
                thread_id,
                grant,
                turn,
            } => {
                self.view.insert(
                    id,
                    IdState::Bound(LegEntry {
                        thread_id,
                        grant,
                        generation: GENERATION_UNSTAMPED,
                        turn,
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

    /// **Revoke every dynamic-tool capability that belonged to a turn that has ended.**
    ///
    /// A tool dispatch is admitted only because its turn was RUNNING — the bundle was
    /// declared, the thread is the session's, and the turn is active. That check happens
    /// once, when the request is observed. Without this, the binding outlives the check:
    /// bind a dispatch, interrupt the turn, and a late TUI result still authorizes and
    /// forwards, handing the model tool output for a turn that is over. Repeated
    /// interrupted calls also accumulate entries toward [`MAX_TRACKED_IDS`].
    ///
    /// Fired on `turn/completed` of **any** status. A3 measured exactly one terminal per
    /// turn (7/7) with the vocabulary `completed | interrupted | failed`, and all three end
    /// the turn — reading the status and revoking only on `completed` would leave an
    /// interrupted turn's dispatches live, which is the case this exists for.
    ///
    /// The entry is REMOVED rather than tombstoned, and that is not the "never rebind"
    /// rule being bent. Tombstoning is for an id whose provenance became AMBIGUOUS while
    /// both claimants might still be live; here the server has authoritatively said the
    /// turn is over, so a later dispatch at the same bare id belongs to a new turn and
    /// nothing about it is ambiguous. Tombstoning would instead poison that id for the
    /// rest of the session and break every subsequent tool call on it.
    ///
    /// Approvals are deliberately untouched: an approval is a USER decision about one
    /// action, its one-use slot lives in the shared arbiter, and it is not a step inside a
    /// turn. See [`LegEntry::turn`].
    fn revoke_turn(&mut self, thread: &str, turn: &str) {
        let doomed: Vec<RequestId> = self
            .view
            .iter()
            .filter_map(|(id, state)| match state {
                IdState::Bound(e) if e.thread_id == thread && e.turn.as_deref() == Some(turn) => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        for id in doomed {
            self.view.remove(&id);
            (self.log)(&format!(
                "capability revoked (its turn ended): id={id:?} thread={thread:?}"
            ));
        }
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

    /// How many bare ids this leg is still tracking — the quantity
    /// [`MAX_TRACKED_IDS`] bounds, and the one a capability that outlived its turn would
    /// grow without limit.
    #[cfg(test)]
    fn tracked_ids(&self) -> usize {
        self.view.len()
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
                //
                // **"won" here is the RESERVATION.** The write behind it has not been
                // attempted yet; `capability confirmed` (or `capability released`) is the
                // line that says what became of it. The wording is unchanged because live
                // gates assert this exact substring.
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

    /// A session with nothing bound: no thread, no turn, no admitted bundle. What every
    /// approval test wants, because an approval's answerability does not depend on any of
    /// them.
    fn no_session() -> crate::session::SessionThreads {
        crate::session::SessionThreads::new("/work")
    }

    const TOOL_THREAD: &str = "01a0-head";
    const TOOL_TURN: &str = "01a0-turn";

    /// A session in the state a real tool dispatch arrives in: the captured bundle
    /// admitted at creation, a bound thread, and one ACTIVE turn.
    ///
    /// Driven through the production paths — the creation response installs the binding,
    /// `try_admit_turn` marks the turn, and the turn response answers it — so the state
    /// this asserts against is the state a live session reaches, not one assembled by
    /// hand.
    fn tool_session() -> crate::session::SessionThreads {
        use crate::session::{ConnId, ThreadBinding, TurnAdmission};
        let threads = crate::session::SessionThreads::new("/work");
        crate::session::ThreadBinding::note_tool_bundle_admitted(&threads);
        let conn = ConnId(1);
        // The creation claim, then its correlated response — the same two steps the relay
        // takes, so the binding this installs is the one production installs.
        assert_eq!(
            threads.try_admit_request(conn, &RequestId::Str("start-1".into()), "thread/start"),
            crate::session::IdAdmission::Admitted
        );
        threads.observe_server_frame(
            conn,
            &serde_json::json!({"id": "start-1",
                "result": {"thread": {"id": TOOL_THREAD}, "cwd": "/work",
                           "runtimeWorkspaceRoots": ["/work"]}})
            .to_string(),
        );
        let id = RequestId::Str("turn-1".into());
        assert_eq!(
            threads.try_admit_turn(
                conn,
                &id,
                TOOL_THREAD,
                Some(&serde_json::json!("/work")),
                Some(&serde_json::json!(["/work"])),
            ),
            TurnAdmission::Admitted,
            "the turn must be admitted for the dispatch to belong to one"
        );
        threads.observe_server_frame(
            conn,
            &serde_json::json!({"id": "turn-1", "result": {"turn": {"id": TOOL_TURN}}}).to_string(),
        );
        assert!(threads.is_active_turn(TOOL_THREAD, TOOL_TURN));
        threads
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
        ccd.observe_server_frame(&no_session(), &frame);
        tui.observe_server_frame(&no_session(), &frame);

        // ccd answers first → authorized; the TUI sibling then loses (revoked), zero bytes.
        assert!(ccd.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(!tui.authorize(Role::Tui, &RequestId::Int(0), false));
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

    /// **What a ccd leg can and cannot be handed, for every non-phone family the
    /// bundle declares.**
    ///
    /// The daemon's approval observer can only ever see what this decides, and
    /// the two halves of the decision are easy to conflate: whether a request is
    /// DELIVERED, and whether the leg that receives it may ANSWER. They differ
    /// exactly here — the permissions family is delivered to both legs and
    /// answerable only at the TUI — and a reader who assumed delivery followed
    /// the grant would conclude the daemon never sees it, while one who assumed
    /// the grant followed delivery would build a phone answer for it.
    ///
    /// So all three are asserted together, on a ccd leg, in one place:
    ///
    /// * `item/permissions/requestApproval` ends in `/requestApproval`, so it
    ///   binds and is DELIVERED; its grant is TUI-only, so a ccd answer forwards
    ///   nothing.
    /// * `item/tool/requestUserInput` and `mcpServer/elicitation/request` do
    ///   not, so they are tombstoned and answered UPSTREAM — never delivered to
    ///   any client, which is what stops the exchange the server opened being
    ///   stranded on a client that may not reply.
    ///
    /// **Mutation:** widen `classify_request`'s family test to any method
    /// containing `request` and the last two become `Deliver`, handing the
    /// client two exchanges whose answers this leg would discard.
    #[test]
    fn a_ccd_leg_is_handed_the_permissions_family_and_never_the_other_two() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut ccd = LegCapabilities::new(Arc::clone(&arb), silent());
        assert_eq!(
            ccd.observe_server_frame(
                &no_session(),
                &approval_frame("item/permissions/requestApproval", "thread-P", 0)
            ),
            S2cDisposition::Deliver,
            "a permissions request reaches the ccd leg; the daemon's observer can \
             only decline to card a frame it is actually handed"
        );

        for never_delivered in [
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
        ] {
            let mut ccd = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
            let frame = format!(
                r#"{{"id":0,"method":"{never_delivered}","params":{{"threadId":"thread-P","turnId":"t","itemId":"i"}}}}"#
            );
            match ccd.observe_server_frame(&no_session(), &frame) {
                S2cDisposition::AnswerUpstream(answer) => {
                    let v: serde_json::Value = serde_json::from_str(&answer).unwrap();
                    assert_eq!(v["error"]["code"], -32601);
                }
                other => panic!("{never_delivered} must not reach a client: {other:?}"),
            }
            assert!(
                !ccd.authorize(Role::Ccd, &RequestId::Int(0), false),
                "and nothing it was never handed is answerable by it either"
            );
        }
    }

    #[test]
    fn observe_only_family_refuses_ccd_authorizes_tui() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut ccd = LegCapabilities::new(Arc::clone(&arb), silent());
        let mut tui = LegCapabilities::new(Arc::clone(&arb), silent());
        let frame = approval_frame("item/permissions/requestApproval", "thread-P", 0);
        ccd.observe_server_frame(&no_session(), &frame);
        tui.observe_server_frame(&no_session(), &frame);

        // ccd can never answer an observe-only family (zero bytes); the TUI still can.
        assert!(!ccd.authorize(Role::Ccd, &RequestId::Int(0), false));
        assert!(tui.authorize(Role::Tui, &RequestId::Int(0), false));
    }

    /// One `item/tool/call` s2c frame, in the shape MEASURED off a real 0.153 session.
    fn tool_call_frame(namespace: &str, thread: &str, id: i64) -> String {
        tool_call_with(namespace, thread, TOOL_TURN, "list_threads", id)
    }

    /// One `item/tool/call`, with every field the tool-dispatch rules read made explicit.
    fn tool_call_with(namespace: &str, thread: &str, turn: &str, tool: &str, id: i64) -> String {
        let ns = if namespace == "null" {
            "null".to_string()
        } else {
            format!("\"{namespace}\"")
        };
        format!(
            r#"{{"id":{id},"method":"item/tool/call","params":{{"arguments":{{"limit":5}},"callId":"exec-1","namespace":{ns},"threadId":"{thread}","tool":"{tool}","turnId":"{turn}"}}}}"#
        )
    }

    /// **The TUI's answer to an admitted bundle's tool call must reach the app-server.**
    ///
    /// This is the hang: the model calls a bundle tool, the broker refuses the method
    /// underneath (correctly), the TUI produces a well-formed failure result — and if this
    /// id is tombstoned, that result is dropped, the app-server never learns the call
    /// finished, and the turn hangs forever with `turn/interrupt` refused. Answerability
    /// is what ends the turn.
    #[test]
    fn an_admitted_bundles_tool_call_is_answerable_by_the_tui() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut tui = LegCapabilities::new(Arc::clone(&arb), silent());
        let session = tool_session();
        assert_eq!(
            tui.observe_server_frame(
                &session,
                &tool_call_frame(ADMITTED_TOOL_NAMESPACE, TOOL_THREAD, 0),
            ),
            S2cDisposition::Deliver,
            "a dispatch the client MAY answer must reach it"
        );
        assert!(
            tui.authorize(Role::Tui, &RequestId::Int(0), false),
            "the TUI's tool result must forward, or the turn never completes"
        );
        // One-use, exactly like an approval: a second answer to the same call is a
        // duplicate and forwards zero bytes.
        assert!(!tui.authorize(Role::Tui, &RequestId::Int(0), false));
    }

    /// **The phone may never answer a tool call.** A tool result is handed straight to the
    /// model as tool output, so granting ccd here would let a phone inject arbitrary text
    /// into the model's context — a strictly larger capability than any approval answer.
    #[test]
    fn a_tool_call_is_never_answerable_by_the_phone() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut ccd = LegCapabilities::new(Arc::clone(&arb), silent());
        let mut tui = LegCapabilities::new(Arc::clone(&arb), silent());
        let session = tool_session();
        let frame = tool_call_frame(ADMITTED_TOOL_NAMESPACE, TOOL_THREAD, 0);
        ccd.observe_server_frame(&session, &frame);
        tui.observe_server_frame(&session, &frame);
        assert!(!ccd.authorize(Role::Ccd, &RequestId::Int(0), false));
        // …and refusing the phone did not consume the slot the TUI still needs.
        assert!(tui.authorize(Role::Tui, &RequestId::Int(0), false));
    }

    /// Only the bundle this broker admitted. A tool call from any other namespace — or
    /// one with no namespace at all, which is how a non-bundle tool call is spelled — is
    /// a dispatch nobody measured, and it stays unanswerable.
    #[test]
    fn a_tool_call_outside_the_admitted_namespace_is_tombstoned() {
        for namespace in ["some_other_bundle", "null", "", "codex_tui "] {
            let arb = Arc::new(ResponseArbiter::new());
            let mut tui = LegCapabilities::new(arb, silent());
            tui.observe_server_frame(&tool_session(), &tool_call_frame(namespace, TOOL_THREAD, 0));
            assert!(
                !tui.authorize(Role::Tui, &RequestId::Int(0), false),
                "namespace {namespace:?} must not be answerable"
            );
        }
        // A tool call with no `threadId` has no arbiter key, so it is tombstoned too.
        let arb = Arc::new(ResponseArbiter::new());
        let mut tui = LegCapabilities::new(arb, silent());
        tui.observe_server_frame(&no_session(), r#"{"id":0,"method":"item/tool/call","params":{"namespace":"codex_tui","tool":"list_threads"}}"#,
        );
        assert!(!tui.authorize(Role::Tui, &RequestId::Int(0), false));
    }

    /// **The dispatch must belong to the bundle THIS session admitted, to a tool that
    /// bundle declares, and to the running turn on the bound thread.**
    ///
    /// The namespace alone was satisfiable without any of that: a `codex_tui` call could
    /// name an undeclared seventh tool, arrive in a session created with
    /// `dynamicTools: null` where the model was handed nothing, or name a thread or a turn
    /// that is not the one running. Each arm below is one of those.
    #[test]
    fn a_tool_call_must_belong_to_the_admitted_bundle_and_the_active_turn() {
        let unanswerable = |session: &crate::session::SessionThreads, frame: &str| {
            let arb = Arc::new(ResponseArbiter::new());
            let mut tui = LegCapabilities::new(arb, silent());
            assert!(
                matches!(
                    tui.observe_server_frame(session, frame),
                    S2cDisposition::AnswerUpstream(_)
                ),
                "must not be delivered: {frame}"
            );
            assert!(!tui.authorize(Role::Tui, &RequestId::Int(0), false));
        };
        let session = tool_session();

        // A SEVENTH tool the captured bundle never declared.
        unanswerable(
            &session,
            &tool_call_with(
                ADMITTED_TOOL_NAMESPACE,
                TOOL_THREAD,
                TOOL_TURN,
                "exfiltrate_everything",
                0,
            ),
        );
        // A session whose creation declared NO bundle: the model was handed no tools, so
        // there is no dispatch of theirs to answer.
        let no_bundle = {
            use crate::session::{ConnId, IdAdmission, ThreadBinding, TurnAdmission};
            let t = crate::session::SessionThreads::new("/work");
            let conn = ConnId(1);
            assert_eq!(
                t.try_admit_request(conn, &RequestId::Str("start-1".into()), "thread/start"),
                IdAdmission::Admitted
            );
            t.observe_server_frame(
                conn,
                &serde_json::json!({"id": "start-1",
                    "result": {"thread": {"id": TOOL_THREAD}, "cwd": "/work",
                               "runtimeWorkspaceRoots": ["/work"]}})
                .to_string(),
            );
            let id = RequestId::Str("turn-1".into());
            assert_eq!(
                t.try_admit_turn(
                    conn,
                    &id,
                    TOOL_THREAD,
                    Some(&serde_json::json!("/work")),
                    Some(&serde_json::json!(["/work"])),
                ),
                TurnAdmission::Admitted
            );
            t.observe_server_frame(
                conn,
                &serde_json::json!({"id": "turn-1", "result": {"turn": {"id": TOOL_TURN}}})
                    .to_string(),
            );
            t
        };
        unanswerable(
            &no_bundle,
            &tool_call_frame(ADMITTED_TOOL_NAMESPACE, TOOL_THREAD, 0),
        );
        // A thread this session does not own.
        unanswerable(
            &session,
            &tool_call_frame(ADMITTED_TOOL_NAMESPACE, "01a0-a-stranger", 0),
        );
        // A turn that is not the active one — an ended turn, or one invented.
        unanswerable(
            &session,
            &tool_call_with(
                ADMITTED_TOOL_NAMESPACE,
                TOOL_THREAD,
                "01a0-some-other-turn",
                "list_threads",
                0,
            ),
        );
    }

    /// **An unanswerable server request is ANSWERED UPSTREAM, not delivered.**
    ///
    /// The C7 hang was not specific to the tool dispatch: every id-bearing non-approval
    /// s2c request has the same shape — delivered to the client, tombstoned here, answer
    /// dropped, exchange stranded. The fix is structural, so it is asserted over the whole
    /// class rather than over the one method that happened to fire.
    #[test]
    fn an_unanswerable_server_request_is_answered_upstream_instead_of_delivered() {
        for method in [
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
            "attestation/generate",
            "some/future/serverRequest",
        ] {
            let arb = Arc::new(ResponseArbiter::new());
            let mut tui = LegCapabilities::new(arb, silent());
            let frame = format!(
                r#"{{"id":0,"method":"{method}","params":{{"threadId":"{TOOL_THREAD}"}}}}"#
            );
            match tui.observe_server_frame(&tool_session(), &frame) {
                S2cDisposition::AnswerUpstream(answer) => {
                    let v: serde_json::Value = serde_json::from_str(&answer).unwrap();
                    assert_eq!(v["id"], 0, "the answer must close the id the server opened");
                    assert_eq!(v["error"]["code"], -32601);
                    // Fixed vocabulary only — no client- or server-chosen text.
                    assert!(!answer.contains(method), "{answer}");
                }
                other => panic!("{method} must not be delivered: {other:?}"),
            }
        }
        // …and a NOTIFICATION, a response, and an admitted approval are all still
        // delivered, or this would be a rule that swallows the wire.
        let arb = Arc::new(ResponseArbiter::new());
        let mut tui = LegCapabilities::new(arb, silent());
        for deliverable in [
            r#"{"method":"thread/started","params":{"thread":{"id":"x"}}}"#.to_string(),
            r#"{"id":9,"result":{"ok":true}}"#.to_string(),
            approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0),
        ] {
            assert_eq!(
                tui.observe_server_frame(&no_session(), &deliverable),
                S2cDisposition::Deliver,
                "{deliverable}"
            );
        }
    }

    /// **A late answer to an id that has since been reused must not consume the new
    /// capability.**
    ///
    /// The existing tests cover an unsolicited answer and an immediate duplicate. This is
    /// the sequential case: request A takes id 0, a NEW request takes id 0 later, and A's
    /// answer arrives after that. A bare response carries no provenance, so neither can be
    /// safely matched — and the collision tombstones the id, which is what makes both
    /// unanswerable rather than letting the stale one consume the live one.
    #[test]
    fn a_late_answer_to_a_reused_id_consumes_nothing() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut tui = LegCapabilities::new(Arc::clone(&arb), silent());
        let session = tool_session();
        // A: bound, and answerable while it stands alone.
        assert_eq!(
            tui.observe_server_frame(
                &session,
                &tool_call_frame(ADMITTED_TOOL_NAMESPACE, TOOL_THREAD, 0)
            ),
            S2cDisposition::Deliver
        );
        // B: the SAME bare id, later. The collision tombstones it — and, now, B is not
        // delivered either, because the client could never answer it.
        assert!(matches!(
            tui.observe_server_frame(
                &session,
                &tool_call_with(
                    ADMITTED_TOOL_NAMESPACE,
                    TOOL_THREAD,
                    TOOL_TURN,
                    "read_thread",
                    0
                )
            ),
            S2cDisposition::AnswerUpstream(_)
        ));
        // A's answer, arriving late: it consumes nothing, so B's slot cannot be spent by
        // a response that belonged to A.
        assert!(
            !tui.authorize(Role::Tui, &RequestId::Int(0), false),
            "a late answer to a reused id must forward zero bytes"
        );
    }

    /// **A poisoned leg CLOSES; it does not go on delivering.**
    ///
    /// Poisoning makes `authorize` fail closed for every id here — so a later clean
    /// approval or tool call would still register `Bound`, still be delivered, and then
    /// have its answer discarded. That is the delivered-then-unanswerable hang again, one
    /// step removed from the one C7 fixed. There is nothing to answer upstream either: the
    /// poisoning frame's id is precisely what could not be read.
    #[test]
    fn a_poisoned_leg_closes_instead_of_delivering_what_it_cannot_service() {
        for poisoning in [
            // Oversized: unclassifiable, so it may have been an id-bearing request.
            format!(r#"{{"method":"x/notify","params":{{"blob":"{}"}}}}"#, "a".repeat(
                MAX_OBSERVE_FRAME_BYTES
            )),
            // A duplicate member at any depth: ambiguous between our parse and the
            // server's, so it yields no trustworthy id either.
            r#"{"id":0,"method":"item/commandExecution/requestApproval","params":{"threadId":"th-A","threadId":"th-B"}}"#
                .to_string(),
        ] {
            let arb = Arc::new(ResponseArbiter::new());
            let mut leg = LegCapabilities::new(arb, silent());
            assert!(
                matches!(
                    leg.observe_server_frame(&no_session(), &poisoning),
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
                    leg.observe_server_frame(&no_session(), &clean),
                    S2cDisposition::CloseLeg(_)
                ),
                "a poisoned leg must not deliver an exchange whose answer it will discard"
            );
            // …and its answer would indeed have been discarded, which is the whole point.
            assert!(!leg.authorize(Role::Ccd, &RequestId::Int(0), false));
        }
    }

    /// **The reserved namespace is closed to upstream, whatever the method under it.**
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
    fn any_method_in_the_reserved_namespace_closes_the_leg() {
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
                    leg.observe_server_frame(&no_session(), forged),
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
        ccd.observe_server_frame(&no_session(), &solicits);
        tui.observe_server_frame(&no_session(), &solicits);

        // Nothing has answered yet, so there is nobody to name.
        assert_eq!(ccd.recorded_winner(&RequestId::Int(0)), None);
        // The keyboard takes the slot; the phone lost it and can say so — but only once
        // the keyboard's write is proven. A reservation still in flight names nobody,
        // which is what stops a phone being told "the Mac answered" about a write that
        // may yet fail.
        assert!(tui.authorize(Role::Tui, &RequestId::Int(0), false));
        assert!(!ccd.authorize(Role::Ccd, &RequestId::Int(0), false));
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

    /// **A dispatch does not outlive its turn.**
    ///
    /// Answerability is checked once, when the request is observed — the bundle was
    /// declared, the thread is the session's, the turn is running. Without revocation the
    /// BINDING outlives that check: bind a dispatch, interrupt the turn, and a late TUI
    /// result still authorizes and forwards, handing the model tool output for a turn that
    /// is over.
    #[test]
    fn a_tool_capability_is_revoked_when_its_turn_ends() {
        // Every terminal status, because an interrupted turn is exactly the case that
        // motivated this and the one a status-reading revoke would have missed.
        for status in ["completed", "interrupted", "failed"] {
            let arb = Arc::new(ResponseArbiter::new());
            let mut tui = LegCapabilities::new(arb, silent());
            let session = tool_session();
            assert_eq!(
                tui.observe_server_frame(
                    &session,
                    &tool_call_frame(ADMITTED_TOOL_NAMESPACE, TOOL_THREAD, 0)
                ),
                S2cDisposition::Deliver,
                "{status}: the dispatch binds while its turn runs"
            );

            let terminal = serde_json::json!({
                "method": "turn/completed",
                "params": {"threadId": TOOL_THREAD,
                           "turn": {"id": TOOL_TURN, "status": status}}
            })
            .to_string();
            assert_eq!(
                tui.observe_server_frame(&session, &terminal),
                S2cDisposition::Deliver,
                "{status}: the terminal itself is a notification and still reaches the client"
            );

            assert!(
                !tui.authorize(Role::Tui, &RequestId::Int(0), false),
                "{status}: a late tool result must forward zero bytes once the turn is over"
            );
            // The entry is GONE, not tombstoned: repeated interrupted calls must not
            // accumulate toward the per-leg id cap, and the next turn's dispatch at the
            // same bare id must still be able to bind.
            assert_eq!(tui.tracked_ids(), 0, "{status}: the entry is not retained");
        }
    }

    /// Revocation is scoped: another turn's dispatch, and an approval, are untouched.
    #[test]
    fn revoking_a_turn_leaves_other_capabilities_alone() {
        let arb = Arc::new(ResponseArbiter::new());
        let mut tui = LegCapabilities::new(arb, silent());
        let session = tool_session();
        tui.observe_server_frame(
            &session,
            &tool_call_frame(ADMITTED_TOOL_NAMESPACE, TOOL_THREAD, 0),
        );
        // An approval on the same thread: a USER decision, not a step inside the turn.
        tui.observe_server_frame(
            &session,
            &approval_frame(COMMAND_EXEC_APPROVAL, TOOL_THREAD, 1),
        );

        // A terminal for a DIFFERENT turn revokes nothing.
        let other = serde_json::json!({
            "method": "turn/completed",
            "params": {"threadId": TOOL_THREAD,
                       "turn": {"id": "01a0-a-different-turn", "status": "completed"}}
        })
        .to_string();
        tui.observe_server_frame(&session, &other);
        assert_eq!(
            tui.tracked_ids(),
            2,
            "a different turn's terminal revokes nothing"
        );

        // This turn's terminal takes the dispatch and leaves the approval.
        let mine = serde_json::json!({
            "method": "turn/completed",
            "params": {"threadId": TOOL_THREAD, "turn": {"id": TOOL_TURN, "status": "interrupted"}}
        })
        .to_string();
        tui.observe_server_frame(&session, &mine);
        assert_eq!(
            tui.tracked_ids(),
            1,
            "the approval survives its turn's terminal"
        );
        assert!(
            tui.authorize(Role::Tui, &RequestId::Int(1), false),
            "the approval is still answerable"
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
            tui.observe_server_frame(&no_session(), &format!(
                r#"{{"id":0,"method":"{method}","params":{{"threadId":"thread-A","namespace":"codex_tui"}}}}"#
            ));
            assert!(
                !tui.authorize(Role::Tui, &RequestId::Int(0), false),
                "{method} must stay unanswerable"
            );
        }
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
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
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
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
        leg.observe_server_frame(
            &no_session(),
            &approval_frame("item/permissions/requestApproval", "thread-B", 0),
        );
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
        leg.observe_server_frame(&no_session(), &frame);
        leg.observe_server_frame(&no_session(), &frame); // duplicate observation ⇒ collision
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
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
        assert!(
            leg.authorize(Role::Ccd, &RequestId::Int(0), false),
            "clean Bound authorizes once"
        );
        // A later same-id observation tombstones the (already-consumed) id.
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-B", 0),
        );
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
        leg.observe_server_frame(
            &no_session(),
            &approval_frame("item/permissions/requestApproval", "th-P", 0),
        );
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-C", 0),
        );
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
        leg.observe_server_frame(
            &no_session(),
            &approval_frame("some/future/requestApproval", "th-U", 0),
        );
        assert_eq!(
            leg.bound_entry(&RequestId::Int(0)).unwrap().grant,
            Grant::TuiOnly,
            "unknown approval registers as the fail-closed TUI-only default"
        );
        // A phone-family reuse of id=0 tombstones it rather than upgrading or preserving.
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-C", 0),
        );
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
        leg.observe_server_frame(&no_session(), &frame);
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
        leg.observe_server_frame(&no_session(), &escaped);
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-B", 0),
        );
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
        leg.observe_server_frame(&no_session(), &format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"id":1,"params":{{"threadId":"th-A"}}}}"#
        ));
        assert!(leg.is_poisoned(), "a NoDup-rejected frame poisons the leg");
        // Duplicate `method` also fails NoDup (idempotent poison, still registers nothing).
        leg.observe_server_frame(&no_session(), &format!(
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
                &no_session(),
                &approval_frame(COMMAND_EXEC_APPROVAL, "th-clean", 9),
            ),
            S2cDisposition::CloseLeg(_)
        ));
        assert!(leg.bound_entry(&RequestId::Int(9)).is_none());
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
        leg.observe_server_frame(&no_session(), &big_notification);
        assert!(
            !leg.is_poisoned(),
            "a large but in-cap notification parses and occupies nothing — no poison"
        );
        assert!(leg.view.is_empty(), "a notification occupies no id");
        // A later CLEAN phone approval is therefore still answerable (real traffic works).
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0),
        );
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
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0),
        );
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_some(),
            "id=0 binds cleanly first"
        );
        // Now poison via an oversized (unclassifiable) frame.
        let pad = "Z".repeat(MAX_OBSERVE_FRAME_BYTES + 1);
        leg.observe_server_frame(&no_session(), &format!(
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
        // top-level id, so it OCCUPIES that id ⇒ Tombstoned. It never grants a capability,
        // and a later same-id approval can never alias through it.
        let arb = Arc::new(ResponseArbiter::new());
        let mut leg = LegCapabilities::new(arb, silent());
        leg.observe_server_frame(&no_session(), &format!(
            r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"params":{{"threadId":"th-A"}},"result":{{}}}}"#
        ));
        leg.observe_server_frame(&no_session(), &format!(
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
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-B", 0),
        );
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
        leg.observe_server_frame(&no_session(), &big);
        assert!(leg.is_poisoned(), "an oversized frame poisons the leg");
        assert!(leg.view.is_empty(), "and still registers nothing");
        // A subsequently-observed CLEAN phone approval at ANY id is not delivered at all:
        // the leg closes. It used to bind and be relayed while its response forwarded ZERO
        // bytes, which is the delivered-then-unanswerable shape.
        assert!(matches!(
            leg.observe_server_frame(
                &no_session(),
                &approval_frame(COMMAND_EXEC_APPROVAL, "th-clean", 7),
            ),
            S2cDisposition::CloseLeg(_)
        ));
        assert!(
            leg.bound_entry(&RequestId::Int(7)).is_none(),
            "nothing binds on a poisoned leg"
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
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
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
        leg.observe_server_frame(&no_session(), &frame);
        assert!(leg.view.is_empty());
    }

    #[test]
    fn per_leg_id_cap_stops_growth_and_leaves_new_ids_unanswerable() {
        // Bounded retention: fill the view to the cap with distinct ids, then a NEW id is
        // not inserted (stays Unseen ⇒ unanswerable), while an EXISTING id still tombstones.
        let mut leg = LegCapabilities::new(Arc::new(ResponseArbiter::new()), silent());
        for i in 0..MAX_TRACKED_IDS as i64 {
            leg.observe_server_frame(
                &no_session(),
                &approval_frame(COMMAND_EXEC_APPROVAL, "th", i),
            );
        }
        assert_eq!(leg.view.len(), MAX_TRACKED_IDS);
        // A brand-new id past the cap is not tracked → unanswerable (fail closed).
        let over = MAX_TRACKED_IDS as i64;
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th", over),
        );
        assert_eq!(
            leg.view.len(),
            MAX_TRACKED_IDS,
            "the view does not grow past the cap"
        );
        assert!(!leg.authorize(Role::Ccd, &RequestId::Int(over), false));
        // An already-tracked id can still transition to Tombstoned (no growth).
        leg.observe_server_frame(
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
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0),
        );
        for _ in 0..(AMBIGUITY_LOG_BUDGET as usize + 50) {
            leg.observe_server_frame(
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
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
        assert!(
            leg.bound_entry(&RequestId::Int(0)).is_some(),
            "A binds first"
        );
        leg.observe_server_frame(&no_session(), &request_frame("tool/requestUserInput", 0));
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
        leg.observe_server_frame(&no_session(), &request_frame("tool/requestUserInput", 0));
        assert!(
            leg.is_tombstoned(&RequestId::Int(0)),
            "an unconfirmed non-approval request tombstones on first observe"
        );
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "thread-A", 0),
        );
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
        leg.observe_server_frame(
            &no_session(),
            &request_frame("some/unknown/serverRequest", 0),
        );
        assert!(leg.is_tombstoned(&RequestId::Int(0)));
        assert!(leg.bound_entry(&RequestId::Int(0)).is_none());
        // A later phone approval reusing id=0 cannot resurrect it.
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-C", 0),
        );
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
        leg.observe_server_frame(
            &no_session(),
            &format!(r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"params":{{"itemId":"x"}}}}"#),
        );
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
            &no_session(),
            r#"{"method":"thread/status/changed","params":{"threadId":"th-A"}}"#,
        );
        leg.observe_server_frame(
            &no_session(),
            r#"{"method":"serverRequest/resolved","params":{"threadId":"th-A","requestId":0}}"#,
        );
        assert!(leg.view.is_empty(), "notifications occupy no id");
        // A later approval at id=0 is a clean Bound and authorizes (not spuriously tombstoned).
        leg.observe_server_frame(
            &no_session(),
            &approval_frame(COMMAND_EXEC_APPROVAL, "th-A", 0),
        );
        assert!(leg.bound_entry(&RequestId::Int(0)).is_some());
        assert!(leg.authorize(Role::Ccd, &RequestId::Int(0), false));
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
        assert!(!leg.authorize(Role::Tui, &RequestId::Int(0), false));
        // A threadId exactly at the cap is still a clean Bound (boundary is inclusive).
        let at_cap = "t".repeat(MAX_THREAD_ID_BYTES);
        leg.observe_server_frame(
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
                &no_session(),
                &approval_frame(COMMAND_EXEC_APPROVAL, "thread-B", 0),
            );
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
