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
//! ## Single-thread session invariant (P3)
//!
//! `try_admit_request` refuses a `thread/start` unless the creation state is
//! [`Creation::Open`] — i.e. it refuses once ONE thread is bound, one creation is already
//! pending, or the state is `Closed`.
//! Including *pending* is what kills the pipeline race: two `thread/start`s in flight
//! before either response lands would otherwise both be admitted and the second response
//! would silently re-point the session's head. Claim-and-record is a single atomic step
//! under one mutex, so there is no check-then-act window either.
//!
//! This is the **pre-D2 form**. D2 (the thread-switch latch, acknowledged quiesce,
//! acceptance fence and upstream seal) is what re-opens multi-thread sessions; until it
//! lands the TUI's `/new-thread` flows are refused rather than silently racing.
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

    /// Does a thread id belong to this session?
    fn is_session_thread(&self, thread_id: &str) -> bool {
        self.bound_thread().is_some_and(|t| t.id == thread_id)
    }

    /// The session's ONE bound thread id, iff exactly one is bound.
    ///
    /// `None` when none is bound — fail closed. The old "two or more bound is a thread
    /// switch" arm is now **unrepresentable** rather than merely checked: the store holds
    /// one [`Creation`] state, and P3 closes the creation slot the moment one thread binds,
    /// so a second binding can never be installed. Kept as the head accessor the pre-D2
    /// turn head-check reads.
    fn sole_session_thread(&self) -> Option<String> {
        self.bound_thread().map(|t| t.id)
    }
}

/// The session's single creation slot, as an explicit state machine (round-2 P2).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Creation {
    /// No creation is admitted; the next fingerprint-clean `thread/start` may claim it.
    Open,
    /// One creation was admitted on `conn` with `id` and its response has not landed.
    Pending { conn: ConnId, id: RequestId },
    /// A creation response was correlated and fully verified: the session's one thread.
    Bound(VerifiedThread),
    /// A pending creation was consumed by a response that proved NEITHER success nor
    /// failure, or its owning connection vanished while it was in flight. Neither installs
    /// nor reopens: the server may hold a thread we cannot name, so a second creation could
    /// break the single-thread invariant. Terminal until reconnect evidence (D2 owns the
    /// reconciliation); pre-D2 the session is wedged-safe.
    Closed(&'static str),
}

/// One connection's request-id ledger. All three sets are bounded — see
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
        // forwarded request. `thread/started`/`thread/resumed` land here and are ignored.
        if header.has_method {
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
            Creation::Pending { conn: pc, id: pid } if *pc == conn && *pid == id
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
                if let Some(slots) = g.conns.get_mut(&conn) {
                    slots.outstanding.remove(&id);
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

        g.creation = classify_creation_response(&v, &self.launch_cwd);
    }
}

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
    let thread_id = result
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(|i| i.as_str())
        .filter(|s| !s.is_empty())?;

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

        // P6: an over-long client-chosen id is NEVER stored, whatever the method.
        if !id_within_cap(id) {
            g.counts.oversized += 1;
            return IdAdmission::Oversized;
        }
        // Round-2 P3: one thread per session, and one creation in flight at a time. Checked
        // and recorded under the SAME lock, so two pipelined creations cannot both claim.
        if is_creation && g.creation != Creation::Open {
            return IdAdmission::CreationSlotClosed;
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
        if is_creation {
            slots.reserved.insert(id.clone());
            g.creation = Creation::Pending {
                conn,
                id: id.clone(),
            };
        }
        IdAdmission::Admitted
    }

    fn id_ledger_counts(&self) -> IdLedgerCounts {
        self.inner.lock().unwrap().counts
    }

    fn rollback_creation(&self, conn: ConnId, id: &RequestId) {
        let mut g = self.inner.lock().unwrap();
        let is_ours = matches!(
            &g.creation,
            Creation::Pending { conn: c, id: i } if *c == conn && i == id
        );
        if !is_ours {
            return;
        }
        // Zero bytes went upstream, so the id is released — from the outstanding ledger as
        // well as the reservation — rather than tombstoned, and the client may retry,
        // including with this very id.
        if let Some(slots) = g.conns.get_mut(&conn) {
            slots.reserved.remove(id);
            slots.outstanding.remove(id);
        }
        g.creation = Creation::Open;
    }

    fn close_connection(&self, conn: ConnId) {
        let mut g = self.inner.lock().unwrap();
        if matches!(&g.creation, Creation::Pending { conn: c, .. } if *c == conn) {
            g.creation = Creation::Closed(
                "the connection that owned the pending creation disconnected before its \
                 response landed; the request DID reach the server, so creation is closed \
                 rather than reopened (D2 owns the reconciliation)",
            );
        }
        g.conns.remove(&conn);
    }

    fn creation_closed_reason(&self) -> Option<&'static str> {
        match &self.inner.lock().unwrap().creation {
            Creation::Closed(why) => Some(why),
            _ => None,
        }
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

    // P3 — the single-thread session invariant, including the pipeline race.
    #[test]
    fn creation_is_closed_while_pending_and_after_binding() {
        let s = store();
        assert!(open(&s, A, &req("a")));
        assert!(!open(&s, A, &req("b")), "pending closes");
        assert!(!open(&s, B, &req("b")), "across connections too");
        s.observe_server_frame(A, &creation_response("a", "01a0"));
        assert!(!open(&s, A, &req("c")), "bound closes");
        // And no later response can re-point the head.
        assert_eq!(s.sole_session_thread(), Some("01a0".to_string()));
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
