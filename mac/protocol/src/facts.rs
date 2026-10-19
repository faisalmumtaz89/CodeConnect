//! What a run's latest facts are, read off its events (minor 23).
//!
//! A phone that holds only a run's newest events cannot see the event that last
//! set its model or its permission mode once that event is older than what it
//! holds. The daemon keeps both from the whole log and sends them on the run's
//! [`crate::event::SessionSummary`], each with the seq of the event that decided
//! it, so a phone keeps whichever of the two is newer.
//!
//! **The model rule is the phone's, ported**, so the daemon and the phone cannot
//! name two different models for one run: `SessionState.modelFact(of:)`,
//! `LocalCommandLine.parse` and `ModelConfirmation.parse` in the iOS app.

use crate::event::{Event, EventKind, Fact, ModelFact, ModelSource, Source};

/// The kind the transcript tailer files Claude's permission-mode line under.
pub const TRANSCRIPT_PERMISSION_MODE: &str = "transcript_permission-mode";

/// A run's facts, each as of the newest event that decided it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFacts {
    pub model: Option<ModelFact>,
    pub permission_mode: Option<Fact>,
}

impl SessionFacts {
    /// Fold one event in. A fact it decides replaces the one held only when its
    /// event is newer, so events may be folded in any order.
    pub fn observe(&mut self, event: &Event) {
        if let Some(fact) = model_fact(event) {
            if self.model.as_ref().is_none_or(|held| held.seq < fact.seq) {
                self.model = Some(fact);
            }
        }
        if let Some(fact) = permission_mode_fact(event) {
            if self
                .permission_mode
                .as_ref()
                .is_none_or(|held| held.seq < fact.seq)
            {
                self.permission_mode = Some(fact);
            }
        }
    }
}

/// What one event says the run's model is, if anything: a `session_start`
/// carrying a non-empty `model`, or Claude Code's `/model` receipt on a
/// transcript user line. Both verbs of the receipt are facts about the model in
/// force; which one it was is not this fact's business.
pub fn model_fact(event: &Event) -> Option<ModelFact> {
    let (value, source) = match event.kind {
        EventKind::SessionStart => {
            let model = event.payload.get("model")?.as_str()?;
            if model.is_empty() {
                return None;
            }
            (model.to_string(), ModelSource::SessionStart)
        }
        EventKind::UserMessage => {
            let content = event.payload.get("message")?.get("content")?.as_str()?;
            (model_receipt(content)?.1, ModelSource::Command)
        }
        _ => return None,
    };
    Some(ModelFact {
        value,
        seq: event.seq,
        source,
        at: event.ts.clone(),
    })
}

/// What one event says the main agent's permission mode is, if anything: the
/// `permission_mode` of a hook's input that names no `agent_id`, or Claude's own
/// permission-mode transcript line.
///
/// A background agent's hook carries its `agent_id`, and its mode (measured:
/// `bubble`) is that agent's, never the run's.
pub fn permission_mode_fact(event: &Event) -> Option<Fact> {
    let mode = match &event.kind {
        EventKind::Other(kind) if kind == TRANSCRIPT_PERMISSION_MODE => {
            event.payload.get("permissionMode")?
        }
        _ if event.source == Source::Hook => {
            // A card files the hook that raised it under `hook`; every other hook
            // event is the hook's input as it arrived.
            let input = if event.kind == EventKind::ApprovalRequest {
                event.payload.get("hook")?
            } else {
                &event.payload
            };
            if input.get("agent_id").is_some_and(|agent| !agent.is_null()) {
                return None;
            }
            input.get("permission_mode")?
        }
        _ => return None,
    };
    Some(Fact {
        value: mode.as_str()?.to_string(),
        seq: event.seq,
    })
}

/// The model a `/model` receipt names, and whether it was `Kept` rather than
/// `Set`, from the content of a transcript user line — `ModelConfirmation.parse`
/// over the `.output` case of `LocalCommandLine.parse`.
fn model_receipt(content: &str) -> Option<(bool, String)> {
    let line = local_command_output(content)?;
    let (kept, name) = if let Some(rest) = line.strip_prefix("Set model to ") {
        let name = match rest.find(" and saved") {
            Some(end) => &rest[..end],
            None => rest,
        };
        (false, name)
    } else {
        (true, line.strip_prefix("Kept model as ")?)
    };
    let name = name.trim_matches(is_whitespace);
    (!name.is_empty()).then(|| (kept, name.to_string()))
}

