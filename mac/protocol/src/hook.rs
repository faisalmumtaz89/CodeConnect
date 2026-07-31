//! Claude Code hook input/output.
//!
//! The output shapes here are not guesses: they were read out of the zod
//! schema embedded in `claude` 2.1.220 and confirmed against live sessions.
//! Two constraints drive every decision in this file:
//!
//! 1. `hookSpecificOutput` is a **union discriminated by `hookEventName`** whose
//!    members are PreToolUse, UserPromptSubmit, UserPromptExpansion,
//!    SessionStart, PostToolUse, PostToolBatch and Stop/SubagentStop. There is
//!    **no PermissionRequest member.** Emitting one fails schema validation and
//!    Claude then discards the *entire* hook output — so a well-meaning extra
//!    field silently voids a real decision. Never emit it.
//! 2. Only `PreToolUse` renders `permissionDecisionReason` to the operator
//!    ("Hook PreToolUse:Bash requires confirmation for this command: <reason>").
//!    `PermissionRequest` renders neither `permissionDecisionReason` nor
//!    `systemMessage` (measured).
//!
//! Emitting nothing is always safe: the tool call proceeds exactly as it would
//! under plain `claude`. That is the fail-open path cc-hook takes whenever the
//! daemon cannot be reached, and it is why CodeConnect never changes local
//! behaviour when it is down.

use serde::{Deserialize, Serialize};

/// Hook events CodeConnect wires. Anything else round-trips as `Other`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEventName {
    PermissionRequest,
    PreToolUse,
    PostToolUse,
    Notification,
    SessionStart,
    SessionEnd,
    Stop,
    Other(String),
}

impl HookEventName {
    pub fn parse(s: &str) -> HookEventName {
        match s {
            "PermissionRequest" => HookEventName::PermissionRequest,
            "PreToolUse" => HookEventName::PreToolUse,
            "PostToolUse" => HookEventName::PostToolUse,
            "Notification" => HookEventName::Notification,
            "SessionStart" => HookEventName::SessionStart,
            "SessionEnd" => HookEventName::SessionEnd,
            "Stop" => HookEventName::Stop,
            other => HookEventName::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            HookEventName::PermissionRequest => "PermissionRequest",
            HookEventName::PreToolUse => "PreToolUse",
            HookEventName::PostToolUse => "PostToolUse",
            HookEventName::Notification => "Notification",
            HookEventName::SessionStart => "SessionStart",
            HookEventName::SessionEnd => "SessionEnd",
            HookEventName::Stop => "Stop",
            HookEventName::Other(s) => s.as_str(),
        }
    }

    /// True for events where the agent is blocked on our answer, i.e. where
    /// cc-hook waits for the daemon instead of firing and forgetting.
    pub fn is_gate(&self) -> bool {
        matches!(
            self,
            HookEventName::PermissionRequest | HookEventName::PreToolUse
        )
    }

    pub fn maps_to_event_kind(&self) -> crate::event::EventKind {
        use crate::event::EventKind;
        match self {
            HookEventName::PermissionRequest => EventKind::ApprovalRequest,
            HookEventName::PreToolUse => EventKind::ToolCall,
            HookEventName::PostToolUse => EventKind::ToolResult,
            HookEventName::Notification => EventKind::Notification,
            HookEventName::SessionStart => EventKind::SessionStart,
            HookEventName::SessionEnd => EventKind::SessionEnd,
            // `Stop` fires when the agent finishes a *turn*, which happens many
            // times in one session. Mapping it to `SessionEnd`, as an earlier
            // version did, made the phone announce a dead session after every
            // reply; the iOS side had to work around it by reading the raw
            // `hook_event_name` back out of the payload. `TurnComplete` is the
            // fact Claude is actually
            // reporting, and `SessionEnd` is now emitted only by the daemon when
            // the process is genuinely gone.
            HookEventName::Stop => EventKind::TurnComplete,
            HookEventName::Other(s) => EventKind::Other(format!("hook_{s}")),
        }
    }
}

/// The fields of hook stdin CodeConnect reads. Everything is optional because
/// the payload shape differs per event and drifts between Claude versions; the
/// untouched original is always kept as the event payload.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    /// Present on PreToolUse/PostToolUse. **Absent on PermissionRequest** —
    /// which is why the daemon correlates the two by `prompt_id` + tool input.
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub prompt_id: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub permission_suggestions: Option<serde_json::Value>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

impl HookInput {
    pub fn event_name(&self) -> HookEventName {
        HookEventName::parse(self.hook_event_name.as_deref().unwrap_or(""))
    }
}

