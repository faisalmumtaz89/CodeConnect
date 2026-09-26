//! Session-scoped thread state: **the head follows the keyboard.**
//!
//! The keyboard leg is a passthrough. Every frame the TUI sends reaches the app-server as it
//! was written, and nothing here refuses one. What this module keeps is what the PHONE leg is
//! judged against, learned by watching the keyboard's requests and the server's answers:
//!
//! * **the head** — the one thread a phone's turn, steer, interrupt or approval answer may
//!   name. It is the thread the keyboard is on;
//! * **the threads this session has left** — readable from the phone, never actuable;
//! * **which turns are running**, so a phone never starts a turn on a busy thread;
//! * **the phone leg's request-id ledger**, so its responses correlate.
//!
//! ## How the head moves
//!
//! A keyboard `thread/start`, `thread/resume` or `thread/fork` — except one that creates
//! an `ephemeral` thread, which does not become the session's thread — is recorded by
//! [`SessionThreads::observe_tui_request`] before its bytes go upstream, and the head is
//! **moving** from that moment: the phone may act on no thread until the move settles. The
//! request is keyed by `(ConnId, RequestId)`, so only the answer on the connection that asked
//! can settle it ([`SessionThreads::observe_server_frame`]):
//!
//! * a success carrying a string `result.thread.id` makes that thread the head, and the
//!   thread the keyboard moved from is retired;
//! * a JSON-RPC error puts back the head the move started from;
//! * any other answer proves neither, and leaves no head until the keyboard binds one again;
//! * a keyboard connection that closes with its move in flight leaves no head either, and a
//!   move whose bytes provably never left ([`SessionThreads::rollback_move`]) puts the old
//!   head back.
//!
//! A second move issued while one is in flight replaces it and can put nothing back: the
//! first may still land, so a failure of the second leaves no head rather than a head the
//! keyboard may already have left.
//!
//! A keyboard `thread/unsubscribe` naming the head marks it **left**: the keyboard is no
//! longer receiving its stream, so the phone may not act on it until the next move lands.
//!
//! Announcements (`thread/started`, `thread/resumed`) move nothing: only an answer to a
//! request this broker watched go out can.
//!
//! ## What is running
//!
//! [`TurnActivity`] records a turn as running from three sources: a keyboard `turn/start` in
//! flight, a phone `turn/start` this broker admitted, and the server's own `turn/started`.
//! The server's announcements are recorded for every thread, so a turn that started while
//! the head was moving is still known if the move fails and the old head comes back.
//! `turn/completed` ends a turn, and every readable one is remembered, so a lagging leg's
//! announcement of a turn that has already ended marks nothing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::message::{RequestId, ResponseKind, Shape};

/// The keyboard requests that move the head.
const MOVE_METHODS: [&str; 3] = ["thread/start", "thread/resume", "thread/fork"];

/// The method whose admission marks a thread busy.
pub const TURN_METHOD: &str = "turn/start";

/// The keyboard request that takes the keyboard off a thread.
const UNSUBSCRIBE_METHOD: &str = "thread/unsubscribe";

/// A monotonic per-**connection** instance id, minted by the relay for each accepted
/// connection (see [`crate::relay`]). A response is correlated to its request by
/// `(ConnId, RequestId)`, so two connections of the same role — the TUI `/resume` picker
/// opens a second TUI connection — never answer each other's requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnId(pub u64);

impl std::fmt::Display for ConnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Cap on the number of phone connections whose id ledgers are tracked at once.
///
/// Entries are removed on disconnect ([`SessionThreads::close_connection`]), so this bounds
/// *concurrently live* connections. Past it a request on an untracked connection is refused
/// rather than allocating.
pub const MAX_TRACKED_CONNECTIONS: usize = 1024;

/// Per-connection cap on the OUTSTANDING (forwarded, not yet answered) phone request ids.
///
/// The ccd control link issues one request at a time, so 256 unanswered requests is not a
/// shape the wire has ever shown. Past the bound the request is dropped and counted
/// ([`IdLedgerCounts::at_capacity`]) rather than forwarded.
pub const MAX_OUTSTANDING_REQUESTS: usize = 256;

/// The strict per-id byte cap for any client-chosen request id this broker STORES.
///
/// MEASURED on the real wire: request ids run 1–59 bytes. 128 bytes cannot refuse a real
/// client, and makes the ledger's memory a product of finite factors. It applies only to
/// the STRING form: [`RequestId::Int`] is fixed-width.
pub const MAX_REQUEST_ID_BYTES: usize = 128;

/// Is this id short enough to store? See [`MAX_REQUEST_ID_BYTES`].
fn id_within_cap(id: &RequestId) -> bool {
    match id {
        RequestId::Str(s) => s.len() <= MAX_REQUEST_ID_BYTES,
        RequestId::Int(_) => true,
    }
}

/// What the phone leg's id ledger decided about a request the classifier is about to
/// forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdAdmission {
    /// The id was recorded as outstanding on this connection; the frame may be forwarded.
    Admitted,
    /// The id is ALREADY outstanding on this connection, which makes the client's own
    /// responses uncorrelatable. The frame is dropped (zero upstream bytes), the leg stays
    /// open, and the event is counted.
    ReusedInFlight,
    /// The id is longer than [`MAX_REQUEST_ID_BYTES`] and is therefore never stored.
    /// Dropped and counted.
    Oversized,
    /// This connection's outstanding ledger or the tracked-connection table is full.
    /// Dropped and counted.
    AtCapacity,
}

/// Counts of the protocol-hostile id events the ledger refuses, for the audit log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdLedgerCounts {
    /// Frames dropped because their id was already outstanding on the connection.
    pub reused_in_flight: u64,
    /// Frames dropped because their id exceeded [`MAX_REQUEST_ID_BYTES`].
    pub oversized: u64,
    /// Frames dropped because a ledger bound was reached.
    pub at_capacity: u64,
}

/// What the phone leg's classifier asks of the session.
pub trait ThreadBinding: Send + Sync {
    /// The head: the thread the keyboard is on, while the phone may act on it. `None`
    /// while there is none, while the keyboard is moving, and once it has left the head.
    fn bound_thread(&self) -> Option<String>;

    /// Record ONE phone request the classifier has decided to forward: its id becomes
    /// **outstanding** on `conn` together with its method, and its response releases it.
    ///
    /// [`IdAdmission::Admitted`] ⇒ forward. Every other verdict means zero upstream bytes;
    /// the frame is dropped, counted, and the leg kept open.
    ///
    /// Client→server **responses** (approval answers) are not tracked here: their ids are
    /// server-chosen and are [`crate::response_capability`]'s business.
    fn try_admit_request(&self, conn: ConnId, id: &RequestId, method: &str) -> IdAdmission;

