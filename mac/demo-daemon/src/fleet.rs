//! The fictional fleet: five runs, their event logs, and the ledgers that make
//! an answer and a takeover exactly-once.
//!
//! Everything is in memory and nothing is ever written to a disk. The engine
//! that drives it is the second half of this file: one state machine per run,
//! stepped from a timer, playing the script next door.
//!
//! Two properties are load-bearing, and both are `ccd`'s:
//!
//!   * **`seq` is per-run, monotonic and gap-free.** A log is appended to and
//!     never rewritten, so a phone that reconnects with `after_seq` sees every
//!     fact once, in order. When a run's log reaches its ceiling the run is
//!     *retired* and a new one takes its slot with a new uid and a new log
//!     starting at 1 — never truncated, because a hole a client cannot see is
//!     the failure that makes an event log worthless.
//!   * **The append and the broadcast happen together.** Both are under the one
//!     lock, so an event is in the log before any connection is told about it,
//!     and a replay reading the log therefore cannot miss what a broadcast
//!     already carried.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use protocol::event::{Event, EventKind, Lifecycle, Link, SessionSummary, Source};
use protocol::ws::{
    AnswerDecision, AnswerOutcome, AnswerPath, AnswerResult, ApprovalCard, ResolvedBy,
    SendTextResult,
};
use serde_json::json;
use tokio::sync::broadcast;

use crate::script::{self, Script};

/// The most events one run's log may hold before the script retires it.
///
/// The number is the iOS client's own initial backfill window
/// (`AppModel.initialBackfill`, 400): a phone subscribing to a run this size
/// with `after_seq = 0` receives the whole of it, so no run here is ever shown
/// with a truncated head and the "N events not shown" banner cannot appear over
/// a demo. The `session_end` that retires a run is the +1 that fits inside it.
const MAX_EVENTS_PER_RUN: usize = 399;

/// How long a run rests after finishing an approval's aftermath before the next
/// card is raised. The aftermath itself is a few seconds, so a reviewer who taps
/// sees the next decision arrive about a minute later — long enough to watch the
/// turn finish, short enough that the next reviewer session finds one waiting.
const RESPAWN_AFTER: Duration = Duration::from_secs(55);

/// Ring depth for live delivery. A connection that falls further behind than
/// this is resynced rather than skipped, exactly as `ccd` does; the depth is
/// generous because the alternative to a resync is a slower reader paying for a
/// faster one.
const BROADCAST_CAPACITY: usize = 1024;

/// What a run's answer is applied *as*. `ccd` reports `send_keys` whenever it
/// holds no hook open, which is this server's shape too: nothing is held, and an
/// answer settles the moment it arrives.
const APPLIED_VIA: AnswerPath = AnswerPath::SendKeys;

/// The needle a real send matched at the Mac's prompt. There is no prompt here,
/// and this is the string the daemon reports for the ordinary case.
const MATCHED_NEEDLE: &str = "foragents";

pub struct Fleet {
    script: Script,
    inner: Mutex<Inner>,
    events_tx: broadcast::Sender<Event>,
}

struct Inner {
    runs: Vec<Run>,
}

/// One agent run. Its identity is `uid`; `name` is a label the next run in the
/// same slot inherits.
struct Run {
    /// Which entry of the script this run is playing.
    slot: usize,
    uid: String,
    name: String,
    cwd: String,
    lifecycle: Lifecycle,
    created_at: String,
    updated_at: String,
    log: Vec<Event>,
    /// Cards raised and not yet answered. Removing a card on resolution is what
    /// makes `identity_bound` true: a card is answerable exactly while it is the
    /// prompt this run is holding.
    cards: Vec<ApprovalCard>,
    /// request_id -> (the hash that was served, the outcome that was recorded).
    answers: HashMap<String, (String, AnswerOutcome)>,
    /// request_id -> what a retried `send_text` replays.
    texts: HashMap<String, TextRecord>,
    stage: Stage,
    /// When the next scripted step is due. `None` means the script is waiting on
    /// a human, or has nothing left to say.
    due: Option<Instant>,
    /// How many cards this run has raised, which is also the index into the
    /// script's approvals and the suffix that makes each respawn a new request.
    cycle: usize,
}

struct TextRecord {
    payload_hash: String,
    matched: String,
    applied_at: String,
}

enum Stage {
    Opening {
        index: usize,
    },
    /// A card is up. Nothing is scheduled: the script is waiting for a human.
    Blocked,
    /// The approved command is running; its result is due at `Run::due`. Only
    /// the request id is carried: everything else the result reports is the
    /// script's own approval, which is where the card got it, so holding a
    /// second copy here would be a second thing to keep in step.
    Running {
        request_id: String,
    },
    Aftermath {
        allowed: bool,
        index: usize,
    },
    /// Resting before the next card.
    Respawn,
    Progress {
        index: usize,
    },
    /// The script has nothing more to say about this run.
    Idle,
}

