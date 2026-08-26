//! Session-scoped thread binding: **lineage, not receipt** (A4 / finding 5; P1 and P3 of
//! the codex review of 2e-4a, hardened by round 2's P1/P2/P3/P4).
//!
//! An ownership-free `thread/resume`, and every `turn/start`, must name a thread whose
//! policy this broker actually proved. The store this replaces bound a thread id from **bare
//! receipt** of a `thread/started` / `thread/resumed` notification off the server→client
//! stream. That is not lineage: a notification is an announcement, and nothing in it
//! proves the thread answers a creation THIS broker admitted. The lineage argument the
//! fingerprint module states — "a bound thread's policy was fingerprint-proven at
//! creation" — was therefore *aspirational*. This module now makes it true.
//!
//! ## The binding is a correlated, admitted creation
//!
//! Binding is a two-step transition, and BOTH steps happen inside this broker:
//!
//! 1. **Admit** — [`ThreadBinding::try_admit_request`] is called from
//!    [`crate::refusal::classify`] at the moment it decides to FORWARD a `thread/start`,
//!    and only then. It atomically claims the session's single creation slot and records
//!    the `(connection, request id)` as **pending**. This mirrors the existing
//!    impure-through-the-env idiom (`capabilities.authorize` likewise consumes a one-use
//!    capability inside `classify`). A request the broker refused is never pending, so it
//!    can never bind.
//! 2. **Verify** — [`SessionThreads::observe_server_frame`] watches the s2c stream **of the
//!    connection that made the request** (hence the [`ConnId`] parameter) for the RESPONSE
//!    whose id matches the pending entry. Only that correlated response can install a
//!    binding, and only if it carries all three proofs: `result.thread.id`, `result.cwd`
//!    (**equal to the coordinator-owned launch cwd**), and a well-typed non-empty
//!    `result.runtimeWorkspaceRoots`.
//!
//! `thread/started` / `thread/resumed` notifications may only ever **CONFIRM** an existing
//! verified binding — an announcement of the id we already bound is a no-op. They may
//! never seed one, so the observer ignores every method-bearing frame outright; "confirm"
//! is a no-op by construction, not a code path.
//!
//! ## Correlation is CONNECTION-scoped, not role-scoped (round-2 P1)
//!
//! The pending key used to be `(Role, RequestId)`. Two connections of the SAME role
//! collide under that key — and the TUI's `/resume` picker really does open a second TUI
//! connection. Connection B answering `id: 1` could then satisfy connection A's pending and
//! install a binding derived from B's response. The relay now mints a monotonic
//! [`ConnId`] per accepted connection and threads it through both the c2s classifier
//! ([`crate::refusal::Env::conn`]) and the s2c observer, exactly as the per-leg
//! [`crate::response_capability::LegCapabilities`] view already flows. The key is
//! `(ConnId, RequestId)`, so a cross-connection answer correlates to nothing.
//!
//! Request-id **reuse and replay on one connection** is the same class of defect, and is
//! closed by three per-connection id sets:
//!
//! * **outstanding** — see the next section: EVERY forwarded request's id, with the method
//!   it belongs to;
//! * **reservation** — an id that is currently in flight as a *creation* cannot be claimed
//!   a second time on that connection (defense-in-depth; see [`ConnIds::reserved`]);
//! * **tombstone** — an id whose pending was *consumed* (installed, reopened or closed) is
//!   permanently spent on that connection: a replayed response for it can never re-install,
//!   and a fresh creation may not reuse it either.
//!
//! All three are **bounded** ([`MAX_CONN_REQUEST_IDS`] creation ids and
//! [`MAX_OUTSTANDING_REQUESTS`] outstanding ids per connection,
//! [`MAX_TRACKED_CONNECTIONS`] connections) so a hostile client cannot grow them without
//! limit; past any bound the request simply refuses (fail closed). A connection's sets
//! are dropped when it disconnects — its ids can only ever be replayed on its own (now
//! dead) s2c stream, so retaining them past the connection buys nothing.
//!
//! ## Correlation needs EVERY forwarded id, not just the creation's (round-3 P1)
//!
//! Round 2 registered only `thread/start` ids. That left a cross-method collision open: a
//! *different* forwarded request on the same connection could carry the SAME id as the
//! pending creation, and then
//!
//! * a method-less response to THAT request — a perfectly ordinary answer the client
//!   solicited — matched the pending entry and could install a binding, or
//! * an ordinary `error` answering that unrelated request re-opened the creation slot
//!   mid-flight.
//!
//! Both are closed by widening the ledger from creation-only to
//! **every FORWARDED request**. [`ThreadBinding::try_admit_request`] is called for every
//! request the classifier is about to forward that carries a usable id, and records
//! `id → the request's ACTUAL method` on that connection. Three consequences:
//!
//! 1. **An id already outstanding on the connection cannot be reused.** A client that
//!    pipelines two live requests under one id has made its own responses uncorrelatable,
//!    which is protocol-hostile; the frame is DROPPED (zero upstream bytes), the leg is kept
//!    open, and the event is COUNTED ([`IdLedgerCounts::reused_in_flight`]). The counters
//!    are the seam the failure-containment / spam-close sub-chunk reads: it owns the
//!    per-leg threshold at which repeated protocol-hostile frames close a leg, which is why
//!    this module counts rather than closes.
//! 2. **A response RELEASES its entry**, so the id is usable again the moment it is
//!    answered — which is exactly what a real client does (see the compatibility note on
//!    [`ThreadBinding::try_admit_request`]).
//! 3. **A creation response is one that satisfies all three of**: its id matches the pending
//!    creation's; the outstanding entry it releases is the *creation request*
//!    (`thread/start`) and not some other method that happened to share the id; and no other
//!    outstanding request still holds that id. The first two make the collision unusable;
//!    the third is belt-and-braces (rule 1 already makes a second holder unrepresentable,
//!    and `outstanding` is a map keyed BY the id, so it can hold at most one entry per id).
//! 4. **The RELEASE is validated, not assumed** (round-4 P1): only a frame the header scan
//!    proves is a response — exclusive `result`, or an exclusive well-formed JSON-RPC error
//!    object — may remove an entry. See [`SessionThreads::observe_server_frame`].
//!
//! ### Two side paths the ledger deliberately does not cover
//!
//! "Every forwarded request" is the exact claim; it is NOT "every request", and two side
//! paths make the difference. Neither forwards a byte, and neither compromises binding:
//!
//! * **A request the classifier REFUSES never registers an id.** The ledger runs LAST in
//!   `crate::refusal::classify_request` and only on a `Forward`, so a refused frame occupies
//!   no id at all — which is what lets the measured TUI retry a policy-refused `thread/start`
//!   under the very same id. Binding is unaffected because a request that forwarded zero
//!   bytes can never be answered, so there is nothing for it to correlate to.
//! * **A duplicate `thread/start` is caught by the CREATION SLOT, not by id reuse.** In
//!   [`ThreadBinding::try_admit_request`] the `creation != Creation::Open` test runs BEFORE
//!   the outstanding-reuse test, so a second `thread/start` — same id or a fresh one — is
//!   refused as [`IdAdmission::CreationSlotClosed`] and never reaches the reuse path. The
//!   reuse path is therefore not what enforces single-creation; the slot is (P3). Binding is
//!   unaffected, and the client gets a synthetic policy error it can act on rather than a
//!   silent drop.
//!
//! ## A client-chosen id is length-capped before it is stored (round-3 P6)
//!
//! Tombstones, reservations and the outstanding ledger all store client-chosen id bytes. In
//! addition to the per-set entry caps, a single id is capped at
//! [`MAX_REQUEST_ID_BYTES`] — MEASURED: real request ids run 1–59 bytes, the longest being
//! `"startup-thread-start-9747f04e-f467-466f-96dd-b6872bd77820"` (a fixed prefix plus a
//! UUID). An over-long id is refused and counted, and is NEVER stored.
//!
//! ## The creation state machine (round-2 P2)
//!
//! The old code dropped the pending on ANY correlated response and re-opened creation on
//! any `error`. That is too loose: a partial result is not evidence that the server failed.
//! [`Creation`] is now explicit, and a consumed pending lands in exactly one of three
//! places:
//!
//! * an **exclusive, structurally valid JSON-RPC error** (an `error` OBJECT present AND no
//!   `result` member) ⇒ the creation provably failed ⇒ **REOPEN**, because a legitimately
//!   failed `thread/start` must be retryable;
//! * a **valid result** carrying all three proofs ⇒ **INSTALL** the binding;
//! * **ANYTHING ELSE** — a partial result (missing/ill-typed proof), a result whose `cwd`
//!   is not the launch cwd, neither member, `error: null`, a non-object `error`, or both
//!   members present ⇒ [`Creation::Closed`], an **indeterminate** state that neither
//!   installs nor reopens. The rationale is the single-thread invariant: on such a response
//!   the server may in fact have created a thread, so reopening creation could produce a
//!   SECOND thread. Refusing to reopen is the only choice that cannot break the invariant.
//!
//! `Closed` is **terminal until reconnect evidence**. Pre-D2 the session is wedged-*safe*:
//! no turn can be authorized (nothing is bound) and no second creation can be admitted.
//! D2's recovery — the thread-switch latch, quiesce and reconciliation against the
//! server's own thread list — is what owns the richer "find out what actually happened"
//! path; this module deliberately does not guess.
//!
//! ## Pending is claimed around a PROVEN send (round-2 P3)
//!
//! `try_admit_request` claims the slot inside `classify`, before any byte moves. If the
//! relay's upstream write then FAILS the request never reached the server, so the claim is
//! rolled back with [`ThreadBinding::rollback_creation`] and creation re-opens (no
//! tombstone: zero bytes went out, so nothing is ambiguous). If instead the owning
//! connection **disconnects** while a creation is still pending,
//! [`ThreadBinding::close_connection`] transitions it to the same `Closed` state as P2 —
//! the request DID go out, so its fate is unknown. A pending is therefore never stranded
//! "in flight" forever, and never reopened without evidence.
//!
//! ## Single-ACTIVE-thread session invariant (P3, as evolved by 2e-4c)
//!
//! The rule used to be "one thread per session": `try_admit_request` refused a
//! `thread/start` unless the creation state was [`Creation::Open`]. That made a real user
//! unable to press `/new`, which is not a security property — it is a missing feature that
//! looked like one.
//!
//! 2e-4c splits the rule in two and keeps the half that was doing the work:
//!
//! * **One creation IN FLIGHT at a time — unchanged, still load-bearing.** A creation
//!   admitted while another is `Pending` is refused. This is what kills the pipeline race:
//!   two `thread/start`s in flight before either response lands would otherwise both be
//!   admitted and the second response would silently re-point the head. Claim-and-record is
//!   a single atomic step under one mutex, so there is no check-then-act window either.
//! * **One ACTIVE thread at a time — the new half.** A `thread/start` admitted while a
//!   thread is `Bound` is a **SWITCH**: the new thread becomes the sole active head and the
//!   old one is RETIRED (see [`Binding::retired`]). Retired is not forgotten — a retired
//!   thread stays resumable, it just stops being turnable.
//!
//! ### Why admitting the second creation is safe
//!
//! MEASURED (2e-4c spike, real codex 0.147 `--remote` TUI driven through `/new`): the
//! second `thread/start` is **byte-identical** to the session's first, on the SAME
//! connection, preceded by two `thread/unsubscribe{threadId: <active>}` frames. It carries
//! the same `approvalPolicy`, `approvalsReviewer`, `sandbox` and `runtimeWorkspaceRoots`,
//! so it passes the identical fingerprint assertion against the identical launch
//! fingerprint, and its response is verified against the identical coordinator-owned launch
//! cwd. A switch therefore proves everything a first creation proves. Anything that does
//! NOT match that shape — a differing ownership field, a `cwd` naming another workspace, a
//! second creation while one is pending — still refuses, unchanged.
//!
//! ### The linearization that IS here, and the fence that is not (round-1 P1/P2)
//!
//! D2 is one design covering two different actuations, and 2e-4c splits it along the line
//! of what can actually be produced today.
//!
//! **IN — the turn-vs-switch fence.** `turn/start` is a producible actuation: the TUI sends
//! one on every turn, and it is authorized against the ACTIVE head. So a switch and a turn
//! genuinely can race, and the fence is built (see [`TurnActivity`]): the head-check,
//! workspace check, id ledger and busy-mark are ONE atomic decision, and a switch is
//! refused while any turn is busy. No approval machinery is needed for it, because its
//! subject is a turn.
//!
//! **DEFERRED — the approval-answer fence.** D3's acknowledged ccd quiesce, and the half of
//! D2 that drains ccd writes before the switch forwards, exist for one thing: making sure
//! no ccd WRITE admitted for the old generation reaches the app-server after the switch. In
//! the observation-only world this broker relays today, **ccd has no such writes** — its
//! entire upstream vocabulary is `initialize`, `initialized`, `thread/resume` and
//! `thread/unsubscribe`, none of which actuates anything. The quiesce would be a barrier in
//! front of an empty queue, and every gate D3 names ("answer-before vs answer-after the
//! actor transition", "post-commit poison N-reply") describes an approval answer that
//! cannot exist. It is deferred to Phase 3 **together with its subject**, not merely
//! postponed.
//!
//! **A BOUND PHASE-3 OBLIGATION, recorded here because it is easy to miss.** When approvals
//! arrive, a response CAPABILITY minted while thread A was the head can still be live after
//! B becomes the head — the capability registry is keyed by the server's request id, and
//! [A4 measured that the same `server_request_id` is answerable from any epoch]. Exercising
//! it after the switch would answer an A-era approval under a B-era session. So Phase 3
//! must REVOKE or re-scope outstanding capabilities at the switch boundary; this is exactly
//! the `visit_request_key` scoping D4 specifies, and it becomes load-bearing the moment a
//! capability exists to scope. Today the registry has no producer, so there is nothing to
//! revoke and nothing to test.
//!
//! ## The workspace anchor is COORDINATOR-owned (round-2 P4)
//!
//! `cwd`/`roots` are still read from the creation **RESPONSE** (the measured reason is
//! below), but a response is only the server echoing back what the client asked for — so on
//! its own it anchors nothing. The anchor is [`crate::fingerprint::LaunchFingerprint`]'s
//! `launch_cwd`: the cwd the COORDINATOR launched this session in, plumbed
//! coordinator → `internal-codex-host` argv (`--launch-cwd`) → broker exactly like the
//! other four fingerprint dimensions. A creation response whose `cwd` is not that string
//! binds nothing, so a client cannot name a workspace of its own choosing.
//!
//! ### Canonicalization lives at the coordinator, and this is measured
//!
//! Measured: the coordinator passes `--cwd /tmp`; the app-server reports the resolved
//! `/private/tmp` (macOS `/tmp` is a symlink). Exact equality of those two strings is
//! FALSE; `realpath` equality is TRUE. The fix is to canonicalize **exactly once, at the
//! coordinator** — the authority that owns the launch cwd — before the path enters the host
//! argv. The broker then performs pure **exact string equality** and stays a comparator
//! with no filesystem access at all (it runs on paths a client controls; a normalizer here
//! would be both a syscall surface and a second, disagreeing notion of path identity).
//! There is deliberately no normalizer in this crate.
//!
//! ## What is bound, and the labeled 2e seam
//!
//! Exactly one thread per session, created through this broker under a fingerprint-asserted
//! `thread/start`. A resume of a thread this session never created — e.g. a pre-existing
//! thread the wrapper captured at bootstrap before the broker was relaying — is **refused**
//! (fail closed). Wiring that fuller lineage (the wrapper's bootstrap thread-identity
//! capture) is Phase 2e; until then an unbindable resume refuses, never forwards.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::message::RequestId;

/// The one method that creates the session's thread. Named once so the outstanding ledger's
/// "is this entry the creation request?" test and the classifier's claim site cannot drift.
pub const CREATION_METHOD: &str = "thread/start";

/// A monotonic per-**connection** instance id, minted by the relay for each accepted
/// connection (see [`crate::relay`]). Correlation of a creation response to the creation
/// request that solicited it is keyed by `(ConnId, RequestId)`, so two connections of the
/// same [`crate::allowlist::Role`] — the TUI `/resume` picker opens a second TUI
/// connection — can never satisfy each other's pending creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnId(pub u64);

impl std::fmt::Display for ConnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Per-connection cap on the combined reservation + tombstone id sets.
///
/// A legitimate session performs ONE creation, so one id is ever reserved and at most one
/// is ever tombstoned per connection; 64 is three orders of magnitude of slack over the
/// only flow that legitimately consumes several (a run of server-side creation failures,
/// each of which reopens creation and tombstones its id). The cap exists purely so a
/// hostile client cannot grow the sets without limit by looping "claim ⇒ fail ⇒ claim
/// again with a fresh id": past it, that connection's creations simply refuse, which is
/// fail-closed and costs a legitimate client nothing.
///
/// Reaching it yields [`IdAdmission::CreationSlotClosed`], **not**
/// [`IdAdmission::AtCapacity`] — only a `thread/start` can reach it, and a dropped
/// `thread/start` would leave the client waiting for an answer that never comes, so it is
/// answered with a synthetic policy error instead. See [`IdAdmission::CreationSlotClosed`]
/// for the full distinction between the two refusal causes.
const MAX_CONN_REQUEST_IDS: usize = 64;

/// Cap on the number of connections whose id sets are tracked at once.
///
/// Entries are removed on disconnect ([`ThreadBinding::close_connection`]), so this bounds
/// *concurrently live* connections rather than lifetime connections; 1024 is far above the
/// two-to-three legs a real session opens (TUI, ccd, and the `/resume` picker's second
/// TUI). Past it a creation on an untracked connection refuses rather than allocating.
pub const MAX_TRACKED_CONNECTIONS: usize = 1024;

/// Per-connection cap on the OUTSTANDING (forwarded, not yet answered) request ids.
///
/// Sized against the measured client behaviour, not a guess. Every real client of this
/// broker mints a strictly increasing per-connection integer and **never pipelines**: the
/// ccd control link issues one request at a time (its `Attach` state machine makes a second
/// outstanding resume unrepresentable), and the TUI's widest burst is the 11-method
/// bootstrap census. 256 is more than an order of magnitude above that, so a real client
/// cannot reach it; a client that does has 256 unanswered requests in flight, which is not
/// a shape the wire has ever shown. Past the bound the request is dropped and counted
/// ([`IdLedgerCounts::at_capacity`]) rather than forwarded — fail closed.
///
/// Entries are released by their responses and the whole set is dropped on disconnect, so
/// this bounds *concurrently unanswered* requests, not lifetime requests.
pub const MAX_OUTSTANDING_REQUESTS: usize = 256;

/// The strict per-id byte cap for any client-chosen request id this broker STORES (P6).
///
/// MEASURED on the real wire: request ids run 1–59 bytes, the longest being
/// `"startup-thread-start-9747f04e-f467-466f-96dd-b6872bd77820"` (a fixed prefix plus a
/// UUID); every other observed client mints bare integers. 128 bytes is more than twice the
/// measured maximum — so it cannot refuse a real client — while making the ledger's memory
/// a product of three finite factors (`MAX_TRACKED_CONNECTIONS` ×
/// (`MAX_OUTSTANDING_REQUESTS` + `MAX_CONN_REQUEST_IDS`) × this cap) instead of being
/// unbounded in the length of one client-chosen string. An over-long id is refused and
/// counted, and is NEVER stored.
///
/// It applies only to the STRING form: [`RequestId::Int`] is a fixed-width `i64` with no
/// client-chosen length.
pub const MAX_REQUEST_ID_BYTES: usize = 128;

/// Is this id short enough to store? See [`MAX_REQUEST_ID_BYTES`].
fn id_within_cap(id: &RequestId) -> bool {
    match id {
        RequestId::Str(s) => s.len() <= MAX_REQUEST_ID_BYTES,
        RequestId::Int(_) => true,
    }
}

/// What the id ledger decided about a request the classifier is about to forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdAdmission {
    /// The id was recorded as outstanding on this connection; the frame may be forwarded.
    /// For a `thread/start` the same atomic step claimed the session's creation slot.
    Admitted,
    /// The id is ALREADY outstanding on this connection: the client is reusing an in-flight
    /// id, which makes its own responses uncorrelatable. Protocol-hostile — the frame is
    /// dropped (zero upstream bytes), the leg stays open, and the event is counted.
    ReusedInFlight,
    /// The id is longer than [`MAX_REQUEST_ID_BYTES`] and is therefore never stored (P6).
    /// Dropped and counted, exactly like [`Self::ReusedInFlight`].
    Oversized,
    /// This connection's outstanding ledger or the tracked-connection table is full.
    /// Dropped and counted — fail closed rather than growing.
    ///
    /// **Not the same refusal as [`Self::CreationSlotClosed`]**, and the difference is
    /// deliberate; see that variant.
    AtCapacity,
    /// `thread/start` only: the session's single creation slot is unavailable (a thread is
    /// bound, a creation is pending, the state is indeterminate, the id is spent on this
    /// connection, or this connection's creation-id sets are full). The caller answers with a
    /// synthetic policy error, as it always has.
    ///
    /// ## Telling the two refusal causes apart
    ///
    /// Both can mean "a bound was reached", so the distinction is the CALLER's contract, not
    /// the cause:
    ///
    /// * [`Self::AtCapacity`] — a VOLUME bound on ordinary traffic
    ///   ([`MAX_OUTSTANDING_REQUESTS`], [`MAX_TRACKED_CONNECTIONS`]). The frame is dropped
    ///   with zero bytes and **no answer**, the leg is kept open, and the event is counted
    ///   into [`IdLedgerCounts::at_capacity`] for the failure-containment seam. Silence is
    ///   fine here: a client this far past the measured maximum is already broken.
    /// * `CreationSlotClosed` — a POLICY state of the one thing a session may do once. It is
    ///   **answered** with a synthetic policy error carrying
    ///   [`ThreadBinding::creation_closed_reason`], and is **not** counted as a hostile event:
    ///   refusing a second creation is the normal, expected outcome of P3, not an attack.
    ///
    /// `MAX_CONN_REQUEST_IDS` is the case where a capacity condition is reported as
    /// `CreationSlotClosed` rather than `AtCapacity`, and that is on purpose: it can only be
    /// hit by a `thread/start`, and a `thread/start` that is silently dropped leaves a client
    /// waiting forever for a creation answer. It gets the error instead.
    CreationSlotClosed,
}

/// Counts of the protocol-hostile id events the ledger refuses, for the audit log.
///
/// **Seam.** These are the input the *failure-containment / spam-close* sub-chunk reads:
/// that sub-chunk owns the per-leg threshold at which repeated hostile frames close a leg
/// (the same seam the refusal matrix's "unknown-method spam" note names). This module
/// deliberately counts and keeps the leg open rather than deciding that policy itself — a
/// single reused id is a broken client, not proof of an attack, and the close decision needs
/// a rate the ledger cannot see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdLedgerCounts {
    /// Frames dropped because their id was already outstanding on the connection.
    pub reused_in_flight: u64,
    /// Frames dropped because their id exceeded [`MAX_REQUEST_ID_BYTES`].
    pub oversized: u64,
    /// Frames dropped because a ledger bound was reached.
    pub at_capacity: u64,
}

