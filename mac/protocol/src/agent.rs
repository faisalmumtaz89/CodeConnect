//! Which agent a session runs.
//!
//! CodeConnect began as a Claude-only tool, so *absence* of this fact means
//! Claude: every row, event and frame written before this type existed is a
//! Claude session, and an older peer that never sends the field must keep
//! meaning that. That is why [`AgentKind::default`] is [`AgentKind::Claude`] and
//! every additive `agent` field is `#[serde(default)]`.
//!
//! The other half of the rule is the dangerous one. An *unrecognised* agent name
//! must never collapse to Claude — a daemon that treated `"gemini"` as Claude
//! would actuate a session it does not understand. So an unknown string is
//! preserved in [`AgentKind::Unsupported`] rather than defaulted away, and every
//! use site that drives actuation is expected to fail closed on it. The variant
//! carries the original string so the fact survives a round-trip through a peer
//! that does not know the name, exactly as [`crate::event::EventKind::Other`]
//! does for event kinds.

use serde::{Deserialize, Serialize};

/// The agent hosting a session.
///
/// Decode rules, both load-bearing:
///   * **absent ⇒ [`AgentKind::Claude`]** (via `#[serde(default)]` at the field).
///   * **unrecognised ⇒ [`AgentKind::Unsupported`]**, never Claude. The string is
///     kept so it can be rendered honestly and round-tripped unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// The default, and what absence means: every session predating this type is
    /// Claude, so an omitted `agent` field decodes here.
    #[default]
    Claude,
    Codex,
    /// An agent name this build does not know. Never treated as Claude: a use
    /// site that could actuate the session must refuse it. Carries the exact
    /// wire string so it survives a round-trip and can be shown to a human.
    #[serde(untagged)]
    Unsupported(String),
}

impl AgentKind {
    /// The wire/storage string. `Unsupported` returns its preserved name, so
    /// `from_str_lossy(k.as_str()) == k` for every value.
    pub fn as_str(&self) -> &str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Unsupported(name) => name.as_str(),
        }
    }

    /// Decode a **present** wire/storage string. Only true absence (an omitted
    /// field, decoded through `#[serde(default)]`, or a `NULL` column handled by
    /// the caller) means Claude. A *present* value that is neither `claude` nor
    /// `codex` — including the empty string — is preserved as
    /// [`AgentKind::Unsupported`], never silently promoted to Claude: an empty or
    /// unrecognised agent that a peer deliberately sent is a fact this build does
    /// not understand, and it fails closed downstream rather than being read as
    /// the one agent every peer can drive.
    pub fn from_str_lossy(value: &str) -> AgentKind {
        match value {
            "claude" => AgentKind::Claude,
            "codex" => AgentKind::Codex,
            other => AgentKind::Unsupported(other.to_string()),
        }
    }

    /// True for the one agent that predates this type. The fail-open default is
    /// keyed off exactly this: only Claude may fall back to connection-global
    /// behaviour when a per-session fact is missing.
    ///
    /// There is deliberately no `is_supported` companion. What a build can drive
    /// is the daemon's authoritative `supported_agents()` list, not a property of
    /// the value — a hardcoded `Claude | Codex` predicate would answer "yes,
    /// Codex" on a build that cannot yet host Codex, which is exactly the
    /// actuation trap the fail-closed registration check exists to prevent.
    pub fn is_claude(&self) -> bool {
        matches!(self, AgentKind::Claude)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_agent_is_claude() {
        // A struct with a `#[serde(default)]` agent field, from JSON that omits
        // it, must land on Claude — the whole additive-compatibility contract.
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default)]
            agent: AgentKind,
        }
        let decoded: Holder = serde_json::from_str("{}").unwrap();
        assert_eq!(decoded.agent, AgentKind::Claude);
        assert!(AgentKind::default().is_claude());
    }

    #[test]
    fn known_agents_use_their_wire_names() {
        assert_eq!(
            serde_json::to_string(&AgentKind::Claude).unwrap(),
            "\"claude\""
        );
        assert_eq!(
            serde_json::to_string(&AgentKind::Codex).unwrap(),
            "\"codex\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"claude\"").unwrap(),
            AgentKind::Claude
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"codex\"").unwrap(),
            AgentKind::Codex
        );
    }

    #[test]
    fn an_unknown_agent_is_preserved_and_never_claude() {
        // The dangerous case: a future agent name must not decode to Claude, and
        // must keep its string so a peer that does not know it round-trips it.
        let decoded: AgentKind = serde_json::from_str("\"gemini\"").unwrap();
        assert_eq!(decoded, AgentKind::Unsupported("gemini".into()));
        assert!(!decoded.is_claude(), "an unknown agent is not Claude");
        let encoded = serde_json::to_string(&decoded).unwrap();
        assert_eq!(encoded, "\"gemini\"");
        assert_eq!(
            serde_json::from_str::<AgentKind>(&encoded).unwrap(),
            decoded
        );
    }

    #[test]
    fn from_str_lossy_round_trips_every_value() {
        for kind in [
            AgentKind::Claude,
            AgentKind::Codex,
            AgentKind::Unsupported("gemini".into()),
        ] {
            assert_eq!(AgentKind::from_str_lossy(kind.as_str()), kind);
        }
        // A **present** empty string is not absence: it is an agent value this
        // build does not understand, so it fails closed as Unsupported rather
        // than being read as Claude. Only true absence (serde default / a NULL
        // column) means Claude, and that is the caller's job, not this decode's.
        assert_eq!(
            AgentKind::from_str_lossy(""),
            AgentKind::Unsupported(String::new())
        );
    }
}
