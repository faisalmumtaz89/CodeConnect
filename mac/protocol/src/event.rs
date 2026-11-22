//! The event log envelope — CodeConnect's source of truth.
//!
//! Non-negotiable properties:
//!   * `seq` is assigned by the daemon at ingest, per-session monotonic, never
//!     by an adapter.
//!   * Events are *facts*, not counter-deltas, so replay is idempotent.
//!   * `(source, source_event_id)` is the dedup key: the same fact arriving
//!     twice (hook + transcript, or a re-scan after restart) consumes one seq.

use serde::{Deserialize, Serialize};

/// Where a fact came from. Higher trust wins when the same fact arrives twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Claude Code hook — structured, first-hand, highest trust. Filed by earlier
    /// builds; read back from their logs.
    Hook,
    /// Session JSONL transcript tail — authoritative for content, lags slightly.
    /// Filed by earlier builds; read back from their logs.
    Transcript,
    /// The daemon itself (resync markers, approval resolutions, link state).
    Daemon,
    /// The Codex app-server notification stream — structured, first-hand, the
    /// single authoritative observation of a Codex session, exactly as
    /// [`Source::Sdk`] is for a hosted Claude run. A Codex session's every timeline fact
    /// carries this source, so the dedup key `(session_uid, source,
    /// source_event_id)` is a clean per-session namespace that can never collide
    /// with a Claude fact (different source *and* different `session_uid`). Its
    /// `source_event_id`s are thread-namespaced so a thread switch inside one
    /// session cannot alias two threads' item ids.
    Codex,
    /// A Claude run driven through the Agent SDK: the messages its agent host
    /// forwards, first-hand and structured, so trusted as [`Source::Hook`] and
    /// [`Source::Codex`] are. Its `source_event_id`s come from
    /// [`sdk_event_id`].
    Sdk,
    /// tmux capture-pane snapshot, from earlier builds. Presence checks only,
    /// never semantics.
    Pty,
    /// A persisted source string this build does not recognise — a fact written
    /// by a newer daemon and read back by an older one. It carries the **lowest**
    /// trust so it can never win dedup against a real hook or transcript fact:
    /// the alternative, which shipped, decoded an unknown source as
    /// [`Source::Daemon`] (trust 3) and let corrupt or future provenance
    /// impersonate the most trusted source there is.
    ///
    /// The daemon never *creates* this — it only arises decoding storage — so it
    /// has no first-hand meaning to preserve, and unlike
    /// [`crate::agent::AgentKind::Unsupported`] it keeps no original string.
    Unknown,
}

impl Source {
    /// 0..3, ordered by how directly the source observed the fact. Dedup keeps
    /// the highest, so a structured hook post beats the same fact recovered by
    /// re-reading the transcript, and both beat anything read off the rendered
    /// screen.
    pub fn trust(self) -> u8 {
        match self {
            Source::Hook => 3,
            Source::Daemon => 3,
            // First-hand structured stream — the highest trust, on a par with a
            // Claude hook. Nothing else observes a Codex session, so this ranking
            // only ever guards against a corrupt/unknown source (trust 0) losing
            // to the real one; it never competes with a Claude source.
            Source::Codex => 3,
            Source::Sdk => 3,
            Source::Transcript => 2,
            Source::Pty => 1,
            Source::Unknown => 0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Hook => "hook",
            Source::Transcript => "transcript",
            Source::Daemon => "daemon",
            Source::Codex => "codex",
            Source::Sdk => "sdk",
            Source::Pty => "pty",
            Source::Unknown => "unknown",
        }
    }
}

/// The `source_event_id` of one event filed from an SDK message: the message's
/// `uuid`, the event's kind and its block position. One message can file several
/// events (a `result` files `turn_complete` and `usage`), and the dedup key keeps
/// only the first of a repeated id, so the message's uuid alone would drop the
/// rest. The same inputs
/// give the same id, so a replayed message dedups against its first filing.
pub fn sdk_event_id(message_uuid: &str, kind: &EventKind, block: usize) -> String {
    format!("{message_uuid}/{}/{block}", kind.as_str())
}

/// The `notification_type` values of the [`EventKind::Notification`] events the
/// engine files itself, beside the ones Claude's own notifications carry.
pub mod notice {
    /// `/clear` reset the conversation; the run's Claude session id changes.
    pub const CONVERSATION_CLEARED: &str = "conversation_cleared";
    /// The content-free line that wakes an idle agent to read its mail.
    pub const MAIL_NUDGE: &str = "mail_nudge";
    /// An app's effort change on a Claude run CodeConnect started, which claude took
    /// (`{effort}`, minor 39): claude echoes nothing for that change, so this records it.
    pub const MODEL_SET: &str = "model_set";
    /// Claude compacted the conversation (`/compact`, or on its own).
    pub const COMPACTED: &str = "compacted";
    /// A Stop cancelled the person's message `request_id` before claude ran it
    /// (`reason: "stopped"`): it was never sent to the model.
    pub const SEND_NOT_SENT: &str = "send_not_sent";
}