/// A thread whose creation this broker admitted AND whose creation response it verified.
///
/// `cwd` and `roots` are taken from the creation **RESPONSE**, not the request — see
/// [`SessionThreads::observe_server_frame`] for the measured reason — and the `cwd` is
/// additionally proven equal to the coordinator-owned launch cwd before it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedThread {
    /// `result.thread.id` of the creation response.
    pub id: String,
    /// `result.cwd` — the SERVER-RESOLVED working directory, proven equal to the launch cwd.
    pub cwd: Value,
    /// `result.runtimeWorkspaceRoots` — a non-empty array of non-empty path strings.
    pub roots: Value,
}

/// Oracle: what does this session's verified thread binding say?
pub trait ThreadBinding: Send + Sync {
    /// The session's ONE verified thread, iff a creation was admitted, correlated and
    /// fully verified. `None` ⇒ fail closed.
    fn bound_thread(&self) -> Option<VerifiedThread>;

    /// Atomically admit ONE request the classifier has decided to forward: record its id as
    /// **outstanding** on `conn` together with the request's ACTUAL `method`, and — when
    /// that method is [`CREATION_METHOD`] — claim the session's single creation slot in the
    /// same step (round-3 P1; round-2 P3).
    ///
    /// [`IdAdmission::Admitted`] ⇒ forward. Every other verdict means **zero upstream
    /// bytes**: `CreationSlotClosed` is answered with a synthetic policy error (it is a
    /// policy refusal the client can act on), and the three hostile verdicts are dropped,
    /// counted, and the leg kept open.
    ///
    /// ## Compatibility (checked against the real clients before this rule shipped)
    ///
    /// Constraining every forwarded request is only safe if no legitimate client reuses an
    /// id while it is in flight. Verified by inspection of every id-emitting site that
    /// speaks to this broker: the ccd control link (`ccd::codex_link`) mints
    /// `next_id += 1` from a per-connection counter and its `Attach` state machine makes a
    /// second outstanding `thread/resume` unrepresentable (a retry mints a FRESH id, it
    /// never re-sends a stored one); the live-gate raw clients mint from per-connection
    /// counters in disjoint blocks and block on each response before the next request; and
    /// `codeconnect` emits no JSON-RPC at all (its only contact with a broker leg is a
    /// connect-and-close liveness probe). Counters reset to 1 per connection, which is
    /// exactly why the ledger is keyed by [`ConnId`] rather than by role or session.
    ///
    /// Note what is deliberately NOT tracked: client→server **responses** (approval
    /// answers), whose ids are server-chosen per-thread integers legitimately reused from 0
    /// — those are the [`crate::response_capability`] registry's business, and tracking them
    /// here would break the fanout.
    fn try_admit_request(&self, conn: ConnId, id: &RequestId, method: &str) -> IdAdmission;

    /// **Head-check, workspace-check, id-ledger and busy-mark, as ONE atomic decision**
    /// (round-1 P1).
    ///
    /// Before this existed the three were separate lock acquisitions from
    /// `crate::refusal`: read the head, read the workspace, then record the id. A switch
    /// admitted between the first and the last left the turn forwarded against a head that
    /// had already moved. They are now one critical section, and the same section marks the
    /// thread busy — which is what stops a switch being admitted between this decision and
    /// the relay's upstream write (see [`TurnActivity`]).
    ///
    /// The default implementation refuses everything, so a binding that does not implement
    /// it authorizes no turns.
    fn try_admit_turn(
        &self,
        _conn: ConnId,
        _id: &RequestId,
        _thread_id: &str,
        _cwd: Option<&Value>,
        _roots: Option<&Value>,
    ) -> TurnAdmission {
        TurnAdmission::NotTheHead {
            detail: "this binding authorizes no turns".to_string(),
        }
    }

    /// A turn provably did not start (its `turn/start` was answered with an error), or a
    /// terminal was observed for its thread. Releases the busy mark.
    fn release_turn(&self, _conn: ConnId, _id: &RequestId) {}

    /// **Would a switch be admitted RIGHT NOW?** (round-1 P4, as re-grounded.)
    ///
    /// Pure — it mutates nothing and claims nothing. It exists so the `thread/unsubscribe`
    /// prefix of a `/new` can be refused BEFORE it reaches the server, in every case where
    /// the `thread/start` behind it is already doomed.
    ///
    /// # Why a pre-check rather than a hold
    ///
    /// The defect is real: `/new` sends `unsubscribe, unsubscribe, thread/start`, and if
    /// the prefix forwards and the start is then refused, the TUI is left on the old thread
    /// and UNSUBSCRIBED from it — silently blind.
    ///
    /// The obvious fix is to HOLD the prefix and release it only when the start is
    /// admitted. That was built, run against the real TUI, and **MEASURED TO DEADLOCK**:
    /// the TUI awaits each unsubscribe's RESPONSE before sending the next frame, so a held
    /// prefix produces exactly one held frame and no `thread/start` ever arrives. The pane
    /// simply stops. Answering the held frame synthetically to unblock it would mean
    /// claiming an unsubscription that did not happen and then either forwarding bytes
    /// whose answer was already faked (duplicate ids on the wire) or never forwarding them
    /// at all — a fabrication either way, on a leg whose whole design is that it fabricates
    /// nothing.
    ///
    /// So the prefix forwards, and the failure is moved EARLIER instead: the unsubscribe is
    /// refused when the switch behind it cannot be admitted. That covers every cause the
    /// broker can know in advance — a turn is active, a switch is already in flight, the
    /// session is wedged, the retirement cap is full — and turns each from "silently
    /// unsubscribed and then refused" into one legible refusal with zero wire effect. The
    /// one cause it cannot cover is the app-server ERRORING the `thread/start`, which no
    /// amount of local checking can predict; that path restores the old head (so turns keep
    /// working) and is documented as the residual.
    /// **Reserve the next switch for `conn`, or say why it cannot be** (round-2 P3).
    ///
    /// Called when a `thread/unsubscribe` prefix is about to forward. It does two things
    /// under one lock, and both matter:
    ///
    /// 1. **Checks every precondition the `thread/start` behind it will face**, from the
    ///    single [`creation_preconditions`] definition — session state, the retirement cap,
    ///    active turns, AND the per-connection creation-id arithmetic the first form
    ///    omitted. A prefix must never forward in front of a creation that is already
    ///    doomed, because that is precisely how a connection ends up unsubscribed and then
    ///    refused.
    /// 2. **Claims the slot.** Between the prefix and the start, a competing turn or a
    ///    creation from another connection would otherwise be admitted and turn a
    ///    then-valid check into a then-invalid one. The reservation makes the prefix and
    ///    the switch one causal unit: while it is live, turns refuse
    ///    ([`TurnAdmission::SwitchReserved`]) and another connection's creation refuses.
    ///
    /// Idempotent for the same connection — the measured `/new` sends the prefix twice, and
    /// the second frame refreshes rather than colliding. Consumed by that connection's next
    /// `thread/start`, released by its disconnect, and ignored past
    /// [`SWITCH_RESERVATION_TTL`] so a client that goes quiet cannot wedge turns for ever.
    fn switch_prefix_admissible(&self, _conn: ConnId, _thread: &str) -> Result<(), &'static str> {
        Err("this binding admits no switch")
    }

    /// Claim the next creation for `conn`, whose `thread/unsubscribe` of the ACTIVE head
    /// has just been admitted to the ledger and is about to forward.
    ///
    /// **Called only AFTER that unsubscribe's own ledger admission succeeds** (round-3 P4).
    /// A prefix the ledger refuses sends zero bytes, so it must leave no reservation behind
    /// — a reservation with no wire effect fences turns and blocks other connections for
    /// nothing.
    fn reserve_switch(&self, _conn: ConnId, _thread: &str) {}

    /// A `thread/resume` naming `thread` is being FORWARDED on `conn` under request `id`.
    ///
    /// It does not clear the wedge (round-3 P6) — an attempt is not a subscription. The id
    /// is remembered, and the wedge clears only when that exact request is answered with a
    /// SUCCESS. A refused or errored resume subscribes nothing and must leave the
    /// connection wedged, or a client could lift it by asking and being told no.
    fn note_resubscribe_attempt(&self, _conn: ConnId, _thread: &str, _id: &RequestId) {}

    /// The running counts of protocol-hostile id events (round-3 P1/P6). See
    /// [`IdLedgerCounts`] for the failure-containment seam these feed.
    fn id_ledger_counts(&self) -> IdLedgerCounts {
        IdLedgerCounts::default()
    }

    /// Un-claim a creation whose bytes provably never left the broker (the relay's upstream
    /// write failed). Creation re-opens and the id is released rather than tombstoned:
    /// nothing went out, so nothing about it is ambiguous. A no-op unless `(conn, id)` is
    /// exactly the pending creation.
    fn rollback_creation(&self, conn: ConnId, id: &RequestId);

    /// The owning connection went away. A creation still pending on it DID reach the
    /// server, so its fate is unknown: it transitions to the indeterminate `Closed` state
    /// (never reopened). The connection's per-connection id sets are dropped.
    fn close_connection(&self, conn: ConnId);

    /// Why the creation slot is permanently closed, if it is (the indeterminate state).
    /// Surfaced in the audit note of a refused creation so an operator can tell "already
    /// bound" from "wedged safe". `None` for every other state.
    fn creation_closed_reason(&self) -> Option<&'static str> {
        None
    }

    /// The thread that IS the head, or is about to stop being it.
    ///
    /// While a switch is `Pending` there is no `Bound` head at all, so
    /// [`ThreadBinding::sole_session_thread`] answers `None` — and an unsubscribe naming the
    /// thread being superseded would look indistinguishable from a retired thread's
    /// cleanup, quietly forwarding a second prefix into a switch already in flight
    /// (round-3 P5). This accessor closes that window: during a switch the superseded
    /// thread still counts as the head for the purpose of "is this a switch prefix?", and
    /// the prefix is then refused by the one-creation-in-flight rule with zero bytes.
    fn head_or_superseded(&self) -> Option<String> {
        self.sole_session_thread()
    }

    /// Does a thread id belong to this session?
    fn is_session_thread(&self, thread_id: &str) -> bool {
        self.bound_thread().is_some_and(|t| t.id == thread_id)
    }

    /// The session's ONE **active** thread id — the head, and the only thread a
    /// `turn/start` may name.
    ///
    /// `None` when none is bound — fail closed. "Two heads at once" stays
    /// **unrepresentable** rather than merely checked: the store holds one [`Creation`]
    /// state, so at most one `Bound` thread exists at any instant. 2e-4c's switch does not
    /// weaken that — it REPLACES the head atomically at the moment a new creation verifies,
    /// moving the previous one to the retired list. The head-check reads this; a retired
    /// thread is deliberately invisible here, which is what makes it unturnable.
    fn sole_session_thread(&self) -> Option<String> {
        self.bound_thread().map(|t| t.id)
    }
}

/// How long a switch reservation stays valid without its `thread/start` (round-2 P3).
///
/// The measured `/new` sends `unsubscribe, unsubscribe, thread/start` back to back and
/// awaits each response, so the whole prefix-to-start window is a few round trips on a unix
/// socket — milliseconds. Ten seconds is three orders of magnitude of slack.
///
/// The bound exists because a held reservation REFUSES turns: a client that sent a prefix
/// and then went quiet must not be able to wedge the session's turns for ever. Past the
/// TTL the reservation is simply ignored (and cleared on the next read), which returns the
/// session to ordinary admission.
pub const SWITCH_RESERVATION_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// Record that `conn` is unsubscribed from `thread`, NEWEST-WINS (closing S5).
///
/// `or_insert` was wrong here: it kept whatever wedge was already in the slot, so a
/// connection that had recovered from an A wedge and then failed a B switch went on being
/// wedged against A — and B's failure, the one that just happened, was not recorded at all.
fn wedge(g: &mut Binding, conn: ConnId, thread: String) {
    g.wedge_seq += 1;
    let seq = g.wedge_seq;
    g.unsubscribed.insert(conn, Wedge { thread, seq });
}

/// Expire a reservation whose TTL has passed — and WEDGE the connection that made it
/// (round-3 P5).
///
/// Expiry is not "nothing happened". A reservation only exists because a
/// `thread/unsubscribe` naming the ACTIVE head really went upstream, so that connection is
/// provably no longer receiving the head's stream. If the `thread/start` it was holding the
/// slot for never arrives, silently restoring its turn authorization would authorize turns
/// nobody on that connection can observe — the same lie P4 closes on the errored-start
/// path, reached by a different route.
///
/// So expiry hands the connection to the same wedge, cleared the same way: by a correlated,
/// accepted `thread/resume`.
fn expire_reservation(g: &mut Binding) {
    let expired = match &g.switch_reservation {
        Some(res) if !res.is_live() => Some((res.conn, res.thread.clone())),
        _ => None,
    };
    if let Some((conn, thread)) = expired {
        g.switch_reservation = None;
        wedge(g, conn, thread);
    }
}

/// A margin past the TTL, so a backdated reservation is unambiguously expired rather than
/// racing the clock's resolution.
#[cfg(test)]
const COMFORTABLY_PAST: std::time::Duration = std::time::Duration::from_secs(1);

/// One connection's "unsubscribed and not back yet" wedge (closing S5).
///
/// Carries the thread AND a monotonic stamp. The stamp is what makes "an older result
/// cannot clear a newer wedge" expressible: a connection can unsubscribe from A, fail its
/// switch, later re-subscribe, then unsubscribe from B — and A's late resume answer must
/// not lift B's wedge. Slot writes are explicitly NEWEST-WINS rather than `or_insert`,
/// which silently kept an obsolete A wedge and let the newer B failure go unrecorded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wedge {
    thread: String,
    seq: u64,
}

/// One connection's claim on the next thread creation (round-2 P3).
#[derive(Debug, Clone)]
struct SwitchReservation {
    conn: ConnId,
    /// The ACTIVE head this connection unsubscribed from (round-3 P5). Only an unsubscribe
    /// naming the head reserves — a retired-thread cleanup unsubscribes something the
    /// session is not on and fences nothing — and this is the thread the connection is
    /// wedged against if the reservation expires without a `thread/start` consuming it.
    thread: String,
    at: std::time::Instant,
}

impl SwitchReservation {
    fn is_live(&self) -> bool {
        self.at.elapsed() < SWITCH_RESERVATION_TTL
    }
}

/// Cap on outstanding re-subscribe attempts across the session (closing S4).
///
/// Bounded for the same reason as every other ledger here: the entries are created by
/// client requests and released by their answers, so a client that asks and never lets the
/// answers land could otherwise grow it without limit. A wedged connection issues one
/// resume at a time; 256 is orders of magnitude past that, and past it an attempt simply is
/// not recorded — which leaves the wedge in place, the fail-closed direction.
pub const MAX_RESUBSCRIBE_ATTEMPTS: usize = 256;

/// How many settled terminal ids are remembered for duplicate suppression (round-3 P2).
///
/// One per turn, and only the recent ones matter: duplicates arrive inside one delivery
/// fan-out, never tens of turns later. 64 is far past that while keeping the set finite in
/// a session of any length.
pub const MAX_CLEARED_TURNS: usize = 64;

/// **Cap on concurrently admitted-but-unterminated `turn/start` requests** (round-2 P2).
///
/// The set is bounded by the same reasoning as every other ledger here: an entry is created
/// by a client request, and an unanswered entry is cleared only by its own error response
/// or its connection closing — so a client that pipelines turns and never lets them settle
/// could otherwise grow it without limit.
///
/// Eight is far above any measured client: the TUI issues one turn at a time and awaits it,
/// and its only multi-start shape is the implicit steer (D12), which is one extra request
/// against a running turn. Past the cap a `turn/start` is REFUSED with its own legible
/// cause rather than dropped — the client is waiting on an answer, and "too many turns in
/// flight" is something an operator can act on.
pub const MAX_ACTIVE_TURNS: usize = 8;

/// The strict byte cap on a thread id this broker STORES (round-1 P9).
///
/// MEASURED: every thread id on the wire is a lowercase UUID — `8-4-4-4-12` hex, exactly
/// 36 bytes — and `crate::redact::thread_id` already relies on that grammar. 64 bytes is
/// most of a factor of two above it, so it cannot refuse a real id, while making the
/// retired list's memory a product of two finite factors ([`MAX_RETIRED_THREADS`] × this)
/// instead of being unbounded in the length of one server-supplied string.
///
/// A creation response naming a longer id binds NOTHING — checked before the id is cloned,
/// so an over-long string is never copied into this process's long-lived state.
pub const MAX_THREAD_ID_BYTES: usize = 64;

/// Cap on the number of RETIRED threads one session may accumulate (2e-4c).
///
/// A retired thread is one the operator switched away from with `/new`. It is kept — not
/// forgotten — because the measured app-server still answers a `thread/resume` for it with
/// its own full history, and the ccd link and the phone both legitimately read a previous
/// thread's timeline. 64 is far above any plausible session (it is 64 presses of `/new`)
/// and exists only so the list cannot grow without bound.
///
/// Reaching it REFUSES the further switch rather than evicting the oldest entry. Eviction
/// would make a resume that was valid a moment ago start refusing — a live phone client's
/// open timeline would break — and "the session may no longer switch" is the failure the
/// operator can see and act on.
pub const MAX_RETIRED_THREADS: usize = 64;

/// The session's thread-creation slot, as an explicit state machine (round-2 P2), extended
/// by 2e-4c so that a creation admitted while one thread is already bound is a **switch**.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Creation {
    /// No creation is admitted; the next fingerprint-clean `thread/start` may claim it.
    Open,
    /// One creation was admitted on `conn` with `id` and its response has not landed.
    ///
    /// `superseding` is `Some(old_active)` when this creation is a SWITCH — a
    /// `thread/start` admitted while `old_active` was the head. It is what makes the
    /// switch reversible: a creation that provably FAILS restores `old_active` as the
    /// head rather than leaving the session headless.
    Pending {
        conn: ConnId,
        id: RequestId,
        superseding: Option<VerifiedThread>,
        /// **This creation consumed a switch reservation** (round-2 P3/P4) — i.e. its
        /// `thread/unsubscribe` prefix really did go upstream, so the connection is no
        /// longer subscribed to the thread it is switching away from. If the creation then
        /// FAILS, that connection is left unsubscribed and must be wedged rather than
        /// quietly handed its old head back (P4).
        prefix_forwarded: bool,
    },
    /// A creation response was correlated and fully verified: the session's ACTIVE thread.
    /// Turns may only name this one.
    Bound(VerifiedThread),
    /// A pending creation was consumed by a response that proved NEITHER success nor
    /// failure, or its owning connection vanished while it was in flight. Neither installs
    /// nor reopens: the server may hold a thread we cannot name, so a second creation could
    /// break the single-thread invariant. Terminal until reconnect evidence (D2 owns the
    /// reconciliation); pre-D2 the session is wedged-safe.
    Closed(&'static str),
}

/// **The turn/switch linearization state** (round-1 P1).
///
/// # What it is for
///
/// A `turn/start` is authorized against the ACTIVE head; a `thread/start` MOVES that head.
/// Without a relation between them the two decisions are independent, and the window
/// between "this turn head-checked against A" and "this turn's bytes reached the server"
/// is a window in which the head can become B. The turn would then run against a thread
/// the session is no longer on, authorized by a check that was true when it was made and
/// false when it mattered.
///
/// The relation is expressed as STATE rather than as a lock held across I/O: a turn
/// admitted for A marks A busy, and a switch is refused while any turn is busy. Because a
/// turn can only be admitted while creation is `Bound` — a `Pending` switch leaves no head
/// to head-check against, so a turn arriving mid-switch refuses — the two rules together
/// make the interleaving unrepresentable in both directions, with no await inside a
/// critical section.
///
/// # When a turn stops being busy
///
/// * the broker observes `turn/completed` for that thread on any connection's s2c stream
///   (A3: exactly one terminal per turn, 7/7 — `completed`, `interrupted` or `failed`);
/// * the `turn/start` is answered with an ERROR (it never started, so nothing will
///   terminate it);
/// * the admitting connection closes (its turn's fate goes with it).
///
/// # The fail-closed direction, stated plainly
///
/// If a terminal is somehow never observed, the session can no longer switch. That is the
/// safe direction — a refused `/new` is legible to the operator, while a turn authorized
/// against a stale head is not — and it is not reachable through the measured client:
/// **the real 0.147 TUI disables `/new` mid-turn on its own** (measured: the pane answers
/// `■ '/new' is disabled while a task is in progress.` and no frame is sent). This state
/// therefore guards against a client the wire has not shown, which is the only kind of
/// client a security core should be written for.
#[derive(Debug, Default)]
struct TurnActivity {
    /// The thread every entry belongs to. `None` iff `admitted` is empty.
    thread: Option<Box<str>>,
    /// The admitted-but-unterminated `turn/start` requests, keyed by the ADMITTING
    /// `(connection, request id)` — never by turn id, because at admission time the turn
    /// does not exist yet.
    ///
    /// **A key may hold at most one LIVE entry** (round-3 P1). See
    /// [`TurnActivity::holds`].
    admitted: HashMap<(ConnId, RequestId), TurnEntry>,
    /// Turn ids whose terminal this broker has already acted on (round-3 P2).
    ///
    /// A terminal is delivered to every subscribed connection, and this broker observes
    /// every leg — so it sees the SAME terminal more than once as a matter of course. Only
    /// the first delivery may clear anything; a duplicate arriving after a fresh
    /// `turn/start` was admitted would otherwise clear that new turn's mark and unfence the
    /// switch it exists to fence.
    cleared: TerminalEpochs,
}

/// The bounded memory of terminals already acted on (round-3 P2).
///
/// Bounded for the same reason as every other ledger here: the ids are server-supplied and
/// a long session produces one per turn. Eviction is oldest-first, and the consequence of
/// evicting is only that a duplicate of a terminal older than [`MAX_CLEARED_TURNS`] turns
/// would be treated as new — a shape the wire has never shown, since duplicates arrive
/// within the same delivery fan-out, not tens of turns later.
#[derive(Debug, Default)]
struct TerminalEpochs {
    seen: HashSet<(String, String)>,
    order: std::collections::VecDeque<(String, String)>,
}

impl TerminalEpochs {
    /// Record this terminal, returning TRUE iff it is one we have not acted on.
    ///
    /// **Keyed by `(thread, turn)`, not by turn id alone** (closing S1). Turn ids are unique
    /// within a thread, not across a session — the app-server mints them per thread — so a
    /// session-wide key lets thread A's turn `t` shadow thread B's turn `t`: B's genuine
    /// terminal would be read as a duplicate and clear nothing, leaving B's marks set and
    /// the session unable to switch. A switch is precisely when two threads' ids coexist
    /// here, which is the one situation this set exists for.
    fn admit(&mut self, thread: &str, turn: &str) -> bool {
        let key = (thread.to_string(), turn.to_string());
        if self.seen.contains(&key) {
            return false;
        }
        if self.order.len() >= MAX_CLEARED_TURNS {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        self.seen.insert(key.clone());
        self.order.push_back(key);
        true
    }
}

/// Where one admitted `turn/start` stands (round-2 P1).
#[derive(Debug, Clone, PartialEq, Eq)]
enum TurnEntry {
    /// Admitted, and its response has NOT been seen. The server has not told us whether
    /// this request became a turn, joined one, or did nothing — so no turn terminal can
    /// speak for it. **This is the state round-2 P1 exists to protect.**
    Unanswered,
    /// Its `turn/start` was answered. `turn` is `result.turn.id` when the response carried
    /// a readable one (D12: for an implicit steer this may be a PHANTOM id that never
    /// exists, which is why the terminal rule below does not require it to match).
    Answered { turn: Option<String> },
}

impl TurnActivity {
    fn is_busy(&self) -> bool {
        !self.admitted.is_empty()
    }

