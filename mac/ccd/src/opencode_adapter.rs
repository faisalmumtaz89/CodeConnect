//! OpenCode bus events → agent-neutral event normalization.
//!
//! The observation half of OpenCode support. It takes what the CodeConnect
//! plugin forwards from an OpenCode TUI — bus events as [`LinkFrame`]s, and a
//! snapshot of the server's history as a [`Snapshot`] — and turns them into
//! [`PendingEvent`]s in the turn → item model the phone already reads. Nothing
//! here talks to a socket, a clock or the daemon: it is a pure mapper, and
//! [`crate::opencode_link`] owns the frames it is fed.
//!
//! The rules were measured on OpenCode 1.18.34 against a scripted model; the
//! captures are in `fixtures/opencode/s8-*`, and the replay tests below hold
//! this module to them.
//!
//! ## Turns
//!
//! A turn is one busy period of a root session, named by the user message that
//! started it. OpenCode itself has no turn: its `session.idle` ends the busy
//! period, and a message typed while the agent works joins the running turn.
//! The plugin forwards no turn of its own, so these boundaries are the daemon's
//! alone. Child sessions (subagents) belong to their root's turn, and their
//! facts carry `subagent_session`.
//!
//! A turn ends in exactly one way:
//!
//!   * **TurnComplete** at the root's idle, once every assistant message of the
//!     turn has completed (after an abort or a provider error, idle comes before
//!     the in-flight message's terminal, so the close waits for it);
//!   * **outcome unknown** when the OpenCode process ends with the turn open
//!     ([`OpencodeAdapter::session_end`]): no TurnComplete, no tool result and no
//!     usage are written, because nothing observed says how it ended. Only a
//!     later snapshot of the persisted session that shows every assistant
//!     message completed and no tool running closes it, with its real outcome.
//!     A snapshot leaves a turn this adapter never saw outcome-unknown the
//!     same way when a reply of it never completed and a later turn began
//!     after it: the reply was abandoned when its process ended. It does the
//!     same when what is persisted cannot say how a turn ended: its prompt got
//!     no reply, or a prompt was written in the millisecond its run may have
//!     ended.
//!
//! A lost link closes nothing: the open turns wait for the snapshot the next
//! connection brings. A snapshot reads a busy period where OpenCode's own run
//! loop draws one: a user message, typed or written by OpenCode, begins a turn
//! when the run before it had ended — on the loop's exit test, an error, an
//! abort, a rejected permission, or a `!command`'s end.
//!
//! ## Order
//!
//! OpenCode ids ascend in creation order across messages, parts and requests,
//! and a snapshot is read in that order. Live, a part's terminal can arrive
//! before the terminal of a text part created earlier in the same turn, so a fact
//! is **held** while an earlier-created text or reasoning part of its turn is
//! still streaming (the plugin forwards a text-less start marker for each such
//! part), and released when that part ends. Cards are never held: they are
//! actionable, and a held fact exists only in memory until it is released.
//!
//! ## Snapshots
//!
//! [`OpencodeAdapter::plan_resync`] reads a snapshot into the same facts, and
//! returns them with the rebuilt adapter; [`OpencodeAdapter::apply_resync`] then
//! swaps the rebuilt state in. The split lets the caller make the facts durable
//! before anything here forgets what it had open, as `codex_adapter.rs` does
//! for a resume answer. A finished turn is described by its real parts and
//! closed with its real outcome; a busy turn continues under the same turn id;
//! nothing is fabricated — a tool the server still shows running gets no
//! result, the text of a message that ended in an abort is not read (whether it
//! was cut cannot be told from a snapshot), and an approval answered while the
//! link was down is not invented.
//!
//! ## Keys
//!
//! Every fact is `Source::Opencode` with the source event id
//! `<OpenCode session>:<suffix>`, and the daemon's dedup is first-wins on it, so
//! a fact re-read from a snapshot or after a daemon restart costs nothing.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use protocol::event::{EventKind, PendingEvent, SessionKey, Source};
use protocol::ws::{
    AnswerDecision, ApprovalCard, ClearCause, CodexResolution, CodexResolutionPayload,
    ResolutionActor,
};
use serde_json::{json, Map, Value};

use crate::opencode_link::{
    CardEnv, CardIds, LinkFrame, RequestBody, RequestKind, SnapMessage, SnapPart, SnapRequest,
    SnapSession, Snapshot, StubIds, SyncScope,
};

/// The error a message ends with when the user stopped it.
const ABORT: &str = "MessageAbortedError";
/// The error that makes OpenCode compact and carry on inside the same turn.
const OVERFLOW: &str = "ContextOverflowError";

/// The names the phone already draws a tool by, for OpenCode's built-in tools.
fn display_name(tool: &str) -> &str {
    match tool {
        "bash" => "Bash",
        "read" => "Read",
        "edit" => "Edit",
        "write" => "Write",
        "glob" => "Glob",
        "grep" => "Grep",
        "webfetch" => "WebFetch",
        "websearch" => "WebSearch",
        "task" => "Task",
        "todowrite" => "TodoWrite",
        other => other,
    }
}

fn str_of<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

fn obj<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    map.get(key).and_then(Value::as_object)
}

/// A JSON value read as a condition: absent, null, false, zero and empty are
/// all "no".
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn completed(info: &Map<String, Value>) -> bool {
    truthy(obj(info, "time").and_then(|t| t.get("completed")))
}

