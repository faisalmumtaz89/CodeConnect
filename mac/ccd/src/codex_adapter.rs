//! Codex app-server → agent-neutral event normalization.
//!
//! This is the observation half of the Codex adapter (CODEX-PLAN Phase 2, the
//! "single source; fences" section). It takes the app-server **notification**
//! frames a connection delivers — the real codex-cli 0.147 shapes, captured in
//! `fixtures/codex/` — and turns them into [`PendingEvent`]s in the same
//! thread → turn → item model the Claude path already feeds the phone. Nothing
//! here talks to a socket, a broker, or the daemon: it is a pure, unit-testable
//! mapper over frames. The live WS-over-UDS client, the broker, approvals, the
//! resolution envelope, steering, and the switch machinery are separate chunks.
//!
//! ## What the frames actually look like (grounded in the fixtures)
//!
//! Every mapping decision below cites the fixture line it was read off. The
//! ground truth is the captured stream, not the plan's prose — where they
//! diverge, the code follows the capture and the divergence is reported.
//!
//! ## Source and dedup
//!
//! Events carry [`Source::Codex`]: the app-server stream is the single
//! first-hand structured observation of a Codex session, exactly as a hook is
//! for Claude. The daemon dedup key is `(session_uid, source, source_event_id)`
//! (`store.rs`), so this module's whole job on the id side is to mint a **stable,
//! unique** `source_event_id` per fact, **thread-namespaced** per D4 so a thread
//! switch inside one session cannot alias two threads' item ids. Tool calls and
//! their results share one upstream `item.id`, so — matching the Claude
//! convention (`state.rs`: `pre:`/`post:`) — the call is `…:pre:<id>` and the
//! result `…:post:<id>`, or they would collapse to one row.
//!
//! ## In-flight state and interrupted turns (D14/D15)
//!
//! Most mappings are per-frame pure. The one exception is an **interrupted**
//! (or failed) turn: `turn/completed{status:"interrupted"}` carries `items: []`
//! (`itemsView:"notLoaded"`) even for items that were mid-flight, and those
//! items never receive their own `item/completed` (D14). So the adapter tracks
//! the items it has seen `item/started` for and, at the aborted boundary,
//! synthesizes their terminal states from what it observed **live** — never
//! from cross-resume item-id equality, which D15 says is unstable inside an
//! interrupted turn.
//!
//! The live caller is [`crate::codex_link`], which holds the connection, stamps
//! each frame at ingress and feeds the admitted ones through here. Nothing in this
//! module knows that: it is still a pure mapper over frames, and the resume-response
//! Nothing here knows that: it is still a pure mapper over frames.

use std::collections::{HashMap, VecDeque};

use protocol::event::{EventKind, PendingEvent, SessionKey, Source};
use serde_json::{json, Value};

/// A schema-required routing identity: present, a JSON string, and **non-empty**.
/// A frame missing any of these is malformed, and the adapter fails closed —
/// dropping the frame rather than defaulting to `""`. Defaulting would strip the
/// thread namespace off a `source_event_id` (`:pre:*` with no thread prefix) and,
/// worse, let an identity-less frame collide with or mutate another thread's
/// correlation state.
fn require_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// An `item/started` we have not yet seen `item/completed` (or a turn terminal)
/// for. Kept in arrival order so a synthesized timeline is deterministic.
struct OpenItem {
    /// The thread this item belongs to. Correlation is keyed by
    /// `(thread_id, item_id)`, never `item_id` alone, so a frame for thread B can
    /// never close or re-namespace thread A's open item.
    thread_id: String,
    item_id: String,
    /// Required and non-empty (validated at `item/started`), so an interrupt can
    /// always drain this item by its turn.
    turn_id: String,
    /// The item's `type` string as the app-server sent it.
    item_type: String,
    /// The started-item snapshot, used to synthesize a terminal for a
    /// tool/file item that an interrupt caught mid-flight.
    started: Value,
    /// Accumulated `item/agentMessage/delta` text, so a message interrupted
    /// before `item/completed` can still be synthesized with what streamed.
    delta_text: String,
}

/// Normalizes one Codex app-server session's notification stream into the
/// neutral event model. Construct it with the daemon's [`SessionKey`] for the
/// run; feed it frames in arrival order.
pub struct CodexAdapter {
    session: SessionKey,
    /// Items seen `item/started` but not yet terminated, in arrival order.
    open: VecDeque<OpenItem>,
    /// The latest `thread/tokenUsage/updated` snapshot per turn. Usage updates
    /// several times within a turn (each a cumulative total); emitting one event
    /// per update would either duplicate the dedup key or, worse, let a stale
    /// early total win it. So the adapter holds the latest and emits exactly one
    /// authoritative Usage fact when the turn terminates. Keyed by
    /// `(thread_id, turn_id)` so one thread's usage can never be attributed to,
    /// or removed by, another thread's turn.
    latest_usage: HashMap<(String, String), Value>,
}

impl CodexAdapter {
    pub fn new(session: SessionKey) -> Self {
        CodexAdapter {
            session,
            open: VecDeque::new(),
            latest_usage: HashMap::new(),
        }
    }