impl Fleet {
    pub fn new(now: Instant) -> anyhow::Result<Fleet> {
        let script = script::load()?;
        let mut runs = Vec::with_capacity(script.runs.len());
        for (slot, scripted) in script.runs.iter().enumerate() {
            runs.push(Run::start(slot, scripted, now)?);
        }
        let (events_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Fleet {
            script,
            inner: Mutex::new(Inner { runs }),
            events_tx,
        })
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.events_tx.subscribe()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic inside the lock would leave the fleet as it stood, which is
        // still a fleet: refusing to serve it afterwards would turn one bad
        // event into a dead demo.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ------------------------------------------------------------- read paths

    pub fn sessions(&self) -> Vec<SessionSummary> {
        let inner = self.lock();
        let mut summaries: Vec<SessionSummary> = inner.runs.iter().map(Run::summary).collect();
        // Deterministic and stable: a run keeps its place in the list across
        // refreshes, and a retired run sorts ahead of the one that replaced it
        // because a uid sorts by the millisecond it was minted.
        summaries.sort_by(|a, b| a.session_uid.cmp(&b.session_uid));
        summaries
    }

    /// A `session_id` on the wire is a *reference*: a uid, or a name that
    /// resolves to the newest run carrying it.
    pub fn resolve(&self, reference: &str) -> Option<String> {
        let inner = self.lock();
        if protocol::uid::is_well_formed(reference)
            && inner.runs.iter().any(|run| run.uid == reference)
        {
            return Some(reference.to_string());
        }
        inner
            .runs
            .iter()
            .filter(|run| run.name == reference)
            .max_by(|a, b| a.uid.cmp(&b.uid))
            .map(|run| run.uid.clone())
    }

    pub fn name_of(&self, uid: &str) -> String {
        self.lock()
            .runs
            .iter()
            .find(|run| run.uid == uid)
            .map(|run| run.name.clone())
            .unwrap_or_default()
    }

    /// One page of a run's log, above `after_seq`.
    pub fn events_after(&self, uid: &str, after_seq: u64, limit: usize) -> Vec<Event> {
        let inner = self.lock();
        let Some(run) = inner.runs.iter().find(|run| run.uid == uid) else {
            return Vec::new();
        };
        run.log
            .iter()
            .filter(|event| event.seq > after_seq)
            .take(limit)
            .cloned()
            .collect()
    }

    // ---------------------------------------------------------- write paths

    /// Apply an answer, or say exactly why not.
    ///
    /// The order of the checks is `ccd`'s and is not arbitrary: the ledger is
    /// consulted **before** the card, so a retried tap replays its original
    /// outcome even when the hash it carries has since gone stale. Reversing the
    /// two would answer a settled request with a refusal and invite the phone to
    /// try again.
    ///
    /// `now` is a parameter for the same reason [`Fleet::tick`]'s is: an answer
    /// schedules the rest of its turn, so a test that cannot say when it arrived
    /// cannot say what should follow it.
    pub fn answer(
        &self,
        request_id: &str,
        payload_hash: &str,
        decision: AnswerDecision,
        session_ref: Option<&str>,
        now: Instant,
    ) -> AnswerResult {
        let uid = match session_ref {
            Some(reference) => match self.resolve(reference) {
                Some(uid) => uid,
                None => {
                    return AnswerResult::Rejected {
                        reason: format!("unknown session {reference}"),
                    }
                }
            },
            // A client below minor 2 names no run. Find the one holding this
            // request; a uid is never reused, so at most one ever does.
            None => match self.run_holding(request_id) {
                Some(uid) => uid,
                None => {
                    return AnswerResult::Rejected {
                        reason: "unknown or already-resolved request".into(),
                    }
                }
            },
        };

        let mut inner = self.lock();
        let Some(run) = inner.runs.iter_mut().find(|run| run.uid == uid) else {
            return AnswerResult::Rejected {
                reason: format!("unknown session {uid}"),
            };
        };

        if let Some((stored_hash, outcome)) = run.answers.get(request_id) {
            return AnswerResult::Duplicate {
                outcome: outcome.clone(),
                stale_payload_hash: stored_hash != payload_hash,
            };
        }
        let Some(position) = run
            .cards
            .iter()
            .position(|card| card.request_id == request_id)
        else {
            return AnswerResult::Rejected {
                reason: "unknown or already-resolved request".into(),
            };
        };
        if run.cards[position].payload_hash != payload_hash {
            return AnswerResult::Rejected {
                reason: "stale payload_hash: the card you answered is out of date".into(),
            };
        }

        let card = run.cards.remove(position);
        let outcome = AnswerOutcome {
            request_id: request_id.to_string(),
            // The run's *name*, as `ccd` records it.
            session_id: run.name.clone(),
            decision: decision.clone(),
            resolved_by: ResolvedBy::Phone,
            applied_via: APPLIED_VIA,
            resolved_at: protocol::time::now_rfc3339(),
            detail: Some("answered from the phone".into()),
            // Neither is true here and both mean something specific, so both stay
            // false: the decision is the one that arrived, and it was applied.
            inferred: false,
            indeterminate: false,
        };
        run.answers.insert(
            request_id.to_string(),
            (card.payload_hash.clone(), outcome.clone()),
        );
        let resolved = run.append(
            EventKind::ApprovalResolved,
            serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null),
            Source::Daemon,
            Some(format!("resolved:{request_id}")),
        );
        let _ = self.events_tx.send(resolved);

        // A deny runs nothing, so there is no tool call to show. Anything else is
        // treated as permission to proceed — the demo's cards offer allow and
        // deny, and an option or a free-text takeover both mean the human chose
        // to let the turn continue.
        if matches!(decision, AnswerDecision::Deny) {
            let scripted = &self.script.runs[run.slot];
            run.stage = Stage::Aftermath {
                allowed: false,
                index: 0,
            };
            run.due = Some(now + Duration::from_millis(scripted.denied[0].after_ms));
        } else {
            // `PreToolUse` fires the instant permission returns, so the tool call
            // is not scheduled — it *is* the answer landing.
            let payload = json!({
                "hook_event_name": "PreToolUse",
                "tool_use_id": &card.request_id,
                "tool_name": &card.tool_name,
                "tool_input": &card.tool_input,
                "cwd": &run.cwd,
            });
            let source_event_id = format!("pre:{}", card.request_id);
            let call = run.append(
                EventKind::ToolCall,
                payload,
                Source::Hook,
                Some(source_event_id),
            );
            let _ = self.events_tx.send(call);
            let approvals = &self.script.runs[run.slot].approvals;
            run.due = Some(
                now + Duration::from_millis(approvals[run.cycle % approvals.len()].duration_ms),
            );
            run.stage = Stage::Running {
                request_id: card.request_id,
            };
        }
        AnswerResult::Applied { outcome }
    }

