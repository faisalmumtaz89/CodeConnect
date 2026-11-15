//! Claude Agent SDK messages as CodeConnect events.
//!
//! One event per SDK message that has a row (a result files two: the turn's end and
//! its usage), as the transcript tailer files one per transcript line, so an app
//! reads an SDK event as it reads a transcript line: an assistant
//! message is one `agent_message` whose content blocks carry its text, thinking and
//! tool calls. A payload keeps only the fields a reader uses, never the whole
//! message, so the account's metadata the SDK yields with every turn never reaches
//! an event: the list of claude.ai connectors in `system/init` (`mcp_servers`, and
//! their entries in `tools` and `slash_commands`), the plan's rate-limit windows,
//! and the login's expiry. A connector the agent calls is part of the conversation
//! and is kept like any tool call.
//!
//! A command's own output (`/usage`, `/effort low`, `/compact`'s "Compacted") is
//! filed as a terminal run's transcript holds it: a `user_message` whose content is
//! its `<local-command-stdout>` (or stderr) string. The SDK gives it three ways: a
//! replayed user line with that content, an assistant line's `local_command_source`
//! (`/effort`'s), and `system/local_command_output`'s bare `content`.
//!
//! What is not a row comes back as a [`Signal`]: the echo of a person's message,
//! a stamped command's lifecycle, the session id, the folder and the screen-only
//! commands every `system/init` carries, claude's command list, and a `/clear`. A
//! subagent's own messages, and every type with no row here, are skipped.

use serde_json::{json, Map, Value};

use crate::event::{notice, sdk_event_id, EventKind, PendingEvent, SessionKey, Source};

/// What an SDK message tells the run without being a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// The person's message `uuid` reached Claude: its echo, or a
    /// `user_message_uuids` naming it.
    Delivered { uuid: String },
    /// A stamped command's state: `queued`, `started`, `completed` or `cancelled`.
    Lifecycle { command_uuid: String, state: String },
    /// The run's Claude session id, from every `system/init`.
    SessionId { id: String },
    /// The folder claude runs in, from every `system/init`: in a new worktree, the
    /// one claude made, not the folder the run was started from.
    Cwd { path: String },
    /// `/clear` began a new conversation. Its id is not the session id a run
    /// resumes: the next `system/init` carries that.
    Reset { new_conversation_id: String },
    /// The commands claude has now, from a `system/commands_changed`, each as it
    /// gives it (`name`, `description`, `argumentHint`, ...).
    Commands { commands: Vec<Value> },
    /// The commands claude offers on its own screen only, from every
    /// `system/init`'s `terminal_slash_commands`.
    ScreenOnly { names: Vec<String> },
}

/// The events and signals of one SDK message.
#[derive(Debug, Default)]
pub struct Mapped {
    pub events: Vec<PendingEvent>,
    pub signals: Vec<Signal>,
}

/// The two lines Claude writes when the person stops a turn, read by the apps as
/// the turn's interruption.
const INTERRUPTS: [&str; 2] = [
    "[Request interrupted by user]",
    "[Request interrupted by user for tool use]",
];

