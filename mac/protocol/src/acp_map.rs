//! An Antigravity run's `agy acp` messages as timeline events (Agent Client Protocol).
//!
//! The agent host forwards each `session/update` notification and each `session/prompt`
//! reply whole ([`crate::host::FromHost::Acp`]). The agent's words and thoughts arrive as
//! chunks: [`read`] hands each back as a [`Read::Chunk`], and the link joins a run of them
//! into one event ([`joined`]) when something else arrives. Everything else is filed in
//! the shapes the apps already draw: a tool call and its result as a hook's are
//! (`tool_use_id`, `tool_name`, `tool_input`; the name its ACP kind's word), its result's outcome as `status` and
//! `aggregated_output`, and the prompt's reply as the turn's end. The session's settings, as its opening's reply
//! (which the host forwards too) and each `config_option_update` and `current_mode_update` name them, are filed as
//! the run's settings ([`ACP_SETTINGS`]). Updates the timeline does not draw (plans, commands, usage) file nothing.
//!
//! Event ids are stable across a replay and unique to the host that sent them: each begins
//! with `run`, the uuid of the start that host was started with (a relaunch's is its own), as
//! a relaunched host numbers its lines from 1 again. A tool's id goes on with its
//! `toolCallId`; a turn's end and a joined text, with the host's sequence number of their line. Each event carries
//! its line's turn, the uuid of the command whose prompt was running, which is what an app
//! names to stop it.

use serde_json::{json, Value};

use crate::event::{EventKind, PendingEvent, SessionKey, Source};
use crate::facts::ACP_SETTINGS;

/// What one forwarded message is.
#[derive(Debug)]
pub enum Read {
    /// A piece of the agent's reply, or of its thinking: joined with the pieces next to it.
    Chunk { thought: bool, text: String },
    /// The events the message files, none for one the timeline does not draw.
    Events(Vec<PendingEvent>),
}

/// What `message`, the line `seq` of the host started as `run`, of the turn `turn`, is.
pub fn read(
    session: &SessionKey,
    run: &str,
    seq: u64,
    turn: Option<&str>,
    message: &Value,
) -> Read {
    let event = |kind: EventKind, id: String, payload: Value| {
        PendingEvent::new(session, kind, payload, Source::Acp)
            .with_source_event_id(id)
            .with_turn_id(turn.map(str::to_string))
    };
    if message.get("method").and_then(Value::as_str) == Some("session/update") {
        let update = &message["params"]["update"];
        let text = |key: &str| update.get(key).and_then(Value::as_str);
        let call = text("toolCallId").unwrap_or_default();
        return match text("sessionUpdate") {
            Some(kind @ ("agent_message_chunk" | "agent_thought_chunk")) => {
                match update["content"]["text"].as_str() {
                    Some(words) if update["content"]["type"] == "text" => Read::Chunk {
                        thought: kind == "agent_thought_chunk",
                        text: words.to_string(),
                    },
                    _ => Read::Events(Vec::new()),
                }
            }
            Some("tool_call") => {
                let mut events = vec![event(
                    EventKind::ToolCall,
                    format!("{run}/tool_call/{call}"),
                    json!({
                        "tool_use_id": call,
                        "tool_name": tool_name(update).unwrap_or("tool"),
                        "tool_input": update.get("rawInput").cloned().unwrap_or_else(|| json!({})),
                    }),
                )];
                events.extend(result(update, call).map(|payload| {
                    event(
                        EventKind::ToolResult,
                        format!("{run}/tool_result/{call}"),
                        payload,
                    )
                }));
                Read::Events(events)
            }
            Some("config_option_update") => Read::Events(
                settings(&update["configOptions"])
                    .map(|payload| {
                        event(
                            EventKind::Other(ACP_SETTINGS.into()),
                            format!("{run}/settings/{seq}"),
                            payload,
                        )
                    })
                    .into_iter()
                    .collect(),
            ),
            Some("current_mode_update") => Read::Events(
                text("currentModeId")
                    .map(|mode| {
                        event(
                            EventKind::Other(ACP_SETTINGS.into()),
                            format!("{run}/settings/{seq}"),
                            json!({ "mode": mode }),
                        )
                    })
                    .into_iter()
                    .collect(),
            ),
            Some("tool_call_update") => Read::Events(
                result(update, call)
                    .map(|payload| {
                        event(
                            EventKind::ToolResult,
                            format!("{run}/tool_result/{call}"),
                            payload,
                        )
                    })
                    .into_iter()
                    .collect(),
            ),
            _ => Read::Events(Vec::new()),
        };
    }
    let turn_end = |reason: &str| {
        event(
            EventKind::TurnComplete,
            format!("{run}/turn/{seq}"),
            json!({ "stop_reason": reason }),
        )
    };
    // The reply to the session's opening (`session/new`, `session/resume`), which the host forwards: its settings.
    if let Some(payload) = settings(&message["result"]["configOptions"]) {
        return Read::Events(vec![event(
            EventKind::Other(ACP_SETTINGS.into()),
            format!("{run}/settings/{seq}"),
            payload,
        )]);
    }
    if let Some(reason) = message["result"]["stopReason"].as_str() {
        return Read::Events(vec![turn_end(reason)]);
    }
    if let Some(error) = message.get("error") {
        let words = error["message"]
            .as_str()
            .unwrap_or("the agent refused the prompt");
        return Read::Events(vec![
            event(
                EventKind::Error,
                format!("{run}/error/{seq}"),
                json!({ "message": words }),
            ),
            turn_end("error"),
        ]);
    }
    Read::Events(Vec::new())
}