    /// Type into a run, or say exactly why not.
    ///
    /// The identity rules are `ccd`'s: a `request_id` without a hash is refused
    /// because the id alone says "this is a retry" without saying a retry of
    /// what, and a hash that does not match the text, the target and the submit
    /// flag is refused because a ledger entry that could be reused for different
    /// text is not an identity.
    pub fn send_text(
        &self,
        session_ref: &str,
        text: String,
        request_id: Option<&str>,
        payload_hash: Option<&str>,
        submit: bool,
    ) -> SendTextResult {
        if text.len() > protocol::ws::MAX_SEND_TEXT_BYTES {
            return SendTextResult::Refused {
                reason: format!(
                    "text is {} bytes; the ceiling is {}",
                    text.len(),
                    protocol::ws::MAX_SEND_TEXT_BYTES
                ),
            };
        }
        let identity = match (request_id, payload_hash) {
            (Some(request_id), Some(given)) => {
                let expected = protocol::hash::send_text_hash(session_ref, &text, submit);
                if given != expected {
                    return SendTextResult::Refused {
                        reason: "payload_hash does not match the text, session and submit flag \
                                 in this request"
                            .into(),
                    };
                }
                Some((request_id.to_string(), expected))
            }
            (Some(_), None) => {
                return SendTextResult::Refused {
                    reason: "request_id without payload_hash: an idempotent mutation has to say \
                             what it is a retry of"
                        .into(),
                }
            }
            (None, _) => None,
        };
        let Some(uid) = self.resolve(session_ref) else {
            return SendTextResult::Refused {
                reason: format!("unknown session {session_ref}"),
            };
        };

        let mut inner = self.lock();
        let Some(run) = inner.runs.iter_mut().find(|run| run.uid == uid) else {
            return SendTextResult::Refused {
                reason: format!("unknown session {uid}"),
            };
        };
        if run.lifecycle != Lifecycle::Live {
            return SendTextResult::Refused {
                reason: "that run has ended; there is nothing left to type into".into(),
            };
        }
        if let Some((request_id, hash)) = &identity {
            if let Some(record) = run.texts.get(request_id) {
                if &record.payload_hash == hash {
                    return SendTextResult::Duplicate {
                        matched: record.matched.clone(),
                        applied_at: record.applied_at.clone(),
                    };
                }
                return SendTextResult::Refused {
                    reason: "this request_id was already used for different text in this \
                             session; use a new one"
                        .into(),
                };
            }
        }

        let applied_at = protocol::time::now_rfc3339();
        if let Some((request_id, hash)) = identity {
            run.texts.insert(
                request_id,
                TextRecord {
                    payload_hash: hash,
                    matched: MATCHED_NEEDLE.to_string(),
                    applied_at: applied_at.clone(),
                },
            );
        }
        // The transcript's record of what was typed, then the agent's reply to
        // it. Both immediately: nothing is queued behind a screen here, so a
        // delay would be a pause this server invented.
        let typed = run.append(
            EventKind::UserMessage,
            json!({ "type": "user", "message": { "role": "user", "content": text } }),
            Source::Transcript,
            None,
        );
        let _ = self.events_tx.send(typed);
        let acknowledged = run.append(
            EventKind::AgentMessage,
            json!({
                "message": {
                    "content": [{
                        "type": "text",
                        "text": "Got it — picking that up now. This fleet is a scripted demonstration, \
                                 so nothing is run on a real machine.",
                    }],
                }
            }),
            Source::Transcript,
            None,
        );
        let _ = self.events_tx.send(acknowledged);

        SendTextResult::Sent {
            matched: MATCHED_NEEDLE.to_string(),
        }
    }

    fn run_holding(&self, request_id: &str) -> Option<String> {
        self.lock()
            .runs
            .iter()
            .find(|run| {
                run.answers.contains_key(request_id)
                    || run.cards.iter().any(|card| card.request_id == request_id)
            })
            .map(|run| run.uid.clone())
    }

    // ------------------------------------------------------------ the engine