    fn len(&self) -> usize {
        self.admitted.len()
    }

    /// **Does this `(conn, id)` already hold a live entry?** (round-3 P1.)
    ///
    /// The defect this closes: a `turn/start` response DRAINS its outstanding-ledger entry,
    /// so the id becomes reusable — and a second `turn/start` under that same id would
    /// `insert` over the first turn's entry, silently destroying the only mark that turn
    /// had. Its own error response would then release a mark that belonged to a turn still
    /// running, and the switch fence would be gone.
    ///
    /// The rule is the same discipline the outstanding ledger already applies to in-flight
    /// ids, extended to cover the window between "answered" and "terminated": an id with a
    /// live turn entry is not reusable for another turn.
    fn holds(&self, conn: ConnId, id: &RequestId) -> bool {
        self.admitted.contains_key(&(conn, id.clone()))
    }

    /// Record an admitted turn for `thread`, awaiting its answer.
    fn admit(&mut self, conn: ConnId, id: RequestId, thread: &str) {
        self.thread = Some(thread.into());
        self.admitted.insert((conn, id), TurnEntry::Unanswered);
    }

    /// This request's `turn/start` was ANSWERED with a result. From here on the entry
    /// belongs to whatever turn epoch the server put it in, and the next terminal on its
    /// thread speaks for it.
    fn answered(&mut self, conn: ConnId, id: &RequestId, turn: Option<String>) {
        if let Some(entry) = self.admitted.get_mut(&(conn, id.clone())) {
            *entry = TurnEntry::Answered { turn };
        }
    }

    /// One admitted turn provably did not start (its request was answered with an error).
    fn release(&mut self, conn: ConnId, id: &RequestId) {
        self.admitted.remove(&(conn, id.clone()));
        self.forget_thread_if_idle();
    }

    /// **A turn TERMINAL was observed for `thread`: clear the ANSWERED entries, and only
    /// those, and only ONCE per terminal** (round-2 P1, round-3 P2).
    ///
    /// # Why "answered" is the dividing line
    ///
    /// A terminal ends the thread's ONE active turn. An entry that has already been
    /// answered was placed by the server into that turn epoch — either as the turn that
    /// just ended, or (D12) as an implicit steer that JOINED it, whose response carries a
    /// phantom turn id that will never terminalize on its own. Both are finished when the
    /// terminal arrives, so both clear. Requiring the recorded turn id to MATCH would leave
    /// every steer's phantom entry uncleared and wedge the session against switching for
    /// ever, which is why the id is recorded for the audit trail and not used as the test.
    ///
    /// An UNANSWERED entry is the opposite case: the server has said nothing about it, so
    /// this terminal cannot be about it, and it survives. It is cleared only by its OWN
    /// error response.
    ///
    /// # Why it must be idempotent per terminal (round-3 P2)
    ///
    /// A terminal reaches every subscribed connection and this broker watches every leg, so
    /// it sees the same terminal repeatedly by design. Without the epoch gate, the second
    /// delivery would clear entries admitted BETWEEN the two deliveries — a turn that is
    /// genuinely running — and unfence the switch. The gate is what makes "one terminal
    /// ends one epoch" true on a fan-out wire.
    ///
    /// A terminal with no readable turn id clears nothing: it names no epoch, so it cannot
    /// be told apart from its own duplicate.
    fn terminal(&mut self, thread: &str, turn: Option<&str>) {
        if self.thread.as_deref() != Some(thread) {
            return;
        }
        let Some(turn) = turn else {
            return;
        };
        if !self.cleared.admit(thread, turn) {
            return;
        }
        self.admitted
            .retain(|_, entry| matches!(entry, TurnEntry::Unanswered));
        self.forget_thread_if_idle();
    }

    /// **The owning connection went away — which is NOT a turn terminal** (round-3 P3).
    ///
    /// Only UNANSWERED entries release. An answered `turn/start` reached the server and the
    /// turn it belongs to keeps running whether or not this leg is there to watch it (A15:
    /// turns are independent of any one subscription), so releasing its mark would let a
    /// switch move the head out from under a live turn — the exact thing the mark exists to
    /// prevent, reintroduced through the back door of a dropped connection.
    ///
    /// An unanswered entry is different: the server never told us it became anything, and
    /// with the connection gone no answer ever will. Fail-closed cost of keeping the
    /// answered ones: if their terminal is never observed the session stops switching. That
    /// is the safe direction, and it is bounded by [`MAX_ACTIVE_TURNS`].
    fn close_connection(&mut self, conn: ConnId) {
        self.admitted
            .retain(|(c, _), entry| *c != conn || matches!(entry, TurnEntry::Answered { .. }));
        self.forget_thread_if_idle();
    }

    fn forget_thread_if_idle(&mut self) {
        if self.admitted.is_empty() {
            self.thread = None;
        }
    }
}

/// The verdict of the ATOMIC turn admission (round-1 P1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnAdmission {
    /// Head-checked, workspace-checked, id-ledgered and marked busy — all under one lock.
    Admitted,
    /// The turn does not name the session's active head (or there is no head).
    NotTheHead { detail: String },
    /// It names the head but not the workspace bound at that head's creation.
    WrongWorkspace { detail: String },
    /// The id ledger refused it; carries the ledger's own verdict so the caller can keep
    /// the existing drop/refuse/count distinctions.
    Ledger(IdAdmission),
    /// A switch has been RESERVED on some connection (round-2 P3): its `thread/unsubscribe`
    /// prefix has already gone upstream and its `thread/start` is expected next. A turn
    /// admitted now would be authorized against a head that is about to move.
    SwitchReserved,
    /// Too many turns are admitted and unterminated on this session (round-2 P2).
    TooManyActiveTurns,
    /// **This CONNECTION unsubscribed itself from the thread and never re-subscribed**
    /// (round-2 P4). Its `thread/unsubscribe` prefix forwarded and the switch behind it
    /// then failed at the server, so the head was restored but this connection is no longer
    /// receiving the thread's stream. Authorizing its turns would produce turns nobody on
    /// that connection can observe.
    ConnectionUnsubscribed { thread: String },
}

/// One connection's request-id ledger./// One connection's request-id ledger. All three sets are bounded — see
/// [`MAX_OUTSTANDING_REQUESTS`] and [`MAX_CONN_REQUEST_IDS`].
#[derive(Debug, Default)]
struct ConnIds {
    /// **Every** id forwarded upstream on this connection and not yet answered, mapped to
    /// the ACTUAL method of the request that carries it (round-3 P1). An id already in this
    /// map cannot be reused; a response removes its entry.
    ///
    /// Storing the real method — rather than a bare "is this the creation?" flag — is what
    /// makes a response correlate to the method it actually answers: the creation-response
    /// test asks whether the entry it releases *is* [`CREATION_METHOD`], so a response to
    /// some other method can never be read as a creation answer.
    outstanding: HashMap<RequestId, String>,
    /// Ids currently claimed as the pending CREATION on this connection.
    ///
    /// **Defense-in-depth, not an independently provable rule.** Given the single
    /// session-wide creation slot, a second creation claim is already refused by
    /// `creation != Creation::Open`, and a non-creation request sharing the id is already
    /// refused by `outstanding`. This set is kept because it is the rule that would still
    /// hold if a future multi-pending design (D2's thread switch) relaxed the single-slot
    /// invariant — see `an_in_flight_creation_id_reservation_is_defense_in_depth`.
    reserved: HashSet<RequestId>,
    /// Ids whose pending creation was CONSUMED on this connection. Permanently spent: a
    /// replayed response for one can never re-install, and a creation may not reuse one.
    tombstoned: HashSet<RequestId>,
}

impl ConnIds {
    fn len(&self) -> usize {
        self.reserved.len() + self.tombstoned.len()
    }

    fn is_free(&self, id: &RequestId) -> bool {
        !self.reserved.contains(id) && !self.tombstoned.contains(id)
    }
}

/// The session's verified-binding store: the one creation slot, the per-connection id
/// ledgers that make its correlation replay-proof, and the hostile-event counters. All live
/// behind ONE mutex so the admit-and-record of [`ThreadBinding::try_admit_request`] is
/// atomic.
#[derive(Debug)]
struct Binding {
    creation: Creation,
    /// **Turn/switch linearization state** (round-1 P1): the `turn/start` requests this
    /// broker admitted for the active head whose turn has not been observed to terminate.
    ///
    /// While this is non-empty a switch is REFUSED, so a `thread/start` can never be
    /// admitted between a turn's authorization and its terminal. Entries are keyed by the
    /// admitting `(ConnId, RequestId)` so a connection that vanishes takes only its own
    /// with it; the thread they belong to is recorded once because they can only ever
    /// belong to the active head.
    active_turns: TurnActivity,
    /// **A switch reserved by one connection** (round-2 P3). Held from the moment its
    /// `thread/unsubscribe` prefix is admitted until its `thread/start` is decided, the
    /// connection closes, or [`SWITCH_RESERVATION_TTL`] elapses. While it is held, a turn
    /// or a competing creation is refused: the prefix has already had a wire effect, so the
    /// switch behind it must not lose a race it already started.
    switch_reservation: Option<SwitchReservation>,
    /// **Connections that unsubscribed themselves from a thread and have not re-subscribed**
    /// (round-2 P4). See [`TurnAdmission::ConnectionUnsubscribed`].
    unsubscribed: HashMap<ConnId, Wedge>,
    /// Monotonic stamp for wedge slots (closing S5). Never reused, so "newer" is a total
    /// order rather than a guess about arrival.
    wedge_seq: u64,
    /// `thread/resume` requests forwarded by a WEDGED connection, by `(conn, request id)`
    /// (round-3 P6). The wedge lifts when one of these is answered with a success; an
    /// error leaves it in place.
    resubscribing: HashMap<(ConnId, RequestId), u64>,
    /// Threads this session bound and later switched AWAY from, oldest first (2e-4c).
    ///
    /// **Retired is not forgotten.** A retired thread stays a session thread for
    /// [`ThreadBinding::is_session_thread`] — so `thread/resume` still forwards for it —
    /// but it is NOT the head, so no `turn/start` may name it. That asymmetry is the whole
    /// switch rule, and it is exactly what the wire does: the measured app-server answers a
    /// resume of a switched-away thread with its own full populated history, while the TUI
    /// runs every subsequent turn on the new one.
    ///
    /// Bounded by [`MAX_RETIRED_THREADS`] entries and [`MAX_THREAD_ID_BYTES`] per entry
    /// (round-1 P9); the count cap refuses a further switch rather than evicting (see that
    /// constant).
    ///
    /// **Ids only, deliberately.** A retired thread is consulted by exactly one predicate —
    /// [`ThreadBinding::is_session_thread`], which scopes `thread/resume` and
    /// `thread/unsubscribe` — and that predicate needs nothing but the id. The workspace
    /// anchor (`cwd`/`roots`) is read only from [`ThreadBinding::bound_thread`], the ACTIVE
    /// head, because a turn may only ever name the active head. Keeping whole
    /// [`VerifiedThread`]s here would retain two client-influenced JSON values per retired
    /// thread for the life of the session to answer a question that never asks about them.
    retired: Vec<Box<str>>,
    conns: HashMap<ConnId, ConnIds>,
    counts: IdLedgerCounts,
}

/// The session's thread binding, learned from admitted creations correlated with their
/// server responses.
#[derive(Debug, Clone)]
pub struct SessionThreads {
    inner: Arc<Mutex<Binding>>,
    /// The coordinator-owned launch cwd, already canonicalized upstream (see the module
    /// header). Compared by **exact string equality** against a creation response's `cwd`;
    /// this crate performs no path normalization of its own.
    launch_cwd: Arc<str>,
}

impl SessionThreads {
    /// Build the store anchored to `launch_cwd` — the canonicalized cwd the coordinator
    /// launched this session in, carried in the launch fingerprint.
    pub fn new(launch_cwd: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Binding {
                creation: Creation::Open,
                active_turns: TurnActivity::default(),
                switch_reservation: None,
                unsubscribed: HashMap::new(),
                wedge_seq: 0,
                resubscribing: HashMap::new(),
                retired: Vec::new(),
                conns: HashMap::new(),
                counts: IdLedgerCounts::default(),
            })),
            launch_cwd: launch_cwd.into().into(),
        }
    }

    /// Does the store hold a per-connection id ledger for `conn`?
    ///
    /// The direct witness M10's capacity test needs: it distinguishes "the connection was
    /// tracked" from "the connection was refused before a ledger was allocated", which is
    /// what proves an over-capacity or over-long-id refusal stores nothing.
    #[cfg(test)]
    pub(crate) fn has_tracked_connection(&self, conn: ConnId) -> bool {
        self.inner.lock().unwrap().conns.contains_key(&conn)
    }

    /// Observe one server→client frame **on the connection with `conn`** and, if it is the
    /// correlated response to the pending creation, run the P2 state machine over it.
    ///
    /// ## Only a correlated RESPONSE can bind
    ///
    /// A JSON-RPC response carries no method — it is matched by `(ConnId, id)` against the
    /// pending creation. Any method-bearing frame is ignored outright: that is exactly the
    /// `thread/started` / `thread/resumed` seeding this fix deletes. Such a notification
    /// can only ever *confirm* the id we already bound, which is a no-op.
    ///
    /// ## Why cwd/roots come from the RESPONSE (measured correction)
    ///
    /// The 2e-4a review asked for "the `thread/start`'s cwd + roots". Measured against a
    /// real codex 0.147 `--remote` TUI, the `thread/start` REQUEST sends `"cwd": null`
    /// while the following `turn/start` sends a concrete path, so binding from the request
    /// would refuse **every** real turn. The creation RESPONSE carries the SERVER-RESOLVED
    /// values, and those compare equal to the turn's:
    ///
    /// * response `cwd` == turn/start `cwd` — TRUE; response `runtimeWorkspaceRoots` ==
    ///   turn/start `runtimeWorkspaceRoots` — TRUE
    /// * request `cwd` == turn/start `cwd` — FALSE (`null` vs a path)
    ///
    /// So the response is the only sound source, and it is also the one this broker can
    /// correlate to an admission. What the response canNOT supply is *authority* — the
    /// server is echoing the client's own ask — so the `cwd` is additionally required to
    /// equal the coordinator-owned launch cwd (round-2 P4, module header).
    ///
    /// ## Cheap guard
    ///
    /// Round-3 P1 makes every response interesting (it has to release its outstanding id),
    /// so the round-2 "is a creation pending?" guard is no longer sufficient on its own.
    /// The cheap step is instead a **top-level header scan**
    /// ([`crate::message::scan_frame_header`]): it walks the frame once and skips every
    /// member that is not `id`/`method` with `IgnoredAny`, so a multi-MB `app/list/updated`
    /// or `plugin/list` body is still never materialized into a `Value`. Only a frame that
    /// correlates to the pending creation is parsed in full.
    ///
    /// The full parse uses the duplicate-member-rejecting parser the c2s classifier uses: a
    /// frame whose members are ambiguous yields no trustworthy proof, so it is skipped
    /// **without consuming the pending entry** — which keeps the creation slot closed and
    /// binds nothing (fail closed). The header scan applies the same discipline to the
    /// top-level members it reads, so an ambiguous header releases nothing either.
    ///
    /// ## The DRAIN is validated, not assumed (round-4 P1)
    ///
    /// Releasing an outstanding id is not free: a released id is claimable again, and the
    /// claimant may be a `thread/start`. So an id is released ONLY by a frame the header scan
    /// has proven to be a response — exactly one of `result`/`error`, and for `error` a
    /// structurally valid JSON-RPC error object. That is deliberately the SAME definition
    /// `classify_creation_response` uses (see `message::ResponseKind` and
    /// `response_kind_agrees_with_the_creation_state_machine`): if the drain rule were the
    /// looser of the two, a frame the creation state machine calls indeterminate could still
    /// free an id, which is the hole that let a bare `{"id":X}` be laddered into a second
    /// thread.
    ///
    /// The validation stays a HEADER scan: it needs presence/exclusivity plus the error's two
    /// field types and nothing else, so a multi-MB `result` is still skipped with
    /// `IgnoredAny` and never becomes a `Value`.
    pub fn observe_server_frame(&self, conn: ConnId, text: &str) {
        let Some(header) = crate::message::scan_frame_header(text) else {
            return;
        };
        // Method-bearing ⇒ a notification or a server→client request, never the answer to a
        // forwarded request. `thread/started`/`thread/resumed` land here and may never seed
        // a binding — but ONE method-bearing frame now has an effect: a turn TERMINAL
        // releases the linearization's busy mark (round-1 P1). It is read out of the raw
        // bytes and only ever CLEARS state, so it can neither bind a thread nor authorize
        // anything.
        if header.has_method {
            self.observe_turn_terminal(text);
            return;
        }
        let Some(id) = header.id else {
            return;
        };

        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;

        // Is this the correlated answer to the pending creation ON THIS CONNECTION? A
        // response from any other connection — including another connection of the same
        // role — matches nothing.
        let correlates = matches!(
            &g.creation,
            Creation::Pending { conn: pc, id: pid, .. } if *pc == conn && *pid == id
        );

        if !correlates {
            // An ordinary response: it RELEASES its outstanding entry (the id is answered,
            // so the client may use it again) and does nothing else. A release installs
            // nothing, so it is safe on any well-formed response — including an `error`,
            // which is exactly how a real client's failed `thread/resume` is answered.
            //
            // ROUND-4 P1 — DRAIN VALIDATION. The release is gated on the frame having been
            // PROVEN a response by the header scan (exactly one of `result`/`error`, and for
            // `error` a structurally valid JSON-RPC error object). Without this gate a bare
            // method-less `{"id":X}` — which proves nothing — drained X, and that is a
            // ladder to a SECOND thread: X looks free, so a `thread/start` reusing X is
            // admitted and becomes the pending creation; the ORIGINAL request's delayed but
            // perfectly valid error for X then correlates to that pending and is
            // misclassified as the CREATION's failure, reopening the slot while the first
            // `thread/start` is still in flight upstream and may already have created a
            // thread. An unprovable frame therefore releases NOTHING.
            if header.response.is_response() {
                // **An ERRORED `turn/start` releases its busy mark** (round-1 P1). The turn
                // provably never started, so no terminal will ever arrive for it, and a
                // mark nothing can clear would wedge the session against switching for
                // ever. Scoped to the entry this response actually releases, so an error
                // answering some other method cannot clear a live turn's mark.
                let was_turn = g
                    .conns
                    .get(&conn)
                    .and_then(|slots| slots.outstanding.get(&id))
                    .map(String::as_str)
                    == Some(TURN_METHOD);
                if let Some(slots) = g.conns.get_mut(&conn) {
                    slots.outstanding.remove(&id);
                }
                // **A correlated ACCEPTED resume lifts the wedge** (round-3 P6). Only a
                // success: an errored or policy-refused resume subscribes nothing, and a
                // client must not be able to lift its own wedge by asking and being told no.
                if let Some(stamp) = g.resubscribing.remove(&(conn, id.clone())) {
                    // Only an ACCEPTED answer to the attempt that named THIS wedge lifts it
                    // (round-3 P6, closing S5): a stale answer stamped against an older
                    // wedge cannot clear the newer one that replaced it.
                    if matches!(header.response, crate::message::ResponseKind::Result)
                        && g.unsubscribed.get(&conn).is_some_and(|w| w.seq == stamp)
                    {
                        g.unsubscribed.remove(&conn);
                    }
                }
                if was_turn {
                    if matches!(header.response, crate::message::ResponseKind::Error) {
                        // Provably never started: no terminal will ever speak for it.
                        g.active_turns.release(conn, &id);
                    } else {
                        // **ANSWERED** (round-2 P1). From here the entry belongs to the
                        // server's turn epoch and the next terminal on its thread clears
                        // it. The turn id is recorded when the response carries a readable
                        // one — for the audit trail and the logs, NOT as the terminal's
                        // matching test (D12: a steer's id is a phantom).
                        //
                        // The parse is deliberate and cheap: a `turn/start` result is a
                        // small object, and this branch is only reached for a frame the
                        // header scan already proved is a response to a request this
                        // broker forwarded as a turn.
                        let turn = crate::message::parse_no_dup_value(text).and_then(|v| {
                            v.pointer("/result/turn/id")
                                .and_then(Value::as_str)
                                .filter(|t| !t.is_empty() && t.len() <= MAX_THREAD_ID_BYTES)
                                .map(str::to_string)
                        });
                        g.active_turns.answered(conn, &id, turn);
                    }
                }
            }
            return;
        }

        // A CREATION response must satisfy all three conditions (round-3 P1).
        let slots = g.conns.entry(conn).or_default();
        // (a) the id matches the pending creation — established above;
        // (b) the outstanding entry it releases is the CREATION request, not some other
        //     method that happened to share the id;
        let is_creation_entry =
            slots.outstanding.get(&id).map(String::as_str) == Some(CREATION_METHOD);
        // (c) no OTHER outstanding request holds that id. Belt-and-braces: `outstanding` is
        //     a map keyed BY the id, and (b)'s collision refusal means an id is outstanding
        //     at most once, so this count is 0 or 1 and can never exceed 1. It is written
        //     out anyway so a future change that admitted a second holder could not silently
        //     re-open the correlation.
        let holders = slots.outstanding.keys().filter(|k| *k == &id).count();
        if !is_creation_entry || holders != 1 {
            // Unreachable by construction (a pending creation always has its own
            // outstanding entry, and no second request can take that id). Fail closed: bind
            // nothing, reopen nothing, and leave the pending in place.
            return;
        }

        // Only now is the full, duplicate-rejecting parse worth its cost.
        let Some(v) = crate::message::parse_no_dup_value(text) else {
            return;
        };

        // Consume the pending: the id is released from `outstanding` and moves
        // reservation ⇒ tombstone, so a replayed copy of this very response can never be
        // correlated again. (The claim-time bound already reserved room for this insert, so
        // it cannot push the set past the cap.)
        slots.outstanding.remove(&id);
        slots.reserved.remove(&id);
        slots.tombstoned.insert(id);

        // What was this creation superseding, if anything? Taken out of the pending state
        // BEFORE it is replaced, so the three arms below can each do the right thing with it.
        let (superseding, prefix_forwarded) = match &g.creation {
            Creation::Pending {
                superseding,
                prefix_forwarded,
                ..
            } => (superseding.clone(), *prefix_forwarded),
            // Unreachable: `correlates` already proved the state is `Pending`.
            _ => (None, false),
        };

        g.creation = match (
            classify_creation_response(&v, &self.launch_cwd),
            superseding,
        ) {
            // The creation VERIFIED. If it superseded a head, that head is now RETIRED —
            // still a session thread (resumable), no longer the head (unturnable). The push
            // cannot exceed the cap: `try_admit_request` refused the switch unless there was
            // room for exactly this entry, and only one switch is ever in flight.
            (Creation::Bound(fresh), Some(old_active)) => {
                retire(g, old_active);
                Creation::Bound(fresh)
            }
            // A first creation verifying, or any non-switch outcome: unchanged.
            (next, None) => next,
            // The switch provably FAILED (an exclusive, well-formed JSON-RPC error). The
            // server created nothing, so the SESSION's old head is untouched and is
            // RESTORED — `Creation::Open` here would leave a live session headless,
            // refusing every subsequent turn on a thread that is still perfectly valid.
            //
            // **But if this switch's `thread/unsubscribe` prefix forwarded, this
            // CONNECTION is no longer subscribed to that head** (round-2 P4), and handing
            // it back its authorization would be a quiet lie: its turns would run and it
            // would observe none of them. So the head restores for the SESSION while THIS
            // CONNECTION's turn authorization WEDGES, with a legible cause, until a real
            // `thread/resume` re-subscribes it — the TUI's own recovery path, not one this
            // broker invents by injecting traffic.
            (Creation::Open, Some(old_active)) => {
                if prefix_forwarded {
                    let id = old_active.id.clone();
                    wedge(g, conn, id);
                }
                Creation::Bound(old_active)
            }
            // The switch response proved NEITHER success nor failure. The old head is NOT
            // restored as the HEAD: the server may hold a new thread this broker cannot
            // name, and the TUI may already be sending turns to it, so continuing to
            // authorize turns on the old head would authorize a turn against a thread
            // nobody is on. Wedged-safe is the only honest state for TURNS.
            //
            // It is still RETIRED rather than dropped, though. It is a thread this session
            // provably bound, its history is real, and the operator's phone may have its
            // timeline open — "we lost track of which thread is live" is no reason to stop
            // answering a read of one we did verify. Turn authorization is what wedges;
            // readability is not.
            (closed @ Creation::Closed(_), Some(old_active)) => {
                retire(g, old_active);
                closed
            }
            // `classify_creation_response` returns only Open/Bound/Closed.
            (Creation::Pending { .. }, Some(_)) => {
                unreachable!("classify_creation_response never returns Pending")
            }
        };
    }
}