/// What the daemon tells cc-hook to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Let the call through without a prompt.
    Allow,
    /// Block the call; `reason` is surfaced to the agent.
    Deny,
    /// Force the local operator to decide, showing `reason`.
    Ask,
    /// Emit nothing — behave exactly like plain `claude`. Always safe.
    Passthrough,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDecision {
    pub decision: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HookDecision {
    pub fn passthrough() -> Self {
        HookDecision {
            decision: Decision::Passthrough,
            reason: None,
        }
    }

    pub fn ask(reason: impl Into<String>) -> Self {
        HookDecision {
            decision: Decision::Ask,
            reason: Some(reason.into()),
        }
    }

    /// Render the stdout JSON for this decision on this event, or `None` when
    /// the correct action is to print nothing.
    ///
    /// Anything we cannot express for a given event degrades to `None` rather
    /// than to an invalid shape, because an invalid shape voids the whole
    /// output — a strictly worse failure than staying silent.
    pub fn render(&self, event: &HookEventName) -> Option<String> {
        match (event, self.decision) {
            (_, Decision::Passthrough) => None,

            // PreToolUse: the one gate whose reason string reaches the operator.
            (HookEventName::PreToolUse, decision) => {
                let verdict = match decision {
                    Decision::Allow => "allow",
                    Decision::Deny => "deny",
                    Decision::Ask => "ask",
                    Decision::Passthrough => unreachable!("handled above"),
                };
                let mut inner = serde_json::Map::new();
                inner.insert("hookEventName".into(), "PreToolUse".into());
                inner.insert("permissionDecision".into(), verdict.into());
                if let Some(reason) = &self.reason {
                    inner.insert(
                        "permissionDecisionReason".into(),
                        serde_json::Value::String(reason.clone()),
                    );
                }
                let mut root = serde_json::Map::new();
                root.insert("hookSpecificOutput".into(), inner.into());
                Some(serde_json::Value::Object(root).to_string())
            }

            // PermissionRequest: only the generic top-level `decision` field is
            // schema-valid. `ask` has no representation here, and none is
            // needed — emitting nothing *is* "let the local prompt appear".
            (HookEventName::PermissionRequest, Decision::Allow) => {
                Some(r#"{"decision":"approve"}"#.to_string())
            }
            (HookEventName::PermissionRequest, Decision::Deny) => {
                let mut root = serde_json::Map::new();
                root.insert("decision".into(), "block".into());
                root.insert(
                    "reason".into(),
                    serde_json::Value::String(
                        self.reason
                            .clone()
                            .unwrap_or_else(|| "Denied via CodeConnect".to_string()),
                    ),
                );
                Some(serde_json::Value::Object(root).to_string())
            }
            (HookEventName::PermissionRequest, Decision::Ask) => None,

            // Observability-only events never carry a decision.
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_prints_nothing_for_every_event() {
        for ev in [
            HookEventName::PreToolUse,
            HookEventName::PermissionRequest,
            HookEventName::PostToolUse,
            HookEventName::Notification,
            HookEventName::SessionStart,
            HookEventName::Other("Whatever".into()),
        ] {
            assert_eq!(HookDecision::passthrough().render(&ev), None);
        }
    }

    #[test]
    fn pretooluse_ask_matches_the_verified_shape() {
        let out = HookDecision::ask("CodeConnect: daemon unreachable, deferring to local operator")
            .render(&HookEventName::PreToolUse)
            .expect("PreToolUse ask must render");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let inner = &v["hookSpecificOutput"];
        assert_eq!(inner["hookEventName"], "PreToolUse");
        assert_eq!(inner["permissionDecision"], "ask");
        assert_eq!(
            inner["permissionDecisionReason"],
            "CodeConnect: daemon unreachable, deferring to local operator"
        );
    }

    #[test]
    fn permission_request_never_emits_hook_specific_output() {
        // Regression guard for the schema trap: a PermissionRequest member does
        // not exist in the union, and an unknown member voids the whole output.
        for decision in [Decision::Allow, Decision::Deny, Decision::Ask] {
            let rendered = HookDecision {
                decision,
                reason: Some("r".into()),
            }
            .render(&HookEventName::PermissionRequest);
            if let Some(out) = rendered {
                assert!(
                    !out.contains("hookSpecificOutput"),
                    "PermissionRequest output must stay on top-level fields: {out}"
                );
                let v: serde_json::Value = serde_json::from_str(&out).unwrap();
                let d = v["decision"].as_str().unwrap();
                assert!(d == "approve" || d == "block", "invalid decision {d}");
            }
        }
    }

    #[test]
    fn permission_request_ask_is_silence() {
        assert_eq!(
            HookDecision::ask("x").render(&HookEventName::PermissionRequest),
            None
        );
    }

    #[test]
    fn stop_is_a_turn_boundary_not_a_session_boundary() {
        use crate::event::EventKind;
        assert_eq!(
            HookEventName::Stop.maps_to_event_kind(),
            EventKind::TurnComplete
        );
        // SessionEnd keeps its own meaning; the two must not collapse again.
        assert_eq!(
            HookEventName::SessionEnd.maps_to_event_kind(),
            EventKind::SessionEnd
        );
    }

    #[test]
    fn gate_classification() {
        assert!(HookEventName::PermissionRequest.is_gate());
        assert!(HookEventName::PreToolUse.is_gate());
        assert!(!HookEventName::PostToolUse.is_gate());
        assert!(!HookEventName::Notification.is_gate());
    }

    #[test]
    fn parses_a_real_permission_request_payload() {
        // Verbatim from a live claude 2.1.220 session (fixtures/hooks/).
        let raw = r#"{"session_id":"28708262-7008-4a21-b664-a4db56304eaf",
          "transcript_path":"/tmp/x.jsonl","cwd":"/tmp","prompt_id":"904a7e02",
          "permission_mode":"default","effort":{"level":"high"},
          "hook_event_name":"PermissionRequest","tool_name":"Bash",
          "tool_input":{"command":"touch /private/tmp/ccprobe_touch.txt","description":"d"},
          "permission_suggestions":[{"type":"addDirectories","directories":["/private/tmp"],"destination":"session"}]}"#;
        let input: HookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(input.event_name(), HookEventName::PermissionRequest);
        assert_eq!(input.tool_name.as_deref(), Some("Bash"));
        // The field that is *not* there is the important one.
        assert_eq!(input.tool_use_id, None);
        assert!(input.permission_suggestions.is_some());
    }
}
