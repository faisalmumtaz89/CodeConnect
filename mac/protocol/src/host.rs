//! The protocol between ccd and the agent host it runs for each Claude run.
//!
//! One JSON object per line over the run's unix socket, whole lines only. ccd's first
//! line on every connection is [`ToHost::Attach`] with its committed cursor. The host
//! replays every later message in order, then sends one [`FromHost::State`]. Every
//! message the host sends carries a sequence number, and the host keeps each one until
//! ccd acknowledges it with [`ToHost::Ack`], which ccd sends only once what the message
//! produced is committed. Every command carries a uuid ccd minted, and its
//! [`FromHost::Outcome`] names that uuid.
//!
//! The host's own record is [`HostFile`], `host.json` in the run's folder. ccd reads its
//! protocol version before attaching and stops a host whose version it does not know.
//! Changes are additive only.
//!
//! `fixtures/host/messages.jsonl` holds a line of every message, emitted by this
//! module's tests. It is the fixture for the agent host's side of the same lines.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The version the host writes into `host.json`. A new host is admitted only at this
/// version, but a restarted ccd attaches a running host again without reading it: a change
/// to this version brings with it, in the same change, the stop for a host still running
/// the old one (P0 H9).
pub const HOST_PROTOCOL_VERSION: u32 = 1;

/// `host.json`, created by the host exclusively, so a second host for the same run
/// exits. Its start time is not here: ccd reads the host's birth identity from the
/// kernel ([`crate::proc_identity`]), the form a process cannot write for itself. claude
/// runs in the host's process group, whose id is `host_pid`, and ccd reaches it through
/// that group alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFile {
    pub protocol_version: u32,
    pub host_pid: i32,
}

/// A line ccd sends the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToHost {
    /// The first line of every connection: `after` is ccd's committed cursor, 0 for
    /// none. A newer connection replaces an older one.
    Attach {
        after: u64,
    },
    /// Every message up to `seq` is committed and may be dropped.
    Ack {
        seq: u64,
    },
    /// Starts the run's one `query()` with its first message.
    Start {
        uuid: String,
        options: StartOptions,
        prompt: String,
    },
    /// One message into Claude's input, stamped with `uuid`, so the echo,
    /// `command_lifecycle` and `user_message_uuids` name the command itself.
    Say {
        uuid: String,
        text: String,
    },
    /// The line that wakes the run's agent for its mail, `text` being the fixed nudge: one
    /// more message into Claude's input, stamped with `uuid` and marked as a background
    /// notification, not the person's words. Queued with the person's messages, answered as
    /// a `say` is.
    Wake {
        uuid: String,
        text: String,
    },
    /// The answer to the permission request `tool_use_id`.
    Answer {
        uuid: String,
        tool_use_id: String,
        decision: Decision,
    },
    /// The model, the effort, or both in that order, for the rest of the session. ccd sends
    /// one of the two per line, so each outcome is one change.
    SetModel {
        uuid: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    SetMode {
        uuid: String,
        mode: String,
    },
    /// The app's Stop of the turn `turn_id`, its `system/init` uuid: the host interrupts it, and nothing queued runs
    /// after it, only while that turn is the one claude is running; any other is refused.
    Interrupt {
        uuid: String,
        turn_id: String,
    },
    /// Ends the run: the host interrupts, ends Claude's input and closes the query.
    Stop {
        uuid: String,
    },
}

/// What the run's `query()` is started with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartOptions {
    pub cwd: String,
    /// The claude the probe found.
    pub claude_path: String,
    /// The mode the app chose. Absent, `query()` is given none, and claude picks its
    /// starting mode itself: the person's `defaultMode`, else its own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    pub trust: Trust,
    /// The Claude session to continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A new worktree for claude to make and start in, as `claude --worktree <name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// CodeConnect's mail server, run by Claude as a stdio MCP server.
    pub mail: McpCommand,
}

/// The person's answer to "trust this folder?".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    Trusted,
    NotTrusted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpCommand {
    pub command: String,
    pub args: Vec<String>,
}

/// An answer to a permission request, as the SDK's `PermissionResult` takes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum Decision {
    Allow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_input: Option<Value>,
        /// The SDK's `updatedPermissions` for "Allow for this session": claude's own allow
        /// rules for this request, each with the session as its destination, never a settings
        /// file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_permissions: Option<Value>,
    },
    Deny {
        message: String,
        /// The deny also ends claude's turn, as the person's deny of a main-conversation call does.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        interrupt: bool,
    },
}