fn created(info: &Map<String, Value>) -> f64 {
    obj(info, "time")
        .and_then(|t| t.get("created"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

fn error_name(info: &Map<String, Value>) -> Option<&str> {
    obj(info, "error").and_then(|e| str_of(e, "name"))
}

/// A text part with no `time` is a user's: OpenCode creates those complete,
/// and every assistant text part carries `time.start`.
fn is_user_text(part: &Map<String, Value>) -> bool {
    str_of(part, "type") == Some("text") && !part.contains_key("time")
}

fn ended(part: &Map<String, Value>) -> bool {
    obj(part, "time").is_some_and(|t| !t.get("end").unwrap_or(&Value::Null).is_null())
}

/// Text the model is shown and the user never typed: subtask and file-read
/// expansions, compaction prompts.
fn is_filler(part: &Map<String, Value>) -> bool {
    truthy(part.get("synthetic")) || truthy(part.get("ignored"))
}

/// The ordering key of an id: OpenCode ids are a prefix, `_`, then a
/// creation-ordered body. An id-less fact sorts after every id.
fn order_key(id: Option<&str>) -> String {
    id.and_then(|id| id.split_once('_'))
        .map_or_else(|| "~".to_string(), |(_, body)| body.to_string())
}

/// A sum of JSON numbers that stays an integer until a fraction joins it.
#[derive(Default)]
struct Sum {
    int: i64,
    float: f64,
    fractional: bool,
}

impl Sum {
    fn add(&mut self, value: Option<&Value>) {
        match value {
            Some(Value::Number(n)) if n.is_f64() => {
                self.float += n.as_f64().unwrap_or(0.0);
                self.fractional = true;
            }
            Some(Value::Number(n)) => {
                // A count past i64 is held at its ceiling, not wrapped.
                let n = n.as_i64().unwrap_or(if n.is_u64() { i64::MAX } else { 0 });
                self.int = self.int.saturating_add(n);
            }
            _ => {}
        }
    }

    fn value(&self) -> Value {
        if self.fractional {
            json!(self.int as f64 + self.float)
        } else {
            json!(self.int)
        }
    }
}

/// A fact as a handler made it. A released fact was already held once and
/// has waited its turn; a fresh one may still have to.
enum Emit {
    Fresh(PendingEvent),
    Released(PendingEvent),
}

#[derive(Debug, Clone)]
struct Held {
    key: String,
    n: u64,
    event: PendingEvent,
}

/// A tool whose call was emitted and whose terminal has not been seen.
#[derive(Debug, Clone)]
struct Running {
    part_id: String,
    session: String,
    turn: Option<String>,
    part: Map<String, Value>,
}

#[derive(Debug, Clone)]
struct OpenCard {
    request_id: String,
    session: String,
    kind: RequestKind,
    /// The tool call the request named, shared by the cards of one call.
    call_id: Option<String>,
    /// When the card's content was computed, for a rebuild of the same open
    /// request to reuse. `None` for a card restored from the log or shown
    /// without being offered.
    computed_at: Option<String>,
}

/// Normalizes one OpenCode run's link into the neutral event model. Construct
/// it with the daemon's [`SessionKey`] for the run; feed it frames in arrival
/// order.
#[derive(Debug, Clone)]
pub(crate) struct OpencodeAdapter {
    session: SessionKey,
    /// Child session → parent session.
    parent: HashMap<String, String>,
    /// Root session → the turn of its running busy period.
    current: BTreeMap<String, String>,
    /// Message → its turn; `None` for a message seen with no turn to join.
    msg_turn: HashMap<String, Option<String>>,
    msg_role: HashMap<String, String>,
    /// Assistant messages that are a compaction summary.
    msg_summary: HashSet<String>,
    /// Turn → its first error that is neither an abort nor a context overflow.
    error: HashMap<String, String>,
    /// Turn → step-finish part → (tokens, cost), root steps only.
    usage: HashMap<String, BTreeMap<String, (Value, Option<Value>)>>,
    /// Turns whose TurnComplete was emitted.
    closed: HashSet<String>,
    /// Turn → its assistant messages not yet completed.
    open_msgs: HashMap<String, BTreeSet<String>>,
    done_msgs: HashSet<String>,
    /// Turns whose root went idle while a message of theirs was still open,
    /// with that root, in the order they went idle.
    idle_pending: Vec<(String, String)>,
    turn_root: HashMap<String, String>,
    /// Turn → the sessions whose own abort was seen in it.
    aborted: HashMap<String, HashSet<String>>,
    /// Turns in which a root assistant message ended in an abort.
    root_abort: HashSet<String>,
    /// Request → the turn it was asked in.
    request_turn: HashMap<String, Option<String>>,
    running: Vec<Running>,
    /// (session, turn) pairs whose error fact was made.
    error_seen: HashSet<(String, String)>,
    /// Turns open when the process ended: their outcome is unknown.
    unknown: BTreeSet<String>,
    /// Turn → text/reasoning part still streaming → (ordering key, message).
    streaming: HashMap<String, BTreeMap<String, (String, String)>>,
    /// Turn → facts waiting behind an earlier part that is still streaming.
    held: HashMap<String, Vec<Held>>,
    held_count: u64,
    open_cards: Vec<OpenCard>,
    /// The cards open when the current snapshot began. Only these can be
    /// cleared by it: a card asked after that is newer than the snapshot.
    open_at_sync: Option<HashSet<String>>,
    /// Cleared at `settled`: open cards the snapshot did not list as pending.
    clear_due: Vec<String>,
    /// Root → the last agent and model a user message was sent with, as the
    /// line a change says.
    selection: HashMap<String, String>,
    selection_seen: HashSet<String>,
    /// Roots with a message this adapter could not put in a turn since the
    /// last full snapshot.
    unassigned: HashSet<String>,
    resync_wanted: bool,
    /// Turns a snapshot left open whose end will prove nothing: they end
    /// outcome-unknown.
    unproven: HashMap<String, Unproven>,
    /// The Mac a card is built on.
    env: Arc<dyn CardEnv>,
    /// The plugin activation cards are stamped with.
    activation: u64,
    /// Requests resolved before any card of them was raised, oldest first.
    retired: VecDeque<String>,
}

/// Why a turn a snapshot left open cannot be closed with an outcome.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Unproven {
    /// Whether a prompt joined it or began a turn of its own could not be
    /// told.
    Start,
    /// It has no reply yet, and how a run that never replied ended is not
    /// persisted. A reply clears it.
    Reply,
    /// The same, on a root idle at the snapshot and not busy since. OpenCode
    /// writes a prompt before it marks the root busy, so the run may still
    /// start; if the next prompt comes first, that run never started and the
    /// prompt begins a turn of its own.
    Idle,
    /// Its run had ended on a root still busy: that busy period is the run's
    /// last moment or the next one's first, which a `!command` begins before
    /// it writes its prompt (`effect/runner.ts` startShell). An idle next
    /// ends the turn as seen; a prompt next could have joined it or begun a
    /// turn of its own, so it joins and the turn becomes `Start`.
    Over,
}

impl OpencodeAdapter {
    pub(crate) fn new(session: SessionKey, env: Arc<dyn CardEnv>) -> Self {
        OpencodeAdapter {
            session,
            parent: HashMap::new(),
            current: BTreeMap::new(),
            msg_turn: HashMap::new(),
            msg_role: HashMap::new(),
            msg_summary: HashSet::new(),
            error: HashMap::new(),
            usage: HashMap::new(),
            closed: HashSet::new(),
            open_msgs: HashMap::new(),
            done_msgs: HashSet::new(),
            idle_pending: Vec::new(),
            turn_root: HashMap::new(),
            aborted: HashMap::new(),
            root_abort: HashSet::new(),
            request_turn: HashMap::new(),
            running: Vec::new(),
            error_seen: HashSet::new(),
            unknown: BTreeSet::new(),
            streaming: HashMap::new(),
            held: HashMap::new(),
            held_count: 0,
            open_cards: Vec::new(),
            open_at_sync: None,
            clear_due: Vec::new(),
            selection: HashMap::new(),
            selection_seen: HashSet::new(),
            unassigned: HashSet::new(),
            resync_wanted: false,
            unproven: HashMap::new(),
            env,
            activation: 0,
            retired: VecDeque::new(),
        }
    }

    // ------------------------------------------------------------ entry points

    /// Normalize one link frame. Bus events, oversized parts and oversized
    /// cards map to facts; every other frame is the link's business and maps
    /// to none. A malformed event yields nothing and changes nothing it did not
    /// read.
    pub(crate) fn ingest(&mut self, frame: &LinkFrame) -> Vec<PendingEvent> {
        let out = match frame {
            LinkFrame::Ev {
                event, properties, ..
            } => self.on_event(event, properties),
            LinkFrame::Stub {
                event,
                ids,
                part_type,
                status,
                size,
                sha256,
                ..
            } if event == "message.part.updated" => {
                self.on_part_stub(ids, part_type.as_deref(), status.as_deref(), *size, sha256)
            }
            LinkFrame::CardStub {
                event,
                properties,
                size,
                sha256,
                ..
            } => match event.as_str() {
                "permission.asked" => {
                    self.on_card_stub(RequestKind::Permission, properties, *size, sha256)
                }
                "question.asked" => {
                    self.on_card_stub(RequestKind::Question, properties, *size, sha256)
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        self.route(out)
    }

    /// A card the store holds open, told to an adapter that has not seen it
    /// asked — the daemon restarted — so a snapshot can still clear it, in the
    /// turn it was asked in.
    pub(crate) fn restore_open_card(
        &mut self,
        request_id: &str,
        session: &str,
        kind: RequestKind,
        turn: Option<String>,
    ) {
        self.request_turn
            .entry(request_id.to_string())
            .or_insert(turn);
        if !self.open_cards.iter().any(|c| c.request_id == request_id) {
            self.open_cards.push(OpenCard {
                request_id: request_id.to_string(),
                session: session.to_string(),
                kind,
                call_id: None,
                computed_at: None,
            });
        }
    }

    /// The last agent and model a root's prompts were sent with, as the store
    /// recorded it, told to an adapter that has not seen them — the daemon
    /// restarted — so the next prompt is compared with it.
    pub(crate) fn restore_selection(&mut self, root: &str, line: &str) {
        self.selection
            .entry(root.to_string())
            .or_insert_with(|| line.to_string());
    }

    /// Whether the agent and model of `root`'s last prompt are known, so its
    /// next prompt can be compared with them.
    pub(crate) fn knows_selection(&self, root: &str) -> bool {
        self.selection.contains_key(root)
    }

    /// A snapshot is beginning (`sync_begin`). The cards open now are the only
    /// ones its `settled` may clear.
    pub(crate) fn sync_begin(&mut self) {
        self.open_at_sync = Some(
            self.open_cards
                .iter()
                .map(|c| c.request_id.clone())
                .collect(),
        );
    }

    /// The snapshot's `settled`: every frame the plugin held up to the
    /// snapshot's end has been delivered, so a reply that arrived meanwhile has
    /// been seen. Each card that was open when the snapshot began, is still
    /// open, and was not listed as pending is cleared as superseded.
    pub(crate) fn settle(&mut self) -> Vec<PendingEvent> {
        let mut out = Vec::new();
        for request_id in std::mem::take(&mut self.clear_due) {
            let Some(at) = self
                .open_cards
                .iter()
                .position(|c| c.request_id == request_id)
            else {
                continue;
            };
            let card = self.open_cards.remove(at);
            let resolution = CodexResolution::Cleared {
                cause: ClearCause::Superseded,
            };
            let turn = self.request_turn.get(&request_id).cloned().flatten();
            out.push(self.resolution(&card.session, card.kind, &request_id, resolution, turn));
        }
        self.open_at_sync = None;
        self.route(out)
    }

    /// The OpenCode process has ended. Facts held only for their order are
    /// real and are released; every open turn becomes outcome-unknown, and no
    /// TurnComplete, tool result or usage is made for it. Every open card is
    /// retired as ended: the request it was about ended with the process.
    pub(crate) fn session_end(&mut self) -> Vec<PendingEvent> {
        let mut out = self.close_all();
        let open: Vec<String> = self
            .open_cards
            .iter()
            .map(|c| c.request_id.clone())
            .collect();
        for request_id in open {
            out.extend(self.ended(&request_id));
        }
        self.route(out)
    }

    /// Whether a full snapshot is needed: a root went idle without closing a
    /// turn while it had messages this adapter could not put in a turn. Asking
    /// clears the request.
    pub(crate) fn take_resync_request(&mut self) -> bool {
        std::mem::take(&mut self.resync_wanted)
    }

    /// **Read a snapshot without changing anything.** Returns the facts it
    /// describes, in timeline order, and the adapter rebuilt from it; `None`
    /// refuses the snapshot and nothing changes.
    ///
    /// Refused when it is not whole: a status or session list that failed, a
    /// status this build cannot read, or a history the plugin could not page
    /// completely. A page item over the cap is not a refusal: that part yields
    /// no facts, and a tool part kept only as a stub still counts as running
    /// unless the stub says it ended.
    ///
    /// A session whose history arrived incomplete is set aside with its whole
    /// session tree: nothing is read of it, no turn of it closes, and none of
    /// its cards is cleared. The other sessions are read as usual.
    ///
    /// A request list that failed refuses nothing, but then no card is cleared
    /// by this snapshot.
    pub(crate) fn plan_resync(&self, snap: &Snapshot) -> Option<(Vec<PendingEvent>, Self)> {
        if snap.broken.is_some() || !snap.status_ok || !snap.list_ok {
            return None;
        }
        let readable = snap.status.values().all(|s| {
            matches!(
                s.get("type").and_then(Value::as_str),
                Some("busy" | "retry" | "idle")
            )
        });
        if !readable {
            return None;
        }
        let mut plan = Resync::new(self, snap);
        let mut out = plan.sessions();
        for request in &snap.requests {
            let set_aside = request.session().is_some_and(|s| plan.set_aside(s));
            if !set_aside {
                out.extend(plan.ask(request));
            }
        }
        let listed: HashSet<&str> = snap.requests.iter().filter_map(SnapRequest::id).collect();
        let clear_due = match (&plan.m.open_at_sync, snap.requests_ok) {
            (Some(at_sync), true) => plan
                .m
                .open_cards
                .iter()
                .filter(|c| {
                    at_sync.contains(&c.request_id) && !listed.contains(c.request_id.as_str())
                })
                .filter(|c| !plan.set_aside(&c.session))
                .map(|c| c.request_id.clone())
                .collect(),
            _ => Vec::new(),
        };
        let skipped = std::mem::take(&mut plan.skipped);
        let mut staged = plan.m;
        staged.clear_due = clear_due;
        if snap.scope == SyncScope::Full {
            staged.unassigned.retain(|root| skipped.contains(root));
            staged.resync_wanted = false;
        }
        // One fact per key: the store keeps the first anyway.
        let mut keys = HashSet::new();
        let mut facts = staged.route(out);
        facts.retain(|f| keys.insert(f.source_event_id.clone()));
        Some((facts, staged))
    }

    /// Swap in the adapter [`plan_resync`](Self::plan_resync) rebuilt. Call it
    /// only once the planned facts are durable: what this adapter held open
    /// lives on only in them.
    pub(crate) fn apply_resync(&mut self, staged: Self) {
        *self = staged;
    }

    // ---------------------------------------------------------------- facts

    fn root<'a>(&'a self, session: &'a str) -> &'a str {
        let mut seen = HashSet::new();
        let mut at = session;
        while let Some(parent) = self.parent.get(at) {
            if !seen.insert(at) {
                break;
            }
            at = parent;
        }
        at
    }

    fn fact(
        &self,
        kind: EventKind,
        session: &str,
        suffix: &str,
        mut payload: Value,
        turn: Option<&str>,
        item: Option<&str>,
    ) -> Emit {
        if session != self.root(session) {
            payload["subagent_session"] = json!(session);
        }
        Emit::Fresh(
            PendingEvent::new(&self.session, kind, payload, Source::Opencode)
                .with_source_event_id(format!("{session}:{suffix}"))
                .with_turn_id(turn.map(str::to_string))
                .with_item_id(item.map(str::to_string)),
        )
    }

    fn other(name: &str) -> EventKind {
        EventKind::Other(name.to_string())
    }

    /// Holds each fresh fact that an earlier-created streaming part of its turn
    /// must precede. Cards, released facts and turn-less facts pass.
    fn route(&mut self, emits: Vec<Emit>) -> Vec<PendingEvent> {
        let mut out = Vec::new();
        for emit in emits {
            let event = match emit {
                Emit::Released(event) => {
                    out.push(event);
                    continue;
                }
                Emit::Fresh(event) => event,
            };
            let card = matches!(
                event.kind,
                EventKind::ApprovalRequest | EventKind::ApprovalResolved
            );
            let Some(turn) = event.turn_id.clone().filter(|_| !card) else {
                out.push(event);
                continue;
            };
            let key = order_key(event.item_id.as_deref());
            if self.blocked(&turn, &key) {
                self.held_count += 1;
                let n = self.held_count;
                self.held
                    .entry(turn)
                    .or_default()
                    .push(Held { key, n, event });
            } else {
                out.push(event);
            }
        }
        out
    }

    fn blocked(&self, turn: &str, key: &str) -> bool {
        self.streaming
            .get(turn)
            .is_some_and(|parts| parts.values().any(|(k, _)| k.as_str() < key))
    }

    /// The held facts of `turn` that nothing earlier blocks any more — all of
    /// them when `everything` — in creation order.
    fn release(&mut self, turn: &str, everything: bool) -> Vec<Emit> {
        let Some(mut held) = self.held.remove(turn) else {
            return Vec::new();
        };
        held.sort_by(|a, b| (&a.key, a.n).cmp(&(&b.key, b.n)));
        let (keep, go): (Vec<Held>, Vec<Held>) = held
            .into_iter()
            .partition(|h| !everything && self.blocked(turn, &h.key));
        if !keep.is_empty() {
            self.held.insert(turn.to_string(), keep);
        }
        go.into_iter().map(|h| Emit::Released(h.event)).collect()
    }

    /// A streaming part ended, or every part of a completed message did.
    fn part_closed(
        &mut self,
        turn: Option<&str>,
        part: Option<&str>,
        message: Option<&str>,
    ) -> Vec<Emit> {
        let Some(turn) = turn else {
            return Vec::new();
        };
        if let Some(parts) = self.streaming.get_mut(turn) {
            parts.retain(|id, (_, mid)| Some(id.as_str()) != part && Some(mid.as_str()) != message);
            if parts.is_empty() {
                self.streaming.remove(turn);
            }
        }
        self.release(turn, false)
    }

    fn turn_of_msg(&self, message: &str, session: &str) -> Option<String> {
        self.msg_turn
            .get(message)
            .cloned()
            .flatten()
            .or_else(|| self.current.get(self.root(session)).cloned())
    }

    fn running_in(&self, turn: &str) -> bool {
        self.running.iter().any(|r| r.turn.as_deref() == Some(turn))
    }

    fn set_idle_pending(&mut self, turn: &str, root: &str) {
        match self.idle_pending.iter_mut().find(|(t, _)| t == turn) {
            Some(entry) => entry.1 = root.to_string(),
            None => self.idle_pending.push((turn.to_string(), root.to_string())),
        }
    }

    // ------------------------------------------------------------ turn ends

    /// Close `turn` with its outcome. `forced` names what ended it when its
    /// own idle cannot: its running tools then get an interrupted result.
    fn terminal(&mut self, session: &str, turn: &str, forced: Option<&str>) -> Vec<Emit> {
        self.idle_pending.retain(|(t, _)| t != turn);
        self.open_msgs.remove(turn);
        self.unknown.remove(turn);
        self.closed.insert(turn.to_string());
        self.streaming.remove(turn);
        // Nothing of a turn may follow its TurnComplete.
        let mut out = self.release(turn, true);
        if let Some(reason) = forced {
            let tools: Vec<Running> = self
                .running
                .iter()
                .filter(|r| r.turn.as_deref() == Some(turn))
                .cloned()
                .collect();
            for tool in tools {
                out.push(self.tool_closed(&tool, turn, reason));
            }
        }
        if let Some(steps) = self.usage.get(turn) {
            let mut sums: [Sum; 6] = Default::default();
            for (tokens, cost) in steps.values() {
                let tokens = tokens.as_object();
                let cache = tokens.and_then(|t| obj(t, "cache"));
                sums[0].add(tokens.and_then(|t| t.get("input")));
                sums[1].add(tokens.and_then(|t| t.get("output")));
                sums[2].add(tokens.and_then(|t| t.get("reasoning")));
                sums[3].add(cache.and_then(|c| c.get("read")));
                sums[4].add(cache.and_then(|c| c.get("write")));
                sums[5].add(cost.as_ref());
            }
            let payload = json!({
                "input": sums[0].value(), "output": sums[1].value(), "reasoning": sums[2].value(),
                "cache_read": sums[3].value(), "cache_write": sums[4].value(), "cost": sums[5].value(),
                "steps": steps.len(),
            });
            out.push(self.fact(
                EventKind::Usage,
                session,
                &format!("usage:{turn}"),
                payload,
                Some(turn),
                None,
            ));
        }
        let cut = self.aborted.get(turn).is_some_and(|s| s.contains(session))
            || self.root_abort.contains(turn);
        let status = if forced.is_some() || cut {
            "interrupted"
        } else if self.error.contains_key(turn) {
            "failed"
        } else {
            "completed"
        };
        let mut payload = json!({"status": status, "error": self.error.get(turn)});
        if let Some(reason) = forced {
            payload["closed_by"] = json!(reason);
        }
        out.push(self.fact(
            EventKind::TurnComplete,
            session,
            &format!("turn:{turn}"),
            payload,
            Some(turn),
            None,
        ));
        out
    }

    /// A child going idle is not a turn boundary. A root's idle closes its turn,
    /// or defers the close while a message of the turn is still open.
    fn close_turn(&mut self, session: &str) -> Vec<Emit> {
        if session != self.root(session) {
            return Vec::new();
        }
        let turn = match self.current.remove(session) {
            Some(turn) if !self.closed.contains(&turn) => turn,
            _ => {
                if self.unassigned.contains(session) {
                    self.resync_wanted = true;
                }
                return Vec::new();
            }
        };
        match self.unproven.remove(&turn) {
            None | Some(Unproven::Over) => {}
            Some(_) => return self.unknown_end(&turn),
        }
        if self.open_msgs.get(&turn).is_some_and(|m| !m.is_empty()) {
            self.set_idle_pending(&turn, session);
            return Vec::new();
        }
        self.terminal(session, &turn, None)
    }

    /// Every open turn becomes outcome-unknown. Its running tools stay
    /// registered, so a later snapshot of the persisted session can still give
    /// them their real result.
    fn close_all(&mut self) -> Vec<Emit> {
        let mut turns: Vec<String> = self.current.values().cloned().collect();
        turns.extend(self.idle_pending.iter().map(|(t, _)| t.clone()));
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for turn in turns {
            if !seen.insert(turn.clone()) || self.closed.contains(&turn) {
                continue;
            }
            out.extend(self.unknown_end(&turn));
        }
        self.current.clear();
        self.idle_pending.clear();
        self.open_msgs.clear();
        out
    }

    /// `turn` ends outcome-unknown: what it holds back is released, and no
    /// TurnComplete, tool result or usage is made for it.
    fn unknown_end(&mut self, turn: &str) -> Vec<Emit> {
        self.unknown.insert(turn.to_string());
        self.unproven.remove(turn);
        self.streaming.remove(turn);
        self.release(turn, true)
    }

    // ------------------------------------------------------------- bus events

    fn on_event(&mut self, event: &str, p: &Map<String, Value>) -> Vec<Emit> {
        match event {
            "session.created" => {
                obj(p, "info").map_or_else(Vec::new, |i| self.on_session_created(i))
            }
            "session.status" => self.on_session_status(p),
            "session.idle" => str_of(p, "sessionID").map_or_else(Vec::new, |s| self.close_turn(s)),
            "session.error" => self.on_session_error(p),
            // Measured: a reload or a disposal aborts in-band first, so this
            // finds nothing open. A turn still open here becomes
            // outcome-unknown, as at an exit.
            "server.instance.disposed" => self.close_all(),
            "message.updated" => {
                obj(p, "info").map_or_else(Vec::new, |i| self.on_message_updated(i))
            }
            "message.removed" => self.on_message_removed(p),
            "message.part.updated" => {
                obj(p, "part").map_or_else(Vec::new, |pt| self.on_part_updated(pt))
            }
            "message.part.removed" => self.on_part_removed(p),
            "permission.asked" => self.on_asked(RequestKind::Permission, p),
            "question.asked" => self.on_asked(RequestKind::Question, p),
            "permission.v2.asked" | "question.v2.asked" => self.on_hidden_asked(p),
            "permission.replied" => self.on_replied(RequestKind::Permission, p),
            "question.replied" | "question.rejected" => self.on_replied(RequestKind::Question, p),
            _ => Vec::new(),
        }
    }

    fn on_session_created(&mut self, info: &Map<String, Value>) -> Vec<Emit> {
        let Some(id) = str_of(info, "id") else {
            return Vec::new();
        };
        if let Some(parent) = str_of(info, "parentID") {
            self.parent.insert(id.to_string(), parent.to_string());
            return Vec::new();
        }
        let payload = json!({
            "opencode_session": id,
            "cwd": info.get("directory"),
            "version": info.get("version"),
        });
        vec![self.fact(
            EventKind::SessionStart,
            id,
            "session_start",
            payload,
            None,
            None,
        )]
    }

    fn on_session_status(&mut self, p: &Map<String, Value>) -> Vec<Emit> {
        let (Some(session), Some(status)) = (str_of(p, "sessionID"), obj(p, "status")) else {
            return Vec::new();
        };
        match str_of(status, "type") {
            Some("retry") => {
                let Some(turn) = self.current.get(self.root(session)).cloned() else {
                    return Vec::new();
                };
                let attempt = status.get("attempt").cloned().unwrap_or(Value::Null);
                let message = format!(
                    "{} (retry {attempt})",
                    str_of(status, "message").unwrap_or_default()
                );
                let payload = json!({
                    "notification_type": "provider_retry", "attempt": attempt, "message": message,
                });
                let suffix = format!("retry:{turn}:{attempt}");
                vec![self.fact(
                    EventKind::Notification,
                    session,
                    &suffix,
                    payload,
                    Some(&turn),
                    None,
                )]
            }
            Some("idle") => self.close_turn(session),
            Some("busy") => {
                if let Some(turn) = self.current.get(session) {
                    if let Some(u @ Unproven::Idle) = self.unproven.get_mut(turn) {
                        *u = Unproven::Reply;
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn on_session_error(&mut self, p: &Map<String, Value>) -> Vec<Emit> {
        let Some(session) = str_of(p, "sessionID") else {
            return Vec::new();
        };
        let error = obj(p, "error");
        let name = error
            .and_then(|e| str_of(e, "name"))
            .unwrap_or("UnknownError");
        let turn = self.current.get(self.root(session)).cloned();
        if name == ABORT {
            // Scoped to the session that was aborted: a child's abort never
            // marks its root.
            if let Some(turn) = turn {
                self.aborted
                    .entry(turn)
                    .or_default()
                    .insert(session.to_string());
            }
            return Vec::new();
        }
        // An overflow is followed by a compaction inside the same turn.
        let Some(turn) = turn.filter(|_| name != OVERFLOW) else {
            return Vec::new();
        };
        self.error_fact(session, &turn, name, error.and_then(|e| obj(e, "data")))
    }

    fn error_fact(
        &mut self,
        session: &str,
        turn: &str,
        name: &str,
        data: Option<&Map<String, Value>>,
    ) -> Vec<Emit> {
        self.error_seen
            .insert((session.to_string(), turn.to_string()));
        let message = data
            .and_then(|d| d.get("message"))
            .cloned()
            .unwrap_or(json!(name));
        let suffix = format!("error:{turn}");
        if session != self.root(session) {
            // A child's error is the child's; its Task result carries it.
            let payload = json!({"name": name, "message": message});
            return vec![self.fact(
                Self::other("opencode_subagent_error"),
                session,
                &suffix,
                payload,
                Some(turn),
                None,
            )];
        }
        self.error
            .entry(turn.to_string())
            .or_insert_with(|| name.to_string());
        let payload = json!({
            "name": name, "message": message,
            "status_code": data.and_then(|d| d.get("statusCode")),
            "retryable": data.and_then(|d| d.get("isRetryable")),
        });
        vec![self.fact(
            EventKind::Error,
            session,
            &suffix,
            payload,
            Some(turn),
            None,
        )]
    }

    fn on_message_updated(&mut self, info: &Map<String, Value>) -> Vec<Emit> {
        let (Some(id), Some(session), Some(role)) = (
            str_of(info, "id"),
            str_of(info, "sessionID"),
            str_of(info, "role"),
        ) else {
            return Vec::new();
        };
        self.msg_role.insert(id.to_string(), role.to_string());
        let mut out = Vec::new();
        if role == "user" {
            if self.msg_turn.contains_key(id) {
                return out;
            }
            let root = self.root(session).to_string();
            let never_ran = self
                .current
                .get(&root)
                .filter(|t| root == session && self.unproven.get(*t) == Some(&Unproven::Idle))
                .cloned();
            if let Some(turn) = never_ran {
                self.current.remove(&root);
                out.extend(self.unknown_end(&turn));
            }
            if let Some(turn) = self.current.get(&root).filter(|_| root == session) {
                if let Some(u @ Unproven::Over) = self.unproven.get_mut(turn) {
                    *u = Unproven::Start;
                }
            }
            if root == session && !self.current.contains_key(&root) {
                // A new busy period ends any close deferred on this root.
                let deferred: Vec<String> = self
                    .idle_pending
                    .iter()
                    .filter(|(_, r)| *r == root)
                    .map(|(t, _)| t.clone())
                    .collect();
                for turn in deferred {
                    let forced = self.running_in(&turn).then_some("next_turn");
                    out.extend(self.terminal(&root, &turn, forced));
                }
                let stale: Vec<String> = self
                    .open_msgs
                    .keys()
                    .filter(|t| self.turn_root.get(*t) == Some(&root))
                    .cloned()
                    .collect();
                for turn in stale {
                    self.open_msgs.remove(&turn);
                }
                self.current.insert(root.clone(), id.to_string());
                self.turn_root.insert(id.to_string(), root.clone());
            }
            let turn = self
                .current
                .get(&root)
                .cloned()
                .unwrap_or_else(|| id.to_string());
            self.msg_turn.insert(id.to_string(), Some(turn.clone()));
            if root == session {
                out.extend(self.selection_change(&root, id, info, &turn));
            }
            return out;
        }
        if info.get("summary") == Some(&Value::Bool(true)) {
            self.msg_summary.insert(id.to_string());
        }
        if !self.msg_turn.contains_key(id) {
            let by_parent =
                str_of(info, "parentID").and_then(|p| self.msg_turn.get(p).cloned().flatten());
            let turn = by_parent.or_else(|| self.current.get(self.root(session)).cloned());
            if turn.is_none() {
                let root = self.root(session).to_string();
                self.unassigned.insert(root);
            }
            self.msg_turn.insert(id.to_string(), turn);
        }
        let turn = self.msg_turn.get(id).cloned().flatten();
        if let Some(turn) = &turn {
            if matches!(
                self.unproven.get(turn),
                Some(Unproven::Reply | Unproven::Idle)
            ) {
                self.unproven.remove(turn);
            }
        }
        if completed(info) {
            self.done_msgs.insert(id.to_string());
            if let Some(turn) = &turn {
                if session == self.root(session) && error_name(info) == Some(ABORT) {
                    // The root's own abort, even when no session.error came.
                    self.root_abort.insert(turn.clone());
                }
                // The error it ended on, even when its session.error was lost.
                if let Some(name) = error_name(info).filter(|n| *n != ABORT && *n != OVERFLOW) {
                    if !self
                        .error_seen
                        .contains(&(session.to_string(), turn.clone()))
                    {
                        let data = obj(info, "error").and_then(|e| obj(e, "data"));
                        out.extend(self.error_fact(session, turn, name, data));
                    }
                }
                if let Some(open) = self.open_msgs.get_mut(turn) {
                    open.remove(id);
                }
            }
            // A completed message streams nothing more.
            out.extend(self.part_closed(turn.as_deref(), None, Some(id)));
            if let Some(turn) = turn {
                let deferred = self
                    .idle_pending
                    .iter()
                    .find(|(t, _)| *t == turn)
                    .map(|(_, r)| r.clone());
                if let Some(root) = deferred {
                    if self.open_msgs.get(&turn).is_none_or(BTreeSet::is_empty) {
                        out.extend(self.terminal(&root, &turn, None));
                    }
                }
            }
        } else if let Some(turn) = turn {
            if !self.done_msgs.contains(id) && !self.closed.contains(&turn) {
                self.open_msgs
                    .entry(turn)
                    .or_default()
                    .insert(id.to_string());
            }
        }
        out
    }

    /// The keyboard's agent or model changed since this root's last prompt.
    /// Said once per prompt, before the prompt itself; the first prompt seen
    /// only records the choice.
    fn selection_change(
        &mut self,
        root: &str,
        message: &str,
        info: &Map<String, Value>,
        turn: &str,
    ) -> Vec<Emit> {
        let model = obj(info, "model");
        let (Some(agent), Some(provider), Some(model_id)) = (
            str_of(info, "agent"),
            model.and_then(|m| str_of(m, "providerID")),
            model.and_then(|m| str_of(m, "modelID")),
        ) else {
            return Vec::new();
        };
        if !self.selection_seen.insert(message.to_string()) {
            return Vec::new();
        }
        let mut line = format!("{agent} · {provider}/{model_id}");
        if let Some(variant) = model.and_then(|m| str_of(m, "variant")) {
            line.push_str(&format!(" ({variant})"));
        }
        match self.selection.insert(root.to_string(), line.clone()) {
            Some(before) if before != line => {
                let payload = json!({"notification_type": "model_changed", "message": line});
                let suffix = format!("model:{message}");
                vec![self.fact(
                    EventKind::Notification,
                    root,
                    &suffix,
                    payload,
                    Some(turn),
                    Some(message),
                )]
            }
            _ => Vec::new(),
        }
    }

    fn on_message_removed(&mut self, p: &Map<String, Value>) -> Vec<Emit> {
        let (Some(session), Some(message)) = (str_of(p, "sessionID"), str_of(p, "messageID"))
        else {
            return Vec::new();
        };
        let turn = self.msg_turn.get(message).cloned().flatten();
        let payload = json!({"message_id": message});
        vec![self.fact(
            Self::other("opencode_message_removed"),
            session,
            &format!("removed:{message}"),
            payload,
            turn.as_deref(),
            Some(message),
        )]
    }

    fn on_part_removed(&mut self, p: &Map<String, Value>) -> Vec<Emit> {
        let (Some(session), Some(message), Some(part)) = (
            str_of(p, "sessionID"),
            str_of(p, "messageID"),
            str_of(p, "partID"),
        ) else {
            return Vec::new();
        };
        let turn = self.msg_turn.get(message).cloned().flatten();
        let payload = json!({"message_id": message, "part_id": part});
        vec![self.fact(
            Self::other("opencode_part_removed"),
            session,
            &format!("removed:{part}"),
            payload,
            turn.as_deref(),
            Some(part),
        )]
    }

    fn on_part_updated(&mut self, part: &Map<String, Value>) -> Vec<Emit> {
        let (Some(session), Some(message), Some(id), Some(kind)) = (
            str_of(part, "sessionID"),
            str_of(part, "messageID"),
            str_of(part, "id"),
            str_of(part, "type"),
        ) else {
            return Vec::new();
        };
        if kind == "text" && is_filler(part) {
            return Vec::new();
        }
        let assistant_text = matches!(kind, "text" | "reasoning")
            && self.msg_role.get(message).map(String::as_str) != Some("user")
            && !is_user_text(part);
        if !assistant_text {
            return self.part_facts(part, session, message, id, kind);
        }
        let turn = self.turn_of_msg(message, session);
        if !ended(part) {
            // A streaming start: later-created facts of the turn wait for it.
            if let Some(turn) = turn {
                if !self.closed.contains(&turn) && !self.done_msgs.contains(message) {
                    self.streaming
                        .entry(turn)
                        .or_default()
                        .insert(id.to_string(), (order_key(Some(id)), message.to_string()));
                }
            }
            return Vec::new();
        }
        let mut out = self.part_facts(part, session, message, id, kind);
        out.extend(self.part_closed(turn.as_deref(), Some(id), None));
        out
    }

    /// A part update over the plugin's frame cap: its ids, its type and its
    /// tool status, without its content. It is read as the update it stands
    /// for, with a note of its size and digest in place of what cannot be
    /// shown:
    ///
    ///   * a tool that ended gets its result, so it is no longer running;
    ///     a tool still running stays running;
    ///   * a text or reasoning part is the part's final update (the plugin
    ///     sends a text-less marker while one streams), so it is said, and
    ///     the facts waiting behind it go on.
    ///
    /// Any other part over the cap is dropped.
    fn on_part_stub(
        &mut self,
        ids: &StubIds,
        part_type: Option<&str>,
        status: Option<&str>,
        size: u64,
        sha256: &str,
    ) -> Vec<Emit> {
        let (Some(session), Some(message), Some(id), Some(kind)) = (
            ids.session_id.as_deref(),
            ids.message_id.as_deref(),
            ids.part_id.as_deref(),
            part_type,
        ) else {
            return Vec::new();
        };
        let note = |what: &str| {
            format!("This {what} is too large to show ({size} bytes, sha256 {sha256})")
        };
        let mut part = Map::new();
        part.insert("id".into(), json!(id));
        part.insert("sessionID".into(), json!(session));
        part.insert("messageID".into(), json!(message));
        part.insert("type".into(), json!(kind));
        match kind {
            "tool" => {
                let field = match status {
                    Some("completed") => "output",
                    Some("error") => "error",
                    _ => return Vec::new(),
                };
                let known = self
                    .running
                    .iter()
                    .find(|r| r.part_id == id)
                    .map(|r| &r.part);
                for key in ["tool", "callID"] {
                    if let Some(value) = known.and_then(|p| p.get(key)) {
                        part.insert(key.into(), value.clone());
                    }
                }
                let mut state = Map::new();
                state.insert("status".into(), json!(status));
                if let Some(input) = known
                    .and_then(|p| obj(p, "state"))
                    .and_then(|s| s.get("input"))
                {
                    state.insert("input".into(), input.clone());
                }
                state.insert(field.into(), json!(note("output")));
                part.insert("state".into(), Value::Object(state));
            }
            "text" | "reasoning" => {
                part.insert("text".into(), json!(note("text")));
                if self.msg_role.get(message).map(String::as_str) != Some("user") {
                    part.insert("time".into(), json!({"start": 0, "end": 0}));
                }
            }
            _ => return Vec::new(),
        }
        self.on_part_updated(&part)
    }

    fn part_facts(
        &mut self,
        part: &Map<String, Value>,
        session: &str,
        message: &str,
        id: &str,
        kind: &str,
    ) -> Vec<Emit> {
        let turn = self.turn_of_msg(message, session);
        if turn.is_none() {
            let root = self.root(session).to_string();
            self.unassigned.insert(root);
        }
        let turn = turn.as_deref();
        let child = session != self.root(session);
        let text = part.get("text").cloned().unwrap_or(json!(""));
        match kind {
            "text"
                if self.msg_role.get(message).map(String::as_str) == Some("user")
                    || is_user_text(part) =>
            {
                // In a child session the parent agent wrote it, not the human.
                let kind = if child {
                    Self::other("opencode_subagent_prompt")
                } else {
                    EventKind::UserMessage
                };
                vec![self.fact(
                    kind,
                    session,
                    &format!("user:{id}"),
                    json!({"text": text}),
                    turn,
                    Some(message),
                )]
            }
            "text" | "reasoning" if !ended(part) => Vec::new(),
            "text" if self.msg_summary.contains(message) => vec![self.fact(
                Self::other("opencode_compaction_summary"),
                session,
                &format!("text:{id}"),
                json!({"text": text}),
                turn,
                Some(id),
            )],
            "text" | "reasoning" => {
                let interrupted =
                    turn.is_some_and(|t| self.aborted.get(t).is_some_and(|s| s.contains(session)));
                let (kind, suffix) = match (kind, child) {
                    ("reasoning", _) => (EventKind::Reasoning, "reasoning"),
                    (_, false) => (EventKind::AgentMessage, "text"),
                    (_, true) => (Self::other("opencode_subagent_message"), "text"),
                };
                let payload = json!({"text": text, "interrupted": interrupted});
                vec![self.fact(
                    kind,
                    session,
                    &format!("{suffix}:{id}"),
                    payload,
                    turn,
                    Some(id),
                )]
            }
            "subtask" => {
                let command = str_of(part, "command");
                let text = match command {
                    Some(command) => json!(format!("/{command}")),
                    None => part.get("prompt").cloned().unwrap_or(Value::Null),
                };
                let payload = json!({"text": text, "command": command, "agent": part.get("agent")});
                vec![self.fact(
                    EventKind::UserMessage,
                    session,
                    &format!("user:{id}"),
                    payload,
                    turn,
                    Some(message),
                )]
            }
            "tool" => {
                let turn = turn.map(str::to_string);
                self.tool(part, session, id, turn)
            }
            "step-finish" => {
                // Root steps only: a child's cost is its own session's.
                if let (Some(turn), false) = (turn, child) {
                    let tokens = part.get("tokens").cloned().unwrap_or(json!({}));
                    let cost = Some(part.get("cost").cloned().unwrap_or(json!(0)));
                    self.usage
                        .entry(turn.to_string())
                        .or_default()
                        .insert(id.to_string(), (tokens, cost));
                }
                Vec::new()
            }
            "patch" => {
                let payload = json!({"hash": part.get("hash"), "files": part.get("files")});
                vec![self.fact(
                    Self::other("opencode_patch"),
                    session,
                    &format!("patch:{id}"),
                    payload,
                    turn,
                    Some(id),
                )]
            }
            "compaction" => {
                let auto = truthy(part.get("auto"));
                let payload = json!({
                    "notification_type": "context_compacted", "auto": part.get("auto"),
                    "message": if auto { "Context compacted automatically" } else { "Context compacted" },
                });
                vec![self.fact(
                    EventKind::Notification,
                    session,
                    &format!("compaction:{id}"),
                    payload,
                    turn,
                    Some(id),
                )]
            }
            "file" => {
                let payload = json!({"filename": part.get("filename"), "mime": part.get("mime")});
                vec![self.fact(
                    Self::other("opencode_file"),
                    session,
                    &format!("file:{id}"),
                    payload,
                    turn,
                    Some(id),
                )]
            }
            "step-start" | "agent" => Vec::new(),
            other => {
                let name = format!("opencode_{}", other.replace('-', "_"));
                vec![self.fact(
                    Self::other(&name),
                    session,
                    &format!("part:{id}"),
                    json!({"type": other}),
                    turn,
                    Some(id),
                )]
            }
        }
    }

    fn tool_input(part: &Map<String, Value>) -> Value {
        let mut input = obj(part, "state")
            .and_then(|s| obj(s, "input"))
            .cloned()
            .unwrap_or_default();
        if let Some(path) = input.get("filePath").cloned() {
            input.entry("file_path").or_insert(path);
        }
        Value::Object(input)
    }

    fn tool(
        &mut self,
        part: &Map<String, Value>,
        session: &str,
        id: &str,
        turn: Option<String>,
    ) -> Vec<Emit> {
        let Some(state) = obj(part, "state") else {
            return Vec::new();
        };
        let tool = str_of(part, "tool").unwrap_or_default();
        let status = str_of(state, "status").unwrap_or_default();
        if status == "pending" || status.is_empty() {
            return Vec::new();
        }
        let name = display_name(tool);
        let input = Self::tool_input(part);
        let call = self.fact(
            EventKind::ToolCall,
            session,
            &format!("pre:{id}"),
            json!({"tool_name": name, "tool": tool, "tool_input": input}),
            turn.as_deref(),
            Some(id),
        );
        if status == "running" {
            if !self.running.iter().any(|r| r.part_id == id) {
                self.running.push(Running {
                    part_id: id.to_string(),
                    session: session.to_string(),
                    turn,
                    part: part.clone(),
                });
            }
            return vec![call];
        }
        self.running.retain(|r| r.part_id != id);
        let empty = Map::new();
        let metadata = obj(state, "metadata").unwrap_or(&empty);
        let exit_code = if tool == "bash" {
            metadata.get("exit").cloned()
        } else {
            None
        };
        let mut timed_out = false;
        // Classified from the part's own terminal state, never from the turn's
        // abort.
        let (outcome, output) = if status == "completed" {
            let output = state.get("output").cloned().unwrap_or(json!(""));
            let mut outcome = "completed";
            if tool == "bash" && metadata.get("exit") == Some(&Value::Null) {
                // The shell's kill paths: the reason is in the metadata block
                // at the end of the output.
                let text = output.as_str().unwrap_or_default();
                let meta = text.rfind("<shell_metadata>").map_or("", |at| &text[at..]);
                outcome = if meta.contains("User aborted the command") {
                    "interrupted"
                } else {
                    timed_out = meta.contains("terminated command after exceeding timeout");
                    "failed"
                };
            }
            (outcome, output)
        } else {
            let output = state.get("error").cloned().unwrap_or(json!(""));
            let cancelled = truthy(metadata.get("interrupted"))
                || (tool == "task" && output.as_str() == Some("Task cancelled"));
            (if cancelled { "interrupted" } else { "failed" }, output)
        };
        let time = obj(state, "time");
        let at = |key: &str| {
            time.and_then(|t| t.get(key))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        };
        let duration = at("end") - at("start");
        let mut payload = json!({
            "tool_name": name, "tool": tool, "tool_input": input, "status": outcome,
            "interrupted": outcome == "interrupted", "aggregated_output": output,
            "exit_code": exit_code,
            "duration_ms": if duration == 0.0 { Value::Null } else { json!(duration as i64) },
        });
        if timed_out {
            payload["timed_out"] = json!(true);
        }
        if truthy(metadata.get("diff")) {
            payload["diff"] = metadata["diff"].clone();
        }
        if let Some(child) = metadata.get("sessionId").filter(|_| tool == "task") {
            payload["child_session"] = child.clone();
        }
        let result = self.fact(
            EventKind::ToolResult,
            session,
            &format!("post:{id}"),
            payload,
            turn.as_deref(),
            Some(id),
        );
        vec![call, result]
    }

    /// The terminal of a tool whose turn was ended by something else. Same key
    /// as the real one, so a real result already recorded wins.
    fn tool_closed(&mut self, tool: &Running, turn: &str, reason: &str) -> Emit {
        self.running.retain(|r| r.part_id != tool.part_id);
        let name = str_of(&tool.part, "tool").unwrap_or_default();
        let payload = json!({
            "tool_name": display_name(name), "tool": name, "tool_input": Self::tool_input(&tool.part),
            "status": "interrupted", "interrupted": true, "aggregated_output": "", "exit_code": null,
            "duration_ms": null, "closed_by": reason,
        });
        self.fact(
            EventKind::ToolResult,
            &tool.session,
            &format!("post:{}", tool.part_id),
            payload,
            Some(turn),
            Some(&tool.part_id),
        )
    }

    // ------------------------------------------------------------------ cards

    fn card_turn(&self, session: &str, tool: Option<&Map<String, Value>>) -> Option<String> {
        tool.and_then(|t| str_of(t, "messageID"))
            .and_then(|m| self.msg_turn.get(m).cloned().flatten())
            .or_else(|| self.current.get(self.root(session)).cloned())
    }

    fn open_card(
        &mut self,
        request_id: &str,
        session: &str,
        kind: RequestKind,
        turn: Option<String>,
        call_id: Option<&str>,
        computed_at: Option<String>,
    ) {
        self.request_turn.insert(request_id.to_string(), turn);
        match self
            .open_cards
            .iter_mut()
            .find(|c| c.request_id == request_id)
        {
            Some(card) => {
                card.session = session.to_string();
                if computed_at.is_some() {
                    card.computed_at = computed_at;
                }
            }
            None => self.open_cards.push(OpenCard {
                request_id: request_id.to_string(),
                session: session.to_string(),
                kind,
                call_id: call_id.map(str::to_string),
                computed_at,
            }),
        }
    }

    /// The plugin activation every card raised from now on is stamped with,
    /// and an answer is checked against.
    pub(crate) fn set_activation(&mut self, activation: u64) {
        self.activation = activation;
    }

    /// The card is no longer open: its terminal was filed. The row and the
    /// daemon's pending entry are the store of an open card; this list only
    /// mirrors them, and every terminal ends here.
    pub(crate) fn forget(&mut self, request_id: &str) {
        self.open_cards.retain(|c| c.request_id != request_id);
    }

    /// A request resolved before any card of it could be raised. Its card
    /// frame, should it still arrive, is hidden: it lost the race to its own
    /// resolution.
    fn remember_retired(&mut self, request_id: &str) {
        if self.retired.iter().any(|id| id == request_id) {
            return;
        }
        if self.retired.len() == RETIRED_MEMORY {
            self.retired.pop_front();
        }
        self.retired.push_back(request_id.to_string());
    }

    fn card_fact(&self, session: &str, filed: Filed<'_>, card: ApprovalCard) -> Emit {
        let request_id = card.request_id.clone();
        let prefix = match filed.kind {
            RequestKind::Permission => "perm",
            RequestKind::Question => "question",
        };
        let payload = json!({
            "request_id": request_id, "family": filed.family, "tool_call_id": filed.call_id,
            "card": card, "answerable": filed.answerable,
        });
        let turn = filed.turn;
        self.fact(
            EventKind::ApprovalRequest,
            session,
            &format!("{prefix}:{request_id}"),
            payload,
            turn,
            Some(&request_id),
        )
    }

    /// The tool part a request is about, as the plugin last showed it
    /// running: its tool and its input. A doom-loop request names no part, so
    /// its anchor is the newest running part whose tool and input are the
    /// ones it says were repeated.
    fn anchor(&self, session: &str, p: &Map<String, Value>) -> Option<Anchor> {
        let tool = obj(p, "tool");
        let metadata = obj(p, "metadata");
        let running = self.running.iter().rev().find(|r| {
            r.session == session
                && match tool {
                    Some(tool) => {
                        str_of(&r.part, "callID") == str_of(tool, "callID")
                            && str_of(&r.part, "messageID") == str_of(tool, "messageID")
                    }
                    None => {
                        str_of(&r.part, "tool") == metadata.and_then(|m| str_of(m, "tool"))
                            && obj(&r.part, "state").and_then(|s| s.get("input"))
                                == metadata.and_then(|m| m.get("input"))
                    }
                }
        })?;
        let input = obj(&running.part, "state")
            .and_then(|s| obj(s, "input"))
            .cloned()
            .or_else(|| obj(p, "anchor_input").cloned())
            .unwrap_or_default();
        Some(Anchor {
            tool: str_of(&running.part, "tool")
                .unwrap_or_default()
                .to_string(),
            input,
        })
    }

    fn on_asked(&mut self, kind: RequestKind, p: &Map<String, Value>) -> Vec<Emit> {
        let (Some(id), Some(session)) = (str_of(p, "id"), str_of(p, "sessionID")) else {
            return Vec::new();
        };
        let tool = obj(p, "tool");
        let turn = self.card_turn(session, tool);
        let call_id = tool.and_then(|t| str_of(t, "callID"));
        let permission = str_of(p, "permission").unwrap_or_default();
        let Some(ctx) = obj(p, "ctx").filter(|_| kind == RequestKind::Permission) else {
            // A question, or a permission the plugin judged against nothing:
            // shown, and answered at the Mac.
            self.open_card(id, session, kind, turn.clone(), call_id, None);
            let (family, card) = match kind {
                RequestKind::Permission => {
                    (permission.to_string(), permission_card(id, permission, p))
                }
                RequestKind::Question => ("question".to_string(), question_card(id, p)),
            };
            let filed = Filed {
                kind,
                family: &family,
                call_id,
                turn: turn.as_deref(),
                answerable: false,
            };
            return vec![self.card_fact(session, filed, card)];
        };
        if self.retired.iter().any(|r| r == id) {
            crate::log_info!("opencode: request {id} resolved before its card arrived; not shown");
            return Vec::new();
        }
        let (built, computed_at) = self.build_ask(p, ctx);
        match built {
            Built::Hidden(why) => {
                crate::log_info!("opencode: permission {id} ({permission}) not shown: {why}");
                Vec::new()
            }
            Built::Stub { family, card } => {
                self.open_card(id, session, kind, turn.clone(), call_id, None);
                let filed = Filed {
                    kind,
                    family: &family,
                    call_id,
                    turn: turn.as_deref(),
                    answerable: false,
                };
                vec![self.card_fact(session, filed, card)]
            }
            Built::Card { family, card } => {
                self.open_card(id, session, kind, turn.clone(), call_id, Some(computed_at));
                let filed = Filed {
                    kind,
                    family: &family,
                    call_id,
                    turn: turn.as_deref(),
                    answerable: true,
                };
                vec![self.card_fact(session, filed, card)]
            }
        }
    }

    /// The card of one permission request the plugin judged, and when its
    /// content was computed: the time a card of the same open request was
    /// built, so its bytes stay the same, else now.
    fn build_ask(&self, p: &Map<String, Value>, ctx: &Map<String, Value>) -> (Built, String) {
        let id = str_of(p, "id").unwrap_or_default();
        let session = str_of(p, "sessionID").unwrap_or_default();
        let call_id = obj(p, "tool").and_then(|t| str_of(t, "callID"));
        let computed_at = self
            .open_cards
            .iter()
            .find(|c| c.request_id == id)
            .and_then(|c| c.computed_at.clone())
            .unwrap_or_else(|| self.env.now());
        let paired = call_id.is_some()
            && self
                .open_cards
                .iter()
                .any(|c| c.request_id != id && c.call_id.as_deref() == call_id);
        let ask = Ask {
            id,
            session,
            permission: str_of(p, "permission").unwrap_or_default(),
            patterns: strings(p.get("patterns")),
            always: strings(p.get("always")),
            metadata: obj(p, "metadata").cloned().unwrap_or_default(),
            anchor: self.anchor(session, p),
            ctx: Ctx::read(ctx),
            computed_at: computed_at.clone(),
            paired,
        };
        (build(&ask, self.env.as_ref(), self.activation), computed_at)
    }

    fn on_card_stub(
        &mut self,
        kind: RequestKind,
        ids: &CardIds,
        size: u64,
        sha256: &str,
    ) -> Vec<Emit> {
        let tool = ids.tool.as_ref().and_then(Value::as_object);
        let turn = self.card_turn(&ids.session_id, tool);
        let call_id = tool.and_then(|t| str_of(t, "callID"));
        self.open_card(&ids.id, &ids.session_id, kind, turn.clone(), call_id, None);
        let family = match kind {
            RequestKind::Permission => ids.permission.clone().unwrap_or_default(),
            RequestKind::Question => "question".to_string(),
        };
        let card = stub_card(&ids.id, &family, size, sha256);
        let filed = Filed {
            kind,
            family: &family,
            call_id,
            turn: turn.as_deref(),
            answerable: false,
        };
        vec![self.card_fact(&ids.session_id, filed, card)]
    }

    /// A request another client made in OpenCode's process, which the TUI
    /// never shows and this daemon never cards: one line says it is there.
    fn on_hidden_asked(&self, p: &Map<String, Value>) -> Vec<Emit> {
        let (Some(id), Some(session)) = (str_of(p, "id"), str_of(p, "sessionID")) else {
            return Vec::new();
        };
        let turn = self.current.get(self.root(session)).cloned();
        let payload = json!({
            "notification_type": "opencode_request_hidden", "request_id": id,
            "message": HIDDEN_REQUEST,
        });
        vec![self.fact(
            EventKind::Notification,
            session,
            &format!("perm_hidden:{id}"),
            payload,
            turn.as_deref(),
            Some(id),
        )]
    }

    fn resolution(
        &self,
        session: &str,
        kind: RequestKind,
        request_id: &str,
        resolution: CodexResolution,
        turn: Option<String>,
    ) -> Emit {
        let prefix = match kind {
            RequestKind::Permission => "perm_resolved",
            RequestKind::Question => "question_resolved",
        };
        let payload = CodexResolutionPayload {
            request_id: request_id.to_string(),
            resolution,
        };
        self.fact(
            EventKind::ApprovalResolved,
            session,
            &format!("{prefix}:{request_id}"),
            serde_json::to_value(payload).unwrap_or_default(),
            turn.as_deref(),
            Some(request_id),
        )
    }

    /// Answered at the Mac, by the keyboard, another client, or OpenCode's own
    /// cascade after an Always or a Reject in the same session: the reply word
    /// is on the wire and nothing says which. A reply for a request with no
    /// open card is nothing to the timeline, and its id is remembered so a
    /// card frame still on its way is not shown.
    fn on_replied(&mut self, kind: RequestKind, p: &Map<String, Value>) -> Vec<Emit> {
        let (Some(id), Some(session)) = (str_of(p, "requestID"), str_of(p, "sessionID")) else {
            return Vec::new();
        };
        if !self.open_cards.iter().any(|c| c.request_id == id) {
            self.remember_retired(id);
            return Vec::new();
        }
        self.forget(id);
        let turn = self
            .request_turn
            .get(id)
            .cloned()
            .flatten()
            .or_else(|| self.current.get(self.root(session)).cloned());
        let decision = match kind {
            RequestKind::Permission => str_of(p, "reply").map(|reply| AnswerDecision::OptionId {
                option_id: reply.to_string(),
            }),
            RequestKind::Question => None,
        };
        let answered = CodexResolution::Answered {
            by: ResolutionActor::Local,
            decision,
        };
        vec![self.resolution(session, kind, id, answered, turn)]
    }

    fn asked(&mut self, request: &SnapRequest) -> Vec<Emit> {
        match &request.body {
            RequestBody::Request { request: body } => self.on_asked(request.kind, body),
            RequestBody::Stub { stub } => {
                self.on_card_stub(request.kind, &stub.properties, stub.size, &stub.sha256)
            }
        }
    }

    /// A snapshot judged an open card's request dead: the card is retired as
    /// ended, in the turn it was asked in.
    fn ended(&mut self, request_id: &str) -> Vec<Emit> {
        let Some(at) = self
            .open_cards
            .iter()
            .position(|c| c.request_id == request_id)
        else {
            return Vec::new();
        };
        let card = self.open_cards.remove(at);
        let turn = self.request_turn.get(request_id).cloned().flatten();
        let cleared = CodexResolution::Cleared {
            cause: ClearCause::Ended,
        };
        vec![self.resolution(&card.session, card.kind, request_id, cleared, turn)]
    }
}

/// How many resolved-before-carded ids a run remembers, evicted oldest first.
/// Permission and question ids ascend only within their own prefix, so the
/// order is the order seen, never the ids'.
const RETIRED_MEMORY: usize = 64;

/// The line a request from another client gets.
const HIDDEN_REQUEST: &str =
    "OpenCode has a request from another client that this app does not answer";

// ---------------------------------------------------------------- the card

/// A card shown on the phone and answered at the Mac.
///
/// `display_text` and `payload_hash` are the pair every other card carries:
/// the phone checks `SHA-256(display_text) == payload_hash` before it trusts
/// the text, so the text is the hashed rendering of `tool_input`, and every
/// string a person reads is cut to 8 KiB with a digest of the whole inside
/// `tool_input`, where the hash covers it.
fn card(request_id: &str, tool_name: &str, tool_input: Value) -> ApprovalCard {
    ApprovalCard {
        request_id: request_id.to_string(),
        payload_hash: protocol::hash::approval_payload_hash(tool_name, &tool_input),
        display_text: protocol::hash::approval_payload_text(tool_name, &tool_input),
        tool_name: tool_name.to_string(),
        tool_input,
        permission_suggestions: None,
        prompt_id: None,
        permission_mode: None,
        risk: None,
        generation: 0,
        identity_bound: false,
        question_hold: None,
    }
}

fn bounded(text: &str) -> String {
    crate::codex_approval::bounded(text, crate::codex_approval::MAX_COMMAND_BYTES)
}

/// The most a card's encoded `tool_input` may take: one field cut to the
/// 8 KiB bound, with its marker, and room for the rest of the card.
const MAX_CARD_BYTES: usize = 2 * crate::codex_approval::MAX_COMMAND_BYTES;

/// `tool_input` as the card, or, when it encodes over [`MAX_CARD_BYTES`], the
/// card of a request too large to show, sized and hashed over the encoded
/// request.
fn bounded_card(
    request_id: &str,
    tool_name: &str,
    tool_input: Value,
    request: &Map<String, Value>,
) -> ApprovalCard {
    let size = serde_json::to_vec(&tool_input).map_or(usize::MAX, |b| b.len());
    if size <= MAX_CARD_BYTES {
        return card(request_id, tool_name, tool_input);
    }
    let encoded = serde_json::to_vec(request).unwrap_or_default();
    let sha256 = protocol::hash::sha256_hex(&encoded);
    stub_card(request_id, tool_name, encoded.len() as u64, &sha256)
}

/// A permission the plugin judged against nothing: a shell command is shown
/// as the command OpenCode will run, never as its patterns, which are a prefix
/// of it; any other permission is shown by name with the patterns it asks for.
fn permission_card(request_id: &str, permission: &str, p: &Map<String, Value>) -> ApprovalCard {
    let command = obj(p, "metadata").and_then(|m| str_of(m, "command"));
    if let (Some(command), "bash") = (command, permission) {
        return card(request_id, "command", json!({"command": bounded(command)}));
    }
    let patterns: Vec<Value> = p
        .get("patterns")
        .and_then(Value::as_array)
        .map(|all| {
            all.iter()
                .map(|v| v.as_str().map_or_else(|| v.clone(), |s| json!(bounded(s))))
                .collect()
        })
        .unwrap_or_default();
    let name = permission.to_lowercase();
    bounded_card(
        request_id,
        &name,
        json!({"permission": permission, "patterns": patterns}),
        p,
    )
}

fn question_card(request_id: &str, p: &Map<String, Value>) -> ApprovalCard {
    let questions = p.get("questions").cloned().unwrap_or(json!([]));
    bounded_card(request_id, "question", json!({"questions": questions}), p)
}

/// A request over the plugin's frame cap arrives without its text.
fn stub_card(request_id: &str, family: &str, size: u64, sha256: &str) -> ApprovalCard {
    let message = format!(
        "This request is too large to show ({size} bytes, sha256 {sha256}); answer it at the Mac"
    );
    card(
        request_id,
        &family.to_lowercase(),
        json!({"message": message}),
    )
}

// ------------------------------------------------------- the answerable card
//
// A permission the phone may answer, built from what the plugin read at the
// ask: the request, the tool part it is about, and the TUI's tool ids,
// connected MCP servers, permission mode and the session's parent. Every
// family below was measured on OpenCode 1.18.34 (`fixtures/opencode/
// s7-permission-*`); a request of any other shape is not shown, because a
// card that says less than the screen beside it, or more, is a card that lies.

/// How a card's request fact is filed: its kind, the family word, the tool
/// call it is about, the turn, and whether the phone may answer it.
struct Filed<'a> {
    kind: RequestKind,
    family: &'a str,
    call_id: Option<&'a str>,
    turn: Option<&'a str>,
    answerable: bool,
}

/// The tool part a request is about.
struct Anchor {
    tool: String,
    input: Map<String, Value>,
}

/// What the plugin read at the ask.
struct Ctx {
    ok: bool,
    tool_ids: Vec<String>,
    mcp: Vec<String>,
    parent: Option<String>,
}

impl Ctx {
    fn read(ctx: &Map<String, Value>) -> Ctx {
        Ctx {
            ok: truthy(ctx.get("ok")),
            tool_ids: strings(ctx.get("tool_ids")),
            mcp: strings(ctx.get("mcp")),
            parent: str_of(ctx, "parent").map(str::to_string),
        }
    }
}

/// One permission request and everything a card of it is built from.
struct Ask<'a> {
    id: &'a str,
    session: &'a str,
    permission: &'a str,
    patterns: Vec<String>,
    always: Vec<String>,
    metadata: Map<String, Value>,
    anchor: Option<Anchor>,
    ctx: Ctx,
    /// When this card's content was computed. Inside the hash, and reused by
    /// a rebuild of the same open request so the bytes stay the same.
    computed_at: String,
    /// Another card about the same tool call is open.
    paired: bool,
}

/// What building a card decided.
enum Built {
    /// Not shown, and why. Nothing is filed for it.
    Hidden(String),
    /// Shown and not answerable: too large to show.
    Stub {
        family: String,
        card: ApprovalCard,
    },
    Card {
        family: String,
        card: ApprovalCard,
    },
}

/// The families a phone is offered, by the request's `permission`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Bash,
    ExternalDirectory,
    Edit,
    Read,
    Glob,
    Grep,
    Webfetch,
    Websearch,
    Task,
    Todowrite,
    Skill,
    Lsp,
    DoomLoop,
    Mcp,
}

impl Family {
    fn of_permission(permission: &str) -> Option<Family> {
        Some(match permission {
            "bash" => Family::Bash,
            "external_directory" => Family::ExternalDirectory,
            "edit" => Family::Edit,
            "read" => Family::Read,
            "glob" => Family::Glob,
            "grep" => Family::Grep,
            "webfetch" => Family::Webfetch,
            "websearch" => Family::Websearch,
            "task" => Family::Task,
            "todowrite" => Family::Todowrite,
            "skill" => Family::Skill,
            "lsp" => Family::Lsp,
            "doom_loop" => Family::DoomLoop,
            _ => return None,
        })
    }

    /// The word the card's row and fact carry.
    fn as_str(self) -> &'static str {
        match self {
            Family::Bash => "bash",
            Family::ExternalDirectory => "external_directory",
            Family::Edit => "edit",
            Family::Read => "read",
            Family::Glob => "glob",
            Family::Grep => "grep",
            Family::Webfetch => "webfetch",
            Family::Websearch => "websearch",
            Family::Task => "task",
            Family::Todowrite => "todowrite",
            Family::Skill => "skill",
            Family::Lsp => "lsp",
            Family::DoomLoop => "doom_loop",
            Family::Mcp => "mcp",
        }
    }

    /// The tools whose part a request of this family is anchored to.
    fn tools(self) -> &'static [&'static str] {
        match self {
            Family::Bash => &["bash"],
            Family::ExternalDirectory => &[
                "read",
                "edit",
                "write",
                "lsp",
                "bash",
                "grep",
                "glob",
                "apply_patch",
            ],
            Family::Edit => &["edit", "write", "apply_patch"],
            Family::Read => &[
                "read",
                "list_mcp_resources",
                "list_mcp_resource_templates",
                "read_mcp_resource",
            ],
            Family::Glob => &["glob"],
            Family::Grep => &["grep"],
            Family::Webfetch => &["webfetch"],
            Family::Websearch => &["websearch"],
            Family::Task => &["task"],
            Family::Todowrite => &["todowrite"],
            Family::Skill => &["skill"],
            Family::Lsp => &["lsp"],
            Family::DoomLoop | Family::Mcp => &[],
        }
    }
}

/// The scope every Always grants, measured: an Always is appended after the
/// config's rules and the last match wins, so it also allows what the config
/// denies, for every session of this OpenCode until it reloads or exits.
const SCOPE: &str = "in this folder's OpenCode until it reloads or exits, for every session \
                     there, including what your config denies";

/// What OpenCode's tool-name sanitiser does to an MCP server's name.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|all| {
            all.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `path` against `base`, lexically: `.` dropped and `..` folded, no link
/// followed. What OpenCode's own path checks do.
fn resolve(base: &Path, path: &str) -> PathBuf {
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn shown(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Every string in a value cut to the display bound, with a digest of the
/// whole where it was cut.
fn bound_strings(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(bounded(s)),
        Value::Array(items) => Value::Array(items.iter().map(bound_strings).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), bound_strings(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Whether an OpenCode wildcard pattern covers `text`: `*` is any run of
/// characters, directory separators included, `?` one character.
fn wildcard_covers(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|k| go(rest, &t[k..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    go(&p, &t)
}

/// The files OpenCode's TUI reads its config from, in the order it merges
/// them: the global config, `OPENCODE_CONFIG`, each folder from the home
/// folder down to the run's folder (`opencode.json(c)` and `.opencode/`),
/// then `OPENCODE_CONFIG_DIR`. Later files win.
fn config_files(env: &dyn CardEnv) -> Vec<PathBuf> {
    let both = |dir: &Path| vec![dir.join("opencode.json"), dir.join("opencode.jsonc")];
    let mut out = Vec::new();
    let global = env
        .env_var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env.home().map(|home| home.join(".config")))
        .map(|config| config.join("opencode"));
    if let Some(global) = global {
        out.extend(both(&global));
    }
    if let Some(path) = env.env_var("OPENCODE_CONFIG") {
        out.push(PathBuf::from(path));
    }
    let worktree = env.worktree();
    let top = env
        .home()
        .filter(|home| worktree.starts_with(home))
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut dirs: Vec<&Path> = worktree
        .ancestors()
        .take_while(|dir| dir.starts_with(&top))
        .collect();
    dirs.reverse();
    for dir in dirs {
        out.extend(both(dir));
        out.extend(both(&dir.join(".opencode")));
    }
    if let Some(dir) = env.env_var("OPENCODE_CONFIG_DIR") {
        out.extend(both(Path::new(&dir)));
    }
    out
}

/// JSON with comments, as OpenCode's config files allow it, as JSON: `//` and
/// `/* */` comments outside strings dropped, a comma before `}` or `]` dropped.
fn jsonc_to_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            ',' => {
                let mut k = i + 1;
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                if !matches!(chars.get(k), Some('}') | Some(']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The patterns the operator's merged config denies under `permission`,
/// read as OpenCode merges its config: later files override earlier ones key
/// by key. A file that cannot be read or parsed fails the whole read, and the
/// card says so rather than naming an incomplete list; `{env:}` and `{file:}`
/// substitutions are not performed, so a config using them reads as written.
fn config_denies(env: &dyn CardEnv, permission: &str) -> Result<Vec<String>, String> {
    fn merge(into: &mut Value, from: Value) {
        match (into, from) {
            (Value::Object(a), Value::Object(b)) => {
                for (k, v) in b {
                    match a.get_mut(&k) {
                        Some(slot) => merge(slot, v),
                        None => {
                            a.insert(k, v);
                        }
                    }
                }
            }
            (slot, v) => *slot = v,
        }
    }
    let mut merged = Value::Null;
    for file in config_files(env) {
        let text = env
            .read_text(&file)
            .map_err(|err| format!("{}: {err}", shown(&file)))?;
        let Some(text) = text else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let parsed: Value = serde_json::from_str(&jsonc_to_json(&text))
            .map_err(|err| format!("{}: {err}", shown(&file)))?;
        if let Some(rules) = parsed.get("permission") {
            merge(&mut merged, rules.clone());
        }
    }
    let rule = merged.get(permission).or_else(|| merged.get("*"));
    Ok(match rule {
        Some(Value::String(word)) if word == "deny" => vec!["*".to_string()],
        Some(Value::Object(rules)) => rules
            .iter()
            .filter(|(_, v)| v.as_str() == Some("deny"))
            .map(|(k, _)| k.clone())
            .collect(),
        _ => Vec::new(),
    })
}

/// The Always option's scope: what it grants, where, for how long, and what
/// the operator's config says against it.
fn scope(
    text: &str,
    caveat: Option<&str>,
    always: &[String],
    denies: &Result<Vec<String>, String>,
) -> String {
    let mut out = format!("{text}, {SCOPE}");
    if let Some(caveat) = caveat {
        out.push_str(". ");
        out.push_str(caveat);
    }
    match denies {
        Err(_) => out.push_str(". Could not read your config, so what it denies is not listed"),
        Ok(denied) => {
            let covered: Vec<&str> = denied
                .iter()
                .filter(|deny| always.iter().any(|a| a == "*" || wildcard_covers(a, deny)))
                .map(String::as_str)
                .collect();
            if !covered.is_empty() {
                out.push_str(". Your config denies ");
                out.push_str(&covered.join(", "));
                out.push_str("; this would allow it");
            }
        }
    }
    out
}

fn options(always: Option<String>, with_words: bool) -> Value {
    let mut out = vec![json!({"id": "once", "label": "Allow once"})];
    if let Some(scope) = always {
        out.push(json!({"id": "always", "label": "Allow always", "scope": scope}));
    }
    out.push(json!({"id": "reject", "label": "Reject"}));
    if with_words {
        out.push(json!({"id": "reject_with_words", "label": "Reject with words"}));
    }
    Value::Array(out)
}

/// Whether an external-directory Always may be offered for `entry`, an
/// `always[]` pattern: one tap on it removes the check for everything below
/// the folder it names until OpenCode reloads, so a folder that is the disk,
/// the home folder, the run's folder or an ancestor of any of them is not
/// offered, nor a pattern this daemon cannot resolve to one folder.
fn external_always_offered(env: &dyn CardEnv, entry: &str) -> bool {
    let dir = entry.strip_suffix("/*").unwrap_or(entry);
    if dir.contains(['*', '?']) {
        return false;
    }
    let Some(target) = env.identity(Path::new(dir)) else {
        return false;
    };
    let mut forbidden: Vec<PathBuf> =
        vec![PathBuf::from("/"), PathBuf::from("/System/Volumes/Data")];
    for root in [env.home(), Some(env.worktree())].into_iter().flatten() {
        forbidden.extend(root.ancestors().map(Path::to_path_buf));
    }
    !forbidden
        .iter()
        .any(|path| env.identity(path) == Some(target))
}

/// The one family the request belongs to, or why it belongs to none.
fn family_of(ask: &Ask) -> Result<Family, String> {
    if let Some(family) = Family::of_permission(ask.permission) {
        return Ok(family);
    }
    let star = |list: &[String]| list == ["*"];
    let anchored = ask
        .anchor
        .as_ref()
        .is_some_and(|a| a.tool == ask.permission);
    let servers: Vec<&String> = ask
        .ctx
        .mcp
        .iter()
        .filter(|s| ask.permission.starts_with(&format!("{}_", sanitize(s))))
        .collect();
    if anchored
        && !ask.ctx.tool_ids.iter().any(|id| id == ask.permission)
        && star(&ask.patterns)
        && star(&ask.always)
        && ask.metadata.is_empty()
        && servers.len() == 1
    {
        return Ok(Family::Mcp);
    }
    Err(format!(
        "`{}` is not a permission this build has a card for",
        ask.permission
    ))
}

/// Whether a tool id is shadowed: registered twice, or a built-in id that a
/// connected MCP server's tools would replace. Measured: the later entry
/// wins, and nothing in the request says which ran.
fn shadowed(ctx: &Ctx, id: &str) -> bool {
    let times = ctx.tool_ids.iter().filter(|t| *t == id).count();
    times > 1
        || (times == 1
            && ctx
                .mcp
                .iter()
                .any(|s| id.starts_with(&format!("{}_", sanitize(s)))))
}

/// The anchored input against what the request says, per family: a card is
/// built from both and must not describe two different calls.
fn consistent(family: Family, ask: &Ask, anchor: &Anchor, worktree: &Path) -> bool {
    let input = &anchor.input;
    let meta = &ask.metadata;
    let same = |key: &str| input.get(key) == meta.get(key);
    let same_path = |input_key: &str, meta_key: &str| match (
        str_of(input, input_key),
        str_of(meta, meta_key),
    ) {
        (Some(a), Some(b)) => resolve(worktree, a) == resolve(worktree, b),
        _ => false,
    };
    match family {
        Family::Bash => same("command"),
        Family::ExternalDirectory => match anchor.tool.as_str() {
            "bash" => same("command"),
            "grep" | "glob" => same_path("path", "filepath"),
            "apply_patch" => true,
            _ => same_path("filePath", "filepath"),
        },
        Family::Edit => anchor.tool == "apply_patch" || same_path("filePath", "filepath"),
        Family::Read => match anchor.tool.as_str() {
            "read" => ask
                .patterns
                .first()
                .zip(str_of(input, "filePath"))
                .is_some_and(|(pattern, path)| {
                    resolve(worktree, path) == resolve(worktree, pattern)
                }),
            _ => same("server"),
        },
        Family::Glob | Family::Grep => same("pattern"),
        Family::Webfetch => same("url"),
        Family::Websearch => same("query"),
        Family::Task => same("subagent_type") && same("description"),
        Family::Skill => ask.patterns.first().map(String::as_str) == str_of(input, "name"),
        Family::Todowrite | Family::Lsp => true,
        Family::DoomLoop | Family::Mcp => true,
    }
}

/// Build the card, or say why there is none.
fn build(ask: &Ask, env: &dyn CardEnv, activation: u64) -> Built {
    if !ask.ctx.ok {
        return Built::Hidden(
            "the plugin could not read the TUI's tools or mode at the ask".into(),
        );
    }
    let Some(anchor) = ask.anchor.as_ref() else {
        return Built::Hidden("no running tool part is the one it is about".into());
    };
    if shadowed(&ask.ctx, &anchor.tool) || shadowed(&ask.ctx, ask.permission) {
        return Built::Hidden(format!("the tool id `{}` is shadowed", anchor.tool));
    }
    if anchor.tool == "execute" {
        return Built::Hidden("an MCP call made from code mode".into());
    }
    let family = match family_of(ask) {
        Ok(family) => family,
        Err(why) => return Built::Hidden(why),
    };
    let worktree = env.worktree();
    let (repeated, family) = match family {
        Family::DoomLoop => {
            let Some(tool) = str_of(&ask.metadata, "tool") else {
                return Built::Hidden("a doom loop that names no tool".into());
            };
            if shadowed(&ask.ctx, tool) {
                return Built::Hidden(format!("the repeated tool id `{tool}` is shadowed"));
            }
            match Family::of_permission(tool).filter(|f| f.tools().contains(&tool)) {
                Some(repeated) => (Some(tool), repeated),
                None => {
                    return Built::Hidden(format!(
                        "a doom loop of `{tool}`, which is not a tool this build has a card for"
                    ))
                }
            }
        }
        Family::Mcp => (None, Family::Mcp),
        family => {
            if !family.tools().contains(&anchor.tool.as_str()) {
                return Built::Hidden(format!(
                    "anchored to a `{}` part, not a `{}` one",
                    anchor.tool, ask.permission
                ));
            }
            if !consistent(family, ask, anchor, &worktree) {
                return Built::Hidden(
                    "the running tool's input is not the one the request describes".into(),
                );
            }
            (None, family)
        }
    };
    let family_word = if repeated.is_some() {
        Family::DoomLoop.as_str()
    } else {
        family.as_str()
    };
    let shape = match repeated {
        Some(tool) => {
            // The repeated call, described as its own family would describe
            // it: its input stands where that family's metadata would.
            let input = ask
                .metadata
                .get("input")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let repeated_ask = Ask {
                id: ask.id,
                session: ask.session,
                permission: tool,
                patterns: Vec::new(),
                always: ask.always.clone(),
                metadata: input.clone(),
                anchor: None,
                ctx: Ctx {
                    ok: true,
                    tool_ids: Vec::new(),
                    mcp: Vec::new(),
                    parent: None,
                },
                computed_at: ask.computed_at.clone(),
                paired: false,
            };
            let anchor = Anchor {
                tool: tool.to_string(),
                input,
            };
            let mut shape = shape_of(family, &repeated_ask, &anchor, env, &worktree);
            shape.input.insert("repeated_call".into(), json!(tool));
            shape.always = Some(format!("stop asking about repeated {tool} calls"));
            shape
        }
        None => shape_of(family, ask, anchor, env, &worktree),
    };
    let Shape {
        tool_name,
        mut input,
        classified,
        always,
        caveat,
    } = shape;
    let denies = always.as_ref().map(|_| config_denies(env, ask.permission));
    let always = always
        .zip(denies)
        .map(|(text, denies)| scope(&text, caveat, &ask.always, &denies));
    input.insert("always".into(), json!(ask.always));
    input.insert("computed_at".into(), json!(ask.computed_at));
    if ask.paired {
        input.insert(
            "part".into(),
            json!("2 of 2: another card is about the same tool call"),
        );
    }
    input.insert("options".into(), options(always, ask.ctx.parent.is_some()));
    let input = Value::Object(input);
    let mut without_changes = input.clone();
    if let Some(map) = without_changes.as_object_mut() {
        map.remove("changes");
        map.remove("diff");
    }
    let size = serde_json::to_vec(&without_changes).map_or(usize::MAX, |b| b.len());
    if size > MAX_CARD_BYTES {
        let encoded = serde_json::to_vec(&ask.metadata).unwrap_or_default();
        let sha256 = protocol::hash::sha256_hex(&encoded);
        return Built::Stub {
            family: family_word.to_string(),
            card: stub_card(ask.id, &tool_name, encoded.len() as u64, &sha256),
        };
    }
    let mut card = card(ask.id, &tool_name, input);
    // What would run, before any display bound: the classifier reads the
    // whole command, not the 8 KiB a card shows of it.
    card.risk = Some(protocol::risk::classify(&tool_name, &classified));
    card.generation = activation;
    Built::Card {
        family: family_word.to_string(),
        card,
    }
}

/// A family's card before the parts every card shares.
struct Shape {
    tool_name: String,
    /// What the phone reads, bounded.
    input: Map<String, Value>,
    /// What the risk classifier reads, unbounded.
    classified: Value,
    /// The Always text, when Always is offered.
    always: Option<String>,
    /// A sentence the Always scope carries after the folder scope.
    caveat: Option<&'static str>,
}

fn put(out: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        out.insert(key.to_string(), value);
    }
}

fn shape_of(
    family: Family,
    ask: &Ask,
    anchor: &Anchor,
    env: &dyn CardEnv,
    worktree: &Path,
) -> Shape {
    let meta = &ask.metadata;
    let input = &anchor.input;
    let mut out = Map::new();
    let text = |v: Option<&Value>| v.and_then(Value::as_str).map(|s| json!(bounded(s)));
    match family {
        Family::Bash => {
            put(&mut out, "command", text(meta.get("command")));
            put(&mut out, "workdir", text(input.get("workdir")));
            put(&mut out, "description", text(input.get("description")));
            Shape {
                tool_name: "command".into(),
                input: out,
                classified: json!({"command": meta.get("command")}),
                always: Some(format!("Allow commands starting {}", ask.always.join(", "))),
                caveat: Some("Matching ignores redirects, $VARS and later arguments"),
            }
        }
        Family::ExternalDirectory => {
            let dirs = strings(meta.get("directories"));
            let dir = str_of(meta, "parentDir")
                .map(str::to_string)
                .or_else(|| dirs.first().cloned())
                .unwrap_or_default();
            put(&mut out, "path", Some(json!(dir)));
            if !dirs.is_empty() {
                put(&mut out, "dirs", Some(json!(dirs)));
            }
            put(&mut out, "filepath", text(meta.get("filepath")));
            put(&mut out, "command", text(meta.get("command")));
            put(&mut out, "via", Some(json!(anchor.tool)));
            let offered = !ask.always.is_empty()
                && ask
                    .always
                    .iter()
                    .all(|entry| external_always_offered(env, entry));
            Shape {
                tool_name: "external directory".into(),
                input: out,
                classified: json!({"command": meta.get("command"), "path": dir, "filepath": meta.get("filepath")}),
                always: offered.then(|| format!("{dir} and everything below it")),
                caveat: None,
            }
        }
        Family::Edit => edit_shape(ask, anchor, env, worktree),
        Family::Read if anchor.tool == "read" => {
            let path = str_of(input, "filePath").map(|p| resolve(worktree, p));
            put(&mut out, "path", path.as_ref().map(|p| json!(shown(p))));
            if let Some(pattern) = ask.patterns.first().filter(|p| p.starts_with("..")) {
                put(&mut out, "relative", Some(json!(bounded(pattern))));
            }
            Shape {
                tool_name: "read".into(),
                input: out,
                classified: json!({"path": path.as_ref().map(|p| shown(p))}),
                always: Some("all reads".into()),
                caveat: None,
            }
        }
        Family::Read => {
            put(&mut out, "server", text(meta.get("server")));
            put(&mut out, "uri", text(meta.get("uri")));
            put(&mut out, "via", Some(json!(anchor.tool)));
            Shape {
                tool_name: "mcp resource".into(),
                input: out,
                classified: json!({"server": meta.get("server"), "uri": meta.get("uri")}),
                always: Some(ask.always.join(", ")),
                caveat: None,
            }
        }
        Family::Glob | Family::Grep => {
            put(&mut out, "pattern", text(meta.get("pattern")));
            put(&mut out, "path", text(meta.get("path")));
            put(&mut out, "include", text(meta.get("include")));
            Shape {
                tool_name: family.as_str().into(),
                input: out,
                classified: json!({"pattern": meta.get("pattern"), "path": meta.get("path"), "include": meta.get("include")}),
                always: Some(format!("all {} calls", family.as_str())),
                caveat: None,
            }
        }
        Family::Webfetch => {
            put(&mut out, "url", text(meta.get("url")));
            put(&mut out, "format", meta.get("format").cloned());
            put(&mut out, "timeout", meta.get("timeout").cloned());
            put(
                &mut out,
                "caveat",
                Some(json!("follows redirects to any host")),
            );
            Shape {
                tool_name: "webfetch".into(),
                input: out,
                classified: json!({"url": meta.get("url")}),
                always: Some("all web fetches".into()),
                caveat: None,
            }
        }
        Family::Websearch => {
            for (key, value) in meta {
                out.insert(key.clone(), bound_strings(value));
            }
            Shape {
                tool_name: "websearch".into(),
                input: out,
                classified: Value::Object(meta.clone()),
                always: Some("all searches".into()),
                caveat: None,
            }
        }
        Family::Task => {
            put(&mut out, "subagent_type", text(meta.get("subagent_type")));
            put(&mut out, "description", text(meta.get("description")));
            put(&mut out, "prompt", text(input.get("prompt")));
            Shape {
                tool_name: "task".into(),
                input: out,
                classified: json!({"subagent_type": meta.get("subagent_type"), "description": meta.get("description"), "prompt": input.get("prompt")}),
                always: Some("every subagent launch, ANY type".into()),
                caveat: None,
            }
        }
        Family::Todowrite => {
            put(&mut out, "todos", input.get("todos").map(bound_strings));
            Shape {
                tool_name: "todowrite".into(),
                input: out,
                classified: json!({"todos": input.get("todos")}),
                always: Some("all todo-list updates".into()),
                caveat: None,
            }
        }
        Family::Skill => {
            put(
                &mut out,
                "name",
                ask.patterns.first().map(|n| json!(bounded(n))),
            );
            Shape {
                tool_name: "skill".into(),
                input: out,
                classified: json!({"name": ask.patterns.first()}),
                always: Some(format!("the skill {}", ask.always.join(", "))),
                caveat: None,
            }
        }
        Family::Lsp => {
            put(&mut out, "operation", text(meta.get("operation")));
            put(&mut out, "filePath", text(meta.get("filePath")));
            put(&mut out, "line", meta.get("line").cloned());
            put(&mut out, "character", meta.get("character").cloned());
            Shape {
                tool_name: "lsp".into(),
                input: out,
                classified: Value::Object(meta.clone()),
                always: Some("all LSP queries".into()),
                caveat: None,
            }
        }
        Family::Mcp => {
            let server = ask
                .ctx
                .mcp
                .iter()
                .find(|s| ask.permission.starts_with(&format!("{}_", sanitize(s))))
                .cloned()
                .unwrap_or_default();
            for (key, value) in input {
                out.insert(key.clone(), bound_strings(value));
            }
            put(&mut out, "server", Some(json!(server)));
            put(
                &mut out,
                "caveat",
                Some(json!("arguments as the model sent them")),
            );
            Shape {
                tool_name: ask.permission.into(),
                input: out,
                classified: Value::Object(input.clone()),
                always: Some("every call of this MCP tool".into()),
                caveat: None,
            }
        }
        Family::DoomLoop => unreachable!("a doom loop is built as the family it repeats"),
    }
}

/// The ceiling on one diff a card shows.
const MAX_DIFF_BYTES: usize = 16 * 1024;

/// What a card says about the content it was computed from: OpenCode computes
/// the new content before the ask and writes it after the reply without
/// reading the file again.
const WRITES_COMPUTED: &str =
    "writes the version computed at computed_at; changes made to the file since then are overwritten";

/// `edit`, `write` and `apply_patch`, which share one permission and one
/// Always.
fn edit_shape(ask: &Ask, anchor: &Anchor, env: &dyn CardEnv, worktree: &Path) -> Shape {
    let meta = &ask.metadata;
    let mut out = Map::new();
    let mut classified = Map::new();
    let mut always = Some("Allow ALL edits".to_string());
    let diff = meta.get("diff").cloned().unwrap_or(json!(""));
    if anchor.tool == "apply_patch" {
        let files = meta
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut changes = Vec::new();
        let mut unchecked = Vec::new();
        for file in files.iter().take(32) {
            let Some(file) = file.as_object() else {
                continue;
            };
            let Some(path) = str_of(file, "filePath") else {
                continue;
            };
            let kind = str_of(file, "type").unwrap_or_default();
            let resolved = resolve(worktree, path);
            let mut change = Map::new();
            change.insert("path".into(), json!(shown(&resolved)));
            if path.starts_with("..") {
                change.insert("relative".into(), json!(bounded(path)));
            }
            change.insert("type".into(), json!(kind));
            if kind == "add" && env.exists(&resolved) {
                change.insert(
                    "overwrites".into(),
                    json!(format!(
                        "overwrites existing {} (current content not shown)",
                        shown(&resolved)
                    )),
                );
            }
            if let Some(dest) = str_of(file, "movePath") {
                let dest_resolved = resolve(worktree, dest);
                change.insert("move_path".into(), json!(shown(&dest_resolved)));
                let same = dest_resolved == resolved
                    || env
                        .identity(&resolved)
                        .is_some_and(|id| env.identity(&dest_resolved) == Some(id));
                if same {
                    change.insert(
                        "deletes".into(),
                        json!(format!(
                            "DELETES {}: the move destination is the file itself",
                            shown(&resolved)
                        )),
                    );
                    always = None;
                } else {
                    if env.exists(&dest_resolved) {
                        change.insert(
                            "overwrites".into(),
                            json!(format!(
                                "overwrites existing {} (current content not shown)",
                                shown(&dest_resolved)
                            )),
                        );
                    }
                    if dest != path {
                        unchecked.push(shown(&dest_resolved));
                    }
                }
            }
            changes.push(Value::Object(change));
        }
        if changes.len() == 1 {
            if let Some(Value::Object(change)) = changes.first_mut() {
                change.insert("diff".into(), diff.clone());
            }
        } else {
            out.insert("diff".into(), diff.clone());
        }
        let bounded_changes: Vec<Value> = changes
            .iter()
            .map(|c| crate::codex_approval::bounded_change(c, MAX_DIFF_BYTES))
            .collect();
        if let Some(Value::String(joined)) = out.get_mut("diff") {
            *joined = crate::codex_approval::bounded(joined, MAX_DIFF_BYTES);
        }
        if !unchecked.is_empty() {
            out.insert("unchecked_dest".into(), json!(unchecked));
            out.insert(
                "caveat".into(),
                json!("destination not checked against your edit rules"),
            );
        }
        out.insert("changes".into(), Value::Array(bounded_changes));
        classified.insert("changes".into(), Value::Array(changes));
    } else {
        let path = str_of(meta, "filepath").map(|p| resolve(worktree, p));
        let mut change = Map::new();
        if let Some(path) = &path {
            change.insert("path".into(), json!(shown(path)));
            if let Some(real) = env.realpath(path).filter(|real| real != path) {
                out.insert("realpath".into(), json!(shown(&real)));
            }
        }
        change.insert(
            "type".into(),
            json!(if anchor.tool == "write" {
                "write"
            } else {
                "update"
            }),
        );
        change.insert("diff".into(), diff);
        let change = Value::Object(change);
        out.insert(
            "changes".into(),
            json!([crate::codex_approval::bounded_change(
                &change,
                MAX_DIFF_BYTES
            )]),
        );
        classified.insert("changes".into(), json!([change]));
    }
    out.insert("note".into(), json!(WRITES_COMPUTED));
    Shape {
        tool_name: "file change".into(),
        input: out,
        classified: Value::Object(classified),
        always,
        caveat: None,
    }
}

// ------------------------------------------------------------- snapshot read

/// One snapshot being read into a copy of the adapter.
struct Resync<'a> {
    m: OpencodeAdapter,
    /// The adapter as it was before this snapshot.
    live: &'a OpencodeAdapter,
    snap: &'a Snapshot,
    sessions: HashMap<&'a str, &'a SnapSession>,
    /// (message, call) → the pending request about that tool call.
    pending: HashMap<(String, String), &'a SnapRequest>,
    /// Requests already placed after their tool call.
    placed: HashSet<String>,
    /// Children already read for the current root.
    done_kids: HashSet<String>,
    /// The current turn's assistant messages that have not completed.
    open: BTreeSet<String>,
    /// The current turn has a tool the server still shows running.
    still_running: bool,
    /// Roots whose session tree the snapshot holds incomplete.
    skipped: HashSet<String>,
    /// Why the turn being read cannot be closed with an outcome, if it
    /// cannot.
    proof: Option<Unproven>,
}

fn request_tool(request: &SnapRequest) -> Option<(String, String)> {
    let tool = match &request.body {
        RequestBody::Request { request } => obj(request, "tool"),
        RequestBody::Stub { stub } => stub.properties.tool.as_ref().and_then(Value::as_object),
    }?;
    Some((
        str_of(tool, "messageID")?.to_string(),
        str_of(tool, "callID")?.to_string(),
    ))
}

fn session_id(session: &SnapSession) -> &str {
    str_of(&session.info, "id").unwrap_or_default()
}

impl<'a> Resync<'a> {
    fn new(live: &'a OpencodeAdapter, snap: &'a Snapshot) -> Self {
        Resync {
            m: live.clone(),
            live,
            snap,
            sessions: snap.sessions.iter().map(|s| (session_id(s), s)).collect(),
            pending: snap
                .requests
                .iter()
                .filter_map(|r| request_tool(r).map(|key| (key, r)))
                .collect(),
            placed: HashSet::new(),
            done_kids: HashSet::new(),
            open: BTreeSet::new(),
            still_running: false,
            skipped: HashSet::new(),
            proof: None,
        }
    }

    fn set_aside(&self, session: &str) -> bool {
        self.skipped.contains(self.m.root(session))
    }

    /// Every root of the snapshot, oldest first, turn by turn.
    fn sessions(&mut self) -> Vec<Emit> {
        for session in &self.snap.sessions {
            if let Some(parent) = str_of(&session.info, "parentID") {
                self.m
                    .parent
                    .insert(session_id(session).to_string(), parent.to_string());
            }
        }
        for (child, parent) in &self.snap.stub_parents {
            self.m.parent.insert(child.clone(), parent.clone());
        }
        let snap: &'a Snapshot = self.snap;
        let mut roots: Vec<&'a SnapSession> = snap
            .sessions
            .iter()
            .filter(|s| !s.info.contains_key("parentID"))
            .collect();
        roots.sort_by(|a, b| created(&a.info).total_cmp(&created(&b.info)));
        self.skipped = snap
            .damaged
            .iter()
            .map(|s| self.m.root(s).to_string())
            .collect();
        roots.retain(|r| !self.skipped.contains(session_id(r)));
        let mut out = Vec::new();
        for root in roots {
            out.extend(self.read_root(root));
        }
        out
    }

    fn read_root(&mut self, session: &'a SnapSession) -> Vec<Emit> {
        let live = self.live;
        let r = session_id(session).to_string();
        let mut out = self.m.on_session_created(&session.info);
        let msgs = &session.messages;
        // A user message, typed or written by OpenCode, begins a turn when
        // the root's busy period before it had ended. A message this adapter
        // saw keeps the turn it was given.
        //
        // OpenCode writes one assistant message of a session at a time, so one
        // that never completed and has a later one after it was abandoned: its
        // process ended, and its run with it.
        let assistant = |x: &SnapMessage| str_of(&x.info, "role") == Some("assistant");
        let mut abandoned: HashSet<&str> = HashSet::new();
        let mut later = false;
        for x in msgs.iter().rev().filter(|x| assistant(x)) {
            if later && !completed(&x.info) {
                abandoned.extend(str_of(&x.info, "id"));
            }
            later = true;
        }
        let mut bounds: HashSet<String> = HashSet::new();
        let mut unsure: Vec<&str> = Vec::new();
        let mut last_user: Option<&SnapMessage> = None;
        let mut last_reply: Option<&SnapMessage> = None;
        let mut answered = false;
        let mut user_began = false;
        for x in msgs {
            if assistant(x) {
                last_reply = Some(x);
                answered = true;
                continue;
            }
            let Some(id) =
                str_of(&x.info, "id").filter(|_| str_of(&x.info, "role") == Some("user"))
            else {
                continue;
            };
            let begins = match (live.msg_turn.get(id).cloned().flatten(), last_reply) {
                (Some(t), _) => Some(t == id),
                // Nothing replied in the root yet.
                (None, None) => Some(true),
                // The latest prompt began a turn and got no reply: its run
                // ended without one.
                (None, Some(_)) if !answered && user_began => Some(true),
                (None, Some(a)) => {
                    let ends = if answered {
                        ends_run(a, last_user, Some(x))
                    } else {
                        // The latest prompt joined the run of `a`, which then
                        // goes on to answer it unless `a` ended it outright.
                        ends_outright(a)
                    };
                    let a_id = str_of(&a.info, "id").unwrap_or_default();
                    if abandoned.contains(a_id) {
                        Some(true)
                    } else if !ends {
                        Some(false)
                    } else {
                        // It ended at `a`'s completion: before this message
                        // was written, or in the same millisecond, which
                        // cannot be told apart from just after.
                        let done = obj(&a.info, "time").and_then(|t| t.get("completed"));
                        match done
                            .and_then(Value::as_f64)
                            .map(|at| at.total_cmp(&created(&x.info)))
                        {
                            Some(std::cmp::Ordering::Less) => Some(true),
                            Some(std::cmp::Ordering::Equal) => None,
                            _ => Some(false),
                        }
                    }
                }
            };
            match begins {
                Some(true) => {
                    bounds.insert(id.to_string());
                }
                None => unsure.push(id),
                Some(false) => {}
            }
            user_began = begins == Some(true);
            last_user = Some(x);
            answered = false;
        }
        let over = answered && last_reply.is_some_and(|a| ends_run(a, last_user, None));
        let busy = matches!(
            self.snap
                .status
                .get(&r)
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str),
            Some("busy" | "retry")
        );
        let mut turn_msgs: HashMap<String, Vec<&'a SnapMessage>> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut cur: Option<String> = None;
        for x in msgs {
            let Some(id) = str_of(&x.info, "id") else {
                continue;
            };
            let role = str_of(&x.info, "role").unwrap_or_default();
            if role == "user" && bounds.contains(id) {
                cur = Some(id.to_string());
                order.push(id.to_string());
            }
            let by_parent = || {
                (role == "assistant")
                    .then(|| str_of(&x.info, "parentID"))
                    .flatten()
                    .and_then(|p| self.m.msg_turn.get(p).cloned().flatten())
            };
            let Some(t) = live
                .msg_turn
                .get(id)
                .cloned()
                .flatten()
                .or_else(by_parent)
                .or_else(|| cur.clone())
            else {
                continue;
            };
            let slot = self.m.msg_turn.entry(id.to_string()).or_insert(None);
            let t = slot.get_or_insert(t).clone();
            self.m.msg_role.insert(id.to_string(), role.to_string());
            if role == "assistant" && x.info.get("summary") == Some(&Value::Bool(true)) {
                self.m.msg_summary.insert(id.to_string());
            }
            self.m
                .turn_root
                .entry(t.clone())
                .or_insert_with(|| r.clone());
            turn_msgs.entry(t).or_default().push(x);
        }
        let kids: Vec<&'a SnapSession> = self
            .snap
            .sessions
            .iter()
            .filter(|s| session_id(s) != r && self.m.root(session_id(s)) == r)
            .collect();
        self.done_kids.clear();
        let starts: HashMap<&str, f64> = msgs
            .iter()
            .filter_map(|x| str_of(&x.info, "id").map(|id| (id, created(&x.info))))
            .collect();
        let doubted: HashSet<String> = unsure
            .iter()
            .filter_map(|u| self.m.msg_turn.get(*u).cloned().flatten())
            .collect();
        for (k, t) in order.iter().enumerate() {
            let next = order.get(k + 1).map(|n| starts[n.as_str()]);
            let msgs = turn_msgs.remove(t).unwrap_or_default();
            // OpenCode writes a prompt before it marks the root busy, so a
            // prompt with no reply yet on an idle root has not been answered
            // yet: its turn goes on.
            let replied = msgs
                .iter()
                .any(|x| str_of(&x.info, "role") == Some("assistant"));
            let finished = next.is_some() || (!busy && replied);
            self.proof = if doubted.contains(t) {
                Some(Unproven::Start)
            } else if replied && busy && over && next.is_none() {
                Some(Unproven::Over)
            } else if replied {
                None
            } else if busy {
                Some(Unproven::Reply)
            } else {
                Some(Unproven::Idle)
            };
            out.extend(self.turn(&r, t, &msgs, &kids, finished, next));
        }
        out
    }

    fn turn(
        &mut self,
        r: &str,
        t: &str,
        msgs: &[&'a SnapMessage],
        kids: &[&'a SnapSession],
        finished: bool,
        next: Option<f64>,
    ) -> Vec<Emit> {
        let last = next.is_none();
        let proof = self.proof.take();
        self.open.clear();
        self.still_running = false;
        let finished = finished || self.m.closed.contains(t);
        let mut out = self.messages(r, t, msgs, finished);
        // Children not reached through a task part: placed by creation time.
        for kid in kids {
            let id = session_id(kid);
            let at = created(&kid.info);
            let inside = msgs.first().is_some_and(|first| at >= created(&first.info))
                && next.is_none_or(|next| at < next);
            if !self.done_kids.contains(id) && inside {
                self.done_kids.insert(id.to_string());
                let kid_msgs: Vec<&SnapMessage> = kid.messages.iter().collect();
                out.extend(self.messages(id, t, &kid_msgs, finished));
            }
        }
        if self.m.closed.contains(t) {
            return out;
        }
        let open: BTreeSet<String> = std::mem::take(&mut self.open);
        if self.m.unknown.contains(t) {
            // The process ended with this turn open: only a persisted session
            // showing it whole closes it.
            if finished
                && proof.is_none()
                && open.is_empty()
                && !self.still_running
                && !self.m.running_in(t)
            {
                out.extend(self.m.terminal(r, t, None));
            }
            return out;
        }
        if !finished {
            // Busy: the same turn goes on, merged with what was seen live.
            self.m.current.insert(r.to_string(), t.to_string());
            self.m
                .open_msgs
                .entry(t.to_string())
                .or_default()
                .extend(open);
            match proof {
                Some(why) => self.m.unproven.insert(t.to_string(), why),
                None => self.m.unproven.remove(t),
            };
            return out;
        }
        if self.m.current.get(r).map(String::as_str) == Some(t) {
            self.m.current.remove(r);
        }
        if proof.is_some() {
            // It got no reply, or where it began could not be told: how it
            // ended is not persisted.
            out.extend(self.m.unknown_end(t));
            return out;
        }
        if last && (!open.is_empty() || self.still_running) {
            // Idle, but a message has not completed or the server still shows
            // a tool running: as live, wait for it.
            self.m.set_idle_pending(t, r);
            self.m.open_msgs.insert(t.to_string(), open);
            return out;
        }
        if !open.is_empty() && !self.live.turn_root.contains_key(t) {
            // A message of it was abandoned and how the turn ended was never
            // seen: its outcome is unknown, as for a turn open at an exit.
            out.extend(self.m.unknown_end(t));
            return out;
        }
        // A later turn began: what is still open was ended by it, as live.
        let forced = (!open.is_empty() && self.m.running_in(t)).then_some("next_turn");
        out.extend(self.m.terminal(r, t, forced));
        out
    }

    fn messages(
        &mut self,
        ses: &str,
        t: &str,
        msgs: &[&'a SnapMessage],
        finished: bool,
    ) -> Vec<Emit> {
        let mut out = Vec::new();
        for x in msgs {
            let info = &x.info;
            let Some(id) = str_of(info, "id") else {
                continue;
            };
            let role = str_of(info, "role").unwrap_or_default();
            let slot = self.m.msg_turn.entry(id.to_string()).or_insert(None);
            let turn = slot.get_or_insert_with(|| t.to_string()).clone();
            self.m.msg_role.insert(id.to_string(), role.to_string());
            if role == "user" && ses == self.m.root(ses) {
                out.extend(self.m.selection_change(ses, id, info, &turn));
            }
            let error = error_name(info);
            let done = completed(info);
            let assistant = role == "assistant";
            if assistant && !done && !self.m.done_msgs.contains(id) {
                self.open.insert(id.to_string());
            }
            if assistant && done {
                self.m.done_msgs.insert(id.to_string());
                if let Some(open) = self.m.open_msgs.get_mut(t) {
                    open.remove(id);
                }
                if error == Some(ABORT) && ses == self.m.root(ses) {
                    self.m.root_abort.insert(t.to_string());
                }
                if let Some(name) = error.filter(|n| *n != ABORT && *n != OVERFLOW) {
                    if !self
                        .m
                        .error_seen
                        .contains(&(ses.to_string(), t.to_string()))
                    {
                        let data = obj(info, "error").and_then(|e| obj(e, "data"));
                        out.extend(self.m.error_fact(ses, t, name, data));
                    }
                }
            }
            for part in &x.parts {
                out.extend(self.part(ses, t, role, error, part, finished));
                out.extend(self.pending_for(part));
            }
            if assistant && done {
                out.extend(self.m.part_closed(Some(t), None, Some(id)));
            }
        }
        out
    }

    fn part(
        &mut self,
        ses: &str,
        t: &str,
        role: &str,
        error: Option<&str>,
        part: &'a SnapPart,
        finished: bool,
    ) -> Vec<Emit> {
        let pt = match part {
            SnapPart::Whole(pt) => pt,
            SnapPart::Stub {
                part_type, status, ..
            } => {
                // Read no facts from it; a tool kept only as a stub has ended
                // only if the stub says so.
                let ended = matches!(status.as_deref(), Some("completed" | "error"));
                if part_type.as_deref() == Some("tool") && !ended {
                    self.still_running = true;
                }
                return Vec::new();
            }
        };
        let kind = str_of(pt, "type").unwrap_or_default();
        if matches!(kind, "text" | "reasoning") && role == "assistant" && error == Some(ABORT) {
            // Whether it was cut cannot be told from a snapshot.
            return Vec::new();
        }
        if kind == "tool" {
            let id = str_of(pt, "id").unwrap_or_default();
            let state = obj(pt, "state");
            let status = state.and_then(|s| str_of(s, "status")).unwrap_or_default();
            let child = state
                .and_then(|s| obj(s, "metadata"))
                .and_then(|m| str_of(m, "sessionId"))
                .map(str::to_string);
            if status == "running" && finished {
                // The server still shows it running: no outcome to give.
                self.still_running = true;
                let mut out = Vec::new();
                if !self.m.running.iter().any(|r| r.part_id == id) {
                    let calls = self.m.tool(pt, ses, id, Some(t.to_string()));
                    out.extend(calls.into_iter().take(1));
                    self.m.running.retain(|r| r.part_id != id);
                }
                out.extend(self.child(child.as_deref(), t, finished));
                return out;
            }
            let task = str_of(pt, "tool") == Some("task");
            if task && matches!(status, "running" | "completed" | "error") {
                // A task's child is read between its call and its result.
                let mut started = pt.clone();
                if let Some(Value::Object(state)) = started.get_mut("state") {
                    state.insert("status".into(), json!("running"));
                }
                let mut out = self.m.tool(&started, ses, id, Some(t.to_string()));
                out.extend(self.child(child.as_deref(), t, finished));
                if status != "running" {
                    out.extend(self.m.on_part_updated(pt));
                }
                return out;
            }
        }
        self.m.on_part_updated(pt)
    }

    fn child(&mut self, child: Option<&str>, t: &str, finished: bool) -> Vec<Emit> {
        let Some(&kid) = child.and_then(|c| self.sessions.get(c)) else {
            return Vec::new();
        };
        let id = session_id(kid);
        if !self.done_kids.insert(id.to_string()) {
            return Vec::new();
        }
        let msgs: Vec<&SnapMessage> = kid.messages.iter().collect();
        self.messages(id, t, &msgs, finished)
    }

    /// One listed request, judged by the plugin's verdict and the mirror of
    /// the cards open. A request judged dead retires its open card as ended,
    /// or, with no card, is remembered when the reason is final: the plugin's
    /// own record says it was asked before this activation or ended by a
    /// stop or a reload, so a card frame still in flight is not shown. The
    /// other dead reasons are re-judged by the next snapshot and hide nothing
    /// for the life of the run. A request the plugin could not judge changes
    /// nothing. A live request whose card is open is not rebuilt: its bytes
    /// are the stored card's. A request list that failed clears nothing and
    /// retires nothing.
    fn ask(&mut self, request: &SnapRequest) -> Vec<Emit> {
        let Some(id) = request.id() else {
            return Vec::new();
        };
        if !self.placed.insert(id.to_string()) {
            return Vec::new();
        }
        let open = self.m.open_cards.iter().any(|c| c.request_id == id);
        if self.snap.requests_ok {
            if let Some(reason) = request.verdict.dead_reason() {
                if open && reason != "absent" {
                    return self.m.ended(id);
                }
                if !open && final_reason(reason) {
                    self.m.remember_retired(id);
                }
                return Vec::new();
            }
            if !request.verdict.is_live() {
                return Vec::new();
            }
        }
        if open {
            return Vec::new();
        }
        self.m.asked(request)
    }

    /// A pending request goes right after the tool call it is about, where it
    /// was asked live.
    fn pending_for(&mut self, part: &SnapPart) -> Vec<Emit> {
        let key = match part {
            SnapPart::Whole(pt) if str_of(pt, "type") == Some("tool") => {
                str_of(pt, "messageID").zip(str_of(pt, "callID"))
            }
            SnapPart::Stub { ids, .. } => ids.message_id.as_deref().zip(ids.call_id.as_deref()),
            _ => None,
        };
        let Some((message, call)) = key else {
            return Vec::new();
        };
        let Some(&request) = self.pending.get(&(message.to_string(), call.to_string())) else {
            return Vec::new();
        };
        self.ask(request)
    }
}

/// A dead verdict the next snapshot cannot overturn: the plugin's own record
/// of the activation says so.
fn final_reason(reason: &str) -> bool {
    reason.starts_with("epoch:") || matches!(reason, "asked-before-activation" | "epoch-evicted")
}

/// What a rejected permission and a dismissed question leave on their tool
/// part (`core/src/v1/permission.ts`, `opencode/src/question/index.ts`).
/// OpenCode's run stops on either (`session/processor.ts` 199-201, 693-695).
const REJECTED: [&str; 2] = [
    "The user rejected permission to use this specific tool call.",
    "The user dismissed this question",
];

fn whole(x: &SnapMessage) -> impl Iterator<Item = &Map<String, Value>> {
    x.parts.iter().filter_map(|p| match p {
        SnapPart::Whole(pt) => Some(pt),
        SnapPart::Stub { .. } => None,
    })
}

/// Whether OpenCode's run stopped at the root's assistant message `a`
/// whatever was written after it (paths under `packages/opencode/src`): on an
/// error — an abort, and every error the run stops on; a context overflow it
/// compacts past sets none (`session/processor.ts` 615-631) — on structured
/// output (`session/prompt.ts` 1288-1291), or on a rejected permission or a
/// dismissed question.
fn ends_outright(a: &SnapMessage) -> bool {
    truthy(a.info.get("error"))
        || a.info.contains_key("structured")
        || whole(a).any(|pt| {
            let state = obj(pt, "state");
            str_of(pt, "type") == Some("tool")
                && state.and_then(|s| str_of(s, "status")) == Some("error")
                && state
                    .and_then(|s| str_of(s, "error"))
                    .is_some_and(|e| REJECTED.contains(&e))
        })
}

/// Whether OpenCode's busy period ended with the root's assistant message
/// `a`, given the latest user message before it and the user message `next`
/// written after it, if one was:
///
///   * it stopped outright ([`ends_outright`]);
///   * a `!command`'s message ends the busy period it ran in: it has no
///     `finish` and no step-start, which every step of the loop opens with
///     (`session/prompt.ts` 451-590, `effect/runner.ts` startShell);
///   * otherwise the loop's own exit test (`session/prompt.ts` 1105-1131): a
///     finish other than `tool-calls` and `unknown`, no tool call, and a reply
///     to the latest prompt. A compaction summary passes that test, but one
///     the loop ran by itself goes on to the prompt it wrote after it
///     ([`written_after_summary`]).
fn ends_run(a: &SnapMessage, last_user: Option<&SnapMessage>, next: Option<&SnapMessage>) -> bool {
    if ends_outright(a) {
        return true;
    }
    let info = &a.info;
    if !completed(info) {
        return false;
    }
    let Some(finish) = str_of(info, "finish") else {
        return !whole(a).any(|pt| str_of(pt, "type") == Some("step-start"));
    };
    // A tool call the loop answers: not one the provider ran, nor one cut by
    // an abort or a retry (`session/prompt.ts` 96-100).
    let tool_call = a.parts.iter().any(|p| match p {
        SnapPart::Whole(pt) => {
            let state = obj(pt, "state");
            str_of(pt, "type") == Some("tool")
                && !obj(pt, "metadata").is_some_and(|m| truthy(m.get("providerExecuted")))
                && !(state.and_then(|s| str_of(s, "status")) == Some("error")
                    && state
                        .and_then(|s| obj(s, "metadata"))
                        .is_some_and(|m| m.get("interrupted") == Some(&Value::Bool(true))))
        }
        SnapPart::Stub { part_type, .. } => part_type.as_deref() == Some("tool"),
    });
    let user = last_user.and_then(|u| str_of(&u.info, "id"));
    if matches!(finish, "tool-calls" | "unknown")
        || tool_call
        || user.is_none()
        || str_of(info, "parentID") != user
    {
        return false;
    }
    let summary = info.get("summary") == Some(&Value::Bool(true));
    !(summary && last_user.is_some_and(|c| written_after_summary(c, next)))
}

/// Whether the compaction prompt `c` is one the loop wrote itself, and `next`
/// the prompt it wrote after the summary, or one it is yet to write
/// (`session/compaction.ts` 468-546): the overflowing prompt replayed, or,
/// with no overflow, a synthetic prompt to continue. A `/compact` writes
/// neither (`server/routes/instance/httpapi/handlers/session.ts` 273-292).
fn written_after_summary(c: &SnapMessage, next: Option<&SnapMessage>) -> bool {
    let Some(part) = whole(c).find(|pt| str_of(pt, "type") == Some("compaction")) else {
        return false;
    };
    truthy(part.get("auto"))
        && (truthy(part.get("overflow"))
            || next.is_none_or(|next| {
                next.parts
                    .iter()
                    .all(|p| matches!(p, SnapPart::Whole(pt) if is_filler(pt)))
            }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode_link::{decode, SeqFilter, SyncAssembly};

    const LINK_V1: &str = include_str!("../../../fixtures/opencode/link-v1-frames.jsonl");
    const TIMELINE_BUS: &str =
        include_str!("../../../fixtures/opencode/s8-timeline-bus-1.18.34.jsonl");
    const LINKCUT: &str =
        include_str!("../../../fixtures/opencode/s8-linkcut-ccd-in-1.18.34.jsonl");
    const LINKCUT_OVERLAP: &str =
        include_str!("../../../fixtures/opencode/s8-linkcut-overlap-ccd-in-1.18.34.jsonl");
    const LINKCUT_UNCUT: &str =
        include_str!("../../../fixtures/opencode/s8-linkcut-uninterrupted-ccd-in-1.18.34.jsonl");
    const PENDING: &str =
        include_str!("../../../fixtures/opencode/s8-pending-ccd-in-1.18.34.jsonl");
    const PENDING_SNAPSHOTS: &str =
        include_str!("../../../fixtures/opencode/s8-pending-snapshots-1.18.34.json");

    /// Each `s8-*` capture as the plugin forwards it, and the facts it was measured
    /// to make.
    const FORWARDED: [(&str, &str, &str); 5] = [
        (
            "timeline",
            include_str!("../../../fixtures/opencode/s8-timeline-forwarded-1.18.34.jsonl"),
            include_str!("../../../fixtures/opencode/s8-timeline-expected-1.18.34.jsonl"),
        ),
        (
            "resume-after-revert",
            include_str!(
                "../../../fixtures/opencode/s8-resume-after-revert-forwarded-1.18.34.jsonl"
            ),
            include_str!(
                "../../../fixtures/opencode/s8-resume-after-revert-expected-1.18.34.jsonl"
            ),
        ),
        (
            "lifecycle",
            include_str!("../../../fixtures/opencode/s8-lifecycle-forwarded-1.18.34.jsonl"),
            include_str!("../../../fixtures/opencode/s8-lifecycle-expected-1.18.34.jsonl"),
        ),
        (
            "dispose-childerr",
            include_str!("../../../fixtures/opencode/s8-dispose-childerr-forwarded-1.18.34.jsonl"),
            include_str!("../../../fixtures/opencode/s8-dispose-childerr-expected-1.18.34.jsonl"),
        ),
        (
            "resync",
            include_str!("../../../fixtures/opencode/s8-resync-forwarded-1.18.34.jsonl"),
            include_str!("../../../fixtures/opencode/s8-resync-expected-1.18.34.jsonl"),
        ),
    ];

    fn key() -> SessionKey {
        SessionKey::new("01K1B3XQ8ZC0DE5FGH7JKMNPQR", "cc-1")
    }

    /// A Mac the tests describe: a pinned clock, a home folder, the run's
    /// folder, the files that exist, the links among them, and the config
    /// files. Nothing of the real filesystem is read.
    #[derive(Debug, Default)]
    pub(crate) struct FakeEnv {
        pub(crate) now: String,
        pub(crate) home: Option<PathBuf>,
        pub(crate) worktree: PathBuf,
        /// Paths that exist, each with its `(device, inode)`.
        pub(crate) files: HashMap<PathBuf, (u64, u64)>,
        /// Link → target, for paths whose realpath is another path.
        pub(crate) links: HashMap<PathBuf, PathBuf>,
        /// Config files by path; `Err` is a file that cannot be read.
        pub(crate) texts: HashMap<PathBuf, Result<String, String>>,
        pub(crate) vars: HashMap<String, String>,
    }

    impl FakeEnv {
        pub(crate) fn ada() -> FakeEnv {
            let mut env = FakeEnv {
                now: "2023-11-14T22:13:20Z".into(),
                home: Some(PathBuf::from("/Users/ada")),
                worktree: PathBuf::from("/Users/ada/project"),
                ..FakeEnv::default()
            };
            for (n, dir) in [
                "/",
                "/Users",
                "/Users/ada",
                "/Users/ada/project",
                "/Users/ada/outside",
            ]
            .iter()
            .enumerate()
            {
                env.files.insert(PathBuf::from(dir), (1, n as u64 + 1));
            }
            env
        }

        pub(crate) fn with_file(mut self, path: &str) -> FakeEnv {
            let n = self.files.values().map(|(_, ino)| *ino).max().unwrap_or(0) + 1;
            self.files.insert(PathBuf::from(path), (1, n));
            self
        }

        pub(crate) fn with_link(mut self, link: &str, target: &str) -> FakeEnv {
            self.links
                .insert(PathBuf::from(link), PathBuf::from(target));
            self
        }

        pub(crate) fn with_text(mut self, path: &str, text: &str) -> FakeEnv {
            self.texts.insert(PathBuf::from(path), Ok(text.to_string()));
            self
        }

        fn target(&self, path: &Path) -> PathBuf {
            self.links
                .get(path)
                .cloned()
                .unwrap_or_else(|| path.to_path_buf())
        }
    }

    impl CardEnv for FakeEnv {
        fn now(&self) -> String {
            self.now.clone()
        }

        fn home(&self) -> Option<PathBuf> {
            self.home.clone()
        }

        fn worktree(&self) -> PathBuf {
            self.worktree.clone()
        }

        fn exists(&self, path: &Path) -> bool {
            self.files.contains_key(path) || self.links.contains_key(path)
        }

        fn realpath(&self, path: &Path) -> Option<PathBuf> {
            let target = self.target(path);
            self.files.contains_key(&target).then_some(target)
        }

        fn identity(&self, path: &Path) -> Option<(u64, u64)> {
            self.files.get(&self.target(path)).copied()
        }

        fn read_text(&self, path: &Path) -> std::io::Result<Option<String>> {
            match self.texts.get(path) {
                None => Ok(None),
                Some(Ok(text)) => Ok(Some(text.clone())),
                Some(Err(why)) => Err(std::io::Error::other(why.clone())),
            }
        }

        fn env_var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
    }

    fn adapter() -> OpencodeAdapter {
        OpencodeAdapter::new(key(), Arc::new(FakeEnv::ada()))
    }

    fn rows(text: &str) -> Vec<Value> {
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("a JSON row"))
            .collect()
    }

    fn frame(value: Value) -> LinkFrame {
        decode(value.to_string().as_bytes())
            .unwrap_or_else(|err| panic!("{err}: {value}"))
            .expect("a known frame")
    }

    fn ev(event: &str, properties: Value) -> LinkFrame {
        frame(json!({"t": "ev", "seq": 0, "type": event, "properties": properties}))
    }

    fn sid(event: &PendingEvent) -> &str {
        event.source_event_id.as_deref().unwrap_or_default()
    }

    /// The daemon's dedup: `(source, source_event_id)`, first wins.
    #[derive(Default, Clone)]
    struct Store {
        events: Vec<PendingEvent>,
        keys: HashSet<String>,
    }

    impl Store {
        fn record(&mut self, events: Vec<PendingEvent>) {
            for event in events {
                if self.keys.insert(sid(&event).to_string()) {
                    self.events.push(event);
                }
            }
        }

        /// The cards the store holds open: asked, never resolved.
        fn open_cards(&self) -> Vec<(String, String, RequestKind, Option<String>)> {
            let resolved: HashSet<&str> = self
                .events
                .iter()
                .filter(|e| e.kind == EventKind::ApprovalResolved)
                .filter_map(|e| e.item_id.as_deref())
                .collect();
            self.events
                .iter()
                .filter(|e| e.kind == EventKind::ApprovalRequest)
                .filter(|e| !resolved.contains(e.item_id.as_deref().unwrap_or_default()))
                .map(|e| {
                    let (session, rest) = sid(e).split_once(':').unwrap();
                    let session = e.payload["subagent_session"].as_str().unwrap_or(session);
                    let kind = if rest.starts_with("question:") {
                        RequestKind::Question
                    } else {
                        RequestKind::Permission
                    };
                    let id = e.item_id.clone().unwrap();
                    (id, session.to_string(), kind, e.turn_id.clone())
                })
                .collect()
        }

        fn turn_completes(&self) -> usize {
            self.events
                .iter()
                .filter(|e| e.kind == EventKind::TurnComplete)
                .count()
        }
    }

    /// One run as the daemon drives it: events, snapshots and settles in the
    /// order the capture holds them, a restarted daemon as a fresh adapter over
    /// the same store, and a new OpenCode process as a new run.
    #[derive(Clone)]
    struct Run {
        adapter: OpencodeAdapter,
        store: Store,
        unknown: BTreeSet<String>,
        restart: Option<u64>,
        syncs: u64,
    }

    impl Run {
        fn new() -> Self {
            Run {
                adapter: adapter(),
                store: Store::default(),
                unknown: BTreeSet::new(),
                restart: None,
                syncs: 0,
            }
        }

        fn replay(text: &str) -> Run {
            let mut run = Run::new();
            for row in rows(text) {
                run.row(&row);
            }
            run
        }

        fn retire(&mut self) {
            self.unknown.extend(self.adapter.unknown.iter().cloned());
            self.adapter = adapter();
        }

        fn daemon_restart(&mut self) {
            self.retire();
            for (id, session, kind, turn) in self.store.open_cards() {
                self.adapter.restore_open_card(&id, &session, kind, turn);
            }
        }

        fn row(&mut self, row: &Value) {
            let restart = row.get("_restart").and_then(Value::as_u64);
            if restart.is_some() && self.restart.is_some() && restart != self.restart {
                self.daemon_restart();
            }
            self.restart = restart.or(self.restart);
            let event = row["type"].as_str().unwrap();
            let properties = &row["properties"];
            let facts = match event {
                "ccd.session.end" => self.adapter.session_end(),
                "ccd.link.opened" if properties["new_process"] == true => {
                    self.retire();
                    Vec::new()
                }
                "ccd.resync" => return self.resync(properties),
                "ccd.resync_settled" => self.adapter.settle(),
                other if other.starts_with("ccd.") || other.starts_with("collector.") => Vec::new(),
                other => self.adapter.ingest(&ev(other, properties.clone())),
            };
            self.store.record(facts);
        }

        /// A captured snapshot, sent as the plugin sends one: the pending
        /// requests, then every session followed by its messages newest first.
        fn resync(&mut self, snap: &Value) {
            self.syncs += 1;
            let sync = self.syncs;
            let mut frames = vec![frame(json!({
                "t": "sync_begin", "sync": sync, "reason": "connect", "scope": "full",
                "as_of": snap["as_of"], "status": snap["status"], "status_ok": snap["status_ok"],
            }))];
            let verdict = json!({"live": true, "dead": null, "listed": true, "in_store": true});
            for (kind, list) in [("permission", "permissions"), ("question", "questions")] {
                for request in snap[list].as_array().into_iter().flatten() {
                    frames.push(frame(json!({
                        "t": "sync_request", "sync": sync, "kind": kind, "request": request,
                        "verdict": verdict,
                    })));
                }
            }
            let mut items = Vec::new();
            for session in snap["sessions"].as_array().into_iter().flatten() {
                items.push(json!({"session": session["info"]}));
                for message in session["messages"].as_array().into_iter().flatten().rev() {
                    items.push(json!({
                        "sessionID": session["info"]["id"], "info": message["info"],
                        "parts": message["parts"],
                    }));
                }
            }
            let pages = u64::from(!items.is_empty());
            let count = items.len();
            if !items.is_empty() {
                frames.push(frame(
                    json!({"t": "sync_page", "sync": sync, "n": 0, "items": items}),
                ));
            }
            let end = frame(json!({
                "t": "sync_end", "sync": sync, "done_at": snap["done_at"], "list_ok": true,
                "permissions_ok": true, "questions_ok": true, "requests_ok": true, "lower": {},
                "pages": pages, "items": count, "bytes": 0, "activation": 1,
            }));
            self.adapter.sync_begin();
            let mut assembly = SyncAssembly::begin(&frames[0]).unwrap();
            for f in &frames[1..] {
                assembly.add(f);
            }
            let snapshot = assembly.finish(&end).unwrap();
            let (facts, staged) = self
                .adapter
                .plan_resync(&snapshot)
                .expect("a whole snapshot");
            let keys: HashSet<&str> = facts.iter().map(sid).collect();
            assert_eq!(keys.len(), facts.len(), "one fact per key in a snapshot");
            self.store.record(facts);
            self.adapter.apply_resync(staged);
        }

        fn unknown(&self) -> BTreeSet<String> {
            let mut all = self.unknown.clone();
            all.extend(self.adapter.unknown.iter().cloned());
            all
        }

        /// Every turn ends exactly once: one TurnComplete, or outcome unknown,
        /// never both.
        fn assert_one_end_per_turn(&self, name: &str) {
            let unknown = self.unknown();
            let turns: BTreeSet<&str> = self
                .store
                .events
                .iter()
                .filter_map(|e| e.turn_id.as_deref())
                .collect();
            for turn in turns {
                let ends = self
                    .store
                    .events
                    .iter()
                    .filter(|e| {
                        e.kind == EventKind::TurnComplete && e.turn_id.as_deref() == Some(turn)
                    })
                    .count();
                assert!(ends <= 1, "{name}: {turn} completed {ends} times");
                assert!(
                    (ends == 1) != unknown.contains(turn),
                    "{name}: {turn} has {ends} TurnComplete and unknown={}",
                    unknown.contains(turn)
                );
            }
        }
    }

    fn is_card(kind: &str) -> bool {
        kind == "approval_request" || kind == "approval_resolved"
    }

    /// A fact as the comparison reads it. A card is compared by identity: its
    /// payload is this module's card, built after the capture was measured.
    fn view(kind: &str, turn: &Value, item: &Value, payload: &Value) -> Value {
        if is_card(kind) {
            json!([kind, turn, item])
        } else {
            json!([kind, turn, item, payload])
        }
    }

    fn mine(store: &Store) -> Vec<(String, Value)> {
        store
            .events
            .iter()
            .map(|e| {
                let v = view(
                    e.kind.as_str(),
                    &json!(e.turn_id),
                    &json!(e.item_id),
                    &e.payload,
                );
                (sid(e).to_string(), v)
            })
            .collect()
    }

    fn measured(text: &str) -> Vec<(String, Value)> {
        rows(text)
            .into_iter()
            .filter(|r| r["source"] == "opencode")
            .map(|r| {
                let v = view(
                    r["kind"].as_str().unwrap(),
                    &r["turn_id"],
                    &r["item_id"],
                    &r["payload"],
                );
                (r["source_event_id"].as_str().unwrap().to_string(), v)
            })
            .collect()
    }

    /// What differs between two fact lists: missing, extra and changed facts,
    /// and whether the facts that are not cards come in the same order.
    fn diff(got: &[(String, Value)], want: &[(String, Value)]) -> Vec<String> {
        let g: HashMap<&str, &Value> = got.iter().map(|(k, v)| (k.as_str(), v)).collect();
        let w: HashMap<&str, &Value> = want.iter().map(|(k, v)| (k.as_str(), v)).collect();
        let mut out = Vec::new();
        for (k, v) in want {
            match g.get(k.as_str()) {
                None => out.push(format!("missing {k}")),
                Some(mine) if *mine != v => out.push(format!("changed {k}: {mine} != {v}")),
                _ => {}
            }
        }
        out.extend(
            got.iter()
                .filter(|(k, _)| !w.contains_key(k.as_str()))
                .map(|(k, _)| format!("extra {k}")),
        );
        let order = |list: &[(String, Value)], other: &HashMap<&str, &Value>| -> Vec<String> {
            list.iter()
                .filter(|(k, v)| other.contains_key(k.as_str()) && !is_card(v[0].as_str().unwrap()))
                .map(|(k, _)| k.clone())
                .collect()
        };
        if order(got, &w) != order(want, &g) {
            out.push("facts that are not cards come in another order".into());
        }
        out
    }

    #[test]
    fn every_forwarded_capture_makes_the_measured_facts_in_the_measured_order() {
        for (name, forwarded, expected) in FORWARDED {
            let run = Run::replay(forwarded);
            let diffs = diff(&mine(&run.store), &measured(expected));
            assert!(diffs.is_empty(), "{name}: {diffs:#?}");
            run.assert_one_end_per_turn(name);
        }
    }

    /// The plugin forwards one start marker per streaming part, a tool's
    /// `running` once, and a message's info only when it changes — and starts
    /// that memory over on every activation. The raw bus is every update any
    /// activation could have forwarded, so a run fed all of it is a run whose
    /// plugin was reactivated before every event, with no state handed to it.
    #[test]
    fn a_plugin_reactivated_at_any_point_changes_no_fact() {
        let forwarded = Run::replay(FORWARDED[0].1);
        let raw = Run::replay(TIMELINE_BUS);
        assert_eq!(raw.store.turn_completes(), 19);
        let diffs = diff(&mine(&raw.store), &mine(&forwarded.store));
        assert!(diffs.is_empty(), "{diffs:#?}");
        raw.assert_one_end_per_turn("timeline, raw bus");
    }

    /// OpenCode's ids, numbered by first appearance, and the measured
    /// durations dropped: two live runs of one script differ in nothing else.
    fn canonical(store: &Store) -> Vec<String> {
        let mut names: HashMap<String, String> = HashMap::new();
        store
            .events
            .iter()
            .map(|e| {
                let mut payload = e.payload.clone();
                if let Some(p) = payload.as_object_mut() {
                    p.remove("duration_ms");
                }
                let text =
                    json!([e.kind.as_str(), sid(e), e.turn_id, e.item_id, payload]).to_string();
                rename(&text, &mut names)
            })
            .collect()
    }

    fn rename(text: &str, names: &mut HashMap<String, String>) -> String {
        const PREFIXES: [&str; 6] = ["ses_", "msg_", "prt_", "per_", "que_", "call_"];
        let mut out = String::new();
        let mut rest = text;
        while !rest.is_empty() {
            let boundary = !out.ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
            if let Some(prefix) = PREFIXES.iter().find(|p| boundary && rest.starts_with(**p)) {
                let len = rest[prefix.len()..]
                    .find(|c: char| !c.is_ascii_alphanumeric())
                    .map_or(rest.len(), |n| n + prefix.len());
                let n = names.len();
                out.push_str(
                    names
                        .entry(rest[..len].to_string())
                        .or_insert_with(|| format!("{prefix}{n}")),
                );
                rest = &rest[len..];
            } else {
                let c = rest.chars().next().unwrap();
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
        out
    }

    #[test]
    fn a_run_whose_link_was_cut_twice_makes_the_facts_of_a_run_never_cut() {
        let uncut = Run::replay(LINKCUT_UNCUT);
        uncut.assert_one_end_per_turn("uninterrupted");
        let want = canonical(&uncut.store);
        assert_eq!(uncut.store.turn_completes(), 4);
        for (name, text) in [("linkcut", LINKCUT), ("linkcut-overlap", LINKCUT_OVERLAP)] {
            let cut = Run::replay(text);
            cut.assert_one_end_per_turn(name);
            let got = canonical(&cut.store);
            let first = got.iter().zip(&want).position(|(g, w)| g != w);
            assert_eq!(got.len(), want.len(), "{name}: {got:#?}");
            assert_eq!(first, None, "{name}: first difference at {first:?}");
        }
    }

    /// A snapshot read while a card waits for its answer. Cut the link just
    /// before the card was asked (the ask is lost in the gap) or just after;
    /// keep the daemon or restart it. Each way, the card is open after the
    /// snapshot settles, the real reply closes it, and the run's facts are the
    /// uninterrupted run's.
    #[test]
    fn a_card_pending_across_a_cut_stays_open_until_its_real_reply() {
        let live = rows(PENDING);
        let uncut = Run::replay(PENDING);
        let want = mine(&uncut.store);
        let snapshots: Vec<Value> = serde_json::from_str(PENDING_SNAPSHOTS).unwrap();
        let seq = |row: &Value| row.get("_seq").and_then(Value::as_u64);
        for snap in &snapshots {
            let (asked_type, list) = if snap["permissions"]
                .as_array()
                .is_some_and(|l| !l.is_empty())
            {
                ("permission.asked", "permissions")
            } else {
                ("question.asked", "questions")
            };
            let request = snap[list][0]["id"].as_str().unwrap();
            let asked = live
                .iter()
                .find(|r| r["type"] == asked_type)
                .and_then(seq)
                .unwrap();
            let (as_of, done_at) = (
                snap["as_of"].as_u64().unwrap(),
                snap["done_at"].as_u64().unwrap(),
            );
            for cut in [asked - 1, asked] {
                for restart in [false, true] {
                    let label = format!("{asked_type} cut at {cut}, restart {restart}");
                    let k = live
                        .iter()
                        .position(|r| seq(r).is_some_and(|s| s > cut))
                        .unwrap();
                    let mut run = Run::new();
                    for row in &live[..k] {
                        run.row(row);
                    }
                    if restart {
                        run.daemon_restart();
                    }
                    run.resync(snap);
                    let tail: Vec<&Value> = live[k..]
                        .iter()
                        .filter(|r| seq(r).is_none_or(|s| s > as_of))
                        .collect();
                    let settle_at = tail
                        .iter()
                        .position(|r| seq(r).is_some_and(|s| s > done_at))
                        .unwrap();
                    for row in &tail[..settle_at] {
                        run.row(row);
                    }
                    run.store.record(run.adapter.settle());
                    let card: Vec<&str> = run
                        .store
                        .events
                        .iter()
                        .filter(|e| e.item_id.as_deref() == Some(request))
                        .map(|e| e.kind.as_str())
                        .collect();
                    assert_eq!(
                        card,
                        ["approval_request"],
                        "{label}: open after the snapshot settles"
                    );
                    assert!(
                        run.adapter
                            .open_cards
                            .iter()
                            .any(|c| c.request_id == request),
                        "{label}"
                    );
                    for row in &tail[settle_at..] {
                        run.row(row);
                    }
                    let diffs = diff(&mine(&run.store), &want);
                    assert!(diffs.is_empty(), "{label}: {diffs:#?}");
                    run.assert_one_end_per_turn(&label);
                }
            }
        }
    }

    /// The OpenCode server as the frames read so far describe it: what a
    /// snapshot taken at that point would hold. Built from the frames'
    /// content alone, never from what the adapter made of them.
    #[derive(Default)]
    struct Server {
        sessions: BTreeMap<String, Value>,
        /// Message → (session, info).
        messages: BTreeMap<String, (String, Value)>,
        /// Message → part → part.
        parts: BTreeMap<String, BTreeMap<String, Value>>,
        status: Map<String, Value>,
        /// Request → (the list it is on, the request).
        requests: BTreeMap<String, (&'static str, Value)>,
        /// Messages written by the running OpenCode process: a new process's
        /// snapshot pages back only to its own first prompt.
        window: HashSet<String>,
        /// The OpenCode process is running and its link could be read.
        alive: bool,
        /// A captured snapshot is being settled.
        syncing: bool,
    }

    impl Server {
        fn new() -> Self {
            Server {
                alive: true,
                ..Server::default()
            }
        }

        fn row(&mut self, row: &Value) {
            let p = &row["properties"];
            let id = |v: &Value| v.as_str().unwrap_or_default().to_string();
            match row["type"].as_str().unwrap_or_default() {
                "ccd.session.end" => self.alive = false,
                "ccd.link.opened" if p["new_process"] == true => {
                    self.alive = true;
                    self.window.clear();
                    self.status.clear();
                }
                "ccd.resync" => {
                    // What the server held when the captured snapshot was read.
                    self.syncing = true;
                    self.status = p["status"].as_object().cloned().unwrap_or_default();
                    for session in p["sessions"].as_array().into_iter().flatten() {
                        self.sessions
                            .insert(id(&session["info"]["id"]), session["info"].clone());
                        for message in session["messages"].as_array().into_iter().flatten() {
                            let (info, parts) = (&message["info"], &message["parts"]);
                            self.window.insert(id(&info["id"]));
                            self.messages
                                .insert(id(&info["id"]), (id(&info["sessionID"]), info.clone()));
                            self.parts.insert(
                                id(&info["id"]),
                                parts
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .map(|pt| (id(&pt["id"]), pt.clone()))
                                    .collect(),
                            );
                        }
                    }
                }
                "ccd.resync_settled" => self.syncing = false,
                "session.created" | "session.updated" => {
                    self.sessions
                        .insert(id(&p["info"]["id"]), p["info"].clone());
                }
                "session.deleted" => {
                    self.sessions.remove(&id(&p["info"]["id"]));
                }
                "session.status" if p["status"]["type"] == "idle" => {
                    self.status.remove(&id(&p["sessionID"]));
                }
                "session.status" => {
                    self.status.insert(id(&p["sessionID"]), p["status"].clone());
                }
                "session.idle" => {
                    self.status.remove(&id(&p["sessionID"]));
                }
                "message.updated" => {
                    let info = &p["info"];
                    self.window.insert(id(&info["id"]));
                    self.messages
                        .insert(id(&info["id"]), (id(&info["sessionID"]), info.clone()));
                }
                "message.removed" => {
                    self.messages.remove(&id(&p["messageID"]));
                    self.parts.remove(&id(&p["messageID"]));
                }
                "message.part.updated" => {
                    let part = &p["part"];
                    self.parts
                        .entry(id(&part["messageID"]))
                        .or_default()
                        .insert(id(&part["id"]), part.clone());
                }
                "message.part.removed" => {
                    if let Some(parts) = self.parts.get_mut(&id(&p["messageID"])) {
                        parts.remove(&id(&p["partID"]));
                    }
                }
                "permission.asked" => {
                    self.requests
                        .insert(id(&p["id"]), ("permissions", p.clone()));
                }
                "question.asked" => {
                    self.requests.insert(id(&p["id"]), ("questions", p.clone()));
                }
                "permission.replied" | "question.replied" | "question.rejected" => {
                    self.requests.remove(&id(&p["requestID"]));
                }
                _ => {}
            }
        }

        /// The snapshot, in the shape the captures hold one: every session
        /// with its messages oldest first, and the pending requests.
        fn snapshot(&self) -> Value {
            let mut sessions: BTreeMap<&str, Value> = BTreeMap::new();
            for (message, (session, info)) in &self.messages {
                if !self.window.contains(message) {
                    continue;
                }
                let entry = sessions.entry(session.as_str()).or_insert_with(|| {
                    let info = self.sessions.get(session).cloned();
                    json!({"info": info.unwrap_or(json!({"id": session, "time": {"created": 0}})),
                        "messages": []})
                });
                let parts: Vec<&Value> = self
                    .parts
                    .get(message)
                    .into_iter()
                    .flat_map(|p| p.values())
                    .collect();
                entry["messages"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"info": info, "parts": parts}));
            }
            for (id, info) in &self.sessions {
                sessions
                    .entry(id.as_str())
                    .or_insert_with(|| json!({"info": info, "messages": []}));
            }
            let list = |name: &str| -> Vec<&Value> {
                self.requests
                    .values()
                    .filter(|(l, _)| *l == name)
                    .map(|(_, r)| r)
                    .collect()
            };
            json!({
                "as_of": 0, "done_at": 0, "status": self.status, "status_ok": true,
                "sessions": sessions.into_values().collect::<Vec<_>>(),
                "permissions": list("permissions"), "questions": list("questions"),
            })
        }
    }

    /// A run cut at row `k`: `before` has read every row before it live; then
    /// the snapshot the plugin would send at that point, settled, then the
    /// rest live.
    fn cut_at(before: &Run, rows: &[Value], k: usize, snapshot: &Value) -> Run {
        let mut run = before.clone();
        run.resync(snapshot);
        let settled = run.adapter.settle();
        run.store.record(settled);
        for row in &rows[k..] {
            run.row(row);
        }
        run
    }

    /// Every capture with a cut at every row where a snapshot can be read:
    /// the facts are the uncut run's, and every turn ends once.
    #[test]
    fn a_snapshot_read_at_any_row_changes_no_fact() {
        let mut captures: Vec<(&str, &str)> = FORWARDED.iter().map(|(n, f, _)| (*n, *f)).collect();
        captures.extend([
            ("linkcut", LINKCUT),
            ("linkcut-overlap", LINKCUT_OVERLAP),
            ("linkcut-uninterrupted", LINKCUT_UNCUT),
            ("pending", PENDING),
        ]);
        let mut cuts = 0;
        for (name, text) in captures {
            let rows = rows(text);
            let want = mine(&Run::replay(text).store);
            let mut server = Server::new();
            let mut before = Run::new();
            for k in 0..rows.len() {
                if k > 0 {
                    server.row(&rows[k - 1]);
                    before.row(&rows[k - 1]);
                }
                if !server.alive || server.syncing {
                    continue;
                }
                cuts += 1;
                let run = cut_at(&before, &rows, k, &server.snapshot());
                let label = format!("{name} cut before row {k}");
                // A snapshot names each root it reads; a capture that begins
                // mid-session never saw that session start.
                let got: Vec<(String, Value)> = mine(&run.store)
                    .into_iter()
                    .filter(|(k, _)| {
                        !k.ends_with(":session_start") || want.iter().any(|(w, _)| w == k)
                    })
                    .collect();
                let diffs = diff(&got, &want);
                assert!(diffs.is_empty(), "{label}: {diffs:#?}");
                run.assert_one_end_per_turn(&label);
            }
        }
        assert_eq!(
            cuts, 1285,
            "a cut at every row where a snapshot can be read"
        );
    }

    /// A prompt typed while the agent's reply still streams joins its turn:
    /// a snapshot read right after it neither ends that turn early nor makes
    /// the prompt a turn of its own.
    #[test]
    fn a_snapshot_after_a_joined_prompt_keeps_one_turn() {
        let rows = rows(FORWARDED[0].1);
        let joined = rows
            .iter()
            .position(|r| r["properties"]["part"]["text"] == "s8:basic joined")
            .unwrap();
        let mut server = Server::new();
        let mut before = Run::new();
        for row in &rows[..=joined] {
            server.row(row);
            before.row(row);
        }
        let snapshot = server.snapshot();
        let session = snapshot["status"].as_object().unwrap();
        assert_eq!(session.len(), 1, "the root is busy: {snapshot}");
        let run = cut_at(&before, &rows, joined + 1, &snapshot);
        let uncut = Run::replay(FORWARDED[0].1);
        assert_eq!(run.store.turn_completes(), uncut.store.turn_completes());
        let diffs = diff(&mine(&run.store), &mine(&uncut.store));
        assert!(diffs.is_empty(), "{diffs:#?}");
        run.assert_one_end_per_turn("joined");
    }

    // ------------------------------------------------------------ synthetic

    const ROOT: &str = "ses_0f00a000000aAAAAAAAAAAAAAA";

    fn user(id: &str, agent: &str, model: Value) -> LinkFrame {
        ev(
            "message.updated",
            json!({"sessionID": ROOT, "info": {
            "id": id, "sessionID": ROOT, "role": "user", "agent": agent, "model": model,
            "time": {"created": 1}}}),
        )
    }

    fn prompt(message: &str, part: &str, text: &str) -> LinkFrame {
        ev(
            "message.part.updated",
            json!({"sessionID": ROOT, "part": {
            "id": part, "sessionID": ROOT, "messageID": message, "type": "text", "text": text}}),
        )
    }

    fn idle() -> LinkFrame {
        ev("session.idle", json!({"sessionID": ROOT}))
    }

    fn feed(adapter: &mut OpencodeAdapter, frames: &[LinkFrame]) -> Vec<PendingEvent> {
        frames.iter().flat_map(|f| adapter.ingest(f)).collect()
    }

    #[test]
    fn a_new_agent_or_model_on_the_keyboard_is_said_once_before_the_prompt() {
        let mut a = adapter();
        let model = json!({"providerID": "mock", "modelID": "m1"});
        let events = feed(
            &mut a,
            &[
                ev(
                    "session.created",
                    json!({"info": {"id": ROOT, "directory": "/Users/ada/project"}}),
                ),
                user("msg_01", "build", model.clone()),
                prompt("msg_01", "prt_01", "first"),
                idle(),
                user("msg_02", "plan", model.clone()),
                user("msg_02", "plan", model.clone()),
                prompt("msg_02", "prt_02", "second"),
                idle(),
                user(
                    "msg_03",
                    "plan",
                    json!({"providerID": "mock", "modelID": "m2", "variant": "high"}),
                ),
                prompt("msg_03", "prt_03", "third"),
                idle(),
                user(
                    "msg_04",
                    "plan",
                    json!({"providerID": "mock", "modelID": "m2", "variant": "high"}),
                ),
                prompt("msg_04", "prt_04", "fourth"),
            ],
        );
        let said: Vec<(String, &str)> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Notification | EventKind::UserMessage))
            .map(|e| {
                (
                    sid(e).to_string(),
                    e.payload["message"].as_str().unwrap_or(""),
                )
            })
            .collect();
        let at = |s: &str| format!("{ROOT}:{s}");
        assert_eq!(
            said,
            [
                (at("user:prt_01"), ""),
                (at("model:msg_02"), "plan · mock/m1"),
                (at("user:prt_02"), ""),
                (at("model:msg_03"), "plan · mock/m2 (high)"),
                (at("user:prt_03"), ""),
                (at("user:prt_04"), ""),
            ]
        );
        let notice = events
            .iter()
            .find(|e| sid(e) == at("model:msg_02"))
            .unwrap();
        assert_eq!(notice.payload["notification_type"], "model_changed");
        assert_eq!(notice.turn_id.as_deref(), Some("msg_02"));
    }

    fn asked(id: &str) -> LinkFrame {
        ev(
            "permission.asked",
            json!({"id": id, "sessionID": ROOT, "permission": "bash",
            "patterns": ["make"], "metadata": {"command": "make"}, "always": ["make *"],
            "tool": {"messageID": "msg_02", "callID": "call_1"}}),
        )
    }

    fn requests_snapshot(requests_ok: bool) -> Snapshot {
        let begin = frame(
            json!({"t": "sync_begin", "sync": 1, "reason": "trigger:session.idle",
            "scope": "requests", "as_of": 9, "status": {}, "status_ok": true}),
        );
        let end = frame(
            json!({"t": "sync_end", "sync": 1, "done_at": 9, "permissions_ok": requests_ok,
            "questions_ok": true, "requests_ok": requests_ok, "lower": {}, "pages": 0, "items": 0,
            "bytes": 0, "activation": 1}),
        );
        SyncAssembly::begin(&begin).unwrap().finish(&end).unwrap()
    }

    #[test]
    fn a_card_asked_after_the_snapshot_began_is_not_cleared_by_it() {
        let mut a = adapter();
        feed(&mut a, &[asked("per_01")]);
        a.sync_begin();
        // Asked while the snapshot is being read, after its request list.
        feed(&mut a, &[asked("per_02")]);
        let (facts, staged) = a.plan_resync(&requests_snapshot(true)).unwrap();
        assert!(facts.is_empty());
        a.apply_resync(staged);
        let cleared = a.settle();
        assert_eq!(cleared.len(), 1);
        assert_eq!(sid(&cleared[0]), format!("{ROOT}:perm_resolved:per_01"));
        assert_eq!(
            cleared[0].payload,
            json!({"request_id": "per_01", "status": "cleared", "cause": "superseded"})
        );
        let open: Vec<&str> = a.open_cards.iter().map(|c| c.request_id.as_str()).collect();
        assert_eq!(open, ["per_02"], "the newer card stays open");
    }

    #[test]
    fn a_snapshot_whose_request_list_failed_clears_nothing() {
        let mut a = adapter();
        feed(&mut a, &[asked("per_01")]);
        a.sync_begin();
        let (_, staged) = a.plan_resync(&requests_snapshot(false)).unwrap();
        a.apply_resync(staged);
        assert!(a.settle().is_empty());
        assert_eq!(a.open_cards.len(), 1);
    }

    #[test]
    fn the_contract_run_reads_through_the_filter_the_snapshots_and_the_settles() {
        let mut a = adapter();
        let mut store = Store::default();
        let mut filter = SeqFilter::default();
        let mut assembly = None;
        let mut verdicts = Vec::new();
        for row in rows(LINK_V1)
            .iter()
            .filter(|r| r["dir"] == "plugin" && r.get("case").is_none())
        {
            if row["frame"]["type"] == "opencode_hello" {
                let protocol::ipc::ClientFrame::OpencodeHello(hello) =
                    serde_json::from_value(row["frame"].clone()).unwrap()
                else {
                    panic!("not a hello");
                };
                filter.admitted(&hello);
                continue;
            }
            let f = frame(row["frame"].clone());
            assert!(filter.pass(&f), "every contract frame is new: {f:?}");
            match &f {
                LinkFrame::SyncBegin { .. } => {
                    a.sync_begin();
                    assembly = SyncAssembly::begin(&f);
                }
                LinkFrame::SyncRequest { .. } | LinkFrame::SyncPage { .. } => {
                    assembly.as_mut().unwrap().add(&f)
                }
                LinkFrame::SyncEnd { .. } => {
                    let snapshot = assembly.take().unwrap().finish(&f).unwrap();
                    verdicts.extend(snapshot.requests.iter().map(|r| r.verdict.live));
                    let (facts, staged) = a.plan_resync(&snapshot).unwrap();
                    store.record(facts);
                    a.apply_resync(staged);
                }
                LinkFrame::Settled { .. } => store.record(a.settle()),
                _ => store.record(a.ingest(&f)),
            }
        }
        assert_eq!(verdicts, [true, false, false, false]);
        let card = |id: &str| -> Vec<Value> {
            store
                .events
                .iter()
                .filter(|e| e.item_id.as_deref() == Some(id))
                .map(|e| json!([e.kind.as_str(), e.payload["status"], e.payload["cause"]]))
                .collect()
        };
        // Pending at the first snapshot, then answered at the Mac.
        assert_eq!(
            card("per_0f00a00000001xxxxxxxxxxxxx"),
            [
                json!(["approval_request", null, null]),
                json!(["approval_resolved", "answered", null])
            ]
        );
        // Listed by every snapshot and judged dead by each: never shown.
        assert_eq!(card("que_0f00b00000001xxxxxxxxxxxxx"), [] as [Value; 0]);
        // A card over the cap, asked live and gone from the next request list.
        assert_eq!(
            card("que_0f00b00000002xxxxxxxxxxxxx"),
            [
                json!(["approval_request", null, null]),
                json!(["approval_resolved", "cleared", "superseded"])
            ]
        );
        let bash = store
            .events
            .iter()
            .find(|e| sid(e).ends_with("perm:per_0f00a00000001xxxxxxxxxxxxx"))
            .unwrap();
        assert_eq!(bash.payload["tool_call_id"], "call_1");
        assert_eq!(bash.payload["answerable"], true);
        assert_eq!(bash.payload["card"]["tool_input"]["command"], "ls -la");
        assert_eq!(
            bash.payload["card"]["tool_input"]["description"],
            json!("list")
        );
        // The tool the snapshot showed running ends live, after its card.
        let result = store
            .events
            .iter()
            .position(|e| sid(e).ends_with("post:prt_0f0004200000xxxxxxxxxxxxxxx"));
        let ask = store
            .events
            .iter()
            .position(|e| sid(e).ends_with("perm:per_0f00a00000001xxxxxxxxxxxxx"));
        assert!(ask < result && result.is_some());
    }

    fn card_of(event: &PendingEvent) -> ApprovalCard {
        serde_json::from_value(event.payload["card"].clone()).unwrap()
    }

    #[test]
    fn a_card_shows_the_command_or_the_permission_and_its_text_is_the_hashed_text() {
        let mut a = adapter();
        let long = format!("echo {} ; rm -rf ~/work", "x".repeat(70_000));
        let events = feed(
            &mut a,
            &[
                ev(
                    "permission.asked",
                    json!({"id": "per_1", "sessionID": ROOT, "permission": "bash",
                    "patterns": ["ls -la"], "metadata": {"command": "ls -la"}, "always": ["ls *"]}),
                ),
                ev(
                    "permission.asked",
                    json!({"id": "per_2", "sessionID": ROOT, "permission": "bash",
                    "patterns": [long.clone()], "metadata": {"command": long.clone()}}),
                ),
                ev(
                    "permission.asked",
                    json!({"id": "per_3", "sessionID": ROOT,
                    "permission": "external_directory", "patterns": ["/Users/ada/outside/*"],
                    "metadata": {"filepath": "/Users/ada/outside/target.txt"}}),
                ),
                ev(
                    "question.asked",
                    json!({"id": "que_1", "sessionID": ROOT, "questions": [
                    {"question": "Which?", "header": "Pick", "options": [{"label": "Red"}]}]}),
                ),
                frame(
                    json!({"t": "card_stub", "seq": 1, "type": "permission.asked", "properties": {
                    "id": "per_4", "sessionID": ROOT, "permission": "bash"}, "size": 3_600_445,
                    "sha256": "ab".repeat(32)}),
                ),
            ],
        );
        let cards: Vec<ApprovalCard> = events.iter().map(card_of).collect();
        for card in &cards {
            assert_eq!(
                protocol::hash::sha256_hex(card.display_text.as_bytes()),
                card.payload_hash,
                "the phone checks the text against the hash"
            );
            assert!(card.risk.is_none() && !card.identity_bound);
        }
        assert_eq!(
            (cards[0].tool_name.as_str(), &cards[0].tool_input),
            ("command", &json!({"command": "ls -la"}))
        );
        let shown = cards[1].tool_input["command"].as_str().unwrap();
        assert!(shown.len() < 9_000 && shown.contains("bytes elided; sha256 of the whole text"));
        assert!(shown.contains(&protocol::hash::sha256_hex(long.as_bytes())));
        assert_eq!(cards[2].tool_name, "external_directory");
        assert_eq!(
            cards[2].tool_input,
            json!({"permission": "external_directory", "patterns": ["/Users/ada/outside/*"]})
        );
        assert_eq!(cards[3].tool_name, "question");
        assert_eq!(cards[3].tool_input["questions"][0]["question"], "Which?");
        assert_eq!(events[3].payload["family"], "question");
        assert!(cards[4].tool_input["message"]
            .as_str()
            .unwrap()
            .starts_with("This request is too large to show (3600445 bytes, sha256 abab"));
        // Shown, never pending: an answer is the Mac's.
        let replied = feed(
            &mut a,
            &[ev(
                "permission.replied",
                json!({"sessionID": ROOT, "requestID": "per_1", "reply": "once"}),
            )],
        );
        assert_eq!(
            replied[0].payload,
            json!({"request_id": "per_1", "status": "answered", "by": "local",
                "decision": {"type": "option_id", "option_id": "once"}})
        );
    }

    fn assistant(id: &str, parent: &str, done: bool) -> LinkFrame {
        let time = if done {
            json!({"created": 2, "completed": 3})
        } else {
            json!({"created": 2})
        };
        ev(
            "message.updated",
            json!({"sessionID": ROOT, "info": {"id": id, "sessionID": ROOT,
            "role": "assistant", "parentID": parent, "time": time}}),
        )
    }

    fn bash(message: &str, part: &str, status: &str) -> LinkFrame {
        let state = if status == "running" {
            json!({"status": "running", "input": {"command": "sleep 8"}, "time": {"start": 1}})
        } else {
            json!({"status": "completed", "input": {"command": "sleep 8"}, "output": "",
                "metadata": {"exit": 0}, "time": {"start": 1, "end": 9}})
        };
        ev(
            "message.part.updated",
            json!({"sessionID": ROOT, "part": {"id": part, "sessionID": ROOT,
            "messageID": message, "type": "tool", "tool": "bash", "callID": "call_1", "state": state}}),
        )
    }

    #[test]
    fn a_turn_open_when_opencode_exits_gets_no_end_and_no_result() {
        let mut a = adapter();
        let mut events = feed(
            &mut a,
            &[
                user(
                    "msg_01",
                    "build",
                    json!({"providerID": "mock", "modelID": "m1"}),
                ),
                assistant("msg_02", "msg_01", false),
                bash("msg_02", "prt_02", "running"),
            ],
        );
        events.extend(a.session_end());
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["tool_call"]);
        assert!(a.unknown.contains("msg_01"));
        assert!(
            a.running_in("msg_01"),
            "kept, so a later snapshot can give its real result"
        );
    }

    #[test]
    fn a_fact_waits_behind_an_earlier_text_still_streaming() {
        let mut a = adapter();
        let text = |end: bool| {
            let time = if end {
                json!({"start": 1, "end": 5})
            } else {
                json!({"start": 1})
            };
            ev(
                "message.part.updated",
                json!({"sessionID": ROOT, "part": {"id": "prt_02a",
                "sessionID": ROOT, "messageID": "msg_02", "type": "text", "text": "done", "time": time}}),
            )
        };
        let mut keys = Vec::new();
        for f in [
            user(
                "msg_01",
                "build",
                json!({"providerID": "mock", "modelID": "m1"}),
            ),
            assistant("msg_02", "msg_01", false),
            text(false),
            bash("msg_02", "prt_02b", "running"),
            bash("msg_02", "prt_02b", "completed"),
            text(true),
        ] {
            keys.push(
                a.ingest(&f)
                    .iter()
                    .map(|e| sid(e).rsplit(':').take(2).collect::<Vec<_>>().join("<"))
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(
            keys[3..],
            [
                vec![],
                vec![],
                // The terminal state repeats the call; the store keeps the first.
                vec![
                    "prt_02a<text".to_string(),
                    "prt_02b<pre".into(),
                    "prt_02b<pre".into(),
                    "prt_02b<post".into()
                ]
            ],
            "the tool waits for the text created before it"
        );
    }

    #[test]
    fn an_idle_that_closes_nothing_over_unplaced_messages_asks_for_a_snapshot() {
        let mut a = adapter();
        // A daemon that started mid-turn: the turn's prompt was never seen.
        feed(&mut a, &[assistant("msg_02", "msg_01", true), idle()]);
        assert!(a.take_resync_request());
        assert!(!a.take_resync_request(), "asked once");

        let mut quiet = adapter();
        feed(&mut quiet, &[idle()]);
        assert!(!quiet.take_resync_request());
    }

    // A second root beside ROOT, for snapshots holding two session trees.
    const OTHER: &str = "ses_0f00b000000bBBBBBBBBBBBBBB";

    fn message_items(session: &str, user: &str, reply: &str) -> Vec<Value> {
        vec![
            json!({"sessionID": session, "info": {"id": reply, "sessionID": session,
                "role": "assistant", "parentID": user, "time": {"created": 3, "completed": 4}},
                "parts": [{"id": format!("prt_{reply}"), "sessionID": session, "messageID": reply,
                    "type": "text", "text": "done", "time": {"start": 3, "end": 4}}]}),
            json!({"sessionID": session, "info": {"id": user, "sessionID": session, "role": "user",
                "time": {"created": 2}}, "parts": [{"id": format!("prt_{user}"), "sessionID": session,
                "messageID": user, "type": "text", "text": "go"}]}),
        ]
    }

    /// Two roots each mid-turn, with a card open on each; then an idle
    /// snapshot listing no request, whose `other` items are `other_items`.
    fn two_roots_then(other_items: Vec<Value>) -> (OpencodeAdapter, Vec<PendingEvent>) {
        let mut a = adapter();
        for (session, user, reply, card) in [
            (ROOT, "msg_a1", "msg_a2", "per_a"),
            (OTHER, "msg_b1", "msg_b2", "per_b"),
        ] {
            feed(
                &mut a,
                &[
                    ev(
                        "message.updated",
                        json!({"sessionID": session, "info": {"id": user,
                        "sessionID": session, "role": "user", "time": {"created": 2}}}),
                    ),
                    ev(
                        "message.updated",
                        json!({"sessionID": session, "info": {"id": reply,
                        "sessionID": session, "role": "assistant", "parentID": user,
                        "time": {"created": 3}}}),
                    ),
                    ev(
                        "permission.asked",
                        json!({"id": card, "sessionID": session,
                        "permission": "edit", "patterns": ["a.txt"]}),
                    ),
                ],
            );
        }
        a.sync_begin();
        let session = |id: &str| {
            json!({"session": {"id": id, "directory": "/Users/ada/project",
            "time": {"created": 1}}})
        };
        let mut items = vec![session(ROOT)];
        items.extend(message_items(ROOT, "msg_a1", "msg_a2"));
        items.extend(other_items);
        let begin = frame(
            json!({"t": "sync_begin", "sync": 1, "reason": "connect", "scope": "full",
            "as_of": 9, "status": {}, "status_ok": true}),
        );
        let count = items.len();
        let page = frame(json!({"t": "sync_page", "sync": 1, "n": 0, "items": items}));
        let end = frame(
            json!({"t": "sync_end", "sync": 1, "done_at": 9, "list_ok": true,
            "permissions_ok": true, "questions_ok": true, "requests_ok": true, "lower": {},
            "pages": 1, "items": count, "bytes": 0, "activation": 1}),
        );
        let mut assembly = SyncAssembly::begin(&begin).unwrap();
        assembly.add(&page);
        let snapshot = assembly.finish(&end).unwrap();
        let (mut facts, staged) = a.plan_resync(&snapshot).expect("the healthy tree is read");
        a.apply_resync(staged);
        facts.extend(a.settle());
        (a, facts)
    }

    fn assert_only_the_healthy_tree_was_read(a: &OpencodeAdapter, facts: &[PendingEvent]) {
        let keys: Vec<&str> = facts.iter().map(sid).collect();
        let at = |s: &str, k: &str| format!("{s}:{k}");
        assert!(keys.contains(&at(ROOT, "turn:msg_a1").as_str()), "{keys:?}");
        assert!(
            keys.contains(&at(ROOT, "perm_resolved:per_a").as_str()),
            "{keys:?}"
        );
        assert!(
            keys.iter().all(|k| !k.starts_with(OTHER)),
            "nothing is read of the incomplete tree: {keys:?}"
        );
        assert_eq!(
            a.current.get(OTHER).map(String::as_str),
            Some("msg_b1"),
            "its turn stays open"
        );
        let open: Vec<&str> = a.open_cards.iter().map(|c| c.request_id.as_str()).collect();
        assert_eq!(open, ["per_b"], "its card is not cleared");
    }

    #[test]
    fn a_session_over_the_page_cap_sets_aside_its_own_tree_and_no_other() {
        let mut items = vec![
            json!({"stub": true, "kind": "session", "ids": {"sessionID": OTHER},
            "size": 300_000, "sha256": "ab".repeat(32)}),
        ];
        // Its messages still follow it.
        items.extend(message_items(OTHER, "msg_b1", "msg_b2"));
        let (a, facts) = two_roots_then(items);
        assert_only_the_healthy_tree_was_read(&a, &facts);
    }

    #[test]
    fn a_message_over_the_page_cap_sets_aside_its_own_tree_and_no_other() {
        let mut items = vec![
            json!({"session": {"id": OTHER, "directory": "/Users/ada/project",
            "time": {"created": 1}}}),
        ];
        items.push(json!({"stub": true, "kind": "message",
            "ids": {"sessionID": OTHER, "messageID": "msg_b2"}, "size": 300_000,
            "sha256": "ab".repeat(32)}));
        items.push(message_items(OTHER, "msg_b1", "msg_b2").remove(1));
        let (a, facts) = two_roots_then(items);
        assert_only_the_healthy_tree_was_read(&a, &facts);
    }

    /// A snapshot of `items` read in one page, `status` its session status.
    fn full_snapshot(status: Value, items: Vec<Value>) -> Snapshot {
        let begin = frame(
            json!({"t": "sync_begin", "sync": 1, "reason": "connect", "scope": "full",
            "as_of": 9, "status": status, "status_ok": true}),
        );
        let count = items.len();
        let page = frame(json!({"t": "sync_page", "sync": 1, "n": 0, "items": items}));
        let end = frame(
            json!({"t": "sync_end", "sync": 1, "done_at": 9, "list_ok": true,
            "permissions_ok": true, "questions_ok": true, "requests_ok": true, "lower": {},
            "pages": 1, "items": count, "bytes": 0, "activation": 1}),
        );
        let mut assembly = SyncAssembly::begin(&begin).unwrap();
        assembly.add(&page);
        assembly.finish(&end).unwrap()
    }

    fn part_stub(part: &str, kind: &str, status: Option<&str>) -> LinkFrame {
        let mut stub = json!({"t": "stub", "seq": 9, "type": "message.part.updated",
            "ids": {"sessionID": ROOT, "messageID": "msg_02", "partID": part, "callID": "call_1"},
            "part_type": kind, "size": 2_000_000, "sha256": "ab".repeat(32)});
        if let Some(status) = status {
            stub["status"] = json!(status);
        }
        frame(stub)
    }

    fn too_large(what: &str) -> String {
        format!(
            "This {what} is too large to show (2000000 bytes, sha256 {})",
            "ab".repeat(32)
        )
    }

    #[test]
    fn a_tool_update_over_the_frame_cap_ends_the_tool_only_when_it_ended() {
        let mut a = adapter();
        let model = json!({"providerID": "mock", "modelID": "m1"});
        let mut events = feed(
            &mut a,
            &[
                user("msg_01", "build", model),
                assistant("msg_02", "msg_01", false),
                bash("msg_02", "prt_02", "running"),
                part_stub("prt_02", "tool", Some("running")),
            ],
        );
        assert!(a.running_in("msg_01"), "still running");
        events.extend(feed(
            &mut a,
            &[
                part_stub("prt_02", "tool", Some("completed")),
                assistant("msg_02", "msg_01", true),
                idle(),
            ],
        ));
        assert!(!a.running_in("msg_01"));
        let results: Vec<&PendingEvent> = events
            .iter()
            .filter(|e| e.kind == EventKind::ToolResult)
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(sid(results[0]), format!("{ROOT}:post:prt_02"));
        let payload = &results[0].payload;
        assert_eq!(payload["tool_name"], "Bash");
        assert_eq!(payload["tool_input"], json!({"command": "sleep 8"}));
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["aggregated_output"], too_large("output"));
        let ends: Vec<&Value> = events
            .iter()
            .filter(|e| e.kind == EventKind::TurnComplete)
            .map(|e| &e.payload["status"])
            .collect();
        assert_eq!(ends, [&json!("completed")]);
    }

    #[test]
    fn a_text_over_the_frame_cap_is_said_by_its_size_and_lets_what_waits_behind_it_go() {
        let mut a = adapter();
        let start = ev(
            "message.part.updated",
            json!({"sessionID": ROOT, "part": {"id": "prt_02a", "sessionID": ROOT,
            "messageID": "msg_02", "type": "text", "text": "", "time": {"start": 1}}}),
        );
        let model = json!({"providerID": "mock", "modelID": "m1"});
        feed(
            &mut a,
            &[
                user("msg_01", "build", model),
                assistant("msg_02", "msg_01", false),
                start,
            ],
        );
        let held = feed(
            &mut a,
            &[
                bash("msg_02", "prt_02b", "running"),
                bash("msg_02", "prt_02b", "completed"),
            ],
        );
        assert!(held.is_empty(), "behind the text still streaming");
        let events = feed(&mut a, &[part_stub("prt_02a", "text", None)]);
        let keys: Vec<&str> = events.iter().map(sid).collect();
        let at = |s: &str| format!("{ROOT}:{s}");
        assert_eq!(
            keys,
            [
                at("text:prt_02a"),
                at("pre:prt_02b"),
                at("pre:prt_02b"),
                at("post:prt_02b")
            ]
        );
        assert_eq!(events[0].kind, EventKind::AgentMessage);
        assert_eq!(events[0].payload["text"], too_large("text"));
    }

    #[test]
    fn a_question_or_permission_too_large_for_a_card_is_shown_as_too_large() {
        let mut a = adapter();
        let question = json!({"id": "que_1", "sessionID": ROOT, "questions": [
            {"question": "x".repeat(900_000), "header": "h", "options": []}]});
        let patterns: Vec<String> = (0..120)
            .map(|i| format!("{i}{}", "p".repeat(8000)))
            .collect();
        let many = json!({"id": "per_1", "sessionID": ROOT, "permission": "edit",
            "patterns": patterns, "metadata": {}});
        let odd = json!({"id": "per_2", "sessionID": ROOT, "permission": "edit",
            "patterns": [{"glob": "q".repeat(20_000)}], "metadata": {}});
        let events = feed(
            &mut a,
            &[
                ev("question.asked", question.clone()),
                ev("permission.asked", many.clone()),
                ev("permission.asked", odd.clone()),
            ],
        );
        let asked = [(question, "question"), (many, "edit"), (odd, "edit")];
        assert_eq!(events.len(), asked.len());
        for (event, (request, name)) in events.iter().zip(asked) {
            let card = card_of(event);
            let encoded = serde_json::to_vec(&request).unwrap();
            let message = format!(
                "This request is too large to show ({} bytes, sha256 {}); answer it at the Mac",
                encoded.len(),
                protocol::hash::sha256_hex(&encoded)
            );
            assert_eq!(card.tool_name, name);
            assert_eq!(card.tool_input, json!({"message": message}));
            assert!(card.display_text.len() <= MAX_CARD_BYTES);
            assert_eq!(
                protocol::hash::sha256_hex(card.display_text.as_bytes()),
                card.payload_hash
            );
        }
    }

    #[test]
    fn a_child_session_over_the_page_cap_sets_aside_its_root_tree() {
        let mut items = vec![
            json!({"session": {"id": OTHER, "directory": "/Users/ada/project",
            "time": {"created": 1}}}),
        ];
        items.extend(message_items(OTHER, "msg_b1", "msg_b2"));
        // The child's info is gone with the rest of it; its parent is kept.
        items.push(json!({"stub": true, "kind": "session",
            "ids": {"sessionID": "ses_0f00c000000cCCCCCCCCCCCCCC", "parentID": OTHER},
            "size": 300_000, "sha256": "ab".repeat(32)}));
        let (a, facts) = two_roots_then(items);
        assert_only_the_healthy_tree_was_read(&a, &facts);
    }

    fn snapshot_message(id: &str, role: &str, parent: Option<&str>, time: Value) -> Value {
        let mut info = json!({"id": id, "sessionID": ROOT, "role": role, "time": time});
        if let Some(parent) = parent {
            info["parentID"] = json!(parent);
        }
        let part = if role == "user" {
            json!({"id": format!("prt_{id}"), "sessionID": ROOT, "messageID": id,
                "type": "text", "text": "go"})
        } else {
            json!({"id": format!("prt_{id}"), "sessionID": ROOT, "messageID": id,
                "type": "text", "text": "done", "time": {"start": 1, "end": 2}})
        };
        json!({"sessionID": ROOT, "info": info, "parts": [part]})
    }

    #[test]
    fn a_reply_that_never_completed_leaves_its_turn_unknown_and_holds_no_later_prompt_back() {
        let items = vec![
            json!({"session": {"id": ROOT, "time": {"created": 0}}}),
            snapshot_message("msg_01", "user", None, json!({"created": 1})),
            // Abandoned: the process ended while it streamed.
            snapshot_message("msg_02", "assistant", Some("msg_01"), json!({"created": 2})),
            snapshot_message("msg_03", "user", None, json!({"created": 5})),
            snapshot_message(
                "msg_04",
                "assistant",
                Some("msg_03"),
                json!({"created": 6, "completed": 7}),
            ),
        ];
        let a = adapter();
        let (facts, staged) = a.plan_resync(&full_snapshot(json!({}), items)).unwrap();
        let ends: Vec<&str> = facts
            .iter()
            .filter(|e| e.kind == EventKind::TurnComplete)
            .map(sid)
            .collect();
        assert_eq!(ends, [format!("{ROOT}:turn:msg_03")]);
        assert!(staged.unknown.contains("msg_01"));
        let prompt = facts
            .iter()
            .find(|e| sid(e) == format!("{ROOT}:user:prt_msg_03"))
            .unwrap();
        assert_eq!(prompt.turn_id.as_deref(), Some("msg_03"));
    }

    #[test]
    fn a_finished_turn_whose_tool_over_the_page_cap_still_runs_is_not_closed_by_the_snapshot() {
        let mut reply = snapshot_message(
            "msg_02",
            "assistant",
            Some("msg_01"),
            json!({"created": 2, "completed": 3}),
        );
        reply["parts"] = json!([]);
        let items = vec![
            json!({"session": {"id": ROOT, "time": {"created": 0}}}),
            reply,
            json!({"stub": true, "kind": "part", "ids": {"sessionID": ROOT, "messageID": "msg_02",
                "partID": "prt_02", "callID": "call_1"}, "part_type": "tool", "status": "running",
                "size": 300_000, "sha256": "ab".repeat(32)}),
            snapshot_message("msg_01", "user", None, json!({"created": 1})),
        ];
        // The reply is split, so its part stub joins it.
        let mut items = items;
        items[1]["split"] = json!(true);
        let mut a = adapter();
        let (facts, staged) = a.plan_resync(&full_snapshot(json!({}), items)).unwrap();
        assert!(
            facts.iter().all(|e| e.kind != EventKind::TurnComplete),
            "{:?}",
            facts.iter().map(sid).collect::<Vec<_>>()
        );
        a.apply_resync(staged);
        a.session_end();
        assert!(a.unknown.contains("msg_01"));
    }

    // ------------------------------------------------- turns from the loop

    /// A root session as OpenCode 1.18.34 writes it, row by row, in the
    /// captures' bus shape: ids ascending, the clock one millisecond on per
    /// write. Paths are under `packages/opencode/src`.
    struct Script {
        rows: Vec<Value>,
        n: u64,
        at: u64,
    }

    impl Script {
        fn new() -> Self {
            let mut s = Script {
                rows: Vec::new(),
                n: 0,
                at: 1_700_000_000_000,
            };
            s.push(
                "session.created",
                json!({"info": {"id": ROOT, "time": {"created": s.at}}}),
            );
            s
        }

        fn push(&mut self, event: &str, properties: Value) {
            self.rows
                .push(json!({"type": event, "properties": properties}));
        }

        fn id(&mut self, prefix: &str) -> String {
            self.n += 1;
            format!("{prefix}_0f00{:08x}AAAAAAAAAAAAAA", self.n)
        }

        fn tick(&mut self) -> u64 {
            self.at += 1;
            self.at
        }

        /// The next write lands in the same millisecond as the last one.
        fn same_ms(&mut self) {
            self.at -= 1;
        }

        fn busy(&mut self) {
            self.push(
                "session.status",
                json!({"sessionID": ROOT, "status": {"type": "busy"}}),
            );
        }

        fn idle(&mut self) {
            self.push(
                "session.status",
                json!({"sessionID": ROOT, "status": {"type": "idle"}}),
            );
            self.push("session.idle", json!({"sessionID": ROOT}));
        }

        fn error(&mut self, name: &str, message: &str) {
            self.push(
                "session.error",
                json!({"sessionID": ROOT, "error": {"name": name, "data": {"message": message}}}),
            );
        }

        fn message(&mut self, info: &Value) {
            self.push("message.updated", json!({"sessionID": ROOT, "info": info}));
        }

        fn part(&mut self, message: &str, mut part: Value) -> Value {
            let id = self.id("prt");
            part["id"] = json!(id);
            part["sessionID"] = json!(ROOT);
            part["messageID"] = json!(message);
            self.push(
                "message.part.updated",
                json!({"sessionID": ROOT, "part": part}),
            );
            part
        }

        /// A user message with its parts (`prompt.ts` createUserMessage).
        fn user(&mut self, parts: &[Value]) -> String {
            let id = self.id("msg");
            let info = json!({"id": id, "sessionID": ROOT, "role": "user",
                "time": {"created": self.tick()}, "agent": "build",
                "model": {"providerID": "mock", "modelID": "m"}});
            self.message(&info);
            for part in parts {
                self.part(&id, part.clone());
            }
            id
        }

        fn prompt(&mut self, text: &str) -> String {
            self.user(&[json!({"type": "text", "text": text})])
        }

        fn assistant(&mut self, parent: &str, extra: Value) -> (String, Value) {
            let id = self.id("msg");
            let mut info = json!({"id": id, "sessionID": ROOT, "role": "assistant",
                "parentID": parent, "mode": "build", "agent": "build", "modelID": "m",
                "providerID": "mock", "time": {"created": self.tick()}});
            for (k, v) in extra.as_object().into_iter().flatten() {
                info[k] = v.clone();
            }
            self.message(&info);
            (id, info)
        }

        fn complete(&mut self, info: &mut Value, finish: Option<&str>) {
            info["time"]["completed"] = json!(self.tick());
            if let Some(finish) = finish {
                info["finish"] = json!(finish);
            }
            self.message(info);
        }

        /// One step of the loop (`processor.ts`): step-start, the model's
        /// output, step-finish with its reason, the message completed.
        fn step(&mut self, parent: &str, extra: Value, finish: &str, tool: bool) -> String {
            let (id, mut info) = self.assistant(parent, extra);
            self.part(&id, json!({"type": "step-start"}));
            if tool {
                let at = self.tick();
                let running = self.part(&id, json!({"type": "tool", "tool": "read",
                    "callID": format!("call_{}", self.n),
                    "state": {"status": "running", "input": {"filePath": "a"}, "time": {"start": at}}}));
                let mut done = running.clone();
                done["state"] = json!({"status": "completed", "input": {"filePath": "a"},
                    "output": "x", "title": "a", "metadata": {}, "time": {"start": at, "end": self.tick()}});
                self.push(
                    "message.part.updated",
                    json!({"sessionID": ROOT, "part": done}),
                );
            } else {
                let at = self.tick();
                self.part(
                    &id,
                    json!({"type": "text", "text": "done",
                    "time": {"start": at, "end": at}}),
                );
            }
            self.part(&id, json!({"type": "step-finish", "reason": finish, "cost": 0,
                "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}}}));
            self.complete(&mut info, Some(finish));
            id
        }

        /// A prompt on an idle root answered in one step.
        fn turn(&mut self, text: &str) {
            let p = self.prompt(text);
            self.busy();
            self.step(&p, json!({}), "stop", false);
            self.idle();
        }
    }

    /// The longest lost window the captures are cut with.
    const LONGEST: usize = 89;

    /// Every lost window of `rows` that `keep` takes, by its length and the
    /// row it ends before (all of them when `keep` is `None`), checked against
    /// the uninterrupted run. Returns how many were checked.
    fn windows_of(
        name: &str,
        rows: &[Value],
        keep: Option<&dyn Fn(usize, usize) -> bool>,
    ) -> usize {
        let uncut = {
            let mut run = Run::new();
            for row in rows {
                run.row(row);
            }
            run
        };
        let want: HashMap<String, (Option<String>, Value)> = uncut
            .store
            .events
            .iter()
            .map(|e| (sid(e).to_string(), (e.turn_id.clone(), e.payload.clone())))
            .collect();
        let turns: HashSet<&str> = uncut
            .store
            .events
            .iter()
            .filter_map(|e| e.turn_id.as_deref())
            .collect();
        let longest = if keep.is_some() { LONGEST } else { rows.len() };
        let daemon = |row: &Value| {
            let t = row["type"].as_str().unwrap_or_default();
            t.starts_with("ccd.") || t.starts_with("collector.")
        };
        // The adapter as it stood after each of the last `longest` rows.
        let mut before: std::collections::VecDeque<Run> = std::collections::VecDeque::new();
        let mut server = Server::new();
        let mut live = Run::new();
        let mut windows = 0;
        for j in 0..=rows.len() {
            before.push_back(live.clone());
            if before.len() > longest + 1 {
                before.pop_front();
            }
            if j > 0 {
                server.row(&rows[j - 1]);
            }
            if j < rows.len() {
                live.row(&rows[j]);
            }
            if !server.alive || server.syncing {
                continue;
            }
            let snapshot = server.snapshot();
            for k in (0..j).rev() {
                let len = j - k;
                if len > longest || daemon(&rows[k]) {
                    break;
                }
                if keep.is_some_and(|keep| !keep(len, j)) {
                    continue;
                }
                windows += 1;
                let label = format!("{name}: rows {k}..{j} lost");
                let run = cut_at(&before[before.len() - 1 - len], rows, j, &snapshot);
                run.assert_one_end_per_turn(&label);
                let unknown = run.unknown();
                for e in &run.store.events {
                    let turn = e.turn_id.as_deref();
                    assert!(
                        turn.is_none_or(|t| turns.contains(t)),
                        "{label}: a turn the run never had: {}",
                        sid(e)
                    );
                    let Some((was, payload)) = want.get(sid(e)) else {
                        assert!(e.kind != EventKind::TurnComplete, "{label}: {}", sid(e));
                        continue;
                    };
                    if e.kind == EventKind::TurnComplete {
                        assert_eq!(payload, &e.payload, "{label}: {}", sid(e));
                    }
                    assert!(
                        was.as_deref() == turn || turn.is_some_and(|t| unknown.contains(t)),
                        "{label}: {} moved from {was:?} to {turn:?}",
                        sid(e)
                    );
                }
                let ended: HashSet<&str> = run
                    .store
                    .events
                    .iter()
                    .filter(|e| e.kind == EventKind::TurnComplete)
                    .filter_map(|e| e.turn_id.as_deref())
                    .collect();
                let held: HashSet<&str> = run
                    .store
                    .events
                    .iter()
                    .filter_map(|e| e.turn_id.as_deref())
                    .collect();
                for e in &uncut.store.events {
                    if e.kind == EventKind::TurnComplete {
                        // A turn whose facts all joined an outcome-unknown
                        // turn is outcome-unknown with it.
                        let t = e.turn_id.as_deref().unwrap();
                        assert!(
                            ended.contains(t) || unknown.contains(t) || !held.contains(t),
                            "{label}: {t} neither ended as it did nor outcome-unknown"
                        );
                    }
                }
            }
        }
        windows
    }

    /// The link lost for a window of rows: OpenCode wrote every row, the
    /// adapter read the rows before the window, then the snapshot taken after
    /// it, then the rest. What only the lost frames said may be lost with
    /// them, but every TurnComplete is one the uninterrupted run made, no turn
    /// is invented or split, and every turn of the uninterrupted run ends as
    /// it did there or is outcome-unknown.
    #[test]
    fn a_snapshot_after_lost_frames_invents_and_splits_no_turn() {
        let mut captures: Vec<(&str, &str)> = FORWARDED.iter().map(|(n, f, _)| (*n, *f)).collect();
        captures.extend([
            ("linkcut", LINKCUT),
            ("linkcut-overlap", LINKCUT_OVERLAP),
            ("linkcut-uninterrupted", LINKCUT_UNCUT),
            ("pending", PENDING),
        ]);
        // Windows of one and two rows at every row; longer ones at every
        // fifth, staggered by length.
        let keep = |len: usize, j: usize| {
            len <= 2 || ([3, 5, 8, 13, 21, 34, 55, LONGEST].contains(&len) && j % 5 == len % 5)
        };
        let windows: usize = captures
            .iter()
            .map(|(name, text)| windows_of(name, &rows(text), Some(&keep)))
            .sum();
        assert_eq!(windows, 3963, "lost windows checked");
    }

    /// The script read live from its first row, and again with nothing read
    /// live and one snapshot after its last row; then every lost window.
    /// Returns how the snapshot-only run's facts differ from the live run's.
    fn snapshot_only(name: &str, script: &Script) -> Vec<String> {
        let rows = &script.rows;
        let uncut = {
            let mut run = Run::new();
            for row in rows {
                run.row(row);
            }
            run
        };
        uncut.assert_one_end_per_turn(name);
        windows_of(name, rows, None);
        let mut server = Server::new();
        for row in rows {
            server.row(row);
        }
        let run = cut_at(&Run::new(), rows, rows.len(), &server.snapshot());
        diff(&mine(&run.store), &mine(&uncut.store))
            .into_iter()
            .filter(|d| !d.ends_with(":session_start"))
            .collect()
    }

    /// An overflow mid-step makes OpenCode compact and write the overflowing
    /// prompt again after the summary, inside the same busy period
    /// (`prompt.ts` 1320-1327, `compaction.ts` 468-496): one turn.
    #[test]
    fn a_prompt_replayed_after_an_overflow_compaction_stays_in_its_turn() {
        let mut s = Script::new();
        s.turn("first");
        let p = s.prompt("long");
        s.busy();
        // `processor.ts` 621-631: no error and no finish on the message.
        let (a, mut info) = s.assistant(&p, json!({}));
        s.part(&a, json!({"type": "step-start"}));
        s.error("ContextOverflowError", "prompt is too long");
        s.complete(&mut info, None);
        let c = s.user(&[json!({"type": "compaction", "auto": true, "overflow": true})]);
        s.step(
            &c,
            json!({"mode": "compaction", "agent": "compaction", "summary": true}),
            "stop",
            false,
        );
        let replay = s.prompt("long");
        s.step(&replay, json!({}), "stop", false);
        s.idle();
        assert_eq!(snapshot_only("overflow", &s), Vec::<String>::new());
    }

    /// A prompt that lands between two steps of one run joins it: the loop
    /// answers it in its next step (`prompt.ts` 1088-1131).
    #[test]
    fn a_prompt_between_two_steps_joins_the_running_turn() {
        let mut s = Script::new();
        s.turn("first");
        let p = s.prompt("go");
        s.busy();
        s.step(&p, json!({}), "tool-calls", true);
        let joined = s.prompt("and this");
        s.step(&joined, json!({}), "stop", false);
        s.idle();
        assert_eq!(snapshot_only("between steps", &s), Vec::<String>::new());
    }

    /// The same, written in the millisecond the step completed.
    #[test]
    fn a_prompt_in_the_millisecond_a_step_completed_joins_the_running_turn() {
        let mut s = Script::new();
        s.turn("first");
        let p = s.prompt("go");
        s.busy();
        s.step(&p, json!({}), "tool-calls", true);
        s.same_ms();
        let joined = s.prompt("and this");
        s.step(&joined, json!({}), "stop", false);
        s.idle();
        assert_eq!(
            snapshot_only("same ms, tool-calls", &s),
            Vec::<String>::new()
        );
    }

    /// A prompt in the millisecond a final step completed: whether the run had
    /// ended (`prompt.ts` 1105-1131) cannot be told from what is persisted, so
    /// the prompt joins and the turn is outcome-unknown.
    #[test]
    fn a_prompt_in_the_millisecond_a_run_may_have_ended_leaves_its_turn_unknown() {
        let mut s = Script::new();
        s.turn("first");
        let p = s.prompt("go");
        s.busy();
        s.step(&p, json!({}), "stop", false);
        s.same_ms();
        let joined = s.prompt("and this");
        s.step(&joined, json!({}), "stop", false);
        s.idle();
        assert_eq!(
            snapshot_only("same ms, stop", &s),
            [
                format!("missing {ROOT}:usage:{p}"),
                format!("missing {ROOT}:turn:{p}")
            ]
        );
    }

    /// A `!command` runs on an idle root only, and is a busy period of its
    /// own: a synthetic prompt, then an assistant message with no finish and
    /// one shell tool (`prompt.ts` 451-590, `effect/runner.ts` startShell).
    #[test]
    fn a_shell_command_is_a_turn_of_its_own() {
        let mut s = Script::new();
        s.turn("first");
        s.busy();
        let u = s.user(&[json!({"type": "text", "synthetic": true,
            "text": "The following tool was executed by the user"})]);
        let (a, mut info) = s.assistant(&u, json!({}));
        let at = s.tick();
        let running = s.part(
            &a,
            json!({"type": "tool", "tool": "bash", "callID": "call_sh",
            "state": {"status": "running", "time": {"start": at}, "input": {"command": "ls"}}}),
        );
        s.complete(&mut info, None);
        let mut done = running;
        done["state"] = json!({"status": "completed", "time": {"start": at, "end": s.at},
            "input": {"command": "ls"}, "title": "", "metadata": {"output": "a\n"}, "output": "a\n"});
        s.push(
            "message.part.updated",
            json!({"sessionID": ROOT, "part": done}),
        );
        s.idle();
        s.turn("next");
        assert_eq!(snapshot_only("shell", &s), Vec::<String>::new());
    }

    /// `/compact` on an idle root: a prompt holding only a compaction part,
    /// then the summary, then idle (`handlers/session.ts` 273-292,
    /// `compaction.ts` 390-420).
    #[test]
    fn a_manual_compaction_is_a_turn_of_its_own() {
        let mut s = Script::new();
        s.turn("first");
        let c = s.user(&[json!({"type": "compaction", "auto": false})]);
        s.busy();
        s.step(
            &c,
            json!({"mode": "compaction", "agent": "compaction", "summary": true}),
            "stop",
            false,
        );
        s.idle();
        s.turn("next");
        assert_eq!(snapshot_only("compact", &s), Vec::<String>::new());
    }

    /// A background task's result is prompted into its idle parent as
    /// synthetic text, which starts a busy period (`tool/task.ts` 227-252).
    #[test]
    fn a_task_result_prompted_into_an_idle_root_is_a_turn_of_its_own() {
        let mut s = Script::new();
        s.turn("first");
        let u = s.user(&[json!({"type": "text", "synthetic": true,
            "text": "<task_result>Background task completed: scan</task_result>"})]);
        s.busy();
        s.step(&u, json!({}), "stop", false);
        s.idle();
        s.turn("next");
        assert_eq!(snapshot_only("task result", &s), Vec::<String>::new());
    }

    /// A prompt whose run failed before it wrote a reply (`prompt.ts`
    /// 594-612): the next prompt is a turn of its own, and how the failed one
    /// ended is not persisted, so a snapshot leaves it outcome-unknown.
    #[test]
    fn a_prompt_that_got_no_reply_ends_unknown_and_holds_no_later_prompt() {
        let mut s = Script::new();
        s.turn("first");
        let p = s.prompt("go");
        s.busy();
        s.error("UnknownError", "Model not found: mock/gone.");
        s.idle();
        s.turn("next");
        assert_eq!(
            snapshot_only("no reply", &s),
            [
                format!("missing {ROOT}:error:{p}"),
                format!("missing {ROOT}:turn:{p}")
            ]
        );
    }

    #[test]
    fn a_usage_sum_past_the_integer_range_is_held_at_its_ceiling() {
        let mut sum = Sum::default();
        sum.add(Some(&json!(u64::MAX)));
        sum.add(Some(&json!(i64::MAX)));
        assert_eq!(sum.value(), json!(i64::MAX));
    }
    // --------------------------------------------------------- answerable cards

    /// One captured permission ask per family (`asked`, its anchoring tool
    /// part and the context the plugin read), and the card this builder makes
    /// of it. The `expected` block is this builder's own output, written by
    /// [`regenerate_the_permission_card_fixtures`]: the byte comparison in
    /// [`every_captured_permission_ask_builds_the_committed_card`] guards
    /// against drift, and [`every_card_rule_holds_on_the_captured_asks`] asserts
    /// the facts each rule stands on.
    const S7: [(&str, &str); 24] = [
        (
            "apply-patch-move-over",
            include_str!(
                "../../../fixtures/opencode/s7-permission-apply-patch-move-over-1.18.34.json"
            ),
        ),
        (
            "bash",
            include_str!("../../../fixtures/opencode/s7-permission-bash-1.18.34.json"),
        ),
        (
            "bash-child",
            include_str!("../../../fixtures/opencode/s7-permission-bash-child-1.18.34.json"),
        ),
        (
            "custom-tool",
            include_str!("../../../fixtures/opencode/s7-permission-custom-tool-1.18.34.json"),
        ),
        (
            "doom-loop",
            include_str!("../../../fixtures/opencode/s7-permission-doom-loop-1.18.34.json"),
        ),
        (
            "edit",
            include_str!("../../../fixtures/opencode/s7-permission-edit-1.18.34.json"),
        ),
        (
            "external-directory",
            include_str!(
                "../../../fixtures/opencode/s7-permission-external-directory-1.18.34.json"
            ),
        ),
        (
            "external-directory-root",
            include_str!(
                "../../../fixtures/opencode/s7-permission-external-directory-root-1.18.34.json"
            ),
        ),
        (
            "glob",
            include_str!("../../../fixtures/opencode/s7-permission-glob-1.18.34.json"),
        ),
        (
            "grep",
            include_str!("../../../fixtures/opencode/s7-permission-grep-1.18.34.json"),
        ),
        (
            "lsp",
            include_str!("../../../fixtures/opencode/s7-permission-lsp-1.18.34.json"),
        ),
        (
            "mcp-code-mode",
            include_str!("../../../fixtures/opencode/s7-permission-mcp-code-mode-1.18.34.json"),
        ),
        (
            "mcp-tool",
            include_str!("../../../fixtures/opencode/s7-permission-mcp-tool-1.18.34.json"),
        ),
        (
            "read",
            include_str!("../../../fixtures/opencode/s7-permission-read-1.18.34.json"),
        ),
        (
            "read-mcp-resource",
            include_str!("../../../fixtures/opencode/s7-permission-read-mcp-resource-1.18.34.json"),
        ),
        (
            "shadowed-apply-patch",
            include_str!(
                "../../../fixtures/opencode/s7-permission-shadowed-apply-patch-1.18.34.json"
            ),
        ),
        (
            "shadowed-bash",
            include_str!("../../../fixtures/opencode/s7-permission-shadowed-bash-1.18.34.json"),
        ),
        (
            "skill",
            include_str!("../../../fixtures/opencode/s7-permission-skill-1.18.34.json"),
        ),
        (
            "task",
            include_str!("../../../fixtures/opencode/s7-permission-task-1.18.34.json"),
        ),
        (
            "todowrite",
            include_str!("../../../fixtures/opencode/s7-permission-todowrite-1.18.34.json"),
        ),
        (
            "v2-bash",
            include_str!("../../../fixtures/opencode/s7-permission-v2-bash-1.18.34.json"),
        ),
        (
            "webfetch",
            include_str!("../../../fixtures/opencode/s7-permission-webfetch-1.18.34.json"),
        ),
        (
            "websearch",
            include_str!("../../../fixtures/opencode/s7-permission-websearch-1.18.34.json"),
        ),
        (
            "s5-auto-mode",
            include_str!("../../../fixtures/opencode/s5-auto-mode-1.18.34.json"),
        ),
    ];

    const S5: &str = include_str!("../../../fixtures/opencode/s5-auto-mode-1.18.34.json");
    const S6: &str = include_str!("../../../fixtures/opencode/s6-liveness-1.18.34.json");
    const CONFIG_DENY: &str =
        include_str!("../../../fixtures/opencode/permission-config-deny-1.18.34.json");

    /// The activation the fixtures' cards are stamped with.
    const ACTIVATION: u64 = 7;

    fn s7() -> Vec<(&'static str, Value)> {
        S7.iter()
            .filter(|(stem, _)| *stem != "s5-auto-mode")
            .map(|(stem, text)| (*stem, serde_json::from_str(text).expect("a JSON fixture")))
            .collect()
    }

    fn env_of(doc: &Value) -> FakeEnv {
        let mut env = FakeEnv::ada();
        for file in strings(doc["context"].get("existing_files")) {
            env = env.with_file(&file);
        }
        env
    }

    /// The context frame the plugin writes on a card, from what the fixture
    /// captured of the TUI at the ask.
    fn ctx_of(doc: &Value) -> Value {
        json!({
            "ok": true,
            "tool_ids": doc["context"]["tool_ids"],
            "mcp": doc["context"]["mcp_connected"],
            "mode": "normal",
            "parent": doc["anchor"]["parentID"],
            "ms": 0,
        })
    }

    /// The anchoring tool part as the plugin forwards it running, then the
    /// ask with its context. A doom loop names no part: its anchor is the
    /// repeated call, running.
    fn frames_of(doc: &Value) -> Vec<LinkFrame> {
        let asked = &doc["asked"];
        let session = asked["sessionID"].as_str().unwrap();
        let mut out = Vec::new();
        if let Some(parent) = doc["anchor"]["parentID"].as_str() {
            out.push(ev(
                "session.created",
                json!({"info": {"id": session, "parentID": parent}}),
            ));
        }
        let (tool, input, message, call) = match doc["anchor"]["partTool"].as_str() {
            Some(tool) => (
                tool.to_string(),
                doc["anchor"]["partInput"].clone(),
                asked["tool"]["messageID"].clone(),
                asked["tool"]["callID"].clone(),
            ),
            None if asked["permission"] == "doom_loop" => (
                asked["metadata"]["tool"].as_str().unwrap().to_string(),
                asked["metadata"]["input"].clone(),
                json!("msg_doom"),
                json!("call_doom"),
            ),
            None => {
                let mut request = asked.clone();
                request["ctx"] = ctx_of(doc);
                out.push(ev(doc["event"].as_str().unwrap(), request));
                return out;
            }
        };
        out.push(ev(
            "message.part.updated",
            json!({"part": {"id": "prt_anchor", "sessionID": session, "messageID": message,
                "type": "tool", "tool": tool, "callID": call,
                "state": {"status": "running", "input": input}}}),
        ));
        let mut request = asked.clone();
        if !input.is_null() {
            request["anchor_input"] = input;
        }
        request["ctx"] = ctx_of(doc);
        out.push(ev(doc["event"].as_str().unwrap(), request));
        out
    }

    /// What the builder makes of one fixture, and what the adapter filed.
    fn built(doc: &Value) -> (Option<Built>, Vec<PendingEvent>, OpencodeAdapter) {
        let mut a = OpencodeAdapter::new(key(), Arc::new(env_of(doc)));
        a.set_activation(ACTIVATION);
        let frames = frames_of(doc);
        let (ask, setup) = frames.split_last().unwrap();
        feed(&mut a, setup);
        let built = match ask {
            LinkFrame::Ev { properties, .. } if doc["event"] == "permission.asked" => Some(
                a.build_ask(properties, properties["ctx"].as_object().unwrap())
                    .0,
            ),
            _ => None,
        };
        let events = feed(&mut a, std::slice::from_ref(ask));
        (built, events, a)
    }

    fn option_ids(card: &ApprovalCard) -> Vec<&str> {
        card.tool_input["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["id"].as_str().unwrap())
            .collect()
    }

    fn always_scope(card: &ApprovalCard) -> Option<String> {
        card.tool_input["options"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == "always")
            .map(|o| o["scope"].as_str().unwrap().to_string())
    }

    fn risk_class(card: &ApprovalCard) -> String {
        serde_json::to_value(card.risk.as_ref().unwrap()).unwrap()["class"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// **Every rule, on the ask it was measured on.** Each fixture's facts are
    /// asserted from the capture, not from the committed card: what is shown
    /// and what is hidden, which options are offered, the Always text, the
    /// risk class the real classifier gives the captured input, and the lines
    /// a card carries or must not.
    #[test]
    fn every_card_rule_holds_on_the_captured_asks() {
        for (stem, doc) in s7() {
            let (built, events, _) = built(&doc);
            let input = |card: &ApprovalCard| card.tool_input.clone();
            match (stem, built) {
                ("v2-bash", None) => {
                    assert_eq!(events.len(), 1, "{stem}: one line, no card");
                    assert_eq!(events[0].kind, EventKind::Notification);
                    assert_eq!(
                        sid(&events[0]),
                        format!(
                            "{}:perm_hidden:{}",
                            doc["asked"]["sessionID"].as_str().unwrap(),
                            doc["asked"]["id"].as_str().unwrap()
                        )
                    );
                    assert_eq!(events[0].payload["message"], HIDDEN_REQUEST);
                }
                (
                    "custom-tool" | "shadowed-bash" | "shadowed-apply-patch" | "mcp-code-mode",
                    Some(Built::Hidden(why)),
                ) => {
                    assert!(
                        events.is_empty(),
                        "{stem}: nothing filed for a hidden request"
                    );
                    let expected = match stem {
                        "custom-tool" => "not a permission this build has a card for",
                        "mcp-code-mode" => "code mode",
                        _ => "is shadowed",
                    };
                    assert!(why.contains(expected), "{stem}: {why}");
                }
                (_, Some(Built::Card { family, card })) => {
                    assert_eq!(events.len(), 1, "{stem}");
                    assert_eq!(events[0].payload["answerable"], true, "{stem}");
                    assert_eq!(events[0].payload["family"], family, "{stem}");
                    assert_eq!(
                        card.generation, ACTIVATION,
                        "{stem}: generation is the activation"
                    );
                    assert!(!card.identity_bound);
                    assert_eq!(
                        protocol::hash::sha256_hex(card.display_text.as_bytes()),
                        card.payload_hash
                    );
                    let asked = &doc["asked"];
                    assert_eq!(
                        card.tool_input["always"], asked["always"],
                        "{stem}: always verbatim"
                    );
                    assert_eq!(
                        card.tool_input["computed_at"], "2023-11-14T22:13:20Z",
                        "{stem}"
                    );
                    let child = doc["anchor"]["parentID"].is_string();
                    let ids = option_ids(&card);
                    assert_eq!(
                        ids.contains(&"reject_with_words"),
                        child,
                        "{stem}: words only in a subagent session"
                    );
                    assert_eq!(ids[0], "once");
                    assert!(ids.contains(&"reject"));
                    if let Some(scope) = always_scope(&card) {
                        assert!(scope.contains(SCOPE), "{stem}: {scope}");
                    }
                    let risk = risk_class(&card);
                    match stem {
                        "bash" | "bash-child" => {
                            assert_eq!(card.tool_name, "command");
                            assert_eq!(card.tool_input["command"], asked["metadata"]["command"]);
                            assert_eq!(
                                card.tool_input["description"],
                                doc["anchor"]["partInput"]["description"]
                            );
                            assert_eq!(family, "bash");
                            let scope = always_scope(&card).unwrap();
                            assert!(scope.starts_with(&format!(
                                "Allow commands starting {}, ",
                                asked["always"][0].as_str().unwrap()
                            )));
                            assert!(scope
                                .contains("Matching ignores redirects, $VARS and later arguments"));
                            assert_eq!(risk, "medium");
                        }
                        "external-directory" => {
                            assert_eq!(card.tool_name, "external directory");
                            assert_eq!(card.tool_input["path"], "/Users/ada/outside");
                            assert_eq!(
                                card.tool_input["filepath"],
                                "/Users/ada/outside/target.txt"
                            );
                            assert_eq!(card.tool_input["via"], "read");
                            assert_eq!(family, "external_directory");
                            assert!(always_scope(&card)
                                .unwrap()
                                .starts_with("/Users/ada/outside and everything below it, "));
                            assert_eq!(risk, "medium");
                        }
                        "external-directory-root" => {
                            assert_eq!(card.tool_name, "external directory");
                            assert_eq!(card.tool_input["path"], "/");
                            assert_eq!(card.tool_input["dirs"], json!(["/"]));
                            assert_eq!(card.tool_input["command"], "cd / && ls");
                            assert_eq!(card.tool_input["via"], "bash");
                            assert!(
                                always_scope(&card).is_none(),
                                "the disk is never granted whole"
                            );
                            assert_eq!(risk, "medium");
                        }
                        "edit" => {
                            assert_eq!(card.tool_name, "file change");
                            assert_eq!(
                                card.tool_input["changes"][0]["path"],
                                "/Users/ada/project/a.txt"
                            );
                            assert_eq!(card.tool_input["changes"][0]["type"], "update");
                            assert_eq!(
                                card.tool_input["changes"][0]["diff"],
                                asked["metadata"]["diff"]
                            );
                            assert!(card.tool_input.get("realpath").is_none());
                            assert_eq!(card.tool_input["note"], WRITES_COMPUTED);
                            assert!(always_scope(&card)
                                .unwrap()
                                .starts_with("Allow ALL edits, "));
                            assert_eq!(family, "edit");
                            assert_eq!(risk, "medium");
                        }
                        "apply-patch-move-over" => {
                            assert_eq!(card.tool_name, "file change");
                            let change = &card.tool_input["changes"][0];
                            assert_eq!(change["path"], "/Users/ada/project/b.txt");
                            assert_eq!(change["type"], "move");
                            assert_eq!(change["move_path"], "/Users/ada/project/c2.txt");
                            assert_eq!(change["overwrites"], "overwrites existing /Users/ada/project/c2.txt (current content not shown)");
                            assert_eq!(change["diff"], asked["metadata"]["diff"]);
                            assert!(change.get("deletes").is_none());
                            assert_eq!(
                                card.tool_input["unchecked_dest"],
                                json!(["/Users/ada/project/c2.txt"])
                            );
                            assert_eq!(
                                card.tool_input["caveat"],
                                "destination not checked against your edit rules"
                            );
                            assert_eq!(card.tool_input["note"], WRITES_COMPUTED);
                            assert!(always_scope(&card).is_some());
                            assert_eq!(risk, "medium");
                        }
                        "read" => {
                            assert_eq!(card.tool_name, "read");
                            assert_eq!(card.tool_input["path"], "/Users/ada/outside/target.txt");
                            assert_eq!(card.tool_input["relative"], "../outside/target.txt");
                            assert!(always_scope(&card).unwrap().starts_with("all reads, "));
                            assert_eq!(risk, "low");
                        }
                        "read-mcp-resource" => {
                            assert_eq!(card.tool_name, "mcp resource");
                            assert_eq!(card.tool_input["server"], "s7mcp");
                            assert_eq!(card.tool_input["uri"], "s7://note");
                            assert_eq!(card.tool_input["via"], "read_mcp_resource");
                            assert_eq!(family, "read");
                            assert!(always_scope(&card).unwrap().starts_with("mcp:s7mcp:*, "));
                            assert_eq!(risk, "medium");
                        }
                        "glob" | "grep" => {
                            assert_eq!(card.tool_name, stem);
                            assert_eq!(card.tool_input["pattern"], asked["metadata"]["pattern"]);
                            assert_eq!(card.tool_input["path"], "/Users/ada/outside");
                            assert!(always_scope(&card)
                                .unwrap()
                                .starts_with(&format!("all {stem} calls, ")));
                            assert_eq!(risk, "low");
                        }
                        "webfetch" => {
                            assert_eq!(card.tool_input["url"], "http://mock.invalid/redir");
                            assert_eq!(card.tool_input["format"], "text");
                            assert_eq!(card.tool_input["caveat"], "follows redirects to any host");
                            assert!(always_scope(&card)
                                .unwrap()
                                .starts_with("all web fetches, "));
                            assert_eq!(risk, "low");
                        }
                        "websearch" => {
                            assert_eq!(card.tool_input["query"], "s7 test query");
                            assert_eq!(card.tool_input["provider"], "exa");
                            assert!(always_scope(&card).unwrap().starts_with("all searches, "));
                            assert_eq!(risk, "low");
                        }
                        "task" => {
                            assert_eq!(card.tool_input["subagent_type"], "general");
                            assert_eq!(card.tool_input["description"], "child job");
                            assert_eq!(
                                card.tool_input["prompt"],
                                doc["anchor"]["partInput"]["prompt"]
                            );
                            assert!(always_scope(&card)
                                .unwrap()
                                .starts_with("every subagent launch, ANY type, "));
                            assert_eq!(risk, "medium");
                        }
                        "todowrite" => {
                            assert_eq!(
                                card.tool_input["todos"],
                                doc["anchor"]["partInput"]["todos"]
                            );
                            assert!(always_scope(&card)
                                .unwrap()
                                .starts_with("all todo-list updates, "));
                            assert_eq!(risk, "medium");
                        }
                        "skill" => {
                            assert_eq!(card.tool_input["name"], "s7skill");
                            assert!(always_scope(&card)
                                .unwrap()
                                .starts_with("the skill s7skill, "));
                            assert_eq!(risk, "medium");
                        }
                        "lsp" => {
                            assert_eq!(card.tool_input["operation"], "hover");
                            assert_eq!(card.tool_input["filePath"], "/Users/ada/project/a.txt");
                            assert_eq!(card.tool_input["line"], 1);
                            assert!(always_scope(&card)
                                .unwrap()
                                .starts_with("all LSP queries, "));
                            assert_eq!(risk, "medium");
                        }
                        "doom-loop" => {
                            assert_eq!(card.tool_name, "glob", "the repeated call's name");
                            assert_eq!(card.tool_input["pattern"], "*.md");
                            assert_eq!(card.tool_input["repeated_call"], "glob");
                            assert_eq!(family, "doom_loop");
                            assert!(always_scope(&card)
                                .unwrap()
                                .starts_with("stop asking about repeated glob calls, "));
                            assert_eq!(risk, "low");
                        }
                        "mcp-tool" => {
                            assert_eq!(card.tool_name, "s7mcp_run_shell");
                            assert_eq!(card.tool_input["cmd"], "rm -rf /");
                            assert_eq!(card.tool_input["server"], "s7mcp");
                            assert_eq!(
                                card.tool_input["caveat"],
                                "arguments as the model sent them"
                            );
                            assert_eq!(family, "mcp");
                            assert!(always_scope(&card)
                                .unwrap()
                                .starts_with("every call of this MCP tool, "));
                            assert_eq!(risk, "high");
                            assert_eq!(
                                serde_json::to_value(card.risk.as_ref().unwrap()).unwrap()
                                    ["matched_pattern"],
                                "rm -rf"
                            );
                        }
                        other => panic!("{other}: a fixture without its rule"),
                    }
                    let _ = input(&card);
                }
                (stem, other) => panic!(
                    "{stem}: {:?}",
                    other.map(|b| matches!(b, Built::Card { .. }))
                ),
            }
        }
    }

    /// What one fixture's `expected` block says, from the builder.
    fn expected_of(doc: &Value) -> Value {
        let (built, _, _) = built(doc);
        let note = doc["expected"].get("note").cloned();
        let mut out = match built {
            None => {
                json!({"card": null, "hidden": true, "reason": "a v2 request: one line, never a card"})
            }
            Some(Built::Hidden(why)) => json!({"card": null, "hidden": true, "reason": why}),
            Some(Built::Stub { .. }) => {
                json!({"card": null, "hidden": false, "reason": "too large to show"})
            }
            Some(Built::Card { card, .. }) => {
                let with_words = option_ids(&card).contains(&"reject_with_words");
                json!({
                    "card": {
                        "tool_name": card.tool_name,
                        "tool_input": card.tool_input,
                        "display_text": card.display_text,
                        "payload_hash": card.payload_hash,
                        "risk": card.risk,
                    },
                    "hidden": false,
                    "always_offered": always_scope(&card).is_some(),
                    "always_text": always_scope(&card),
                    "reject_with_words": with_words,
                })
            }
        };
        if let Some(note) = note {
            out["note"] = note;
        }
        out
    }

    fn fixture_path(stem: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/opencode")
            .join(format!("s7-permission-{stem}-1.18.34.json"))
    }

    /// **The committed cards are this builder's output**, byte for byte: a
    /// change to any card's bytes shows in the fixture's diff. A regression
    /// guard only; the rules are asserted above.
    #[test]
    fn every_captured_permission_ask_builds_the_committed_card() {
        for (stem, doc) in s7() {
            assert_eq!(
                expected_of(&doc),
                doc["expected"],
                "{stem}: run the regenerator"
            );
        }
    }

    /// Rewrites each fixture's `expected` block from the builder. Run with
    /// `cargo test -p ccd --bin ccd -- --ignored regenerate_the_permission_card_fixtures`.
    #[test]
    #[ignore]
    fn regenerate_the_permission_card_fixtures() {
        for (stem, mut doc) in s7() {
            doc["expected"] = expected_of(&doc);
            let text = format!("{}\n", serde_json::to_string_pretty(&doc).unwrap());
            std::fs::write(fixture_path(stem), text).unwrap();
        }
    }

    /// No card wording says or implies that a call stays inside the project:
    /// OpenCode's path checks are lexical, and the absence of an
    /// external-directory card proves nothing.
    #[test]
    fn no_card_says_a_call_stays_inside_the_project() {
        for (stem, doc) in s7() {
            let text = serde_json::to_string(&expected_of(&doc))
                .unwrap()
                .to_lowercase();
            for forbidden in ["inside the project", "project only", "stays in the project"] {
                assert!(!text.contains(forbidden), "{stem} says {forbidden:?}");
            }
        }
    }

    /// A second card about the same tool call says so; each is answered by
    /// its own id.
    #[test]
    fn two_cards_about_one_tool_call_are_told_apart() {
        let (stem, doc) = s7()
            .into_iter()
            .find(|(s, _)| *s == "external-directory")
            .unwrap();
        let (_, events, mut a) = built(&doc);
        assert_eq!(events.len(), 1, "{stem}");
        let mut second = doc["asked"].clone();
        second["id"] = json!("per_second");
        second["permission"] = json!("read");
        second["patterns"] = json!(["../outside/target.txt"]);
        second["always"] = json!(["*"]);
        second["metadata"] = json!({});
        second["anchor_input"] = doc["anchor"]["partInput"].clone();
        second["ctx"] = ctx_of(&doc);
        let events = feed(&mut a, &[ev("permission.asked", second)]);
        let card = card_of(&events[0]);
        assert_eq!(card.tool_name, "read");
        assert_eq!(card.request_id, "per_second");
        assert_eq!(
            card.tool_input["part"],
            "2 of 2: another card is about the same tool call"
        );
        assert_eq!(
            events[0].payload["tool_call_id"],
            doc["asked"]["tool"]["callID"]
        );
    }

    /// The bounds: a command over 8 KiB is cut with a digest and classified
    /// whole, and a card whose input is over 16 KiB without its diffs is a
    /// stub.
    #[test]
    fn a_long_command_is_cut_for_the_phone_and_classified_whole() {
        let (_, doc) = s7().into_iter().find(|(s, _)| *s == "bash").unwrap();
        let long = format!("echo {} ; rm -rf ~/work", "x".repeat(70_000));
        let mut doc = doc;
        doc["asked"]["metadata"]["command"] = json!(long);
        doc["anchor"]["partInput"]["command"] = json!(long);
        let (outcome, _, _) = built(&doc);
        let Some(Built::Card { card, .. }) = outcome else {
            panic!("a card");
        };
        let shown = card.tool_input["command"].as_str().unwrap();
        assert!(
            shown.len() < 9_000 && shown.contains(&protocol::hash::sha256_hex(long.as_bytes()))
        );
        assert_eq!(risk_class(&card), "high", "classified on the whole command");

        doc["anchor"]["partInput"]["description"] = json!("d".repeat(20_000));
        let (outcome, events, _) = built(&doc);
        assert!(matches!(outcome, Some(Built::Stub { .. })));
        assert_eq!(events[0].payload["answerable"], false);
        assert!(card_of(&events[0]).tool_input["message"]
            .as_str()
            .unwrap()
            .starts_with("This request is too large to show"));
    }

    /// A bash card shows the command OpenCode will run, never its patterns:
    /// the patterns come from a shell scan that drops redirects, so a
    /// redirect-only clause is a command the patterns say nothing of.
    #[test]
    fn a_bash_card_shows_the_command_and_never_its_patterns() {
        let (_, doc) = s7().into_iter().find(|(s, _)| *s == "bash").unwrap();
        let mut doc = doc;
        let command = "{ echo pwned; } > r2.txt";
        doc["asked"]["metadata"]["command"] = json!(command);
        doc["asked"]["patterns"] = json!(["echo pwned"]);
        doc["asked"]["always"] = json!(["echo *"]);
        doc["anchor"]["partInput"]["command"] = json!(command);
        let (outcome, _, _) = built(&doc);
        let Some(Built::Card { card, .. }) = outcome else {
            panic!("a card");
        };
        assert_eq!(card.tool_input["command"], command);
        assert_eq!(card.tool_input["always"], json!(["echo *"]));
        assert!(always_scope(&card)
            .unwrap()
            .starts_with("Allow commands starting echo *, "));
    }

    /// Hidden by what the plugin could not read: a context that failed, or a
    /// mode it could not find, shows nothing.
    #[test]
    fn a_card_the_plugin_judged_against_nothing_is_not_shown() {
        let (_, doc) = s7().into_iter().find(|(s, _)| *s == "bash").unwrap();
        for patch in [
            json!({"ok": false}),
            json!({"mode": "unknown", "ok": false}),
        ] {
            let mut a = OpencodeAdapter::new(key(), Arc::new(env_of(&doc)));
            let frames = frames_of(&doc);
            let (ask, setup) = frames.split_last().unwrap();
            feed(&mut a, setup);
            let LinkFrame::Ev { properties, .. } = ask else {
                panic!()
            };
            let mut p = properties.clone();
            for (k, v) in patch.as_object().unwrap() {
                p["ctx"][k] = v.clone();
            }
            assert!(feed(&mut a, &[ev("permission.asked", Value::Object(p))]).is_empty());
            assert!(a.open_cards.is_empty());
        }
    }

    /// A doom loop whose repeated tool is shadowed, or is one this build has no
    /// card for, is not shown either.
    #[test]
    fn a_doom_loop_is_judged_by_the_call_it_repeats() {
        let (_, doc) = s7().into_iter().find(|(s, _)| *s == "doom-loop").unwrap();
        let mut shadowed = doc.clone();
        shadowed["context"]["tool_ids"]
            .as_array_mut()
            .unwrap()
            .push(json!("glob"));
        assert!(matches!(built(&shadowed).0, Some(Built::Hidden(why)) if why.contains("shadowed")));
        let mut custom = doc.clone();
        custom["asked"]["metadata"]["tool"] = json!("s7custom");
        custom["anchor"]["partTool"] = Value::Null;
        assert!(matches!(built(&custom).0, Some(Built::Hidden(_))));
    }

    /// A running part that is not the one the request describes hides the
    /// card: the two would describe different calls.
    #[test]
    fn a_card_is_hidden_when_its_anchor_and_its_request_disagree() {
        let (_, doc) = s7().into_iter().find(|(s, _)| *s == "bash").unwrap();
        let mut other = doc.clone();
        other["anchor"]["partInput"]["command"] = json!("rm -rf /");
        assert!(
            matches!(built(&other).0, Some(Built::Hidden(why)) if why.contains("not the one the request describes"))
        );
        let mut tool = doc.clone();
        tool["anchor"]["partTool"] = json!("read");
        assert!(
            matches!(built(&tool).0, Some(Built::Hidden(why)) if why.contains("anchored to a `read` part"))
        );
    }

    /// Where an external-directory Always is not offered: the disk, the home
    /// folder, the run's folder, an ancestor of either, a pattern with a
    /// wildcard before its last segment, a folder that does not resolve; and
    /// where it is: a plain folder outside them all.
    #[test]
    fn an_external_directory_always_is_withheld_for_the_folders_that_hold_everything() {
        let env = FakeEnv::ada()
            .with_file("/Users/ada/outside")
            .with_file("/System/Volumes/Data")
            .with_link("/Users/ada/link-to-home", "/Users/ada");
        for (entry, offered) in [
            ("/*", false),
            ("/Users/*", false),
            ("/Users/ada/*", false),
            ("/Users/ada/project/*", false),
            ("/Users/ada/link-to-home/*", false),
            ("/System/Volumes/Data/*", false),
            ("/Users/*/outside/*", false),
            ("/Users/ada/n?t/*", false),
            ("/Users/ada/missing/*", false),
            ("/Users/ada/outside/*", true),
        ] {
            assert_eq!(external_always_offered(&env, entry), offered, "{entry}");
        }
    }

    /// A move onto the file itself, by a link or by the same inode, deletes
    /// it: the card says so and offers no Always.
    #[test]
    fn a_patch_that_moves_a_file_onto_itself_deletes_it() {
        let (_, doc) = s7()
            .into_iter()
            .find(|(s, _)| *s == "apply-patch-move-over")
            .unwrap();
        let mut doc = doc;
        doc["asked"]["metadata"]["files"][0]["movePath"] = json!("B.txt");
        doc["context"]["existing_files"] = json!(["/Users/ada/project/b.txt"]);
        let mut a = OpencodeAdapter::new(
            key(),
            Arc::new(
                env_of(&doc).with_link("/Users/ada/project/B.txt", "/Users/ada/project/b.txt"),
            ),
        );
        let frames = frames_of(&doc);
        let events = feed(&mut a, &frames);
        let card = card_of(events.last().unwrap());
        assert_eq!(
            card.tool_input["changes"][0]["deletes"],
            "DELETES /Users/ada/project/b.txt: the move destination is the file itself"
        );
        assert!(always_scope(&card).is_none());
        assert!(card.tool_input.get("unchecked_dest").is_none());
    }

    /// The Always scope names what the operator's config denies when the
    /// grant would cover it, from the config files OpenCode's TUI reads, and
    /// says when that could not be read.
    #[test]
    fn the_always_scope_names_what_the_config_denies() {
        let doc: Value = serde_json::from_str(CONFIG_DENY).unwrap();
        let mut env = FakeEnv::ada();
        env.home = Some(PathBuf::from(doc["home"].as_str().unwrap()));
        env.worktree = PathBuf::from(doc["worktree"].as_str().unwrap());
        for (path, text) in doc["files"].as_object().unwrap() {
            env = env.with_text(path, text.as_str().unwrap());
        }
        for (name, value) in doc["env"].as_object().unwrap() {
            env.vars
                .insert(name.clone(), value.as_str().unwrap().to_string());
        }
        assert_eq!(
            config_files(&env),
            doc["files_read"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| PathBuf::from(p.as_str().unwrap()))
                .collect::<Vec<_>>(),
            "the files, in merge order"
        );
        for case in doc["cases"].as_array().unwrap() {
            let permission = case["permission"].as_str().unwrap();
            let always = strings(case.get("always"));
            let denies = config_denies(&env, permission).unwrap();
            assert_eq!(
                json!(denies),
                case["denies"],
                "{permission}: the merged deny list"
            );
            let scope = scope("the grant", None, &always, &Ok(denies));
            assert_eq!(scope, case["scope"], "{permission} with always {always:?}");
            assert!(scope.contains("including what your config denies"));
        }
        let unreadable = doc["unreadable"].as_str().unwrap();
        env.texts
            .insert(PathBuf::from(unreadable), Err("permission denied".into()));
        let failed = config_denies(&env, "edit");
        assert!(failed.is_err());
        assert_eq!(
            scope("the grant", None, &["*".to_string()], &failed),
            doc["scope_unreadable"]
        );
    }

    /// A request answered at the Mac before its card frame reached the
    /// daemon, or judged ended by a snapshot before it, is never shown: the
    /// card lost the race to its own resolution. A dead reason the next
    /// snapshot re-judges hides nothing.
    #[test]
    fn a_card_that_arrives_after_its_resolution_is_not_shown() {
        let (_, doc) = s7().into_iter().find(|(s, _)| *s == "bash").unwrap();
        let id = doc["asked"]["id"].as_str().unwrap();
        let session = doc["asked"]["sessionID"].as_str().unwrap();
        let frames = frames_of(&doc);
        let (ask, setup) = frames.split_last().unwrap();

        let mut a = OpencodeAdapter::new(key(), Arc::new(env_of(&doc)));
        assert!(
            feed(
                &mut a,
                &[ev(
                    "permission.replied",
                    json!({"sessionID": session, "requestID": id, "reply": "once"})
                )]
            )
            .is_empty(),
            "a reply with no card files nothing"
        );
        feed(&mut a, setup);
        assert!(feed(&mut a, std::slice::from_ref(ask)).is_empty());
        assert!(a.open_cards.is_empty());

        for (reason, shown) in [
            ("epoch:abort", false),
            ("asked-before-activation", false),
            ("not-busy", true),
        ] {
            let mut a = OpencodeAdapter::new(key(), Arc::new(env_of(&doc)));
            feed(&mut a, setup);
            let snap = snapshot_listing(
                vec![(
                    doc["asked"].clone(),
                    json!({"live": false, "dead": reason, "listed": true, "in_store": true}),
                )],
                true,
            );
            let (facts, staged) = a.plan_resync(&snap).unwrap();
            assert!(
                facts.is_empty(),
                "{reason}: nothing is filed for a dead request with no card"
            );
            a.apply_resync(staged);
            let events = feed(&mut a, std::slice::from_ref(ask));
            assert_eq!(!events.is_empty(), shown, "{reason}");
        }
    }

    /// A requests-only snapshot listing `requests` with their verdicts.
    fn snapshot_listing(requests: Vec<(Value, Value)>, requests_ok: bool) -> Snapshot {
        let begin = frame(
            json!({"t": "sync_begin", "sync": 1, "reason": "trigger:session.idle",
            "scope": "requests", "as_of": 9, "status": {}, "status_ok": true}),
        );
        let mut assembly = SyncAssembly::begin(&begin).unwrap();
        for (request, verdict) in requests {
            assembly.add(&frame(
                json!({"t": "sync_request", "sync": 1, "kind": "permission", "request": request, "verdict": verdict}),
            ));
        }
        let end = frame(
            json!({"t": "sync_end", "sync": 1, "done_at": 9, "permissions_ok": requests_ok,
            "questions_ok": true, "requests_ok": requests_ok, "lower": {}, "pages": 0, "items": 0,
            "bytes": 0, "activation": 1}),
        );
        assembly.finish(&end).unwrap()
    }

    /// A snapshot's verdicts on an open card: dead retires it as ended under
    /// the adapter's own key and a second snapshot with the same ghost files
    /// nothing; live leaves it, unrebuilt; unknown changes nothing; a list
    /// that failed applies no verdict at all.
    #[test]
    fn a_snapshots_verdict_retires_a_dead_card_once_and_leaves_a_live_one_as_it_is() {
        let (_, doc) = s7().into_iter().find(|(s, _)| *s == "bash").unwrap();
        let id = doc["asked"]["id"].as_str().unwrap();
        let session = doc["asked"]["sessionID"].as_str().unwrap();
        let mut listed = doc["asked"].clone();
        listed["anchor_input"] = doc["anchor"]["partInput"].clone();
        listed["ctx"] = ctx_of(&doc);
        let verdict = |live: bool, dead: Option<&str>, unknown: Option<&str>| json!({"live": live, "dead": dead, "unknown": unknown, "listed": true, "in_store": true});

        let (_, events, mut a) = built(&doc);
        let card = card_of(&events[0]);
        for (verdict, requests_ok) in [
            (verdict(true, None, None), true),
            (verdict(false, None, Some("server")), true),
            (verdict(false, Some("not-busy"), None), false),
        ] {
            let (facts, staged) = a
                .plan_resync(&snapshot_listing(
                    vec![(listed.clone(), verdict)],
                    requests_ok,
                ))
                .unwrap();
            assert!(facts.is_empty(), "nothing rebuilt, nothing retired");
            a.apply_resync(staged);
            assert_eq!(a.open_cards.len(), 1);
            assert_eq!(
                a.open_cards[0].computed_at.as_deref(),
                Some("2023-11-14T22:13:20Z")
            );
        }
        // A re-delivered ask builds the same bytes: the stored time is reused.
        let again = feed(&mut a, &[ev("permission.asked", listed.clone())]);
        assert_eq!(card_of(&again[0]).payload_hash, card.payload_hash);

        let (facts, staged) = a
            .plan_resync(&snapshot_listing(
                vec![(listed.clone(), verdict(false, Some("not-busy"), None))],
                true,
            ))
            .unwrap();
        a.apply_resync(staged);
        assert_eq!(facts.len(), 1);
        assert_eq!(sid(&facts[0]), format!("{session}:perm_resolved:{id}"));
        assert_eq!(facts[0].source, Source::Opencode);
        assert_eq!(
            facts[0].payload,
            json!({"request_id": id, "status": "cleared", "cause": "ended"})
        );
        assert!(a.open_cards.is_empty());

        let (facts, _) = a
            .plan_resync(&snapshot_listing(
                vec![(listed, verdict(false, Some("not-busy"), None))],
                true,
            ))
            .unwrap();
        assert!(
            facts.is_empty(),
            "the ghost is judged dead again and files nothing"
        );
    }

    /// OpenCode's own cascade: after an Always or a Reject in a session, every
    /// pending request of it is replied the same word, each on its own
    /// `permission.replied` with nothing that says so. Each card is answered
    /// at the Mac with the word on the wire, as the keyboard's reply is.
    #[test]
    fn a_cascaded_reply_answers_each_card_with_the_word_on_the_wire() {
        let (_, doc) = s7().into_iter().find(|(s, _)| *s == "bash").unwrap();
        let session = "ses_f0795e02affe02FT8ErNd6G9WO";
        for (reply, first, second) in [
            (
                "always",
                "per_0f86a29e7001I2bgsaXNXFilC5",
                "per_0f86a29e8001RA6x6q01ZKVf06",
            ),
            (
                "reject",
                "per_0f869cfc0001Wetl7E7G4kRTpy",
                "per_0f869cfc9001rlOrGufjSqkOLz",
            ),
        ] {
            let mut a = OpencodeAdapter::new(key(), Arc::new(env_of(&doc)));
            let mut frames = Vec::new();
            for (id, call, command) in [
                (first, "call_30_0", "echo one > out1.txt"),
                (second, "call_30_1", "echo two > out2.txt"),
            ] {
                frames.push(ev(
                    "message.part.updated",
                    json!({"part": {"id": format!("prt_{call}"), "sessionID": session, "messageID": "msg_01",
                        "type": "tool", "tool": "bash", "callID": call,
                        "state": {"status": "running", "input": {"command": command}}}}),
                ));
                let mut ask = doc["asked"].clone();
                ask["id"] = json!(id);
                ask["sessionID"] = json!(session);
                ask["metadata"] = json!({"command": command});
                ask["patterns"] = json!([command]);
                ask["tool"] = json!({"messageID": "msg_01", "callID": call});
                ask["anchor_input"] = json!({"command": command});
                ask["ctx"] = ctx_of(&doc);
                frames.push(ev("permission.asked", ask));
            }
            let cards = feed(&mut a, &frames);
            assert_eq!(
                cards
                    .iter()
                    .filter(|e| e.kind == EventKind::ApprovalRequest)
                    .count(),
                2
            );
            let replies = feed(
                &mut a,
                &[
                    ev(
                        "permission.replied",
                        json!({"sessionID": session, "requestID": first, "reply": reply}),
                    ),
                    ev(
                        "message.part.updated",
                        json!({"part": {"id": "prt_call_30_0", "sessionID": session,
                        "messageID": "msg_01", "type": "tool", "tool": "bash", "callID": "call_30_0",
                        "state": {"status": "completed", "input": {"command": "echo one > out1.txt"}, "output": ""}}}),
                    ),
                    ev(
                        "permission.replied",
                        json!({"sessionID": session, "requestID": second, "reply": reply}),
                    ),
                ],
            );
            let resolved: Vec<&PendingEvent> = replies
                .iter()
                .filter(|e| e.kind == EventKind::ApprovalResolved)
                .collect();
            assert_eq!(resolved.len(), 2, "{reply}");
            for (event, id) in resolved.iter().zip([first, second]) {
                assert_eq!(sid(event), format!("{session}:perm_resolved:{id}"));
                assert_eq!(
                    event.payload,
                    json!({"request_id": id, "status": "answered", "by": "local",
                        "decision": {"type": "option_id", "option_id": reply}})
                );
            }
            assert!(a.open_cards.is_empty());
        }
    }

    /// Under auto mode OpenCode replies `once` before any plugin listener
    /// runs: no card frame is written, and the reply that follows has no card
    /// and files nothing. A request pending when auto was switched on stays
    /// listed and keeps its card.
    #[test]
    fn an_auto_handled_ask_files_nothing_and_a_pre_toggle_card_stands() {
        let doc: Value = serde_json::from_str(S5).unwrap();
        let (_, bash) = s7().into_iter().find(|(s, _)| *s == "bash").unwrap();
        for case in doc["cases"].as_array().unwrap() {
            let name = case["case"].as_str().unwrap();
            let frames = case["frames"].as_array().unwrap();
            let mut a = OpencodeAdapter::new(key(), Arc::new(env_of(&bash)));
            let mut filed = 0;
            for f in frames {
                match f["kind"].as_str() {
                    Some("asked") if f["inStoreSync"] == true => {
                        let (id, sid_, cmd) = (
                            f["id"].as_str().unwrap(),
                            f["sid"].as_str().unwrap(),
                            f["cmd"].as_str().unwrap(),
                        );
                        let mut ask = bash["asked"].clone();
                        ask["id"] = json!(id);
                        ask["sessionID"] = json!(sid_);
                        ask["metadata"] = json!({"command": cmd});
                        ask["patterns"] = json!([cmd]);
                        ask["tool"] = json!({"messageID": "msg_01", "callID": id});
                        ask["anchor_input"] = json!({"command": cmd});
                        ask["ctx"] = ctx_of(&bash);
                        let events = feed(
                            &mut a,
                            &[
                                ev(
                                    "message.part.updated",
                                    json!({"part": {"id": format!("prt_{id}"), "sessionID": sid_,
                                "messageID": "msg_01", "type": "tool", "tool": "bash", "callID": id,
                                "state": {"status": "running", "input": {"command": cmd}}}}),
                                ),
                                ev("permission.asked", ask),
                            ],
                        );
                        assert_eq!(
                            events.len(),
                            2,
                            "{name}: a card for an ask the keyboard shows"
                        );
                        filed += 1;
                    }
                    // Seen under auto: the TUI never stores it, the plugin writes no frame.
                    Some("asked") => {}
                    Some("replied") => {
                        let events = feed(
                            &mut a,
                            &[ev(
                                "permission.replied",
                                json!({"sessionID": f["sid"], "requestID": f["id"], "reply": f["reply"]}),
                            )],
                        );
                        let open_before = filed > 0;
                        assert_eq!(
                            !events.is_empty(),
                            open_before
                                && a.retired.iter().all(|r| r != f["id"].as_str().unwrap())
                                && events.len() == 1,
                            "{name}: a reply files a resolution only for a card"
                        );
                        if !events.is_empty() {
                            filed -= 1;
                        }
                    }
                    _ => {}
                }
            }
            assert!(a.open_cards.is_empty(), "{name}");
        }
    }

    /// **The liveness captures replayed into cards.** Each case's asks are
    /// raised and each recorded snapshot is applied as the plugin's verdict
    /// on them. A request the oracle found dead is retired by the snapshot
    /// that judged it so, and one it found live keeps its card — except the
    /// two designed refusals the capture names.
    #[test]
    fn every_liveness_case_retires_exactly_the_requests_the_oracle_found_dead() {
        let doc: Value = serde_json::from_str(S6).unwrap();
        let (_, bash) = s7().into_iter().find(|(s, _)| *s == "bash").unwrap();
        let known: Vec<&str> = doc["known_disagreements"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut judged: HashSet<(String, u64, String, String)> = HashSet::new();
        for case in doc["cases"].as_array().unwrap() {
            let name = case["case"].as_str().unwrap();
            let mut a = OpencodeAdapter::new(key(), Arc::new(env_of(&bash)));
            let mut asked: HashMap<String, Value> = HashMap::new();
            // Snapshots are numbered per activation, so a judgement is
            // keyed by the snapshot's tag as well.
            let oracle: HashMap<(u64, String, String), String> = case["judged"]
                .as_array()
                .unwrap()
                .iter()
                .map(|j| {
                    (
                        (
                            j["snap"].as_u64().unwrap(),
                            j["tag"].as_str().unwrap().to_string(),
                            j["request"].as_str().unwrap().to_string(),
                        ),
                        j["oracle"].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            for f in case["frames"].as_array().unwrap() {
                match (f["kind"].as_str(), f["ty"].as_str()) {
                    (Some("ev"), Some("session.created")) => {
                        let mut info = json!({"id": f["sid"]});
                        if let Some(parent) = f["parent"].as_str() {
                            info["parentID"] = json!(parent);
                        }
                        feed(&mut a, &[ev("session.created", json!({"info": info}))]);
                    }
                    (Some("ev"), Some("permission.asked")) => {
                        let (id, sid_, perm) = (
                            f["id"].as_str().unwrap(),
                            f["sid"].as_str().unwrap(),
                            f["perm"].as_str().unwrap(),
                        );
                        let meta: Value = f["meta"]
                            .as_str()
                            .map(|m| serde_json::from_str(m).unwrap())
                            .unwrap_or(json!({}));
                        let (tool, input) = match perm {
                            "doom_loop" => (
                                meta["tool"].as_str().unwrap().to_string(),
                                meta["input"].clone(),
                            ),
                            "bash" => ("bash".to_string(), json!({"command": meta["command"]})),
                            "edit" => ("edit".to_string(), json!({"filePath": meta["filepath"]})),
                            other => (other.to_string(), meta.clone()),
                        };
                        let call = f["tool"]["callID"]
                            .as_str()
                            .unwrap_or("call_doom")
                            .to_string();
                        let message = f["tool"]["messageID"]
                            .as_str()
                            .unwrap_or("msg_doom")
                            .to_string();
                        let mut ask = json!({"id": id, "sessionID": sid_, "permission": perm,
                            "patterns": [], "metadata": meta, "always": [], "anchor_input": input,
                            "ctx": {"ok": true, "tool_ids": bash["context"]["tool_ids"], "mcp": [], "mode": "normal", "parent": null, "ms": 0}});
                        if f["tool"].is_object() {
                            ask["tool"] = f["tool"].clone();
                        }
                        let events = feed(
                            &mut a,
                            &[
                                ev(
                                    "message.part.updated",
                                    json!({"part": {"id": format!("prt_{call}"), "sessionID": sid_,
                                "messageID": message, "type": "tool", "tool": tool, "callID": call,
                                "state": {"status": "running", "input": input}}}),
                                ),
                                ev("permission.asked", ask.clone()),
                            ],
                        );
                        assert_eq!(events.len(), 2, "{name}: {perm} is carded");
                        asked.insert(id.to_string(), ask);
                    }
                    (Some("ev"), Some("question.asked")) => {
                        feed(
                            &mut a,
                            &[ev(
                                "question.asked",
                                json!({"id": f["id"], "sessionID": f["sid"], "questions": []}),
                            )],
                        );
                    }
                    (Some("ev"), Some("permission.replied" | "question.replied")) => {
                        feed(
                            &mut a,
                            &[ev(
                                f["ty"].as_str().unwrap(),
                                json!({"sessionID": f["sid"], "requestID": f["id"], "reply": f["reply"]}),
                            )],
                        );
                    }
                    (Some("snap"), _) => {
                        let n = f["n"].as_u64().unwrap();
                        // The plugin lists what the server lists, or what the
                        // TUI store holds when the list failed; a request in
                        // neither is sent by no snapshot, and the settle after
                        // a readable list clears its card.
                        let reqs: Vec<(Value, Value)> = f["reqs"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|r| r["listed"] != false)
                            .map(|r| {
                                let id = r["id"].as_str().unwrap();
                                let request = asked.get(id).cloned().unwrap_or(json!({"id": id, "sessionID": r["sid"], "questions": []}));
                                let live = r["r3"] == true;
                                // The probe says `epoch` for every record the
                                // plugin spells as `epoch:<event>`.
                                let dead = match (live, r["deadReason"].as_str()) {
                                    (true, _) => Value::Null,
                                    (false, Some("epoch")) => json!("epoch:abort"),
                                    (false, _) => r["deadReason"].clone(),
                                };
                                (request, json!({"live": live, "dead": dead, "listed": r["listed"], "in_store": r["inStore"]}))
                            })
                            .collect();
                        let ok = f["list"]["perm"]["ok"] == true && f["list"]["q"]["ok"] == true;
                        a.sync_begin();
                        let (mut facts, staged) =
                            a.plan_resync(&snapshot_listing(reqs, ok)).unwrap();
                        a.apply_resync(staged);
                        facts.extend(a.settle());
                        // A request the server still lists as live after
                        // its card was retired is raised again: the phone is
                        // shown the truth rather than an unblocked run.
                        for fact in facts
                            .iter()
                            .filter(|f| f.kind != EventKind::ApprovalRequest)
                        {
                            assert_eq!(fact.kind, EventKind::ApprovalResolved, "{name} snap {n}");
                            assert!(
                                matches!(
                                    fact.payload["cause"].as_str(),
                                    Some("ended" | "superseded")
                                ),
                                "{name} snap {n}: {}",
                                fact.payload
                            );
                        }
                        for r in f["reqs"].as_array().unwrap() {
                            let id = r["id"].as_str().unwrap();
                            // A snapshot's tag may carry the activation it
                            // was read in (`activate#2`); the judgement's
                            // does not.
                            let tag = f["tag"]
                                .as_str()
                                .unwrap_or_default()
                                .split('#')
                                .next()
                                .unwrap_or_default()
                                .to_string();
                            let Some(truth) = oracle.get(&(n, tag.clone(), id.to_string())) else {
                                continue;
                            };
                            judged.insert((name.to_string(), n, tag, id.to_string()));
                            // A failed permission list applies no verdict:
                            // the cards keep their state and the plugin's
                            // own check decides at the tap.
                            if known.contains(&name) || !ok {
                                continue;
                            }
                            let open = a.open_cards.iter().any(|c| c.request_id == id);
                            assert_eq!(open, truth == "LIVE", "{name} snap {n} {id}: the card is open exactly while the request is live");
                        }
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(
            judged.len(),
            doc["cases"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["judged"].as_array().unwrap().len())
                .sum::<usize>()
        );
    }
}