/// Item kinds, following the thread -> turn -> item model.
///
/// `Other` is load-bearing: an unknown kind from a newer daemon must survive a
/// round-trip through an older client rather than being dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStart,
    /// The agent's *process* is gone: its agent host or app-server ended.
    /// Reserved for that, and only that — a finished turn is
    /// [`EventKind::TurnComplete`]. Conflating them is what made the phone
    /// render "session ended" after every single reply.
    SessionEnd,
    /// The agent finished a turn. For a hosted Claude run it is filed from the
    /// SDK's `result`, which the run's agent host relays as the turn ends (an
    /// earlier build filed it from the transcript or the Stop hook). The session
    /// is still alive and takes the next message.
    TurnComplete,
    /// A tool is about to run (PreToolUse). Observability, never the gate.
    ToolCall,
    /// A tool finished (PostToolUse).
    ToolResult,
    /// Claude is about to ask a human (PermissionRequest). The money signal.
    ApprovalRequest,
    /// An approval reached a terminal state, whoever answered it.
    ApprovalResolved,
    /// A notice: ccd's own (`model_set`, `mail_nudge`, …), or, from an earlier
    /// build, Claude Code's Notification hook (permission_prompt, idle_prompt, ...).
    Notification,
    UserMessage,
    AgentMessage,
    Reasoning,
    Usage,
    Error,
    /// The daemon could not guarantee a gap-free stream; client must re-subscribe.
    Resync,
    /// Link/liveness fact about *our* observation, never about the agent.
    LinkState,
    #[serde(untagged)]
    Other(String),
}

impl EventKind {
    pub fn as_str(&self) -> &str {
        match self {
            EventKind::SessionStart => "session_start",
            EventKind::SessionEnd => "session_end",
            EventKind::TurnComplete => "turn_complete",
            EventKind::ToolCall => "tool_call",
            EventKind::ToolResult => "tool_result",
            EventKind::ApprovalRequest => "approval_request",
            EventKind::ApprovalResolved => "approval_resolved",
            EventKind::Notification => "notification",
            EventKind::UserMessage => "user_message",
            EventKind::AgentMessage => "agent_message",
            EventKind::Reasoning => "reasoning",
            EventKind::Usage => "usage",
            EventKind::Error => "error",
            EventKind::Resync => "resync",
            EventKind::LinkState => "link_state",
            EventKind::Other(s) => s.as_str(),
        }
    }

    pub fn from_str_lossy(s: &str) -> EventKind {
        match s {
            "session_start" => EventKind::SessionStart,
            "session_end" => EventKind::SessionEnd,
            "turn_complete" => EventKind::TurnComplete,
            "tool_call" => EventKind::ToolCall,
            "tool_result" => EventKind::ToolResult,
            "approval_request" => EventKind::ApprovalRequest,
            "approval_resolved" => EventKind::ApprovalResolved,
            "notification" => EventKind::Notification,
            "user_message" => EventKind::UserMessage,
            "agent_message" => EventKind::AgentMessage,
            "reasoning" => EventKind::Reasoning,
            "usage" => EventKind::Usage,
            "error" => EventKind::Error,
            "resync" => EventKind::Resync,
            "link_state" => EventKind::LinkState,
            other => EventKind::Other(other.to_string()),
        }
    }
}

/// A fact about a session, as stored and as sent to the phone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Daemon-assigned, monotonic and gap-free **per `session_uid`**.
    ///
    /// Per *uid*, not per name: a later run can carry the same name, and
    /// numbering a new run from where a dead one stopped spliced two unrelated
    /// timelines into one. See [`crate::uid`].
    pub seq: u64,
    /// The run's unique identity, minted at spawn and never reused. This is the
    /// event log's key and what a client should store events under.
    ///
    /// Defaulted on decode so an event persisted by an older daemon — which had
    /// no such concept — still parses; the migration fills those in, so an empty
    /// value here means the record predates the migration entirely.
    #[serde(default)]
    pub session_uid: String,
    /// The run's name. A run CodeConnect started is named by its uid.
    pub session_id: String,
    /// RFC3339 UTC, millisecond precision.
    pub ts: String,
    pub kind: EventKind,
    pub payload: serde_json::Value,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}

impl Event {
    pub fn source_trust(&self) -> u8 {
        self.source.trust()
    }
}