/// The text of a `<local-command-stdout>` line, ANSI stripped and trimmed. An
/// unclosed tag yields the rest of the line, as on the phone.
fn local_command_output(content: &str) -> Option<String> {
    const OPEN: &str = "<local-command-stdout>";
    const CLOSE: &str = "</local-command-stdout>";
    let trimmed = content.trim_matches(is_whitespace_or_newline);
    if !trimmed.starts_with(OPEN) {
        return None;
    }
    let after = &trimmed[OPEN.len()..];
    let inner = after.find(CLOSE).map_or(after, |end| &after[..end]);
    Some(
        strip_ansi(inner)
            .trim_matches(is_whitespace_or_newline)
            .to_string(),
    )
}

/// Removes ECMA-48 CSI sequences: `ESC [`, any parameter and intermediate bytes
/// (0x20-0x3F), then one final byte (0x40-0x7E) if there is one. Anything else
/// stops the strip and stays — losing styling is fine, losing content is not.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.next_if_eq(&'[').is_some() {
            while chars
                .next_if(|c| ('\u{20}'..='\u{3f}').contains(c))
                .is_some()
            {}
            chars.next_if(|c| ('\u{40}'..='\u{7e}').contains(c));
            continue;
        }
        out.push(c);
    }
    out
}

/// Foundation's `CharacterSet.whitespaces`, as enumerated on macOS 26: general
/// category Zs, tab, and U+200B. Not `char::is_whitespace`, which differs on
/// U+200B and on the line separators.
fn is_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200b}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// Foundation's `CharacterSet.whitespacesAndNewlines`, as enumerated on macOS 26:
/// [`is_whitespace`] plus U+000A-U+000D, U+0085, U+2028 and U+2029.
fn is_whitespace_or_newline(c: char) -> bool {
    is_whitespace(c) || matches!(c, '\n'..='\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(seq: u64, kind: EventKind, source: Source, payload: serde_json::Value) -> Event {
        Event {
            seq,
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            session_id: "cc-1".into(),
            ts: format!("2026-10-14T10:00:{seq:02}.000Z"),
            kind,
            payload,
            source,
            source_event_id: None,
            turn_id: None,
            item_id: None,
        }
    }

    fn user_line(seq: u64, content: &str) -> Event {
        event(
            seq,
            EventKind::UserMessage,
            Source::Transcript,
            json!({"type": "user", "message": {"role": "user", "content": content}}),
        )
    }

    /// **The same strings, through the phone's parser and through this one.**
    ///
    /// Each case is a transcript user line's content; the expected column is what
    /// the iOS app's `LocalCommandLine.parse` then `ModelConfirmation.parse`
    /// produced for it, run on this machine from `ios/CodeConnect/Model/` at
    /// 7c34442 (`set:` / `kept:` and the name, or none).
    #[test]
    fn the_model_receipt_reads_every_string_exactly_as_the_phone_does() {
        const E: &str = "\u{1b}";
        let cases: Vec<(String, Option<&str>)> = vec![
            ("<local-command-stdout>Set model to claude-sonnet-5 and saved as your default for new sessions</local-command-stdout>".into(), Some("set:claude-sonnet-5")),
            (format!("<local-command-stdout>{E}[1mKept model as {E}[1mOpus 5{E}[22m</local-command-stdout>"), Some("kept:Opus 5")),
            ("<local-command-stdout>Set model to opus[1m] (claude-opus-5[1m])</local-command-stdout>".into(), Some("set:opus[1m] (claude-opus-5[1m])")),
            ("  \n<local-command-stdout>Set model to sonnet</local-command-stdout>\n".into(), Some("set:sonnet")),
            ("<local-command-stdout>Set model to </local-command-stdout>".into(), None),
            ("<local-command-stdout>Model 'bananas' not found</local-command-stdout>".into(), None),
            ("<command-name>/model</command-name>\n<command-args>sonnet</command-args>".into(), None),
            ("Set model to sonnet".into(), None),
            ("<local-command-stdout>Set model to sonnet".into(), Some("set:sonnet")),
            ("<local-command-stdout>Kept model as\u{a0}Haiku</local-command-stdout>".into(), None),
            ("<local-command-stdout>Set model to \u{a0}Sonnet 5\u{a0} and saved as default</local-command-stdout>".into(), Some("set:Sonnet 5")),
            ("<local-command-stdout>Set model to sonnet\tand saved</local-command-stdout>".into(), Some("set:sonnet\tand saved")),
            (format!("<local-command-stdout>{E}[2~Set model to x</local-command-stdout>"), Some("set:x")),
            (format!("<local-command-stdout>{E}[12\nSet model to x</local-command-stdout>"), Some("set:x")),
            ("<local-command-stdout>Set model to Sonnet\nsecond line</local-command-stdout>".into(), Some("set:Sonnet\nsecond line")),
            ("<local-command-stdout>Set model to x and saved and saved</local-command-stdout>".into(), Some("set:x")),
            ("<local-command-stdout>Kept model as x and saved</local-command-stdout>".into(), Some("kept:x and saved")),
            ("<local-command-caveat>Caveat: Set model to x</local-command-caveat>".into(), None),
            ("".into(), None),
            ("<local-command-stdout></local-command-stdout>".into(), None),
            (format!("<local-command-stdout>Set model to x{E}</local-command-stdout>"), Some("set:x\u{1b}")),
            ("<local-command-stdout>Set model to x\r\n</local-command-stdout>".into(), Some("set:x")),
            ("<local-command-stdout>Set model to Ünïcödé 🚀</local-command-stdout>".into(), Some("set:Ünïcödé 🚀")),
            (format!("<local-command-stdout>{E}[31mSet model to{E}[0m sonnet</local-command-stdout>"), Some("set:sonnet")),
            ("<local-command-stdout>Set model to \u{b}x</local-command-stdout>".into(), Some("set:\u{b}x")),
            ("<local-command-stdout>set model to sonnet</local-command-stdout>".into(), None),
            ("<local-command-stdout>Kept model as </local-command-stdout>".into(), None),
            ("<local-command-stdout>Set model to \u{3000}x\u{3000}</local-command-stdout>".into(), Some("set:x")),
            ("<local-command-stdout>x</local-command-stdout><local-command-stdout>Set model to y</local-command-stdout>".into(), None),
            ("<local-command-stdout> Set model to y</local-command-stdout>".into(), Some("set:y")),
            ("\u{2028}<local-command-stdout>Kept model as y\u{2029}</local-command-stdout>".into(), Some("kept:y")),
            (format!("<local-command-stdout>Set model to {E}[38;2;1;2;3mfable{E}[39m and saved</local-command-stdout>"), Some("set:fable")),
            ("\t<local-command-stdout>Set model to x\t\u{85}</local-command-stdout>".into(), Some("set:x")),
            ("<local-command-stdout>Set model to \u{1680}x\u{200b}</local-command-stdout>".into(), Some("set:x")),
            ("<local-command-stdout>Set model to x\u{85}y</local-command-stdout>".into(), Some("set:x\u{85}y")),
            ("<local-command-stdout>Set model to \u{85}x</local-command-stdout>".into(), Some("set:\u{85}x")),
            ("<local-command-stdout>Set model to \u{180e}x</local-command-stdout>".into(), Some("set:\u{180e}x")),
            ("<local-command-stdout>Set model to \u{c}x</local-command-stdout>".into(), Some("set:\u{c}x")),
        ];
        assert_eq!(
            cases.len(),
            38,
            "the table the phone was run on has 38 rows"
        );
        for (content, expected) in &cases {
            let got = model_receipt(content)
                .map(|(kept, name)| format!("{}:{name}", if kept { "kept" } else { "set" }));
            assert_eq!(got.as_deref(), *expected, "content {content:?}");
        }
    }

    /// The phone's `modelFact(of:)`: a `session_start` with a non-empty `model`,
    /// or a `/model` receipt on a user line. Both verbs are a fact about the model
    /// in force, and the provenance says which kind of event it was.
    #[test]
    fn the_model_comes_from_a_session_start_or_a_receipt() {
        let start = event(
            1,
            EventKind::SessionStart,
            Source::Hook,
            json!({"hook_event_name": "SessionStart", "model": "claude-opus-5[1m]"}),
        );
        assert_eq!(
            model_fact(&start),
            Some(ModelFact {
                value: "claude-opus-5[1m]".into(),
                seq: 1,
                source: ModelSource::SessionStart,
                at: "2026-10-14T10:00:01.000Z".into(),
            })
        );
        let kept = user_line(
            7,
            "<local-command-stdout>Kept model as claude-sonnet-5</local-command-stdout>",
        );
        assert_eq!(
            model_fact(&kept),
            Some(ModelFact {
                value: "claude-sonnet-5".into(),
                seq: 7,
                source: ModelSource::Command,
                at: "2026-10-14T10:00:07.000Z".into(),
            })
        );
        for silent in [
            event(
                2,
                EventKind::SessionStart,
                Source::Hook,
                json!({"model": ""}),
            ),
            event(
                3,
                EventKind::SessionStart,
                Source::Codex,
                json!({"thread_id": "t"}),
            ),
            event(
                4,
                EventKind::SessionStart,
                Source::Hook,
                json!({"model": 5}),
            ),
            // A receipt is read only off a user line, and only off a string content.
            event(
                5,
                EventKind::AgentMessage,
                Source::Transcript,
                json!({"message": {"content": "<local-command-stdout>Set model to x</local-command-stdout>"}}),
            ),
            event(
                6,
                EventKind::UserMessage,
                Source::Transcript,
                json!({"message": {"content": [{"type": "text", "text": "<local-command-stdout>Set model to x</local-command-stdout>"}]}}),
            ),
        ] {
            assert_eq!(model_fact(&silent), None, "{silent:?}");
        }
    }

    /// The mode is the main agent's: a hook carrying no `agent_id`, or Claude's
    /// own transcript line. A background agent's hook (measured: `bubble`, with an
    /// `effort` of its own) speaks for that agent, never for the run.
    #[test]
    fn only_the_main_agent_decides_the_permission_mode() {
        let main = event(
            3,
            EventKind::ToolCall,
            Source::Hook,
            json!({"hook_event_name": "PreToolUse", "permission_mode": "acceptEdits"}),
        );
        assert_eq!(
            permission_mode_fact(&main),
            Some(Fact {
                value: "acceptEdits".into(),
                seq: 3
            })
        );
        let line = event(
            4,
            EventKind::Other(TRANSCRIPT_PERMISSION_MODE.into()),
            Source::Transcript,
            json!({"type": "permission-mode", "permissionMode": "plan"}),
        );
        assert_eq!(
            permission_mode_fact(&line),
            Some(Fact {
                value: "plan".into(),
                seq: 4
            })
        );
        // A card files its hook under `hook`.
        let card = event(
            5,
            EventKind::ApprovalRequest,
            Source::Hook,
            json!({"card": {"permission_mode": "ignored"}, "hook": {"permission_mode": "default"}}),
        );
        assert_eq!(
            permission_mode_fact(&card).map(|fact| fact.value),
            Some("default".into())
        );

        let fork: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/claude/askuq-fork-permissionrequest-2.1.286.json"
        ))
        .unwrap();
        assert_eq!(fork["permission_mode"], "bubble", "the premise");
        assert!(fork["agent_id"].is_string(), "the premise");
        for background in [
            event(
                6,
                EventKind::ApprovalRequest,
                Source::Hook,
                json!({"card": {}, "hook": fork}),
            ),
            event(
                7,
                EventKind::Other("hook_PermissionRequest".into()),
                Source::Hook,
                fork.clone(),
            ),
            event(8, EventKind::ToolCall, Source::Hook, fork.clone()),
        ] {
            assert_eq!(permission_mode_fact(&background), None, "{background:?}");
        }
        // Only a hook's input is read for it: the same key on another source is
        // not the agent reporting its mode.
        let quoted = event(
            9,
            EventKind::UserMessage,
            Source::Transcript,
            json!({"permission_mode": "bypassPermissions"}),
        );
        assert_eq!(permission_mode_fact(&quoted), None);
    }

    /// Folded in any order, each fact is the newest event's, and a background
    /// agent's hook after the main agent's changes nothing.
    #[test]
    fn the_newest_deciding_event_wins_whatever_the_order() {
        let fork: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/claude/askuq-fork-permissionrequest-2.1.286.json"
        ))
        .unwrap();
        let events = [
            event(1, EventKind::SessionStart, Source::Hook, json!({"model": "claude-opus-5[1m]", "permission_mode": "default"})),
            user_line(5, "<local-command-stdout>Set model to claude-sonnet-5 and saved</local-command-stdout>"),
            event(6, EventKind::Other(TRANSCRIPT_PERMISSION_MODE.into()), Source::Transcript, json!({"permissionMode": "acceptEdits"})),
            event(9, EventKind::ApprovalRequest, Source::Hook, json!({"card": {}, "hook": fork})),
        ];
        for order in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1]] {
            let mut facts = SessionFacts::default();
            for index in order {
                facts.observe(&events[index]);
            }
            assert_eq!(
                facts.model.as_ref().map(|m| (m.value.as_str(), m.seq)),
                Some(("claude-sonnet-5", 5))
            );
            assert_eq!(
                facts.permission_mode,
                Some(Fact {
                    value: "acceptEdits".into(),
                    seq: 6
                })
            );
        }
    }
}