/// **Every precondition a `thread/start` must satisfy, in one place** (round-2 P3).
///
/// The first form of `switch_admissibility` restated a SUBSET of `try_admit_request`'s
/// rules, and the subset was wrong in a way that reintroduced the very defect the
/// pre-check exists to prevent: it omitted the per-connection creation-id arithmetic, so on
/// a connection that had already spent its [`MAX_CONN_REQUEST_IDS`] budget (63 switches and
/// their tombstones) the check answered `Ok`, the `thread/unsubscribe` prefix forwarded and
/// dropped the subscription, and the `thread/start` behind it was then refused
/// `CreationSlotClosed`. Exactly the "unsubscribed, then refused" sequence P4 is about.
///
/// One function, two callers, no subset: `try_admit_request` consults it before claiming,
/// and `switch_admissibility` consults it before letting a prefix forward. `id` is the
/// creation id when one is known (the claim path) and `None` when it is not yet (the
/// prefix path, which cannot know the id the client will choose).
fn creation_preconditions(g: &Binding, conn: ConnId, for_prefix: bool) -> Result<(), &'static str> {
    // Session-wide state — checked on BOTH paths.
    match &g.creation {
        Creation::Open => {}
        Creation::Bound(_) => {
            if g.active_turns.is_busy() {
                return Err(
                    "a turn is running on this session's thread; a switch may not move the \
                     head out from under an authorized turn",
                );
            }
            if g.retired.len() >= MAX_RETIRED_THREADS {
                return Err("this session has retired as many threads as it may hold");
            }
        }
        Creation::Pending { .. } => return Err("a thread creation is already in flight"),
        Creation::Closed(_) => return Err("this session's creation slot is closed"),
    }
    // Per-connection LEDGER state.
    //
    // On the CLAIM path these are `admit_id`'s to enforce, with its own verdicts and its
    // own hostile-event counters (`ReusedInFlight`, `AtCapacity`) — restating them here
    // would flatten those distinctions into `CreationSlotClosed` and stop the counters that
    // feed the failure-containment seam. On the PREFIX path there is no `admit_id` call to
    // reach, and the whole point of the pre-check is to predict what the creation will
    // face, so they are checked here instead.
    if !for_prefix {
        return Ok(());
    }
    if !g.conns.contains_key(&conn) && g.conns.len() >= MAX_TRACKED_CONNECTIONS {
        return Err("this broker is tracking as many connections as it may hold");
    }
    if let Some(slots) = g.conns.get(&conn) {
        // **The 63-switch arithmetic** (round-2 P3): every settled creation leaves a
        // permanent tombstone, so a connection's creation-id budget is finite. The first
        // form of this check omitted it, and that omission was the defect: on a connection
        // that had spent the budget the prefix forwarded and dropped the subscription, and
        // the `thread/start` behind it was then refused.
        if slots.len() >= MAX_CONN_REQUEST_IDS {
            return Err(
                "this connection has spent its creation-id budget; a further creation on \
                 it cannot be recorded",
            );
        }
        if slots.outstanding.len() >= MAX_OUTSTANDING_REQUESTS {
            return Err("this connection's outstanding-request ledger is full");
        }
    }
    Ok(())
}

/// Move a thread onto the retired list — id only, and never past the cap (round-1 P9).
///
/// The cap cannot be exceeded here in practice: `try_admit_request` refuses a switch unless
/// there is room for exactly this entry, and only one switch is ever in flight. The guard
/// is written anyway because "the caller checked" is the kind of invariant that survives
/// until the second caller, and this function now has three.
fn retire(g: &mut Binding, thread: VerifiedThread) {
    if g.retired.len() >= MAX_RETIRED_THREADS {
        return;
    }
    if g.retired.iter().any(|t| **t == *thread.id) {
        return;
    }
    g.retired.push(thread.id.into_boxed_str());
}

/// The per-connection id ledger's admission rules, shared by [`ThreadBinding::try_admit_request`]
/// and [`ThreadBinding::try_admit_turn`] (round-1 P1).
///
/// Factored out precisely because the turn path now needs them INSIDE the same critical
/// section as the head-check. Two copies of these rules would be two chances for the two
/// paths to disagree about what an id ledger admits.
fn admit_id(g: &mut Binding, conn: ConnId, id: &RequestId, method: &str) -> IdAdmission {
    let is_creation = method == CREATION_METHOD;
    // **P6's byte cap lives HERE, not at one call site** (round-2 P2). It used to sit in
    // `try_admit_request` above this call, so `try_admit_turn` — which calls this directly
    // — stored a client-chosen id of any length. Every path that can put an id into the
    // ledger now goes through the same cap.
    if !id_within_cap(id) {
        g.counts.oversized += 1;
        return IdAdmission::Oversized;
    }
    // Bound the tracked-connection table before allocating a ledger for a new one.
    if !g.conns.contains_key(&conn) && g.conns.len() >= MAX_TRACKED_CONNECTIONS {
        g.counts.at_capacity += 1;
        return IdAdmission::AtCapacity;
    }
    let slots = g.conns.entry(conn).or_default();
    // Round-3 P1: the id must not ALREADY be outstanding on this connection.
    if slots.outstanding.contains_key(id) {
        g.counts.reused_in_flight += 1;
        return IdAdmission::ReusedInFlight;
    }
    if slots.outstanding.len() >= MAX_OUTSTANDING_REQUESTS {
        g.counts.at_capacity += 1;
        return IdAdmission::AtCapacity;
    }
    if is_creation {
        // The creation-id sets: the id must be free (not a spent tombstone, and — as
        // defense-in-depth — not an in-flight reservation), and the set must have room
        // for the tombstone this claim will eventually become.
        if !slots.is_free(id) || slots.len() >= MAX_CONN_REQUEST_IDS {
            return IdAdmission::CreationSlotClosed;
        }
    }
    slots.outstanding.insert(id.clone(), method.to_string());
    IdAdmission::Admitted
}

impl SessionThreads {
    /// **Observe a turn TERMINAL and clear the linearization's busy mark** (round-1 P1).
    ///
    /// The terminal is `turn/completed`, whatever its `status` — A3 measured exactly one
    /// terminal per turn (7/7) and named the vocabulary `completed | interrupted | failed`.
    /// All three end the turn, so all three release; reading the status and releasing only
    /// on `completed` would leave an interrupted turn's mark set for ever.
    ///
    /// Deliberately narrow: it parses, reads two fields, and can only ever CLEAR state.
    /// A frame it cannot read clears nothing, which is the fail-closed direction (the
    /// session keeps refusing switches rather than admitting one on a frame it misread).
    fn observe_turn_terminal(&self, text: &str) {
        // Cheap prefilter before any parse: multi-MB notifications reach this path and a
        // terminal is ~200 bytes with a fixed method name.
        if !text.contains("\"turn/completed\"") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if v.get("method").and_then(Value::as_str) != Some("turn/completed") {
            return;
        }
        let Some(thread) = v
            .pointer("/params/threadId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= MAX_THREAD_ID_BYTES)
        else {
            return;
        };
        // The terminal's own turn id names the EPOCH it ends (round-3 P2). A terminal
        // without a readable one clears nothing: it cannot be told apart from its duplicate.
        let turn = v
            .pointer("/params/turn/id")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && t.len() <= MAX_THREAD_ID_BYTES);
        self.inner
            .lock()
            .unwrap()
            .active_turns
            .terminal(thread, turn);
    }
}

/// The method whose admission marks a thread busy for the switch linearization.
pub const TURN_METHOD: &str = "turn/start";

/// The P2 state machine over a correlated creation response. Pure, so the three arms are
/// unit-testable without a store.
///
/// **Shared definition of "a valid response" (round-4 P1).** Arms 1 and 2 below are the same
/// rule the header scan applies when it decides whether a frame may DRAIN an outstanding id
/// — [`crate::message::ResponseKind::Error`] is arm 1's precondition and
/// [`crate::message::ResponseKind::Result`] is arm 2's. The two must never drift apart, or an
/// id could be freed by a frame this machine calls indeterminate;
/// `tests::response_kind_agrees_with_the_creation_state_machine` pins the equivalence.
fn classify_creation_response(v: &Value, launch_cwd: &str) -> Creation {
    let result = v.get("result");
    let error = v.get("error");

    // 1) An EXCLUSIVE, STRUCTURALLY VALID JSON-RPC error: an `error` object carrying an
    //    INTEGER `code` and a STRING `message`, and no `result` member at all. This is the
    //    one shape that proves the creation FAILED, so it — and only it — reopens creation,
    //    keeping a legitimately failed `thread/start` retryable.
    //
    //    Round 3 P2 tightens "an object" to "a JSON-RPC error object". `{}`, `{"code":-1}`,
    //    `{"message":"x"}`, a float code or a non-string message do not prove a failure —
    //    they are a frame this broker cannot read — and reopening creation on one would
    //    risk a SECOND thread. They fall through to the indeterminate arm below.
    if result.is_none() && error.is_some_and(is_jsonrpc_error_object) {
        return Creation::Open;
    }

    // 2) A valid result carrying all three proofs INSTALLS the binding.
    if error.is_none() {
        if let Some(result) = result {
            if let Some(thread) = verify_creation_result(result, launch_cwd) {
                return Creation::Bound(thread);
            }
        }
    }

    // 3) ANYTHING ELSE is indeterminate. A partial/ill-typed result, a `cwd` that is not the
    //    launch cwd, `error: null`, a non-object error, both members, or neither: the server
    //    may have created a thread we cannot name, so reopening could produce a SECOND
    //    thread and break the single-thread invariant. Neither install nor reopen.
    Creation::Closed(
        "a correlated creation response proved neither success nor failure; creation is \
         closed rather than reopened, because the server may hold a thread this broker \
         cannot name (D2 owns the reconciliation)",
    )
}

/// Is `error` structurally a JSON-RPC error object (round-3 P2)?
///
/// The JSON-RPC 2.0 error object is `{code: integer, message: string, data?: any}`. Both
/// required members must be present AND well-typed: an INTEGER code (`-32601`, not `-1.5`
/// and not `"-1"`) and a STRING message. Extra members are allowed — `data` is part of the
/// spec — because they cannot make the frame less of an error.
///
/// This is the ONLY shape that lets a consumed pending creation REOPEN the slot, so the
/// check is deliberately structural rather than "is it an object": a partial or ill-typed
/// error proves nothing about whether the server created a thread, and the single-thread
/// invariant makes "prove nothing" mean "do not reopen".
///
/// It is ALSO — since round-4 P1 — the definition the ledger's DRAIN rule uses, reached from
/// the other side by the header scan's error probe (`crate::message::ResponseKind::Error`),
/// which decides the same predicate on the raw bytes without building a `Value`. One
/// definition, two call sites: `tests::response_kind_agrees_with_the_creation_state_machine`
/// fails if either side is changed alone.
fn is_jsonrpc_error_object(error: &Value) -> bool {
    let Some(map) = error.as_object() else {
        return false;
    };
    let code_is_integer = map.get("code").is_some_and(|c| c.is_i64() || c.is_u64());
    let message_is_string = map.get("message").is_some_and(Value::is_string);
    code_is_integer && message_is_string
}

/// The three proofs, fully type-checked, with the workspace anchored to the launch cwd.
fn verify_creation_result(result: &Value, launch_cwd: &str) -> Option<VerifiedThread> {
    // Round-1 P9: the id is server-supplied and is about to be STORED for the life of the
    // session (as the head, and later on the retired list). Capped BEFORE it is copied, so
    // an over-long string never enters this process's long-lived state — it binds nothing,
    // which leaves the session headless and is the fail-closed direction.
    let thread_id = result
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(|i| i.as_str())
        .filter(|s| !s.is_empty() && s.len() <= MAX_THREAD_ID_BYTES)?;

    // `cwd`: a non-empty string, and EXACTLY the coordinator-owned launch cwd. Exact
    // equality only — the canonicalization happened once at the coordinator (module header).
    let cwd = result.get("cwd")?;
    let cwd_str = cwd.as_str().filter(|s| !s.is_empty())?;
    if cwd_str != launch_cwd {
        return None;
    }

    // `runtimeWorkspaceRoots`: a non-empty ARRAY of non-empty strings. A null, a string, an
    // empty array or an array with a non-string / empty-string element is a shape whose
    // meaning this broker cannot prove.
    let roots = result.get("runtimeWorkspaceRoots")?;
    if !is_nonempty_path_array(roots) {
        return None;
    }

    Some(VerifiedThread {
        id: thread_id.to_string(),
        cwd: cwd.clone(),
        roots: roots.clone(),
    })
}

/// `runtimeWorkspaceRoots` must be a NON-EMPTY array of NON-EMPTY strings. A null, a bare
/// string, an object, an empty array, or an array holding a non-string / empty-string
/// element is a shape whose meaning this broker never measured and cannot prove.
fn is_nonempty_path_array(v: &Value) -> bool {
    v.as_array().is_some_and(|arr| {
        !arr.is_empty()
            && arr
                .iter()
                .all(|r| r.as_str().is_some_and(|s| !s.is_empty()))
    })
}

impl ThreadBinding for SessionThreads {
    fn bound_thread(&self) -> Option<VerifiedThread> {
        match &self.inner.lock().unwrap().creation {
            Creation::Bound(t) => Some(t.clone()),
            _ => None,
        }
    }

    fn try_admit_request(&self, conn: ConnId, id: &RequestId, method: &str) -> IdAdmission {
        let is_creation = method == CREATION_METHOD;
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;

        // SWITCH ADMISSION (2e-4c). Round-2 P3's rule was "one thread per session, and one
        // creation in flight at a time"; 2e-4c keeps the second half exactly and replaces
        // the first with "one ACTIVE thread at a time".
        //
        // MEASURED (2e-4c spike, real codex 0.147 TUI, `/new`): the affordance sends
        // `thread/unsubscribe{threadId: <active>}` TWICE and then a `thread/start` whose
        // params are BYTE-IDENTICAL to the session's first one, on the SAME connection.
        // There is nothing about that second creation a fingerprint can distinguish from
        // the first — which is precisely why it is safe to admit it as a switch: it was
        // proven by the identical rule, against the identical launch fingerprint.
        //
        // The four arms:
        //   Open           — the session's FIRST creation (unchanged).
        //   Bound(active)  — a SWITCH. Admitted, carrying `active` so a failed switch can
        //                    restore it. `active` becomes retired only when the new
        //                    creation is CORRELATED AND VERIFIED, never at claim time.
        //   Pending{..}    — a switch is already in flight. ONE AT A TIME: refuse. This is
        //                    round-2 P3's pipeline race, unchanged and still load-bearing —
        //                    two creations in flight would let the second response silently
        //                    re-point the head.
        //   Closed(_)      — wedged-safe; refuse.
        if is_creation {
            expire_reservation(g);
            // **A reservation held by ANOTHER connection wins** (round-2 P3). That
            // connection's `thread/unsubscribe` prefix has already had a wire effect, so
            // the switch behind it must not lose the slot to a creation that started later.
            if let Some(res) = &g.switch_reservation {
                if res.conn != conn {
                    return IdAdmission::CreationSlotClosed;
                }
            }
            // Every precondition, from the ONE definition the prefix pre-check also uses.
            if creation_preconditions(g, conn, false).is_err() {
                return IdAdmission::CreationSlotClosed;
            }
        }
        // **THE LEDGER RUNS BEFORE THE RESERVATION IS CONSUMED** (round-3 P4). An oversized
        // or reused creation id sends ZERO bytes, so it is not the switch the prefix was
        // holding the slot for and must not consume it. Ordering this the other way round
        // let a malformed creation quietly eat a reservation that a real unsubscribe had
        // already paid for on the wire.
        match admit_id(g, conn, id, method) {
            IdAdmission::Admitted => {}
            other => {
                if is_creation {
                    // The prefix DID land, and this creation will never consume it. The
                    // connection is provably unsubscribed from the head with no switch
                    // behind it — exactly P4's errored-start situation, reached earlier.
                    if let Some(res) = g.switch_reservation.take() {
                        if res.conn == conn {
                            wedge(g, conn, res.thread);
                        } else {
                            g.switch_reservation = Some(res);
                        }
                    }
                }
                return other;
            }
        }
        // Admitted. NOW the reservation is consumed: this is the switch it was held for.
        let (superseding, consumed_reservation) = if is_creation {
            let mine = match g.switch_reservation.take() {
                Some(res) if res.conn == conn => true,
                Some(other) => {
                    g.switch_reservation = Some(other);
                    false
                }
                None => false,
            };
            // The clone happens only after every cheap refusal (P9).
            let superseding = match &g.creation {
                Creation::Bound(active) => Some(active.clone()),
                _ => None,
            };
            (superseding, mine)
        } else {
            (None, false)
        };
        if is_creation {
            g.conns.entry(conn).or_default().reserved.insert(id.clone());
            g.creation = Creation::Pending {
                conn,
                id: id.clone(),
                superseding,
                prefix_forwarded: consumed_reservation,
            };
        }
        IdAdmission::Admitted
    }

    fn try_admit_turn(
        &self,
        conn: ConnId,
        id: &RequestId,
        thread_id: &str,
        cwd: Option<&Value>,
        roots: Option<&Value>,
    ) -> TurnAdmission {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;

        // 1. The head-check, against the ACTIVE head only.
        //
        //    A `turn/start` may name only a thread whose creation THIS broker admitted and
        //    whose creation RESPONSE it correlated and verified — a thread it merely
        //    *heard announced* was never a valid head. A retired thread is readable and
        //    unturnable; a `Pending` switch leaves no head at all, so a turn arriving
        //    mid-switch refuses here, which is the other half of the linearization.
        //
        //    This is also the ONLY thing that discharges the measured `sandboxPolicy: null`
        //    deferral (see `crate::fingerprint`): the named thread's policy was
        //    fingerprint-proven at ITS creation, and `thread/settings/update` cannot move
        //    it afterwards.
        //
        //    The ids are grammar-checked before they are logged (`redact::thread_id`):
        //    `params.threadId` is client-chosen and this detail lands in a durable
        //    `broker.log`, so echoing it raw would be a log-injection channel.
        let Creation::Bound(active) = &g.creation else {
            return TurnAdmission::NotTheHead {
                detail: format!(
                    "turn/start names thread {} but this session has no verified bound                      thread (no creation this broker admitted has been answered by a                      correlated, fully verified creation response; a switch in flight also                      leaves no head)",
                    crate::redact::thread_id(thread_id)
                ),
            };
        };
        if active.id != thread_id {
            return TurnAdmission::NotTheHead {
                detail: format!(
                    "turn/start names thread {} but this session's bound thread is {}",
                    crate::redact::thread_id(thread_id),
                    crate::redact::thread_id(&active.id)
                ),
            };
        }
        // 2. The workspace bound at that head's creation, by exact `Value` equality.
        //
        //    RESPONSE, not request: the measured `thread/start` sends `"cwd": null` while
        //    the turn sends a concrete path, so binding the request's cwd would refuse
        //    every real turn. The creation RESPONSE carries the server-resolved values, and
        //    `crate::session` only installs them after proving that response's `cwd` equals
        //    the coordinator-owned launch cwd — so this equality transitively anchors the
        //    turn to the launch workspace.
        //
        //    EXACT equality, not normalization: a path canonicalizer is speculative until
        //    a real client is observed varying its representation, and it could only ever
        //    make the check accept MORE. The one canonicalization in the system happens at
        //    the coordinator. There is deliberately no scope-SUBSET reasoning either — a
        //    turn under a narrower root is still a different workspace than the one the
        //    policy was proven over.
        //
        //    Refusal details name the FIELD, never the value: these land in a durable log
        //    read by operators and gates, and both sides are attacker-supplied or
        //    filesystem layout.
        if cwd != Some(&active.cwd) {
            return TurnAdmission::WrongWorkspace {
                detail: format!(
                    "turn/start: params.cwd does not equal the cwd bound at this thread's                      creation (turn: {}; bound: {}) — values withheld from the audit log",
                    crate::redact::value_shape(cwd),
                    crate::redact::value_shape(Some(&active.cwd))
                ),
            };
        }
        if roots != Some(&active.roots) {
            return TurnAdmission::WrongWorkspace {
                detail: format!(
                    "turn/start: params.runtimeWorkspaceRoots does not equal the roots                      bound at this thread's creation (turn: {}; bound: {}) — values                      withheld from the audit log",
                    crate::redact::value_shape(roots),
                    crate::redact::value_shape(Some(&active.roots))
                ),
            };
        }
        // 2b. **This connection unsubscribed itself and never came back** (round-2 P4).
        //     Its prefix forwarded and the switch behind it failed, so it is no longer
        //     receiving this thread's stream. Authorizing its turns would run turns nobody
        //     on that connection can see. Cleared by a real `thread/resume` (see
        //     `note_resubscribe`), which is the client's own recovery path.
        expire_reservation(g);
        if let Some(w) = g.unsubscribed.get(&conn) {
            if w.thread == thread_id {
                return TurnAdmission::ConnectionUnsubscribed {
                    thread: w.thread.clone(),
                };
            }
        }
        // 2c. **A switch is reserved** (round-2 P3): its prefix has already had a wire
        //     effect and its `thread/start` is expected next, so a turn admitted now would
        //     be authorized against a head that is about to move.
        expire_reservation(g);
        if g.switch_reservation.is_some() {
            return TurnAdmission::SwitchReserved;
        }
        // 2d. **This id still holds a live turn entry** (round-3 P1). A `turn/start`
        //     response drains the OUTSTANDING ledger, so the id becomes reusable there —
        //     but the turn it started may still be running, and admitting a second turn
        //     under the same id would overwrite that turn's only mark. Its error response
        //     would then release a mark belonging to a live turn. Same discipline as the
        //     in-flight-reuse rule, extended across the answered-to-terminated window;
        //     counted as the protocol-hostile event it is.
        //
        //     Checked BEFORE the cardinality cap (closing S11): a reused id is a
        //     protocol-hostile event whose accounting must be universal, and at the cap it
        //     would otherwise be reported as "too many turns" and never counted.
        if g.active_turns.holds(conn, id) {
            g.counts.reused_in_flight += 1;
            return TurnAdmission::Ledger(IdAdmission::ReusedInFlight);
        }
        // 2e. Cardinality (round-2 P2).
        if g.active_turns.len() >= MAX_ACTIVE_TURNS {
            return TurnAdmission::TooManyActiveTurns;
        }
        // 3. The id ledger, in the SAME section.
        match admit_id(g, conn, id, TURN_METHOD) {
            IdAdmission::Admitted => {}
            other => return TurnAdmission::Ledger(other),
        }
        // 4. Busy-mark, still in the same section. Nothing between here and the relay's
        //    write can admit a switch.
        g.active_turns.admit(conn, id.clone(), thread_id);
        TurnAdmission::Admitted
    }