    /// **The phone's `turn/start`: head-check, workspace, idle rule, id ledger and busy-mark
    /// as ONE atomic decision.**
    ///
    /// The frame names no workspace — `cwd` is null and `runtimeWorkspaceRoots` absent — so
    /// the turn runs in the thread's own. The roots could not be sent anyway: codex 0.153.4
    /// refuses `turn/start.runtimeWorkspaceRoots` from a client that did not declare
    /// `experimentalApi`, and this leg declares nothing but `clientInfo`.
    ///
    /// The default refuses everything, so a binding that does not implement it authorizes
    /// no turns.
    fn try_admit_idle_turn(
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

    /// Is `turn` an **active** turn of `thread` — one the server announced with
    /// `turn/started`, or one whose `turn/start` the server answered with that id — whose
    /// terminal has not arrived?
    ///
    /// The predicate a phone's interrupt and steer are bound by. An entry still unanswered
    /// has no id yet, and one whose terminal has been seen is gone, so a turn that has
    /// already ended cannot be named and a phantom id names nothing.
    fn is_active_turn(&self, _thread: &str, _turn: &str) -> bool {
        false
    }

    /// The running counts of protocol-hostile id events.
    fn id_ledger_counts(&self) -> IdLedgerCounts {
        IdLedgerCounts::default()
    }

    /// Does a thread id belong to this session — the head (left or not), the head a move
    /// started from, or a thread this session retired? The phone may READ these.
    fn is_session_thread(&self, thread_id: &str) -> bool {
        self.bound_thread().is_some_and(|t| t == thread_id)
    }
}

/// How many turn terminals are remembered for duplicate and late-announcement suppression.
///
/// One per turn, and only the recent ones matter: duplicates and lagging announcements
/// arrive inside one delivery fan-out, never tens of turns later.
pub const MAX_CLEARED_TURNS: usize = 64;

/// **Cap on concurrently admitted-but-unterminated phone `turn/start` requests.**
///
/// The idle rule already holds the phone to one, so this is the bound on the set rather
/// than a limit a client meets; past it a phone `turn/start` is refused with its own cause.
pub const MAX_ACTIVE_TURNS: usize = 8;

/// How many threads' running turns are remembered at once.
///
/// One entry per thread with a turn running — the head, and the threads a multi-agent
/// session runs beside it. Past the cap every entry but the head's is forgotten.
pub const MAX_RUNNING_THREADS: usize = 64;

/// The strict byte cap on a thread or turn id this broker STORES.
///
/// MEASURED: every thread id on the wire is a 36-byte lowercase UUID. A longer id binds
/// nothing and marks nothing, and is never copied into long-lived state.
pub const MAX_THREAD_ID_BYTES: usize = 64;

/// Cap on the number of RETIRED threads one session remembers.
///
/// A retired thread is one the keyboard moved away from. It stays readable from the phone:
/// the app-server still answers a `thread/resume` for it with its full history. Past the
/// cap a further retired thread is not remembered, so it stops being readable from the
/// phone — the fail-closed direction.
pub const MAX_RETIRED_THREADS: usize = 64;

/// A thread the keyboard is on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settled {
    thread: String,
    /// The keyboard unsubscribed from it and has not moved since.
    left: bool,
}

/// Where the head is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Head {
    /// No thread is the head: none has bound yet, or the last move ended in a way that
    /// proved nothing.
    Unbound,
    /// The keyboard is on this thread.
    On(Settled),
    /// A keyboard move is in flight under `(conn, id)`. `from` is the head it started from,
    /// put back if the move provably fails.
    Moving {
        conn: ConnId,
        id: RequestId,
        from: Option<Settled>,
    },
}

/// What a correlated answer to a keyboard move proved.
enum MoveOutcome {
    /// The server answered with this thread.
    Landed(String),
    /// A JSON-RPC error: the move did not happen.
    Refused,
    /// Neither.
    Unknown,
}

/// **Which turns are running, from every source this broker can see.**
#[derive(Debug, Default)]
struct TurnActivity {
    /// `turn/start` requests in flight or answered and not yet terminated — the keyboard's
    /// as observed, the phone's as admitted — keyed by the `(connection, request id)` that
    /// sent them, because at that time the turn does not exist yet.
    admitted: HashMap<(ConnId, RequestId), TurnEntry>,
    /// The turn the server announced running on each thread, until its own terminal.
    running: HashMap<Box<str>, Box<str>>,
    /// Terminals already seen.
    ///
    /// A terminal is delivered to every subscribed connection and this broker observes
    /// every leg, so it sees the SAME terminal more than once. Only the first may clear
    /// anything; a duplicate arriving after a fresh `turn/start` was recorded would
    /// otherwise clear that new turn's mark.
    cleared: TerminalEpochs,
}

/// The bounded memory of terminals already seen, keyed by `(thread, turn)` because the
/// app-server mints turn ids per thread. Eviction is oldest-first.
#[derive(Debug, Default)]
struct TerminalEpochs {
    seen: HashSet<(String, String)>,
    order: std::collections::VecDeque<(String, String)>,
}

impl TerminalEpochs {
    fn contains(&self, thread: &str, turn: &str) -> bool {
        self.seen.contains(&(thread.to_string(), turn.to_string()))
    }

    /// Record this terminal, returning TRUE iff it had not been seen.
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

/// One recorded `turn/start`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnEntry {
    /// The thread it names.
    thread: Box<str>,
    state: TurnState,
}

/// Where one recorded `turn/start` stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TurnState {
    /// Its response has NOT been seen. The server has not said whether it became a turn,
    /// so no terminal speaks for it; only its own error response or its connection closing
    /// ends it.
    Unanswered,
    /// Its response carried a result. `turn` is `result.turn.id` when readable. The next
    /// terminal on its thread ends it, whatever turn that terminal names.
    Answered { turn: Option<String> },
}

impl TurnActivity {
    /// Is a turn running on `thread`, or a `turn/start` naming it in flight?
    fn busy_on(&self, thread: &str) -> bool {
        self.running.contains_key(thread) || self.admitted.values().any(|e| &*e.thread == thread)
    }

    fn is_active(&self, thread: &str, turn: &str) -> bool {
        self.running.get(thread).is_some_and(|t| &**t == turn)
            || self.admitted.values().any(|e| {
                &*e.thread == thread
                    && matches!(&e.state, TurnState::Answered { turn: Some(id) } if id == turn)
            })
    }

    /// The server announced `turn` running on `thread`. Ignored for a turn whose terminal
    /// has already been seen: a lagging leg's announcement must not mark an ended turn.
    fn started(&mut self, thread: &str, turn: &str, head: Option<&str>) {
        if self.cleared.contains(thread, turn) {
            return;
        }
        if self.running.len() >= MAX_RUNNING_THREADS && !self.running.contains_key(thread) {
            self.running.retain(|t, _| Some(&**t) == head);
        }
        self.running.insert(thread.into(), turn.into());
    }

    fn len(&self) -> usize {
        self.admitted.len()
    }

    /// Does this `(conn, id)` hold a live entry? A second `turn/start` under it would
    /// overwrite the only mark the first turn has.
    fn holds(&self, conn: ConnId, id: &RequestId) -> bool {
        self.admitted.contains_key(&(conn, id.clone()))
    }

    /// Is `(conn, id)` a `turn/start` whose answer has not been seen?
    fn awaits_answer(&self, conn: ConnId, id: &RequestId) -> bool {
        self.admitted
            .get(&(conn, id.clone()))
            .is_some_and(|e| e.state == TurnState::Unanswered)
    }

    fn admit(&mut self, conn: ConnId, id: RequestId, thread: &str) {
        self.admitted.insert(
            (conn, id),
            TurnEntry {
                thread: thread.into(),
                state: TurnState::Unanswered,
            },
        );
    }

    /// The `turn/start` under `(conn, id)` was answered with `turn`. A turn whose terminal
    /// has already been seen — another leg can deliver it before this leg delivers the
    /// answer — has ended, so its entry goes rather than waiting for a terminal that has
    /// passed.
    fn answered(&mut self, conn: ConnId, id: &RequestId, turn: Option<String>) {
        let key = (conn, id.clone());
        let Some(entry) = self.admitted.get_mut(&key) else {
            return;
        };
        if turn
            .as_deref()
            .is_some_and(|t| self.cleared.contains(&entry.thread, t))
        {
            self.admitted.remove(&key);
            return;
        }
        entry.state = TurnState::Answered { turn };
    }

    /// This `turn/start` provably did not start (its request was answered with an error).
    fn release(&mut self, conn: ConnId, id: &RequestId) {
        self.admitted.remove(&(conn, id.clone()));
    }

    /// **A terminal was observed for `(thread, turn)`.** Every readable one is remembered;
    /// a terminal seen before clears nothing. A new one ends the announced turn it names
    /// and every ANSWERED entry on its thread — an answered `turn/start` belongs to the
    /// thread's one running turn, whatever id its answer carried — while an UNANSWERED
    /// entry survives: the server has said nothing about it yet.
    ///
    /// A terminal with no readable turn id clears nothing: it cannot be told apart from its
    /// own duplicate.
    fn terminal(&mut self, thread: &str, turn: Option<&str>) {
        let Some(turn) = turn else {
            return;
        };
        if !self.cleared.admit(thread, turn) {
            return;
        }
        if self.running.get(thread).is_some_and(|t| &**t == turn) {
            self.running.remove(thread);
        }
        self.admitted
            .retain(|_, e| &*e.thread != thread || e.state == TurnState::Unanswered);
    }

    /// **The owning connection went away — which is NOT a turn terminal.** An answered
    /// `turn/start` reached the server and its turn keeps running, so only UNANSWERED
    /// entries go.
    fn close_connection(&mut self, conn: ConnId) {
        self.admitted
            .retain(|(c, _), e| *c != conn || e.state != TurnState::Unanswered);
    }
}