/// A fact ready for ingest, before the daemon has assigned a `seq`.
///
/// Constructed against a [`SessionKey`] rather than a bare string so no ingest
/// path can accidentally file a fact under a name instead of a uid — the
/// mistake that would put two runs back in one log.
#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub session_uid: String,
    pub session_id: String,
    pub ts: String,
    pub kind: EventKind,
    pub payload: serde_json::Value,
    pub source: Source,
    pub source_event_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
}

/// The pair every ingest path carries: the identity to file under and the name
/// to render. Keeping them together is what makes "which one is this?"
/// unaskable at the call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKey {
    pub uid: String,
    pub name: String,
}

impl SessionKey {
    pub fn new(uid: impl Into<String>, name: impl Into<String>) -> SessionKey {
        SessionKey {
            uid: uid.into(),
            name: name.into(),
        }
    }
}

impl PendingEvent {
    pub fn new(
        session: &SessionKey,
        kind: EventKind,
        payload: serde_json::Value,
        source: Source,
    ) -> Self {
        PendingEvent {
            session_uid: session.uid.clone(),
            session_id: session.name.clone(),
            ts: crate::time::now_rfc3339(),
            kind,
            payload,
            source,
            source_event_id: None,
            turn_id: None,
            item_id: None,
        }
    }

    pub fn with_source_event_id(mut self, id: impl Into<String>) -> Self {
        self.source_event_id = Some(id.into());
        self
    }

    pub fn with_turn_id(mut self, id: Option<String>) -> Self {
        self.turn_id = id;
        self
    }

    pub fn with_item_id(mut self, id: Option<String>) -> Self {
        self.item_id = id;
        self
    }
}

/// Lifecycle facts, kept separate from `link` so observation never masquerades
/// as agent state: how well we can see a session is our problem, not a claim
/// about what the agent is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// ccd has written the run's row and is bringing its host up, or has sent the
    /// host `start` and has not yet heard how it went.
    Starting,
    Live,
    /// The run was asked to end ([`crate::ws::ClientMessage::StopRun`]) and has not ended
    /// yet: its host is sent `stop` if its link is attached, and is stopped by its process
    /// group if it has not exited 10 s later.
    Stopping,
    Exited,
    /// The run never started: its host did not come up, or ended before saying how the
    /// start went, or its start was refused.
    Failed,
    Unknown,
}

/// How fresh *our observation* is. Never rendered as agent state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Attached,
    Detached,
}