/// The events and signals of `message`, filed under `session`. `turn` is the
/// uuid of the turn's `system/init`, which an init's own events carry too.
pub fn map(session: &SessionKey, turn: Option<&str>, message: &Value) -> Mapped {
    let mut mapped = Mapped::default();
    let Some(uuid) = message.get("uuid").and_then(Value::as_str) else {
        return mapped;
    };
    if message
        .get("parent_tool_use_id")
        .is_some_and(|parent| !parent.is_null())
    {
        return mapped;
    }
    let event = |kind: EventKind, block: usize, payload: Value| {
        let id = sdk_event_id(uuid, &kind, block);
        PendingEvent::new(session, kind, payload, Source::Sdk)
            .with_source_event_id(id)
            .with_turn_id(turn.map(str::to_string))
            .with_item_id(Some(uuid.to_string()))
    };
    let text = |key: &str| message.get(key).and_then(Value::as_str);
    let flag = |key: &str| message.get(key).and_then(Value::as_bool) == Some(true);
    let output = |content: &str| {
        event(
            EventKind::UserMessage,
            0,
            json!({"message": {"content": content}}),
        )
    };

    match (text("type"), text("subtype")) {
        (Some("system"), Some("init")) => {
            mapped.events.push(event(
                EventKind::SessionStart,
                0,
                pick(message, &["model", "permissionMode"]),
            ));
            if let Some(id) = text("session_id") {
                mapped.signals.push(Signal::SessionId { id: id.into() });
            }
            if let Some(path) = text("cwd") {
                mapped.signals.push(Signal::Cwd { path: path.into() });
            }
            if let Some(names) = message
                .get("terminal_slash_commands")
                .and_then(Value::as_array)
            {
                mapped.signals.push(Signal::ScreenOnly {
                    names: names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                });
            }
        }
        (Some("system"), Some("commands_changed")) => {
            if let Some(commands) = message.get("commands").and_then(Value::as_array) {
                mapped.signals.push(Signal::Commands {
                    commands: commands.clone(),
                });
            }
        }
        (Some("system"), Some("local_command_output")) => {
            if let Some(content) = text("content") {
                mapped.events.push(output(&format!(
                    "<local-command-stdout>{content}</local-command-stdout>"
                )));
            }
        }
        (Some("system"), Some("status")) if message.get("permissionMode").is_some() => {
            mapped.events.push(event(
                EventKind::Other("transcript_permission-mode".into()),
                0,
                pick(message, &["permissionMode"]),
            ));
        }
        (Some("system"), Some("compact_boundary")) => {
            mapped.events.push(event(
                EventKind::Notification,
                0,
                json!({"notification_type": notice::COMPACTED, "message": "Conversation compacted"}),
            ));
        }
        (Some("assistant"), _) => {
            if let Some(source) = text("local_command_source") {
                mapped.events.push(output(source));
            } else {
                let mut payload = Map::new();
                if let Some(inner) = message.get("message") {
                    payload.insert("message".into(), pick(inner, &["model", "content"]));
                }
                if flag("isApiErrorMessage") {
                    payload.insert("isApiErrorMessage".into(), Value::Bool(true));
                }
                mapped
                    .events
                    .push(event(EventKind::AgentMessage, 0, Value::Object(payload)));
            }
            mapped.signals.extend(delivered(message));
        }
        (Some("user"), _) if flag("isReplay") => {
            if let Some(content) = message
                .pointer("/message/content")
                .and_then(Value::as_str)
                .filter(|content| is_command_output(content))
            {
                mapped.events.push(output(content));
            }
            mapped.signals.push(Signal::Delivered { uuid: uuid.into() });
        }
        (Some("user"), _) => {
            let content = message.pointer("/message/content");
            let blocks = content.and_then(Value::as_array);
            let kind = if blocks.is_some_and(|blocks| blocks.iter().any(is_tool_result)) {
                Some(EventKind::ToolResult)
            } else if !flag("isSynthetic") && blocks.is_some_and(|blocks| is_interrupt(blocks)) {
                Some(EventKind::UserMessage)
            } else {
                None
            };
            if let (Some(kind), Some(content)) = (kind, content) {
                mapped
                    .events
                    .push(event(kind, 0, json!({"message": {"content": content}})));
            }
        }
        (Some("result"), _) => {
            // `num_turns` 0 is a command's own turn (/compact, /clear, /effort), which
            // otherwise ends exactly like the agent's.
            mapped.events.push(event(
                EventKind::TurnComplete,
                0,
                pick(message, &["subtype", "is_error", "num_turns"]),
            ));
            mapped.events.push(event(
                EventKind::Usage,
                1,
                pick(message, &["usage", "total_cost_usd"]),
            ));
            mapped.signals.extend(delivered(message));
        }
        (Some("command_lifecycle"), _) => {
            if let (Some(command_uuid), Some(state)) = (text("command_uuid"), text("state")) {
                mapped.signals.push(Signal::Lifecycle {
                    command_uuid: command_uuid.into(),
                    state: state.into(),
                });
            }
        }
        (Some("conversation_reset"), _) => {
            mapped.events.push(event(
                EventKind::Notification,
                0,
                json!({"notification_type": notice::CONVERSATION_CLEARED, "message": "Conversation cleared"}),
            ));
            if let Some(id) = text("new_conversation_id") {
                mapped.signals.push(Signal::Reset {
                    new_conversation_id: id.into(),
                });
            }
        }
        _ => {}
    }
    mapped
}

/// The fields of `value` named in `keys` that it has, and no others.
fn pick(value: &Value, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| {
                value
                    .get(*key)
                    .map(|field| (key.to_string(), field.clone()))
            })
            .collect(),
    )
}

