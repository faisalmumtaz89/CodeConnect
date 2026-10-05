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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use protocol::event::{EventKind, PendingEvent, SessionKey, Source};
use protocol::ws::{
    ApprovalCard, ClearCause, CodexResolution, CodexResolutionPayload, ResolutionActor,
};
use serde_json::{json, Map, Value};

use crate::opencode_link::{
    CardIds, LinkFrame, RequestBody, RequestKind, SnapMessage, SnapPart, SnapRequest, SnapSession,
    Snapshot, StubIds, SyncScope,
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
    pub(crate) fn new(session: SessionKey) -> Self {
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
    /// TurnComplete, tool result or usage is made for it.
    pub(crate) fn session_end(&mut self) -> Vec<PendingEvent> {
        let out = self.close_all();
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
            if !set_aside && request.id().is_some_and(|id| !plan.placed.contains(id)) {
                out.extend(plan.m.asked(request));
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
    ) {
        self.request_turn.insert(request_id.to_string(), turn);
        match self
            .open_cards
            .iter_mut()
            .find(|c| c.request_id == request_id)
        {
            Some(card) => card.session = session.to_string(),
            None => self.open_cards.push(OpenCard {
                request_id: request_id.to_string(),
                session: session.to_string(),
                kind,
            }),
        }
    }

    fn card_fact(
        &self,
        kind: RequestKind,
        session: &str,
        family: &str,
        call_id: Option<&str>,
        card: ApprovalCard,
        turn: Option<&str>,
    ) -> Emit {
        let request_id = card.request_id.clone();
        let prefix = match kind {
            RequestKind::Permission => "perm",
            RequestKind::Question => "question",
        };
        let payload = json!({
            "request_id": request_id, "family": family, "tool_call_id": call_id, "card": card,
        });
        self.fact(
            EventKind::ApprovalRequest,
            session,
            &format!("{prefix}:{request_id}"),
            payload,
            turn,
            Some(&request_id),
        )
    }

    fn on_asked(&mut self, kind: RequestKind, p: &Map<String, Value>) -> Vec<Emit> {
        let (Some(id), Some(session)) = (str_of(p, "id"), str_of(p, "sessionID")) else {
            return Vec::new();
        };
        let tool = obj(p, "tool");
        let turn = self.card_turn(session, tool);
        self.open_card(id, session, kind, turn.clone());
        let (family, card) = match kind {
            RequestKind::Permission => {
                let permission = str_of(p, "permission").unwrap_or_default();
                (permission.to_string(), permission_card(id, permission, p))
            }
            RequestKind::Question => ("question".to_string(), question_card(id, p)),
        };
        let call_id = tool.and_then(|t| str_of(t, "callID"));
        vec![self.card_fact(kind, session, &family, call_id, card, turn.as_deref())]
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
        self.open_card(&ids.id, &ids.session_id, kind, turn.clone());
        let family = match kind {
            RequestKind::Permission => ids.permission.clone().unwrap_or_default(),
            RequestKind::Question => "question".to_string(),
        };
        let card = stub_card(&ids.id, &family, size, sha256);
        let call_id = tool.and_then(|t| str_of(t, "callID"));
        vec![self.card_fact(
            kind,
            &ids.session_id,
            &family,
            call_id,
            card,
            turn.as_deref(),
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

    /// Answered at the Mac. Which way is not said: the reply words are not
    /// part of this card's vocabulary.
    fn on_replied(&mut self, kind: RequestKind, p: &Map<String, Value>) -> Vec<Emit> {
        let (Some(id), Some(session)) = (str_of(p, "requestID"), str_of(p, "sessionID")) else {
            return Vec::new();
        };
        self.open_cards.retain(|c| c.request_id != id);
        let turn = self
            .request_turn
            .get(id)
            .cloned()
            .flatten()
            .or_else(|| self.current.get(self.root(session)).cloned());
        let answered = CodexResolution::Answered {
            by: ResolutionActor::Local,
            decision: None,
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
}

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

/// A shell command is shown as the command OpenCode will run, never as its
/// patterns, which are a prefix of it. Any other permission is shown by name
/// with the patterns it asks for.
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
        match request.id() {
            Some(id) if self.placed.insert(id.to_string()) => self.m.asked(request),
            _ => Vec::new(),
        }
    }
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
                adapter: OpencodeAdapter::new(key()),
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
            self.adapter = OpencodeAdapter::new(key());
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
        let mut a = OpencodeAdapter::new(key());
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
        let mut a = OpencodeAdapter::new(key());
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
        let mut a = OpencodeAdapter::new(key());
        feed(&mut a, &[asked("per_01")]);
        a.sync_begin();
        let (_, staged) = a.plan_resync(&requests_snapshot(false)).unwrap();
        a.apply_resync(staged);
        assert!(a.settle().is_empty());
        assert_eq!(a.open_cards.len(), 1);
    }

    #[test]
    fn the_contract_run_reads_through_the_filter_the_snapshots_and_the_settles() {
        let mut a = OpencodeAdapter::new(key());
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
        // Still listed by every snapshot: never cleared.
        assert_eq!(
            card("que_0f00b00000001xxxxxxxxxxxxx"),
            [json!(["approval_request", null, null])]
        );
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
        assert_eq!(
            bash.payload["card"]["tool_input"],
            json!({"command": "ls -la"})
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
        let mut a = OpencodeAdapter::new(key());
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
            json!({"request_id": "per_1", "status": "answered", "by": "local"})
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
        let mut a = OpencodeAdapter::new(key());
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
        let mut a = OpencodeAdapter::new(key());
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
        let mut a = OpencodeAdapter::new(key());
        // A daemon that started mid-turn: the turn's prompt was never seen.
        feed(&mut a, &[assistant("msg_02", "msg_01", true), idle()]);
        assert!(a.take_resync_request());
        assert!(!a.take_resync_request(), "asked once");

        let mut quiet = OpencodeAdapter::new(key());
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
        let mut a = OpencodeAdapter::new(key());
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
        let mut a = OpencodeAdapter::new(key());
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
        let mut a = OpencodeAdapter::new(key());
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
        let mut a = OpencodeAdapter::new(key());
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
        let a = OpencodeAdapter::new(key());
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
        let mut a = OpencodeAdapter::new(key());
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
}
