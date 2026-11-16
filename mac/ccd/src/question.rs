//! Claude's `AskUserQuestion`, answered from the phone.
//!
//! The answer goes back as `allow` with `updatedInput` = the input as shown plus
//! `answers` and `annotations`, through the agent host's return to the SDK's
//! `canUseTool` ([`host_updated_input`]). Claude validates none of it (measured on
//! 2.1.286: a partial answer and a label from nowhere are both accepted), so this
//! module is the only check there is. It builds exactly what Claude's own dialog builds when
//! the same choices are made at the keyboard, read from the 2.1.286 dialog:
//!
//! * a single choice is the option's label as the dialog shows it (whitespace
//!   runs folded to one space, trimmed), or the "Other" text as typed;
//! * several choices are those labels in the order they were picked, then the
//!   "Other" text, joined with `", "`; an item that itself contains `", "` or a
//!   `"` is written as a JSON string;
//! * a single-choice question whose options carry previews gets
//!   `annotations[question] = {preview, notes}` — the chosen option's preview
//!   (unless it is too long for the dialog to show) and the notes, trimmed
//!   as the dialog trims them ([`js_trim`]);
//! * `annotations` is always present, empty when nothing needs it.
//!
//! A question asked twice, or with two options the dialog shows alike, is
//! refused: no answer could say which one was meant.

use protocol::ws::QuestionAnswer;
use serde_json::{Map, Value};

/// Previews longer than this (UTF-16 units) are withheld by the dialog, so the
/// keyboard's answer carries no preview for them.
const PREVIEW_SHOWN_MAX: usize = 2000;
/// One question as the dialog reads it.
struct Question<'a> {
    text: &'a str,
    multi: bool,
    /// Each option's label as the dialog shows it, and its preview if any.
    options: Vec<(String, Option<&'a str>)>,
}

impl Question<'_> {
    /// The dialog takes notes only on a single choice with a preview to note.
    fn takes_notes(&self) -> bool {
        !self.multi && self.options.iter().any(|(_, preview)| preview.is_some())
    }
}

/// The `updatedInput` for these answers to a hosted run's question, or why they
/// are refused. Nothing is built from anything but the stored question and the
/// indices given.
pub fn host_updated_input(tool_input: &Value, answers: &[QuestionAnswer]) -> Result<Value, String> {
    let questions = questions(tool_input)?;
    if answers.len() != questions.len() {
        return Err(format!(
            "{} answer(s) for {} question(s); every question needs exactly one answer",
            answers.len(),
            questions.len()
        ));
    }
    let mut chosen = Map::new();
    let mut annotations = Map::new();
    for (number, (question, answer)) in questions.iter().zip(answers).enumerate() {
        let number = number + 1;
        let mut items = Vec::new();
        for (position, &index) in answer.selected.iter().enumerate() {
            let Some((label, _)) = question.options.get(index as usize) else {
                return Err(format!("question {number} has no option {}", index + 1));
            };
            if answer.selected[..position].contains(&index) {
                return Err(format!(
                    "question {number} picks option {} twice",
                    index + 1
                ));
            }
            items.push(label.clone());
        }
        if let Some(other) = &answer.other {
            if question.takes_notes() {
                return Err(format!(
                    "question {number} has no \"Other\" at the computer, so it takes none here"
                ));
            }
            if other.trim().is_empty() {
                return Err(format!("question {number}'s \"Other\" answer is empty"));
            }
            items.push(other.clone());
        }
        let value = if question.multi {
            if items.is_empty() {
                return Err(format!("question {number} has no answer"));
            }
            join_choices(&items)
        } else {
            match items.as_slice() {
                [only] => only.clone(),
                [] => return Err(format!("question {number} has no answer")),
                _ => return Err(format!("question {number} takes one answer")),
            }
        };
        let notes = answer
            .notes
            .as_deref()
            .map(js_trim)
            .filter(|notes| !notes.is_empty());
        if notes.is_some() && !question.takes_notes() {
            return Err(format!("question {number} takes no notes"));
        }
        if question.takes_notes() {
            let preview = question
                .options
                .iter()
                .find(|(label, _)| *label == value)
                .and_then(|(_, preview)| *preview)
                .filter(|preview| preview.encode_utf16().count() <= PREVIEW_SHOWN_MAX);
            let mut note = Map::new();
            if let Some(preview) = preview {
                note.insert("preview".into(), preview.into());
            }
            if let Some(notes) = notes {
                note.insert("notes".into(), notes.into());
            }
            if !note.is_empty() {
                annotations.insert(question.text.to_string(), note.into());
            }
        }
        chosen.insert(question.text.to_string(), value.into());
    }
    let mut input = tool_input
        .as_object()
        .cloned()
        .expect("questions() read the card's questions from this object");
    input.insert("answers".into(), chosen.into());
    input.insert("annotations".into(), annotations.into());
    Ok(input.into())
}