    /// Normalize one app-server frame. Returns zero or more facts: most frames
    /// map to exactly one event, a few (an interrupted turn) fan out to several,
    /// and the many observation-only frames map to none. Never panics — an
    /// unrecognized or malformed frame yields an empty result.
    pub fn ingest(&mut self, frame: &Value) -> Vec<PendingEvent> {
        // A frame with no `method` is a response to one of our requests (an
        // `{id,result}`/`{id,error}`), not a notification — the observation path
        // does not read those. Drop safely.
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            return Vec::new();
        };
        let params = frame.get("params").unwrap_or(&Value::Null);

        match method {
            // thread/started — the thread identity + rollout path + cwd, on a
            // merely-initialized connection (fixtures/codex/lifecycle.jsonl:1,
            // params.thread.{id,path,cwd,status}). Announces the session.
            "thread/started" => self.on_thread_started(params),

            // item/started — a new item enters the turn. Tool/file items render
            // a ToolCall now; message/reasoning items wait for their terminal.
            // (lifecycle.jsonl userMessage/agentMessage; command-execution.jsonl
            // commandExecution; file-change.jsonl fileChange.)
            "item/started" => self.on_item_started(params),

            // item/completed — the authoritative terminal item. This is where a
            // message/reasoning becomes a fact and a tool item becomes a result.
            "item/completed" => self.on_item_completed(params),

            // item/agentMessage/delta — streamed text (lifecycle.jsonl). Not a
            // fact (the event log is facts, not deltas); accumulated only so an
            // interrupted message can still be synthesized.
            "item/agentMessage/delta" => {
                self.on_agent_delta(params);
                Vec::new()
            }

            // thread/tokenUsage/updated — per-turn cumulative token usage
            // (lifecycle.jsonl: params.{threadId,turnId,tokenUsage}).
            "thread/tokenUsage/updated" => {
                self.on_token_usage(params);
                Vec::new()
            }

            // turn/completed — the turn terminal, and the aborted boundary where
            // interrupted items are synthesized (lifecycle.jsonl completed;
            // interrupt.jsonl interrupted with items:[]).
            "turn/completed" => self.on_turn_completed(params),

            // turn/started — observed live (interrupt.jsonl), but per A1/A3 it is
            // never replayed and a late subscriber rebuilds in-flight state from
            // the resume response's turns[].status instead. The neutral model has
            // no "turn started" fact, so it maps to nothing.
            //
            // In-flight state on a late attach comes from the `thread/resume`
            // **response** instead. Reconciling that response against this open
            // set is **2e-4's**, not this chunk's: no turn can run through the
            // broker until the D2 head-check lands (`turn/start` fails closed on
            // the TUI leg), so no turn, no in-flight item and no populated
            // `turns[]` can exist on the wire yet — and a reconciliation designed
            // against inputs nobody can produce is a guess. 2e-4 builds it against
            // real evidence; until then [`crate::codex_link`] refuses any resume
            // response that describes a turn at all.
            "turn/started" => Vec::new(),

            // Known, deliberately not rendered: observation noise, ownership/
            // config, or streamed output that carries no timeline fact.
            "thread/status/changed"
            | "thread/goal/cleared"
            | "account/rateLimits/updated"
            | "remoteControl/status/changed"
            | "mcpServer/startupStatus/updated"
            | "thread/settings/updated"
            | "turn/diff/updated"
            | "app/list/updated"
            | "item/commandExecution/outputDelta"
            | "item/commandExecution/terminalInteraction" => Vec::new(),

            // Approval request/resolution is Phase 3 (typed cards, composite-id
            // activations, the resolution envelope, option ids). It is observed
            // in this stream (command-execution.jsonl, interrupt.jsonl) but
            // deliberately NOT normalized here — emitting a bare approval event
            // without that machinery would be exactly the speculative half-surface
            // the plan forbids. Dropped, tracked for Phase 3.
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "serverRequest/resolved" => Vec::new(),

            // A method this build does not know: never panic, never guess. Drop.
            _ => Vec::new(),
        }
    }

    fn on_thread_started(&mut self, params: &Value) -> Vec<PendingEvent> {
        let Some(thread) = params.get("thread") else {
            return Vec::new();
        };
        let Some(tid) = require_str(thread, "id") else {
            return Vec::new();
        };
        let field = |k: &str| thread.get(k).cloned().unwrap_or(Value::Null);
        let payload = json!({
            "thread_id": tid,
            "cwd": field("cwd"),
            "rollout_path": field("path"),
            "status": field("status"),
            "cli_version": field("cliVersion"),
        });
        vec![self
            .event(EventKind::SessionStart, payload)
            .with_source_event_id(sid(tid, "thread_started"))]
    }

    fn on_item_started(&mut self, params: &Value) -> Vec<PendingEvent> {
        let Some(item) = params.get("item") else {
            return Vec::new();
        };
        // Fail closed: every routing identity the 0.147 schema requires must be
        // present and non-empty, or the frame is malformed and mutates nothing.
        let (Some(tid), Some(turn), Some(item_id), Some(item_type)) = (
            require_str(params, "threadId"),
            require_str(params, "turnId"),
            require_str(item, "id"),
            require_str(item, "type"),
        ) else {
            return Vec::new();
        };

        // Track the open item so an interrupt can synthesize its terminal.
        self.open.push_back(OpenItem {
            thread_id: tid.to_string(),
            item_id: item_id.to_string(),
            turn_id: turn.to_string(),
            item_type: item_type.to_string(),
            started: item.clone(),
            // agentMessage started with text:"" (lifecycle.jsonl); seed from it.
            delta_text: item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });

        // Only executable items render a call at start; a message/reasoning has
        // nothing terminal to show yet.
        match item_type {
            "commandExecution" | "fileChange" => {
                vec![self
                    .event(EventKind::ToolCall, tool_call_payload(item))
                    .with_source_event_id(sid(tid, &format!("pre:{item_id}")))
                    .with_turn_id(Some(turn.to_string()))
                    .with_item_id(Some(item_id.to_string()))]
            }
            _ => Vec::new(),
        }
    }

    fn on_item_completed(&mut self, params: &Value) -> Vec<PendingEvent> {
        let Some(item) = params.get("item") else {
            return Vec::new();
        };
        let (Some(tid), Some(turn), Some(item_id), Some(item_type)) = (
            require_str(params, "threadId"),
            require_str(params, "turnId"),
            require_str(item, "id"),
            require_str(item, "type"),
        ) else {
            return Vec::new();
        };

        // A commandExecution/fileChange terminal MUST carry its required `status`
        // (0.147 schema). A missing one is malformed: fabricating a "completed"
        // ToolResult would persist false success. Drop it WITHOUT closing the open
        // item, so a real terminal arriving later still resolves — or, if the turn
        // is interrupted first, the open item is still synthesized.
        if matches!(item_type, "commandExecution" | "fileChange")
            && require_str(item, "status").is_none()
        {
            return Vec::new();
        }

        self.close_open(tid, turn, item_id);

        self.terminal_item_event(tid, Some(turn.to_string()), item_id, item_type, item, false)
            .into_iter()
            .collect()
    }

    fn on_agent_delta(&mut self, params: &Value) {
        let (Some(tid), Some(turn), Some(item_id), Some(delta)) = (
            require_str(params, "threadId"),
            require_str(params, "turnId"),
            require_str(params, "itemId"),
            params.get("delta").and_then(Value::as_str),
        ) else {
            return;
        };
        // Correlate by (thread, turn, item) — a delta missing its turn, or naming
        // the wrong turn, must not mutate text that a later interrupted-turn
        // synthesis would emit under a different turn.
        if let Some(open) = self
            .open
            .iter_mut()
            .find(|o| o.thread_id == tid && o.turn_id == turn && o.item_id == item_id)
        {
            open.delta_text.push_str(delta);
        }
    }

    fn on_token_usage(&mut self, params: &Value) {
        let (Some(tid), Some(turn)) = (
            require_str(params, "threadId"),
            require_str(params, "turnId"),
        ) else {
            return;
        };
        let Some(usage) = params.get("tokenUsage") else {
            return;
        };
        // Hold the latest cumulative total for this (thread, turn); the one Usage
        // fact is emitted when the turn terminates (see `on_turn_completed`).
        self.latest_usage
            .insert((tid.to_string(), turn.to_string()), usage.clone());
    }

    fn on_turn_completed(&mut self, params: &Value) -> Vec<PendingEvent> {
        let Some(tid) = require_str(params, "threadId") else {
            return Vec::new();
        };
        let Some(turn) = params.get("turn") else {
            return Vec::new();
        };
        let Some(turn_id) = require_str(turn, "id") else {
            return Vec::new();
        };
        // `status` is required (0.147 schema). A missing one is malformed:
        // defaulting to "completed" would both fabricate a successful TurnComplete
        // AND drain-and-discard the open items, suppressing the interrupted
        // synthesis a real terminal would have driven. Drop WITHOUT mutating
        // state, so a later well-formed terminal still resolves the turn.
        let Some(status) = require_str(turn, "status") else {
            return Vec::new();
        };

        let mut out = Vec::new();

        // Aborted boundary (D14): the terminal carries items:[] even for
        // in-flight items, which never get their own item/completed. Synthesize
        // a terminal for each still-open item of this turn, from what we tracked
        // live (D15: our own observed ids, never cross-resume equality). A clean
        // `completed` turn's items already terminalized via item/completed, so
        // only interrupted/failed synthesize.
        let synthesize = matches!(status, "interrupted" | "failed");
        let dangling: Vec<OpenItem> = self.drain_turn_items(tid, turn_id);
        if synthesize {
            for open in dangling {
                if let Some(ev) = self.terminal_item_event(
                    tid,
                    Some(open.turn_id.clone()),
                    &open.item_id,
                    &open.item_type,
                    &open.synthesized_terminal(),
                    true,
                ) {
                    out.push(ev);
                }
            }
        }
        // (For `completed`, `dangling` is dropped without synthesis — the items
        // completed normally and were already emitted.)

        // The one authoritative token-usage fact for this turn — the latest
        // cumulative total we saw, emitted once as the turn closes.
        if let Some(usage) = self
            .latest_usage
            .remove(&(tid.to_string(), turn_id.to_string()))
        {
            out.push(
                self.event(EventKind::Usage, usage)
                    .with_source_event_id(sid(tid, &format!("usage:{turn_id}")))
                    .with_turn_id(Some(turn_id.to_string())),
            );
        }

        // The turn terminal itself.
        let tfield = |k: &str| turn.get(k).cloned().unwrap_or(Value::Null);
        let payload = json!({
            "status": status,
            "error": tfield("error"),
            "started_at": tfield("startedAt"),
            "completed_at": tfield("completedAt"),
            "duration_ms": tfield("durationMs"),
        });
        out.push(
            self.event(EventKind::TurnComplete, payload)
                .with_source_event_id(sid(tid, &format!("turn:{turn_id}")))
                .with_turn_id(Some(turn_id.to_string())),
        );

        // A non-null turn error is surfaced as its own fact. Not fixture-grounded
        // (every captured turn carried error:null) — handled defensively.
        if let Some(err) = turn.get("error") {
            if !err.is_null() {
                out.push(
                    self.event(EventKind::Error, err.clone())
                        .with_source_event_id(sid(tid, &format!("turn_error:{turn_id}")))
                        .with_turn_id(Some(turn_id.to_string())),
                );
            }
        }

        out
    }

    /// Map a terminal item (real `item/completed`, or a synthesized interrupt
    /// terminal) to its neutral event. `interrupted` tags the payload so the
    /// phone can render an aborted item honestly.
    fn terminal_item_event(
        &self,
        tid: &str,
        turn: Option<String>,
        item_id: &str,
        item_type: &str,
        item: &Value,
        interrupted: bool,
    ) -> Option<PendingEvent> {
        let (kind, sid_suffix, payload) = match item_type {
            "userMessage" => (
                EventKind::UserMessage,
                format!("item:{item_id}"),
                message_payload(item, interrupted),
            ),
            "agentMessage" => (
                EventKind::AgentMessage,
                format!("item:{item_id}"),
                message_payload(item, interrupted),
            ),
            // Reasoning is stored, not rendered (feature matrix). It is still
            // normalized to a fact so it round-trips; the phone drops it.
            "reasoning" => (
                EventKind::Reasoning,
                format!("item:{item_id}"),
                reasoning_payload(item, interrupted),
            ),
            "commandExecution" | "fileChange" => (
                EventKind::ToolResult,
                format!("post:{item_id}"),
                tool_result_payload(item, interrupted),
            ),
            // A future item type this build does not model: preserve it as an
            // Other fact rather than dropping it, so a newer daemon's item
            // survives a round-trip (parity with EventKind::Other's contract).
            other => (
                EventKind::Other(format!("codex_{other}")),
                format!("item:{item_id}"),
                {
                    let mut p = item.clone();
                    if interrupted {
                        if let Some(obj) = p.as_object_mut() {
                            obj.insert("interrupted".into(), Value::Bool(true));
                        }
                    }
                    p
                },
            ),
        };
        Some(
            self.event(kind, payload)
                .with_source_event_id(sid(tid, &sid_suffix))
                .with_turn_id(turn)
                .with_item_id(Some(item_id.to_string())),
        )
    }

    /// Remove one open item by (thread, id) — it completed normally. Scoped to
    /// the thread so a cross-thread frame can never close another thread's item.
    fn close_open(&mut self, thread_id: &str, turn_id: &str, item_id: &str) {
        if let Some(pos) = self
            .open
            .iter()
            .position(|o| o.thread_id == thread_id && o.turn_id == turn_id && o.item_id == item_id)
        {
            self.open.remove(pos);
        }
    }

    /// Remove and return, in arrival order, every open item belonging to one
    /// thread's turn. Keyed by (thread, turn) so another thread's items survive.
    fn drain_turn_items(&mut self, thread_id: &str, turn_id: &str) -> Vec<OpenItem> {
        let mut kept = VecDeque::new();
        let mut drained = Vec::new();
        while let Some(open) = self.open.pop_front() {
            if open.thread_id == thread_id && open.turn_id == turn_id {
                drained.push(open);
            } else {
                kept.push_back(open);
            }
        }
        self.open = kept;
        drained
    }

    /// Build a [`PendingEvent`] under this session with a Codex source.
    fn event(&self, kind: EventKind, payload: Value) -> PendingEvent {
        PendingEvent::new(&self.session, kind, payload, Source::Codex)
    }
}