/// The settings agy's `configOptions` say are current, as the run's settings are filed ([`ACP_SETTINGS`]): each
/// option's `currentValue` by its category (`model`, `thought_level` as `effort`, `mode`). None when they name none.
fn settings(options: &Value) -> Option<Value> {
    let current: serde_json::Map<String, Value> = options
        .as_array()?
        .iter()
        .filter_map(|option| {
            let key = match option["category"].as_str()? {
                "model" => "model",
                "thought_level" => "effort",
                "mode" => "mode",
                _ => return None,
            };
            Some((key.to_string(), option["currentValue"].as_str()?.into()))
        })
        .collect();
    (!current.is_empty()).then_some(Value::Object(current))
}

/// The run of chunks that began at the line `first` of the host started as `run`, of the turn
/// `turn`, as one event.
pub fn joined(
    session: &SessionKey,
    run: &str,
    thought: bool,
    text: &str,
    first: u64,
    turn: Option<&str>,
) -> PendingEvent {
    let kind = if thought {
        EventKind::Reasoning
    } else {
        EventKind::AgentMessage
    };
    PendingEvent::new(session, kind, json!({ "text": text }), Source::Acp)
        .with_source_event_id(format!("{run}/text/{first}"))
        .with_turn_id(turn.map(str::to_string))
}

/// A tool's name on its row: the word for its ACP `kind`, as the row's argument already shows what agy's title
/// repeats (a command, a path); a tool of no known kind (`other`, or none) by its title.
fn tool_name(update: &Value) -> Option<&str> {
    let named = match update.get("kind").and_then(Value::as_str) {
        Some("execute") => "Run",
        Some("read") => "Read",
        Some("edit") => "Edit",
        Some("delete") => "Delete",
        Some("move") => "Move",
        Some("search") => "Search",
        Some("fetch") => "Fetch",
        Some("think") => "Think",
        _ => return update.get("title").and_then(Value::as_str),
    };
    Some(named)
}