/// The verdict of the phone's ATOMIC turn admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnAdmission {
    /// Head-checked, workspace-checked, id-ledgered and marked busy — all under one lock.
    Admitted,
    /// The turn does not name the head, or there is none.
    NotTheHead { detail: String },
    /// It names a workspace; a phone's turn names none.
    WrongWorkspace { detail: String },
    /// The id ledger refused it.
    Ledger(IdAdmission),
    /// The keyboard is moving the head, or has left it.
    Moving,
    /// Too many turns are admitted and unterminated on this session.
    TooManyActiveTurns,
    /// **A turn is already running on the head.**
    ///
    /// MEASURED on codex 0.153.4: a `turn/start` sent while a turn is running is accepted
    /// and answered with the RUNNING turn's id — an implicit steer carrying no
    /// `expectedTurnId` and therefore no staleness guard. `turn/steer`, which carries the
    /// guard, is the method for the busy case.
    ThreadAlreadyBusy,
}

/// The session's state, behind ONE mutex so every decision the phone leg takes is atomic.
#[derive(Debug)]
struct Binding {
    head: Head,
    /// Has any thread ever become the head?
    ever_bound: bool,
    /// Threads the head moved away from, oldest first: readable from the phone, never
    /// actuable. Ids only, bounded by [`MAX_RETIRED_THREADS`] and [`MAX_THREAD_ID_BYTES`].
    retired: Vec<Box<str>>,
    turns: TurnActivity,
    /// Each phone connection's outstanding request ids, mapped to their methods.
    conns: HashMap<ConnId, HashMap<RequestId, String>>,
    counts: IdLedgerCounts,
}

/// The session's thread state, shared by every connection.
#[derive(Debug, Clone)]
pub struct SessionThreads {
    inner: Arc<Mutex<Binding>>,
}

impl Default for SessionThreads {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionThreads {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Binding {
                head: Head::Unbound,
                ever_bound: false,
                retired: Vec::new(),
                turns: TurnActivity::default(),
                conns: HashMap::new(),
                counts: IdLedgerCounts::default(),
            })),
        }
    }

    fn enter(&self) -> std::sync::MutexGuard<'_, Binding> {
        self.inner.lock().unwrap()
    }

    /// Has any thread ever become the head? The host reads this to tell "the TUI exited
    /// without ever starting a thread" from a session that ran.
    pub fn thread_ever_bound(&self) -> bool {
        self.enter().ever_bound
    }

    /// Does the store hold an id ledger for `conn`?
    #[cfg(test)]
    pub(crate) fn has_tracked_connection(&self, conn: ConnId) -> bool {
        self.enter().conns.contains_key(&conn)
    }

    /// **Watch one keyboard request on its way upstream.** Called before its bytes are
    /// forwarded, so its answer always finds it recorded. Refuses nothing.
    ///
    /// * `thread/start`, `thread/resume`, `thread/fork` — the head starts moving, unless the
    ///   request creates an `ephemeral` thread;
    /// * `turn/start` — its thread is busy until the request is answered with an error, its
    ///   turn ends, or (unanswered) its connection closes;
    /// * `thread/unsubscribe` naming the head — the keyboard has left it.
    ///
    /// Returns TRUE iff the request began a head move, so the relay can undo it with
    /// [`Self::rollback_move`] if the bytes never leave.
    pub fn observe_tui_request(&self, conn: ConnId, shape: &Shape) -> bool {
        let Shape::Request {
            method,
            id: Some(id),
            obj,
        } = shape
        else {
            return false;
        };
        let mut guard = self.enter();
        let g = &mut *guard;
        if MOVE_METHODS.contains(&method.as_str()) {
            // An `ephemeral` thread is not materialized on disk (its `thread.path` is null)
            // and does not become the session's thread. Two producers, both measured on
            // 0.153.4: the title the TUI generates on the first user turn, a `thread/start`
            // (`fixtures/codex/title-thread-0.153.4.jsonl`), and `/side`, a `thread/fork` of
            // the head that leaves the TUI subscribed to the head
            // (`fixtures/codex/side-fork-0.153.4.jsonl`).
            if obj.pointer("/params/ephemeral") == Some(&Value::Bool(true)) {
                return false;
            }
            let from = match std::mem::replace(&mut g.head, Head::Unbound) {
                Head::On(settled) => Some(settled),
                Head::Moving { from, .. } => {
                    if let Some(from) = from {
                        retire(g, from.thread);
                    }
                    None
                }
                Head::Unbound => None,
            };
            g.head = Head::Moving {
                conn,
                id: id.clone(),
                from,
            };
            return true;
        }
        let Some(thread) = obj
            .pointer("/params/threadId")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && t.len() <= MAX_THREAD_ID_BYTES)
        else {
            return false;
        };
        if method == TURN_METHOD {
            g.turns.admit(conn, id.clone(), thread);
        } else if method == UNSUBSCRIBE_METHOD {
            if let Head::On(settled)
            | Head::Moving {
                from: Some(settled),
                ..
            } = &mut g.head
            {
                if settled.thread == thread {
                    settled.left = true;
                }
            }
        }
        false
    }

    /// The keyboard move under `(conn, id)` provably never left the broker (the upstream
    /// write failed): the head goes back to where it was.
    pub fn rollback_move(&self, conn: ConnId, id: &RequestId) {
        let mut g = self.enter();
        if let Head::Moving {
            conn: c,
            id: i,
            from,
        } = &g.head
        {
            if *c == conn && i == id {
                g.head = from.clone().map_or(Head::Unbound, Head::On);
            }
        }
    }

    /// The connection went away. A keyboard move still in flight on it DID reach the
    /// server, so its outcome is unknown: no head until the keyboard binds one again, and
    /// the head it started from is retired, still readable. Its unanswered `turn/start`s and
    /// its id ledger go with it.
    pub fn close_connection(&self, conn: ConnId) {
        let mut guard = self.enter();
        let g = &mut *guard;
        if matches!(&g.head, Head::Moving { conn: c, .. } if *c == conn) {
            if let Head::Moving {
                from: Some(from), ..
            } = std::mem::replace(&mut g.head, Head::Unbound)
            {
                retire(g, from.thread);
            }
        }
        g.turns.close_connection(conn);
        g.conns.remove(&conn);
    }

    /// Observe one server→client frame **on the connection `conn`**.
    ///
    /// * `turn/started` and `turn/completed` update [`TurnActivity`], whichever leg carries
    ///   them;
    /// * a response to the keyboard move in flight on `conn` settles it;
    /// * a response to a recorded `turn/start` on `conn` ends it (error) or ties it to its
    ///   turn (result);
    /// * a response releases its id from `conn`'s phone ledger.
    ///
    /// ## Cheap, and validated
    ///
    /// A top-level header scan ([`crate::message::scan_frame_header`]) skips every body
    /// member, so a multi-MB `plugin/list` answer never becomes a `Value`. Only a response
    /// the header proves to be one — exactly one of `result`/`error`, and an `error` that
    /// is a JSON-RPC error object — releases an id or ends a turn; a bare `{"id":X}` proves
    /// nothing. A move's success is parsed in full with the duplicate-rejecting parser, and
    /// a frame whose members are ambiguous leaves the move in flight.
    pub fn observe_server_frame(&self, conn: ConnId, text: &str) {
        let Some(header) = crate::message::scan_frame_header(text) else {
            return;
        };
        if header.has_method {
            self.observe_turn_lifecycle(text);
            return;
        }
        let Some(id) = header.id else {
            return;
        };
        let mut guard = self.enter();
        let g = &mut *guard;

        if matches!(&g.head, Head::Moving { conn: c, id: i, .. } if *c == conn && *i == id) {
            let outcome = match header.response {
                ResponseKind::Error => MoveOutcome::Refused,
                ResponseKind::NotAResponse => MoveOutcome::Unknown,
                ResponseKind::Result => {
                    let Some(v) = crate::message::parse_no_dup_value(text) else {
                        return;
                    };
                    v.pointer("/result/thread/id")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty() && t.len() <= MAX_THREAD_ID_BYTES)
                        .map_or(MoveOutcome::Unknown, |t| MoveOutcome::Landed(t.to_string()))
                }
            };
            let Head::Moving { from, .. } = std::mem::replace(&mut g.head, Head::Unbound) else {
                unreachable!("matched as Moving above")
            };
            g.head = match outcome {
                MoveOutcome::Landed(thread) => {
                    if let Some(from) = from.filter(|f| f.thread != thread) {
                        retire(g, from.thread);
                    }
                    g.ever_bound = true;
                    Head::On(Settled {
                        thread,
                        left: false,
                    })
                }
                MoveOutcome::Refused => from.map_or(Head::Unbound, Head::On),
                MoveOutcome::Unknown => {
                    if let Some(from) = from {
                        retire(g, from.thread);
                    }
                    Head::Unbound
                }
            };
            return;
        }

        if !header.response.is_response() {
            return;
        }
        if g.turns.awaits_answer(conn, &id) {
            if header.response == ResponseKind::Error {
                g.turns.release(conn, &id);
            } else {
                let turn = crate::message::parse_no_dup_value(text).and_then(|v| {
                    v.pointer("/result/turn/id")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty() && t.len() <= MAX_THREAD_ID_BYTES)
                        .map(str::to_string)
                });
                g.turns.answered(conn, &id, turn);
            }
        }
        if let Some(outstanding) = g.conns.get_mut(&conn) {
            outstanding.remove(&id);
        }
    }

    /// **`turn/started` marks its thread running; `turn/completed` ends the turn it
    /// names**, whatever its `status` — `completed`, `interrupted` and `failed` all end it.
    ///
    /// Recorded for every thread, not only the head: a turn announced while the keyboard is
    /// moving belongs to a thread that may be the head again a moment later.
    ///
    /// Deliberately narrow: it reads three fields, and a frame it cannot read changes
    /// nothing.
    fn observe_turn_lifecycle(&self, text: &str) {
        // Cheap prefilter before any parse: multi-MB notifications reach this path.
        if !text.contains("\"turn/completed\"") && !text.contains("\"turn/started\"") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let started = match v.get("method").and_then(Value::as_str) {
            Some("turn/started") => true,
            Some("turn/completed") => false,
            _ => return,
        };
        let readable = |pointer: &str| {
            v.pointer(pointer)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= MAX_THREAD_ID_BYTES)
        };
        let Some(thread) = readable("/params/threadId") else {
            return;
        };
        let turn = readable("/params/turn/id");
        let mut guard = self.enter();
        let g = &mut *guard;
        if !started {
            g.turns.terminal(thread, turn);
        } else if let Some(turn) = turn {
            let head = head_of(&g.head).map(str::to_string);
            g.turns.started(thread, turn, head.as_deref());
        }
    }

    /// How many threads this session has retired.
    pub fn retired_len(&self) -> usize {
        self.enter().retired.len()
    }
}