/// A `Delivered` for each uuid the message's `user_message_uuids` names.
fn delivered(message: &Value) -> Vec<Signal> {
    message
        .get("user_message_uuids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|uuid| Signal::Delivered { uuid: uuid.into() })
        .collect()
}

/// A command's own output, as claude wraps it.
fn is_command_output(content: &str) -> bool {
    content.starts_with("<local-command-stdout>") || content.starts_with("<local-command-stderr>")
}

fn is_tool_result(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("tool_result")
}

/// One text block reading exactly one of Claude's interruption lines.
fn is_interrupt(blocks: &[Value]) -> bool {
    matches!(blocks, [block]
        if block.get("type").and_then(Value::as_str) == Some("text")
            && block
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| INTERRUPTS.contains(&text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// One real session (SDK 0.3.322, claude 2.1.322) with its account data dropped.
    const CAPTURE: &str = include_str!("../../../fixtures/claude/sdk-session-0.3.322.jsonl");
    const WIRE: &str = include_str!("../../../fixtures/claude/sdk-session-wire.json");

    fn session() -> SessionKey {
        SessionKey::new("01K1B3XQ8ZC0DE5FGH7JKMNPQR", "cc-1")
    }

    fn messages() -> Vec<Value> {
        CAPTURE
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// The capture mapped in order, each turn's events carrying the uuid of its
    /// `system/init`, the init's own included.
    fn mapped() -> (Vec<PendingEvent>, Vec<Signal>) {
        let (mut events, mut signals, mut turn) = (Vec::new(), Vec::new(), None::<String>);
        for message in messages() {
            if message["type"] == "system" && message["subtype"] == "init" {
                turn = message["uuid"].as_str().map(str::to_string);
            }
            let mapped = map(&session(), turn.as_deref(), &message);
            events.extend(mapped.events);
            signals.extend(mapped.signals);
        }
        (events, signals)
    }

    /// What an app reads of each event.
    fn wire(events: &[PendingEvent]) -> Value {
        Value::Array(
            events
                .iter()
                .map(|event| {
                    json!({
                        "kind": event.kind,
                        "payload": event.payload,
                        "source": event.source,
                        "source_event_id": event.source_event_id,
                        "turn_id": event.turn_id,
                        "item_id": event.item_id,
                    })
                })
                .collect(),
        )
    }

    /// **The events of the recorded session, emitted by this build:** the mapper's own
    /// gate. The apps' readers test against what ccd files and sends,
    /// `fixtures/claude/sdk-session-events.json`.
    #[test]
    fn the_sdk_session_wire_fixture_is_what_this_build_emits() {
        let committed: Value = serde_json::from_str(WIRE).unwrap();
        assert_eq!(
            wire(&mapped().0),
            committed,
            "the committed sdk session events no longer match what this build emits. \
             Regenerate them with cargo test -p protocol -- --ignored \
             regenerate_the_sdk_session_wire_fixture"
        );
    }

    /// Rewrite `fixtures/claude/sdk-session-wire.json` with what this build emits.
    #[test]
    #[ignore = "generator, not a gate"]
    fn regenerate_the_sdk_session_wire_fixture() {
        let text = serde_json::to_string_pretty(&wire(&mapped().0)).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/sdk-session-wire.json"
            ),
            text + "\n",
        )
        .unwrap();
    }

    /// The recorded session's 150 messages file 111 events: one for each message that
    /// has a row, and two, its end and its usage, for each turn's result. Three of the
    /// `user_message`s are commands' output (`/model`'s, `/compact`'s and `/effort`'s).
    #[test]
    fn the_session_files_its_events_by_kind() {
        let mut counts = BTreeMap::new();
        for event in mapped().0 {
            *counts.entry(event.kind.as_str().to_string()).or_insert(0) += 1;
        }
        let expected: BTreeMap<String, i32> = [
            ("session_start", 17),
            ("transcript_permission-mode", 2),
            ("agent_message", 36),
            ("tool_result", 15),
            ("user_message", 5),
            ("turn_complete", 17),
            ("usage", 17),
            ("notification", 2),
        ]
        .into_iter()
        .map(|(kind, count)| (kind.to_string(), count))
        .collect();
        assert_eq!(counts, expected);
    }

    /// Every event has its own id, so the store keeps each, and mapping the same
    /// messages again gives the same ids, so a replay files none twice.
    #[test]
    fn every_event_has_its_own_id_and_a_replay_has_the_same() {
        let ids = |events: Vec<PendingEvent>| -> Vec<String> {
            events
                .into_iter()
                .map(|event| event.source_event_id.unwrap())
                .collect()
        };
        let first = ids(mapped().0);
        assert_eq!(first.iter().collect::<BTreeSet<_>>().len(), first.len());
        assert_eq!(ids(mapped().0), first);
    }

    /// Every delivery names a person's message whose echo the session carries,
    /// and every echo is a delivery; one session id, one folder and the screen-only
    /// commands per init; the lifecycles, the two command lists and the one `/clear`.
    #[test]
    fn the_signals_are_the_session_s_deliveries_lifecycles_ids_and_reset() {
        let echoes: BTreeSet<String> = messages()
            .iter()
            .filter(|message| message["isReplay"] == true)
            .map(|message| message["uuid"].as_str().unwrap().to_string())
            .collect();
        let signals = mapped().1;
        let delivered: BTreeSet<String> = signals
            .iter()
            .filter_map(|signal| match signal {
                Signal::Delivered { uuid } => Some(uuid.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(delivered, echoes);
        let count = |of: fn(&Signal) -> bool| signals.iter().filter(|s| of(s)).count();
        assert_eq!(count(|s| matches!(s, Signal::SessionId { .. })), 17);
        assert_eq!(count(|s| matches!(s, Signal::Cwd { .. })), 17);
        assert_eq!(count(|s| matches!(s, Signal::Lifecycle { .. })), 12);
        assert_eq!(count(|s| matches!(s, Signal::Reset { .. })), 1);
        assert_eq!(count(|s| matches!(s, Signal::Commands { .. })), 2);
        let screen_only = [
            "doctor",
            "send-to-project",
            "color",
            "focus",
            "reload-plugins",
        ]
        .map(str::to_string)
        .to_vec();
        assert_eq!(
            signals
                .iter()
                .filter(|s| matches!(s, Signal::ScreenOnly { names } if *names == screen_only))
                .count(),
            17
        );
        let lists: Vec<&Vec<Value>> = signals
            .iter()
            .filter_map(|signal| match signal {
                Signal::Commands { commands } => Some(commands),
                _ => None,
            })
            .collect();
        let changed: Vec<Value> = messages()
            .into_iter()
            .filter(|message| message["subtype"] == "commands_changed")
            .map(|message| message["commands"].clone())
            .collect();
        assert_eq!(
            lists
                .into_iter()
                .cloned()
                .map(Value::Array)
                .collect::<Vec<_>>(),
            changed
        );
    }

    /// **B8 G3: a command's output is the person's row in a terminal run's shape**, from
    /// each of the SDK's three ways of giving it; on the recorded session those are
    /// `/model`'s line 108, `/compact`'s line 118 and `/effort`'s line 131, and no other
    /// line.
    #[test]
    fn a_commands_output_is_filed_as_a_terminal_run_files_it() {
        let row = |content: &str| json!({"message": {"content": content}});
        for (message, content) in [
            (
                json!({"type": "user", "uuid": "o1", "isReplay": true,
                    "message": {"role": "user", "content": "<local-command-stdout>Compacted </local-command-stdout>"}}),
                "<local-command-stdout>Compacted </local-command-stdout>",
            ),
            (
                json!({"type": "user", "uuid": "o2", "isReplay": true,
                    "message": {"role": "user", "content": "<local-command-stderr>No such model</local-command-stderr>"}}),
                "<local-command-stderr>No such model</local-command-stderr>",
            ),
            (
                json!({"type": "assistant", "uuid": "o3",
                    "message": {"model": "<synthetic>", "content": [{"type": "text", "text": "Set effort level to low"}]},
                    "local_command_run": {"command": "effort", "args": "low"},
                    "local_command_source": "<local-command-stdout>Set effort level to low</local-command-stdout>"}),
                "<local-command-stdout>Set effort level to low</local-command-stdout>",
            ),
            (
                json!({"type": "system", "subtype": "local_command_output", "uuid": "o4",
                    "content": "Current session: 3% used"}),
                "<local-command-stdout>Current session: 3% used</local-command-stdout>",
            ),
        ] {
            let events = map(&session(), Some("t1"), &message).events;
            assert_eq!(events.len(), 1, "{message}");
            assert_eq!(events[0].kind, EventKind::UserMessage, "{message}");
            assert_eq!(events[0].payload, row(content), "{message}");
            assert_eq!(events[0].turn_id.as_deref(), Some("t1"));
        }
        let outputs: Vec<(usize, Value)> = messages()
            .iter()
            .enumerate()
            .filter_map(|(at, message)| {
                let events = map(&session(), None, message).events;
                events
                    .iter()
                    .find(|event| {
                        event.kind == EventKind::UserMessage
                            && event.payload["message"]["content"]
                                .as_str()
                                .is_some_and(is_command_output)
                    })
                    .map(|event| (at + 1, event.payload["message"]["content"].clone()))
            })
            .collect();
        assert_eq!(
            outputs,
            vec![
                (108, json!("<local-command-stdout>Set model to `claude-sonnet-5-5`</local-command-stdout>")),
                (118, json!("<local-command-stdout>Compacted </local-command-stdout>")),
                (131, json!("<local-command-stdout>Set effort level to low (this session only): Quick, straightforward implementation with minimal overhead</local-command-stdout>")),
            ]
        );
    }

    /// The person's own echo, and a replayed line that is not a command's output, file
    /// nothing: each is only a delivery.
    #[test]
    fn the_persons_echo_is_only_a_delivery() {
        for message in [
            json!({"type": "user", "uuid": "e1", "isReplay": true,
                "message": {"role": "user", "content": "and the docs"}}),
            json!({"type": "user", "uuid": "e2", "isReplay": true,
                "message": {"role": "user", "content": "see <local-command-stdout>x</local-command-stdout>"}}),
            json!({"type": "user", "uuid": "e3", "isReplay": true,
                "message": {"role": "user", "content": [{"type": "text", "text": "<local-command-stdout>x</local-command-stdout>"}]}}),
        ] {
            let mapped = map(&session(), None, &message);
            assert!(mapped.events.is_empty(), "{message}");
            assert_eq!(
                mapped.signals,
                vec![Signal::Delivered {
                    uuid: message["uuid"].as_str().unwrap().into()
                }]
            );
        }
        let echoes = messages()
            .into_iter()
            .filter(|message| {
                message["isReplay"] == true
                    && !message["message"]["content"]
                        .as_str()
                        .is_some_and(is_command_output)
            })
            .count();
        assert_eq!(echoes, 15);
    }

    /// The account's metadata never reaches an event: the payloads keep what a
    /// reader uses, never the whole message. Messages that carry every such field,
    /// as the SDK yields them, file none of them.
    #[test]
    fn no_account_field_reaches_an_event() {
        let account = [
            json!({"type": "system", "subtype": "init", "uuid": "i1", "session_id": "s1",
                "model": "claude-opus-5-5", "permissionMode": "default", "apiKeySource": "none",
                "mcp_servers": [{"name": "claude.ai Example", "source": "claudeai", "status": "connected"}],
                "tools": ["Bash", "mcp__claude_ai_Example__search"],
                "slash_commands": ["compact", "mcp__claude_ai_Example__prompt"]}),
            json!({"type": "result", "subtype": "success", "uuid": "r1", "is_error": false,
                "usage": {"input_tokens": 1}, "total_cost_usd": 0.01,
                "credential_expires_at": 1_800_000_000_000_u64}),
            json!({"type": "rate_limit_event", "uuid": "l1", "rate_limit_info": {"status": "allowed"}}),
        ];
        let mut events: Vec<PendingEvent> = account
            .iter()
            .flat_map(|message| map(&session(), None, message).events)
            .collect();
        events.extend(mapped().0);
        let text = serde_json::to_string(&wire(&events)).unwrap();
        for field in [
            "\"mcp_servers\"",
            "\"tools\"",
            "\"slash_commands\"",
            "\"apiKeySource\"",
            "\"rate_limit_info\"",
            "\"credential_expires_at\"",
            "claude.ai Example",
            "mcp__claude_ai_Example",
        ] {
            assert!(!text.contains(field), "{field} reached an event");
        }
        let start = &events[0];
        assert_eq!(start.kind, EventKind::SessionStart);
        assert_eq!(
            start.payload,
            json!({"model": "claude-opus-5-5", "permissionMode": "default"})
        );
    }

    /// What has no row files nothing: a subagent's own message, the compaction
    /// summary, a type the map does not know.
    #[test]
    fn what_has_no_row_files_nothing() {
        for message in [
            json!({"type": "assistant", "uuid": "u1", "parent_tool_use_id": "toolu_1",
                "message": {"content": [{"type": "text", "text": "inside a subagent"}]}}),
            json!({"type": "user", "uuid": "u2", "isSynthetic": true,
                "message": {"content": [{"type": "text", "text": "This session is being continued"}]}}),
            json!({"type": "rate_limit_event", "uuid": "u3"}),
            json!({"type": "something_new", "uuid": "u4"}),
        ] {
            let mapped = map(&session(), None, &message);
            assert!(mapped.events.is_empty(), "{message}");
            assert!(mapped.signals.is_empty(), "{message}");
        }
    }
}