impl OpenItem {
    /// The started snapshot, folded with the streamed delta text, as the basis
    /// for a synthesized interrupt terminal. A commandExecution/fileChange keeps
    /// its started fields (command, cwd, changes …); an agentMessage gets the
    /// text that streamed before the abort.
    fn synthesized_terminal(&self) -> Value {
        let mut v = self.started.clone();
        if self.item_type == "agentMessage" {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("text".into(), Value::String(self.delta_text.clone()));
            }
        }
        v
    }
}

/// The thread-namespaced source-event id (D4): `<thread_id>:<suffix>`. Threads
/// never share the separator-free prefix, so two threads' identical item ids
/// cannot alias after a switch.
fn sid(thread_id: &str, suffix: &str) -> String {
    format!("{thread_id}:{suffix}")
}

/// Payload for a commandExecution/fileChange ToolCall from its started snapshot.
fn tool_call_payload(item: &Value) -> Value {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    match item_type {
        "fileChange" => json!({
            "tool": "file_change",
            "changes": item.get("changes").cloned().unwrap_or(Value::Null),
        }),
        _ => json!({
            "tool": "command_execution",
            "command": item.get("command").cloned().unwrap_or(Value::Null),
            "cwd": item.get("cwd").cloned().unwrap_or(Value::Null),
            "command_actions": item.get("commandActions").cloned().unwrap_or(Value::Null),
        }),
    }
}

