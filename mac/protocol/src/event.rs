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
    /// Claude Code hook — structured, first-hand, highest trust.
    Hook,
    /// Session JSONL transcript tail — authoritative for content, lags slightly.
    Transcript,
    /// The daemon itself (resync markers, approval resolutions, link state).
    Daemon,
    /// The Codex app-server notification stream — structured, first-hand, the
    /// single authoritative observation of a Codex session, exactly as
    /// [`Source::Hook`] is for Claude. A Codex session's every timeline fact
    /// carries this source, so the dedup key `(session_uid, source,
    /// source_event_id)` is a clean per-session namespace that can never collide
    /// with a Claude fact (different source *and* different `session_uid`). Its
    /// `source_event_id`s are thread-namespaced (D4) so a thread switch inside one
    /// session cannot alias two threads' item ids.
    Codex,
    /// tmux capture-pane snapshot. Presence checks only, never semantics.
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
            Source::Pty => "pty",
            Source::Unknown => "unknown",
        }
    }
}

/// Item kinds, following the thread -> turn -> item model.
///
/// `Other` is load-bearing: an unknown kind from a newer daemon must survive a
/// round-trip through an older client rather than being dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStart,
    /// The agent's *process* is gone: the supervisor exited or the tmux session
    /// disappeared. Reserved for that, and only that — a finished turn is
    /// [`EventKind::TurnComplete`]. Conflating them is what made the phone
    /// render "session ended" after every single reply.
    SessionEnd,
    /// The agent finished a turn and handed the keyboard back (Stop hook). The
    /// session is still alive and can be typed into.
    TurnComplete,
    /// A tool is about to run (PreToolUse). Observability, never the gate.
    ToolCall,
    /// A tool finished (PostToolUse).
    ToolResult,
    /// Claude is about to ask a human (PermissionRequest). The money signal.
    ApprovalRequest,
    /// An approval reached a terminal state, whoever answered it.
    ApprovalResolved,
    /// Claude Code Notification hook (permission_prompt, idle_prompt, ...).
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
    /// Per *uid*, not per name: `cc-1` is reused by the next session, and
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
    /// The tmux session name (e.g. `cc-1`) — for display and for `codeconnect attach`.
    /// **Not** an identity: it is reassigned when a session exits.
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
    Spawning,
    Live,
    Exited,
    Unknown,
}

/// How fresh *our observation* is. Never rendered as agent state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Attached,
    Degraded,
    Detached,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// The run's identity. Subscribe, answer and send_text all accept it, and a
    /// client on `protocol_minor >= 2` should prefer it: two entries in this
    /// list can share a `session_id` when a name has been reused, and only this
    /// tells them apart.
    #[serde(default)]
    pub session_uid: String,
    /// The tmux session name. Display and `codeconnect attach` only.
    pub session_id: String,
    pub tmux_session: String,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            "tmux_session": "cc-1",
            "cwd": "/Users/dev/Aion",
            "lifecycle": "live",
            "link": "detached",
            "last_seq": 3,
            "created_at": "2026-08-07T10:00:00.000Z",
            "updated_at": "2026-08-07T10:00:00.000Z"
        });
        let decoded: SessionSummary = serde_json::from_value(older).expect("decodes");
        assert_eq!(decoded.project_label, "");
        assert_eq!(decoded.session_id, "cc-1", "the handle survives for attach");
    }

    #[test]
    fn two_runs_of_the_same_name_are_distinguishable_in_a_session_list() {
        // The bug this guards against, expressed as a wire property: a
        // fleet list containing a dead `cc-1` and a live `cc-1` must not be two
        // indistinguishable rows.
        let summary = |uid: &str, lifecycle| SessionSummary {
            session_uid: uid.into(),
            session_id: "cc-1".into(),
            tmux_session: "cc-1".into(),
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
            "tmux_session": "cc-1",
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