/// The thread the keyboard is on or is moving from, left or not.
fn head_of(head: &Head) -> Option<&str> {
    match head {
        Head::On(s) | Head::Moving { from: Some(s), .. } => Some(&s.thread),
        _ => None,
    }
}

/// Move a thread onto the retired list — id only, once, and never past the cap.
fn retire(g: &mut Binding, thread: String) {
    if g.retired.len() >= MAX_RETIRED_THREADS || g.retired.iter().any(|t| **t == *thread) {
        return;
    }
    g.retired.push(thread.into_boxed_str());
}

/// The phone leg's id ledger: record `id` as outstanding on `conn` with its method.
fn admit_id(g: &mut Binding, conn: ConnId, id: &RequestId, method: &str) -> IdAdmission {
    if !id_within_cap(id) {
        g.counts.oversized += 1;
        return IdAdmission::Oversized;
    }
    if !g.conns.contains_key(&conn) && g.conns.len() >= MAX_TRACKED_CONNECTIONS {
        g.counts.at_capacity += 1;
        return IdAdmission::AtCapacity;
    }
    let outstanding = g.conns.entry(conn).or_default();
    if outstanding.contains_key(id) {
        g.counts.reused_in_flight += 1;
        return IdAdmission::ReusedInFlight;
    }
    if outstanding.len() >= MAX_OUTSTANDING_REQUESTS {
        g.counts.at_capacity += 1;
        return IdAdmission::AtCapacity;
    }
    outstanding.insert(id.clone(), method.to_string());
    IdAdmission::Admitted
}

impl ThreadBinding for SessionThreads {
    fn bound_thread(&self) -> Option<String> {
        match &self.enter().head {
            Head::On(Settled {
                thread,
                left: false,
            }) => Some(thread.clone()),
            _ => None,
        }
    }

    fn try_admit_request(&self, conn: ConnId, id: &RequestId, method: &str) -> IdAdmission {
        admit_id(&mut self.enter(), conn, id, method)
    }

    fn try_admit_idle_turn(
        &self,
        conn: ConnId,
        id: &RequestId,
        thread_id: &str,
        cwd: Option<&Value>,
        roots: Option<&Value>,
    ) -> TurnAdmission {
        let mut guard = self.enter();
        let g = &mut *guard;

        // 1. The head-check. The ids are grammar-checked before they are logged
        //    (`redact::thread_id`): `params.threadId` is client-chosen and this detail lands
        //    in a durable `broker.log`.
        let head = match &g.head {
            Head::On(Settled {
                thread,
                left: false,
            }) => thread.clone(),
            Head::On(_) | Head::Moving { .. } => return TurnAdmission::Moving,
            Head::Unbound => {
                return TurnAdmission::NotTheHead {
                    detail: format!(
                        "turn/start names thread {} but the keyboard is on no thread",
                        crate::redact::thread_id(thread_id)
                    ),
                }
            }
        };
        if head != thread_id {
            return TurnAdmission::NotTheHead {
                detail: format!(
                    "turn/start names thread {} but the keyboard is on {}",
                    crate::redact::thread_id(thread_id),
                    crate::redact::thread_id(&head)
                ),
            };
        }
        // 2. The workspace: a phone's turn names none. The detail names the FIELD, never
        //    the value.
        if cwd != Some(&Value::Null) || roots.is_some() {
            return TurnAdmission::WrongWorkspace {
                detail: format!(
                    "turn/start: a phone's turn names no workspace — params.cwd must be null \
                     and params.runtimeWorkspaceRoots absent (cwd: {}; roots: {})",
                    crate::redact::value_shape(cwd),
                    crate::redact::value_shape(roots)
                ),
            };
        }
        // 3. This id still holds a live turn entry: a second turn under it would overwrite
        //    the first turn's only mark. Counted, and checked before the idle rule so the
        //    count does not depend on whether a turn happens to be running.
        if g.turns.holds(conn, id) {
            g.counts.reused_in_flight += 1;
            return TurnAdmission::Ledger(IdAdmission::ReusedInFlight);
        }
        // 4. The idle rule, read in THIS section: the mark it consults is set below.
        if g.turns.busy_on(&head) {
            return TurnAdmission::ThreadAlreadyBusy;
        }
        if g.turns.len() >= MAX_ACTIVE_TURNS {
            return TurnAdmission::TooManyActiveTurns;
        }
        match admit_id(g, conn, id, TURN_METHOD) {
            IdAdmission::Admitted => {}
            other => return TurnAdmission::Ledger(other),
        }
        g.turns.admit(conn, id.clone(), thread_id);
        TurnAdmission::Admitted
    }

    fn is_active_turn(&self, thread: &str, turn: &str) -> bool {
        self.enter().turns.is_active(thread, turn)
    }

    fn id_ledger_counts(&self) -> IdLedgerCounts {
        self.enter().counts
    }

    fn is_session_thread(&self, thread_id: &str) -> bool {
        let g = self.enter();
        head_of(&g.head) == Some(thread_id) || g.retired.iter().any(|t| &**t == thread_id)
    }
}

/// A binding that knows no threads: every head-check refuses and every phone request is
/// admitted untracked. The default in unit tests that are not exercising the session.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoThreads;

impl ThreadBinding for NoThreads {
    fn bound_thread(&self) -> Option<String> {
        None
    }