/// A line the host sends ccd. Each carries its sequence number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromHost {
    /// A message the SDK yielded, untouched but for a `system/commands_changed`'s
    /// claude.ai connectors' commands, which the host leaves out as it does from the
    /// initialize answer's.
    Sdk { seq: u64, message: Value },
    /// A `canUseTool` call, held in the host until it is answered or withdrawn.
    Permission {
        seq: u64,
        request: PermissionRequest,
    },
    /// The request's signal aborted: its card closes.
    Withdrawn { seq: u64, tool_use_id: String },
    /// What became of command `uuid`.
    Outcome {
        seq: u64,
        uuid: String,
        status: OutcomeStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        /// On `start`'s outcome: the SDK's initialize answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initialize: Option<InitializeAnswer>,
        /// On `interrupt`'s outcome: the uuids its receipt lists as cancelled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancelled: Option<Vec<String>>,
        /// On `interrupt`'s outcome: the stopped turn's `result.terminal_reason` (`aborted_tools`,
        /// `aborted_streaming`, `completed`, …).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_reason: Option<String>,
    },
    /// Sent once after every replay, so it is never older than anything replayed.
    State {
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
        /// The permission requests still waiting for an answer.
        pending: Vec<PermissionRequest>,
    },
    /// Claude ended. The host exits once this is acknowledged.
    Exited {
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
}

/// A `canUseTool` call. `input` is the one the card's hash covers. `options` is the
/// call's options object as the SDK gives it, without its abort signal, its tool-use id
/// and its control request id: an answer goes by `tool_use_id` alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub tool_use_id: String,
    pub tool_name: String,
    pub input: Value,
    pub options: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Done,
    Refused,
}

/// The SDK's initialize answer, as far as ccd keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeAnswer {
    /// The SDK's command list, as it gives it but for the claude.ai connectors' commands
    /// (named `claude.ai <connector>:<prompt> (MCP)`), which the host leaves out: they name
    /// the account's connected services.
    pub commands: Vec<Value>,
    /// The SDK's model list, as it gives it.
    pub models: Vec<Value>,
    pub account: Account,
    /// The version of the claude that answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code_version: Option<String>,
}