    /// Advance every run whose next step is due.
    ///
    /// `now` is a parameter rather than read here so a test can drive the whole
    /// fleet through hours of script in a few microseconds.
    pub fn tick(&self, now: Instant) {
        let mut inner = self.lock();
        let live: Vec<usize> = (0..inner.runs.len())
            .filter(|&index| inner.runs[index].lifecycle == Lifecycle::Live)
            .collect();
        let mut exhausted = Vec::new();
        for index in live {
            if inner.runs[index].log.len() >= MAX_EVENTS_PER_RUN {
                exhausted.push(index);
                continue;
            }
            if inner.runs[index].due.is_some_and(|due| due <= now) {
                let scripted = &self.script.runs[inner.runs[index].slot];
                for event in inner.runs[index].advance(scripted, now) {
                    let _ = self.events_tx.send(event);
                }
            }
        }
        for index in exhausted {
            self.retire(&mut inner, index, now);
        }
    }

    /// End a run whose log is full and start its successor.
    ///
    /// A fresh uid and a log from seq 1, which is exactly what a phone sees when
    /// a real run exits and `codeconnect claude` takes the freed name: two runs,
    /// told apart by their uids, never one timeline with a seam in it.
    fn retire(&self, inner: &mut Inner, index: usize, now: Instant) {
        let slot = inner.runs[index].slot;
        let run = &mut inner.runs[index];
        let source_event_id = format!("end:{}", run.uid);
        let ended = run.append(
            EventKind::SessionEnd,
            json!({
                "exit_code": 0,
                "reason": "this demonstration run reached the end of its log and was restarted",
            }),
            Source::Daemon,
            Some(source_event_id),
        );
        let _ = self.events_tx.send(ended);
        run.lifecycle = Lifecycle::Exited;
        run.cards.clear();
        run.stage = Stage::Idle;
        run.due = None;
        run.updated_at = protocol::time::now_rfc3339();

        // At most two runs per slot: the one that just ended, and the one about
        // to start. Any earlier corpse goes now, which is what bounds this
        // server's memory over a deployment measured in weeks.
        let keep = inner.runs[index].uid.clone();
        inner
            .runs
            .retain(|run| run.slot != slot || run.lifecycle == Lifecycle::Live || run.uid == keep);
        match Run::start(slot, &self.script.runs[slot], now) {
            Ok(fresh) => inner.runs.push(fresh),
            // The kernel refused entropy, so no uid can be minted and no run can
            // be identified. The fleet keeps serving the runs it has.
            Err(err) => eprintln!("demo-daemon: could not start a run in slot {slot}: {err}"),
        }
    }
}

impl Run {
    fn start(slot: usize, scripted: &script::Run, now: Instant) -> anyhow::Result<Run> {
        let stamp = protocol::time::now_rfc3339();
        Ok(Run {
            slot,
            uid: protocol::uid::new()?,
            name: scripted.name.clone(),
            cwd: scripted.cwd.clone(),
            lifecycle: Lifecycle::Live,
            created_at: stamp.clone(),
            updated_at: stamp,
            log: Vec::new(),
            cards: Vec::new(),
            answers: HashMap::new(),
            texts: HashMap::new(),
            stage: Stage::Opening { index: 0 },
            due: Some(now + Duration::from_millis(scripted.opening[0].after_ms)),
            cycle: 0,
        })
    }

    fn summary(&self) -> SessionSummary {
        SessionSummary {
            session_uid: self.uid.clone(),
            session_id: self.name.clone(),
            tmux_session: self.name.clone(),
            cwd: self.cwd.clone(),
            lifecycle: self.lifecycle,
            link: match self.lifecycle {
                Lifecycle::Live => Link::Attached,
                _ => Link::Detached,
            },
            claude_session_id: None,
            transcript_path: None,
            last_seq: self.log.last().map(|event| event.seq).unwrap_or(0),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            blocked_on: self
                .cards
                .iter()
                .map(|card| card.request_id.clone())
                .collect(),
            project_label: script::project_label(&self.cwd).to_string(),
        }
    }

    /// Append one fact. The only place a `seq` is assigned, and the only place
    /// the log grows.
    fn append(
        &mut self,
        kind: EventKind,
        payload: serde_json::Value,
        source: Source,
        source_event_id: Option<String>,
    ) -> Event {
        let seq = self.log.last().map(|event| event.seq).unwrap_or(0) + 1;
        let event = Event {
            seq,
            session_uid: self.uid.clone(),
            session_id: self.name.clone(),
            ts: protocol::time::now_rfc3339(),
            kind,
            payload,
            source,
            source_event_id,
            turn_id: None,
            item_id: None,
        };
        self.updated_at = event.ts.clone();
        self.log.push(event.clone());
        event
    }