/// A finished tool's result: its status, and what it printed or wrote as text. Nothing
/// while it is still running.
fn result(update: &Value, call: &str) -> Option<Value> {
    let status = update.get("status").and_then(Value::as_str)?;
    if status != "completed" && status != "failed" {
        return None;
    }
    let output: Vec<String> = update["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| match item["type"].as_str() {
            Some("content") => item["content"]["text"].as_str().map(str::to_string),
            // A new file's diff has no old text: its path and its whole new text.
            Some("diff") => Some(format!(
                "{}\n{}",
                item["path"].as_str().unwrap_or_default(),
                item["newText"].as_str().unwrap_or_default()
            )),
            _ => None,
        })
        .collect();
    let mut payload = json!({
        "tool_use_id": call,
        "tool": update.get("kind").and_then(Value::as_str).unwrap_or("other"),
        "status": status,
        "aggregated_output": output.join("\n"),
    });
    if let Some(name) = tool_name(update) {
        payload["tool_name"] = name.into();
    }
    if let Some(input) = update.get("rawInput") {
        payload["tool_input"] = input.clone();
    }
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionKey {
        SessionKey::new("01RUN", "01RUN")
    }

    fn update(update: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update",
               "params": {"sessionId": "sess-1", "update": update}})
    }

    fn events(read: Read) -> Vec<PendingEvent> {
        match read {
            Read::Events(events) => events,
            Read::Chunk { .. } => panic!("a chunk, not events"),
        }
    }

    #[test]
    fn the_agents_words_and_thoughts_are_chunks() {
        let words = update(json!({"sessionUpdate": "agent_message_chunk",
                                  "content": {"type": "text", "text": "Hel"}}));
        assert!(matches!(read(&session(), "c-start", 3, None, &words),
            Read::Chunk { thought: false, text } if text == "Hel"));
        let thought = update(json!({"sessionUpdate": "agent_thought_chunk",
                                    "content": {"type": "text", "text": "hmm"}}));
        assert!(matches!(read(&session(), "c-start", 4, None, &thought),
            Read::Chunk { thought: true, text } if text == "hmm"));
        let joined = joined(&session(), "c-start", false, "Hello", 3, Some("c-say"));
        assert_eq!(joined.kind, EventKind::AgentMessage);
        assert_eq!(joined.payload, json!({"text": "Hello"}));
        assert_eq!(joined.source, Source::Acp);
        assert_eq!(joined.source_event_id.as_deref(), Some("c-start/text/3"));
        assert_eq!(joined.turn_id.as_deref(), Some("c-say"));
        assert_eq!(
            super::joined(&session(), "c-start", true, "hmm", 4, None).kind,
            EventKind::Reasoning
        );
    }

    #[test]
    fn a_tool_call_and_its_result_are_filed_as_a_hooks_are() {
        let call = update(json!({"sessionUpdate": "tool_call", "toolCallId": "call_1",
            "title": "cat NOTE.txt", "kind": "read", "status": "pending",
            "rawInput": {"command": "cat NOTE.txt"}}));
        let filed = events(read(&session(), "c-start", 5, None, &call));
        assert_eq!(filed.len(), 1);
        assert_eq!(filed[0].kind, EventKind::ToolCall);
        assert_eq!(
            filed[0].payload,
            json!({"tool_use_id": "call_1", "tool_name": "Read",
                   "tool_input": {"command": "cat NOTE.txt"}})
        );
        assert_eq!(
            filed[0].source_event_id.as_deref(),
            Some("c-start/tool_call/call_1")
        );
        // Still running: nothing yet.
        let running = update(json!({"sessionUpdate": "tool_call_update",
                                    "toolCallId": "call_1", "status": "in_progress"}));
        assert!(events(read(&session(), "c-start", 6, None, &running)).is_empty());
        let done = update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "call_1",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "the word"}}]}),
        );
        let filed = events(read(&session(), "c-start", 7, None, &done));
        assert_eq!(filed[0].kind, EventKind::ToolResult);
        assert_eq!(
            filed[0].payload,
            json!({"tool_use_id": "call_1", "tool": "other", "status": "completed",
                   "aggregated_output": "the word"})
        );
        assert_eq!(
            filed[0].source_event_id.as_deref(),
            Some("c-start/tool_result/call_1")
        );
    }

    #[test]
    fn a_failed_tool_and_a_new_files_diff_keep_what_people_see() {
        let failed = update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "call_2",
            "status": "failed", "kind": "execute",
            "content": [{"type": "content", "content": {"type": "text", "text": "exit 1"}}]}),
        );
        let filed = events(read(&session(), "c-start", 8, None, &failed));
        assert_eq!(filed[0].payload["status"], "failed");
        assert_eq!(filed[0].payload["tool"], "execute");
        assert_eq!(filed[0].payload["aggregated_output"], "exit 1");
        let written = update(json!({"sessionUpdate": "tool_call", "toolCallId": "call_3",
            "title": "Write a.txt", "kind": "edit", "status": "completed",
            "content": [{"type": "diff", "path": "/p/a.txt", "oldText": null, "newText": "hi"}]}));
        let filed = events(read(&session(), "c-start", 9, None, &written));
        assert_eq!(
            filed.len(),
            2,
            "a call that comes finished files its result too"
        );
        assert_eq!(filed[1].payload["aggregated_output"], "/p/a.txt\nhi");
        assert_eq!(filed[1].payload["tool_name"], "Edit", "named by its kind");
    }

    #[test]
    fn a_tool_is_named_by_its_kind_and_one_of_no_known_kind_by_its_title() {
        for (kind, name) in [
            ("execute", "Run"),
            ("read", "Read"),
            ("edit", "Edit"),
            ("delete", "Delete"),
            ("move", "Move"),
            ("search", "Search"),
            ("fetch", "Fetch"),
            ("think", "Think"),
        ] {
            let call = update(json!({"sessionUpdate": "tool_call", "toolCallId": "c",
                "title": "ls -la", "kind": kind, "rawInput": {"command": "ls -la"}}));
            assert_eq!(
                events(read(&session(), "c-start", 1, None, &call))[0].payload["tool_name"],
                name
            );
        }
        for kind in [Some("other"), None] {
            let mut tool =
                json!({"sessionUpdate": "tool_call", "toolCallId": "c", "title": "Ask the docs"});
            if let Some(kind) = kind {
                tool["kind"] = kind.into();
            }
            assert_eq!(
                events(read(&session(), "c-start", 1, None, &update(tool)))[0].payload["tool_name"],
                "Ask the docs"
            );
        }
    }

    #[test]
    fn the_prompts_reply_ends_the_turn_and_an_error_says_so() {
        let reply = json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}});
        let filed = events(read(&session(), "c-start", 10, Some("c-say"), &reply));
        assert_eq!(
            filed[0].turn_id.as_deref(),
            Some("c-say"),
            "its turn is the prompt's"
        );
        assert_eq!(filed[0].kind, EventKind::TurnComplete);
        assert_eq!(filed[0].payload, json!({"stop_reason": "end_turn"}));
        assert_eq!(filed[0].source_event_id.as_deref(), Some("c-start/turn/10"));
        let refused = json!({"jsonrpc": "2.0", "id": 4,
                             "error": {"code": -32000, "message": "Authentication required"}});
        let filed = events(read(&session(), "c-start", 11, None, &refused));
        assert_eq!(filed[0].kind, EventKind::Error);
        assert_eq!(
            filed[0].payload,
            json!({"message": "Authentication required"})
        );
        assert_eq!(filed[1].kind, EventKind::TurnComplete);
    }

    /// agy's settings as its `configOptions` give them (agy 1.5.15's shape, values cut down): the model family, the
    /// current model's thought levels and the mode, each with its `currentValue`, and one option of no category.
    fn options(model: &str, effort: &str, mode: &str) -> Value {
        json!([
            {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": model,
             "options": [{"value": "gemini-4-neon", "name": "Gemini 4 Neon"},
                         {"value": "gemini-3.8-flash", "name": "Gemini 3.8 Flash"}]},
            {"id": "thought_level", "name": "Thinking Level", "category": "thought_level", "type": "select",
             "currentValue": effort, "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
            {"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": mode,
             "options": [{"value": "default", "name": "Default"}, {"value": "plan", "name": "Plan"}]},
            {"id": "other", "name": "Other", "type": "select", "currentValue": "x", "options": []}
        ])
    }

    #[test]
    fn the_sessions_opening_and_each_change_file_its_settings() {
        let settings = |filed: Vec<PendingEvent>| {
            assert_eq!(filed.len(), 1);
            assert_eq!(
                filed[0].kind,
                EventKind::Other(crate::facts::ACP_SETTINGS.into())
            );
            (
                filed[0].source_event_id.clone().unwrap(),
                filed[0].payload.clone(),
            )
        };
        let opening = json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "sess-1",
            "configOptions": options("gemini-4-neon", "high", "default")}});
        assert_eq!(
            settings(events(read(&session(), "c-start", 3, None, &opening))),
            (
                "c-start/settings/3".to_string(),
                json!({"model": "gemini-4-neon", "effort": "high", "mode": "default"})
            )
        );
        let changed = update(json!({"sessionUpdate": "config_option_update",
            "configOptions": options("gemini-3.8-flash", "low", "plan")}));
        assert_eq!(
            settings(events(read(
                &session(),
                "c-start",
                7,
                Some("c-say"),
                &changed
            ))),
            (
                "c-start/settings/7".to_string(),
                json!({"model": "gemini-3.8-flash", "effort": "low", "mode": "plan"})
            )
        );
        let mode = update(json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}));
        assert_eq!(
            settings(events(read(&session(), "c-start", 8, None, &mode))),
            ("c-start/settings/8".to_string(), json!({"mode": "plan"}))
        );
    }

    #[test]
    fn updates_the_timeline_does_not_draw_file_nothing() {
        for kind in ["plan", "available_commands_update", "usage_update"] {
            let message = update(json!({"sessionUpdate": kind}));
            assert!(
                events(read(&session(), "c-start", 12, None, &message)).is_empty(),
                "{kind}"
            );
        }
        let image = update(json!({"sessionUpdate": "agent_message_chunk",
                                  "content": {"type": "image", "data": "", "mimeType": "image/png"}}));
        assert!(events(read(&session(), "c-start", 13, None, &image)).is_empty());
    }
}