    fn release_turn(&self, conn: ConnId, id: &RequestId) {
        self.inner.lock().unwrap().active_turns.release(conn, id);
    }

    fn head_or_superseded(&self) -> Option<String> {
        self.head_or_superseded_impl()
    }

    fn switch_prefix_admissible(&self, conn: ConnId, thread: &str) -> Result<(), &'static str> {
        let g = self.inner.lock().unwrap();
        // **Only an unsubscribe naming the ACTIVE HEAD is a switch prefix** (round-3 P5).
        // A retired thread's cleanup unsubscribes something this session is not on: it
        // begins no switch, so reserving for it would fence turns and block other
        // connections' creations for a frame that changes nothing.
        match &g.creation {
            Creation::Bound(active) if active.id == thread => {}
            Creation::Bound(_) => {
                return Err(
                    "this unsubscribe does not name the session's active head, so \
                            it begins no switch",
                )
            }
            // A switch is already in flight. The thread it is superseding still reads as
            // the head (round-3 P5), so a SECOND prefix lands here and is refused with zero
            // bytes rather than quietly forwarding into an in-flight switch.
            Creation::Pending { .. } => return Err("a thread creation is already in flight"),
            // No head bound yet: the next `thread/start` is a FIRST creation, not a switch,
            // and it needs no unsubscribe prefix.
            _ => return Err("this session has no bound thread to switch away from"),
        }
        creation_preconditions(&g, conn, true)
    }

    fn reserve_switch(&self, conn: ConnId, thread: &str) {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        expire_reservation(g);
        // Reserve, or refresh — `/new` sends the prefix TWICE, and the second frame must
        // not collide with the reservation the first one made.
        g.switch_reservation = Some(SwitchReservation {
            conn,
            thread: thread.to_string(),
            at: std::time::Instant::now(),
        });
    }

    fn note_resubscribe_attempt(&self, conn: ConnId, thread: &str, id: &RequestId) {
        let mut g = self.inner.lock().unwrap();
        // The attempt is stamped with the wedge it is trying to lift, so an answer that
        // arrives after a NEWER wedge replaced it clears nothing (closing S5).
        if let Some(w) = g.unsubscribed.get(&conn) {
            if w.thread == thread {
                let stamp = w.seq;
                if g.resubscribing.len() >= MAX_RESUBSCRIBE_ATTEMPTS {
                    return;
                }
                g.resubscribing.insert((conn, id.clone()), stamp);
            }
        }
    }

    fn id_ledger_counts(&self) -> IdLedgerCounts {
        self.inner.lock().unwrap().counts
    }

    fn rollback_creation(&self, conn: ConnId, id: &RequestId) {
        let mut g = self.inner.lock().unwrap();
        let superseding = match &g.creation {
            Creation::Pending {
                conn: c,
                id: i,
                superseding,
                ..
            } if *c == conn && i == id => superseding.clone(),
            _ => return,
        };
        // Zero bytes went upstream, so the id is released — from the outstanding ledger as
        // well as the reservation — rather than tombstoned, and the client may retry,
        // including with this very id.
        if let Some(slots) = g.conns.get_mut(&conn) {
            slots.reserved.remove(id);
            slots.outstanding.remove(id);
        }
        g.switch_reservation = None;
        // A rolled-back SWITCH restores the head it was superseding: nothing reached the
        // server, so the old thread is still the one the session is on and turns must keep
        // working. (2e-4c; the non-switch case is round-2 P3's original reopen.)
        g.creation = match superseding {
            Some(old_active) => Creation::Bound(old_active),
            None => Creation::Open,
        };
    }

    fn close_connection(&self, conn: ConnId) {
        let mut g = self.inner.lock().unwrap();
        if let Creation::Pending {
            conn: c,
            superseding,
            ..
        } = &g.creation
        {
            if *c == conn {
                // **A disconnected SWITCH still retires the head it superseded**
                // (round-1 P3). The head is not RESTORED — unlike the rollback above, the
                // request DID reach the server, so a thread may exist this broker cannot
                // name and authorizing turns on the old head could authorize a turn
                // against a thread nobody is on. But it must not be FORGOTTEN either: it
                // is a thread this session provably bound, its history is real, and a
                // phone client may have its timeline open. Turn authorization is what
                // wedges; readability is not — the same rule as the indeterminate-response
                // path, and stating it in both places is what makes "readable on every
                // path out" an invariant rather than a coincidence.
                let superseded = superseding.clone();
                g.creation = Creation::Closed(
                    "the connection that owned the pending creation disconnected before \
                     its response landed; the request DID reach the server, so creation is \
                     closed rather than reopened (D2 owns the reconciliation)",
                );
                if let Some(old_active) = superseded {
                    retire(&mut g, old_active);
                }
            }
        }
        g.active_turns.close_connection(conn);
        if g.switch_reservation
            .as_ref()
            .is_some_and(|r| r.conn == conn)
        {
            g.switch_reservation = None;
        }
        g.unsubscribed.remove(&conn);
        g.resubscribing.retain(|(c, _), _| *c != conn);
        g.conns.remove(&conn);
    }

    /// A session thread is the ACTIVE one **or any thread this session retired** (2e-4c).
    ///
    /// This is the one place the switch widens an authorization surface, and it is measured
    /// rather than assumed: after `/new`, a `thread/resume` for the switched-away thread is
    /// answered by the real app-server with that thread's own full populated history. The
    /// ccd link resumes a retired thread while it follows a switch, and a phone client
    /// legitimately reads a previous thread's timeline.
    ///
    /// It widens `thread/resume` and `thread/unsubscribe` ONLY. `turn/start` goes through
    /// [`ThreadBinding::sole_session_thread`], which stays the ACTIVE head — so a retired
    /// thread is readable and unturnable, which is exactly what the TUI itself does.
    /// ## The in-flight switch window
    ///
    /// While a switch is `Pending` the superseded thread is in NEITHER `Bound` nor
    /// `retired` — it is held inside the pending state, waiting to learn whether it will be
    /// restored or retired. It must still answer TRUE here, and that is not a nicety: the
    /// ccd link can be mid-`thread/resume` retry at the instant the operator presses
    /// `/new`, and a resume of a thread that was a session thread a millisecond ago must
    /// not refuse because of a race the client cannot see. Whichever way the switch
    /// resolves, the answer stays TRUE (restored ⇒ active; verified ⇒ retired; wedged ⇒
    /// retired), so this is not a window of *optimism* — the thread is a session thread on
    /// every path out.
    fn is_session_thread(&self, thread_id: &str) -> bool {
        let g = self.inner.lock().unwrap();
        match &g.creation {
            Creation::Bound(active) if active.id == thread_id => return true,
            Creation::Pending {
                superseding: Some(old),
                ..
            } if old.id == thread_id => return true,
            _ => {}
        }
        g.retired.iter().any(|t| &**t == thread_id)
    }

    fn creation_closed_reason(&self) -> Option<&'static str> {
        match &self.inner.lock().unwrap().creation {
            Creation::Closed(why) => Some(why),
            _ => None,
        }
    }
}

impl SessionThreads {
    /// See [`ThreadBinding::head_or_superseded`].
    fn head_or_superseded_impl(&self) -> Option<String> {
        match &self.inner.lock().unwrap().creation {
            Creation::Bound(t) => Some(t.id.clone()),
            Creation::Pending {
                superseding: Some(t),
                ..
            } => Some(t.id.clone()),
            _ => None,
        }
    }

    /// Backdate the live switch reservation so its TTL has provably elapsed (round-2 P3).
    ///
    /// Test-only, and the alternative was worse: the only other way to exercise the TTL is
    /// to sleep for [`SWITCH_RESERVATION_TTL`], which would put a ten-second sleep in the
    /// unit suite and still prove nothing about the boundary. This drives the REAL
    /// `is_live()` path with a real elapsed instant.
    #[cfg(test)]
    fn backdate_reservation(&self) {
        let mut g = self.inner.lock().unwrap();
        if let Some(res) = g.switch_reservation.as_mut() {
            res.at = std::time::Instant::now() - SWITCH_RESERVATION_TTL - COMFORTABLY_PAST;
        }
    }

    /// How many re-subscribe attempts are outstanding (closing S4). Test/observability
    /// accessor: an attempt is invisible from outside until its answer lands, so a test
    /// that could not see the map would be asserting the rule only through its effect.
    pub fn resubscribe_attempts(&self) -> usize {
        self.inner.lock().unwrap().resubscribing.len()
    }

    /// How many threads this session has retired (2e-4c). Test/observability accessor: the
    /// retirement cap is a fail-closed rule and a test that could not see the list would be
    /// asserting the cap only through its side effect.
    pub fn retired_len(&self) -> usize {
        self.inner.lock().unwrap().retired.len()
    }
}

/// A binding that knows no threads and never opens a creation — every resume target is
/// unbound and every creation is refused (fail closed). Used as the default in unit tests
/// that are not exercising thread binding.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoThreads;

impl ThreadBinding for NoThreads {
    fn bound_thread(&self) -> Option<VerifiedThread> {
        None
    }

    fn try_admit_request(&self, _conn: ConnId, _id: &RequestId, method: &str) -> IdAdmission {
        // No creation is ever opened (that is what makes every resume target unbound); every
        // other request is admitted without being tracked, because this double has no
        // per-connection state for a response to release. The reuse/capacity/length rules
        // are [`SessionThreads`]'s, and the tests that exercise them use it.
        if method == CREATION_METHOD {
            IdAdmission::CreationSlotClosed
        } else {
            IdAdmission::Admitted
        }
    }

    fn rollback_creation(&self, _conn: ConnId, _id: &RequestId) {}

    fn close_connection(&self, _conn: ConnId) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LAUNCH_CWD: &str = "/work/proj";
    const A: ConnId = ConnId(1);
    const B: ConnId = ConnId(2);

    fn store() -> SessionThreads {
        SessionThreads::new(LAUNCH_CWD)
    }

    fn req(id: &str) -> RequestId {
        RequestId::Str(id.to_string())
    }

    /// Admit a `thread/start` — round 2's `try_open_creation`, which round-3 P1 folded into
    /// [`ThreadBinding::try_admit_request`] under [`CREATION_METHOD`].
    fn open(s: &SessionThreads, conn: ConnId, id: &RequestId) -> bool {
        s.try_admit_request(conn, id, CREATION_METHOD) == IdAdmission::Admitted
    }

    /// Admit an ordinary (non-creation) forwarded request.
    fn admit(s: &SessionThreads, conn: ConnId, id: &RequestId, method: &str) -> IdAdmission {
        s.try_admit_request(conn, id, method)
    }

    /// A creation response in the captured shape (`result.thread.id`, `result.cwd`,
    /// `result.runtimeWorkspaceRoots`).
    fn creation_response(id: &str, thread: &str) -> String {
        json!({
            "id": id,
            "result": {
                "thread": {"id": thread},
                "cwd": LAUNCH_CWD,
                "runtimeWorkspaceRoots": ["/work"]
            }
        })
        .to_string()
    }

    /// The `cwd` and `roots` a [`creation_response`] binds — what a turn must name to pass
    /// the workspace half of the atomic admission (round-1 P1).
    fn cwd() -> Value {
        json!(LAUNCH_CWD)
    }
    fn roots() -> Value {
        json!(["/work"])
    }

    /// The `turn/start` RESPONSE, in the measured shape (`result.turn.id`). This is what
    /// moves an admitted entry from `Unanswered` to `Answered` (round-2 P1).
    fn turn_started_response(id: &str, turn: &str) -> String {
        json!({"id": id, "result": {"turn": {"id": turn, "status": "inProgress"}}}).to_string()
    }

    /// A `turn/completed` naming the epoch it ends (round-3 P2).
    fn terminal(thread: &str, turn: &str, status: &str) -> String {
        json!({"method": "turn/completed",
               "params": {"threadId": thread, "turn": {"id": turn, "status": status}}})
        .to_string()
    }

    /// A successful `thread/resume` answer — the shape that lifts a P4 wedge (round-3 P6).
    fn resume_ok(id: &str) -> String {
        json!({"id": id, "result": {"thread": {"id": "01a0"}}}).to_string()
    }

    /// A structurally valid JSON-RPC error answer (round-3 P2: an INTEGER `code` AND a
    /// STRING `message`).
    fn error_response(id: &str) -> String {
        json!({"id": id, "error": {"code": -1, "message": "boom"}}).to_string()
    }