/// Payload for a commandExecution/fileChange ToolResult from its terminal
/// snapshot. `interrupted` marks a synthesized aborted terminal.
fn tool_result_payload(item: &Value, interrupted: bool) -> Value {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    let status = if interrupted {
        "interrupted".to_string()
    } else {
        item.get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_string()
    };
    match item_type {
        "fileChange" => json!({
            "tool": "file_change",
            "status": status,
            "interrupted": interrupted,
            "changes": item.get("changes").cloned().unwrap_or(Value::Null),
        }),
        _ => json!({
            "tool": "command_execution",
            "status": status,
            "interrupted": interrupted,
            "command": item.get("command").cloned().unwrap_or(Value::Null),
            "exit_code": item.get("exitCode").cloned().unwrap_or(Value::Null),
            "aggregated_output": item.get("aggregatedOutput").cloned().unwrap_or(Value::Null),
            "duration_ms": item.get("durationMs").cloned().unwrap_or(Value::Null),
        }),
    }
}

/// Payload for a user/agent message terminal: the flattened text plus the raw
/// content array (userMessage carries `content:[{type:"text",text}]`;
/// agentMessage carries a flat `text`).
fn message_payload(item: &Value, interrupted: bool) -> Value {
    let text = if let Some(t) = item.get("text").and_then(Value::as_str) {
        t.to_string()
    } else {
        // userMessage: join its text content parts.
        item.get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default()
    };
    json!({
        "text": text,
        "interrupted": interrupted,
    })
}