/// The card's questions, or why it cannot be answered.
fn questions(tool_input: &Value) -> Result<Vec<Question<'_>>, String> {
    let list = tool_input
        .get("questions")
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty())
        .ok_or("the card has no questions")?;
    let mut seen = Vec::new();
    list.iter()
        .enumerate()
        .map(|(number, question)| {
            let number = number + 1;
            let text = question
                .get("question")
                .and_then(Value::as_str)
                .ok_or(format!("question {number} has no text"))?;
            if seen.contains(&text) {
                return Err(format!("question {number} repeats an earlier question"));
            }
            seen.push(text);
            if question
                .get("kind")
                .is_some_and(|kind| kind.as_str() != Some("choice"))
            {
                return Err(format!("question {number} is not a choice"));
            }
            let multi = question
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .filter(|options| !options.is_empty())
                .ok_or(format!("question {number} has no options"))?;
            let mut shown = Vec::new();
            for option in options {
                let label = option
                    .get("label")
                    .and_then(Value::as_str)
                    .ok_or(format!(
                        "question {number} has an option the phone cannot show as the computer does"
                    ))?;
                let label = folded(label);
                if label.is_empty() || shown.iter().any(|(seen, _)| *seen == label) {
                    return Err(format!(
                        "question {number} has two options the computer shows alike"
                    ));
                }
                let preview = match option.get("preview") {
                    None => None,
                    Some(Value::String(preview)) => {
                        (!preview.trim().is_empty()).then_some(preview.as_str())
                    }
                    Some(_) => {
                        return Err(format!(
                            "question {number} has a preview the phone cannot show as the computer does"
                        ))
                    }
                };
                shown.push((label, preview));
            }
            Ok(Question {
                text,
                multi,
                options: shown,
            })
        })
        .collect()
}