    /// Do exactly one step of the script, and say what it produced.
    ///
    /// One step per call, never a burst: the fleet is stepped from a timer, and
    /// a step that played a whole stage would collapse the pacing the script is
    /// written in. The only call that emits two events is the one where a card
    /// follows the opening line that led to it.
    fn advance(&mut self, scripted: &script::Run, now: Instant) -> Vec<Event> {
        let mut emitted = Vec::new();
        match std::mem::replace(&mut self.stage, Stage::Idle) {
            Stage::Opening { index } => {
                emitted.push(self.play(&scripted.opening[index]));
                let next = index + 1;
                if next < scripted.opening.len() {
                    self.stage = Stage::Opening { index: next };
                    self.schedule(now, scripted.opening[next].after_ms);
                } else if !scripted.approvals.is_empty() {
                    emitted.push(self.raise_card(scripted));
                } else if !scripted.progress.is_empty() {
                    self.stage = Stage::Progress { index: 0 };
                    self.schedule(now, scripted.progress[0].after_ms);
                } else {
                    // The run is finished. Left `Idle` with nothing due, so it
                    // costs no further tick for the life of the process.
                    self.due = None;
                }
            }
            Stage::Running { request_id } => {
                let approval = &scripted.approvals[self.cycle % scripted.approvals.len()];
                let payload = json!({
                    "hook_event_name": "PostToolUse",
                    "tool_use_id": &request_id,
                    "tool_name": &approval.tool_name,
                    "tool_input": &approval.tool_input,
                    "tool_response": &approval.tool_response,
                    "duration_ms": approval.duration_ms,
                });
                let source_event_id = format!("post:{request_id}");
                emitted.push(self.append(
                    EventKind::ToolResult,
                    payload,
                    Source::Hook,
                    Some(source_event_id),
                ));
                self.stage = Stage::Aftermath {
                    allowed: true,
                    index: 0,
                };
                self.schedule(now, scripted.allowed[0].after_ms);
            }
            Stage::Aftermath { allowed, index } => {
                let steps = if allowed {
                    &scripted.allowed
                } else {
                    &scripted.denied
                };
                emitted.push(self.play(&steps[index]));
                let next = index + 1;
                if next < steps.len() {
                    self.stage = Stage::Aftermath {
                        allowed,
                        index: next,
                    };
                    self.schedule(now, steps[next].after_ms);
                } else {
                    self.stage = Stage::Respawn;
                    self.due = Some(now + RESPAWN_AFTER);
                }
            }
            Stage::Respawn => {
                self.cycle += 1;
                emitted.push(self.raise_card(scripted));
            }
            Stage::Progress { index } => {
                emitted.push(self.play(&scripted.progress[index]));
                let next = (index + 1) % scripted.progress.len();
                self.stage = Stage::Progress { index: next };
                self.schedule(now, scripted.progress[next].after_ms);
                // One pass of the loop is one cycle, so the tool ids `play`
                // scopes are distinct between passes.
                if next == 0 {
                    self.cycle += 1;
                }
            }
            // Neither schedules anything, so neither is reachable from a step
            // that was due. Clearing the deadline keeps that true rather than
            // leaving a stale one to fire on every tick.
            Stage::Blocked => {
                self.stage = Stage::Blocked;
                self.due = None;
            }
            Stage::Idle => self.due = None,
        }
        emitted
    }

    /// Emit one scripted step verbatim, except for a looping tool id.
    fn play(&mut self, step: &script::Step) -> Event {
        let mut payload = step.payload.clone();
        // A looping script would otherwise hand the same `tool_use_id` to every
        // pass of the loop, and a client that joins a result to its call by that
        // id would join the wrong pair. The cycle makes each pass its own.
        if let Some(id) = payload.get("tool_use_id").and_then(|id| id.as_str()) {
            let scoped = format!("{id}_{}", self.cycle);
            payload["tool_use_id"] = json!(scoped);
        }
        self.append(step.kind(), payload, step.source(), None)
    }

    /// Raise the cycle's card, with its hash and its risk class computed rather
    /// than authored.
    fn raise_card(&mut self, scripted: &script::Run) -> Event {
        let approval = &scripted.approvals[self.cycle % scripted.approvals.len()];
        let request_id = format!("{}_{}", approval.request_id, self.cycle);
        let card = ApprovalCard {
            request_id: request_id.clone(),
            payload_hash: protocol::hash::approval_payload_hash(
                &approval.tool_name,
                &approval.tool_input,
            ),
            tool_name: approval.tool_name.clone(),
            tool_input: approval.tool_input.clone(),
            display_text: protocol::hash::approval_payload_text(
                &approval.tool_name,
                &approval.tool_input,
            ),
            permission_suggestions: None,
            prompt_id: None,
            permission_mode: Some("default".into()),
            risk: Some(protocol::risk::classify(
                &approval.tool_name,
                &approval.tool_input,
            )),
            // One per card raised, which is `ccd`'s rule read off the log.
            generation: self
                .log
                .iter()
                .filter(|event| event.kind == EventKind::ApprovalRequest)
                .count() as u64
                + 1,
            // True, and provable: a card is answerable exactly while it is in
            // `cards`, and resolving it takes it out. There is no screen here for
            // the prompt to leave without this server noticing.
            identity_bound: true,
        };
        // The same two halves `ccd` files: the card the phone renders and
        // answers, and the hook that raised it.
        let payload = json!({
            "card": &card,
            "hook": {
                "hook_event_name": "PermissionRequest",
                "tool_name": &approval.tool_name,
                "tool_input": &approval.tool_input,
                "permission_mode": "default",
                "cwd": &self.cwd,
            },
        });
        let event = self.append(
            EventKind::ApprovalRequest,
            payload,
            Source::Hook,
            Some(format!("perm:{request_id}")),
        );
        self.cards.push(card);
        self.stage = Stage::Blocked;
        self.due = None;
        event
    }