/// Of the account, only which provider Claude uses. Every other key the SDK gives (the
/// email, the organization and the rest) has no field here, so it is dropped when a
/// line is read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_provider: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const GOLDEN: &str = include_str!("../../../fixtures/host/messages.jsonl");

    fn to_host_name(message: &ToHost) -> &'static str {
        match message {
            ToHost::Attach { .. } => "attach",
            ToHost::Ack { .. } => "ack",
            ToHost::Start { .. } => "start",
            ToHost::Say { .. } => "say",
            ToHost::Wake { .. } => "wake",
            ToHost::Answer { .. } => "answer",
            ToHost::SetModel { .. } => "set_model",
            ToHost::SetMode { .. } => "set_mode",
            ToHost::Interrupt { .. } => "interrupt",
            ToHost::Stop { .. } => "stop",
        }
    }

    fn from_host_name(message: &FromHost) -> &'static str {
        match message {
            FromHost::Sdk { .. } => "sdk",
            FromHost::Permission { .. } => "permission",
            FromHost::Withdrawn { .. } => "withdrawn",
            FromHost::Outcome { .. } => "outcome",
            FromHost::State { .. } => "state",
            FromHost::Exited { .. } => "exited",
        }
    }

    const TO_HOST: [&str; 10] = [
        "attach",
        "ack",
        "start",
        "say",
        "wake",
        "answer",
        "set_model",
        "set_mode",
        "interrupt",
        "stop",
    ];
    const FROM_HOST: [&str; 6] = [
        "sdk",
        "permission",
        "withdrawn",
        "outcome",
        "state",
        "exited",
    ];

    fn request() -> PermissionRequest {
        PermissionRequest {
            tool_use_id: "toolu_01".into(),
            tool_name: "Bash".into(),
            input: json!({"command": "cargo test", "description": "Run the tests"}),
            options: json!({"title": "Claude wants to run cargo test", "displayName": "Run command"}),
        }
    }

    fn mail() -> McpCommand {
        McpCommand {
            command: "/Users/person/.codeconnect/bin/codeconnect".into(),
            args: vec![
                "mcp".into(),
                "--session-uid".into(),
                "01ABCDEFGHJKMNPQRSTVWXYZ00".into(),
                "--socket".into(),
                "/Users/person/.codeconnect/ccd.sock".into(),
            ],
        }
    }

    /// One value of every message, in the order of `fixtures/host/messages.jsonl`.
    fn lines() -> Vec<(&'static str, Value)> {
        let to = |message: ToHost| ("to_host", serde_json::to_value(message).unwrap());
        let from = |message: FromHost| ("from_host", serde_json::to_value(message).unwrap());
        vec![
            to(ToHost::Attach { after: 0 }),
            to(ToHost::Ack { seq: 7 }),
            to(ToHost::Start {
                uuid: "c-start".into(),
                options: StartOptions {
                    cwd: "/Users/person/project".into(),
                    claude_path: "/Users/person/.local/bin/claude".into(),
                    permission_mode: None,
                    trust: Trust::Trusted,
                    resume: None,
                    model: None,
                    worktree: Some("feature-x".into()),
                    mail: mail(),
                },
                prompt: "fix the tests".into(),
            }),
            to(ToHost::Start {
                uuid: "c-continue".into(),
                options: StartOptions {
                    cwd: "/Users/person/project".into(),
                    claude_path: "/Users/person/.local/bin/claude".into(),
                    permission_mode: Some("acceptEdits".into()),
                    trust: Trust::NotTrusted,
                    resume: Some("5f0c1e2a-0000-4000-8000-000000000001".into()),
                    model: Some("opus".into()),
                    worktree: None,
                    mail: mail(),
                },
                prompt: "and the docs".into(),
            }),
            to(ToHost::Say {
                uuid: "c-say".into(),
                text: "also the README".into(),
            }),
            to(ToHost::Wake {
                uuid: "c-wake".into(),
                text: crate::ipc::MAIL_NUDGE_ACT.into(),
            }),
            to(ToHost::Answer {
                uuid: "c-allow".into(),
                tool_use_id: "toolu_01".into(),
                decision: Decision::Allow {
                    updated_input: Some(json!({"command": "cargo test"})),
                    updated_permissions: Some(json!([{"type": "addRules",
                        "rules": [{"toolName": "Bash", "ruleContent": "cargo test"}],
                        "behavior": "allow", "destination": "session"}])),
                },
            }),
            to(ToHost::Answer {
                uuid: "c-allow-plain".into(),
                tool_use_id: "toolu_03".into(),
                decision: Decision::Allow {
                    updated_input: None,
                    updated_permissions: None,
                },
            }),
            to(ToHost::Answer {
                uuid: "c-deny".into(),
                tool_use_id: "toolu_02".into(),
                decision: Decision::Deny {
                    message: "Not now".into(),
                    interrupt: true,
                },
            }),
            to(ToHost::SetModel {
                uuid: "c-model".into(),
                model: Some("sonnet".into()),
                effort: Some("high".into()),
            }),
            to(ToHost::SetMode {
                uuid: "c-mode".into(),
                mode: "plan".into(),
            }),
            to(ToHost::Interrupt {
                uuid: "c-interrupt".into(),
                turn_id: "9d1f0000-0000-4000-8000-000000000002".into(),
            }),
            to(ToHost::Stop {
                uuid: "c-stop".into(),
            }),
            from(FromHost::Sdk {
                seq: 1,
                message: json!({"type": "system", "subtype": "init", "session_id": "5f0c1e2a-0000-4000-8000-000000000001", "uuid": "9d1f0000-0000-4000-8000-000000000002"}),
            }),
            from(FromHost::Permission {
                seq: 2,
                request: request(),
            }),
            from(FromHost::Withdrawn {
                seq: 3,
                tool_use_id: "toolu_01".into(),
            }),
            from(FromHost::Outcome {
                seq: 4,
                uuid: "c-start".into(),
                status: OutcomeStatus::Done,
                detail: None,
                initialize: Some(InitializeAnswer {
                    commands: vec![
                        json!({"name": "compact", "description": "Clear conversation history but keep a summary in context", "argumentHint": "<optional custom summarization instructions>"}),
                    ],
                    models: vec![
                        json!({"value": "default", "displayName": "Default (recommended)", "description": "Opus for complex tasks", "supportsEffort": true, "supportedEffortLevels": ["low", "medium", "high"]}),
                    ],
                    account: Account {
                        api_provider: Some("firstParty".into()),
                    },
                    claude_code_version: Some("2.1.322".into()),
                }),
                cancelled: None,
                terminal_reason: None,
            }),
            from(FromHost::Outcome {
                seq: 5,
                uuid: "c-interrupt".into(),
                status: OutcomeStatus::Done,
                detail: None,
                initialize: None,
                cancelled: Some(vec!["c-say".into()]),
                terminal_reason: Some("aborted_tools".into()),
            }),
            from(FromHost::Outcome {
                seq: 6,
                uuid: "c-mode".into(),
                status: OutcomeStatus::Refused,
                detail: Some("no such permission mode: plann".into()),
                initialize: None,
                cancelled: None,
                terminal_reason: None,
            }),
            from(FromHost::State {
                seq: 7,
                session_id: Some("5f0c1e2a-0000-4000-8000-000000000001".into()),
                model: Some("sonnet".into()),
                effort: Some("high".into()),
                permission_mode: Some("default".into()),
                pending: vec![request()],
            }),
            from(FromHost::Exited {
                seq: 8,
                code: Some(0),
                signal: None,
            }),
            from(FromHost::Exited {
                seq: 9,
                code: None,
                signal: Some("SIGTERM".into()),
            }),
            (
                "host_json",
                serde_json::to_value(HostFile {
                    protocol_version: HOST_PROTOCOL_VERSION,
                    host_pid: 4242,
                })
                .unwrap(),
            ),
        ]
    }

    fn derived() -> Vec<Value> {
        lines()
            .into_iter()
            .map(|(dir, message)| json!({"dir": dir, "message": message}))
            .collect()
    }

    fn golden() -> Vec<Value> {
        GOLDEN
            .lines()
            .enumerate()
            .map(|(n, line)| {
                serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("messages.jsonl line {} is not JSON: {e}", n + 1))
            })
            .collect()
    }

    /// **The golden file is what this build emits**, line by line, as JSON values.
    #[test]
    fn the_host_golden_is_what_this_build_emits() {
        let (derived, golden) = (derived(), golden());
        for (n, (ours, committed)) in derived.iter().zip(&golden).enumerate() {
            assert_eq!(
                ours,
                committed,
                "messages.jsonl line {} no longer matches what this build emits. Regenerate it with \
                 cargo test -p protocol -- --ignored regenerate_the_host_golden",
                n + 1
            );
        }
        assert_eq!(
            derived.len(),
            golden.len(),
            "messages.jsonl has a different number of lines"
        );
    }

    /// Every line decodes as its direction says and re-encodes to itself.
    #[test]
    fn every_golden_line_decodes_and_reencodes_to_itself() {
        for (n, line) in golden().into_iter().enumerate() {
            let message = line["message"].clone();
            let again = match line["dir"].as_str() {
                Some("to_host") => serde_json::to_value(
                    serde_json::from_value::<ToHost>(message.clone())
                        .unwrap_or_else(|e| panic!("line {}: not a ToHost: {e}", n + 1)),
                ),
                Some("from_host") => serde_json::to_value(
                    serde_json::from_value::<FromHost>(message.clone())
                        .unwrap_or_else(|e| panic!("line {}: not a FromHost: {e}", n + 1)),
                ),
                Some("host_json") => serde_json::to_value(
                    serde_json::from_value::<HostFile>(message.clone())
                        .unwrap_or_else(|e| panic!("line {}: not a HostFile: {e}", n + 1)),
                ),
                other => panic!("line {}: unknown dir {other:?}", n + 1),
            }
            .unwrap();
            assert_eq!(
                again,
                message,
                "line {} does not re-encode to itself",
                n + 1
            );
        }
    }

    /// Every message of both directions, and `host.json`, has a line.
    #[test]
    fn every_message_has_a_golden_line() {
        let mut to_host = std::collections::BTreeSet::new();
        let mut from_host = std::collections::BTreeSet::new();
        let mut host_json = 0;
        for line in golden() {
            let message = line["message"].clone();
            match line["dir"].as_str() {
                Some("to_host") => {
                    to_host.insert(to_host_name(&serde_json::from_value(message).unwrap()));
                }
                Some("from_host") => {
                    from_host.insert(from_host_name(&serde_json::from_value(message).unwrap()));
                }
                _ => host_json += 1,
            }
        }
        assert_eq!(to_host, TO_HOST.into_iter().collect());
        assert_eq!(from_host, FROM_HOST.into_iter().collect());
        assert_eq!(host_json, 1);
    }

    /// The account is read for its provider alone: an email and an organization in a
    /// start outcome are gone once the line is read.
    #[test]
    fn an_account_keeps_only_its_provider() {
        let line = r#"{"type":"outcome","seq":4,"uuid":"c-start","status":"done","initialize":{"commands":[],"models":[],"account":{"api_provider":"firstParty","email":"marker-7f3a@example.com","organization":"Marker Org 7f3a","subscriptionType":"max"}}}"#;
        let read: FromHost = serde_json::from_str(line).unwrap();
        let kept = serde_json::to_string(&read).unwrap();
        assert!(kept.contains("firstParty"), "{kept}");
        for marker in ["marker-7f3a@example.com", "Marker Org 7f3a", "max"] {
            assert!(!kept.contains(marker), "{marker} survived: {kept}");
        }
    }

    /// Rewrite `fixtures/host/messages.jsonl` with what this build emits.
    #[test]
    #[ignore = "generator, not a gate"]
    fn regenerate_the_host_golden() {
        let text: String = derived()
            .iter()
            .map(|line| serde_json::to_string(line).unwrap() + "\n")
            .collect();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/host/messages.jsonl"
            ),
            text,
        )
        .unwrap();
    }
}