    fn try_admit_request(&self, _conn: ConnId, _id: &RequestId, _method: &str) -> IdAdmission {
        IdAdmission::Admitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{classify_shape, WsPayload};
    use serde_json::json;

    /// The keyboard's connection.
    const K: ConnId = ConnId(1);
    /// A second connection: another keyboard leg, or the phone's.
    const B: ConnId = ConnId(2);

    fn req(id: &str) -> RequestId {
        RequestId::Str(id.to_string())
    }

    /// Show the store one keyboard request, as the relay does before forwarding it.
    fn keyboard(s: &SessionThreads, conn: ConnId, method: &str, id: &str, params: Value) -> bool {
        let frame = json!({"method": method, "id": id, "params": params}).to_string();
        s.observe_tui_request(conn, &classify_shape(&WsPayload::Text(frame)))
    }

    /// A keyboard move to wherever the server says.
    fn move_head(s: &SessionThreads, conn: ConnId, id: &str) -> bool {
        keyboard(s, conn, "thread/start", id, json!({"cwd": null}))
    }

    /// The server's success answer to a move, naming `thread`.
    fn landed(id: &str, thread: &str) -> String {
        json!({"id": id, "result": {"thread": {"id": thread}, "cwd": "/anywhere"}}).to_string()
    }

    fn error_response(id: &str) -> String {
        json!({"id": id, "error": {"code": -1, "message": "boom"}}).to_string()
    }

    /// A store whose head is `thread`, bound the only way a head binds.
    fn on(thread: &str) -> SessionThreads {
        let s = SessionThreads::new();
        assert!(move_head(&s, K, "start"));
        s.observe_server_frame(K, &landed("start", thread));
        assert_eq!(s.bound_thread(), Some(thread.to_string()));
        s
    }

    fn announced(thread: &str, turn: &str) -> String {
        json!({"method": "turn/started",
               "params": {"threadId": thread, "turn": {"id": turn, "status": "inProgress"}}})
        .to_string()
    }

    fn terminal(thread: &str, turn: &str, status: &str) -> String {
        json!({"method": "turn/completed",
               "params": {"threadId": thread, "turn": {"id": turn, "status": status}}})
        .to_string()
    }

    fn turn_answer(id: &str, turn: &str) -> String {
        json!({"id": id, "result": {"turn": {"id": turn, "status": "inProgress"}}}).to_string()
    }

    /// The phone's `turn/start` on `thread`, as the admission sees it.
    fn phone(s: &SessionThreads, id: &str, thread: &str) -> TurnAdmission {
        s.try_admit_idle_turn(B, &req(id), thread, Some(&Value::Null), None)
    }

    // ---------------------------------------------------------------------------
    // The head follows the keyboard.
    // ---------------------------------------------------------------------------

    #[test]
    fn every_keyboard_move_method_moves_the_head_and_the_answer_names_it() {
        for method in MOVE_METHODS {
            let s = on("01a0");
            assert!(keyboard(&s, K, method, "m", json!({"threadId": "01a0"})));
            assert_eq!(s.bound_thread(), None, "{method}: the head is moving");
            assert!(
                s.is_session_thread("01a0"),
                "the thread it moves from stays readable"
            );
            s.observe_server_frame(K, &landed("m", "01a1"));
            assert_eq!(s.bound_thread(), Some("01a1".into()), "{method}");
            assert!(
                s.is_session_thread("01a0"),
                "{method}: retired, still readable"
            );
            assert!(s.thread_ever_bound());
        }
    }

    /// A session launched as `codex resume` or `codex fork` never sends `thread/start`:
    /// its first creation is the resume or fork, and that binds the head and counts as
    /// the session's thread.
    #[test]
    fn a_first_resume_or_fork_binds_the_head() {
        for method in ["thread/resume", "thread/fork"] {
            let s = SessionThreads::new();
            assert!(keyboard(&s, K, method, "r", json!({"threadId": "01a0"})));
            assert!(!s.thread_ever_bound(), "{method}: nothing bound yet");
            s.observe_server_frame(K, &landed("r", "01a1"));
            assert_eq!(s.bound_thread(), Some("01a1".into()), "{method}");
            assert!(s.thread_ever_bound(), "{method}");
        }
    }

    #[test]
    fn requests_that_move_nothing_leave_the_head_alone() {
        let s = on("01a0");
        for method in ["thread/read", "thread/list", "model/list", "turn/steer"] {
            assert!(!keyboard(&s, K, method, "x", json!({"threadId": "01a1"})));
            assert_eq!(s.bound_thread(), Some("01a0".into()), "{method}");
        }
        assert!(!s.observe_tui_request(
            K,
            &classify_shape(&WsPayload::Text(r#"{"method":"thread/start"}"#.into()))
        ));
        assert_eq!(
            s.bound_thread(),
            Some("01a0".into()),
            "a notification moves nothing"
        );
    }

    #[test]
    fn only_the_answer_on_the_asking_connection_settles_a_move() {
        let s = SessionThreads::new();
        move_head(&s, K, "start");
        s.observe_server_frame(B, &landed("start", "01a0"));
        assert_eq!(
            s.bound_thread(),
            None,
            "another connection's answer correlates to nothing"
        );
        s.observe_server_frame(K, &landed("other-id", "01a0"));
        assert_eq!(
            s.bound_thread(),
            None,
            "an answer to another request settles nothing"
        );
        s.observe_server_frame(K, &landed("start", "01a0"));
        assert_eq!(s.bound_thread(), Some("01a0".into()));
    }

    #[test]
    fn an_announcement_moves_nothing() {
        let s = SessionThreads::new();
        move_head(&s, K, "start");
        for frame in [
            r#"{"method":"thread/started","params":{"thread":{"id":"01a0"}}}"#,
            r#"{"method":"thread/resumed","params":{"thread":{"id":"01a0"}}}"#,
        ] {
            s.observe_server_frame(K, frame);
        }
        assert_eq!(s.bound_thread(), None);
        assert!(!s.is_session_thread("01a0"));
        assert!(!s.thread_ever_bound());
    }

    #[test]
    fn a_refused_move_puts_the_head_back() {
        let s = on("01a0");
        move_head(&s, K, "sw");
        s.observe_server_frame(K, &error_response("sw"));
        assert_eq!(s.bound_thread(), Some("01a0".into()));
        assert_eq!(s.retired_len(), 0);
    }

    #[test]
    fn an_answer_that_proves_nothing_leaves_no_head_and_keeps_the_old_one_readable() {
        for answer in [
            r#"{"id":"sw","result":{"thread":{}}}"#.to_string(),
            r#"{"id":"sw","result":{"thread":{"id":7}}}"#.to_string(),
            json!({"id": "sw", "result": {"thread": {"id": "x".repeat(MAX_THREAD_ID_BYTES + 1)}}})
                .to_string(),
            r#"{"id":"sw"}"#.to_string(),
            r#"{"id":"sw","error":{"code":"x","message":"m"}}"#.to_string(),
        ] {
            let s = on("01a0");
            move_head(&s, K, "sw");
            s.observe_server_frame(K, &answer);
            assert_eq!(s.bound_thread(), None, "{answer}");
            assert!(s.is_session_thread("01a0"), "{answer}");
        }
    }

    #[test]
    fn an_ambiguous_success_leaves_the_move_in_flight() {
        let s = on("01a0");
        move_head(&s, K, "sw");
        s.observe_server_frame(
            K,
            r#"{"id":"sw","result":{"thread":{"id":"01a1","id":"01a2"}}}"#,
        );
        assert_eq!(s.bound_thread(), None);
        s.observe_server_frame(K, &landed("sw", "01a1"));
        assert_eq!(
            s.bound_thread(),
            Some("01a1".into()),
            "the real answer still settles it"
        );
    }

    #[test]
    fn a_resume_of_the_head_is_not_a_retirement() {
        let s = on("01a0");
        keyboard(&s, K, "thread/resume", "r", json!({"threadId": "01a0"}));
        s.observe_server_frame(K, &landed("r", "01a0"));
        assert_eq!(s.bound_thread(), Some("01a0".into()));
        assert_eq!(s.retired_len(), 0);
    }

    #[test]
    fn a_connection_that_closes_mid_move_leaves_no_head_until_the_next_bind() {
        let s = on("01a0");
        move_head(&s, K, "sw");
        s.close_connection(B);
        assert_eq!(
            s.bound_thread(),
            None,
            "another connection's close changes nothing"
        );
        s.close_connection(K);
        assert_eq!(s.bound_thread(), None);
        assert!(s.is_session_thread("01a0"), "retired, still readable");
        move_head(&s, B, "again");
        s.observe_server_frame(B, &landed("again", "01a2"));
        assert_eq!(s.bound_thread(), Some("01a2".into()));
    }

    #[test]
    fn a_move_that_never_left_puts_the_head_back() {
        let s = on("01a0");
        move_head(&s, K, "sw");
        s.rollback_move(K, &req("other"));
        assert_eq!(
            s.bound_thread(),
            None,
            "only the move itself can be rolled back"
        );
        s.rollback_move(K, &req("sw"));
        assert_eq!(s.bound_thread(), Some("01a0".into()));

        let s = SessionThreads::new();
        move_head(&s, K, "start");
        s.rollback_move(K, &req("start"));
        assert_eq!(s.bound_thread(), None);
    }

    #[test]
    fn a_second_move_in_flight_can_put_nothing_back() {
        let s = on("01a0");
        move_head(&s, K, "first");
        move_head(&s, B, "second");
        s.observe_server_frame(B, &error_response("second"));
        assert_eq!(
            s.bound_thread(),
            None,
            "the first move may still land, so nothing is put back"
        );
        s.observe_server_frame(K, &landed("first", "01a1"));
        assert_eq!(
            s.bound_thread(),
            None,
            "and the replaced move settles nothing"
        );
        assert!(s.is_session_thread("01a0"));
    }

    #[test]
    fn an_unsubscribe_of_the_head_takes_the_phone_off_it_until_the_next_bind() {
        let s = on("01a0");
        keyboard(
            &s,
            K,
            "thread/unsubscribe",
            "u",
            json!({"threadId": "01a1"}),
        );
        assert_eq!(
            s.bound_thread(),
            Some("01a0".into()),
            "another thread: nothing"
        );
        keyboard(
            &s,
            K,
            "thread/unsubscribe",
            "u",
            json!({"threadId": "01a0"}),
        );
        assert_eq!(s.bound_thread(), None);
        assert!(s.is_session_thread("01a0"));
        assert_eq!(phone(&s, "p", "01a0"), TurnAdmission::Moving);

        // A move the server refuses puts back the head as it was: still left.
        move_head(&s, K, "sw");
        s.observe_server_frame(K, &error_response("sw"));
        assert_eq!(s.bound_thread(), None);

        move_head(&s, K, "sw2");
        s.observe_server_frame(K, &landed("sw2", "01a1"));
        assert_eq!(s.bound_thread(), Some("01a1".into()));
    }

    #[test]
    fn an_unsubscribe_during_a_move_marks_the_head_it_would_restore() {
        let s = on("01a0");
        keyboard(&s, K, "thread/resume", "r", json!({"threadId": "01a1"}));
        keyboard(
            &s,
            K,
            "thread/unsubscribe",
            "u",
            json!({"threadId": "01a0"}),
        );
        s.observe_server_frame(K, &error_response("r"));
        assert_eq!(s.bound_thread(), None, "restored, and still left");
    }

    #[test]
    fn the_retired_list_is_bounded() {
        let s = on("t-0");
        for i in 1..=MAX_RETIRED_THREADS + 3 {
            let id = format!("m-{i}");
            move_head(&s, K, &id);
            s.observe_server_frame(K, &landed(&id, &format!("t-{i}")));
        }
        assert_eq!(s.retired_len(), MAX_RETIRED_THREADS);
        assert!(s.is_session_thread("t-0"));
        assert!(!s.is_session_thread(&format!("t-{}", MAX_RETIRED_THREADS + 1)));
    }

    // ---------------------------------------------------------------------------
    // What is running.
    // ---------------------------------------------------------------------------

    #[test]
    fn a_keyboard_turn_start_in_flight_makes_its_thread_busy() {
        let s = on("01a0");
        keyboard(
            &s,
            K,
            "turn/start",
            "k",
            json!({"threadId": "01a0", "model": "x"}),
        );
        assert_eq!(phone(&s, "p", "01a0"), TurnAdmission::ThreadAlreadyBusy);
        s.observe_server_frame(K, &error_response("k"));
        assert_eq!(
            phone(&s, "p", "01a0"),
            TurnAdmission::Admitted,
            "an errored start ends it"
        );
    }

    #[test]
    fn a_keyboard_turn_is_ended_by_its_threads_terminal() {
        let s = on("01a0");
        keyboard(&s, K, "turn/start", "k", json!({"threadId": "01a0"}));
        s.observe_server_frame(K, &terminal("01a0", "turn-0", "completed"));
        assert_eq!(
            phone(&s, "p", "01a0"),
            TurnAdmission::ThreadAlreadyBusy,
            "an unanswered start survives a terminal: the server has said nothing about it"
        );
        s.observe_server_frame(K, &turn_answer("k", "turn-1"));
        assert!(s.is_active_turn("01a0", "turn-1"));
        s.observe_server_frame(K, &terminal("01a0", "turn-1", "interrupted"));
        assert!(!s.is_active_turn("01a0", "turn-1"));
        assert_eq!(phone(&s, "p", "01a0"), TurnAdmission::Admitted);
    }

    #[test]
    fn a_turn_the_server_announces_runs_until_its_own_terminal() {
        let s = on("01a0");
        s.observe_server_frame(B, &announced("01a0", "turn-1"));
        s.observe_server_frame(K, &announced("01a0", "turn-1"));
        assert!(s.is_active_turn("01a0", "turn-1"));
        assert!(!s.is_active_turn("01a0", "turn-0"));
        assert_eq!(phone(&s, "p1", "01a0"), TurnAdmission::ThreadAlreadyBusy);
        s.observe_server_frame(B, &terminal("01a0", "turn-0", "completed"));
        assert!(
            s.is_active_turn("01a0", "turn-1"),
            "another turn's terminal"
        );
        s.observe_server_frame(B, &terminal("01a0", "turn-1", "failed"));
        assert_eq!(phone(&s, "p2", "01a0"), TurnAdmission::Admitted);
    }

    /// A turn announced while the keyboard is moving is kept, and the head the
    /// failed move puts back is busy.
    #[test]
    fn a_turn_announced_during_a_move_is_busy_when_the_move_fails() {
        let s = on("01a0");
        move_head(&s, K, "sw");
        s.observe_server_frame(B, &announced("01a0", "turn-1"));
        s.observe_server_frame(K, &error_response("sw"));
        assert_eq!(phone(&s, "p", "01a0"), TurnAdmission::ThreadAlreadyBusy);
        assert!(s.is_active_turn("01a0", "turn-1"));
    }

    /// A turn announced on a thread before it becomes the head is running when it does.
    #[test]
    fn a_turn_announced_before_its_thread_becomes_the_head_is_busy_after() {
        let s = on("01a0");
        s.observe_server_frame(B, &announced("01a1", "turn-1"));
        assert_eq!(
            phone(&s, "p", "01a0"),
            TurnAdmission::Admitted,
            "not the head's"
        );
        keyboard(&s, K, "thread/resume", "r", json!({"threadId": "01a1"}));
        s.observe_server_frame(K, &landed("r", "01a1"));
        assert_eq!(phone(&s, "p2", "01a1"), TurnAdmission::ThreadAlreadyBusy);
    }

    /// Every readable terminal is remembered, so a lagging announcement of a turn
    /// that already ended — even one nothing had marked — marks nothing.
    #[test]
    fn a_late_announcement_of_an_ended_turn_marks_nothing() {
        let s = on("01a0");
        s.observe_server_frame(B, &terminal("01a0", "turn-1", "completed"));
        s.observe_server_frame(K, &announced("01a0", "turn-1"));
        assert!(!s.is_active_turn("01a0", "turn-1"));
        assert_eq!(phone(&s, "p", "01a0"), TurnAdmission::Admitted);

        let s = on("01a0");
        s.observe_server_frame(B, &announced("01a0", "turn-2"));
        s.observe_server_frame(K, &terminal("01a0", "turn-1", "completed"));
        assert!(
            s.is_active_turn("01a0", "turn-2"),
            "an older turn's terminal clears nothing"
        );
    }

    #[test]
    fn announcements_that_name_no_readable_turn_mark_nothing() {
        let s = on("01a0");
        for turn in [json!(null), json!(7), json!(""), json!({"id": "x"})] {
            s.observe_server_frame(
                K,
                &json!({"method": "turn/started",
                        "params": {"threadId": "01a0", "turn": {"id": turn}}})
                .to_string(),
            );
        }
        s.observe_server_frame(
            K,
            r#"{"method":"turn/started","params":{"threadId":"01a0","turn":{}}}"#,
        );
        assert_eq!(phone(&s, "p", "01a0"), TurnAdmission::Admitted);
    }

    #[test]
    fn a_terminal_naming_no_turn_or_another_thread_clears_nothing() {
        let s = on("01a0");
        s.observe_server_frame(B, &announced("01a0", "turn-0"));
        for frame in [
            r#"{"method":"turn/completed","params":{"threadId":"01a0","turn":{"status":"completed"}}}"#.to_string(),
            r#"{"method":"turn/completed","params":{"threadId":"01a0","turn":{"id":""}}}"#.to_string(),
            r#"{"method":"turn/completed","params":{"threadId":"01a0"}}"#.to_string(),
            r#"{"method":"turn/completed","params":{"threadId":123}}"#.to_string(),
            "not json but mentions \"turn/completed\"".to_string(),
            terminal("01a0-OTHER", "turn-0", "completed"),
        ] {
            s.observe_server_frame(K, &frame);
            assert_eq!(phone(&s, "p", "01a0"), TurnAdmission::ThreadAlreadyBusy, "{frame}");
        }
    }

    #[test]
    fn a_duplicate_terminal_clears_nothing() {
        let s = on("01a0");
        assert_eq!(phone(&s, "t0", "01a0"), TurnAdmission::Admitted);
        s.observe_server_frame(B, &turn_answer("t0", "turn-0"));
        s.observe_server_frame(B, &terminal("01a0", "turn-0", "completed"));
        assert_eq!(phone(&s, "t1", "01a0"), TurnAdmission::Admitted);
        s.observe_server_frame(B, &turn_answer("t1", "turn-1"));
        s.observe_server_frame(K, &terminal("01a0", "turn-0", "completed"));
        assert!(
            s.is_active_turn("01a0", "turn-1"),
            "the duplicate cleared nothing"
        );
        assert_eq!(phone(&s, "t2", "01a0"), TurnAdmission::ThreadAlreadyBusy);
    }

    #[test]
    fn a_terminal_epoch_is_scoped_to_its_thread() {
        let s = on("01a0");
        s.observe_server_frame(B, &terminal("01a0", "t", "completed"));
        s.observe_server_frame(B, &announced("01a1", "t"));
        assert!(
            s.is_active_turn("01a1", "t"),
            "another thread's turn of the same id is its own turn"
        );
    }

    #[test]
    fn an_answered_phone_turn_is_cleared_by_a_terminal_naming_another_id() {
        let s = on("01a0");
        assert_eq!(phone(&s, "t0", "01a0"), TurnAdmission::Admitted);
        s.observe_server_frame(B, &turn_answer("t0", "phantom"));
        s.observe_server_frame(B, &terminal("01a0", "turn-0", "completed"));
        assert_eq!(phone(&s, "t1", "01a0"), TurnAdmission::Admitted);
    }

    #[test]
    fn an_unanswered_turn_start_names_no_turn() {
        let s = on("01a0");
        assert_eq!(phone(&s, "t0", "01a0"), TurnAdmission::Admitted);
        for invented in ["turn-0", "anything", ""] {
            assert!(!s.is_active_turn("01a0", invented));
        }
        s.observe_server_frame(B, &turn_answer("t0", "turn-0"));
        assert!(s.is_active_turn("01a0", "turn-0"));
        assert!(
            !ThreadBinding::is_active_turn(&s, "01a1", "turn-0"),
            "scoped to its thread"
        );
    }

    #[test]
    fn a_disconnect_releases_only_unanswered_turns() {
        let s = on("01a0");
        assert_eq!(phone(&s, "answered", "01a0"), TurnAdmission::Admitted);
        s.observe_server_frame(B, &turn_answer("answered", "turn-0"));
        s.close_connection(B);
        assert_eq!(phone(&s, "p", "01a0"), TurnAdmission::ThreadAlreadyBusy);

        let s = on("01a0");
        keyboard(&s, K, "turn/start", "k", json!({"threadId": "01a0"}));
        s.close_connection(K);
        assert_eq!(phone(&s, "p", "01a0"), TurnAdmission::Admitted);
    }

    #[test]
    fn the_running_table_is_bounded_and_keeps_the_heads_turn() {
        let s = on("01a0");
        s.observe_server_frame(B, &announced("01a0", "head-turn"));
        for i in 0..MAX_RUNNING_THREADS + 5 {
            s.observe_server_frame(B, &announced(&format!("child-{i}"), "t"));
        }
        assert!(s.is_active_turn("01a0", "head-turn"));
        assert!(s.enter().turns.running.len() <= MAX_RUNNING_THREADS);
    }

    // ---------------------------------------------------------------------------
    // The phone's turn admission.
    // ---------------------------------------------------------------------------

    #[test]
    fn a_phone_turn_names_the_head_and_no_workspace() {
        let s = SessionThreads::new();
        assert!(matches!(
            phone(&s, "p", "01a0"),
            TurnAdmission::NotTheHead { .. }
        ));
        let s = on("01a0");
        assert!(matches!(
            phone(&s, "p", "01a1"),
            TurnAdmission::NotTheHead { .. }
        ));
        let null = Value::Null;
        for (cwd, roots) in [
            (Some(json!("/work")), None),
            (Some(null.clone()), Some(json!(["/work"]))),
            (None, None),
        ] {
            assert!(
                matches!(
                    s.try_admit_idle_turn(B, &req("p"), "01a0", cwd.as_ref(), roots.as_ref()),
                    TurnAdmission::WrongWorkspace { .. }
                ),
                "cwd={cwd:?} roots={roots:?}"
            );
        }
        move_head(&s, K, "sw");
        assert_eq!(phone(&s, "p", "01a0"), TurnAdmission::Moving);
    }

    #[test]
    fn a_turn_id_with_a_live_entry_cannot_start_another_turn() {
        let s = on("01a0");
        assert_eq!(phone(&s, "t", "01a0"), TurnAdmission::Admitted);
        s.observe_server_frame(B, &turn_answer("t", "turn-0"));
        assert_eq!(
            phone(&s, "t", "01a0"),
            TurnAdmission::Ledger(IdAdmission::ReusedInFlight)
        );
        assert_eq!(s.id_ledger_counts().reused_in_flight, 1);
        s.observe_server_frame(B, &terminal("01a0", "turn-0", "completed"));
        assert_eq!(phone(&s, "t", "01a0"), TurnAdmission::Admitted);
    }

    #[test]
    fn a_response_to_another_request_under_a_turns_id_does_not_end_the_turn() {
        let s = on("01a0");
        assert_eq!(phone(&s, "t", "01a0"), TurnAdmission::Admitted);
        s.observe_server_frame(B, &turn_answer("t", "turn-0"));
        assert_eq!(
            s.try_admit_request(B, &req("t"), "thread/read"),
            IdAdmission::Admitted
        );
        s.observe_server_frame(B, &error_response("t"));
        assert!(
            s.is_active_turn("01a0", "turn-0"),
            "the read's error is not the turn's"
        );
    }

    #[test]
    fn the_phone_turn_path_applies_the_request_id_byte_cap() {
        let s = on("01a0");
        let long = req(&"z".repeat(MAX_REQUEST_ID_BYTES + 1));
        assert_eq!(
            s.try_admit_idle_turn(B, &long, "01a0", Some(&Value::Null), None),
            TurnAdmission::Ledger(IdAdmission::Oversized)
        );
        assert_eq!(s.id_ledger_counts().oversized, 1);
    }

    // ---------------------------------------------------------------------------
    // The phone leg's id ledger.
    // ---------------------------------------------------------------------------

    fn admit(s: &SessionThreads, conn: ConnId, id: &RequestId, method: &str) -> IdAdmission {
        s.try_admit_request(conn, id, method)
    }

    #[test]
    fn an_id_reused_while_in_flight_is_refused_and_counted() {
        let s = SessionThreads::new();
        assert_eq!(
            admit(&s, B, &req("dup"), "thread/read"),
            IdAdmission::Admitted
        );
        for method in ["thread/read", "thread/resume"] {
            assert_eq!(
                admit(&s, B, &req("dup"), method),
                IdAdmission::ReusedInFlight
            );
        }
        assert_eq!(s.id_ledger_counts().reused_in_flight, 2);
        assert_eq!(
            admit(&s, K, &req("dup"), "thread/read"),
            IdAdmission::Admitted
        );
    }

    #[test]
    fn a_response_releases_its_id_on_its_own_connection_only() {
        let s = SessionThreads::new();
        assert_eq!(
            admit(&s, B, &req("7"), "thread/read"),
            IdAdmission::Admitted
        );
        s.observe_server_frame(K, r#"{"id":"7","result":{}}"#);
        assert_eq!(
            admit(&s, B, &req("7"), "thread/read"),
            IdAdmission::ReusedInFlight
        );
        s.observe_server_frame(B, r#"{"id":"7","result":{"data":[]}}"#);
        assert_eq!(
            admit(&s, B, &req("7"), "thread/read"),
            IdAdmission::Admitted
        );
        s.observe_server_frame(B, &error_response("7"));
        assert_eq!(
            admit(&s, B, &req("7"), "thread/read"),
            IdAdmission::Admitted
        );
    }

    #[test]
    fn only_a_provable_response_releases_an_id() {
        for frame in [
            r#"{"id":"X"}"#,
            r#"{"id":"X","result":{"ok":true},"error":{"code":-1,"message":"m"}}"#,
            r#"{"id":"X","error":{}}"#,
            r#"{"id":"X","error":{"code":-1.5,"message":"m"}}"#,
            r#"{"id":"X","error":{"code":"str","code":-1,"message":"m"}}"#,
            r#"{"id":"X","id":"X","result":{}}"#,
            r#"{"id":"X","method":"item/commandExecution/requestApproval","params":{}}"#,
        ] {
            let s = SessionThreads::new();
            assert_eq!(
                admit(&s, B, &req("X"), "thread/read"),
                IdAdmission::Admitted
            );
            s.observe_server_frame(B, frame);
            assert_eq!(
                admit(&s, B, &req("X"), "thread/read"),
                IdAdmission::ReusedInFlight,
                "must NOT have released: {frame}"
            );
        }
    }

    #[test]
    fn an_over_long_request_id_is_refused_and_never_stored() {
        let s = SessionThreads::new();
        let long = req(&"x".repeat(MAX_REQUEST_ID_BYTES + 1));
        assert_eq!(admit(&s, B, &long, "thread/read"), IdAdmission::Oversized);
        assert!(!s.has_tracked_connection(B));
        let at_cap = req(&"x".repeat(MAX_REQUEST_ID_BYTES));
        assert_eq!(admit(&s, B, &at_cap, "thread/read"), IdAdmission::Admitted);
        assert_eq!(
            admit(&s, B, &RequestId::Int(i64::MIN), "thread/read"),
            IdAdmission::Admitted
        );
    }

    #[test]
    fn the_outstanding_ledger_is_bounded_per_connection() {
        let s = SessionThreads::new();
        for i in 0..MAX_OUTSTANDING_REQUESTS {
            assert_eq!(
                admit(&s, B, &req(&format!("id-{i}")), "thread/read"),
                IdAdmission::Admitted
            );
        }
        assert_eq!(
            admit(&s, B, &req("more"), "thread/read"),
            IdAdmission::AtCapacity
        );
        s.observe_server_frame(B, r#"{"id":"id-0","result":{}}"#);
        assert_eq!(
            admit(&s, B, &req("more"), "thread/read"),
            IdAdmission::Admitted
        );
    }

    #[test]
    fn the_tracked_connection_table_is_bounded_and_released_on_close() {
        let s = SessionThreads::new();
        for i in 0..MAX_TRACKED_CONNECTIONS {
            assert_eq!(
                admit(&s, ConnId(i as u64), &req("1"), "thread/read"),
                IdAdmission::Admitted
            );
        }
        let overflow = ConnId(MAX_TRACKED_CONNECTIONS as u64);
        assert_eq!(
            admit(&s, overflow, &req("1"), "thread/read"),
            IdAdmission::AtCapacity
        );
        assert!(!s.has_tracked_connection(overflow));
        s.close_connection(ConnId(0));
        assert_eq!(
            admit(&s, overflow, &req("1"), "thread/read"),
            IdAdmission::Admitted
        );
    }

    #[test]
    fn no_threads_binds_nothing() {
        assert_eq!(NoThreads.bound_thread(), None);
        assert!(!NoThreads.is_session_thread("anything"));
        assert!(!NoThreads.is_active_turn("a", "b"));
        assert_eq!(
            NoThreads.try_admit_request(K, &req("x"), "thread/read"),
            IdAdmission::Admitted
        );
        assert!(matches!(
            NoThreads.try_admit_idle_turn(K, &req("x"), "t", Some(&Value::Null), None),
            TurnAdmission::NotTheHead { .. }
        ));
    }

    /// **The TUI's title thread is not a head move.** Captured on 0.153.4: on the first
    /// user turn the TUI starts an `ephemeral` thread, runs one turn on it and unsubscribes
    /// (`fixtures/codex/title-thread-0.153.4.jsonl`). The head stays where the keyboard is.
    #[test]
    fn the_tuis_title_thread_is_not_a_head_move() {
        const CAPTURE: &str = include_str!("../../../fixtures/codex/title-thread-0.153.4.jsonl");
        let s = on("01a0-head");
        for line in CAPTURE.lines() {
            let row: Value = serde_json::from_str(line).expect("a captured row");
            let text = row["frame"].to_string();
            if row["dir"] == "c2s" {
                s.observe_tui_request(K, &classify_shape(&WsPayload::Text(text)));
            } else {
                s.observe_server_frame(K, &text);
            }
        }
        assert_eq!(s.bound_thread(), Some("01a0-head".into()));
        assert!(!s.is_session_thread("01a0d141-880a-7281-b327-d132d57d3144"));
        assert_eq!(phone(&s, "p", "01a0-head"), TurnAdmission::Admitted);
    }

    /// **`/side` is not a head move.** Captured on 0.153.4: after a turn on the head, the
    /// TUI forks the head with `"ephemeral": true`, the app-server announces the fork, and
    /// the user's side message is a `turn/start` on the fork; the TUI never unsubscribes
    /// from the head (`fixtures/codex/side-fork-0.153.4.jsonl`, which ends with the side
    /// turn running). The head stays on the thread the side conversation forked from.
    #[test]
    fn a_side_conversation_is_not_a_head_move() {
        const CAPTURE: &str = include_str!("../../../fixtures/codex/side-fork-0.153.4.jsonl");
        const MAIN: &str = "01a0d16d-8722-7a41-9b89-26cd48f0299e";
        const SIDE: &str = "01a0d16e-87f6-7f92-8bd7-56e7611d0338";
        let s = on(MAIN);
        for line in CAPTURE.lines() {
            let row: Value = serde_json::from_str(line).expect("a captured row");
            let text = row["frame"].to_string();
            if row["dir"] == "c2s" {
                s.observe_tui_request(K, &classify_shape(&WsPayload::Text(text)));
            } else {
                s.observe_server_frame(K, &text);
            }
        }
        assert_eq!(s.bound_thread(), Some(MAIN.into()));
        assert!(!s.is_session_thread(SIDE));
        assert!(matches!(
            phone(&s, "side", SIDE),
            TurnAdmission::NotTheHead { .. }
        ));
        assert_eq!(phone(&s, "main", MAIN), TurnAdmission::Admitted);
    }

    /// **A turn whose end one leg saw before the keyboard's answer to it stays ended.**
    #[test]
    fn a_turn_answered_after_its_terminal_is_not_running() {
        let s = on("01a0");
        keyboard(&s, K, "turn/start", "k", json!({"threadId": "01a0"}));
        s.observe_server_frame(B, &terminal("01a0", "t1", "completed"));
        s.observe_server_frame(K, &turn_answer("k", "t1"));
        s.observe_server_frame(K, &terminal("01a0", "t1", "completed"));
        assert!(!s.is_active_turn("01a0", "t1"));
        assert_eq!(phone(&s, "p", "01a0"), TurnAdmission::Admitted);
    }
}