    fn schedule(&mut self, now: Instant, after_ms: u64) {
        self.due = Some(now + Duration::from_millis(after_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock the fleet is driven by rather than one it reads.
    ///
    /// Each step jumps further than any delay the script can schedule, so one
    /// `tick` is exactly one step of every run that has one — the whole of a
    /// demo's hour reachable in microseconds, and reachable the same way twice.
    struct Clock(Instant);

    impl Clock {
        fn new() -> Clock {
            Clock(Instant::now())
        }

        fn step(&mut self) -> Instant {
            self.0 += RESPAWN_AFTER * 2;
            self.0
        }
    }

    fn pump(fleet: &Fleet, clock: &mut Clock, steps: usize) {
        for _ in 0..steps {
            fleet.tick(clock.step());
        }
    }

    /// The uid of the run playing script slot `slot`, live one preferred —
    /// which `resolve` gives us, since a uid sorts by the millisecond it was
    /// minted and a successor is always the newer of the two.
    fn uid_of(fleet: &Fleet, name: &str) -> String {
        fleet.resolve(name).expect("the fleet must hold that run")
    }

    /// The card a run is holding, read the way the phone reads it: out of the
    /// `approval_request` event's payload. A test that reached into the server's
    /// own structs could pass while the frame on the wire was wrong.
    fn card_on_the_wire(fleet: &Fleet, uid: &str) -> ApprovalCard {
        let event = fleet
            .events_after(uid, 0, 1000)
            .into_iter()
            .rfind(|event| event.kind == EventKind::ApprovalRequest)
            .expect("that run has raised no card");
        serde_json::from_value(event.payload["card"].clone()).expect("the card must decode")
    }

    /// A fleet pumped until its blocked runs are holding their first card.
    fn blocked_fleet() -> (Fleet, Clock) {
        let mut clock = Clock::new();
        let fleet = Fleet::new(clock.0).expect("the shipped script must load");
        pump(&fleet, &mut clock, 4);
        (fleet, clock)
    }

    #[test]
    fn the_opening_of_every_run_is_a_gap_free_log_starting_at_one() {
        let (fleet, _) = blocked_fleet();
        for summary in fleet.sessions() {
            let seqs: Vec<u64> = fleet
                .events_after(&summary.session_uid, 0, 1000)
                .iter()
                .map(|event| event.seq)
                .collect();
            assert_eq!(
                seqs,
                (1..=seqs.len() as u64).collect::<Vec<u64>>(),
                "{} numbered its log {seqs:?}",
                summary.session_id
            );
            assert_eq!(summary.last_seq, *seqs.last().unwrap());
        }
    }

    #[test]
    fn a_blocked_run_reports_the_card_it_is_holding_in_blocked_on() {
        let (fleet, _) = blocked_fleet();
        let blocked: Vec<_> = fleet
            .sessions()
            .into_iter()
            .filter(|summary| !summary.blocked_on.is_empty())
            .collect();
        assert_eq!(blocked.len(), 3, "one blocked run per risk class");
        for summary in blocked {
            let card = card_on_the_wire(&fleet, &summary.session_uid);
            assert_eq!(summary.blocked_on, vec![card.request_id]);
            // The hash the phone must echo is the hash of the text it is shown.
            assert_eq!(
                card.payload_hash,
                protocol::hash::approval_payload_hash(&card.tool_name, &card.tool_input)
            );
            assert_eq!(
                card.display_text,
                protocol::hash::approval_payload_text(&card.tool_name, &card.tool_input)
            );
            assert!(card.identity_bound, "a card this server holds is provable");
        }
    }

    #[test]
    fn an_answer_applies_once_and_every_retry_replays_the_original_outcome() {
        let (fleet, mut clock) = blocked_fleet();
        let uid = uid_of(&fleet, "cc-1");
        let card = card_on_the_wire(&fleet, &uid);

        let first = fleet.answer(
            &card.request_id,
            &card.payload_hash,
            AnswerDecision::Allow,
            Some(&uid),
            clock.step(),
        );
        let applied = match first {
            AnswerResult::Applied { outcome } => outcome,
            other => panic!("the first answer must apply; got {other:?}"),
        };
        assert_eq!(applied.resolved_by, ResolvedBy::Phone);
        assert_eq!(applied.applied_via, AnswerPath::SendKeys);
        assert!(!applied.inferred && !applied.indeterminate);
        // Named by the run's tmux name, as `ccd` records it.
        assert_eq!(applied.session_id, "cc-1");

        // The retry a flaky link produces: same id, same hash.
        match fleet.answer(
            &card.request_id,
            &card.payload_hash,
            AnswerDecision::Allow,
            Some(&uid),
            clock.step(),
        ) {
            AnswerResult::Duplicate {
                outcome,
                stale_payload_hash,
            } => {
                assert_eq!(outcome, applied, "a duplicate replays the original outcome");
                assert!(!stale_payload_hash);
            }
            other => panic!("a retry must be a duplicate; got {other:?}"),
        }

        // And the retry that also carries a stale hash: still the original
        // outcome, with the staleness surfaced rather than a refusal. The ledger
        // is consulted before the card, so a settled tap is never re-offered.
        match fleet.answer(
            &card.request_id,
            "0000",
            AnswerDecision::Deny,
            Some(&uid),
            clock.step(),
        ) {
            AnswerResult::Duplicate {
                outcome,
                stale_payload_hash,
            } => {
                assert_eq!(
                    outcome.decision,
                    AnswerDecision::Allow,
                    "the first answer wins"
                );
                assert!(stale_payload_hash);
            }
            other => panic!("a stale retry must still be a duplicate; got {other:?}"),
        }

        // Exactly one resolution reached the log, whatever the phone sent.
        let resolutions = fleet
            .events_after(&uid, 0, 1000)
            .iter()
            .filter(|event| event.kind == EventKind::ApprovalResolved)
            .count();
        assert_eq!(resolutions, 1);
    }

    #[test]
    fn an_answer_carrying_the_wrong_hash_is_refused_and_leaves_the_card_answerable() {
        let (fleet, mut clock) = blocked_fleet();
        let uid = uid_of(&fleet, "cc-2");
        let card = card_on_the_wire(&fleet, &uid);

        match fleet.answer(
            &card.request_id,
            "not the hash this card was served with",
            AnswerDecision::Allow,
            Some(&uid),
            clock.step(),
        ) {
            AnswerResult::Rejected { reason } => assert_eq!(
                reason, "stale payload_hash: the card you answered is out of date",
                "the phone matches on this sentence"
            ),
            other => panic!("a mismatched hash must be refused; got {other:?}"),
        }
        assert!(
            fleet
                .events_after(&uid, 0, 1000)
                .iter()
                .all(|event| event.kind != EventKind::ApprovalResolved),
            "a refused answer must leave no resolution behind"
        );
        // Nothing was typed, so the card is still the one on screen.
        assert!(matches!(
            fleet.answer(
                &card.request_id,
                &card.payload_hash,
                AnswerDecision::Allow,
                Some(&uid),
                clock.step(),
            ),
            AnswerResult::Applied { .. }
        ));
    }

    #[test]
    fn an_unknown_request_is_refused_in_the_words_the_phone_reads() {
        let (fleet, mut clock) = blocked_fleet();
        let uid = uid_of(&fleet, "cc-1");
        match fleet.answer(
            "toolu_nothing",
            "h",
            AnswerDecision::Allow,
            Some(&uid),
            clock.step(),
        ) {
            AnswerResult::Rejected { reason } => {
                assert_eq!(reason, "unknown or already-resolved request")
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn resolving_a_card_continues_the_turn_and_respawns_a_new_request_id() {
        let (fleet, mut clock) = blocked_fleet();
        let uid = uid_of(&fleet, "cc-1");
        let first = card_on_the_wire(&fleet, &uid);

        fleet.answer(
            &first.request_id,
            &first.payload_hash,
            AnswerDecision::Allow,
            Some(&uid),
            clock.step(),
        );
        // The turn visibly continues: the command runs, then it completes.
        pump(&fleet, &mut clock, 4);
        let kinds: Vec<EventKind> = fleet
            .events_after(&uid, 0, 1000)
            .into_iter()
            .map(|event| event.kind)
            .collect();
        for expected in [
            EventKind::ApprovalRequest,
            EventKind::ApprovalResolved,
            EventKind::ToolCall,
            EventKind::ToolResult,
            EventKind::TurnComplete,
        ] {
            assert!(
                kinds.contains(&expected),
                "{expected:?} missing from {kinds:?}"
            );
        }

        // And a fresh decision is waiting for the next reviewer.
        pump(&fleet, &mut clock, 1);
        let second = card_on_the_wire(&fleet, &uid);
        assert_ne!(
            second.request_id, first.request_id,
            "a respawned card must be a new request, not a retry of the settled one"
        );
        assert_ne!(
            second.payload_hash, first.payload_hash,
            "a new request has to be a new payload, or the ledger answers a tap nobody gave"
        );
        assert_eq!(fleet.resolve("cc-1").as_deref(), Some(uid.as_str()));
        assert_eq!(
            fleet
                .sessions()
                .iter()
                .find(|summary| summary.session_uid == uid)
                .map(|summary| summary.blocked_on.clone()),
            Some(vec![second.request_id.clone()]),
            "the fleet reports the card it is now holding, and only that one"
        );
        // The settled request stays settled, so a phone still holding the old
        // card cannot answer the new one by accident.
        assert!(matches!(
            fleet.answer(
                &first.request_id,
                &first.payload_hash,
                AnswerDecision::Allow,
                Some(&uid),
                clock.step(),
            ),
            AnswerResult::Duplicate { .. }
        ));
    }

    #[test]
    fn a_denied_card_runs_nothing() {
        let (fleet, mut clock) = blocked_fleet();
        let uid = uid_of(&fleet, "cc-1");
        let card = card_on_the_wire(&fleet, &uid);
        fleet.answer(
            &card.request_id,
            &card.payload_hash,
            AnswerDecision::Deny,
            Some(&uid),
            clock.step(),
        );
        pump(&fleet, &mut clock, 3);
        let kinds: Vec<EventKind> = fleet
            .events_after(&uid, 0, 1000)
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert!(kinds.contains(&EventKind::ApprovalResolved));
        assert!(
            !kinds.contains(&EventKind::ToolCall),
            "a refused command must not be shown running: {kinds:?}"
        );
        assert!(kinds.contains(&EventKind::TurnComplete));
    }

    #[test]
    fn a_send_text_is_idempotent_and_lands_on_the_run_it_names() {
        let (fleet, _) = blocked_fleet();
        let uid = uid_of(&fleet, "cc-5");
        let before = fleet.events_after(&uid, 0, 1000).len();
        let hash = protocol::hash::send_text_hash(&uid, "status?", true);

        match fleet.send_text(&uid, "status?".into(), Some("st-1"), Some(&hash), true) {
            SendTextResult::Sent { matched } => assert_eq!(matched, MATCHED_NEEDLE),
            other => panic!("got {other:?}"),
        }
        let after = fleet.events_after(&uid, 0, 1000);
        assert_eq!(
            after.len(),
            before + 2,
            "what was typed, and the reply to it"
        );
        assert_eq!(after[before].kind, EventKind::UserMessage);
        assert_eq!(after[before].payload["message"]["content"], "status?");
        assert_eq!(after[before + 1].kind, EventKind::AgentMessage);

        // The retry a dropped reply produces types nothing a second time.
        match fleet.send_text(&uid, "status?".into(), Some("st-1"), Some(&hash), true) {
            SendTextResult::Duplicate { matched, .. } => assert_eq!(matched, MATCHED_NEEDLE),
            other => panic!("a retry must be a duplicate; got {other:?}"),
        }
        assert_eq!(fleet.events_after(&uid, 0, 1000).len(), before + 2);

        // The same id with different text is a different mutation, and is
        // refused rather than silently treated as the settled one.
        let other_hash = protocol::hash::send_text_hash(&uid, "rm -rf /", true);
        assert!(matches!(
            fleet.send_text(
                &uid,
                "rm -rf /".into(),
                Some("st-1"),
                Some(&other_hash),
                true
            ),
            SendTextResult::Refused { .. }
        ));
        // And a hash that does not match its own request is refused before any
        // ledger is touched.
        assert!(matches!(
            fleet.send_text(&uid, "hello".into(), Some("st-2"), Some(&hash), true),
            SendTextResult::Refused { .. }
        ));
    }

    #[test]
    fn a_run_whose_log_fills_is_retired_and_replaced_rather_than_truncated() {
        let mut clock = Clock::new();
        let fleet = Fleet::new(clock.0).unwrap();
        let original = uid_of(&fleet, "cc-5");

        // Far more steps than one log can hold, so the slot turns over.
        pump(&fleet, &mut clock, MAX_EVENTS_PER_RUN + 8);

        let log = fleet.events_after(&original, 0, 10_000);
        let seqs: Vec<u64> = log.iter().map(|event| event.seq).collect();
        assert_eq!(
            seqs,
            (1..=seqs.len() as u64).collect::<Vec<u64>>(),
            "a retired log is never renumbered and never has a hole cut in it"
        );
        assert!(
            log.len() <= MAX_EVENTS_PER_RUN + 1,
            "a run is bounded: {} events",
            log.len()
        );
        assert_eq!(
            log.last().map(|event| event.kind.clone()),
            Some(EventKind::SessionEnd),
            "a retired run says it ended rather than simply stopping"
        );

        // The name now resolves to a *different* run, whose log starts again at
        // one. Two runs, told apart by their uids, never one spliced timeline.
        let successor = uid_of(&fleet, "cc-5");
        assert_ne!(successor, original);
        assert_eq!(fleet.events_after(&successor, 0, 10)[0].seq, 1);

        let summaries = fleet.sessions();
        assert_eq!(
            summaries
                .iter()
                .filter(|summary| summary.session_id == "cc-5")
                .count(),
            2,
            "at most two runs per slot, which is what bounds this server's memory"
        );
        let retired = summaries
            .iter()
            .find(|summary| summary.session_uid == original)
            .expect("a retired run stays listed, or the phone wipes its cache of it");
        assert_eq!(retired.lifecycle, Lifecycle::Exited);
        assert_eq!(retired.link, Link::Detached);
        assert!(retired.blocked_on.is_empty());
    }

    #[test]
    fn a_session_reference_may_be_a_uid_or_a_name_and_nothing_else() {
        let (fleet, _) = blocked_fleet();
        let uid = uid_of(&fleet, "cc-3");
        assert_eq!(fleet.resolve(&uid).as_deref(), Some(uid.as_str()));
        assert_eq!(fleet.resolve("cc-3").as_deref(), Some(uid.as_str()));
        assert_eq!(fleet.name_of(&uid), "cc-3");
        assert_eq!(fleet.resolve("cc-99"), None);
        assert_eq!(fleet.resolve("01K1B3XQ8ZC0DE5FGH7JKMNPQR"), None);
    }

    #[test]
    fn every_run_names_its_project_and_carries_a_uid() {
        let (fleet, _) = blocked_fleet();
        let summaries = fleet.sessions();
        assert_eq!(summaries.len(), 5);
        for summary in &summaries {
            assert!(protocol::uid::is_well_formed(&summary.session_uid));
            assert!(!summary.project_label.is_empty(), "{summary:?}");
            assert!(summary.cwd.starts_with("/Users/dev/app"), "{summary:?}");
            assert_eq!(summary.lifecycle, Lifecycle::Live);
        }
    }
}