    #[test]
    fn a_correlated_creation_response_binds() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        s.observe_server_frame(A, &creation_response("startup-1", "01a0"));
        assert_eq!(
            s.bound_thread(),
            Some(VerifiedThread {
                id: "01a0".into(),
                cwd: json!(LAUNCH_CWD),
                roots: json!(["/work"]),
            })
        );
        assert!(s.is_session_thread("01a0"));
        assert_eq!(s.sole_session_thread(), Some("01a0".to_string()));
    }

    // P1 ROOT FIX — receipt is not lineage.
    #[test]
    fn a_bare_thread_started_binds_nothing() {
        let s = store();
        // With nothing pending (the cheap guard) …
        s.observe_server_frame(
            A,
            r#"{"method":"thread/started","params":{"thread":{"id":"01a0","path":"/x"}}}"#,
        );
        assert_eq!(s.bound_thread(), None);
        // … and with a creation pending, so the guard is not what refuses.
        assert!(open(&s, A, &req("startup-1")));
        s.observe_server_frame(
            A,
            r#"{"method":"thread/started","params":{"thread":{"id":"01a0","path":"/x"}}}"#,
        );
        s.observe_server_frame(
            A,
            r#"{"method":"thread/resumed","params":{"thread":{"id":"01a0","path":"/x"}}}"#,
        );
        assert_eq!(
            s.bound_thread(),
            None,
            "a notification may only confirm a binding, never seed one"
        );
        assert!(!s.is_session_thread("01a0"));
    }

    #[test]
    fn an_uncorrelated_response_binds_nothing() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        // Right shape, wrong id.
        s.observe_server_frame(A, &creation_response("some-other-id", "01a0"));
        assert_eq!(s.bound_thread(), None);
        // Right id, wrong CONNECTION.
        s.observe_server_frame(B, &creation_response("startup-1", "01a0"));
        assert_eq!(s.bound_thread(), None);
        // The pending entry survived both, so the correlated response still binds.
        s.observe_server_frame(A, &creation_response("startup-1", "01a0"));
        assert!(s.is_session_thread("01a0"));
    }

    // ROUND-2 P1 — the real vulnerability: two connections of the SAME role. Connection B
    // answering with connection A's request id must install NOTHING.
    #[test]
    fn a_same_role_sibling_connection_cannot_satisfy_our_pending() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        // B is a second TUI connection (the `/resume` picker). It answers A's id with a
        // response of its own choosing.
        s.observe_server_frame(B, &creation_response("startup-1", "attacker-thread"));
        assert_eq!(
            s.bound_thread(),
            None,
            "a sibling connection's response must not install a binding"
        );
        assert!(!s.is_session_thread("attacker-thread"));
        // And A's pending is untouched, so the genuine response still binds.
        s.observe_server_frame(A, &creation_response("startup-1", "01a0"));
        assert_eq!(s.sole_session_thread(), Some("01a0".into()));
    }

    // ROUND-3 P1 — a sibling connection's ERROR carrying our pending creation's id must not
    // REOPEN creation either (the reopen path is the one that could produce a second thread).
    #[test]
    fn a_sibling_connections_error_for_our_pending_id_does_not_reopen_creation() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        s.observe_server_frame(B, &error_response("startup-1"));
        assert!(
            !open(&s, A, &req("startup-2")),
            "a cross-connection error must not reopen the creation slot"
        );
        // A's pending is still live, so its own answer still works.
        s.observe_server_frame(A, &creation_response("startup-1", "01a0"));
        assert_eq!(s.sole_session_thread(), Some("01a0".into()));
    }

    // ROUND-2 P1 — tombstone: a replayed/duplicate response for a consumed id installs
    // nothing, even when creation has legitimately reopened in between.
    #[test]
    fn a_replayed_response_for_a_consumed_id_installs_nothing() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        // The creation fails, so the id is consumed and creation reopens.
        s.observe_server_frame(A, &error_response("startup-1"));
        assert!(s.creation_closed_reason().is_none());
        // The TOMBSTONE is what makes a replay unusable, and it acts on the CLAIM side:
        // without it the client could re-claim `startup-1`, and the stale response below
        // would then correlate to the NEW creation and bind a thread the broker never
        // proved. A consumed id is therefore permanently spent on this connection.
        assert!(
            !open(&s, A, &req("startup-1")),
            "a consumed request id must never be claimable again"
        );
        // A fresh creation is admitted under a NEW id …
        assert!(open(&s, A, &req("startup-2")));
        // … and the old response is replayed. It must not be correlated to anything.
        s.observe_server_frame(A, &creation_response("startup-1", "replayed"));
        assert_eq!(s.bound_thread(), None, "a replayed response binds nothing");
        // The live pending is still live.
        s.observe_server_frame(A, &creation_response("startup-2", "01a0"));
        assert_eq!(s.sole_session_thread(), Some("01a0".into()));
    }

    // ROUND-2 P1 — tombstone, claim side: a consumed id may not be claimed again.
    #[test]
    fn a_tombstoned_id_cannot_be_claimed_again() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        s.observe_server_frame(A, &error_response("startup-1"));
        assert!(
            !open(&s, A, &req("startup-1")),
            "reusing a consumed request id must be refused"
        );
        // A different id on the same connection is fine.
        assert!(open(&s, A, &req("startup-2")));
    }

    // O11 — RELABELLED. The `reserved` set is **defense-in-depth, not an independently
    // provable rule**: with one session-wide creation slot, a second claim while one is
    // pending is ALREADY refused by `creation != Creation::Open`, and a non-creation request
    // reusing the id is ALREADY refused by the outstanding ledger. Deleting the
    // `!slots.is_free(id)` check alone therefore fails no test, and this test does not claim
    // otherwise — it pins the behaviour that must survive a future MULTI-PENDING design
    // (D2's thread switch), where the single-slot rule no longer covers it.
    #[test]
    fn an_in_flight_creation_id_reservation_is_defense_in_depth() {
        let s = store();
        assert!(open(&s, A, &req("dup")));
        assert!(
            !open(&s, A, &req("dup")),
            "a second claim of an in-flight creation id is refused — today by the single \
             creation slot, and by the reservation if that slot ever admits several"
        );
        // Roll the claim back (nothing went out) and the id is free again.
        s.rollback_creation(A, &req("dup"));
        assert!(open(&s, A, &req("dup")));
    }

    // ROUND-2 P3 — a proven send failure rolls the claim back and re-opens creation.
    #[test]
    fn a_rolled_back_claim_reopens_creation() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        assert!(!open(&s, A, &req("startup-2")), "pending closes");
        s.rollback_creation(A, &req("startup-1"));
        assert!(
            open(&s, A, &req("startup-2")),
            "a creation whose bytes never went out must re-open"
        );
        // A rollback of something that is not the pending creation is a no-op.
        s.rollback_creation(B, &req("startup-2"));
        s.rollback_creation(A, &req("nope"));
        assert!(!open(&s, A, &req("startup-3")), "still pending");
    }

    // ROUND-3 P1 — a rollback releases the OUTSTANDING entry too, so the retried creation
    // may legitimately reuse the very id whose bytes never left the broker.
    #[test]
    fn a_rolled_back_claim_releases_its_outstanding_id() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        s.rollback_creation(A, &req("startup-1"));
        assert_eq!(
            admit(&s, A, &req("startup-1"), "app/list"),
            IdAdmission::Admitted,
            "the id must be free again for any method"
        );
    }

    // ROUND-2 P3 — the owning connection disconnects with a creation pending ⇒ CLOSED, and
    // a later creation is REFUSED, not reopened.
    #[test]
    fn a_disconnect_with_a_pending_creation_closes_it_terminally() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        s.close_connection(A);
        assert_eq!(s.bound_thread(), None);
        assert!(
            s.creation_closed_reason().is_some(),
            "the pending must land in the indeterminate closed state"
        );
        assert!(
            !open(&s, B, &req("startup-2")),
            "a closed creation is terminal: never reopened without evidence"
        );
        // And a late response for the vanished pending cannot resurrect it.
        s.observe_server_frame(A, &creation_response("startup-1", "01a0"));
        assert_eq!(s.bound_thread(), None);
    }

    #[test]
    fn a_disconnect_of_an_unrelated_connection_leaves_the_pending_alone() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        s.close_connection(B);
        assert!(s.creation_closed_reason().is_none());
        s.observe_server_frame(A, &creation_response("startup-1", "01a0"));
        assert_eq!(s.sole_session_thread(), Some("01a0".into()));
    }

    // ROUND-2 P2 — INVERTED from round 1's `a_creation_response_missing_a_proof_binds_
    // nothing`, which asserted that a partial/invalid response RE-OPENS creation. That
    // codified an unsafe behaviour: the server may have created a thread anyway, so
    // reopening risks a second one. The state is now CLOSED — binds nothing AND does not
    // reopen.
    #[test]
    fn a_creation_response_missing_a_proof_closes_creation_rather_than_reopening_it() {
        for body in [
            json!({"cwd": LAUNCH_CWD, "runtimeWorkspaceRoots": ["/work"]}),
            json!({"thread": {"id": "01a0"}, "runtimeWorkspaceRoots": ["/work"]}),
            json!({"thread": {"id": "01a0"}, "cwd": LAUNCH_CWD}),
            json!({"thread": {"id": ""}, "cwd": LAUNCH_CWD, "runtimeWorkspaceRoots": ["/work"]}),
            json!({"thread": {"id": "01a0"}, "cwd": null, "runtimeWorkspaceRoots": ["/work"]}),
            json!({"thread": {"id": "01a0"}, "cwd": LAUNCH_CWD, "runtimeWorkspaceRoots": null}),
        ] {
            let s = store();
            assert!(open(&s, A, &req("startup-1")));
            s.observe_server_frame(A, &json!({"id": "startup-1", "result": body}).to_string());
            assert_eq!(s.bound_thread(), None, "{body}");
            assert!(
                s.creation_closed_reason().is_some(),
                "the indeterminate state must be recorded: {body}"
            );
            assert!(
                !open(&s, A, &req("startup-2")),
                "an indeterminate response must NOT reopen creation: {body}"
            );
        }
    }

    // ROUND-2 P2 — the "neither / both / null" arms of the state machine are all closed.
    #[test]
    fn only_an_exclusive_error_object_reopens_creation() {
        for frame in [
            json!({"id": "startup-1", "error": null}),
            json!({"id": "startup-1", "error": "boom"}),
            json!({"id": "startup-1", "error": {"code": -1, "message": "x"},
                   "result": {"thread": {"id": "x"}}}),
            json!({"id": "startup-1"}),
        ] {
            let s = store();
            assert!(open(&s, A, &req("startup-1")));
            s.observe_server_frame(A, &frame.to_string());
            assert_eq!(s.bound_thread(), None, "{frame}");
            assert!(
                !open(&s, A, &req("startup-2")),
                "only an exclusive, structurally valid error reopens creation: {frame}"
            );
        }
    }

    // ROUND-3 P2 — an exclusive `error` reopens creation ONLY if it is STRUCTURALLY a
    // JSON-RPC error: an object with an INTEGER `code` AND a STRING `message`. A partial or
    // ill-typed error proves nothing about whether the server created a thread, so it lands
    // in the indeterminate CLOSED state instead.
    #[test]
    fn only_a_well_formed_jsonrpc_error_reopens_creation() {
        for bad in [
            json!({}),
            json!({"code": -1}),
            json!({"message": "boom"}),
            json!({"code": "-1", "message": "boom"}),
            json!({"code": -1.5, "message": "boom"}),
            json!({"code": null, "message": "boom"}),
            json!({"code": -1, "message": 7}),
            json!({"code": -1, "message": null}),
            json!({"code": -1, "message": {"text": "boom"}}),
        ] {
            let s = store();
            assert!(open(&s, A, &req("startup-1")));
            s.observe_server_frame(A, &json!({"id": "startup-1", "error": bad}).to_string());
            assert_eq!(s.bound_thread(), None, "{bad}");
            assert!(
                s.creation_closed_reason().is_some(),
                "a malformed error must land in the indeterminate closed state: {bad}"
            );
            assert!(
                !open(&s, A, &req("startup-2")),
                "a malformed error must NOT reopen creation: {bad}"
            );
        }
        // …and the well-formed shapes DO reopen, including a positive code, a large code,
        // and one carrying the spec's optional `data`.
        for good in [
            json!({"code": -1, "message": "boom"}),
            json!({"code": 0, "message": ""}),
            json!({"code": 4294967296i64, "message": "big"}),
            json!({"code": -32601, "message": "no such method", "data": {"any": true}}),
        ] {
            let s = store();
            assert!(open(&s, A, &req("startup-1")));
            s.observe_server_frame(A, &json!({"id": "startup-1", "error": good}).to_string());
            assert!(s.creation_closed_reason().is_none(), "{good}");
            assert!(
                open(&s, A, &req("startup-2")),
                "a well-formed failure must stay retryable: {good}"
            );
        }
    }

    // ROUND-2 P4 — the workspace anchor. A response naming a cwd other than the
    // COORDINATOR-owned launch cwd binds nothing (and closes creation).
    #[test]
    fn a_creation_response_outside_the_launch_cwd_binds_nothing() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        s.observe_server_frame(
            A,
            &json!({
                "id": "startup-1",
                "result": {
                    "thread": {"id": "01a0"},
                    "cwd": "/somewhere/else",
                    "runtimeWorkspaceRoots": ["/work"]
                }
            })
            .to_string(),
        );
        assert_eq!(s.bound_thread(), None);
        assert!(s.creation_closed_reason().is_some());
    }

    // ROUND-2 P4 — `runtimeWorkspaceRoots` must be a well-typed, non-empty array of
    // non-empty strings.
    #[test]
    fn malformed_workspace_roots_bind_nothing() {
        for roots in [
            json!("/work"),
            json!([]),
            json!([1]),
            json!([""]),
            json!(["/work", 2]),
            json!({"0": "/work"}),
        ] {
            let s = store();
            assert!(open(&s, A, &req("startup-1")));
            s.observe_server_frame(
                A,
                &json!({
                    "id": "startup-1",
                    "result": {
                        "thread": {"id": "01a0"},
                        "cwd": LAUNCH_CWD,
                        "runtimeWorkspaceRoots": roots
                    }
                })
                .to_string(),
            );
            assert_eq!(s.bound_thread(), None, "roots {roots}");
        }
    }

    #[test]
    fn an_error_response_clears_the_pending_and_reopens_creation() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        assert!(
            !open(&s, A, &req("startup-2")),
            "a second creation while one is pending is refused"
        );
        s.observe_server_frame(A, &error_response("startup-1"));
        assert_eq!(s.bound_thread(), None);
        assert!(
            open(&s, A, &req("startup-2")),
            "a failed creation must be retryable"
        );
    }

    // P3 (as evolved by 2e-4c) — ONE CREATION IN FLIGHT is still absolute; ONE THREAD PER
    // SESSION is now ONE ACTIVE thread per session.
    #[test]
    fn creation_is_closed_while_pending_but_a_bound_head_admits_a_switch() {
        let s = store();
        assert!(open(&s, A, &req("a")));
        assert!(!open(&s, A, &req("b")), "pending closes");
        assert!(!open(&s, B, &req("b")), "across connections too");
        s.observe_server_frame(A, &creation_response("a", "01a0"));
        // The half that CHANGED: a bound head admits the measured `/new` switch.
        assert!(open(&s, A, &req("c")), "a bound head admits a switch");
        // The half that did NOT: one at a time, across connections too.
        assert!(
            !open(&s, A, &req("d")),
            "a switch in flight closes the slot"
        );
        assert!(!open(&s, B, &req("d")), "across connections too");
        // While the switch is in flight there is NO head: `Bound` was consumed by the
        // pending state, so a turn refuses. That is the linearization point and it is
        // fail-closed — the TUI does not send a turn until its `thread/start` is answered,
        // and a turn arriving in the window would name the new thread, which is unproven.
        assert_eq!(s.sole_session_thread(), None, "no head during the switch");
        assert_eq!(s.bound_thread(), None);
        // But the superseded thread is STILL a session thread throughout the window — a ccd
        // link mid-resume-retry must not be refused by a race it cannot see.
        assert!(
            s.is_session_thread("01a0"),
            "the superseded head stays resumable while the switch is in flight"
        );
    }

    /// The switch's happy path: the head MOVES, the old thread is RETIRED — still a session
    /// thread (resumable), no longer the head (unturnable) — and the workspace binding
    /// follows the new head.
    #[test]
    fn a_verified_switch_moves_the_head_and_retires_the_old_one() {
        let s = store();
        assert!(open(&s, A, &req("a")));
        s.observe_server_frame(A, &creation_response("a", "01a0"));
        assert!(open(&s, A, &req("b")));
        s.observe_server_frame(A, &creation_response("b", "01a1"));

        assert_eq!(s.sole_session_thread(), Some("01a1".to_string()));
        assert_eq!(s.bound_thread().unwrap().id, "01a1");
        assert!(
            s.is_session_thread("01a1"),
            "the new head is a session thread"
        );
        assert!(
            s.is_session_thread("01a0"),
            "RETIRED IS NOT FORGOTTEN — a switched-away thread stays resumable"
        );
        assert!(!s.is_session_thread("01a2"), "a stranger is still refused");

        // A→B→A→B chain: every thread the session bound stays readable, exactly one is
        // ever the head.
        assert!(open(&s, A, &req("c")));
        s.observe_server_frame(A, &creation_response("c", "01a2"));
        assert_eq!(s.sole_session_thread(), Some("01a2".to_string()));
        for tid in ["01a0", "01a1", "01a2"] {
            assert!(s.is_session_thread(tid), "{tid} must stay a session thread");
        }
    }

    /// A switch that provably FAILS restores the old head. Anything else would leave a live
    /// session headless — refusing every turn on a thread that is still perfectly valid.
    #[test]
    fn a_failed_switch_restores_the_superseded_head() {
        let s = store();
        assert!(open(&s, A, &req("a")));
        s.observe_server_frame(A, &creation_response("a", "01a0"));
        assert!(open(&s, A, &req("b")));
        // An exclusive, well-formed JSON-RPC error: the server created nothing.
        s.observe_server_frame(A, &error_response("b"));
        assert_eq!(
            s.sole_session_thread(),
            Some("01a0".to_string()),
            "a proven-failed switch must restore the head it superseded"
        );
        assert!(
            s.retired_len() == 0,
            "a switch that never bound retires nothing"
        );
        // And the session can switch again.
        assert!(open(&s, A, &req("c")));
        s.observe_server_frame(A, &creation_response("c", "01a1"));
        assert_eq!(s.sole_session_thread(), Some("01a1".to_string()));
    }

    /// A ROLLED-BACK switch (the relay's upstream write failed — zero bytes left the
    /// broker) likewise restores the head, and does not tombstone the id.
    #[test]
    fn a_rolled_back_switch_restores_the_superseded_head() {
        let s = store();
        assert!(open(&s, A, &req("a")));
        s.observe_server_frame(A, &creation_response("a", "01a0"));
        assert!(open(&s, A, &req("b")));
        s.rollback_creation(A, &RequestId::Str("b".into()));
        assert_eq!(s.sole_session_thread(), Some("01a0".to_string()));
        assert!(
            open(&s, A, &req("b")),
            "zero bytes went out, so the very same id may be retried"
        );
    }

    /// A switch whose response proves NEITHER success nor failure does NOT restore the old
    /// head: the server may hold a thread this broker cannot name and the TUI may already
    /// be sending turns to it, so authorizing turns on the old head would authorize a turn
    /// against a thread nobody is on. Wedged-safe — but every thread the session DID bind
    /// stays readable.
    #[test]
    fn an_indeterminate_switch_wedges_safe_without_forgetting_the_retired() {
        let s = store();
        assert!(open(&s, A, &req("a")));
        s.observe_server_frame(A, &creation_response("a", "01a0"));
        assert!(open(&s, A, &req("b")));
        s.observe_server_frame(A, &creation_response("b", "01a1"));
        assert_eq!(s.sole_session_thread(), Some("01a1".to_string()));
        // Now a third switch answered with a bare, unprovable frame.
        assert!(open(&s, A, &req("c")));
        s.observe_server_frame(A, r#"{"id":"c","result":{"thread":{"id":"01a2"}}}"#);
        assert_eq!(s.sole_session_thread(), None, "wedged safe: no head");
        assert!(s.creation_closed_reason().is_some());
        assert!(!open(&s, A, &req("d")), "and no further creation");
        // The retired list survives: an operator's phone can still read those timelines.
        assert!(s.is_session_thread("01a0"));
        assert!(s.is_session_thread("01a1"));
        assert!(
            !s.is_session_thread("01a2"),
            "the unproven thread is NOT adopted"
        );
    }

    // ---------------------------------------------------------------------------
    // Round-1 P1 — the turn/switch linearization.
    // ---------------------------------------------------------------------------

    /// A turn admitted for the head makes that head BUSY, and a switch is refused for as
    /// long as it is — through to the terminal of the turn it actually became.
    #[test]
    fn a_switch_is_refused_while_a_turn_is_active_and_admitted_once_it_terminates() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));

        // A switch IS admissible before any turn.
        assert!(
            s.try_admit_turn(A, &req("t1"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        assert!(
            !open(&s, A, &req("switch")),
            "a switch while a turn is ACTIVE must be refused"
        );
        assert_eq!(s.sole_session_thread(), Some("01a0".to_string()));

        // The server ANSWERS the turn/start: the entry now belongs to a turn epoch.
        s.observe_server_frame(A, &turn_started_response("t1", "01a0-turn"));
        assert!(!open(&s, A, &req("switch")), "still running");

        // The terminal releases it — on ANY status, because all three end the turn.
        s.observe_server_frame(A, &terminal("01a0", "01a0-turn", "interrupted"));
        assert!(
            open(&s, A, &req("switch")),
            "once the answered turn terminates the switch must be admitted"
        );
    }

    /// **ROUND-2 P1, THE CAUSAL DEFECT.** A terminal must not release a `turn/start` the
    /// server has not answered yet.
    ///
    /// The sequence, all of it reachable: turn T0 is running; a second `turn/start` S is
    /// admitted (and marks the thread busy) while T0 still runs; T0's terminal arrives
    /// BEFORE S's bytes have even been forwarded. Under the first form the terminal cleared
    /// every entry for the thread, S's mark included — so S went on to start turn T1 with
    /// nothing fencing it, and a `/new` admitted during T1 moved the head out from under a
    /// live turn. The mark existed precisely to make that impossible.
    ///
    /// An unanswered entry survives every terminal. It is cleared only by its OWN error
    /// response or by its admitting connection closing.
    #[test]
    fn a_terminal_does_not_release_a_turn_start_the_server_has_not_answered() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));

        // T0: admitted and ANSWERED.
        assert!(
            s.try_admit_turn(A, &req("t0"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.observe_server_frame(A, &turn_started_response("t0", "turn-0"));
        // S: admitted, NOT yet answered — its bytes have not even gone out.
        assert!(
            s.try_admit_turn(A, &req("s"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );

        // T0's terminal arrives first.
        s.observe_server_frame(A, &terminal("01a0", "turn-0", "completed"));

        assert!(
            !open(&s, A, &req("switch")),
            "T0's terminal must NOT release the unanswered S: S may still start a turn, \
             and a switch admitted now would move the head out from under it"
        );
        // S is released the moment the server says it never started...
        s.observe_server_frame(A, &error_response("s"));
        assert!(
            open(&s, A, &req("switch")),
            "an errored S releases its own mark"
        );
    }

    /// A steer's PHANTOM turn id (D12) must not wedge the session. The entry is answered,
    /// so the thread's next terminal clears it even though the recorded id never
    /// terminalizes on its own.
    #[test]
    fn an_answered_steer_with_a_phantom_turn_id_is_cleared_by_the_threads_terminal() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(
            s.try_admit_turn(A, &req("t0"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.observe_server_frame(A, &turn_started_response("t0", "turn-0"));
        // The steer: admitted and answered with an id that will never terminalize.
        assert!(
            s.try_admit_turn(A, &req("steer"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.observe_server_frame(A, &turn_started_response("steer", "phantom-never-exists"));
        // ONE terminal, naming the real turn, ends both.
        s.observe_server_frame(A, &terminal("01a0", "turn-0", "completed"));
        assert!(
            open(&s, A, &req("switch")),
            "requiring the recorded turn id to MATCH would leave every steer's phantom \
             entry uncleared and wedge switching for ever"
        );
    }

    /// **ROUND-3 P1 — an id with a LIVE turn entry is not reusable for a turn.**
    ///
    /// The defect: a `turn/start` response DRAINS its outstanding-ledger entry, so the id
    /// becomes reusable there while the turn it started is still running. A second
    /// `turn/start` under that id would `insert` over the first turn's entry — destroying
    /// the only mark that turn had — and its own error response would then release a mark
    /// belonging to a live turn, leaving the switch unfenced.
    #[test]
    fn a_turn_id_with_a_live_entry_cannot_be_reused_for_another_turn() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));

        // T0 admitted and ANSWERED — its ledger id is now drained and reusable.
        assert!(
            s.try_admit_turn(A, &req("t"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.observe_server_frame(A, &turn_started_response("t", "turn-0"));
        assert_eq!(
            s.try_admit_request(A, &req("t"), "thread/read"),
            IdAdmission::Admitted,
            "the OUTSTANDING ledger really has released it — which is the premise"
        );
        // ...and drain that probe again, so the refusal below can only come from the TURN
        // entry rather than from the probe's own outstanding id.
        s.observe_server_frame(A, &resume_ok("t"));

        // ...but the TURN entry is still live, so the id may not start another turn.
        assert_eq!(
            s.try_admit_turn(A, &req("t"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::Ledger(IdAdmission::ReusedInFlight),
            "reusing an id that still holds a live turn entry would overwrite that turn's \
             only mark"
        );
        assert!(
            s.id_ledger_counts().reused_in_flight >= 1,
            "and it is counted"
        );
        // The first turn's mark is intact: the switch is still fenced.
        assert!(!open(&s, A, &req("switch")));
        // Once its terminal lands, the id is free again.
        s.observe_server_frame(A, &terminal("01a0", "turn-0", "completed"));
        assert_eq!(
            s.try_admit_turn(A, &req("t"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::Admitted
        );
    }

    /// **ROUND-3 P2 — a terminal that names no EPOCH clears nothing.**
    ///
    /// Without a readable turn id a terminal cannot be told apart from its own duplicate,
    /// so acting on it would reintroduce exactly the defect the epoch gate closes. Asserted
    /// against an ANSWERED entry, because an unanswered one is protected by a different
    /// rule and would let this pass for the wrong reason.
    #[test]
    fn a_terminal_naming_no_turn_clears_nothing() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(
            s.try_admit_turn(A, &req("t0"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.observe_server_frame(A, &turn_started_response("t0", "turn-0"));
        for bad in [
            r#"{"method":"turn/completed","params":{"threadId":"01a0","turn":{"status":"completed"}}}"#,
            r#"{"method":"turn/completed","params":{"threadId":"01a0","turn":{"id":"","status":"completed"}}}"#,
            r#"{"method":"turn/completed","params":{"threadId":"01a0","turn":{"id":7}}}"#,
            r#"{"method":"turn/completed","params":{"threadId":"01a0"}}"#,
        ] {
            s.observe_server_frame(A, bad);
            assert!(
                !open(&s, A, &req("switch")),
                "a terminal naming no epoch must clear nothing: {bad}"
            );
        }
        // The real one still works.
        s.observe_server_frame(A, &terminal("01a0", "turn-0", "completed"));
        assert!(open(&s, A, &req("switch")));
    }

    /// **ROUND-3 P2 — a DUPLICATE terminal clears nothing.**
    ///
    /// A terminal reaches every subscribed connection and this broker watches every leg, so
    /// it sees the same terminal repeatedly by design. Without an epoch gate the second
    /// delivery clears entries admitted BETWEEN the two — a turn that is genuinely running
    /// — and unfences the switch it exists to fence.
    #[test]
    fn a_duplicate_terminal_clears_nothing() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(
            s.try_admit_turn(A, &req("t0"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.observe_server_frame(A, &turn_started_response("t0", "turn-0"));
        // First delivery: T0's epoch ends.
        s.observe_server_frame(A, &terminal("01a0", "turn-0", "completed"));

        // A NEW turn starts and is answered — genuinely running.
        assert!(
            s.try_admit_turn(A, &req("t1"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.observe_server_frame(A, &turn_started_response("t1", "turn-1"));
        assert!(!open(&s, A, &req("switch")), "turn-1 fences the switch");

        // The SAME terminal again, arriving on another leg.
        s.observe_server_frame(B, &terminal("01a0", "turn-0", "completed"));
        assert!(
            !open(&s, A, &req("switch")),
            "a duplicate terminal must clear NOTHING — turn-1 is still running, and a \
             switch admitted now would move the head out from under it"
        );
        // The genuine terminal for turn-1 still works.
        s.observe_server_frame(A, &terminal("01a0", "turn-1", "completed"));
        assert!(open(&s, A, &req("switch")));
    }

    /// **CLOSING S1 — a terminal epoch is keyed by `(thread, turn)`.**
    ///
    /// Turn ids are unique within a thread, not across a session. Keyed by turn alone,
    /// thread A's turn `t` shadows thread B's turn `t`: B's genuine terminal reads as a
    /// duplicate, clears nothing, and the session can never switch again. A switch is
    /// exactly when two threads' ids coexist in this set, which is the one situation it
    /// exists for.
    #[test]
    fn a_terminal_epoch_is_scoped_to_its_thread() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        // A's turn `t` runs and ends.
        assert!(
            s.try_admit_turn(A, &req("ta"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.observe_server_frame(A, &turn_started_response("ta", "t"));
        s.observe_server_frame(A, &terminal("01a0", "t", "completed"));

        // Switch to B.
        assert!(open(&s, A, &req("sw")));
        s.observe_server_frame(A, &creation_response("sw", "01a1"));
        // B has its OWN turn `t` — the server mints turn ids per thread.
        assert!(
            s.try_admit_turn(A, &req("tb"), "01a1", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.observe_server_frame(A, &turn_started_response("tb", "t"));
        assert!(!open(&s, A, &req("sw2")), "B's turn fences the next switch");

        s.observe_server_frame(A, &terminal("01a1", "t", "completed"));
        assert!(
            open(&s, A, &req("sw2")),
            "B's terminal must not be mistaken for A's duplicate — a session-wide key \
             would leave this session unable to switch for ever"
        );
    }

    /// **CLOSING S5 — a newer wedge REPLACES an older one, and an old answer cannot lift
    /// it.**
    ///
    /// Both halves need the first wedge to still be LIVE when the second failure happens,
    /// which needs the head to move in between — so the sequence is: wedge on A, switch
    /// successfully to B while still wedged, then fail a switch away from B.
    #[test]
    fn a_newer_wedge_replaces_an_older_one_and_a_stale_answer_cannot_lift_it() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));

        // (1) Wedge this connection against A: prefix lands, switch FAILS.
        assert!(prefix(&s, A, &req("u1"), "01a0"));
        assert!(open(&s, A, &req("sw1")));
        s.observe_server_frame(A, &error_response("sw1"));
        assert_eq!(
            s.try_admit_turn(A, &req("t0"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::ConnectionUnsubscribed {
                thread: "01a0".to_string()
            }
        );
        // An attempt to lift it is registered but NOT answered — it stays outstanding
        // across everything below.
        s.note_resubscribe_attempt(A, "01a0", &req("r-stale"));

        // (2) Switch SUCCESSFULLY to B, still wedged against A.
        assert!(prefix(&s, A, &req("u2"), "01a0"));
        assert!(open(&s, A, &req("sw2")));
        s.observe_server_frame(A, &creation_response("sw2", "01a1"));
        assert_eq!(s.sole_session_thread(), Some("01a1".to_string()));

        // (3) Now fail a switch away from B. The wedge must move to B.
        assert!(prefix(&s, A, &req("u3"), "01a1"));
        assert!(open(&s, A, &req("sw3")));
        s.observe_server_frame(A, &error_response("sw3"));
        assert_eq!(
            s.try_admit_turn(A, &req("t1"), "01a1", Some(&cwd()), Some(&roots())),
            TurnAdmission::ConnectionUnsubscribed {
                thread: "01a1".to_string()
            },
            "the NEWEST failure is what the wedge records — `or_insert` keeps the obsolete \
             A wedge and loses this one entirely, so the connection goes on being refused \
             for a thread it long since left while its real problem is unrecorded"
        );

        // (4) The LATE answer to the A-era attempt must lift nothing: it is stamped against
        //     a wedge that no longer exists.
        s.observe_server_frame(A, &resume_ok("r-stale"));
        assert_eq!(
            s.try_admit_turn(A, &req("t2"), "01a1", Some(&cwd()), Some(&roots())),
            TurnAdmission::ConnectionUnsubscribed {
                thread: "01a1".to_string()
            },
            "a stale answer stamped against an older wedge must clear nothing"
        );
        // ...while a fresh, correlated, accepted one does.
        s.note_resubscribe_attempt(A, "01a1", &req("r-now"));
        s.observe_server_frame(A, &resume_ok("r-now"));
        assert_eq!(
            s.try_admit_turn(A, &req("t3"), "01a1", Some(&cwd()), Some(&roots())),
            TurnAdmission::Admitted
        );
    }

    /// **ROUND-3 P3 — a disconnect is NOT a terminal.**
    ///
    /// An ANSWERED `turn/start` reached the server, and the turn it belongs to keeps
    /// running whether or not this leg is there to watch it (A15: turns are independent of
    /// any one subscription). Releasing its mark on close would let a switch move the head
    /// out from under a live turn — the very thing the mark prevents, reintroduced through
    /// a dropped connection.
    ///
    /// An UNANSWERED entry is the opposite: the server never said it became anything, and
    /// with the connection gone no answer ever will.
    #[test]
    fn a_disconnect_releases_only_unanswered_turns() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        // One ANSWERED turn and one UNANSWERED, both on A.
        assert!(
            s.try_admit_turn(A, &req("answered"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.observe_server_frame(A, &turn_started_response("answered", "turn-0"));
        assert!(
            s.try_admit_turn(A, &req("unanswered"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );

        s.close_connection(A);
        assert!(
            !open(&s, B, &req("switch")),
            "the ANSWERED turn is still running server-side, so the head must stay fenced"
        );
        // Its real terminal — observed on ANY leg — is what releases it.
        s.observe_server_frame(B, &terminal("01a0", "turn-0", "completed"));
        assert!(
            open(&s, B, &req("switch")),
            "and only that terminal releases it"
        );
    }

    /// Round-2 P2 — the concurrent-turn cardinality bound, refused legibly.
    #[test]
    fn the_active_turn_set_is_bounded() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        for i in 0..MAX_ACTIVE_TURNS {
            assert_eq!(
                s.try_admit_turn(
                    A,
                    &req(&format!("t{i}")),
                    "01a0",
                    Some(&cwd()),
                    Some(&roots())
                ),
                TurnAdmission::Admitted,
                "turn {i}"
            );
        }
        assert_eq!(
            s.try_admit_turn(
                A,
                &req("one-too-many"),
                "01a0",
                Some(&cwd()),
                Some(&roots())
            ),
            TurnAdmission::TooManyActiveTurns
        );
    }

    /// Round-2 P2 — the id byte cap applies to the TURN path too, which called `admit_id`
    /// directly and so used to bypass it entirely.
    #[test]
    fn the_turn_path_applies_the_request_id_byte_cap() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        let long = req(&"z".repeat(MAX_REQUEST_ID_BYTES + 1));
        assert_eq!(
            s.try_admit_turn(A, &long, "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::Ledger(IdAdmission::Oversized),
            "an over-long id must never be stored, whichever path admits it"
        );
        assert!(
            s.id_ledger_counts().oversized >= 1,
            "and it must be counted"
        );
    }

    /// A terminal for ANOTHER thread does not release this thread's turn    /// A terminal for ANOTHER thread does not release this thread's turn — otherwise a
    /// frame about a thread the session is not on could unlock a switch mid-turn.
    #[test]
    fn a_terminal_for_another_thread_does_not_release_the_busy_mark() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(
            s.try_admit_turn(A, &req("t1"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.observe_server_frame(A, &terminal("01a0-OTHER", "turn-0", "completed"));
        assert!(!open(&s, A, &req("switch")), "still busy");
        // ...and a malformed terminal releases nothing either (fail closed).
        for bad in [
            r#"{"method":"turn/completed","params":{}}"#,
            r#"{"method":"turn/completed"}"#,
            r#"{"method":"turn/completed","params":{"threadId":123}}"#,
            // Round-3 P2: no readable turn id names no EPOCH, so it cannot be told apart
            // from its own duplicate and must clear nothing.
            r#"{"method":"turn/completed","params":{"threadId":"01a0","turn":{"status":"completed"}}}"#,
            "not json at all but mentions \"turn/completed\"",
        ] {
            s.observe_server_frame(A, bad);
            assert!(!open(&s, A, &req("switch")), "released by {bad}");
        }
    }

    /// A turn that provably never started releases its mark, or the session could never
    /// switch again.
    #[test]
    fn an_errored_turn_start_releases_the_busy_mark() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(
            s.try_admit_turn(A, &req("t1"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        assert!(!open(&s, A, &req("switch")));
        s.observe_server_frame(A, &error_response("t1"));
        assert!(
            open(&s, A, &req("switch")),
            "an errored turn/start must release its mark"
        );
    }

    /// A disconnect takes only ITS OWN turns' marks.
    #[test]
    fn a_disconnect_releases_only_that_connections_turn_marks() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(
            s.try_admit_turn(A, &req("ta"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        assert!(
            s.try_admit_turn(B, &req("tb"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        s.close_connection(B);
        assert!(!open(&s, A, &req("switch")), "A's turn is still live");
        s.close_connection(A);
        assert!(
            open(&s, ConnId(9), &req("switch")),
            "with every turn-owning connection gone the session may switch again"
        );
    }

    /// The atomic admission refuses a turn that names a thread which is not the ACTIVE
    /// head — including one that is merely retired, and including mid-switch when there is
    /// no head at all.
    #[test]
    fn the_atomic_turn_admission_is_head_and_workspace_scoped() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(open(&s, A, &req("sw")));
        // Mid-switch: no head.
        assert!(matches!(
            s.try_admit_turn(A, &req("t0"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::NotTheHead { .. }
        ));
        s.observe_server_frame(A, &creation_response("sw", "01a1"));
        // Retired is unturnable.
        assert!(matches!(
            s.try_admit_turn(A, &req("t1"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::NotTheHead { .. }
        ));
        // Wrong workspace is its OWN refusal, not a head mismatch.
        assert!(matches!(
            s.try_admit_turn(
                A,
                &req("t2"),
                "01a1",
                Some(&json!("/elsewhere")),
                Some(&roots())
            ),
            TurnAdmission::WrongWorkspace { .. }
        ));
        assert!(matches!(
            s.try_admit_turn(
                A,
                &req("t3"),
                "01a1",
                Some(&cwd()),
                Some(&json!(["/elsewhere"]))
            ),
            TurnAdmission::WrongWorkspace { .. }
        ));
        // And a reused id is the LEDGER's refusal, kept distinct from both.
        assert!(
            s.try_admit_turn(A, &req("t4"), "01a1", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        assert!(matches!(
            s.try_admit_turn(A, &req("t4"), "01a1", Some(&cwd()), Some(&roots())),
            TurnAdmission::Ledger(IdAdmission::ReusedInFlight)
        ));
    }

    /// **The interleaving is unrepresentable, hammered from two threads.**
    ///
    /// The atomicity of `try_admit_turn` is a property of one held `MutexGuard`, which no
    /// single-line mutation can express and no sequential test can observe. What CAN be
    /// observed is the invariant it exists to produce, and it is asserted here under real
    /// contention: **a turn is never authorized against a head that a switch has already
    /// moved**, in either racing order.
    ///
    /// Each round drives one admit-a-turn and one admit-a-switch concurrently against the
    /// same store and then checks the outcome pair. Exactly three are legal:
    ///   * the turn won  ⇒ the switch was refused (the thread was busy);
    ///   * the switch won ⇒ the turn was refused (a pending switch leaves no head);
    ///   * both were refused (the loser arrived after the winner had already settled).
    ///
    /// "Both admitted" is the defect, and it is what this fails on.
    #[test]
    fn a_turn_and_a_switch_never_both_win() {
        use std::sync::Arc;
        for round in 0..200 {
            let s = Arc::new(store());
            assert!(open(&s, A, &req("start")));
            s.observe_server_frame(A, &creation_response("start", "01a0"));

            let s1 = Arc::clone(&s);
            let turn = std::thread::spawn(move || {
                s1.try_admit_turn(A, &req("t"), "01a0", Some(&cwd()), Some(&roots()))
                    == TurnAdmission::Admitted
            });
            let s2 = Arc::clone(&s);
            let switch = std::thread::spawn(move || {
                s2.try_admit_request(B, &req("sw"), CREATION_METHOD) == IdAdmission::Admitted
            });
            let turn_won = turn.join().expect("turn thread");
            let switch_won = switch.join().expect("switch thread");
            assert!(
                !(turn_won && switch_won),
                "round {round}: a turn was authorized against the head AND a switch was \
                 admitted to move it. That is the exact window the atomic admission exists \
                 to close."
            );
        }
    }

    // ---------------------------------------------------------------------------
    // Round-1 P3 — a disconnected switch still retires the head it superseded.
    // ---------------------------------------------------------------------------

    #[test]
    fn a_disconnect_mid_switch_retires_the_superseded_head_rather_than_forgetting_it() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(open(&s, A, &req("sw")));
        s.close_connection(A);
        // Wedged safe for TURNS...
        assert_eq!(s.sole_session_thread(), None);
        assert!(s.creation_closed_reason().is_some());
        // ...but the thread this session provably bound is still READABLE. "Readable on
        // every path out" is the invariant; a phone client may have its timeline open.
        assert!(
            s.is_session_thread("01a0"),
            "a disconnect mid-switch must RETIRE the superseded head, not forget it"
        );
        assert_eq!(s.retired_len(), 1);
    }

    // ---------------------------------------------------------------------------
    // Round-2 P3 / round-3 P4+P5 — the switch RESERVATION.
    // ---------------------------------------------------------------------------

    /// Admit a `thread/unsubscribe` of `thread` the way the classifier does: check
    /// admissibility, then admit the id, then reserve (round-3 P4 ordering).
    fn prefix(s: &SessionThreads, conn: ConnId, id: &RequestId, thread: &str) -> bool {
        if s.switch_prefix_admissible(conn, thread).is_err() {
            return false;
        }
        if s.try_admit_request(conn, id, "thread/unsubscribe") != IdAdmission::Admitted {
            return false;
        }
        s.reserve_switch(conn, thread);
        true
    }

    /// **The prefix is admissible exactly when the switch behind it is**, and the predicate
    /// is the SAME one the creation itself uses.
    #[test]
    fn a_switch_prefix_reserves_exactly_when_the_switch_is_admissible() {
        let s = store();
        assert!(
            s.switch_prefix_admissible(A, "01a0").is_err(),
            "nothing bound"
        );

        assert!(open(&s, A, &req("start")));
        assert!(
            s.switch_prefix_admissible(A, "01a0").is_err(),
            "a creation is in flight — the prefix of a SECOND switch must not forward"
        );
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(
            s.switch_prefix_admissible(A, "01a0").is_ok(),
            "bound and idle"
        );

        // A running turn blocks the prefix too.
        let t = store();
        assert!(open(&t, A, &req("start")));
        t.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(
            t.try_admit_turn(A, &req("t1"), "01a0", Some(&cwd()), Some(&roots()))
                == TurnAdmission::Admitted
        );
        assert!(
            t.switch_prefix_admissible(A, "01a0").is_err(),
            "a turn is active"
        );

        // A wedged session cannot switch, so it cannot unsubscribe either.
        let w = store();
        assert!(open(&w, A, &req("start")));
        w.observe_server_frame(A, r#"{"id":"start","result":{"thread":{"id":"01a0"}}}"#);
        assert!(w.creation_closed_reason().is_some(), "wedged");
        assert!(w.switch_prefix_admissible(A, "01a0").is_err(), "wedged");
    }

    /// **ROUND-3 P5 — only an unsubscribe naming the ACTIVE HEAD is a switch prefix.**
    ///
    /// A retired thread's cleanup unsubscribes something this session is not on. It begins
    /// no switch, so reserving for it would fence turns and block other connections'
    /// creations for a frame that changes nothing — a phantom fence.
    #[test]
    fn only_an_unsubscribe_of_the_active_head_is_a_switch_prefix() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(prefix(&s, A, &req("u1"), "01a0"));
        assert!(open(&s, A, &req("sw")));
        s.observe_server_frame(A, &creation_response("sw", "01a1"));
        // `01a0` is now RETIRED. It is still a session thread — resumable, unsubscribable —
        // but unsubscribing it begins no switch.
        assert!(s.is_session_thread("01a0"));
        assert!(
            s.switch_prefix_admissible(A, "01a0").is_err(),
            "a retired thread's cleanup must not reserve"
        );
        // ...and with no reservation, turns keep working.
        assert_eq!(
            s.try_admit_turn(A, &req("t"), "01a1", Some(&cwd()), Some(&roots())),
            TurnAdmission::Admitted
        );
    }

    /// **A reservation fences turns and competing creations, and is consumed by its own
    /// `thread/start`.**
    #[test]
    fn a_reservation_fences_the_window_between_the_prefix_and_the_start() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(prefix(&s, A, &req("u1"), "01a0"));

        // A turn admitted in the window would be authorized against a head about to move.
        assert_eq!(
            s.try_admit_turn(A, &req("t"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::SwitchReserved
        );
        // A creation from ANOTHER connection must not steal the slot.
        assert!(!open(&s, B, &req("thief")));
        // The reserving connection's own creation consumes it.
        assert!(open(&s, A, &req("sw")));
        s.observe_server_frame(A, &creation_response("sw", "01a1"));
        assert_eq!(s.sole_session_thread(), Some("01a1".to_string()));
        assert_eq!(
            s.try_admit_turn(A, &req("t2"), "01a1", Some(&cwd()), Some(&roots())),
            TurnAdmission::Admitted
        );
    }

    /// **ROUND-3 P4, BOTH DIRECTIONS.**
    ///
    /// (a) A prefix the ID LEDGER refuses sends zero bytes, so it leaves NO reservation —
    ///     otherwise turns are fenced and other connections blocked for a frame that never
    ///     went out.
    /// (b) A creation whose id the ledger refuses does NOT consume a reservation a REAL
    ///     prefix paid for on the wire — and because that prefix did land, the connection
    ///     is wedged exactly as it is on the errored-start path.
    #[test]
    fn the_reservation_lives_inside_the_transaction_in_both_directions() {
        // (a) the prefix's own id is over-long: refused by the ledger, nothing reserved.
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        let long = req(&"z".repeat(MAX_REQUEST_ID_BYTES + 1));
        assert!(
            !prefix(&s, A, &long, "01a0"),
            "an over-long prefix id is refused by the ledger"
        );
        assert_eq!(
            s.try_admit_turn(A, &req("t"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::Admitted,
            "a prefix that sent ZERO bytes must fence nothing"
        );

        // (b) a real prefix lands; the creation behind it has an unusable id.
        let u = store();
        assert!(open(&u, A, &req("start")));
        u.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(prefix(&u, A, &req("u1"), "01a0"));
        assert_eq!(
            u.try_admit_request(A, &long, CREATION_METHOD),
            IdAdmission::Oversized,
            "an oversized creation id is refused by the ledger, not by the slot"
        );
        // It did NOT consume the reservation as a switch — but the prefix DID land, so the
        // connection is unsubscribed with no switch behind it and must be wedged.
        assert_eq!(
            u.try_admit_turn(A, &req("t"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::ConnectionUnsubscribed {
                thread: "01a0".to_string()
            },
            "a real prefix with no switch behind it wedges the connection"
        );
    }

    /// A reservation dies with its connection; and expiry WEDGES rather than silently
    /// restoring authorization (round-3 P5).
    #[test]
    fn a_reservation_expires_with_the_connection_and_with_time() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(prefix(&s, A, &req("u1"), "01a0"));
        s.close_connection(A);
        assert_eq!(
            s.try_admit_turn(B, &req("t"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::Admitted,
            "a reservation must not outlive the connection that made it"
        );

        // **Expiry is not 'nothing happened'** (round-3 P5). The unsubscribe really went
        // upstream, so the connection is provably not receiving the head's stream; letting
        // the TTL silently restore its turn authorization would authorize turns nobody on
        // that connection can observe.
        let t = store();
        assert!(open(&t, A, &req("start")));
        t.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(prefix(&t, A, &req("u1"), "01a0"));
        assert_eq!(
            t.try_admit_turn(A, &req("fenced"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::SwitchReserved,
            "while live, the reservation fences turns"
        );
        t.backdate_reservation();
        assert_eq!(
            t.try_admit_turn(A, &req("after"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::ConnectionUnsubscribed {
                thread: "01a0".to_string()
            },
            "an expired reservation must WEDGE the connection that unsubscribed, not hand \
             its authorization back"
        );
        // ...and the expired reservation no longer holds the slot against anyone else.
        assert!(
            t.switch_prefix_admissible(B, "01a0").is_ok(),
            "an expired reservation must not hold the slot against another connection"
        );
    }

    // ---------------------------------------------------------------------------
    // Round-2 P4 — a failed PREFIXED switch wedges that connection, not the session.
    // ---------------------------------------------------------------------------

    /// The exact sequence: prefix forwards, the server ERRORS the `thread/start`, the head
    /// restores for the SESSION — and the connection whose subscription was dropped is
    /// refused turns, with its own cause, until a real resume re-subscribes it.
    #[test]
    fn a_failed_prefixed_switch_wedges_that_connection_until_it_resubscribes() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        // The prefix forwards (reserved), then the switch is admitted and FAILS.
        assert!(prefix(&s, A, &req("u1"), "01a0"));
        assert!(open(&s, A, &req("sw")));
        s.observe_server_frame(A, &error_response("sw"));

        // The SESSION keeps its head — it still owns the thread.
        assert_eq!(s.sole_session_thread(), Some("01a0".to_string()));
        // But THIS connection is wedged, and says why.
        assert_eq!(
            s.try_admit_turn(A, &req("t"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::ConnectionUnsubscribed {
                thread: "01a0".to_string()
            }
        );
        // Another connection is unaffected: it never unsubscribed.
        assert_eq!(
            s.try_admit_turn(B, &req("t"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::Admitted
        );
        // **ROUND-3 P6 — only a CORRELATED, ACCEPTED resume lifts the wedge.**
        //
        // A resume of some other thread is not a re-subscription to this one.
        s.note_resubscribe_attempt(A, "01a0-other", &req("r0"));
        s.observe_server_frame(A, &resume_ok("r0"));
        assert!(matches!(
            s.try_admit_turn(A, &req("t2"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::ConnectionUnsubscribed { .. }
        ));
        // An ATTEMPT is not a subscription: the wedge holds until the answer lands...
        s.note_resubscribe_attempt(A, "01a0", &req("r1"));
        assert!(matches!(
            s.try_admit_turn(A, &req("t3"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::ConnectionUnsubscribed { .. }
        ));
        // ...and an ERRORED answer leaves it in place. A client must not be able to lift
        // its own wedge by asking and being told no.
        s.observe_server_frame(A, &error_response("r1"));
        assert!(matches!(
            s.try_admit_turn(A, &req("t4"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::ConnectionUnsubscribed { .. }
        ));
        // Only the accepted one does.
        s.note_resubscribe_attempt(A, "01a0", &req("r2"));
        s.observe_server_frame(A, &resume_ok("r2"));
        assert_eq!(
            s.try_admit_turn(A, &req("t5"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::Admitted
        );
    }

    /// A failed switch WITHOUT a forwarded prefix wedges nothing — the connection never
    /// gave up its subscription.
    #[test]
    fn a_failed_switch_with_no_prefix_does_not_wedge_the_connection() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "01a0"));
        assert!(
            open(&s, A, &req("sw")),
            "a switch with no prefix is still admitted"
        );
        s.observe_server_frame(A, &error_response("sw"));
        assert_eq!(s.sole_session_thread(), Some("01a0".to_string()));
        assert_eq!(
            s.try_admit_turn(A, &req("t"), "01a0", Some(&cwd()), Some(&roots())),
            TurnAdmission::Admitted,
            "nothing was unsubscribed, so nothing wedges"
        );
    }

    // ---------------------------------------------------------------------------
    // Round-1 P9 — bounded, id-only retired storage.
    // ---------------------------------------------------------------------------

    /// An over-long thread id binds NOTHING, so it is never copied into long-lived state.
    #[test]
    fn an_over_long_thread_id_binds_nothing() {
        let s = store();
        assert!(open(&s, A, &req("start")));
        let long = "z".repeat(MAX_THREAD_ID_BYTES + 1);
        s.observe_server_frame(A, &creation_response("start", &long));
        assert_eq!(s.bound_thread(), None, "an over-long id must bind nothing");
        assert!(!s.is_session_thread(&long));
        // The boundary itself is admitted.
        let s2 = store();
        assert!(open(&s2, A, &req("start")));
        let at_cap = "z".repeat(MAX_THREAD_ID_BYTES);
        s2.observe_server_frame(A, &creation_response("start", &at_cap));
        assert_eq!(s2.sole_session_thread(), Some(at_cap));
    }

    /// The retirement cap REFUSES the further switch rather than evicting the oldest — a
    /// resume that was valid a moment ago must not start refusing.
    ///
    /// Driven across FRESH connections, and that detail is the point of the sibling test
    /// below: on ONE connection the per-connection creation-id cap bites first, so the
    /// retirement cap is only reachable by a client that also keeps reconnecting.
    #[test]
    fn the_retirement_cap_refuses_the_switch_rather_than_forgetting_a_thread() {
        let s = store();
        assert!(open(&s, ConnId(0), &req("start")));
        s.observe_server_frame(ConnId(0), &creation_response("start", "t0"));
        for i in 0..MAX_RETIRED_THREADS {
            let c = ConnId(i as u64 + 1);
            let id = format!("s{i}");
            assert!(open(&s, c, &req(&id)), "switch {i} must be admitted");
            s.observe_server_frame(c, &creation_response(&id, &format!("t{}", i + 1)));
        }
        assert_eq!(s.retired_len(), MAX_RETIRED_THREADS);
        assert!(
            !open(&s, ConnId(9999), &req("one-too-many")),
            "past the cap the SWITCH is refused"
        );
        // Nothing was forgotten, and the head still works.
        for i in 0..=MAX_RETIRED_THREADS {
            assert!(s.is_session_thread(&format!("t{i}")), "t{i} was forgotten");
        }
        assert_eq!(
            s.sole_session_thread(),
            Some(format!("t{MAX_RETIRED_THREADS}"))
        );
    }

    /// On ONE connection the per-connection creation-id cap ([`MAX_CONN_REQUEST_IDS`]) is
    /// reached BEFORE the retirement cap, because every settled creation leaves a permanent
    /// tombstone. Pinned so the two bounds cannot silently swap order: whichever bites
    /// first, the outcome is the same fail-closed `CreationSlotClosed`, and no thread is
    /// ever forgotten.
    #[test]
    fn on_one_connection_the_creation_id_cap_bites_before_the_retirement_cap() {
        let s = store();
        let mut switches = 0usize;
        assert!(open(&s, A, &req("start")));
        s.observe_server_frame(A, &creation_response("start", "t0"));
        for i in 0..MAX_RETIRED_THREADS {
            let id = format!("s{i}");
            if !open(&s, A, &req(&id)) {
                break;
            }
            switches += 1;
            s.observe_server_frame(A, &creation_response(&id, &format!("t{}", i + 1)));
        }
        assert!(
            switches < MAX_RETIRED_THREADS,
            "the per-connection id cap must bite first on a single connection, but \
             {switches} switches were admitted"
        );
        assert!(s.retired_len() < MAX_RETIRED_THREADS);
        // Everything bound so far is still readable, and the head is the last one verified.
        for i in 0..=switches {
            assert!(s.is_session_thread(&format!("t{i}")), "t{i} was forgotten");
        }
        assert_eq!(s.sole_session_thread(), Some(format!("t{switches}")));
        // A FRESH connection has a fresh id ledger, so the session can keep switching —
        // which is exactly why the retirement cap exists as a second, session-wide bound.
        assert!(open(&s, B, &req("fresh")));
    }

    #[test]
    fn unparseable_and_duplicate_member_frames_bind_nothing() {
        let s = store();
        assert!(open(&s, A, &req("startup-1")));
        s.observe_server_frame(A, "not json");
        s.observe_server_frame(
            A,
            r#"{"id":"startup-1","result":{"thread":{"id":"01a0"},"cwd":"/a","cwd":"/b","runtimeWorkspaceRoots":["/w"]}}"#,
        );
        assert_eq!(s.bound_thread(), None);
        // The pending survived (nothing was consumed), so the genuine response still binds.
        s.observe_server_frame(A, &creation_response("startup-1", "01a0"));
        assert_eq!(s.sole_session_thread(), Some("01a0".into()));
    }

    // The per-connection CREATION id sets are bounded, so a hostile client cannot grow them.
    #[test]
    fn the_per_connection_id_sets_are_bounded() {
        let s = store();
        for i in 0..MAX_CONN_REQUEST_IDS {
            let id = req(&format!("id-{i}"));
            assert!(open(&s, A, &id), "claim {i}");
            s.observe_server_frame(
                A,
                &json!({"id": id.to_value(), "error": {"code": -1, "message": "x"}}).to_string(),
            );
        }
        assert!(
            !open(&s, A, &req("one-too-many")),
            "past the per-connection id bound a creation refuses rather than growing"
        );
        // A different connection is unaffected (and its own set is separately bounded).
        assert!(open(&s, B, &req("fresh")));
    }

    // -----------------------------------------------------------------
    // ROUND-3 P1 — the TOTAL outstanding-request-id ledger.
    // -----------------------------------------------------------------

    // The headline rule: an id already in flight on a connection cannot be reused, whatever
    // the two methods are. The frame is refused (the relay turns this into a zero-byte drop)
    // and the event is COUNTED for the failure-containment seam.
    #[test]
    fn an_id_reused_while_in_flight_is_refused_and_counted() {
        let s = store();
        assert_eq!(admit(&s, A, &req("dup"), "app/list"), IdAdmission::Admitted);
        assert_eq!(s.id_ledger_counts(), IdLedgerCounts::default());

        // …the same id again, on the same connection, for any method.
        for method in ["app/list", "model/list", CREATION_METHOD] {
            assert_eq!(
                admit(&s, A, &req("dup"), method),
                IdAdmission::ReusedInFlight,
                "{method} reusing an in-flight id"
            );
        }
        assert_eq!(
            s.id_ledger_counts(),
            IdLedgerCounts {
                reused_in_flight: 3,
                ..Default::default()
            }
        );
        // A DIFFERENT connection is unaffected: ids are per-connection, and every real
        // client resets its counter to 1 on reconnect.
        assert_eq!(admit(&s, B, &req("dup"), "app/list"), IdAdmission::Admitted);
        // And nothing was bound or claimed along the way.
        assert_eq!(s.bound_thread(), None);
        assert!(s.creation_closed_reason().is_none());
    }

    // A response RELEASES its entry, after which the id may be used again — including by a
    // different method. This is the property that keeps a real client (whose counters reset
    // per connection and whose retries mint fresh ids) working.
    #[test]
    fn a_response_releases_its_id_and_the_id_may_then_be_reused() {
        let s = store();
        assert_eq!(admit(&s, A, &req("7"), "app/list"), IdAdmission::Admitted);
        assert_eq!(
            admit(&s, A, &req("7"), "app/list"),
            IdAdmission::ReusedInFlight
        );
        // The server answers it — a plain result.
        s.observe_server_frame(A, r#"{"id":"7","result":{"data":[]}}"#);
        assert_eq!(
            admit(&s, A, &req("7"), "model/list"),
            IdAdmission::Admitted,
            "an answered id is free again"
        );
        // An ERROR answer releases too — the ccd link's failed `thread/resume` is answered
        // exactly that way, and stranding its id would wedge the link.
        s.observe_server_frame(A, &error_response("7"));
        assert_eq!(
            admit(&s, A, &req("7"), "thread/read"),
            IdAdmission::Admitted
        );
        // A response on the WRONG connection releases nothing.
        s.observe_server_frame(B, r#"{"id":"7","result":{}}"#);
        assert_eq!(
            admit(&s, A, &req("7"), "thread/read"),
            IdAdmission::ReusedInFlight
        );
    }

    // A method-BEARING s2c frame is not a response and releases nothing — otherwise a
    // `thread/started` notification that happened to carry an id could free an id mid-flight.
    #[test]
    fn a_method_bearing_frame_releases_nothing() {
        let s = store();
        assert_eq!(admit(&s, A, &req("3"), "app/list"), IdAdmission::Admitted);
        s.observe_server_frame(
            A,
            r#"{"id":"3","method":"item/requestApproval","params":{}}"#,
        );
        s.observe_server_frame(A, r#"{"method":"thread/started","params":{}}"#);
        assert_eq!(
            admit(&s, A, &req("3"), "app/list"),
            IdAdmission::ReusedInFlight,
            "a server→client request must not release a client request's id"
        );
    }

    // ROUND-3 P1, the cross-method collision this round closes. A NON-creation request
    // occupies the id first; the `thread/start` that wanted it is refused, so NO creation is
    // pending — and the response to that non-creation request, even shaped exactly like a
    // creation answer, installs NOTHING.
    #[test]
    fn a_non_creation_request_holding_an_id_blocks_the_creation_and_binds_nothing() {
        let s = store();
        assert_eq!(
            admit(&s, A, &req("startup-1"), "app/list"),
            IdAdmission::Admitted
        );
        assert!(
            !open(&s, A, &req("startup-1")),
            "the creation cannot take an id that is already in flight"
        );
        // The server answers `app/list` with a perfectly-shaped creation RESULT.
        s.observe_server_frame(A, &creation_response("startup-1", "smuggled"));
        assert_eq!(
            s.bound_thread(),
            None,
            "a response to a non-creation request must never install a binding"
        );
        assert!(!s.is_session_thread("smuggled"));
        // …and the creation slot is untouched: still Open, never Closed.
        assert!(s.creation_closed_reason().is_none());
        assert!(open(&s, A, &req("startup-2")));
    }

    // The same shape on the ERROR side: a response to a non-creation request must not
    // re-open the creation slot.
    #[test]
    fn a_non_creation_requests_error_does_not_reopen_creation() {
        let s = store();
        // One creation is pending under its own id …
        assert!(open(&s, A, &req("creation")));
        // … and an unrelated request is in flight under a different id.
        assert_eq!(
            admit(&s, A, &req("unrelated"), "thread/read"),
            IdAdmission::Admitted
        );
        s.observe_server_frame(A, &error_response("unrelated"));
        assert!(
            !open(&s, A, &req("creation-2")),
            "an unrelated error must not reopen the creation slot"
        );
        // The pending is intact and still bindable.
        s.observe_server_frame(A, &creation_response("creation", "01a0"));
        assert_eq!(s.sole_session_thread(), Some("01a0".into()));
    }

    // ROUND-3 P6 — a client-chosen id longer than the cap is refused and NEVER stored.
    #[test]
    fn an_over_long_request_id_is_refused_and_never_stored() {
        let s = store();
        let long = req(&"x".repeat(MAX_REQUEST_ID_BYTES + 1));
        for method in ["app/list", CREATION_METHOD] {
            assert_eq!(
                admit(&s, A, &long, method),
                IdAdmission::Oversized,
                "{method} with an over-long id"
            );
        }
        assert_eq!(
            s.id_ledger_counts(),
            IdLedgerCounts {
                oversized: 2,
                ..Default::default()
            }
        );
        // NEVER STORED: the id is not outstanding, not reserved and not tombstoned — proven
        // by the store having no per-connection ledger for A at all, and by the creation slot
        // still being open.
        assert!(!s.has_tracked_connection(A), "no ledger was allocated");
        assert!(s.creation_closed_reason().is_none());
        assert!(open(&s, A, &req("startup-1")), "the slot was never claimed");
        // Exactly at the cap is fine — the cap is >2x the 59-byte measured maximum.
        let s2 = store();
        let at_cap = req(&"x".repeat(MAX_REQUEST_ID_BYTES));
        assert_eq!(
            admit(&s2, A, &at_cap, "app/list"),
            IdAdmission::Admitted,
            "an id exactly at the cap is stored"
        );
        // An integer id has no client-chosen length.
        assert_eq!(
            admit(&s2, A, &RequestId::Int(i64::MIN), "app/list"),
            IdAdmission::Admitted
        );
    }

    // ROUND-3 P1 — the outstanding ledger is bounded per connection.
    #[test]
    fn the_outstanding_ledger_is_bounded_per_connection() {
        let s = store();
        for i in 0..MAX_OUTSTANDING_REQUESTS {
            assert_eq!(
                admit(&s, A, &req(&format!("id-{i}")), "app/list"),
                IdAdmission::Admitted,
                "outstanding {i}"
            );
        }
        assert_eq!(
            admit(&s, A, &req("one-too-many"), "app/list"),
            IdAdmission::AtCapacity
        );
        assert_eq!(s.id_ledger_counts().at_capacity, 1);
        // Answering one frees a slot.
        s.observe_server_frame(A, r#"{"id":"id-0","result":{}}"#);
        assert_eq!(
            admit(&s, A, &req("one-too-many"), "app/list"),
            IdAdmission::Admitted
        );
        // A different connection is unaffected.
        assert_eq!(
            admit(&s, B, &req("fresh"), "app/list"),
            IdAdmission::Admitted
        );
    }

    // M10 — a DIRECT capacity test for MAX_TRACKED_CONNECTIONS: fill it with live
    // connections, prove the next one is refused, close one, prove the capacity is released.
    // (Round 2 argued this bound in a comment; round 3 proves it.)
    #[test]
    fn the_tracked_connection_table_is_bounded_and_released_on_close() {
        let s = store();
        // 1024 LIVE connections, each holding one outstanding request id.
        for i in 0..MAX_TRACKED_CONNECTIONS {
            assert_eq!(
                admit(&s, ConnId(i as u64), &req("1"), "app/list"),
                IdAdmission::Admitted,
                "connection {i}"
            );
        }
        let overflow = ConnId(MAX_TRACKED_CONNECTIONS as u64);
        // The 1025th connection is refused and counted…
        assert_eq!(
            admit(&s, overflow, &req("1"), "app/list"),
            IdAdmission::AtCapacity
        );
        // …and its creation would be refused too, so a full table cannot bind a thread.
        assert!(!open(&s, overflow, &req("startup-1")));
        assert_eq!(s.id_ledger_counts().at_capacity, 2);
        assert!(
            !s.has_tracked_connection(overflow),
            "a refused connection must not have allocated a ledger"
        );

        // Close ONE connection: its ledger is dropped, so capacity is released.
        s.close_connection(ConnId(0));
        assert!(!s.has_tracked_connection(ConnId(0)));
        assert_eq!(
            admit(&s, overflow, &req("1"), "app/list"),
            IdAdmission::Admitted,
            "closing a connection must release its slot in the table"
        );
        assert!(s.has_tracked_connection(overflow));
        // The table is full again, so the NEXT new connection is refused once more.
        assert_eq!(
            admit(&s, ConnId(9999), &req("1"), "app/list"),
            IdAdmission::AtCapacity
        );
        // An ALREADY-tracked connection is never refused by the table bound.
        assert_eq!(
            admit(&s, ConnId(1), &req("2"), "app/list"),
            IdAdmission::Admitted
        );
    }

    // -----------------------------------------------------------------
    // ROUND-4 P1 — DRAIN VALIDATION. An outstanding id is released only by a frame PROVEN
    // to be a response.
    // -----------------------------------------------------------------

    // The hole itself: a bare method-less `{"id":X}` proves nothing, so it must not drain X.
    #[test]
    fn a_bare_methodless_id_frame_does_not_drain_an_outstanding_id() {
        let s = store();
        let x = req("X");
        // An ORDINARY request is forwarded under id X.
        assert_eq!(admit(&s, A, &x, "app/list"), IdAdmission::Admitted);
        // A bare method-less `{"id":"X"}` arrives: no `result`, no `error`.
        s.observe_server_frame(A, r#"{"id":"X"}"#);
        // X is STILL outstanding — for any method …
        assert_eq!(admit(&s, A, &x, "model/list"), IdAdmission::ReusedInFlight);
        // … including the creation, so the id cannot be laddered into a pending creation.
        assert_eq!(
            admit(&s, A, &x, CREATION_METHOD),
            IdAdmission::ReusedInFlight,
            "a thread/start may not reuse an id that was never legitimately answered"
        );
        assert_eq!(s.bound_thread(), None);
        assert!(s.creation_closed_reason().is_none());
    }

    // CODEX'S EXACT EXPLOIT TRACE, end to end. Before the drain validation this produced a
    // SECOND admitted creation while the first `thread/start` was still in flight upstream
    // and may already have created a thread — i.e. it broke the single-thread invariant.
    //
    // 1. connection A forwards an ORDINARY request under id X ⇒ X is outstanding;
    // 2. a bare method-less `{"id":"X"}` arrives ⇒ pre-fix it DRAINED X;
    // 3. a `thread/start` reusing X is admitted (the id looks free) ⇒ pending is (A, X);
    // 4. the ORIGINAL request's DELAYED but perfectly valid error for X lands ⇒ it correlates
    //    to the pending and is misclassified as the CREATION's failure ⇒ REOPEN;
    // 5. a second creation is admitted. Two creations, one session.
    //
    // MUTATION-VERIFIED: reverting the `header.response.is_response()` gate in
    // `observe_server_frame` makes this test fail with `creations admitted: 2`.
    #[test]
    fn the_bare_id_drain_exploit_cannot_produce_a_second_creation() {
        let s = store();
        let x = req("X");
        let mut creations_admitted = 0;

        // 1.
        assert_eq!(admit(&s, A, &x, "app/list"), IdAdmission::Admitted);
        // 2.
        s.observe_server_frame(A, r#"{"id":"X"}"#);
        // 3.
        if s.try_admit_request(A, &x, CREATION_METHOD) == IdAdmission::Admitted {
            creations_admitted += 1;
        }
        // 4.
        s.observe_server_frame(A, &error_response("X"));
        // 5.
        if open(&s, A, &req("startup-2")) {
            creations_admitted += 1;
        }

        assert_eq!(
            creations_admitted, 1,
            "a bare method-less {{\"id\":X}} must not make a SECOND creation admissible"
        );
        // Nothing was bound along the way, and the slot was never wedged.
        assert_eq!(s.bound_thread(), None);
    }

    // The POSITIVE counterpart, so the fix cannot be "never drain": a WELL-FORMED answer
    // does release its id, after which a `thread/start` reusing that id is legitimately
    // admitted and binds normally.
    #[test]
    fn a_well_formed_response_still_drains_its_id_and_the_id_is_then_claimable() {
        // The ERROR side.
        let s = store();
        let x = req("X");
        assert_eq!(admit(&s, A, &x, "app/list"), IdAdmission::Admitted);
        s.observe_server_frame(A, &error_response("X"));
        assert!(
            open(&s, A, &x),
            "a well-formed error DOES drain, so a creation may claim the id"
        );
        s.observe_server_frame(A, &creation_response("X", "01a0"));
        assert_eq!(s.sole_session_thread(), Some("01a0".into()));

        // The RESULT side.
        let s = store();
        let y = req("Y");
        assert_eq!(admit(&s, A, &y, "app/list"), IdAdmission::Admitted);
        s.observe_server_frame(A, r#"{"id":"Y","result":{"apps":[]}}"#);
        assert!(
            open(&s, A, &y),
            "a plain result DOES drain, so a creation may claim the id"
        );
        s.observe_server_frame(A, &creation_response("Y", "01a1"));
        assert_eq!(s.sole_session_thread(), Some("01a1".into()));
    }

    // The full shape table on the DRAIN side: none of these release an outstanding id.
    #[test]
    fn only_a_provable_response_drains_an_outstanding_id() {
        for frame in [
            // The exploit frame, and its "looks like a header" cousins.
            r#"{"id":"X"}"#,
            r#"{"id":"X","jsonrpc":"2.0"}"#,
            // BOTH members.
            r#"{"id":"X","result":{"ok":true},"error":{"code":-1,"message":"m"}}"#,
            r#"{"id":"X","result":null,"error":null}"#,
            // An `error` that is not a JSON-RPC error object.
            r#"{"id":"X","error":{}}"#,
            r#"{"id":"X","error":null}"#,
            r#"{"id":"X","error":"boom"}"#,
            r#"{"id":"X","error":{"code":-1}}"#,
            r#"{"id":"X","error":{"message":"m"}}"#,
            // Non-INTEGER code / non-STRING message.
            r#"{"id":"X","error":{"code":"str","message":"m"}}"#,
            r#"{"id":"X","error":{"code":-1.5,"message":"m"}}"#,
            r#"{"id":"X","error":{"code":-1,"message":5}}"#,
            // AMBIGUOUS `code` — our read and the app-server's disagree, so nothing moves.
            r#"{"id":"X","error":{"code":"str","code":-1,"message":"m"}}"#,
            // A repeated TOP-LEVEL member: the header itself is untrustworthy.
            r#"{"id":"X","id":"X","result":{}}"#,
            r#"{"id":"X","result":{},"result":{}}"#,
        ] {
            let s = store();
            assert_eq!(admit(&s, A, &req("X"), "app/list"), IdAdmission::Admitted);
            s.observe_server_frame(A, frame);
            assert_eq!(
                admit(&s, A, &req("X"), "app/list"),
                IdAdmission::ReusedInFlight,
                "must NOT have drained: {frame}"
            );
            // …and it neither bound nor disturbed the creation slot.
            assert_eq!(s.bound_thread(), None, "{frame}");
            assert!(s.creation_closed_reason().is_none(), "{frame}");
        }
    }

    // ONE definition of "a valid response", used by two rules: the ledger's DRAIN gate
    // (decided on raw bytes by the header scan) and the P2 creation-response state machine
    // (decided on a parsed `Value`). If either side is changed alone, this fails.
    #[test]
    fn response_kind_agrees_with_the_creation_state_machine() {
        use crate::message::{scan_frame_header, ResponseKind};
        for frame in [
            r#"{"id":1,"result":{"a":1}}"#,
            r#"{"id":1,"result":null}"#,
            r#"{"id":1,"error":{"code":-1,"message":"m"}}"#,
            r#"{"id":1,"error":{"code":0,"message":""}}"#,
            r#"{"id":1,"error":{"code":4294967296,"message":"big"}}"#,
            r#"{"id":1,"error":{"code":-32601,"message":"m","data":{"any":true}}}"#,
            r#"{"id":1}"#,
            r#"{"id":1,"result":{"a":1},"error":{"code":-1,"message":"m"}}"#,
            r#"{"id":1,"error":{}}"#,
            r#"{"id":1,"error":null}"#,
            r#"{"id":1,"error":"boom"}"#,
            r#"{"id":1,"error":[1]}"#,
            r#"{"id":1,"error":{"code":-1}}"#,
            r#"{"id":1,"error":{"message":"m"}}"#,
            r#"{"id":1,"error":{"code":"-1","message":"m"}}"#,
            r#"{"id":1,"error":{"code":-1.5,"message":"m"}}"#,
            r#"{"id":1,"error":{"code":null,"message":"m"}}"#,
            r#"{"id":1,"error":{"code":-1,"message":5}}"#,
            r#"{"id":1,"error":{"code":-1,"message":null}}"#,
            r#"{"id":1,"error":{"code":-1,"message":{"t":"m"}}}"#,
        ] {
            let scanned = scan_frame_header(frame).expect("scans").response;
            let v = crate::message::parse_no_dup_value(frame).expect("parses");
            // The state machine's two arms, read straight off `classify_creation_response`.
            let sm_reopen_arm =
                v.get("result").is_none() && v.get("error").is_some_and(is_jsonrpc_error_object);
            let sm_install_arm = v.get("error").is_none() && v.get("result").is_some();
            assert_eq!(
                scanned == ResponseKind::Error,
                sm_reopen_arm,
                "error arm disagrees: {frame}"
            );
            assert_eq!(
                scanned == ResponseKind::Result,
                sm_install_arm,
                "result arm disagrees: {frame}"
            );
            assert_eq!(
                scanned == ResponseKind::NotAResponse,
                !sm_reopen_arm && !sm_install_arm,
                "indeterminate arm disagrees: {frame}"
            );
        }
    }

    #[test]
    fn no_threads_binds_nothing_and_opens_nothing() {
        assert_eq!(NoThreads.bound_thread(), None);
        assert_eq!(NoThreads.sole_session_thread(), None);
        assert!(!NoThreads.is_session_thread("anything"));
        assert_eq!(
            NoThreads.try_admit_request(A, &req("x"), CREATION_METHOD),
            IdAdmission::CreationSlotClosed
        );
        assert_eq!(
            NoThreads.try_admit_request(A, &req("x"), "app/list"),
            IdAdmission::Admitted
        );
        assert_eq!(NoThreads.creation_closed_reason(), None);
        assert_eq!(NoThreads.id_ledger_counts(), IdLedgerCounts::default());
        NoThreads.rollback_creation(A, &req("x"));
        NoThreads.close_connection(A);
    }
}