/// **Where this run's Codex control link stands — the one fact that says whether
/// a Stop or a Compose aimed at THIS session can land right now.**
///
/// Not to be confused with [`Link`], which is about how fresh CodeConnect's
/// *observation* of a session is. This is about what an ask can reach, which is why
/// every Claude run reads [`CodexLink::None`] whatever its `link`.
///
/// **Why it exists.** [`crate::ws::Capabilities::codex_interrupt`] and
/// [`crate::ws::Capabilities::codex_compose`] are build facts about the whole
/// daemon; they are true on a connection whose only Codex session cannot be
/// stopped. And [`SessionSummary::codex_thread_id`] reads the same whether or not
/// the run's link is attached, because it is the thread the run's row holds. So
/// until this field a client could not compute, from
/// the fleet, whether one particular session could be stopped — it had to offer
/// the button and let a refusal be the answer, which is the "offered and silently
/// broken" affordance this app's rule forbids.
///
/// **[`CodexLink::Subscribed`] is actuatable, and [`CodexLink::BoundNotStarted`] is
/// actuatable for a COMPOSE alone**: a run whose app-server ended without the daemon
/// stopping it is relaunched by the next message, and has no turn to stop.
/// [`CodexLink::None`] is neither.
///
/// **A refusal still arrives after the fact and still wins.** This is a snapshot
/// of the fleet at the moment it was assembled; the link can move between the
/// summary and the tap. A client renders the daemon's refusal sentence verbatim
/// when that happens rather than treating this field as a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexLink {
    /// **Connected and subscribed to this session's thread.** Frames flow, and this
    /// is the one state in which a stop or a compose reaches the model.
    Subscribed,
    /// **A run CodeConnect started on its engine whose app-server ended without the
    /// daemon stopping it** ("Stopped: its process ended": a crash, or a Mac restart):
    /// a message may be sent into its thread although nothing is subscribed to it,
    /// because nothing runs on it. Its lifecycle stays `exited`, and the person's next
    /// message relaunches its app-server, resumes its thread and is sent there. A run
    /// ended by End run reads [`CodexLink::None`]. A relaunch that cannot be done
    /// refuses the message with nothing sent and leaves the run `exited` and offered
    /// this word again, even when the daemon's own watch found the new app-server gone
    /// first; a daemon that dies during the relaunch leaves it `failed`, read as
    /// [`CodexLink::None`]. The refusal is the last word, as for any compose.
    ///
    /// **Compose only.** Nothing runs on the thread until the message relaunches it, so
    /// there is nothing to stop: a client offers Compose here and does not offer Stop.
    ///
    /// The word is [`crate::PROTOCOL_MINOR`] 20's. A minor-19 client that meets it treats
    /// it as not actuatable, which is what the phone's unrecognised arm already does.
    BoundNotStarted,
    /// **Nothing reaches this run over a Codex link right now.** Every Claude session,
    /// and every Codex run that is neither of the above: before its reconnect, after
    /// one that failed, while it is stopping and once End run ended it. Also any
    /// summary from a daemon predating this field. The
    /// default, so an older daemon's fleet decodes as what it is: nothing here can be
    /// stopped over a Codex link.
    #[default]
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// The run's identity. Subscribe, answer and send_text all accept it, and a
    /// client on `protocol_minor >= 2` should prefer it: two entries in this
    /// list can share a `session_id` when a name has been reused, and only this
    /// tells them apart.
    #[serde(default)]
    pub session_uid: String,
    /// The run's name. A run CodeConnect started is named by its uid.
    pub session_id: String,
    pub cwd: String,
    pub lifecycle: Lifecycle,
    pub link: Link,
    /// Populated once the first hook reveals Claude's own session uuid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    pub last_seq: u64,
    pub created_at: String,
    pub updated_at: String,
    /// Request ids currently awaiting a human. Non-empty == push-worthy.
    #[serde(default)]
    pub blocked_on: Vec<String>,
    /// **What to call this run out loud.** The final component of `cwd` — the
    /// project someone is working in — resolved by the daemon rather than by
    /// each client, so that a surface using it agrees with every other one.
    ///
    /// Empty when `cwd` names no project (empty, `/`, or unusable). Empty is
    /// also what an older daemon sends, since it does not know the field, and a
    /// client should treat both the same way: say it does not know rather than
    /// fall back to [`SessionKey::name`], which is a reused counter (`cc-1`)
    /// and names nothing a human chose.
    ///
    /// A nonempty value is authoritative: a client that has one uses it rather
    /// than re-deriving its own, because two rules produce two names for one
    /// run — which is the state this replaced, where a reader saw one thing on
    /// a list, another on a card, and a third on their lock screen.
    ///
    #[serde(default)]
    pub project_label: String,
    /// Which agent this run hosts. Absent — from any daemon predating the agent
    /// seam — decodes as [`crate::agent::AgentKind::Claude`], which is exactly
    /// what every such run is. This per-session fact is authoritative: a client
    /// scopes what it offers to the agent named here, and falls back to
    /// connection-global behaviour only when it is Claude.
    #[serde(default)]
    pub agent: crate::agent::AgentKind,
    /// The Codex thread this run is attached to, when the agent is Codex. Absent
    /// for Claude and for any daemon predating the seam. Opaque to the client —
    /// carried for correlation and rendering, never parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_thread_id: Option<String>,
    /// **Whether a Stop or a Compose aimed at THIS run can land right now.**
    ///
    /// Resolved from the very same addressee `codex_thread_id` above is resolved
    /// from — one read, two fields — so the thread and the state of the link
    /// carrying it can never disagree within one summary.
    ///
    /// Always sent, never skipped: [`CodexLink::None`] is a fact about this run
    /// (there is no Codex link on it), not an absence of news. Absent — from any
    /// daemon predating this field — decodes as `None`, which is what such a
    /// daemon's fleet is to a client that cannot address any of it.
    ///
    /// **It is not a lifecycle claim, and a client must read `lifecycle` too.**
    /// The two fields answer different questions: this one whether an ask could be
    /// delivered, `lifecycle` whether there is still a run to deliver it to.
    #[serde(default)]
    pub codex_link: CodexLink,
    /// **Every card in this run still waiting on a human, by reference** (minor
    /// 23): the same cards as `blocked_on`, each with the seq of its
    /// `approval_request` event, so a client holding only the newest events can
    /// still fetch the one that carries a card's body. A card whose request event
    /// is not yet in the log is left out; that event follows live.
    ///
    /// Never a card body: three open cards can carry megabytes between them, and
    /// this list rides every `sessions` reply. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<PendingRef>,
    /// The model this run last confirmed, and the event that confirmed it (minor
    /// 23). See [`crate::facts`]. Omitted when nothing has said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Fact>,
    /// The permission mode the main agent last reported, and the event that
    /// reported it (minor 23). See [`crate::facts`]. Omitted when nothing has
    /// said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<Fact>,
    /// The effort this run last took, and the event that recorded it (minor 39): an app's
    /// change ([`notice::MODEL_SET`]) or claude's own receipt for a typed `/effort`, or the
    /// settings a Codex run's Codex names (minor 43). Omitted when none has happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Fact>,
    /// What an app may set this run to (minor 39): a live Claude run CodeConnect started,
    /// once its start is answered, or a live Codex one (minor 43). Omitted for any other run,
    /// before then, once it has ended, and from an older daemon; a client offers the model,
    /// effort and mode controls only where it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menus: Option<RunMenus>,
    /// The name the apps give `model` on a Claude run CodeConnect started that is sent no
    /// `menus` (minor 46: it is not live), by the rule they name a live run's model by: the label
    /// of its start's entry whose value the model is, else of the entry, the default one aside,
    /// that runs it. Omitted for any other run and for a model no entry names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_label: Option<String>,
    /// The branch checked out in `cwd` (minor 31), as git read it when a turn
    /// last ended, when the run registered, or when the daemon started. Absent
    /// outside a repository, on a detached head, before the first read, and from
    /// any older daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Lines added and removed in `cwd` against `HEAD` (minor 31), read at the
    /// same moments as `git_branch`. Tracked files only: a new file git does not
    /// track yet is not counted. Absent outside a repository, before the first
    /// read, and from any older daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_stat: Option<DiffStat>,
    /// The person's next message relaunches this ended run (minor 42): a run CodeConnect
    /// started whose agent's process ended without anyone stopping it, its end reading
    /// "Stopped: its process ended". Never a run End run ended or a start that failed.
    /// Omitted when false, and from an older daemon, whose ended runs stay ended.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relaunchable: bool,
}