/// `", "`-joined, as the dialog joins several choices.
fn join_choices(items: &[String]) -> String {
    items
        .iter()
        .map(|item| {
            if item.contains(", ") || item.contains('"') {
                serde_json::to_string(item).expect("a string always encodes")
            } else {
                item.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `text` trimmed as JavaScript's `String.prototype.trim` trims it, which is
/// how the dialog trims notes: ECMAScript white space (tab, vertical tab, form
/// feed, U+FEFF and the space separators) and line terminators. Neither Rust's
/// nor Swift's trim is this set: they differ on U+FEFF, U+0085 and U+200B.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c| {
        matches!(
            c,
            '\u{9}'..='\u{D}'
                | ' '
                | '\u{A0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
        )
    })
}

/// A label as the dialog shows it: runs of spaces as one, trimmed.
fn folded(label: &str) -> String {
    label
        .split(' ')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A live 2.1.286 session's 4-question `PermissionRequest`, and the
    /// `PostToolUse` of a run where the same choices were typed at the keyboard.
    const HOOKS: &str = include_str!("../../../fixtures/claude/askuq-4q-2.1.286.jsonl");
    const KEYBOARD: &str =
        include_str!("../../../fixtures/claude/askuq-4q-keyboard-posttooluse-2.1.286.json");

    fn card_input() -> Value {
        HOOKS
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|hook| hook["hook_event_name"] == "PermissionRequest")
            .unwrap()["tool_input"]
            .clone()
    }

    fn pick(selected: &[u32]) -> QuestionAnswer {
        QuestionAnswer {
            selected: selected.to_vec(),
            ..Default::default()
        }
    }

    fn keyboard_choices() -> Vec<QuestionAnswer> {
        vec![
            pick(&[0]),
            pick(&[0, 2]),
            QuestionAnswer {
                other: Some("Archive it under docs/plans, café ☕".into()),
                ..Default::default()
            },
            QuestionAnswer {
                selected: vec![1],
                notes: Some("wider screens only".into()),
                other: None,
            },
        ]
    }

    /// The whole claim: the phone's answer equals the keyboard's, field for
    /// field, on every shape the card has (single, several, "Other" with
    /// unicode, a preview with notes).
    #[test]
    fn the_phone_answer_is_what_the_keyboard_produced() {
        let keyboard: Value = serde_json::from_str(KEYBOARD).unwrap();
        let built = host_updated_input(&card_input(), &keyboard_choices()).unwrap();
        assert_eq!(built["answers"], keyboard["tool_input"]["answers"]);
        assert_eq!(built["annotations"], keyboard["tool_input"]["annotations"]);
        assert_eq!(built["questions"], keyboard["tool_input"]["questions"]);
    }

    #[test]
    fn several_choices_keep_the_order_they_were_picked_in() {
        let built = host_updated_input(
            &card_input(),
            &[pick(&[1]), pick(&[2, 0]), pick(&[0]), pick(&[0])],
        )
        .unwrap();
        assert_eq!(
            built["answers"]["Which checks should run before release?"],
            "Soak test, Unit tests"
        );
        // A preview chosen without notes still carries the preview, as the dialog does.
        assert_eq!(
            built["annotations"]["Which layout do you prefer?"],
            json!({"preview": "+------+\n| A    |\n+------+\n| B    |\n+------+"})
        );
        assert_eq!(built["annotations"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn a_choice_that_contains_the_separator_is_quoted() {
        let input = json!({"questions": [{"question": "Q", "multiSelect": true,
            "options": [{"label": "a, b"}, {"label": "say \"hi\""}, {"label": "c"}]}]});
        let built = host_updated_input(
            &input,
            &[QuestionAnswer {
                selected: vec![0, 2, 1],
                other: Some("d, e".into()),
                notes: None,
            }],
        )
        .unwrap();
        assert_eq!(built["answers"]["Q"], r#""a, b", c, "say \"hi\"", "d, e""#);
        assert_eq!(built["annotations"], json!({}));
    }

    /// **B6 part 2a:** a hosted run's question is answered whatever the terminal would
    /// do to its text: question text with a tab and a variation selector (⚠️), a label
    /// with one, and a preview indented with a tab are taken as written.
    #[test]
    fn a_hosted_question_takes_text_the_terminal_would_rewrite() {
        let input = json!({"questions": [{"question": "Which\tstart ⚠️?", "options": [
            {"label": "⚠️ Now", "preview": "if ready {\n\tstart()\n}"},
            {"label": "Later", "preview": "wait()"}]}]});
        let built = host_updated_input(&input, &[pick(&[0])]).unwrap();
        assert_eq!(built["answers"]["Which\tstart ⚠️?"], "⚠️ Now");
        assert_eq!(
            built["annotations"]["Which\tstart ⚠️?"],
            json!({"preview": "if ready {\n\tstart()\n}"})
        );
        assert!(host_updated_input(&input, &[pick(&[1])]).is_ok());
        // The structure is still checked: one answer per question, in range.
        assert!(host_updated_input(&input, &[]).is_err());
        assert!(host_updated_input(&input, &[pick(&[2])]).is_err());
    }

    #[test]
    fn a_label_is_answered_as_the_dialog_shows_it() {
        let input = json!({"questions": [{"question": "Q",
            "options": [{"label": "  Two   spaces "}, {"label": "B"}]}]});
        let built = host_updated_input(&input, &[pick(&[0])]).unwrap();
        assert_eq!(built["answers"]["Q"], "Two spaces");
    }

    /// Claude accepts all of these (measured), so refusing them is this module's job.
    #[test]
    fn incomplete_or_foreign_answers_are_refused() {
        let input = card_input();
        let mut partial = keyboard_choices();
        partial.truncate(2);
        assert!(host_updated_input(&input, &partial)
            .unwrap_err()
            .contains("2 answer(s) for 4"));

        let mut empty = keyboard_choices();
        empty[0] = QuestionAnswer::default();
        assert!(host_updated_input(&input, &empty)
            .unwrap_err()
            .contains("question 1 has no answer"));

        let mut out_of_range = keyboard_choices();
        out_of_range[0] = pick(&[2]);
        assert!(host_updated_input(&input, &out_of_range)
            .unwrap_err()
            .contains("no option 3"));

        let mut two_singles = keyboard_choices();
        two_singles[0] = pick(&[0, 1]);
        assert!(host_updated_input(&input, &two_singles)
            .unwrap_err()
            .contains("takes one answer"));

        let mut label_and_other = keyboard_choices();
        label_and_other[0].other = Some("x".into());
        assert!(host_updated_input(&input, &label_and_other)
            .unwrap_err()
            .contains("takes one answer"));

        let mut twice = keyboard_choices();
        twice[1] = pick(&[0, 0]);
        assert!(host_updated_input(&input, &twice)
            .unwrap_err()
            .contains("twice"));

        let mut blank_other = keyboard_choices();
        blank_other[2].other = Some("  ".into());
        assert!(host_updated_input(&input, &blank_other)
            .unwrap_err()
            .contains("empty"));

        let mut notes_elsewhere = keyboard_choices();
        notes_elsewhere[0].notes = Some("n".into());
        assert!(host_updated_input(&input, &notes_elsewhere)
            .unwrap_err()
            .contains("takes no notes"));

        let mut too_many = keyboard_choices();
        too_many.push(pick(&[0]));
        assert!(host_updated_input(&input, &too_many).is_err());
    }

    /// A card of a shape the dialog does not draw takes no answer at all: no
    /// questions, a question that is not a choice or offers nothing, two options
    /// shown alike, and a question asked twice.
    #[test]
    fn a_card_of_a_shape_the_dialog_does_not_draw_takes_no_answer() {
        let one = |options: Value| json!({"question": "Q", "options": options});
        for (input, refusal) in [
            (json!({"questions": []}), "has no questions"),
            (
                json!({"questions": [{"question": "Q", "kind": "text",
                    "options": [{"label": "A"}]}]}),
                "is not a choice",
            ),
            (json!({"questions": [one(json!([]))]}), "has no options"),
            (
                json!({"questions": [one(json!([{"label": "A  b"}, {"label": "A b"}]))]}),
                "shows alike",
            ),
            (
                json!({"questions": [one(json!([{"label": "A"}])), one(json!([{"label": "A"}]))]}),
                "repeats an earlier question",
            ),
        ] {
            let refused = host_updated_input(&input, &[pick(&[0])]).unwrap_err();
            assert!(refused.contains(refusal), "{input}: {refused}");
        }
    }

    /// A single choice whose options carry previews is drawn without "Type
    /// something": the keyboard cannot answer it with "Other", so the phone may not.
    #[test]
    fn a_question_with_previews_takes_no_other() {
        let input = json!({"questions": [{"question": "Q",
            "options": [{"label": "A", "preview": "a"}, {"label": "B", "preview": "b"}]}]});
        let other = QuestionAnswer {
            other: Some("something else".into()),
            ..Default::default()
        };
        assert!(host_updated_input(&input, std::slice::from_ref(&other)).is_err());
        let plain = json!({"questions": [{"question": "Q",
            "options": [{"label": "A"}, {"label": "B"}]}]});
        assert!(host_updated_input(&plain, &[other]).is_ok());
    }

    #[test]
    fn a_preview_the_dialog_withholds_is_not_annotated() {
        let long = "x".repeat(PREVIEW_SHOWN_MAX + 1);
        let input = json!({"questions": [{"question": "Q",
            "options": [{"label": "A", "preview": long}, {"label": "B", "preview": "  "}]}]});
        let built = host_updated_input(&input, &[pick(&[0])]).unwrap();
        assert_eq!(built["annotations"], json!({}));
        let noted = host_updated_input(
            &input,
            &[QuestionAnswer {
                selected: vec![0],
                notes: Some(" keep it ".into()),
                other: None,
            }],
        )
        .unwrap();
        assert_eq!(noted["annotations"], json!({"Q": {"notes": "keep it"}}));
    }

    /// Notes are trimmed as the dialog trims them, with JavaScript's `trim`
    /// (ECMAScript white space and line terminators): U+FEFF and U+00A0 go,
    /// U+0085, U+200B and U+180E stay. Notes that trim to nothing are not sent.
    #[test]
    fn notes_are_trimmed_as_the_dialog_trims_them() {
        let input = json!({"questions": [{"question": "Q",
            "options": [{"label": "A", "preview": "a"}, {"label": "B", "preview": "b"}]}]});
        let noted = |notes: &str| {
            host_updated_input(
                &input,
                &[QuestionAnswer {
                    selected: vec![0],
                    notes: Some(notes.into()),
                    other: None,
                }],
            )
            .unwrap()["annotations"]["Q"]["notes"]
                .clone()
        };
        assert_eq!(noted("\u{feff}keep it\u{feff}"), "keep it");
        assert_eq!(noted("\u{a0}keep it\u{a0}"), "keep it");
        assert_eq!(noted("\u{85}keep it\u{85}"), "\u{85}keep it\u{85}");
        assert_eq!(noted("\u{200b}keep it\u{200b}"), "\u{200b}keep it\u{200b}");
        assert_eq!(noted("\u{180e}keep it\u{180e}"), "\u{180e}keep it\u{180e}");
        assert_eq!(
            noted("\u{3000}\u{2028} keep\u{feff}it \t\n"),
            "keep\u{feff}it"
        );
        // Each of the rest of the set on its own, on both ends.
        for c in [
            '\u{b}', '\u{c}', '\r', '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}',
            '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200a}',
            '\u{2029}', '\u{202f}', '\u{205f}',
        ] {
            assert_eq!(noted(&format!("{c}keep it{c}")), "keep it", "{c:?}");
        }
        assert_eq!(
            noted("\u{feff} \u{a0}"),
            Value::Null,
            "nothing left, no notes"
        );
    }
}
