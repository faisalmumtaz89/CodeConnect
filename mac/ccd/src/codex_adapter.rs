//! Codex app-server → agent-neutral event normalization.
//!
//! This is the observation half of the Codex adapter. It takes the app-server
//! **notification** frames a connection delivers — the real codex-cli 0.147
//! shapes, captured in `fixtures/codex/` — and turns them into [`PendingEvent`]s
//! in the same thread → turn → item model the Claude path already feeds the
//! phone. Nothing here talks to a socket, a broker, or the daemon: it is a pure,
//! unit-testable mapper over frames. The live WS-over-UDS client, the broker,
//! approvals, the resolution envelope, steering, and the switch machinery live
//! elsewhere.
//!
//! ## What the frames actually look like (grounded in the fixtures)
//!
//! Every mapping decision below cites the fixture line it was read off. The
//! ground truth is the captured stream, not any written description — where they
//! diverge, the code follows the capture and the divergence is reported.
//!
//! ## Source and dedup
//!
//! Events carry [`Source::Codex`]: the app-server stream is the single
//! first-hand structured observation of a Codex session, exactly as a hook is
//! for Claude. The daemon dedup key is `(session_uid, source, source_event_id)`
//! (`store.rs`), so this module's whole job on the id side is to mint a **stable,
//! unique** `source_event_id` per fact, **thread-namespaced** so a thread
//! switch inside one session cannot alias two threads' item ids. Tool calls and
//! their results share one upstream `item.id`, so — matching the Claude
//! convention (`state.rs`: `pre:`/`post:`) — the call is `…:pre:<id>` and the
//! result `…:post:<id>`, or they would collapse to one row.
//!
//! ## In-flight state and interrupted turns
//!
//! Most mappings are per-frame pure. The one exception is an **interrupted**
//! (or failed) turn: `turn/completed{status:"interrupted"}` carries `items: []`
//! (`itemsView:"notLoaded"`) even for items that were mid-flight, and those
//! items never receive their own `item/completed`. So the adapter tracks
//! the items it has seen `item/started` for and, at the aborted boundary,
//! synthesizes their terminal states from what it observed **live** — never
//! from cross-resume item-id equality, which is unstable inside an interrupted
//! turn.
//!
//! ## The two entry points
//!
//! [`CodexAdapter::ingest`] maps one **notification** to facts. [`CodexAdapter::plan_resume_seed`]
//! reads a **`thread/resume` answer** — the other thing the app-server describes a
//! thread with — into the same facts, and is what lets a link that was down across a
//! turn recover it. Both are pure: no socket, no daemon, no clock. The seeding pair is
//! deliberately split into plan-then-apply so its caller can make the recovered facts
//! durable *before* anything in here forgets what it had open; the rules that split
//! enforces are on [`CodexAdapter::apply_resume_seed`].
//!
//! The live caller is [`crate::codex_link`], which holds the connection, stamps each
//! frame at ingress and feeds the admitted ones through here. Nothing in this module
//! knows that: it is still a pure mapper.

use std::collections::{BTreeMap, HashMap, VecDeque};

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

/// An **optional** field that must be a non-empty string whenever it is there at all.
///
/// Absent and `null` both pass — the resume answer spells "no value" both ways
/// (`reasoningEffort: null` beside an absent key) and neither is a shape this build
/// cannot read. A present value that is not a non-empty string is: it means the field
/// has changed type under us, and reading past it would be reading a shape nobody
/// measured.
fn absent_or_nonempty_str(v: &Value, key: &str) -> bool {
    match v.get(key) {
        None | Some(Value::Null) => true,
        Some(other) => other.as_str().is_some_and(|s| !s.is_empty()),
    }
}

/// An **optional** field that names a thread: absent or `null` passes, anything
/// present must be exactly the thread we asked about. This is what makes a second
/// thread-naming field unable to contradict the identity the answer is read under.
fn absent_or_equal(v: &Value, key: &str, expected: &str) -> bool {
    match v.get(key) {
        None | Some(Value::Null) => true,
        Some(other) => other.as_str() == Some(expected),
    }
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

/// **Is this a finished-turn item the seeding path is allowed to mint a fact from?**
///
/// An **allowlist**, not a sanity check, and the difference is the point. The live path
/// reads these same fields from a frame the wire just emitted, one field at a time, and a
/// malformed one costs that one frame. The seeding path mints facts onto **first-wins**
/// dedup keys, so a payload built from a wrong-typed field does not cost one frame — it
/// occupies the key the real live fact would have taken, permanently, and the real one is
/// then dropped as a duplicate. A `""` where the assistant's reply belongs would be
/// indistinguishable from an empty reply for ever.
///
/// So the answer may only describe item types this build has **measured on this wire**,
/// and each of them only in the shape it was measured in:
///
///   * **`agentMessage`** — `text` must be a string (`"ok"` in the fixture). Absent or
///     non-string, [`message_payload`] falls through to the `content` branch and yields
///     `""`; that is the defaulted value this refuses to mint.
///   * **`userMessage`** — `content` must be an array, and **every part an object with a
///     string `text`** (`{"type":"text","text":…,"text_elements":[]}` as measured). A
///     scalar part, or an object without `text`, is silently skipped by the join, so the
///     recorded prompt would be a truncated version of what the user actually typed —
///     and that truncation would hold the key for ever. It must also carry **no
///     top-level `text`**: [`message_payload`] *prefers* that field when it is a string
///     and never looks at `content` at all, so an answer carrying both would render the
///     one field nothing here validated and persist it on a first-wins key. The measured
///     shape has `content` and no `text`, so the alias is refused rather than
///     accommodated — and the renderer stays shared with the live path rather than
///     diverging into two spellings of the same fact.
///   * **`reasoning`** — `summary` and `content` are arrays where present (both `[]` in
///     the fixture). These are cloned rather than read, so there is no defaulting hazard,
///     but a changed type is still a shape nobody measured.
///
///   * **`commandExecution`** — the three fields the payload is built from, each
///     checked for the type it is read as: `command` a string, `cwd` a string,
///     `commandActions` an array. Plus a **terminal `status`**, which is the whole of
///     why this type is admissible at all: [`tool_result_payload`] defaults a missing
///     one to `"completed"`, so an item that does not state its own outcome would
///     persist a success that never happened, on a first-wins key, permanently.
///   * **`fileChange`** — `changes` an array (cloned into the payload, not read), and
///     the same terminal `status` on the same reasoning.
///
/// All four of those field names, and `status`'s presence, are what codex 0.153's
/// app-server schema (`codex app-server generate-json-schema`) declares REQUIRED for the
/// two variants (`thread/resume`'s result → `ThreadItem`), and the terminal set is its
/// `CommandExecutionStatus`/`PatchApplyStatus` enums minus `inProgress`. So this is not a
/// shape guessed from one capture; it is the schema, asserted.
///
/// **Everything else is not read**, including item types this build models perfectly well
/// from the live wire, and including several — `mcpToolCall`, `dynamicToolCall`,
/// `collabAgentToolCall`, `imageGeneration` — that carry a required `status` of their
/// own. Carrying the field is not what earns admission; having been measured is. An item
/// type that is simply unknown would otherwise be preserved wholesale as an `Other` fact,
/// which sounds harmless and is not — nothing has ever compared such a fact against the
/// one the live path produces for the same item, so there is no evidence the two would
/// agree, and a first-wins key is the wrong place to find out. Such an item mints no fact
/// and the rest of the answer is read: real history carries them (web searches, MCP and
/// sub-agent calls, context compactions, images), and they are recorded live whenever the
/// link was attached.
///
/// The admitted types are what a resume answer has been measured to carry:
/// `userMessage` and `agentMessage` in the committed
/// `fixtures/codex/resume-populated-answer.json`, `reasoning` in a live two-session
/// measurement (a completed turn whose live stream was userMessage → reasoning →
/// agentMessage came back carrying all three), and the two
/// tool types in `fixtures/codex/resume-after-tool-turn-0.153.4.jsonl` — the 2026-09-08
/// production answer, whose refusal cost 70 epochs over 18 minutes.
///
/// **An item is only ever offered to this function from a FINISHED turn**:
/// [`CodexAdapter::plan_resume_seed`] reads a running turn for its id alone, so whether
/// the turn is history is decided there, and whether the item states its own outcome is
/// decided here.
fn seeded_item_shape_is_measured(item: &Value, item_type: &str) -> bool {
    match item_type {
        "agentMessage" => item.get("text").is_some_and(Value::is_string),
        "userMessage" => {
            item.get("text").is_none()
                && item
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().all(|part| {
                            part.is_object() && part.get("text").is_some_and(Value::is_string)
                        })
                    })
        }
        "reasoning" => {
            item.get("summary").is_none_or(Value::is_array)
                && item.get("content").is_none_or(Value::is_array)
        }
        "commandExecution" => {
            item.get("command").is_some_and(Value::is_string)
                && item.get("cwd").is_some_and(Value::is_string)
                && item.get("commandActions").is_some_and(Value::is_array)
                && item
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(is_terminal_tool_status)
        }
        "fileChange" => {
            item.get("changes").is_some_and(Value::is_array)
                && item
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(is_terminal_tool_status)
        }
        _ => false,
    }
}

/// **Is every field the tool payload READS the type it is read as?**
///
/// [`seeded_item_shape_is_measured`] is the admission rule — which item types may mint a
/// fact at all, and whether each states its own outcome. This is the narrower question
/// underneath it: of the fields [`tool_result_payload`] then *copies into the payload*,
/// is each one a value the phone can read?
///
/// # Why it is a second check rather than more clauses in the first
///
/// They ask different questions. [`seeded_item_shape_is_measured`] asks whether this is an
/// item type and shape this build reads at all; this asks whether the values a tool
/// result's payload copies are of the types the payload is built from. A no from either
/// costs **that item's fact and nothing else**: the rest of the turn seeds, the answer is
/// still accepted, and the link keeps its subscription. The gap is one item, counted in
/// one warning, rather than a leg.
///
/// # Why the values need checking at all, now
///
/// On the live path these fields come from a frame this build watched arrive, and a
/// malformed one costs that one frame. Since the resume reader began admitting a FINISHED
/// tool-bearing turn they also arrive from **history**, and there they are minted onto a
/// first-wins dedup key — so a wrong-typed value does not cost one frame, it takes the key
/// the real live fact would have taken and holds it for ever, uncorrectable.
///
/// # What each field is measured to be
///
///   * **`exitCode` and `durationMs` on a `completed` commandExecution — NUMBERS.**
///     Measured on every captured completed item (`fixtures/codex/command-execution.jsonl`
///     frame 20 and `fixtures/codex/file-change.jsonl` frames 12 and 30, all
///     `exitCode: 0, durationMs: 0`), and in the resume answer itself. A string `"0"`
///     where a number belongs is the shape refused here.
///
///     **Absent or `null` is accepted on the other two terminal statuses**, and that is a
///     measurement rather than a softening: a live `declined` item carries `null` for
///     both (`fixtures/codex/approval-switch-p1-ctrlc-new-0.153.jsonl`), the `failed` items
///     in both 0.164.0 resume captures carry numbers (`exitCode: 6, durationMs: 0`), and
///     the LIVE path already writes `null` for both fields from
///     `tool_result_payload`'s own `unwrap_or(Value::Null)` (measured on `item/started`,
///     where all three are null). Demanding a number on those two statuses would make the
///     seeded fact for a declined command differ from the live fact for the
///     same item — and those two must be byte-identical or the dedup key stops collapsing
///     them, which is the property the whole recovery path rests on.
///   * **`aggregatedOutput` — a string, or absent/`null`.** Measured BOTH ways on a
///     `completed` item: the string in `file-change.jsonl` frame 12, and an explicit
///     `null` in `command-execution.jsonl` frame 20 (a command that printed nothing). So
///     null is the wire's own spelling of "no output" here and is admitted; an object or a
///     number is not.
///   * **`changes[]` on a `fileChange` — every member an object carrying the measured
///     keys.** The measured shape is `{"path": <string>, "kind": {"type": …,
///     "move_path": …}, "diff": <string>}` (`fixtures/codex/file-change.jsonl` frames 17
///     and 22). All three are load-bearing on the phone and each fails differently:
///     `CodexWire.changes` DROPS a row with no string `path`, so the patch would silently
///     lose a file; it defaults a missing `diff` to `""`, which renders as a change with
///     no content and is indistinguishable from an empty one for ever; and it reads
///     `kind["type"]`, defaulting to `"update"`, so a `kind` that is not an object
///     relabels a delete as an edit. `changes` being an array at all is
///     [`seeded_item_shape_is_measured`]'s clause; this is what is inside it.
///
/// Nothing here is checked for a `commandExecution`'s `command`, `cwd` or
/// `commandActions`, or for `status` on either variant: those are the admission rule's,
/// and duplicating them would put two verdicts on one field.
fn seeded_tool_payload_is_readable(item: &Value, item_type: &str) -> bool {
    let absent_null_or = |key: &str, ok: fn(&Value) -> bool| match item.get(key) {
        None | Some(Value::Null) => true,
        Some(value) => ok(value),
    };
    match item_type {
        "commandExecution" => {
            let completed =
                item.get("status").and_then(Value::as_str) == Some(RESUME_TURN_COMPLETED);
            let number = |key: &str| match item.get(key) {
                Some(value) if value.is_number() => true,
                // See the doc: a `failed` or `declined` item may carry null, as the
                // live path writes it; a `completed` one always carries numbers.
                None | Some(Value::Null) => !completed,
                Some(_) => false,
            };
            number("exitCode")
                && number("durationMs")
                && absent_null_or("aggregatedOutput", Value::is_string)
        }
        "fileChange" => item
            .get("changes")
            .and_then(Value::as_array)
            .is_some_and(|changes| {
                changes.iter().all(|change| {
                    change.get("path").is_some_and(Value::is_string)
                        && change.get("diff").is_some_and(Value::is_string)
                        && change.get("kind").is_some_and(Value::is_object)
                })
            }),
        // Every other admitted type's payload is built from fields
        // [`seeded_item_shape_is_measured`] has already checked, and none of them is a
        // tool result. Nothing to add.
        _ => true,
    }
}

/// **Has this tool item's outcome been DECIDED?**
///
/// The three terminal members of the 0.153 `CommandExecutionStatus` and
/// `PatchApplyStatus` enums, which are the same three (codex 0.153's app-server
/// schema). `inProgress` is the
/// fourth and the one deliberately absent: an item still running has no outcome to
/// record, and the seeding path's only reason to read a tool item is to record the
/// outcome it states.
///
/// Spelled as an allowlist rather than as `!= "inProgress"` for the reason every other
/// check in this file is: a status codex's schema has not declared is a shape
/// nobody has measured, and the honest response to one is to measure it, not to assume
/// it means "finished". A `failed` or `declined` item is recorded as exactly that —
/// `tool_result_payload` copies the string through — so nothing here decides what an
/// outcome MEANS, only that one was stated.
fn is_terminal_tool_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "declined")
}

/// The `turn.status` a `thread/resume` answer uses for a turn that **finished normally**.
const RESUME_TURN_COMPLETED: &str = "completed";