/// How much a working tree has changed, in lines. See [`SessionSummary::diff_stat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStat {
    pub added: u64,
    pub removed: u64,
}

/// One open card, named without its body. See [`SessionSummary::pending`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRef {
    pub request_id: String,
    /// The seq of the card's `approval_request` event.
    pub seq: u64,
    /// The card's hold, as the card carries it. Absent on a card that has none,
    /// which is every Codex card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<crate::ws::QuestionHold>,
    /// The turn a Codex card belongs to, which a Stop has to name. Absent on a
    /// Claude card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

/// A fact and the seq of the event that decided it, so a client can keep
/// whichever of this and its own events is newer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub value: String,
    pub seq: u64,
}

/// The settings an app may choose for a run (minor 39), as its agent offers them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMenus {
    pub models: Vec<MenuModel>,
    /// The permission modes an app may set, the agent's own default first: the mode that asks
    /// before acting, which a run starts in when no setting names another (Claude's `default`,
    /// Codex's `auto`, minor 43). The order is CodeConnect's, not the agent's own.
    pub modes: Vec<MenuMode>,
    /// The value in `models` the run's model fact names, for an app to show as chosen (display
    /// only: the run's model is its agent's). A Codex run's is the fact itself, Codex naming
    /// its model by the value its list gives (minor 43). A Claude run's is read off claude's
    /// own start answer: the fact itself,
    /// the alias of claude's receipt of a change (`sonnet (claude-sonnet-5-5)`), or the one model
    /// claude resolves to the fact's id. When two do (`default` and `opus` both resolve to
    /// `claude-opus-5-5`), the alias of the run's newest receipt if it is one of them, else
    /// `default`. Omitted when none matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_model: Option<String>,
}

/// One model of a run's menu: claude's own `value`, its name, what claude says of it, the
/// efforts it takes, and (minor 45) the model id it stands for now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuModel {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub efforts: Vec<String>,
    /// The model id the agent says this entry runs (Claude Code's `resolvedModel`:
    /// `default` and `opus` both `claude-opus-5-5`), so an app names the model a run is on
    /// by a structured field rather than a sentence. Absent when the agent gives none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    /// The effort this entry runs at when none is chosen (minor 47), so an app names and marks it while the run
    /// reports no effort: Codex's own `defaultReasoningEffort`; for Claude, the default Claude Code's documentation
    /// names for the model the entry resolves to. Absent when neither names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