/// Payload for a reasoning item — stored, not rendered. Carries the summary and
/// content the app-server sent (empty in the captures) so nothing is lost.
fn reasoning_payload(item: &Value, interrupted: bool) -> Value {
    json!({
        "summary": item.get("summary").cloned().unwrap_or(Value::Null),
        "content": item.get("content").cloned().unwrap_or(Value::Null),
        "interrupted": interrupted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SessionKey {
        SessionKey::new("01K1B3XQ8ZC0DE5FGH7JKMNPQR", "cc-1")
    }

    fn frames(fixture: &str) -> Vec<Value> {
        fixture
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("fixture line is JSON"))
            .collect()
    }

    const LIFECYCLE: &str = include_str!("../../../fixtures/codex/lifecycle.jsonl");
    const COMMAND: &str = include_str!("../../../fixtures/codex/command-execution.jsonl");
    const FILE_CHANGE: &str = include_str!("../../../fixtures/codex/file-change.jsonl");
    const INTERRUPT: &str = include_str!("../../../fixtures/codex/interrupt.jsonl");

    /// Replay a whole fixture and collect the flat event stream.
    fn replay(fixture: &str) -> Vec<PendingEvent> {
        let mut a = CodexAdapter::new(key());
        frames(fixture).iter().flat_map(|f| a.ingest(f)).collect()
    }

    fn kinds(events: &[PendingEvent]) -> Vec<String> {
        events.iter().map(|e| e.kind.as_str().to_string()).collect()
    }

    fn find<'a>(events: &'a [PendingEvent], sid: &str) -> Option<&'a PendingEvent> {
        events
            .iter()
            .find(|e| e.source_event_id.as_deref() == Some(sid))
    }

    // ---- individual frame shapes, each grounded in a fixture line ----

    #[test]
    fn thread_started_maps_to_session_start() {
        // fixtures/codex/lifecycle.jsonl line 2 (thread/started).
        let f: Value = frames(LIFECYCLE)
            .into_iter()
            .find(|f| f["method"] == "thread/started")
            .unwrap();
        let mut a = CodexAdapter::new(key());
        let out = a.ingest(&f);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::SessionStart);
        assert_eq!(out[0].source, Source::Codex);
        let tid = "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86";
        assert_eq!(
            out[0].source_event_id.as_deref(),
            Some(format!("{tid}:thread_started").as_str())
        );
        assert_eq!(out[0].payload["thread_id"], tid);
        assert_eq!(out[0].payload["cwd"], "/work/proj");
    }

    #[test]
    fn user_and_agent_messages_are_facts_at_item_completed() {
        // lifecycle.jsonl: userMessage + agentMessage item/completed carry the
        // full text; the delta stream in between is not a fact.
        let events = replay(LIFECYCLE);
        let tid = "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86";

        let user = events
            .iter()
            .find(|e| e.kind == EventKind::UserMessage)
            .expect("a user message");
        assert_eq!(
            user.payload["text"],
            "reply with the word pong and nothing else"
        );
        assert_eq!(
            user.source_event_id.as_deref(),
            Some(format!("{tid}:item:01a0127a-dbdd-7d11-b925-5bb0c2dac319").as_str())
        );

        let agent = events
            .iter()
            .find(|e| e.kind == EventKind::AgentMessage)
            .expect("an agent message");
        assert_eq!(agent.payload["text"], "pong");
        // Exactly one agent message — the empty item/started text never leaks in.
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::AgentMessage)
                .count(),
            1
        );
        // The streamed delta produced no event of its own.
        assert!(!kinds(&events).iter().any(|k| k.contains("delta")));
    }

    #[test]
    fn token_usage_maps_to_usage_keyed_by_turn() {
        // lifecycle.jsonl thread/tokenUsage/updated: params.tokenUsage.total.
        let events = replay(LIFECYCLE);
        let tid = "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86";
        let turn = "01a0127a-d9cd-7461-84d7-6eea6d0b98a5";
        let usage = find(&events, &format!("{tid}:usage:{turn}")).expect("a usage event");
        assert_eq!(usage.kind, EventKind::Usage);
        assert_eq!(usage.payload["total"]["totalTokens"], 16950);
        assert_eq!(usage.turn_id.as_deref(), Some(turn));
    }

    #[test]
    fn turn_completed_maps_to_turn_complete_with_status() {
        let events = replay(LIFECYCLE);
        let tid = "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86";
        let turn = "01a0127a-d9cd-7461-84d7-6eea6d0b98a5";
        let tc = find(&events, &format!("{tid}:turn:{turn}")).expect("a turn complete");
        assert_eq!(tc.kind, EventKind::TurnComplete);
        assert_eq!(tc.payload["status"], "completed");
        assert_eq!(tc.turn_id.as_deref(), Some(turn));
    }

    #[test]
    fn command_execution_makes_a_call_and_a_result_with_distinct_ids() {
        // command-execution.jsonl: commandExecution item/started (inProgress)
        // then item/completed (status:completed, exitCode:0). Call and result
        // share item.id, so they MUST get pre:/post: ids or dedup collapses them.
        let events = replay(COMMAND);
        let tid = "01a01282-ba87-7660-9f8d-05e2219cd505";
        let item = "exec-cf7b67c7-3a19-4dd8-a9a6-6f243db33bd4";

        let call = find(&events, &format!("{tid}:pre:{item}")).expect("a tool call");
        assert_eq!(call.kind, EventKind::ToolCall);
        assert_eq!(call.payload["command"], "/bin/zsh -lc 'touch marker.txt'");
        assert_eq!(call.item_id.as_deref(), Some(item));

        let result = find(&events, &format!("{tid}:post:{item}")).expect("a tool result");
        assert_eq!(result.kind, EventKind::ToolResult);
        assert_eq!(result.payload["status"], "completed");
        assert_eq!(result.payload["exit_code"], 0);
        assert_eq!(result.payload["interrupted"], false);

        assert_ne!(call.source_event_id, result.source_event_id);
    }

    #[test]
    fn reasoning_is_normalized_but_marked_for_storage() {
        // command-execution.jsonl carries reasoning items.
        let events = replay(COMMAND);
        assert!(
            events.iter().any(|e| e.kind == EventKind::Reasoning),
            "reasoning should be normalized (stored, not rendered)"
        );
    }

    #[test]
    fn file_change_maps_to_call_and_result() {
        // file-change.jsonl: fileChange item/started + item/completed with
        // changes[].{path,kind,diff}.
        let events = replay(FILE_CHANGE);
        let call = events
            .iter()
            .find(|e| e.kind == EventKind::ToolCall && e.payload["tool"] == "file_change")
            .expect("a file-change call");
        assert!(call.payload["changes"].is_array());
        let result = events
            .iter()
            .find(|e| e.kind == EventKind::ToolResult && e.payload["tool"] == "file_change")
            .expect("a file-change result");
        assert_eq!(result.payload["status"], "completed");
    }

    // ---- interrupted-turn synthesis (D14/D15) ----

    #[test]
    fn an_interrupted_turn_synthesizes_terminals_for_dangling_items() {
        // interrupt.jsonl: the second turn starts a commandExecution
        // (exec-be9ac742…, inProgress) that never gets item/completed, then
        // turn/completed{status:"interrupted", items:[]}. The adapter must
        // synthesize the ToolResult from what it saw live.
        let events = replay(INTERRUPT);

        // Locate the synthesized result by the dangling exec item id.
        let exec = "exec-be9ac742-7660-408f-a07c-b70acb03ed6b";
        let ri = events
            .iter()
            .position(|e| e.kind == EventKind::ToolResult && e.item_id.as_deref() == Some(exec))
            .expect("a synthesized tool result for the dangling exec");
        let result = &events[ri];
        assert_eq!(result.payload["status"], "interrupted");
        assert_eq!(result.payload["interrupted"], true);

        // And the turn itself terminalized as interrupted, AFTER the item.
        let ti = events
            .iter()
            .position(|e| e.kind == EventKind::TurnComplete && e.payload["status"] == "interrupted")
            .expect("an interrupted turn complete");
        assert!(ri < ti, "item terminalizes before the turn closes");
    }

    #[test]
    fn interrupt_does_not_double_terminalize_completed_items() {
        // The interrupted turn also has a userMessage/reasoning/agentMessage that
        // DID complete before the abort. They must each appear exactly once (from
        // their real item/completed), never re-synthesized.
        let events = replay(INTERRUPT);
        // Every source_event_id is unique across the whole replay (see the dedup
        // test), which already forbids a double terminal; assert the count of
        // ToolResults equals the one dangling exec.
        let results = events
            .iter()
            .filter(|e| e.kind == EventKind::ToolResult)
            .count();
        assert_eq!(results, 1, "only the dangling exec is synthesized");
    }

    // ---- robustness ----

    #[test]
    fn unknown_and_malformed_frames_never_panic_and_are_safe() {
        let mut a = CodexAdapter::new(key());
        // A method this build has never heard of.
        assert!(a
            .ingest(&json!({"method": "future/thing", "params": {}}))
            .is_empty());
        // A response (no method) — not part of observation.
        assert!(a.ingest(&json!({"id": 7, "result": {}})).is_empty());
        // Structurally broken frames.
        assert!(a.ingest(&json!({})).is_empty());
        assert!(a.ingest(&json!(42)).is_empty());
        assert!(a.ingest(&json!({"method": "item/started"})).is_empty()); // no params
        assert!(a
            .ingest(&json!({"method": "item/completed", "params": {"item": {}}}))
            .is_empty()); // item without id/type
    }

    #[test]
    fn an_unknown_item_type_is_preserved_as_an_other_fact() {
        // A future item type must survive as an Other event, not vanish.
        let mut a = CodexAdapter::new(key());
        let completed = json!({
            "method": "item/completed",
            "params": {
                "item": {"type": "webSearch", "id": "ws-1"},
                "threadId": "th_X",
                "turnId": "turn_1"
            }
        });
        let out = a.ingest(&completed);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::Other("codex_webSearch".into()));
        assert_eq!(out[0].source_event_id.as_deref(), Some("th_X:item:ws-1"));
    }

    // ---- fail-closed on missing/empty required identities ----

    #[test]
    fn a_frame_missing_or_empty_thread_id_is_dropped_and_mutates_nothing() {
        let mut a = CodexAdapter::new(key());
        // item/started with an EMPTY threadId — must not open an item (no
        // non-thread-namespaced state), must emit nothing.
        let started = |tid: Value| {
            json!({
                "method": "item/started",
                "params": {
                    "item": {"type": "commandExecution", "id": "exec-1", "status": "inProgress"},
                    "threadId": tid, "turnId": "turn_1"
                }
            })
        };
        assert!(a.ingest(&started(json!(""))).is_empty());
        assert!(a.ingest(&started(json!(null))).is_empty());
        // A turnId missing on item/started is equally fatal (undrainable later).
        assert!(a
            .ingest(&json!({
                "method": "item/started",
                "params": {
                    "item": {"type": "commandExecution", "id": "exec-1", "status": "inProgress"},
                    "threadId": "th_A"
                }
            }))
            .is_empty());
        // Now a WELL-FORMED interrupted turn for th_A finds no dangling item —
        // proving none of the malformed starts mutated state.
        let interrupted = a.ingest(&json!({
            "method": "turn/completed",
            "params": {
                "threadId": "th_A",
                "turn": {"id": "turn_1", "status": "interrupted", "items": []}
            }
        }));
        assert_eq!(kinds(&interrupted), vec!["turn_complete"]);
    }

    #[test]
    fn a_cross_thread_frame_never_touches_another_threads_state() {
        let mut a = CodexAdapter::new(key());
        // Thread A opens an exec and records usage.
        a.ingest(&json!({
            "method": "item/started",
            "params": {
                "item": {"type": "commandExecution", "id": "exec-A", "status": "inProgress"},
                "threadId": "th_A", "turnId": "turn_A"
            }
        }));
        a.ingest(&json!({
            "method": "thread/tokenUsage/updated",
            "params": {"threadId": "th_A", "turnId": "turn_A", "tokenUsage": {"total": {"totalTokens": 1}}}
        }));
        // Thread B, reusing the SAME item id and turn id, completes and closes.
        // It must NOT close A's open item nor consume A's usage.
        a.ingest(&json!({
            "method": "item/completed",
            "params": {
                "item": {"type": "commandExecution", "id": "exec-A", "status": "completed"},
                "threadId": "th_B", "turnId": "turn_A"
            }
        }));
        a.ingest(&json!({
            "method": "turn/completed",
            "params": {"threadId": "th_B", "turn": {"id": "turn_A", "status": "completed", "items": []}}
        }));
        // Now interrupt A's turn: its exec is still open (B never touched it), so
        // it synthesizes; and A's usage is still present, so it is emitted.
        let out = a.ingest(&json!({
            "method": "turn/completed",
            "params": {"threadId": "th_A", "turn": {"id": "turn_A", "status": "interrupted", "items": []}}
        }));
        assert!(
            out.iter()
                .any(|e| e.kind == EventKind::ToolResult && e.item_id.as_deref() == Some("exec-A")),
            "A's open item survived B's same-id frame"
        );
        assert!(
            out.iter().any(|e| e.kind == EventKind::Usage),
            "A's usage was not consumed by B"
        );
    }

    #[test]
    fn a_delta_missing_or_wrong_turn_never_mutates_an_open_items_text() {
        let mut a = CodexAdapter::new(key());
        a.ingest(&json!({
            "method": "item/started",
            "params": {
                "item": {"type": "agentMessage", "id": "msg-1", "status": "inProgress"},
                "threadId": "th", "turnId": "turn_1"
            }
        }));
        // A delta with no turnId is dropped; a delta naming the wrong turn misses
        // the (thread, turn, item) key. Neither may accumulate into msg-1.
        a.ingest(&json!({
            "method": "item/agentMessage/delta",
            "params": {"threadId": "th", "itemId": "msg-1", "delta": "NO_TURN"}
        }));
        a.ingest(&json!({
            "method": "item/agentMessage/delta",
            "params": {"threadId": "th", "turnId": "turn_2", "itemId": "msg-1", "delta": "WRONG_TURN"}
        }));
        // The correct-turn delta does accumulate.
        a.ingest(&json!({
            "method": "item/agentMessage/delta",
            "params": {"threadId": "th", "turnId": "turn_1", "itemId": "msg-1", "delta": "kept"}
        }));
        // Interrupt so the open message is synthesized from its accumulated text.
        let out = a.ingest(&json!({
            "method": "turn/completed",
            "params": {"threadId": "th", "turn": {"id": "turn_1", "status": "interrupted", "items": []}}
        }));
        let synthesized = out
            .iter()
            .find(|e| e.item_id.as_deref() == Some("msg-1"))
            .expect("the interrupted message is synthesized");
        let text = serde_json::to_string(&synthesized.payload).unwrap();
        assert!(text.contains("kept"), "the correct-turn delta was kept");
        assert!(
            !text.contains("NO_TURN") && !text.contains("WRONG_TURN"),
            "a turnless or wrong-turn delta must not mutate the item's text"
        );
    }

    // ---- fail-closed on missing required status (never fabricate success) ----

    #[test]
    fn a_turn_completed_missing_status_is_dropped_without_draining() {
        let mut a = CodexAdapter::new(key());
        a.ingest(&json!({
            "method": "item/started",
            "params": {
                "item": {"type": "commandExecution", "id": "exec-1", "status": "inProgress"},
                "threadId": "th_A", "turnId": "turn_1"
            }
        }));
        // A malformed terminal with NO status must not fabricate a completed turn
        // and must not drain the open exec.
        let bad = a.ingest(&json!({
            "method": "turn/completed",
            "params": {"threadId": "th_A", "turn": {"id": "turn_1", "items": []}}
        }));
        assert!(bad.is_empty(), "no fabricated TurnComplete{{completed}}");
        // The real interrupted terminal still arrives and still synthesizes.
        let good = a.ingest(&json!({
            "method": "turn/completed",
            "params": {"threadId": "th_A", "turn": {"id": "turn_1", "status": "interrupted", "items": []}}
        }));
        assert!(
            good.iter().any(|e| e.kind == EventKind::ToolResult
                && e.item_id.as_deref() == Some("exec-1")
                && e.payload["status"] == "interrupted"),
            "the still-open exec synthesizes once the real terminal lands"
        );
    }

    #[test]
    fn a_tool_item_completed_missing_status_is_dropped_without_closing() {
        let mut a = CodexAdapter::new(key());
        a.ingest(&json!({
            "method": "item/started",
            "params": {
                "item": {"type": "commandExecution", "id": "exec-1", "status": "inProgress"},
                "threadId": "th_A", "turnId": "turn_1"
            }
        }));
        // A commandExecution item/completed with NO status is malformed: no
        // ToolResult, and the open item is NOT closed.
        let bad = a.ingest(&json!({
            "method": "item/completed",
            "params": {
                "item": {"type": "commandExecution", "id": "exec-1"},
                "threadId": "th_A", "turnId": "turn_1"
            }
        }));
        assert!(bad.is_empty(), "no fabricated completed ToolResult");
        // A later well-formed terminal resolves it exactly once.
        let good = a.ingest(&json!({
            "method": "item/completed",
            "params": {
                "item": {"type": "commandExecution", "id": "exec-1", "status": "completed", "exitCode": 0},
                "threadId": "th_A", "turnId": "turn_1"
            }
        }));
        assert_eq!(good.len(), 1);
        assert_eq!(good[0].kind, EventKind::ToolResult);
        assert_eq!(good[0].payload["status"], "completed");
    }

    // ---- the whole-lifecycle replay + dedup discipline ----

    #[test]
    fn a_full_lifecycle_replays_to_an_ordered_sensible_timeline() {
        let events = replay(LIFECYCLE);
        let ks = kinds(&events);
        // The neutral timeline for one message turn: the session starts, the
        // user's prompt, the agent's reply, the token usage, then the turn close.
        assert_eq!(
            ks,
            vec![
                "session_start",
                "user_message",
                "agent_message",
                "usage",
                "turn_complete",
            ]
        );
    }

    #[test]
    fn dedup_key_is_gap_free_and_duplicate_free_over_every_replay() {
        for fixture in [LIFECYCLE, COMMAND, FILE_CHANGE, INTERRUPT] {
            let events = replay(fixture);
            // Every event has a source_event_id (the dedup key's third term).
            assert!(
                events.iter().all(|e| e.source_event_id.is_some()),
                "every Codex event must carry a source_event_id"
            );
            // No two facts share one (source, source_event_id) — a duplicate would
            // silently drop one; a collision (call vs result) would fuse two.
            let mut seen = std::collections::HashSet::new();
            for e in &events {
                let dedup = (e.source.as_str(), e.source_event_id.clone().unwrap());
                assert!(
                    seen.insert(dedup.clone()),
                    "duplicate dedup key {dedup:?} in replay"
                );
            }
        }
    }

    #[test]
    fn replaying_a_frame_twice_yields_the_same_ids() {
        // Idempotency: a thread/started re-broadcast on reconnect (D2) must map to
        // the same dedup key, so the store collapses it to one row.
        let f: Value = frames(LIFECYCLE)
            .into_iter()
            .find(|f| f["method"] == "thread/started")
            .unwrap();
        let mut a = CodexAdapter::new(key());
        let first = a.ingest(&f);
        let second = a.ingest(&f);
        assert_eq!(first[0].source_event_id, second[0].source_event_id);
    }
}