/// The `turn.status` of a turn **stopped** before it finished (Esc at the Mac, Stop on
/// the phone). Measured on 0.164.0 through the live TUI's app-server
/// (`fixtures/codex/resume-interrupted-*`): its terminal fields equal the live
/// `turn/completed`'s. Until a later turn has run, its prompt carries a placeholder id
/// and an in-flight command may appear `inProgress`; after one, the prompt carries its
/// live id, and the command is absent until its shell has ended, then appears under its
/// live id with the outcome it reached.
const RESUME_TURN_INTERRUPTED: &str = "interrupted";

/// The `turn.status` of a turn that **ended in an error**. Measured on 0.164.0 with a
/// bare `codex app-server` (`fixtures/codex/resume-failed-turn-0.164.0.json`): `error` is
/// an object, and every terminal field equals the live `turn/completed`'s.
const RESUME_TURN_FAILED: &str = "failed";

/// **Is this Codex's placeholder for an item id it does not have?** `item-<n>`, exactly.
///
/// Measured in three places, never on the live wire: the message items of a running
/// turn, the prompt of a stopped turn read through the live TUI's app-server before a
/// later turn has run, and the message
/// and compaction items of the threads codex 0.146.0 wrote, completed turns included. A
/// fact minted under one would sit on a key the live path never uses, and every running
/// turn's first item would alias onto `item-1`. Every other id compared with the live
/// wire (`msg_…`, `rs_…`, `exec-…`, UUIDs) is the live one.
fn is_placeholder_item_id(id: &str) -> bool {
    id.strip_prefix("item-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The `turn.status` a `thread/resume` answer uses for a turn that is **still
/// running**. Measured: its message items carry **placeholder ids** (`item-1`,
/// `item-2`), never the ids the live wire emitted, while a tool item keeps its real one
/// (`exec-…` in `fixtures/codex/resume-running-tool-turn-0.164.0.json`). None of them
/// is read — see [`CodexAdapter::plan_resume_seed`].
const RESUME_TURN_IN_PROGRESS: &str = "inProgress";

/// The only `itemsView` a `thread/resume` answer has been measured to report for a
/// turn — running or finished, every turn in every captured answer said `"full"`.
///
/// It is a **second, independent** guard beside the status: `turn/completed` on the
/// live wire carries `itemsView:"summary"` with a partial `items[]`, and
/// `turn/started` carries `"notLoaded"` with an empty one, so the vocabulary demonstrably
/// distinguishes a whole item list from a partial one. An answer that starts reporting
/// either of those — a paged history, a summarized turn — is describing a list this
/// build must not read as complete, and fails closed instead.
const RESUME_ITEMS_VIEW_FULL: &str = "full";

/// The reconciliation one accepted `thread/resume` answer implies, computed **without
/// mutating anything**.
///
/// Two phases rather than one, and the split is the whole safety property (see
/// [`CodexAdapter::plan_resume_seed`]): the facts are made durable first, and only a
/// caller that got them all written applies the state rebuild. A seed that is planned
/// and never applied has changed nothing, so the attach that failed can simply be
/// retried on the next connection.
pub struct ResumeSeed {
    /// The thread this answer was read under — the one the resume asked about.
    thread_id: String,
    /// The facts the answer describes, in timeline order. These must be durable
    /// **before** [`CodexAdapter::apply_resume_seed`] is allowed to forget anything.
    events: Vec<PendingEvent>,
    /// The turns the answer reports as still running. An open item belonging to one of
    /// these keeps the state this adapter observed live — the answer confirms the
    /// item's turn is alive, and confirms nothing else about it.
    running_turns: Vec<String>,
    /// The answer's last turn, when it stopped or failed and still named items by
    /// placeholder ids — a turn read through the live TUI's app-server before a later
    /// turn has run. Its ids settle once a later turn runs (measured on codex 0.164.0).
    unsettled_turn: Option<String>,
    /// The `(thread, turn)` usage totals held for turns the answer reports finished.
    /// They are **dropped** by the rebuild, never emitted — see the note on discarding
    /// a held usage total in [`CodexAdapter::plan_resume_seed`]: a total held for a turn
    /// whose completion was missed is a mid-turn snapshot, and the dedup key is
    /// first-wins.
    discarded_usage: Vec<(String, String)>,
    /// How many turns the answer described, and how many of those had finished —
    /// reported by the caller, never inferred from the event count.
    described_turns: usize,
    terminal_turns: usize,
}

impl ResumeSeed {
    /// The facts to record, taken out so the caller cannot record them twice.
    pub fn take_events(&mut self) -> Vec<PendingEvent> {
        std::mem::take(&mut self.events)
    }

    /// How many turns the answer described in total.
    pub fn described_turns(&self) -> usize {
        self.described_turns
    }

    /// How many of them had finished, and therefore contributed facts.
    pub fn terminal_turns(&self) -> usize {
        self.terminal_turns
    }

    /// How many were still running, and therefore only confirmed liveness.
    pub fn running_turns(&self) -> usize {
        self.running_turns.len()
    }

    /// **The turns this answer reported still running.**
    ///
    /// The caller needs their ids, not just a count, because a turn seeded in this state
    /// carries a debt: its items that had already finished were described with
    /// placeholder ids and could not be recovered, and the only thing that can settle
    /// them is a later answer reporting the same turn finished. See
    /// [`crate::codex_link`]'s follow-up attach.
    pub fn running_turn_ids(&self) -> &[String] {
        &self.running_turns
    }

    /// **The stopped or failed turn whose items this answer could not name yet**, if
    /// the answer's last turn is one. Its debt is paid by the next turn on the thread that
    /// completes after it: see [`crate::codex_link`]'s follow-up attach.
    pub fn unsettled_turn_id(&self) -> Option<&str> {
        self.unsettled_turn.as_deref()
    }
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

            // turn/started — observed live (interrupt.jsonl), but it is never
            // replayed and a late subscriber rebuilds in-flight state from
            // the resume response's turns[].status instead. The neutral model has
            // no "turn started" fact, so it maps to nothing.
            //
            // In-flight state on a late attach comes from the `thread/resume`
            // **response** instead, which [`CodexAdapter::plan_resume_seed`] now
            // reads: a turn the answer reports `inProgress` confirms its open items
            // are alive, and a finished one describes the terminals of the items it
            // can read. So there is still no "turn started" fact to mint here —
            // the turn's existence is carried by its terminal, from either source.
            //
            // **It is not inert, though — it is just not a fact.** The frame is the
            // only thing on this wire that says the agent has gone back to work, and
            // the push gate reads it as such: see `codex_link`'s
            // `Connection::note_turn_start`, which takes it off the raw frame
            // precisely because there is no event here to hang it on.
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

            // Approval request/resolution is `codex_approval`'s (typed cards,
            // composite-id activations, the resolution envelope, option ids). It
            // is observed in this stream (command-execution.jsonl,
            // interrupt.jsonl) but deliberately NOT normalized here — emitting a
            // bare approval event without that machinery would be a speculative
            // half-surface. Dropped here.
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "serverRequest/resolved" => Vec::new(),

            // A method this build does not know: never panic, never guess. Drop.
            _ => Vec::new(),
        }
    }

    /// Read a `thread/resume` **result** into a [`ResumeSeed`] for `requested_thread`,
    /// or refuse it. Pure: reads the answer and this adapter's own state, mutates
    /// neither. `None` means "a shape this build has not measured" and is the caller's
    /// signal to fail closed.
    ///
    /// # What the wire actually says, and what that forces
    ///
    /// Measured against a real codex 0.147 over two live sessions (seven turns, four
    /// resume answers, cross-checked frame by frame against the same turns observed
    /// live on a subscribed connection):
    ///
    ///   * **`turns[]` is complete.** After one, two and three turns the answer carried
    ///     one, two and three turns — oldest first, `initialTurnsPage: null`. A
    ///     finished turn's `items[]` is complete too: a turn whose live stream was
    ///     `userMessage`, `reasoning`, `agentMessage` came back with all three, in that
    ///     order, under `itemsView:"full"`.
    ///   * **A finished turn keys stably.** Every turn id and every item id in a
    ///     `completed` turn was **byte-identical** to the id the live wire emitted for
    ///     it, and identical again across later resumes of the same thread. That is
    ///     what makes recovering one safe: it lands on the dedup key the live path
    ///     would have minted, so a fact observed live and the same fact recovered from
    ///     an answer collapse to one row instead of doubling.
    ///   * **A running turn does NOT.** The message items of an `inProgress` turn carry
    ///     **placeholder ids** — `item-1`, `item-2` — while the live wire is emitting
    ///     `01a03888-ca10-…` and `msg_060c22…` for those very items. The same turn,
    ///     resumed twice, reported `item-1`/`item-2` while it ran and the real ids once
    ///     it finished. Item ids are unstable in **every turn still running**. A tool item there keeps its real `exec-…` id
    ///     (`fixtures/codex/resume-running-tool-turn-0.164.0.json`), which changes
    ///     nothing below: no item of a running turn is read.
    ///
    ///   * **A stopped or failed turn is history too.** `interrupted` and `failed` turns
    ///     report terminal fields equal to the live `turn/completed`'s (codex 0.164.0). A
    ///     stopped turn read through the live TUI's app-server names its prompt by a
    ///     placeholder, and may show an in-flight command `inProgress`, until a later turn
    ///     has run; after that its prompt carries its live id, and the command is absent
    ///     until its shell has ended. A failed turn's prompt carried a real id when read on
    ///     a bare app-server right after the failure.
    ///
    /// So the rule is forced, not chosen:
    ///
    ///   * a **finished** turn — `completed`, `interrupted` or `failed` — contributes its
    ///     items' terminals and its turn terminal, and **no usage fact**: a total held for
    ///     a turn whose completion was missed is a mid-turn snapshot, and `usage:<turn>` is
    ///     first-wins, so promoting it would durably shadow the real final total. The stale
    ///     total is discarded instead and the gap is left legible;
    ///   * an item of a finished turn that this build cannot read — a placeholder id, a
    ///     type or shape nobody measured, an outcome not yet decided, a payload value of
    ///     the wrong type — **costs that item and nothing else**. Finished history only
    ///     ever becomes recovered facts, so refusing the whole answer over one item would
    ///     buy nothing, and on a first attach it would refuse the same unchanging history
    ///     on every reconnect for the life of the session;
    ///   * an `inProgress` turn contributes **no item fact whatsoever** — its message
    ///     ids are known-fabricated, and minting `…:item:item-1` would both invent a fact and
    ///     alias every running turn's first item onto one key — only the confirmation
    ///     that its turn is alive;
    ///   * **what makes the answer itself untrustworthy refuses it**: another thread, a
    ///     page instead of the whole history, contradictory identities, a turn status
    ///     outside the four codex's schema declares. The refusal is loud and names the
    ///     shape.
    pub fn plan_resume_seed(&self, result: &Value, requested_thread: &str) -> Option<ResumeSeed> {
        // --- identity: this answer must be about the thread we asked about -------
        let thread = result.get("thread")?;
        if require_str(thread, "id") != Some(requested_thread) {
            return None;
        }
        // A second field naming a thread may not contradict the first. Picking one
        // would be choosing which contradiction to believe — the same rule
        // `codex_link::frame_thread_id` applies to a notification.
        if !absent_or_equal(result, "threadId", requested_thread) {
            return None;
        }
        // **`sessionId` is a second name for the same thread, and it must agree.**
        // Measured equal to `thread.id` on every captured answer. It is checked because
        // it is an *alias*: a reader that trusted it over `id` — or a future field that
        // starts being read — would file this session's facts under whatever it says, and
        // a contradictory pair is precisely the shape that would go unnoticed.
        //
        // A forked thread might legitimately carry a different `sessionId`; if one
        // ever reaches here the refusal is the designed re-grounding trigger rather
        // than a silent misfiling.
        if !absent_or_equal(thread, "sessionId", requested_thread) {
            return None;
        }

        let turns = result.pointer("/thread/turns")?.as_array()?;
        // **An answer that describes no turn at all is refused.** Every answer measured
        // on this wire carried at least one — a thread with no turns has no rollout and
        // answers with the not-ready error instead, which is the *other* accepted
        // shape. An empty `turns[]` under an otherwise well-formed envelope has never
        // been seen, and attaching on it would mean subscribing on the strength of an
        // answer that describes nothing.
        if turns.is_empty() {
            return None;
        }
        // **An answer that is a PAGE of history is refused.** Every captured answer
        // carried `initialTurnsPage: null`, and `turns[]` complete for the whole thread
        // (1→1, 2→2, 3→3 turns). A non-null page is the shape a paged history would
        // arrive as, and attaching on one would mean subscribing while older turns stay
        // silently unrecovered — the seed would look complete and be a window.
        //
        // The two backwards cursors are NOT required absent: measured, they are always
        // present, non-empty JSON strings, on every answer including the complete ones,
        // so they say "here is where paging backwards would start", not "this is a
        // page". They are checked for TYPE only.
        if !matches!(result.get("initialTurnsPage"), None | Some(Value::Null)) {
            return None;
        }
        for cursor in ["turnsBackwardsCursor", "itemsBackwardsCursor"] {
            if !absent_or_nonempty_str(result, cursor) {
                return None;
            }
        }

        let mut events = Vec::new();
        let mut running_turns = Vec::new();
        let mut unsettled_turn = None;
        let mut discarded_usage = Vec::new();
        let mut terminal_turns = 0;
        // The finished-turn items this build does not read, counted by why and by type.
        // They contribute no fact; the turn around them still seeds. Reported once at the
        // bottom of this function, never per item — a long thread skips thousands.
        let mut skipped: BTreeMap<(&'static str, String), usize> = BTreeMap::new();

        // The session's own identity, from the same Thread object `thread/started`
        // carries. This is how a link that never watched the announcement — a daemon
        // that restarted mid-session — records the session at all.
        events.push(self.thread_identity_event(thread)?);

        // **Identities must be unique within one answer.** A turn that appears
        // twice — once `completed` and once `inProgress` — is a contradiction, and
        // reading it would both seed a terminal and hold the same turn open. Item ids
        // are tracked across the whole answer for the same reason one layer down: two
        // fact-producing items sharing an id would collide on one dedup key, and
        // whichever was written first would silently swallow the other.
        let mut seen_turns = std::collections::HashSet::new();
        let mut seen_items = std::collections::HashSet::new();
        // **Oldest-first, validated rather than assumed.** Measured on every
        // captured answer (startedAt 1787654880 → …885 → …890). The order is what a
        // reader would rely on to decide which turn is current, so it is checked.
        let mut previous_started_at = i64::MIN;

        for turn in turns {
            let (Some(turn_id), Some(status)) =
                (require_str(turn, "id"), require_str(turn, "status"))
            else {
                return None;
            };
            if !seen_turns.insert(turn_id) {
                return None;
            }
            // Whole list or nothing: see [`RESUME_ITEMS_VIEW_FULL`].
            if require_str(turn, "itemsView") != Some(RESUME_ITEMS_VIEW_FULL) {
                return None;
            }
            let started_at = turn.get("startedAt").and_then(Value::as_i64)?;
            if started_at < previous_started_at {
                return None;
            }
            previous_started_at = started_at;
            let items = turn.get("items")?.as_array()?;
            // Every item must carry both routing identities — checked for EVERY turn,
            // including the running ones whose ids are then deliberately not used. A
            // turn whose items are not even shaped like items is not an answer this
            // build can read, whatever it then chooses to do with them.
            if items.iter().any(|item| {
                require_str(item, "id").is_none() || require_str(item, "type").is_none()
            }) {
                return None;
            }
            // **A running turn is read for its id alone, whatever its items are.** Its
            // items are never minted — the `inProgress` arm below records the turn and
            // nothing else — so no item of it can put a fact under a key the live wire
            // never emits. That includes tool items: a turn that ran a command and now
            // waits on an approval is exactly what a link resuming mid-turn finds
            // (`fixtures/codex/resume-running-tool-turn-0.164.0.json`), and refusing it
            // would be the STOP-AND-AMEND verdict on every reconnect until the turn ended
            // at the Mac: the approval could not reach the phone while the turn could not
            // end without it. The settled items of such a turn are recovered by the
            // follow-up resume when it finishes, as for any turn the link attached to
            // while it ran.

            // Overwritten by every turn, so after the loop it describes the last one.
            unsettled_turn = None;
            match status {
                // Alive: the answer confirms the turn is running and nothing more.
                // Its items are never read.
                RESUME_TURN_IN_PROGRESS => running_turns.push(turn_id.to_string()),
                // Finished: the items this build reads, and a terminal.
                RESUME_TURN_COMPLETED | RESUME_TURN_INTERRUPTED | RESUME_TURN_FAILED => {
                    // **The terminal's own fields, validated before it is minted.**
                    // `turn_terminal_events` copies them verbatim, so each must be a
                    // value codex reports. A `completed` turn has carried numeric
                    // `completedAt`/`durationMs` and a null `error` in every answer
                    // measured (125 of 125 in this Mac's interactive history). A stopped
                    // or failed one carries what codex's schema allows — an integer or
                    // null for the two times (null measured on sessions of codex 0.162.1
                    // and 0.164.0 that ended mid-turn), null or an error object for
                    // `error` (an object on
                    // every failed turn) — and those values equal the live
                    // `turn/completed`'s, measured.
                    let integer = |key: &str| turn.get(key).and_then(Value::as_i64).is_some();
                    let integer_or_null = |key: &str| {
                        turn.get(key)
                            .is_none_or(|value| value.is_null() || value.as_i64().is_some())
                    };
                    let error = turn.get("error").filter(|error| !error.is_null());
                    let terminal_is_measured = if status == RESUME_TURN_COMPLETED {
                        integer("completedAt") && integer("durationMs") && error.is_none()
                    } else {
                        integer_or_null("completedAt")
                            && integer_or_null("durationMs")
                            && error.is_none_or(Value::is_object)
                    };
                    if !terminal_is_measured {
                        return None;
                    }
                    terminal_turns += 1;
                    for item in items {
                        let (Some(item_id), Some(item_type)) =
                            (require_str(item, "id"), require_str(item, "type"))
                        else {
                            return None;
                        };
                        // **An item this build cannot read costs that item.** Checked
                        // BEFORE a fact is minted, because the key it would take is
                        // first-wins: a placeholder id is a key the live path never uses;
                        // an unmeasured type or shape, or an outcome not yet decided, would
                        // be a guess; a payload value of the wrong type would hold the key
                        // against the real fact for ever.
                        let unread = if is_placeholder_item_id(item_id) {
                            Some("placeholder id")
                        } else if !seeded_item_shape_is_measured(item, item_type) {
                            Some("type, shape or outcome not read")
                        } else if !seeded_tool_payload_is_readable(item, item_type) {
                            Some("payload value not read")
                        } else {
                            None
                        };
                        if let Some(reason) = unread {
                            *skipped.entry((reason, item_type.to_string())).or_default() += 1;
                            if status != RESUME_TURN_COMPLETED && is_placeholder_item_id(item_id) {
                                unsettled_turn = Some(turn_id.to_string());
                            }
                            continue;
                        }
                        // Only the ids that BECOME facts are checked for uniqueness: two
                        // facts sharing one would collide on one dedup key. A skipped
                        // item takes no key, and two turns can carry the same placeholder.
                        if !seen_items.insert(item_id) {
                            return None;
                        }
                        if let Some(event) = self.terminal_item_event(
                            requested_thread,
                            Some(turn_id.to_string()),
                            item_id,
                            item_type,
                            item,
                            false,
                        ) {
                            events.push(event);
                        }
                    }
                    // **A held usage total is DISCARDED here, never promoted.**
                    //
                    // `latest_usage` holds the newest cumulative total this adapter has
                    // *seen*, and a turn whose completion it missed is exactly a turn
                    // whose final total it never saw: what it is holding is some
                    // mid-turn snapshot U1, while the turn really ended at U2. Emitting
                    // U1 under `usage:<turn>` would be worse than emitting nothing,
                    // because the dedup key is first-wins — U1 would take the key and
                    // permanently shadow the true U2, and no later observation could
                    // ever correct it. So a recovered turn contributes no usage fact,
                    // the stale total is dropped, and the gap is legible rather than
                    // wrong. (A turn whose completion WAS observed live already emitted
                    // its usage at `on_turn_completed`, which consumed the entry — so
                    // there is nothing here to promote in that case either.)
                    let key = (requested_thread.to_string(), turn_id.to_string());
                    if self.latest_usage.contains_key(&key) {
                        discarded_usage.push(key);
                    }
                    events.extend(self.turn_terminal_events(
                        requested_thread,
                        turn_id,
                        turn,
                        status,
                        None,
                    ));
                }
                // A status outside codex's schema. Fail closed, loudly.
                _ => return None,
            }
        }

        // **Said once per answer, and only when there is something to say.** The
        // answer is accepted either way, so this is not a loop the way a refusal was:
        // it is one line naming what the seed is missing and why, so the gap the
        // operator sees in the transcript has a reason attached to it.
        if !skipped.is_empty() {
            crate::log_warn!(
                "codex link for {}: {} item(s) of {requested_thread} were not read from \
                 its history; any of them this daemon saw live is recorded already, and \
                 the rest of the answer was seeded normally: {}",
                self.session.name,
                skipped.values().sum::<usize>(),
                skipped
                    .iter()
                    .map(|((reason, item_type), count)| format!("{count} {item_type} ({reason})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Some(ResumeSeed {
            thread_id: requested_thread.to_string(),
            events,
            described_turns: turns.len(),
            terminal_turns,
            running_turns,
            unsettled_turn,
            discarded_usage,
        })
    }

    /// Rebuild in-flight state from a seed whose facts are **already durable**.
    ///
    /// Called only after every event in the seed has been recorded, because this is
    /// where the adapter forgets things. Three rules, each of them a constraint that
    /// has to hold here:
    ///
    ///   * **Never fabricate.** An open item the answer does not confirm still running
    ///     is dropped **without a terminal**. It may have completed while the link was
    ///     down — in which case the answer already described its real terminal and that
    ///     fact is now durable — or it may have vanished from history entirely.
    ///     Either way, synthesizing one from an id the answer did not vouch for is
    ///     inventing a fact. Placeholder-id renames land here by construction: a
    ///     placeholder id matches no observed item, so the observed item is simply
    ///     dropped rather than terminalized under a fabricated identity.
    ///   * **Merge, never replace.** An open item whose turn the answer reports still
    ///     running keeps everything this adapter watched happen — its `item/started`
    ///     snapshot and every delta that streamed into it. The answer confirms the
    ///     turn is alive; it does not re-describe the item, and it could not, because
    ///     its ids for a running turn are placeholders.
    ///   * **Another thread is not this answer's business.** Open items belonging to a
    ///     thread this answer is not about survive untouched.
    pub fn apply_resume_seed(&mut self, seed: &ResumeSeed) {
        self.open.retain(|open| {
            open.thread_id != seed.thread_id || seed.running_turns.contains(&open.turn_id)
        });
        // Dropped, not emitted: the total held for a turn whose completion this
        // adapter missed is stale, and a stale total under a first-wins key is a
        // permanent lie rather than a temporary gap.
        for key in &seed.discarded_usage {
            self.latest_usage.remove(key);
        }
    }

    fn on_thread_started(&mut self, params: &Value) -> Vec<PendingEvent> {
        let Some(thread) = params.get("thread") else {
            return Vec::new();
        };
        self.thread_identity_event(thread).into_iter().collect()
    }

    /// The session's identity fact, from a **Thread object**.
    ///
    /// Shared by the two places that carry one: the `thread/started` broadcast and a
    /// `thread/resume` answer's `result.thread` — which is the same object, measured
    /// field for field. Sharing the mapping is what makes the fact a link recovers on
    /// re-attach byte-identical to the one it would have recorded live, so the dedup
    /// key collapses them instead of the two paths drifting apart.
    ///
    /// **Only immutable fields go in.** The dedup key is `<thread>:thread_started`, so
    /// whichever of the two routes reaches the store first wins the payload for ever —
    /// which means any field that can *change* between the announcement and a later
    /// resume would make the recorded content depend on arrival order rather than on
    /// what happened. The Thread object carries several such fields, and they are
    /// excluded by construction rather than by being forgotten:
    ///
    ///   * `status` — `{"type":"idle"}` at announcement, `{"type":"active",…}` while a
    ///     turn runs. It is live state, and a *session started* fact is the wrong place
    ///     for it: the timeline already carries turn terminals for that.
    ///   * `preview`, `updatedAt`, `recencyAt` — all move as the thread is used
    ///     (measured: `preview` is `""` at announcement and the user's first prompt
    ///     afterwards), and none is read here.
    ///
    /// What is left — the thread id, its cwd, its rollout path and the CLI version that
    /// created it — is fixed for the life of the thread, so both routes render the same
    /// bytes whichever arrives first.
    fn thread_identity_event(&self, thread: &Value) -> Option<PendingEvent> {
        let tid = require_str(thread, "id")?;
        // **Validated before minting, not defaulted while minting.** These three are the
        // whole payload, and the key they land on is first-wins — so a Thread object
        // missing one, or carrying it as something other than a string, would take
        // `<thread>:thread_started` with a null in it and hold it against the correct
        // description arriving afterwards. Measured present and string-valued on both
        // routes: the `thread/started` broadcast and every `thread/resume` answer.
        let (Some(cwd), Some(path), Some(cli_version)) = (
            require_str(thread, "cwd"),
            require_str(thread, "path"),
            require_str(thread, "cliVersion"),
        ) else {
            return None;
        };
        let payload = json!({
            "thread_id": tid,
            "cwd": cwd,
            "rollout_path": path,
            "cli_version": cli_version,
        });
        Some(
            self.event(EventKind::SessionStart, payload)
                .with_source_event_id(sid(tid, "thread_started")),
        )
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
                    .with_source_event_id(tool_call_source_event_id(tid, item_id))
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

        // Aborted boundary: the terminal carries items:[] even for
        // in-flight items, which never get their own item/completed. Synthesize
        // a terminal for each still-open item of this turn, from what we tracked
        // live (our own observed ids, never cross-resume equality). A clean
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

        // The usage this turn accumulated is CONSUMED here: a live terminal is the
        // one place the running total becomes a fact.
        let usage = self
            .latest_usage
            .remove(&(tid.to_string(), turn_id.to_string()));
        out.extend(self.turn_terminal_events(tid, turn_id, turn, status, usage));

        out
    }

    /// The facts a **terminal turn** produces, whether the terminal was observed live
    /// (`turn/completed`) or described by a `thread/resume` answer.
    ///
    /// One mapping, two callers, deliberately: the whole dedup story across a re-attach
    /// is that the turn a link recovers from an answer carries the same key AND the
    /// same payload as the one it would have recorded live. Two copies of this
    /// rendering would be two chances for that to stop being true silently.
    ///
    /// `usage` is passed in rather than read here because the two callers own it
    /// differently: the live terminal **consumes** the running total, while planning a
    /// seed may only read it — nothing may be forgotten until the facts are durable.
    fn turn_terminal_events(
        &self,
        tid: &str,
        turn_id: &str,
        turn: &Value,
        status: &str,
        usage: Option<Value>,
    ) -> Vec<PendingEvent> {
        let mut out = Vec::new();

        // The one authoritative token-usage fact for this turn — the latest
        // cumulative total we saw, emitted once as the turn closes.
        if let Some(usage) = usage {
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
                .with_source_event_id(turn_terminal_source_event_id(tid, turn_id))
                .with_turn_id(Some(turn_id.to_string())),
        );

        // A non-null turn error is surfaced as its own fact: a failed turn's error object
        // (`fixtures/codex/resume-failed-turn-0.164.0.json`).
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

    /// The `item/started` snapshot of one still-open item, scoped to its thread.
    ///
    /// **The only reader is the approval observer, and only for a `fileChange`.**
    /// That family's `requestApproval` carries no content at all — measured
    /// `reason: null`, `grantRoot: null`, nothing else — while its `item/started`
    /// carries `changes[].{path, kind, diff}` and arrives two frames earlier
    /// (`fixtures/codex/file-change.jsonl`, frames 17 and 19). So the content a
    /// human is being asked about lives here, and joining on the item id is how
    /// the card gets it.
    ///
    /// Keyed by `(thread, turn, item)` — the whole of what an `OpenItem` is
    /// stored under, and the same key [`CodexAdapter::close_open`] uses. Reading
    /// on a narrower key than the one the entry is filed by is how a frame ends
    /// up joined against another turn's item, and the request carries every part
    /// of it (`threadId`, `turnId`, `itemId` are all required in both bundles).
    pub(crate) fn open_item(
        &self,
        thread_id: &str,
        turn_id: &str,
        item_id: &str,
    ) -> Option<&Value> {
        self.open
            .iter()
            .find(|o| o.thread_id == thread_id && o.turn_id == turn_id && o.item_id == item_id)
            .map(|o| &o.started)
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

/// The thread-namespaced source-event id: `<thread_id>:<suffix>`. Threads
/// never share the separator-free prefix, so two threads' identical item ids
/// cannot alias after a switch.
fn sid(thread_id: &str, suffix: &str) -> String {
    format!("{thread_id}:{suffix}")
}

/// **The identity of one turn's terminal fact**, thread-namespaced like every
/// other source-event id here.
///
/// Named rather than inlined because it has a second caller that is not writing
/// the fact but *asking whether it was ever written*
/// ([`crate::store::Store::turn_terminal_filed`], through
/// `Connection::attach_from_seed`). A turn id is unique per thread and not per
/// session, so that question is unanswerable without the thread — and the two
/// sites must build the same string or the ask is about a fact nothing files.
pub fn turn_terminal_source_event_id(thread_id: &str, turn_id: &str) -> String {
    sid(thread_id, &format!("turn:{turn_id}"))
}

/// **The identity of a tool item's call**, filed when the item starts. Named for the
/// same reason as [`turn_terminal_source_event_id`]: `Connection::attach_from_seed`
/// asks whether it was filed.
pub fn tool_call_source_event_id(thread_id: &str, item_id: &str) -> String {
    sid(thread_id, &format!("pre:{item_id}"))
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

    // ---- interrupted-turn synthesis ----

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

    // ------------------------------------ seeding from a `thread/resume` answer

    /// The committed answer a post-turn `thread/resume` really returned.
    const POPULATED: &str = include_str!("../../../fixtures/codex/resume-populated-answer.json");
    /// The live notification stream for **that same turn**, captured on the same run.
    const FIRST_TURN: &str = include_str!("../../../fixtures/codex/first-turn.jsonl");
    const POPULATED_THREAD: &str = "01a03652-f207-76e2-b1f5-aece767a3081";
    /// The captured answer's two items, by id.
    const POPULATED_USER_ITEM: &str = "01a03653-012c-70f1-97e0-cc6652384b07";
    const POPULATED_AGENT_ITEM: &str = "msg_0e8a8045ba7befb5dcd62b61f18562e0ad6d7e0d814073e9c1";

    fn populated_result() -> Value {
        serde_json::from_str::<Value>(POPULATED).expect("the captured answer is JSON")["result"]
            .clone()
    }

    fn seed_of(result: &Value) -> ResumeSeed {
        CodexAdapter::new(key())
            .plan_resume_seed(result, POPULATED_THREAD)
            .expect("the captured answer is one this build reads")
    }

    /// One fact, as everything about it that has to match: its dedup key **and** the
    /// content that key will lock in for ever.
    type Fact = (String, String, String, Option<String>, Option<String>);

    fn fact(event: &PendingEvent) -> Fact {
        (
            event.source_event_id.clone().unwrap_or_default(),
            event.kind.as_str().to_string(),
            serde_json::to_string(&event.payload).expect("a payload serializes"),
            event.turn_id.clone(),
            event.item_id.clone(),
        )
    }

    /// Every fact, as a **multiset that cannot collapse**, with its keys proven unique.
    ///
    /// An earlier version of this helper built a `BTreeMap` keyed by
    /// `source_event_id` — which silently discards a second fact carrying the same key,
    /// and a second fact carrying the same key with a *different payload* is exactly
    /// the conflict this comparison exists to catch. The map made the evidence
    /// disappear into the evidence-gatherer.
    fn facts(events: &[PendingEvent]) -> Vec<Fact> {
        let mut keys = std::collections::HashSet::new();
        for event in events {
            let key = event.source_event_id.clone().unwrap_or_default();
            assert!(
                keys.insert(key.clone()),
                "two facts share the dedup key {key}, so one of them can never be \
                 written: {events:?}"
            );
        }
        let mut out: Vec<Fact> = events.iter().map(fact).collect();
        out.sort();
        out
    }

    /// **The seeding path and the live path describe the same turn identically.**
    ///
    /// This is the assertion the whole reconciliation rests on, and it is only possible
    /// because the two fixtures are two views of one real turn: `first-turn.jsonl` is
    /// the notification stream the wire emitted, and `resume-populated-answer.json` is
    /// what `thread/resume` returned about it moments later. If a fact recovered from
    /// the answer carried a different key from the same fact observed live, a
    /// reconnect would double every turn in the timeline; if it carried the same key
    /// and a different payload, whichever arrived second would be silently discarded
    /// and the timeline would depend on the order the daemon happened to see things.
    ///
    /// So: every fact the answer yields must also be one the live stream yields, under
    /// the same key, with a byte-identical payload.
    #[test]
    fn a_seeded_fact_is_the_same_fact_the_live_stream_would_have_recorded() {
        let live = replay(FIRST_TURN);
        let mut seed = seed_of(&populated_result());
        let seeded = seed.take_events();

        let live_facts = facts(&live);
        let seeded_facts = facts(&seeded);

        // **Whole-fact comparison, not key-by-key.** Every recovered fact must appear in
        // the live stream identically — same key, same kind, same payload bytes, same
        // turn and item attribution. Comparing only what a map could hold is what let an
        // earlier version of this test miss a payload conflict.
        for candidate in &seeded_facts {
            assert!(
                live_facts.contains(candidate),
                "the answer recovered a fact the live stream does not produce \
                 identically:\n  recovered: {candidate:?}\n  live: {live_facts:#?}\n\
                 A recovered fact that differs from the observed one in ANY of these — \
                 key, kind, payload, turn, item — is not a recovery. A different key is \
                 a duplicate row; the same key with different content means whichever \
                 arrived second is silently dropped, so the timeline would depend on \
                 whether the link happened to be up."
            );
        }
        assert!(seeded.iter().all(|e| e.source == Source::Codex));

        // And it recovers the whole turn, not a fragment of it: the session identity,
        // both items, and the turn terminal.
        let recovered: Vec<&str> = seeded_facts.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(
            recovered,
            vec![
                "01a03652-f207-76e2-b1f5-aece767a3081:item:01a03653-012c-70f1-97e0-cc6652384b07",
                "01a03652-f207-76e2-b1f5-aece767a3081:item:msg_0e8a8045ba7befb5dcd62b61f18562e0ad6d7e0d814073e9c1",
                "01a03652-f207-76e2-b1f5-aece767a3081:thread_started",
                "01a03652-f207-76e2-b1f5-aece767a3081:turn:01a03652-fe8e-79d2-99f8-2d5e445e6d8d",
            ]
        );
        // The one fact it CANNOT recover, stated rather than left as a silent gap: the
        // token totals arrive as notifications and the answer does not carry them, so a
        // link that was down for the whole turn recovers everything except its usage.
        assert!(
            live.iter().any(|e| e.kind == EventKind::Usage),
            "the live stream does produce a usage fact"
        );
        assert!(
            !seeded.iter().any(|e| e.kind == EventKind::Usage),
            "and the answer must not invent one it was never told"
        );
        assert_eq!(seed.described_turns(), 1);
        assert_eq!(seed.terminal_turns(), 1);
        assert_eq!(seed.running_turns(), 0);
    }

    /// **The session-identity payload does not move when the thread does.**
    ///
    /// `<thread>:thread_started` is minted by two routes — the `thread/started`
    /// broadcast and every resume answer — and the dedup key is **first-wins**, so
    /// whichever arrives first fixes the payload for ever. Any field that changes over
    /// the life of the thread would therefore make the recorded content depend on
    /// arrival order rather than on what happened.
    ///
    /// The committed fixtures cannot catch this on their own: `first-turn.jsonl`'s
    /// announcement and `resume-populated-answer.json` both happen to report
    /// `status: {"type":"idle"}`. So the divergence is constructed here — a thread seen
    /// while a turn is running, against the same thread seen idle — and the payloads
    /// must be byte-identical.
    #[test]
    fn the_session_identity_payload_is_stable_across_mutable_thread_state() {
        let adapter = CodexAdapter::new(key());
        let announced = populated_result()["thread"].clone();
        let mut later = announced.clone();
        // Everything measured to move over a thread's life.
        later["status"] = json!({"type": "active", "activeFlags": []});
        later["preview"] = json!("Reply with the single word ok and nothing else.");
        later["updatedAt"] = json!(1_787_617_999_i64);
        later["recencyAt"] = json!(1_787_617_998_i64);

        let first = adapter
            .thread_identity_event(&announced)
            .expect("an identity fact");
        let second = adapter.thread_identity_event(&later).expect("and another");
        assert_eq!(
            first.source_event_id, second.source_event_id,
            "both routes mint the same key"
        );
        assert_eq!(
            first.payload, second.payload,
            "the session-identity payload moved because the THREAD moved. The dedup key \
             is first-wins, so this makes the recorded fact depend on whether the \
             announcement or a later resume answer reached the store first — the same \
             session would be recorded differently depending on when the daemon \
             happened to be up.\n  announced: {}\n  later:     {}",
            first.payload, second.payload
        );
        // And it still says the things that identify the session.
        assert_eq!(first.payload["thread_id"], POPULATED_THREAD);
        assert_eq!(first.payload["cwd"], "/work/proj");
        assert!(first.payload.get("status").is_none(), "{}", first.payload);
    }

    /// **The parity helper refuses to collapse a conflict.**
    ///
    /// [`facts`] is the instrument every cross-route comparison is read through. An
    /// earlier version built a `BTreeMap` keyed by `source_event_id`, which silently
    /// drops a second fact carrying the same key — and a second fact carrying the same
    /// key with a *different payload* is exactly the conflict those comparisons exist to
    /// catch. The evidence disappeared into the evidence-gatherer.
    #[test]
    #[should_panic(expected = "share the dedup key")]
    fn the_parity_helper_refuses_to_collapse_two_facts_onto_one_key() {
        let adapter = CodexAdapter::new(key());
        let one = adapter
            .event(EventKind::AgentMessage, json!({"text": "first"}))
            .with_source_event_id("th:item:same".to_string());
        let two = adapter
            .event(EventKind::AgentMessage, json!({"text": "second"}))
            .with_source_event_id("th:item:same".to_string());
        facts(&[one, two]);
    }

    /// **A held usage total is DISCARDED by a recovered terminal, never promoted.**
    ///
    /// Asserting the opposite is an easy mistake, and the defect it would bless is worth
    /// spelling out. `latest_usage` holds the newest cumulative total the adapter has
    /// *seen*. A turn whose completion it missed is precisely a turn whose final total
    /// it never saw — what it holds is some mid-turn snapshot U1, while the turn really
    /// ended at U2. The dedup key `usage:<turn>` is **first-wins**, so seeding U1 would
    /// take that key permanently and no later observation could ever correct it: not a
    /// gap, a durable wrong number.
    ///
    /// So a recovered turn contributes no usage fact at all, and the stale total is
    /// dropped rather than left to be promoted by some later terminal. The missing
    /// usage is the legible gap — the same disposition as the tool residual.
    #[test]
    fn a_held_usage_total_is_discarded_by_a_recovered_terminal_never_promoted() {
        let mut adapter = CodexAdapter::new(key());
        let turn = "01a03652-fe8e-79d2-99f8-2d5e445e6d8d";
        // A mid-turn snapshot: the totals so far, not the turn's final total.
        adapter.ingest(&json!({
            "method": "thread/tokenUsage/updated",
            "params": {"threadId": POPULATED_THREAD, "turnId": turn,
                       "tokenUsage": {"total": {"totalTokens": 4_000}}}
        }));
        let mut seed = adapter
            .plan_resume_seed(&populated_result(), POPULATED_THREAD)
            .expect("accepted");
        let events = seed.take_events();
        assert!(
            !events.iter().any(|e| e.kind == EventKind::Usage),
            "a recovered terminal must NOT promote the total it happens to be holding: \
             it is a mid-turn snapshot, and usage:<turn> is first-wins, so it would \
             shadow the real final total for ever: {events:?}"
        );

        // And the stale total is dropped, not left to be promoted later.
        adapter.apply_resume_seed(&seed);
        let late = adapter.ingest(&json!({
            "method": "turn/completed",
            "params": {"threadId": POPULATED_THREAD,
                       "turn": {"id": turn, "status": "completed", "items": []}}
        }));
        assert!(
            !late.iter().any(|e| e.kind == EventKind::Usage),
            "the stale total must be gone, not waiting to be emitted by the next \
             terminal that comes along: {late:?}"
        );
    }

    /// **A turn whose completion WAS observed live keeps its usage** — the discard above
    /// is scoped to what a recovery cannot know, and must not cost the live path its
    /// one authoritative total.
    #[test]
    fn a_live_terminal_still_emits_the_usage_it_observed() {
        let events = replay(FIRST_TURN);
        let usage = events
            .iter()
            .find(|e| e.kind == EventKind::Usage)
            .expect("the live path emits the turn's usage at its terminal");
        assert_eq!(usage.payload["total"]["totalTokens"], 12211);
    }

    /// **A running turn's items are never read, and what was observed of it survives.**
    ///
    /// Measured on a real 0.147: the `items[]` of an `inProgress` turn carry
    /// placeholder ids (`item-1`, `item-2`) while the live wire is emitting the real
    /// ones — the same turn, resumed twice, reported placeholders while it ran and the
    /// real ids once it finished. So the answer may confirm that such a turn is alive
    /// and may say nothing else about it.
    ///
    /// Both halves are asserted here, because they fail in opposite directions: reading
    /// the ids INVENTS facts, and dropping the open item LOSES the ones already
    /// observed.
    #[test]
    fn a_running_turn_confirms_liveness_and_contributes_nothing_else() {
        let mut adapter = CodexAdapter::new(key());
        let turn = "01a03652-fe8e-79d2-99f8-2d5e445e6d8d";
        // Watched live: a message opened, and text streamed into it.
        adapter.ingest(&json!({
            "method": "item/started",
            "params": {"item": {"type": "agentMessage", "id": "msg_REAL", "text": ""},
                       "threadId": POPULATED_THREAD, "turnId": turn}
        }));
        adapter.ingest(&json!({
            "method": "item/agentMessage/delta",
            "params": {"threadId": POPULATED_THREAD, "turnId": turn,
                       "itemId": "msg_REAL", "delta": "half a sen"}
        }));

        // The answer, with that turn still running and its items renamed.
        let mut result = populated_result();
        let running = &mut result["thread"]["turns"][0];
        running["status"] = json!("inProgress");
        running["completedAt"] = Value::Null;
        running["items"] = json!([
            {"type": "userMessage", "id": "item-1", "content": []},
            {"type": "agentMessage", "id": "item-2", "text": "half a sen"},
        ]);

        let mut seed = adapter
            .plan_resume_seed(&result, POPULATED_THREAD)
            .expect("a running turn is a shape this build reads");
        assert_eq!(seed.described_turns(), 1);
        assert_eq!(seed.terminal_turns(), 0);
        assert_eq!(seed.running_turns(), 1);
        let events = seed.take_events();
        // Only the session identity. No item fact, no turn terminal.
        assert_eq!(
            events
                .iter()
                .map(|e| e.source_event_id.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec![format!("{POPULATED_THREAD}:thread_started")],
            "a running turn contributes no fact of its own: its ids are placeholders \
             and its terminal has not happened"
        );

        // MERGE, never replace: the item is still open and still carries what streamed.
        adapter.apply_resume_seed(&seed);
        let out = adapter.ingest(&json!({
            "method": "turn/completed",
            "params": {"threadId": POPULATED_THREAD,
                       "turn": {"id": turn, "status": "interrupted", "items": []}}
        }));
        let synthesized = out
            .iter()
            .find(|e| e.item_id.as_deref() == Some("msg_REAL"))
            .expect("the item the answer confirmed alive is still open, under its REAL id");
        assert!(
            serde_json::to_string(&synthesized.payload)
                .unwrap()
                .contains("half a sen"),
            "an item confirmed still running keeps everything that was observed of it; \
             the answer only vouches for its turn: {synthesized:?}"
        );
        assert!(
            !out.iter().any(|e| e.item_id.as_deref() == Some("item-1")
                || e.item_id.as_deref() == Some("item-2")),
            "a placeholder id must never reach a fact: {out:?}"
        );
    }

    /// **An open item the answer does not confirm is dropped WITHOUT a terminal.**
    ///
    /// It may have finished while the link was down — in which case the answer just
    /// described its real terminal and that fact is already durable — or it may have
    /// vanished from history altogether. Synthesizing one from an id the answer
    /// did not vouch for would be inventing a result for something that may never have
    /// produced one.
    #[test]
    fn an_unconfirmed_open_item_is_dropped_without_a_terminal() {
        let mut adapter = CodexAdapter::new(key());
        // An exec left open in a turn the answer reports FINISHED, and which the
        // answer's item list does not mention at all.
        adapter.ingest(&json!({
            "method": "item/started",
            "params": {"item": {"type": "commandExecution", "id": "exec-GONE",
                                "status": "inProgress", "command": "sleep 1"},
                       "threadId": POPULATED_THREAD,
                       "turnId": "01a03652-fe8e-79d2-99f8-2d5e445e6d8d"}
        }));
        let mut seed = adapter
            .plan_resume_seed(&populated_result(), POPULATED_THREAD)
            .expect("accepted");
        assert!(
            !seed
                .take_events()
                .iter()
                .any(|e| e.item_id.as_deref() == Some("exec-GONE")),
            "the answer describes no such item, so no fact may be built for it"
        );
        adapter.apply_resume_seed(&seed);

        // The turn terminal that would have synthesized it now finds nothing to.
        let out = adapter.ingest(&json!({
            "method": "turn/completed",
            "params": {"threadId": POPULATED_THREAD,
                       "turn": {"id": "01a03652-fe8e-79d2-99f8-2d5e445e6d8d",
                                "status": "interrupted", "items": []}}
        }));
        assert!(
            !out.iter()
                .any(|e| e.item_id.as_deref() == Some("exec-GONE")),
            "an unconfirmed open item is forgotten silently, never terminalized from an \
             identity the answer did not vouch for: {out:?}"
        );
    }

    /// **Another thread's in-flight state is not this answer's business.**
    #[test]
    fn a_seed_leaves_another_threads_open_items_alone() {
        let mut adapter = CodexAdapter::new(key());
        adapter.ingest(&json!({
            "method": "item/started",
            "params": {"item": {"type": "commandExecution", "id": "exec-B",
                                "status": "inProgress"},
                       "threadId": "th_OTHER", "turnId": "turn_B"}
        }));
        let seed = adapter
            .plan_resume_seed(&populated_result(), POPULATED_THREAD)
            .expect("accepted");
        adapter.apply_resume_seed(&seed);
        let out = adapter.ingest(&json!({
            "method": "turn/completed",
            "params": {"threadId": "th_OTHER",
                       "turn": {"id": "turn_B", "status": "interrupted", "items": []}}
        }));
        assert!(
            out.iter()
                .any(|e| e.kind == EventKind::ToolResult && e.item_id.as_deref() == Some("exec-B")),
            "a resume answer about one thread may not reach into another's open items"
        );
    }

    /// **Planning mutates nothing**, which is what makes a failed attach safe: the
    /// caller records the facts first and applies only when every one of them is
    /// durable, so a plan that is never applied has to leave the adapter exactly as it
    /// was for the retry to produce the same plan.
    #[test]
    fn planning_a_seed_changes_nothing_until_it_is_applied() {
        let mut adapter = CodexAdapter::new(key());
        let turn = "01a03652-fe8e-79d2-99f8-2d5e445e6d8d";
        adapter.ingest(&json!({
            "method": "item/started",
            "params": {"item": {"type": "commandExecution", "id": "exec-1",
                                "status": "inProgress"},
                       "threadId": POPULATED_THREAD, "turnId": turn}
        }));
        adapter.ingest(&json!({
            "method": "thread/tokenUsage/updated",
            "params": {"threadId": POPULATED_THREAD, "turnId": turn,
                       "tokenUsage": {"total": {"totalTokens": 7}}}
        }));

        // Plan twice, apply neither. Identical plans, because nothing moved.
        let first = seed_keys(&adapter);
        let second = seed_keys(&adapter);
        assert_eq!(first, second, "planning is pure");

        // And the state it read is still there to be acted on.
        let out = adapter.ingest(&json!({
            "method": "turn/completed",
            "params": {"threadId": POPULATED_THREAD,
                       "turn": {"id": turn, "status": "interrupted", "items": []}}
        }));
        assert!(
            out.iter().any(|e| e.item_id.as_deref() == Some("exec-1")),
            "the open item survived an unapplied plan"
        );
        assert!(
            out.iter().any(|e| e.kind == EventKind::Usage),
            "and so did the usage total"
        );
    }

    fn seed_keys(adapter: &CodexAdapter) -> Vec<String> {
        let mut seed = adapter
            .plan_resume_seed(&populated_result(), POPULATED_THREAD)
            .expect("accepted");
        seed.take_events()
            .iter()
            .map(|e| e.source_event_id.clone().unwrap_or_default())
            .collect()
    }

    // -------------------- a FINISHED tool-bearing turn is history, not a running turn

    /// The reconstructed answer that broke a production session on 2026-09-08. See its
    /// row in `fixtures/README.md` for exactly which parts are captured and which are
    /// assembled: the envelope and both item shapes are real 0.153 captures, the
    /// arrangement into one answer is not.
    const RESUME_AFTER_TOOL_TURN: &str =
        include_str!("../../../fixtures/codex/resume-after-tool-turn-0.153.4.jsonl");
    /// The thread that answer is about — the 2026-09-08 trace's.
    const TOOL_TURN_THREAD: &str = "01a0807a-1c69-73e3-91db-a06724eeb479";
    /// The completed `commandExecution` it carries, by id.
    const TOOL_TURN_EXEC_ITEM: &str = "exec-cf7b67c7-3a19-4dd8-a9a6-6f243db33bd4";

    /// The `result` of that answer's `s2c` response line.
    fn tool_turn_result() -> Value {
        let response = RESUME_AFTER_TOOL_TURN
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<Value>(line).expect("a captured frame is JSON"))
            .find(|frame| frame["dir"] == "s2c")
            .expect("the fixture carries the response");
        response["frame"]["result"].clone()
    }

    /// **The tool item of the captured tool-turn answer, changed by `mutate`, costs its
    /// own facts and nothing else**: the answer is still read, the item mints neither a
    /// `ToolCall` nor a `ToolResult` — a first-wins key taken by a guess is permanent —
    /// and the rest of its turn and the turn terminal still seed.
    fn the_tool_item_alone_is_not_read(what: &str, mutate: &dyn Fn(&mut Value)) {
        let mut result = tool_turn_result();
        mutate(&mut result);
        let mut seed = CodexAdapter::new(key())
            .plan_resume_seed(&result, TOOL_TURN_THREAD)
            .unwrap_or_else(|| {
                panic!(
                    "{what} must cost the ITEM, not the answer — the answer is otherwise \
                     the captured one and the leg must survive it"
                )
            });
        assert_eq!(
            seed.terminal_turns(),
            2,
            "{what}: the answer is still read, both turns"
        );
        let events = seed.take_events();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, EventKind::ToolCall | EventKind::ToolResult)),
            "{what} must mint no tool fact: {events:?}"
        );
        for expected in [
            EventKind::UserMessage,
            EventKind::Reasoning,
            EventKind::AgentMessage,
        ] {
            assert!(
                events.iter().any(|e| e.kind == expected),
                "{what}: {expected:?} belongs to the same turn and must still seed: {events:?}"
            );
        }
        assert!(
            events.iter().any(|e| e.source_event_id.as_deref()
                == Some(
                    turn_terminal_source_event_id(
                        TOOL_TURN_THREAD,
                        "01a0807a-4d63-7ae4-be16-5f3f7c4eaf32"
                    )
                    .as_str()
                )),
            "{what}: the turn terminal is not the item's to lose: {events:?}"
        );
    }

    /// **A FINISHED turn that ran a command is READ, and its tool outcome is minted
    /// from the answer's own `status` rather than defaulted.**
    ///
    /// This is the production defect, stated as a test. The reader used to refuse any
    /// turn carrying a `commandExecution` or a `fileChange` **outright**, running or
    /// finished — so a link that attached mid-turn, re-asked when the turn ended, and
    /// was told about the very tool item it had joined across could not read the reply.
    /// The refusal is the STOP-AND-AMEND verdict, which ends the leg; the reconnect
    /// asked the same durable history and was told the same thing. It ran 70 epochs in
    /// 18 minutes — 2026-09-08, 10:04-10:22Z — and the phone had no Stop and no compose
    /// for the rest of the session.
    ///
    /// A finished turn is **history**. Its ids are the real ones (that is what
    /// `RESUME_TURN_COMPLETED` means), so nothing here is a guess, and it cannot change
    /// what the link believes is running. What was genuinely unsafe — and stays
    /// refused, in the case below — is a tool item whose own outcome is not yet decided.
    #[test]
    fn a_finished_turn_that_ran_a_command_is_read_and_its_outcome_is_not_defaulted() {
        let result = tool_turn_result();
        let seed = CodexAdapter::new(key())
            .plan_resume_seed(&result, TOOL_TURN_THREAD)
            .expect("a finished tool-bearing turn is history this build can read");

        // **Nothing is running.** Both turns in the answer are `completed`, so the link
        // must come out of this holding no open turn at all — the state the production
        // link could never reach, because the answer was refused before it was read.
        assert_eq!(
            seed.running_turns(),
            0,
            "every turn the answer describes has finished; a link left holding a \
             running turn here would show a Stop for a turn that ended"
        );
        assert_eq!(
            seed.terminal_turns(),
            2,
            "the answer describes the whole durable history — the earlier message turn \
             and the tool-bearing one the link attached across"
        );

        let mut seed = seed;
        let events = seed.take_events();
        let tool = events
            .iter()
            .find(|e| e.kind == EventKind::ToolResult)
            .unwrap_or_else(|| panic!("the completed command must mint a ToolResult: {events:?}"));
        assert_eq!(
            tool.source_event_id.as_deref(),
            Some(format!("{TOOL_TURN_THREAD}:post:{TOOL_TURN_EXEC_ITEM}").as_str()),
            "keyed exactly as the LIVE path keys the same item, which is the whole \
             reason a recovery dedups instead of doubling: {tool:?}"
        );
        // **Read out of the answer, never defaulted.** `tool_result_payload` falls back
        // to `"completed"` for an item with no `status`, so a payload that merely *says*
        // completed proves nothing on its own — the case below is what proves this one
        // came from the wire.
        assert_eq!(
            tool.payload.get("status").and_then(Value::as_str),
            Some("completed"),
            "the status the answer stated: {tool:?}"
        );
        assert_eq!(
            tool.payload.get("exit_code").and_then(Value::as_i64),
            Some(0),
            "and the exit code it stated with it: {tool:?}"
        );
        assert_eq!(
            tool.payload.get("tool").and_then(Value::as_str),
            Some("command_execution"),
            "{tool:?}"
        );
        assert_eq!(
            tool.payload.get("command").and_then(Value::as_str),
            Some("/bin/zsh -lc 'touch /work/cc-e2e/marker.txt'"),
            "{tool:?}"
        );

        // The rest of the turn is recovered too, which is what the follow-up resume
        // exists for: these items finished before the link subscribed.
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        for expected in [
            EventKind::UserMessage,
            EventKind::Reasoning,
            EventKind::AgentMessage,
        ] {
            assert!(
                events.iter().any(|e| e.kind == expected),
                "{expected:?} is part of the same finished turn: {kinds:?}"
            );
        }
    }

    /// **A running turn is alive whatever items it holds, and none of them is read.**
    ///
    /// Codex's own answer to a resume while a turn waits on an approval after a tool
    /// ran — captured on 0.164.0 — and the same running turn holding a malformed, an
    /// undecided and a well-formed tool item: each is one running turn and the
    /// session's identity, never an item fact. Refusing any of these is a reconnect loop
    /// that keeps a Codex approval off the phone.
    #[test]
    fn a_running_turn_holding_a_tool_item_is_alive_and_mints_nothing() {
        let adapter = CodexAdapter::new(key());
        let alive = |what: &str, result: &Value, thread: &str| {
            let mut seed = adapter
                .plan_resume_seed(result, thread)
                .unwrap_or_else(|| panic!("{what} must be read as a running turn"));
            assert_eq!(seed.running_turns(), 1, "{what}");
            assert_eq!(seed.terminal_turns(), 0, "{what}");
            let events = seed.take_events();
            assert_eq!(
                events
                    .iter()
                    .map(|e| e.source_event_id.clone().unwrap_or_default())
                    .collect::<Vec<_>>(),
                vec![format!("{thread}:thread_started")],
                "{what}: nothing but the session identity"
            );
        };

        let captured: Value = serde_json::from_str(include_str!(
            "../../../fixtures/codex/resume-running-tool-turn-0.164.0.json"
        ))
        .expect("the captured answer is JSON");
        let result = &captured["result"];
        let thread = result["thread"]["id"]
            .as_str()
            .expect("the answer names its thread");
        let turn = &result["thread"]["turns"][0];
        assert_eq!(
            turn["status"], "inProgress",
            "the capture is of a running turn"
        );
        assert!(
            turn["items"]
                .as_array()
                .expect("the turn lists its items")
                .iter()
                .any(|item| item["type"] == "commandExecution"),
            "the capture holds a tool item inside the running turn"
        );
        alive("codex 0.164.0's answer mid-approval", result, thread);

        for tool in ["commandExecution", "fileChange"] {
            for (what, item) in [
                (
                    "states none of its fields",
                    json!({"type": tool, "id": "tool-1"}),
                ),
                (
                    "is still running",
                    json!({"type": tool, "id": "tool-1", "status": "inProgress",
                           "command": "/bin/zsh -lc 'true'", "cwd": "/work",
                           "commandActions": [], "changes": []}),
                ),
                (
                    "is complete",
                    json!({"type": tool, "id": "tool-1", "status": "completed",
                           "command": "/bin/zsh -lc 'true'", "cwd": "/work",
                           "commandActions": [], "changes": []}),
                ),
            ] {
                let mut result = populated_result();
                let running = &mut result["thread"]["turns"][0];
                running["status"] = json!("inProgress");
                running["completedAt"] = Value::Null;
                running["durationMs"] = Value::Null;
                running["items"] = json!([item]);
                alive(
                    &format!("a running turn whose {tool} {what}"),
                    &result,
                    POPULATED_THREAD,
                );
            }
        }
    }

    /// **Such a turn is read once it finishes.** Codex 0.164.0's answer after the
    /// approved command ran, on a second thread driven the same way: one finished turn
    /// holding the sandboxed attempt that `failed`, the approved one that `completed`, and
    /// the messages around them. Both commands are history and both become facts — the
    /// shape of the follow-up the link asks for when a running turn it attached to ends.
    #[test]
    fn the_answer_after_an_approved_tool_turn_is_read_whole() {
        let captured: Value = serde_json::from_str(include_str!(
            "../../../fixtures/codex/resume-after-approved-tool-turn-0.164.0.json"
        ))
        .expect("the captured answer is JSON");
        let result = &captured["result"];
        let thread = result["thread"]["id"]
            .as_str()
            .expect("the answer names its thread");
        let tools: Vec<(&str, &str)> = result["thread"]["turns"][0]["items"]
            .as_array()
            .expect("the turn lists its items")
            .iter()
            .filter(|item| item["type"] == "commandExecution")
            .map(|item| {
                (
                    item["id"].as_str().expect("an item id"),
                    item["status"].as_str().expect("a status"),
                )
            })
            .collect();
        assert_eq!(
            tools.iter().map(|(_, status)| *status).collect::<Vec<_>>(),
            ["failed", "completed"],
            "the capture is the failed attempt and the approved run"
        );

        let mut seed = CodexAdapter::new(key())
            .plan_resume_seed(result, thread)
            .expect("a finished turn with settled tool items is read");
        assert_eq!(seed.running_turns(), 0);
        assert_eq!(seed.terminal_turns(), 1);
        let keys: Vec<String> = seed
            .take_events()
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect();
        for (id, _) in tools {
            assert!(
                keys.iter().any(|key| key.contains(id)),
                "the command {id} becomes a fact: {keys:?}"
            );
        }
    }

    /// A capture holding a turn's live frames beside Codex's answer about it.
    fn captured(fixture: &str) -> Value {
        serde_json::from_str(fixture).expect("the capture is JSON")
    }

    /// What the LIVE path records for `frames`, fed in order to one fresh adapter.
    fn live_facts(frames: &Value) -> Vec<Fact> {
        let mut adapter = CodexAdapter::new(key());
        frames
            .as_array()
            .expect("the capture lists its live frames")
            .iter()
            .flat_map(|frame| adapter.ingest(frame))
            .map(|event| fact(&event))
            .collect()
    }

    /// What the reader recovers from `answer`, a `thread/resume` response.
    fn seeded_facts(answer: &Value) -> Vec<Fact> {
        let result = &answer["result"];
        let thread = result["thread"]["id"]
            .as_str()
            .expect("the answer names its thread");
        CodexAdapter::new(key())
            .plan_resume_seed(result, thread)
            .unwrap_or_else(|| panic!("an answer about {thread} this build must read"))
            .take_events()
            .iter()
            .map(fact)
            .collect()
    }

    fn of_kind(facts: &[Fact], kind: &EventKind) -> Vec<Fact> {
        facts
            .iter()
            .filter(|fact| fact.1 == kind.as_str())
            .cloned()
            .collect()
    }

    /// **A turn stopped mid-reply is read for what has settled, and nothing else.**
    ///
    /// Codex 0.164.0's answer through the live TUI's app-server, asked right after Esc:
    /// the stopped turn's prompt still carries the placeholder `item-1`, and the partial
    /// reply is absent. The answer is read; the placeholder mints nothing; the turn
    /// terminal is the one the live path records for the same stop.
    #[test]
    fn a_turn_stopped_mid_reply_is_read_while_its_ids_are_placeholders() {
        let capture = captured(include_str!(
            "../../../fixtures/codex/resume-interrupted-turn-0.164.0.json"
        ));
        let stopped = &capture["answer"]["result"]["thread"]["turns"][1];
        assert_eq!(stopped["status"], "interrupted", "the capture is of a stop");
        assert_eq!(
            stopped["items"][0]["id"], "item-1",
            "the capture holds the placeholder"
        );

        let seeded = seeded_facts(&capture["answer"]);
        assert!(
            !seeded
                .iter()
                .any(|fact| fact.4.as_deref() == Some("item-1") || fact.0.contains("item-1")),
            "a placeholder id mints nothing: {seeded:?}"
        );
        let live = live_facts(&capture["live"]);
        let stop = |facts: &[Fact]| {
            of_kind(facts, &EventKind::TurnComplete)
                .into_iter()
                .filter(|fact| fact.3.as_deref() == stopped["id"].as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(stop(&seeded), stop(&live), "the terminal is the live one");
        assert_eq!(stop(&seeded).len(), 1);
    }

    /// **A stopped turn's items are read once they have settled.**
    ///
    /// The same stop, on a turn running `sleep 60`. Right after Esc the live answer holds
    /// the prompt as `item-1` and the command `inProgress`: neither is read. Asked again
    /// after a later turn and once the shell had finished, the answer holds both under
    /// their live ids, and both are read — the prompt as exactly the fact the live path
    /// recorded, the command with the outcome the answer states.
    #[test]
    fn a_stopped_turn_is_read_once_it_has_settled() {
        let capture = captured(include_str!(
            "../../../fixtures/codex/resume-interrupted-command-0.164.0.json"
        ));
        let live = live_facts(&capture["live"]);
        let command = capture["live"]
            .as_array()
            .expect("live frames")
            .iter()
            .find(|frame| frame["params"]["item"]["type"] == "commandExecution")
            .and_then(|frame| frame["params"]["item"]["id"].as_str())
            .expect("the live command id")
            .to_string();

        let right_after = seeded_facts(&capture["answerRightAfter"]);
        assert!(
            !right_after
                .iter()
                .any(|fact| fact.4.as_deref() == Some("item-1")
                    || fact.4.as_deref() == Some(command.as_str())),
            "neither the placeholder prompt nor the undecided command is read: {right_after:?}"
        );
        let stop = of_kind(&live, &EventKind::TurnComplete);
        assert_eq!(stop.len(), 1, "the live path recorded the stop");
        assert!(
            right_after.contains(&stop[0]),
            "the stopped turn's terminal is the live one, read right away: {right_after:?}"
        );

        let settled = seeded_facts(&capture["answerAfterTheCommandEnded"]);
        let prompt = of_kind(&live, &EventKind::UserMessage);
        assert_eq!(prompt.len(), 1, "the live path recorded the prompt");
        assert!(
            settled.contains(&prompt[0]),
            "the settled prompt is the live fact, key and payload: {settled:?}"
        );
        let outcome = settled
            .iter()
            .find(|fact| {
                fact.1 == EventKind::ToolResult.as_str()
                    && fact.4.as_deref() == Some(command.as_str())
            })
            .unwrap_or_else(|| panic!("the ended command is read: {settled:?}"));
        let payload: Value = serde_json::from_str(&outcome.2).expect("payload JSON");
        assert_eq!(payload["status"], "completed", "{payload}");
        assert_eq!(payload["exit_code"], 0, "{payload}");

        // The payload check holds inside a stopped turn too.
        let mut retyped = capture["answerAfterTheCommandEnded"].clone();
        for turn in retyped["result"]["thread"]["turns"]
            .as_array_mut()
            .expect("turns")
        {
            for item in turn["items"].as_array_mut().expect("items") {
                if item["id"] == command.as_str() {
                    item["exitCode"] = json!("0");
                }
            }
        }
        let retyped = seeded_facts(&retyped);
        assert!(
            !retyped
                .iter()
                .any(|fact| fact.4.as_deref() == Some(command.as_str())),
            "a command whose exit code is not a number mints nothing: {retyped:?}"
        );
        assert!(retyped.contains(&prompt[0]), "the rest of the turn is read");
    }

    /// **A failed turn is read, and its error is the live one.** Codex 0.164.0 on a model
    /// it does not serve: the answer's terminal fields equal the live `turn/completed`'s,
    /// so the `TurnComplete` and `Error` facts are byte-equal to the live path's.
    #[test]
    fn a_failed_turn_is_read_and_its_error_is_the_live_one() {
        let capture = captured(include_str!(
            "../../../fixtures/codex/resume-failed-turn-0.164.0.json"
        ));
        assert_eq!(
            capture["answer"]["result"]["thread"]["turns"][0]["status"],
            "failed"
        );
        let seeded = seeded_facts(&capture["answer"]);
        let live = live_facts(&capture["live"]);
        for kind in [EventKind::TurnComplete, EventKind::Error] {
            let expected = of_kind(&live, &kind);
            assert_eq!(expected.len(), 1, "{kind:?} on the live path");
            assert_eq!(
                of_kind(&seeded, &kind),
                expected,
                "{kind:?} equals the live fact"
            );
        }
        assert_eq!(of_kind(&seeded, &EventKind::UserMessage).len(), 1);
    }

    /// **A stopped or failed last turn whose items are still under placeholder ids is
    /// owed a follow-up; nothing else is.** Each answer is a real codex 0.164.0 capture:
    /// through the live TUI's app-server, except the failed turn's, from a bare one.
    #[test]
    fn a_stopped_last_turn_under_placeholders_is_reported_unsettled() {
        let unsettled = |answer: &Value| {
            let result = &answer["result"];
            CodexAdapter::new(key())
                .plan_resume_seed(result, result["thread"]["id"].as_str().expect("a thread"))
                .expect("a captured answer this build reads")
                .unsettled_turn_id()
                .map(str::to_string)
        };
        let last_turn = |answer: &Value| {
            answer["result"]["thread"]["turns"]
                .as_array()
                .and_then(|turns| turns.last())
                .and_then(|turn| turn["id"].as_str())
                .map(str::to_string)
        };

        let stopped_mid_reply = captured(include_str!(
            "../../../fixtures/codex/resume-interrupted-turn-0.164.0.json"
        ));
        assert_eq!(
            unsettled(&stopped_mid_reply["answer"]),
            last_turn(&stopped_mid_reply["answer"]),
            "stopped mid-reply, its prompt under `item-1`"
        );
        let stopped_command = captured(include_str!(
            "../../../fixtures/codex/resume-interrupted-command-0.164.0.json"
        ));
        assert_eq!(
            unsettled(&stopped_command["answerRightAfter"]),
            last_turn(&stopped_command["answerRightAfter"]),
            "stopped while a command ran: an earlier stopped turn with real ids is not it"
        );
        assert_eq!(
            unsettled(&stopped_command["answerAfterTheCommandEnded"]),
            None,
            "a later turn has run and the stopped turn's ids have settled"
        );
        let failed = captured(include_str!(
            "../../../fixtures/codex/resume-failed-turn-0.164.0.json"
        ));
        assert_eq!(
            unsettled(&failed["answer"]),
            None,
            "a failed turn whose prompt carries its real id"
        );
        let follow_up = captured(include_str!(
            "../../../fixtures/codex/resume-follow-up-turn-0.164.0.json"
        ));
        assert_eq!(
            unsettled(&follow_up["answerMidTurn"]),
            None,
            "a running last turn is owed as running, not as unsettled"
        );
        // A completed turn whose ids never settle — the shape of a codex 0.146.0 thread —
        // is not owed: no later turn names them.
        let mut completed = stopped_mid_reply["answer"].clone();
        let turn = completed["result"]["thread"]["turns"]
            .as_array_mut()
            .and_then(|turns| turns.last_mut())
            .expect("a last turn");
        turn["status"] = json!("completed");
        turn["error"] = Value::Null;
        turn["completedAt"] = json!(1_792_962_000);
        turn["durationMs"] = json!(1000);
        assert_eq!(unsettled(&completed), None);
    }

    /// **A stop the link missed is applied as finished, not as running.** One adapter
    /// watches a turn start a command and holds its token usage, then its connection drops
    /// and the turn is stopped; the answer the link reads on reconnecting reports the turn
    /// `interrupted`. The turn is not running, its open command is let go (the phone
    /// closes it at the turn's end), its held usage is a mid-turn snapshot and is
    /// discarded, and nothing is minted for the placeholder prompt or the undecided
    /// command.
    #[test]
    fn a_stop_the_link_missed_is_applied_as_finished() {
        let capture = captured(include_str!(
            "../../../fixtures/codex/resume-interrupted-command-0.164.0.json"
        ));
        let live = capture["live"].as_array().expect("live frames");
        let thread = live[0]["params"]["threadId"]
            .as_str()
            .expect("thread")
            .to_string();
        let turn = live[0]["params"]["turn"]["id"]
            .as_str()
            .expect("turn")
            .to_string();
        let command = live
            .iter()
            .find_map(|frame| {
                (frame["params"]["item"]["type"] == "commandExecution")
                    .then(|| frame["params"]["item"]["id"].as_str())
                    .flatten()
            })
            .expect("the live command")
            .to_string();

        let mut adapter = CodexAdapter::new(key());
        for frame in live
            .iter()
            .filter(|frame| frame["method"] != "turn/completed")
        {
            adapter.ingest(frame);
        }
        adapter.ingest(&json!({"method": "thread/tokenUsage/updated", "params": {
            "threadId": thread, "turnId": turn, "tokenUsage": {"total": {"totalTokens": 123}}
        }}));
        assert!(adapter.open_item(&thread, &turn, &command).is_some());

        let mut seed = adapter
            .plan_resume_seed(&capture["answerRightAfter"]["result"], &thread)
            .expect("the answer after the stop is read");
        assert_eq!(seed.running_turns(), 0, "a stopped turn is not running");
        let events = seed.take_events();
        assert!(
            !events.iter().any(|e| e.kind == EventKind::Usage
                || e.item_id.as_deref() == Some("item-1")
                || e.item_id.as_deref() == Some(command.as_str())),
            "no usage snapshot, placeholder prompt or undecided command: {events:?}"
        );
        adapter.apply_resume_seed(&seed);
        assert!(
            adapter.open_item(&thread, &turn, &command).is_none(),
            "the stopped turn's open command is let go"
        );
        assert!(adapter.latest_usage.is_empty(), "the snapshot is discarded");
    }

    /// **A stopped or failed turn's terminal fields are read as codex's schema allows
    /// them, and no further.** The times may be null — measured on sessions that ended
    /// mid-turn — or absent; a time that is not an integer, or an error that is not an
    /// object, is a shape nobody has seen and refuses the answer.
    #[test]
    fn a_stopped_or_failed_turn_s_terminal_fields_follow_codex_s_schema() {
        let read = |fixture: &str, turn: usize, mutate: &dyn Fn(&mut Value)| {
            let mut answer = captured(fixture)["answer"].clone();
            mutate(&mut answer["result"]["thread"]["turns"][turn]);
            let thread = answer["result"]["thread"]["id"]
                .as_str()
                .expect("thread")
                .to_string();
            CodexAdapter::new(key())
                .plan_resume_seed(&answer["result"], &thread)
                .map(|mut seed| seed.take_events())
        };
        let stopped = include_str!("../../../fixtures/codex/resume-interrupted-turn-0.164.0.json");
        let failed = include_str!("../../../fixtures/codex/resume-failed-turn-0.164.0.json");

        let events = read(stopped, 1, &|turn| {
            turn["completedAt"] = Value::Null;
            turn["durationMs"] = Value::Null;
        })
        .expect("null times on a stopped turn are read");
        let terminal = events
            .iter()
            .rfind(|e| e.kind == EventKind::TurnComplete)
            .expect("the stopped turn's terminal");
        assert_eq!(terminal.payload["completed_at"], Value::Null);
        assert_eq!(terminal.payload["duration_ms"], Value::Null);
        assert!(
            read(stopped, 1, &|turn| {
                let turn = turn.as_object_mut().expect("turn");
                turn.remove("completedAt");
                turn.remove("durationMs");
            })
            .is_some(),
            "absent times on a stopped turn are read"
        );
        for (what, time) in [
            ("a string", json!("1792962050")),
            ("a fraction", json!(1.5)),
        ] {
            assert!(
                read(stopped, 1, &|turn| turn["completedAt"] = time.clone()).is_none(),
                "{what} for completedAt is refused"
            );
            assert!(
                read(failed, 0, &|turn| turn["durationMs"] = time.clone()).is_none(),
                "{what} for durationMs is refused"
            );
        }
        assert!(
            read(failed, 0, &|turn| turn["error"] = json!("model_not_found")).is_none(),
            "an error that is not an object is refused"
        );
        assert!(
            read(failed, 0, &|turn| turn["error"] = Value::Null).is_some(),
            "a failed turn with a null error is read, as the schema allows"
        );
        assert!(
            read(stopped, 1, &|turn| turn["error"] =
                json!({"message": "stopped"}))
            .is_some(),
            "an error object on a stopped turn is read"
        );
    }

    /// **An id is unique among the facts it would become, not among items not read.**
    /// A skipped item takes no key, so sharing an id with a read one refuses nothing.
    #[test]
    fn an_item_not_read_takes_no_id() {
        let mut result = populated_result();
        result["thread"]["turns"][0]["items"]
            .as_array_mut()
            .expect("items")
            .push(json!({"type": "webSearch", "id": POPULATED_AGENT_ITEM, "query": "codex"}));
        let events = CodexAdapter::new(key())
            .plan_resume_seed(&result, POPULATED_THREAD)
            .expect("a skipped item sharing an id refuses nothing")
            .take_events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.item_id.as_deref() == Some(POPULATED_AGENT_ITEM))
                .map(|e| e.kind.clone())
                .collect::<Vec<_>>(),
            vec![EventKind::AgentMessage],
            "the read item keeps its fact"
        );
    }

    /// **A web search costs that item alone.** Codex 0.164.0's answer after a turn that
    /// searched the web: the `webSearch` item mints nothing, and the prompt, both replies
    /// and the terminal are read.
    #[test]
    fn a_web_search_costs_that_item_alone() {
        let answer = captured(include_str!(
            "../../../fixtures/codex/resume-web-search-0.164.0.json"
        ));
        let items = answer["result"]["thread"]["turns"][0]["items"]
            .as_array()
            .expect("the turn lists its items");
        let search = items
            .iter()
            .find(|item| item["type"] == "webSearch")
            .and_then(|item| item["id"].as_str())
            .expect("the capture holds the search");
        let seeded = seeded_facts(&answer);
        assert!(
            !seeded.iter().any(|fact| fact.4.as_deref() == Some(search)),
            "the search mints nothing: {seeded:?}"
        );
        assert_eq!(of_kind(&seeded, &EventKind::UserMessage).len(), 1);
        assert_eq!(of_kind(&seeded, &EventKind::AgentMessage).len(), 2);
        assert_eq!(of_kind(&seeded, &EventKind::TurnComplete).len(), 1);
    }

    /// **Only codex's own placeholder form is skipped**: `item-` and digits, nothing
    /// more and nothing less.
    #[test]
    fn only_codex_s_placeholder_form_is_skipped() {
        let adapter = CodexAdapter::new(key());
        let minted = |id: &str| {
            let mut result = populated_result();
            for item in result["thread"]["turns"][0]["items"]
                .as_array_mut()
                .expect("items")
                .iter_mut()
            {
                if item["type"] == "agentMessage" {
                    item["id"] = json!(id);
                }
            }
            adapter
                .plan_resume_seed(&result, POPULATED_THREAD)
                .expect("the answer is read either way")
                .take_events()
                .iter()
                .any(|event| event.item_id.as_deref() == Some(id))
        };
        for placeholder in ["item-1", "item-228"] {
            assert!(!minted(placeholder), "{placeholder} is a placeholder");
        }
        for real in ["item-", "item-12x", "item_1", "xitem-1", "Item-1"] {
            assert!(minted(real), "{real} is not codex's placeholder form");
        }
    }

    /// **What a FINISHED turn's tool item must state to be read.** A settled outcome and
    /// every field its payload copies; one that does not costs its own facts and nothing
    /// else.
    #[test]
    fn a_tool_item_whose_outcome_is_undecided_costs_that_item() {
        // **The item's own outcome is not decided.** `tool_result_payload` defaults a
        // missing `status` to `"completed"`, on a FIRST-WINS key — so an item without
        // one would persist a success that never happened and hold the key against the
        // real terminal for ever. `status` is required on this variant by codex 0.153's
        // app-server schema, so an answer lacking it is malformed
        // rather than merely unmeasured.
        the_tool_item_alone_is_not_read("a commandExecution with no status at all", &|r| {
            r["thread"]["turns"][1]["items"][2]
                .as_object_mut()
                .expect("the tool item is an object")
                .remove("status");
        });
        the_tool_item_alone_is_not_read("a commandExecution whose status is not a string", &|r| {
            r["thread"]["turns"][1]["items"][2]["status"] = json!(7);
        });
        the_tool_item_alone_is_not_read("a commandExecution still in progress", &|r| {
            r["thread"]["turns"][1]["items"][2]["status"] = json!("inProgress");
        });
        the_tool_item_alone_is_not_read(
            "a commandExecution whose status is not in the enum",
            &|r| {
                r["thread"]["turns"][1]["items"][2]["status"] = json!("cancelled");
            },
        );

        // The schema's other required fields for this variant. Each is copied straight
        // into the payload by `tool_call_payload`/`tool_result_payload`, so a missing or
        // retyped one lands as `null` on a first-wins key.
        the_tool_item_alone_is_not_read("a commandExecution with no command", &|r| {
            r["thread"]["turns"][1]["items"][2]
                .as_object_mut()
                .expect("the tool item is an object")
                .remove("command");
        });
        the_tool_item_alone_is_not_read("a commandExecution whose cwd is not a string", &|r| {
            r["thread"]["turns"][1]["items"][2]["cwd"] = json!({"path": "/work"});
        });
        the_tool_item_alone_is_not_read(
            "a commandExecution whose commandActions is not an array",
            &|r| {
                r["thread"]["turns"][1]["items"][2]["commandActions"] = json!("touch");
            },
        );

        // `fileChange` is the same rule on the other variant: `changes` and a terminal
        // `status` are what the schema requires of it.
        the_tool_item_alone_is_not_read("a fileChange with no changes", &|r| {
            r["thread"]["turns"][1]["items"][2] = json!({
                "type": "fileChange", "id": "patch-1", "status": "completed"
            });
        });
        the_tool_item_alone_is_not_read("a fileChange still in progress", &|r| {
            r["thread"]["turns"][1]["items"][2] = json!({
                "type": "fileChange", "id": "patch-1", "status": "inProgress", "changes": []
            });
        });

        // And an item type nobody has measured is not read either. `mcpToolCall`
        // requires a `status` too, which is exactly why it is worth naming here:
        // carrying the required field is not what earns admission.
        the_tool_item_alone_is_not_read("an mcpToolCall, which nothing has measured", &|r| {
            r["thread"]["turns"][1]["items"][2] = json!({
                "type": "mcpToolCall", "id": "mcp-1", "status": "completed"
            });
        });
    }

    /// **A `fileChange` that has landed is read, on the same rule.**
    ///
    /// Kept as its own case rather than folded into the refusal loop above: the two
    /// variants have different required fields, and a guard that admitted only the one
    /// the production trace happened to carry would pass every test here but still
    /// refuse the answer the first real patch turn produces.
    #[test]
    fn a_finished_turn_that_changed_a_file_is_read_too() {
        let mut result = tool_turn_result();
        result["thread"]["turns"][1]["items"][2] = json!({
            "type": "fileChange",
            "id": "patch-b2a1",
            "status": "completed",
            // The MEASURED member shape: `kind` is an object, not a bare word
            // (`fixtures/codex/file-change.jsonl` frames 17 and 22). The phone reads
            // `kind["type"]` and defaults a non-object to `"update"`, so a bare string
            // here would relabel the change — see [`seeded_tool_payload_is_readable`].
            "changes": [{
                "path": "/work/cc-e2e/marker.txt",
                "kind": {"type": "add", "move_path": null},
                "diff": "+marker\n",
            }],
        });
        let mut seed = CodexAdapter::new(key())
            .plan_resume_seed(&result, TOOL_TURN_THREAD)
            .expect("a finished fileChange is history this build can read");
        assert_eq!(seed.running_turns(), 0);
        let events = seed.take_events();
        let tool = events
            .iter()
            .find(|e| e.kind == EventKind::ToolResult)
            .unwrap_or_else(|| panic!("the applied patch must mint a ToolResult: {events:?}"));
        assert_eq!(
            tool.payload.get("tool").and_then(Value::as_str),
            Some("file_change"),
            "{tool:?}"
        );
        assert_eq!(
            tool.payload.get("status").and_then(Value::as_str),
            Some("completed"),
            "stated by the answer, not defaulted: {tool:?}"
        );
    }

    /// **A MALFORMED TOOL PAYLOAD COSTS THAT ITEM'S FACT AND NOTHING ELSE.**
    ///
    /// The pair of verdicts, asserted together, because separating them is the whole
    /// point of [`seeded_tool_payload_is_readable`] existing beside
    /// [`seeded_item_shape_is_measured`]:
    ///
    ///   * the answer is still **accepted** — every other item of that turn, and the
    ///     turn terminal, are seeded, and the leg is kept. Refusing the whole answer
    ///     for one bad field is the 2026-09-08 disposition that cost 70 epochs over
    ///     18 minutes, reintroduced through a different door;
    ///   * that item mints **no `ToolResult`**. The key is first-wins, so a payload
    ///     built from a wrong-typed value would occupy the key the real live fact
    ///     would have taken and hold it, uncorrectable, for ever.
    #[test]
    fn a_malformed_tool_payload_costs_that_item_and_not_the_answer() {
        let adapter = CodexAdapter::new(key());
        // Each mutation is the captured answer with ONE value retyped — the only kind
        // of near-miss worth testing, since a reader that had quietly widened would
        // still reject nonsense.

        // `commandExecution` — the three fields the payload copies out of it.
        the_tool_item_alone_is_not_read("an exitCode that is a string", &|r| {
            r["thread"]["turns"][1]["items"][2]["exitCode"] = json!("0");
        });
        the_tool_item_alone_is_not_read("an exitCode a completed item does not state", &|r| {
            r["thread"]["turns"][1]["items"][2]["exitCode"] = Value::Null;
        });
        the_tool_item_alone_is_not_read("a durationMs that is a string", &|r| {
            r["thread"]["turns"][1]["items"][2]["durationMs"] = json!("0ms");
        });
        the_tool_item_alone_is_not_read("a durationMs a completed item does not state", &|r| {
            r["thread"]["turns"][1]["items"][2]
                .as_object_mut()
                .expect("the tool item is an object")
                .remove("durationMs");
        });
        the_tool_item_alone_is_not_read("an aggregatedOutput that is not text", &|r| {
            r["thread"]["turns"][1]["items"][2]["aggregatedOutput"] = json!({"stdout": "hi"});
        });

        // `fileChange` — every member of `changes[]`, on the three keys the phone reads.
        let patch = |change: Value| {
            json!({
                "type": "fileChange",
                "id": "patch-b2a1",
                "status": "completed",
                "changes": [change],
            })
        };
        the_tool_item_alone_is_not_read("a change that is not an object at all", &|r| {
            r["thread"]["turns"][1]["items"][2] = patch(json!("/work/hello.txt"));
        });
        the_tool_item_alone_is_not_read("a change with no path", &|r| {
            r["thread"]["turns"][1]["items"][2] =
                patch(json!({"kind": {"type": "update"}, "diff": "@@\n"}));
        });
        the_tool_item_alone_is_not_read("a change with no diff", &|r| {
            r["thread"]["turns"][1]["items"][2] =
                patch(json!({"path": "/work/hello.txt", "kind": {"type": "update"}}));
        });
        the_tool_item_alone_is_not_read("a change whose kind is a bare word", &|r| {
            r["thread"]["turns"][1]["items"][2] =
                patch(json!({"path": "/work/hello.txt", "kind": "update", "diff": "@@\n"}));
        });

        // **And the mirror image, or the assertions above prove nothing.** The two
        // shapes a `null` is MEASURED in are read, not refused: a completed command
        // that printed nothing, and the two number fields on a status nobody has
        // captured — where the live path writes null for the same item, so refusing
        // would make the seeded fact differ from the live one under one key.
        let read = |what: &str, mutate: &dyn Fn(&mut Value)| {
            let mut result = tool_turn_result();
            mutate(&mut result);
            let mut seed = adapter
                .plan_resume_seed(&result, TOOL_TURN_THREAD)
                .unwrap_or_else(|| panic!("{what} is a measured shape"));
            let events = seed.take_events();
            assert!(
                events.iter().any(|e| e.kind == EventKind::ToolResult),
                "{what} must still mint its outcome: {events:?}"
            );
        };
        read("a completed command that printed nothing", &|r| {
            r["thread"]["turns"][1]["items"][2]["aggregatedOutput"] = Value::Null;
        });
        read("a declined command that states no exit code", &|r| {
            let item = r["thread"]["turns"][1]["items"][2]
                .as_object_mut()
                .expect("the tool item is an object");
            item.insert("status".into(), json!("declined"));
            item.insert("exitCode".into(), Value::Null);
            item.insert("durationMs".into(), Value::Null);
        });
    }

    /// **An item this build does not read costs that item alone**, each one the captured
    /// answer with a single item changed: the answer is still read, the turn terminal and
    /// the other items still seed, and the changed item mints nothing — no first-wins key
    /// is taken by a value the live path would not have written. Refusing the answer
    /// instead would refuse the same unchanging history on every reconnect.
    #[test]
    fn an_item_this_build_does_not_read_costs_that_item_alone() {
        let adapter = CodexAdapter::new(key());
        let not_read = |item: &str, what: &str, mutate: &dyn Fn(&mut Value)| {
            let mut result = populated_result();
            mutate(&mut result);
            let mut seed = adapter
                .plan_resume_seed(&result, POPULATED_THREAD)
                .unwrap_or_else(|| panic!("{what} must cost the item, not the answer"));
            assert_eq!(seed.terminal_turns(), 1, "{what}: the turn is still read");
            let events = seed.take_events();
            assert!(
                !events.iter().any(|e| e.item_id.as_deref() == Some(item)),
                "{what}: item {item} must mint nothing: {events:?}"
            );
            assert!(
                events.iter().any(|e| e.kind == EventKind::TurnComplete),
                "{what}: the turn terminal is not the item's to lose: {events:?}"
            );
            for other in [POPULATED_USER_ITEM, POPULATED_AGENT_ITEM] {
                if other != item {
                    assert!(
                        events.iter().any(|e| e.item_id.as_deref() == Some(other)),
                        "{what}: item {other} is not the changed one's to lose: {events:?}"
                    );
                }
            }
        };

        // **A tool item is read only with its own outcome stated.** These two cases pin
        // the near-misses on the 0.147 answer;
        // `a_tool_item_whose_outcome_is_undecided_costs_that_item` pins the whole rule
        // against the reconstructed 0.153 answer that actually carries one.
        //
        // The case retypes a `userMessage` in a FINISHED turn — so the item is a tool
        // item carrying none of the fields the variant requires, `status` included, and
        // it is not read for that: an item that does not state its outcome would be
        // defaulted to `"completed"` by `tool_result_payload` and persist a success that
        // never happened, on a first-wins key. The same item in a RUNNING turn is read
        // for nothing — see `a_running_turn_holding_a_tool_item_is_alive_and_mints_nothing`.
        for tool in ["commandExecution", "fileChange"] {
            not_read(
                POPULATED_USER_ITEM,
                &format!("a {tool} that states none of its required fields"),
                &|r| {
                    r["thread"]["turns"][0]["items"][0]["type"] = json!(tool);
                },
            );
        }

        // Every field the SEEDING path consults is type-checked before a fact is
        // minted, because the key that fact would take is first-wins: a payload
        // defaulted from a wrong-typed field would occupy it ahead of the real live
        // fact and never be corrected.
        not_read(
            POPULATED_AGENT_ITEM,
            "an agentMessage whose text is not a string",
            &|r| {
                let items = r["thread"]["turns"][0]["items"].as_array_mut().unwrap();
                for item in items.iter_mut() {
                    if item["type"] == "agentMessage" {
                        item["text"] = json!({"parts": ["ok"]});
                    }
                }
            },
        );
        not_read(
            POPULATED_AGENT_ITEM,
            "an agentMessage with no text at all",
            &|r| {
                let items = r["thread"]["turns"][0]["items"].as_array_mut().unwrap();
                for item in items.iter_mut() {
                    if item["type"] == "agentMessage" {
                        item.as_object_mut().unwrap().remove("text");
                    }
                }
            },
        );
        not_read(
            POPULATED_USER_ITEM,
            "a userMessage whose content is not an array",
            &|r| {
                let items = r["thread"]["turns"][0]["items"].as_array_mut().unwrap();
                for item in items.iter_mut() {
                    if item["type"] == "userMessage" {
                        item["content"] = json!("Reply with the single word ok");
                    }
                }
            },
        );
        not_read(
            POPULATED_USER_ITEM,
            "a userMessage content part whose text is not a string",
            &|r| {
                let items = r["thread"]["turns"][0]["items"].as_array_mut().unwrap();
                for item in items.iter_mut() {
                    if item["type"] == "userMessage" {
                        item["content"] = json!([{"type": "text", "text": 42}]);
                    }
                }
            },
        );
        not_read("rs_1", "a reasoning summary that is not an array", &|r| {
            let items = r["thread"]["turns"][0]["items"].as_array_mut().unwrap();
            items.push(json!({"type": "reasoning", "id": "rs_1", "summary": "thought"}));
        });
        not_read("rs_2", "a reasoning content that is not an array", &|r| {
            let items = r["thread"]["turns"][0]["items"].as_array_mut().unwrap();
            items.push(json!({"type": "reasoning", "id": "rs_2", "content": "thought"}));
        });
        // Every part must be an OBJECT WITH A STRING TEXT. Each shape below is silently
        // skipped by the join that builds the prompt, so each would record a truncated
        // version of what the user typed — onto a first-wins key.
        not_read(
            POPULATED_USER_ITEM,
            "a userMessage content part that is a bare scalar",
            &|r| {
                let items = r["thread"]["turns"][0]["items"].as_array_mut().unwrap();
                for item in items.iter_mut() {
                    if item["type"] == "userMessage" {
                        item["content"] = json!(["Reply with the single word ok"]);
                    }
                }
            },
        );
        not_read(
            POPULATED_USER_ITEM,
            "a userMessage content part carrying no text at all",
            &|r| {
                let items = r["thread"]["turns"][0]["items"].as_array_mut().unwrap();
                for item in items.iter_mut() {
                    if item["type"] == "userMessage" {
                        item["content"] = json!([{"type": "image", "url": "/tmp/x.png"}]);
                    }
                }
            },
        );
        not_read(
            POPULATED_USER_ITEM,
            "a userMessage where ONE part of several is malformed",
            &|r| {
                let items = r["thread"]["turns"][0]["items"].as_array_mut().unwrap();
                for item in items.iter_mut() {
                    if item["type"] == "userMessage" {
                        item["content"] =
                            json!([{"type": "text", "text": "Reply with "}, {"type": "text"}]);
                    }
                }
            },
        );
        // The top-level `text` alias, which `message_payload` PREFERS over `content` —
        // so a userMessage carrying both renders the field nothing validated, and holds
        // the key with it.
        not_read(
            POPULATED_USER_ITEM,
            "a userMessage carrying a top-level text alias",
            &|r| {
                let items = r["thread"]["turns"][0]["items"].as_array_mut().unwrap();
                for item in items.iter_mut() {
                    if item["type"] == "userMessage" {
                        item["text"] = json!("shadowed prompt");
                    }
                }
            },
        );

        // An item type this build does not read from an answer. The adapter models it
        // perfectly well from a live frame — it becomes an `Other` fact — but nothing has
        // ever compared that fact against the one a live frame produces for the same item,
        // and a first-wins key is the wrong place to discover a disagreement.
        not_read("ws-1", "an item type this build does not read", &|r| {
            let items = r["thread"]["turns"][0]["items"].as_array_mut().unwrap();
            items.push(json!({"type": "webSearch", "id": "ws-1", "query": "codex"}));
        });
    }

    /// **Every answer-level shape this build has not measured is refused**, each one the
    /// captured answer with a single thing changed — which is the only kind of near-miss worth
    /// testing, since a reader that had quietly widened would still reject nonsense.
    #[test]
    fn an_unmeasured_answer_shape_is_refused_rather_than_read() {
        let adapter = CodexAdapter::new(key());
        let refused = |what: &str, mutate: &dyn Fn(&mut Value)| {
            let mut result = populated_result();
            mutate(&mut result);
            assert!(
                adapter
                    .plan_resume_seed(&result, POPULATED_THREAD)
                    .is_none(),
                "{what} must be refused, not read"
            );
        };

        // Identity.
        refused("an answer about another thread", &|r| {
            r["thread"]["id"] = json!("some-other-thread");
        });
        refused("a second field naming a different thread", &|r| {
            r["threadId"] = json!("a-different-thread");
        });
        refused("an answer with no thread at all", &|r| {
            r.as_object_mut().unwrap().remove("thread");
        });

        // Turn shape — the measured particulars.
        refused("a turn status outside codex's schema", &|r| {
            r["thread"]["turns"][0]["status"] = json!("cancelled");
        });
        refused("a turn with no status", &|r| {
            r["thread"]["turns"][0]
                .as_object_mut()
                .unwrap()
                .remove("status");
        });
        refused("a turn whose item list is flagged partial", &|r| {
            r["thread"]["turns"][0]["itemsView"] = json!("summary");
        });
        refused("a turn whose items were not loaded", &|r| {
            r["thread"]["turns"][0]["itemsView"] = json!("notLoaded");
        });
        refused(
            "a turn that does not say how complete its items are",
            &|r| {
                r["thread"]["turns"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("itemsView");
            },
        );
        refused("a turn with no id", &|r| {
            r["thread"]["turns"][0]
                .as_object_mut()
                .unwrap()
                .remove("id");
        });
        refused("a turns[] that is not an array", &|r| {
            r["thread"]["turns"] = json!({"0": "not an array"});
        });
        refused("an answer describing no turn at all", &|r| {
            r["thread"]["turns"] = json!([]);
        });

        // Item shape — checked for EVERY turn, running ones included.
        refused("an item with no type", &|r| {
            r["thread"]["turns"][0]["items"][0]
                .as_object_mut()
                .unwrap()
                .remove("type");
        });
        refused("an item with no id", &|r| {
            r["thread"]["turns"][0]["items"][0]
                .as_object_mut()
                .unwrap()
                .remove("id");
        });
        refused("an item with an empty id", &|r| {
            r["thread"]["turns"][0]["items"][0]["id"] = json!("");
        });
        refused("a malformed item inside a RUNNING turn", &|r| {
            r["thread"]["turns"][0]["status"] = json!("inProgress");
            r["thread"]["turns"][0]["completedAt"] = Value::Null;
            r["thread"]["turns"][0]["items"][0]
                .as_object_mut()
                .unwrap()
                .remove("type");
        });

        // Pagination: a page of history must never attach, because the seed would look
        // complete and be a window, with older turns silently unrecovered.
        refused("a non-null initialTurnsPage", &|r| {
            r["initialTurnsPage"] = json!({"turns": [], "nextCursor": "x"});
        });
        refused("a backwards cursor that is not a string", &|r| {
            r["turnsBackwardsCursor"] = json!({"rolloutOrdinal": 1});
        });
        refused("an items cursor that is not a string", &|r| {
            r["itemsBackwardsCursor"] = json!(7);
        });

        // Ordering: oldest-first is what a reader relies on to know which turn is
        // current, so it is validated rather than assumed.
        // **Fresh ids on the clone, or this case proves nothing.** Cloning a turn keeps
        // its item ids too, and the uniqueness guard below would then refuse the answer
        // before the ordering guard ever saw it — mutation testing showed the ordering
        // check could be deleted with this case still passing.
        refused("turns out of chronological order", &|r| {
            let mut older = r["thread"]["turns"][0].clone();
            older["id"] = json!("01a03652-0000-0000-0000-000000000001");
            older["startedAt"] = json!(1_787_617_000_i64);
            for (n, item) in older["items"]
                .as_array_mut()
                .expect("items")
                .iter_mut()
                .enumerate()
            {
                item["id"] = json!(format!("older-item-{n}"));
            }
            let newer = r["thread"]["turns"][0].clone();
            r["thread"]["turns"] = json!([newer, older]);
        });
        refused("a turn that does not say when it started", &|r| {
            r["thread"]["turns"][0]
                .as_object_mut()
                .unwrap()
                .remove("startedAt");
        });

        // An identity that appears twice in one answer is a contradiction.
        refused("the same turn twice", &|r| {
            let turn = r["thread"]["turns"][0].clone();
            r["thread"]["turns"] = json!([turn.clone(), turn]);
        });
        refused("the same turn reported both finished and running", &|r| {
            let finished = r["thread"]["turns"][0].clone();
            let mut running = finished.clone();
            running["status"] = json!("inProgress");
            running["completedAt"] = Value::Null;
            r["thread"]["turns"] = json!([finished, running]);
        });
        refused("one fact-producing id in two turns", &|r| {
            let mut later = r["thread"]["turns"][0].clone();
            later["id"] = json!("01a03652-0000-0000-0000-000000000002");
            later["startedAt"] = json!(1_787_618_000_i64);
            r["thread"]["turns"]
                .as_array_mut()
                .expect("turns")
                .push(later);
        });
        refused("two fact-producing items sharing one id", &|r| {
            let first = r["thread"]["turns"][0]["items"][0]["id"].clone();
            r["thread"]["turns"][0]["items"][1]["id"] = first;
        });

        // `sessionId` is a second name for the same thread and may not disagree.
        refused("a sessionId naming a different thread", &|r| {
            r["thread"]["sessionId"] = json!("01a03652-dead-beef-0000-000000000000");
        });

        // The turn terminal's own fields, copied verbatim into the payload.
        refused("a completed turn with no completedAt", &|r| {
            r["thread"]["turns"][0]
                .as_object_mut()
                .unwrap()
                .remove("completedAt");
        });
        refused("a completed turn whose completedAt is null", &|r| {
            r["thread"]["turns"][0]["completedAt"] = Value::Null;
        });
        refused("a completed turn whose completedAt is not a number", &|r| {
            r["thread"]["turns"][0]["completedAt"] = json!("2026-08-25T03:30:06Z");
        });
        refused("a completed turn with no durationMs", &|r| {
            r["thread"]["turns"][0]
                .as_object_mut()
                .unwrap()
                .remove("durationMs");
        });
        refused("a completed turn whose durationMs is not a number", &|r| {
            r["thread"]["turns"][0]["durationMs"] = json!("2546ms");
        });
        refused("a completed turn carrying an error", &|r| {
            r["thread"]["turns"][0]["error"] = json!({"message": "the model refused"});
        });

        // The nested immutable identity fields, which are the WHOLE `thread_started`
        // payload. Defaulting one to null would take that key ahead of the correct
        // announcement and hold it.
        for field in ["cwd", "path", "cliVersion"] {
            refused(&format!("a thread whose {field} is missing"), &|r| {
                r["thread"].as_object_mut().unwrap().remove(field);
            });
            refused(&format!("a thread whose {field} is not a string"), &|r| {
                r["thread"][field] = json!({"value": "/work/proj"});
            });
        }

        // **A completed turn whose userMessage carries no content parts is ACCEPTED.**
        //
        // `all()` over an empty array is true, and that is the intended reading: an empty
        // `content` renders as an empty prompt, which is the honest rendering of an empty
        // array rather than a value defaulted out of a wrong type. The other empty-ish
        // cases in this table sit on `inProgress` turns, where the item guard is bypassed
        // entirely — so without this one a `!parts.is_empty()` creeping into the
        // validator would refuse a legitimate answer and nothing here would notice.
        {
            let mut result = populated_result();
            let items = result["thread"]["turns"][0]["items"]
                .as_array_mut()
                .unwrap();
            for item in items.iter_mut() {
                if item["type"] == "userMessage" {
                    item["content"] = json!([]);
                }
            }
            let mut seed = adapter
                .plan_resume_seed(&result, POPULATED_THREAD)
                .expect("an empty content array is a shape this build reads");
            let user = seed
                .take_events()
                .into_iter()
                .find(|e| e.kind == EventKind::UserMessage)
                .expect("the prompt is still a fact");
            assert_eq!(user.payload["text"], "", "an empty prompt renders as empty");
        }

        // The measured item types are all accepted in their measured shapes, so the
        // allowlist above is a list and not a wall.
        for well_formed in [
            json!({"type": "reasoning", "id": "rs_ok", "summary": [], "content": []}),
            json!({"type": "reasoning", "id": "rs_bare"}),
        ] {
            let mut result = populated_result();
            result["thread"]["turns"][0]["items"]
                .as_array_mut()
                .unwrap()
                .push(well_formed.clone());
            assert!(
                adapter
                    .plan_resume_seed(&result, POPULATED_THREAD)
                    .is_some(),
                "a measured item type in its measured shape must be ACCEPTED: {well_formed}"
            );
        }

        // ...and the captured answer itself is accepted, so none of the above passed
        // for the boring reason that everything is refused.
        assert!(
            adapter
                .plan_resume_seed(&populated_result(), POPULATED_THREAD)
                .is_some(),
            "the committed captured answer must be ACCEPTED"
        );
    }

    #[test]
    fn replaying_a_frame_twice_yields_the_same_ids() {
        // Idempotency: a thread/started re-broadcast on reconnect must map to
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