/// One permission mode of a run's menu. `confirm` (minor 40) marks the mode that asks nothing
/// (Claude's `bypassPermissions`): an app raises its confirmation before it sends it, and sends
/// [`crate::ws::ClientMessage::SetMode`] with `confirmed`, without which the daemon refuses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuMode {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// **An ended run's model label (minor 46)**, as the daemon emits it: carried and kept on
    /// a round trip, and left out of the wire when there is none.
    #[test]
    fn an_ended_runs_model_label_round_trips_and_is_omitted_when_absent() {
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/claude/minor-46-wire.json"))
                .unwrap();
        let ended: SessionSummary =
            serde_json::from_value(wire["session_hosted_ended"].clone()).unwrap();
        assert_eq!(ended.model_label.as_deref(), Some("Opus 5.5"));
        assert_eq!(
            serde_json::to_value(&ended).unwrap(),
            wire["session_hosted_ended"]
        );
        let live: SessionSummary =
            serde_json::from_value(wire["session_hosted_live"].clone()).unwrap();
        assert_eq!(live.model_label, None);
        assert_eq!(
            serde_json::to_value(&live).unwrap().get("model_label"),
            None
        );
    }

    #[test]
    fn unknown_kind_round_trips() {
        let kind = EventKind::from_str_lossy("some_future_kind");
        assert_eq!(kind, EventKind::Other("some_future_kind".into()));
        let encoded = serde_json::to_string(&kind).unwrap();
        assert_eq!(encoded, "\"some_future_kind\"");
        let decoded: EventKind = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, kind);
    }

    #[test]
    fn known_kinds_use_snake_case_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&EventKind::ApprovalRequest).unwrap(),
            "\"approval_request\""
        );
        let decoded: EventKind = serde_json::from_str("\"approval_request\"").unwrap();
        assert_eq!(decoded, EventKind::ApprovalRequest);
    }

    #[test]
    fn event_round_trip() {
        let ev = Event {
            seq: 7,
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            session_id: "cc-1".into(),
            ts: "2026-07-30T16:36:58.412Z".into(),
            kind: EventKind::ToolCall,
            payload: json!({"tool_name": "Bash"}),
            source: Source::Hook,
            source_event_id: Some("toolu_1".into()),
            turn_id: None,
            item_id: None,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert_eq!(ev, serde_json::from_str::<Event>(&s).unwrap());
        // Optional fields must not appear when unset — the phone decodes strictly.
        assert!(!s.contains("turn_id"));
        // Both identities ride every event: one to file it under, one to show.
        assert!(s.contains("\"session_uid\""), "{s}");
        assert!(s.contains("\"session_id\":\"cc-1\""), "{s}");
    }

    #[test]
    fn an_event_without_a_uid_still_decodes() {
        // Additive-only: an event persisted before session uids existed must
        // parse rather than fail, and must be visibly un-migrated rather than
        // silently claiming an identity it does not have.
        let ev: Event = serde_json::from_str(
            r#"{"seq":3,"session_id":"cc-1","ts":"t","kind":"tool_call",
                "payload":{},"source":"hook"}"#,
        )
        .unwrap();
        assert_eq!(ev.session_uid, "");
        assert_eq!(ev.session_id, "cc-1");
    }

    #[test]
    fn a_pending_event_carries_both_halves_of_the_key() {
        let key = SessionKey::new("01K1B3XQ8ZC0DE5FGH7JKMNPQR", "cc-1");
        let pending = PendingEvent::new(&key, EventKind::ToolCall, json!({}), Source::Hook);
        assert_eq!(pending.session_uid, key.uid);
        assert_eq!(pending.session_id, "cc-1");
    }

    /// An older daemon does not know the field. Absent decodes as empty, and
    /// empty is what a client renders as "nobody has said" — never the tmux
    /// counter, which is what this whole minor exists to stop showing.
    #[test]
    fn a_summary_from_a_daemon_that_does_not_know_projects_still_decodes() {
        let older = serde_json::json!({
            "session_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "session_id": "cc-1",
            "cwd": "/Users/dev/Aion",
            "lifecycle": "live",
            "link": "detached",
            "last_seq": 3,
            "created_at": "2026-08-07T10:00:00.000Z",
            "updated_at": "2026-08-07T10:00:00.000Z"
        });
        let decoded: SessionSummary = serde_json::from_value(older).expect("decodes");
        assert_eq!(decoded.project_label, "");
        assert_eq!(decoded.session_id, "cc-1", "the run's name survives");
    }

    #[test]
    fn two_runs_of_the_same_name_are_distinguishable_in_a_session_list() {
        // The bug this guards against, expressed as a wire property: a
        // fleet list containing a dead `cc-1` and a live `cc-1` must not be two
        // indistinguishable rows.
        let summary = |uid: &str, lifecycle| SessionSummary {
            session_uid: uid.into(),
            session_id: "cc-1".into(),
            cwd: "/tmp".into(),
            project_label: "tmp".into(),
            lifecycle,
            link: Link::Detached,
            claude_session_id: None,
            transcript_path: None,
            last_seq: 12,
            created_at: "t".into(),
            updated_at: "t".into(),
            blocked_on: Vec::new(),
            agent: crate::agent::AgentKind::Claude,
            codex_thread_id: None,
            codex_link: CodexLink::None,
            pending: Vec::new(),
            model: None,
            permission_mode: None,
            effort: None,
            menus: None,
            model_label: None,
            git_branch: None,
            diff_stat: None,
            relaunchable: false,
        };
        let dead = summary("01K1B3XQ8ZC0DE5FGH7JKMNPQR", Lifecycle::Exited);
        let live = summary("01K1B3XZZZC0DE5FGH7JKMNPQR", Lifecycle::Live);
        assert_eq!(dead.session_id, live.session_id);
        assert_ne!(dead.session_uid, live.session_uid);
        let encoded = serde_json::to_string(&live).unwrap();
        assert!(encoded.contains("session_uid"), "{encoded}");
        assert_eq!(live, serde_json::from_str(&encoded).unwrap());
    }

    #[test]
    fn trust_ranks_hook_above_transcript_above_pty() {
        assert!(Source::Hook.trust() > Source::Transcript.trust());
        assert!(Source::Transcript.trust() > Source::Pty.trust());
    }

    #[test]
    fn the_sdk_source_is_first_hand_and_round_trips() {
        let source: Source =
            serde_json::from_str("\"sdk\"").expect("\"sdk\" is a source this build knows");
        assert_eq!(source.as_str(), "sdk");
        assert_eq!(source.trust(), 3);
        assert_eq!(serde_json::to_string(&source).unwrap(), "\"sdk\"");
    }

    #[test]
    fn an_unknown_source_carries_the_lowest_trust() {
        // The property the store's decode relies on: an unrecognised persisted
        // source can never out-rank a real fact, so it never wins dedup — and in
        // particular it is not the trusted Daemon it used to decode as.
        assert_eq!(Source::Unknown.trust(), 0);
        assert!(Source::Unknown.trust() < Source::Pty.trust());
        assert!(Source::Unknown.trust() < Source::Daemon.trust());
        assert_eq!(Source::Unknown.as_str(), "unknown");
    }

    /// A daemon predating the agent seam sends no `agent`, and every run it
    /// hosts is Claude. Absent must decode as Claude, and a Codex summary must
    /// round-trip its agent and thread id.
    #[test]
    fn a_summary_without_an_agent_decodes_as_claude() {
        use crate::agent::AgentKind;
        let older = serde_json::json!({
            "session_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "session_id": "cc-1",
            "cwd": "/Users/dev/Aion",
            "lifecycle": "live",
            "link": "detached",
            "last_seq": 3,
            "created_at": "t",
            "updated_at": "t"
        });
        let decoded: SessionSummary = serde_json::from_value(older).expect("decodes");
        assert_eq!(decoded.agent, AgentKind::Claude);
        assert_eq!(decoded.codex_thread_id, None);

        let codex = SessionSummary {
            agent: AgentKind::Codex,
            codex_thread_id: Some("th_1".into()),
            ..decoded
        };
        let s = serde_json::to_string(&codex).unwrap();
        assert!(s.contains("\"agent\":\"codex\""), "{s}");
        assert_eq!(codex, serde_json::from_str::<SessionSummary>(&s).unwrap());
    }

    /// `git_branch` and `diff_stat` (minor 31): absent from an older daemon and
    /// decoded as unknown, left off the wire while unknown, and carried exactly
    /// when known.
    #[test]
    fn the_git_fields_are_optional_both_ways() {
        let older = serde_json::json!({
            "session_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "session_id": "cc-1",
            "cwd": "/Users/dev/Aion",
            "lifecycle": "live",
            "link": "detached",
            "last_seq": 3,
            "created_at": "t",
            "updated_at": "t"
        });
        let decoded: SessionSummary = serde_json::from_value(older).expect("decodes");
        assert_eq!(decoded.git_branch, None);
        assert_eq!(decoded.diff_stat, None);
        let encoded = serde_json::to_string(&decoded).unwrap();
        assert!(!encoded.contains("git_branch"), "{encoded}");
        assert!(!encoded.contains("diff_stat"), "{encoded}");

        let known = SessionSummary {
            git_branch: Some("main".into()),
            diff_stat: Some(DiffStat {
                added: 120,
                removed: 7,
            }),
            ..decoded
        };
        let encoded = serde_json::to_value(&known).unwrap();
        assert_eq!(encoded["git_branch"], "main");
        assert_eq!(
            encoded["diff_stat"],
            serde_json::json!({"added": 120, "removed": 7})
        );
        assert_eq!(
            known,
            serde_json::from_value::<SessionSummary>(encoded).unwrap()
        );
    }

    /// **A run's `link` is `attached` or `detached`**, spelled by hand, and `degraded`
    /// and `stale` do not decode.
    #[test]
    fn a_link_is_attached_or_detached() {
        for (link, word) in [(Link::Attached, "attached"), (Link::Detached, "detached")] {
            let encoded = serde_json::to_string(&link).unwrap();
            assert_eq!(encoded, format!("\"{word}\""));
            assert_eq!(serde_json::from_str::<Link>(&encoded).unwrap(), link);
        }
        for gone in ["degraded", "stale"] {
            assert!(
                serde_json::from_str::<Link>(&format!("\"{gone}\"")).is_err(),
                "{gone}"
            );
        }
    }

    /// **A summary's `model` is `{value, seq}`**, as its `permission_mode` and `effort`
    /// are. A summary whose model also carries `source` and `at` still decodes, and is
    /// sent without them.
    #[test]
    fn a_summarys_model_is_its_value_and_seq() {
        let older = serde_json::json!({
            "session_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "session_id": "cc-1",
            "cwd": "/Users/dev/Aion",
            "lifecycle": "live",
            "link": "detached",
            "last_seq": 3,
            "created_at": "t",
            "updated_at": "t",
            "model": {"value": "claude-opus-5", "seq": 3, "source": "command",
                      "at": "2026-11-12T00:00:00.000Z"}
        });
        let decoded: SessionSummary = serde_json::from_value(older).expect("decodes");
        assert_eq!(
            serde_json::to_value(&decoded).unwrap()["model"],
            serde_json::json!({"value": "claude-opus-5", "seq": 3})
        );
    }

    /// **The three words the phone matches on, and the one an older daemon means.**
    ///
    /// `codex_link` is what a client scopes Stop and Compose by, so each of the
    /// three is spelled by hand here rather than trusted to `rename_all`: renaming a
    /// variant would compile, pass, ship, and leave a phone greying a control on a
    /// session that could have been stopped. `bound` and `offline` do not decode. The
    /// default matters just as much — every summary from every daemon below this minor
    /// arrives without the field, and reading that as anything but `none` would offer an
    /// action against a link the daemon cannot even name.
    #[test]
    fn the_codex_link_states_are_exactly_the_three_words_the_phone_matches_on() {
        for (state, word) in [
            (CodexLink::Subscribed, "subscribed"),
            (CodexLink::BoundNotStarted, "bound_not_started"),
            (CodexLink::None, "none"),
        ] {
            let encoded = serde_json::to_string(&state).unwrap();
            assert_eq!(encoded, format!("\"{word}\""));
            assert_eq!(serde_json::from_str::<CodexLink>(&encoded).unwrap(), state);
        }
        for gone in ["bound", "offline"] {
            assert!(
                serde_json::from_str::<CodexLink>(&format!("\"{gone}\"")).is_err(),
                "{gone}"
            );
        }
        assert_eq!(CodexLink::default(), CodexLink::None);

        // A summary from a daemon below this minor: no field at all.
        let older = serde_json::json!({
            "session_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "session_id": "cc-1",
            "cwd": "/Users/dev/Aion",
            "lifecycle": "live",
            "link": "detached",
            "last_seq": 3,
            "created_at": "t",
            "updated_at": "t",
            "agent": "codex"
        });
        let decoded: SessionSummary = serde_json::from_value(older).expect("decodes");
        assert_eq!(
            decoded.codex_link,
            CodexLink::None,
            "a Codex run from a daemon that cannot say where its link stands is not \
             one a client may aim a Stop at"
        );

        // And a summary from this daemon always says, including when the answer is
        // `none` — the field is never skipped, because "no link" is news.
        let encoded = serde_json::to_string(&decoded).unwrap();
        assert!(encoded.contains("\"codex_link\":\"none\""), "{encoded}");
    }

    #[test]
    fn turn_complete_is_distinct_from_session_end() {
        // The bug this exists to prevent: a finished reply rendering as a dead
        // session. They must never share a wire name.
        assert_eq!(EventKind::TurnComplete.as_str(), "turn_complete");
        assert_ne!(EventKind::TurnComplete, EventKind::SessionEnd);
        assert_eq!(
            EventKind::from_str_lossy("turn_complete"),
            EventKind::TurnComplete
        );
        assert_eq!(
            serde_json::to_string(&EventKind::TurnComplete).unwrap(),
            "\"turn_complete\""
        );
    }

    #[test]
    fn every_known_kind_round_trips_through_its_wire_name() {
        // Guards the three parallel lists (variant, as_str, from_str_lossy)
        // against one of them being updated and the others forgotten.
        for kind in [
            EventKind::SessionStart,
            EventKind::SessionEnd,
            EventKind::TurnComplete,
            EventKind::ToolCall,
            EventKind::ToolResult,
            EventKind::ApprovalRequest,
            EventKind::ApprovalResolved,
            EventKind::Notification,
            EventKind::UserMessage,
            EventKind::AgentMessage,
            EventKind::Reasoning,
            EventKind::Usage,
            EventKind::Error,
            EventKind::Resync,
            EventKind::LinkState,
        ] {
            assert_eq!(
                EventKind::from_str_lossy(kind.as_str()),
                kind,
                "{kind:?} does not survive its own wire name"
            );
            let encoded = serde_json::to_string(&kind).unwrap();
            assert_eq!(encoded, format!("\"{}\"", kind.as_str()));
            assert_eq!(serde_json::from_str::<EventKind>(&encoded).unwrap(), kind);
        }
    }
}
