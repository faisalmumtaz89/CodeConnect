//! Claude's `AskUserQuestion`, answered from the phone.
//!
//! The answer goes back through the `PermissionRequest` hook Claude is holding:
//! `allow` with `updatedInput` = the input as shown plus `answers` and
//! `annotations`. Claude validates none of it (measured on 2.1.286: a partial
//! answer and a label from nowhere are both accepted), so this module is the
//! only check there is. It builds exactly what Claude's own dialog builds when
//! the same choices are made at the keyboard, read from the 2.1.286 dialog:
//!
//! * a single choice is the option's label as the dialog shows it (whitespace
//!   runs folded to one space, trimmed), or the "Other" text as typed;
//! * several choices are those labels in the order they were picked, then the
//!   "Other" text, joined with `", "`; an item that itself contains `", "` or a
//!   `"` is written as a JSON string;
//! * a single-choice question whose options carry previews gets
//!   `annotations[question] = {preview, notes}` — the chosen option's preview
//!   (unless it is too long for the dialog to show) and the trimmed notes;
//! * `annotations` is always present, empty when nothing needs it.
//!
//! The dialog shows a label, and answers with it, only after rewriting what it
//! will not draw as written: invisible and format characters, controls, bidi
//! marks, variation selectors, tabs, more than 8 zero-width characters joined
//! to one character, overlong text. A question whose text holds any of that —
//! anything but letters, combining marks, numbers, punctuation, symbols, the
//! plain space and the zero-width joiner and non-joiner — or a label shown
//! twice is not offered to the phone at all: [`answerable`] says no, and the
//! card stays at the Mac. Better unanswerable here than answered with a string
//! the keyboard would not have produced.

use protocol::ws::QuestionAnswer;
use serde_json::{Map, Value};
use unicode_general_category::{get_general_category, GeneralCategory};

/// Previews longer than this (UTF-16 units) are withheld by the dialog, so the
/// keyboard's answer carries no preview for them.
const PREVIEW_SHOWN_MAX: usize = 2000;

/// Labels longer than this (UTF-16 units) are cut short by the dialog.
const LABEL_SHOWN_MAX: usize = 2000;

/// The most zero-width characters joined to one character that the dialog
/// shows as written: combining marks, the zero-width joiner and non-joiner, and
/// Hangul vowel and final jamo. Its width check walks the text grapheme by
/// grapheme and writes U+FFFD for one that does not add its own
/// `Bun.stringWidth` to the text before it; measured on 2.1.286, 8 such
/// characters after a letter are kept and a ninth is rewritten, whichever of
/// them they are, while an emoji modifier or a halfwidth sound mark, which have
/// width, start the count again.
const JOINED_SHOWN_MAX: usize = 8;

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

/// True when every question on this card can be answered from the phone and
/// the answer would be byte-identical to the keyboard's.
pub fn answerable(tool_input: &Value) -> bool {
    questions(tool_input).is_ok()
}

/// The `updatedInput` for these answers, or why they are refused. Nothing is
/// built from anything but the stored question and the indices given.
pub fn updated_input(tool_input: &Value, answers: &[QuestionAnswer]) -> Result<Value, String> {
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
                    "question {number} has no \"Other\" at the Mac, so it takes none here"
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
            .map(str::trim)
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
        .ok_or("the question is not an object")?;
    input.insert("answers".into(), chosen.into());
    input.insert("annotations".into(), annotations.into());
    Ok(input.into())
}

/// Whether a `PostToolUse` for this question carries the answer the phone sent:
/// the same `answers` and `annotations`. Anything else was answered at the Mac.
pub fn same_answer(sent: &Value, ran_with: &Value) -> bool {
    sent.get("answers") == ran_with.get("answers")
        && sent.get("annotations") == ran_with.get("annotations")
}

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
            if !shown_as_written(text, false) {
                return Err(format!(
                    "question {number}'s text is shown differently at the Mac"
                ));
            }
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
                .map(|multi| {
                    multi
                        .as_bool()
                        .ok_or(format!("question {number}'s multiSelect is not a flag"))
                })
                .transpose()?
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
                    .filter(|label| shown_as_written(label, false))
                    .ok_or(format!(
                        "question {number} has an option the phone cannot show as the Mac does"
                    ))?;
                let label = folded(label);
                if label.is_empty() || shown.iter().any(|(seen, _)| *seen == label) {
                    return Err(format!(
                        "question {number} has two options the Mac shows alike"
                    ));
                }
                let preview = match option.get("preview") {
                    None => None,
                    Some(Value::String(preview))
                        if preview.encode_utf16().count() > PREVIEW_SHOWN_MAX
                            || shown_as_written(preview, true) =>
                    {
                        (!preview.trim().is_empty()).then_some(preview.as_str())
                    }
                    Some(_) => {
                        return Err(format!(
                            "question {number} has a preview the phone cannot show as the Mac does"
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

/// A label as the dialog shows it: runs of spaces as one, trimmed.
fn folded(label: &str) -> String {
    label
        .split(' ')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether the dialog shows this text exactly as written: short enough not to
/// be cut, and only letters, combining marks, numbers, punctuation, symbols,
/// the plain space, the zero-width joiner and non-joiner (and line breaks in a
/// preview), none of them default-ignorable, with no more than
/// [`JOINED_SHOWN_MAX`] zero-width characters joined to one character.
/// Everything else is something the 2.1.286 dialog rewrites, drops or may
/// rewrite.
fn shown_as_written(text: &str, lines: bool) -> bool {
    use GeneralCategory::*;
    let mut joined = 0;
    text.encode_utf16().count() <= LABEL_SHOWN_MAX
        && text.chars().all(|c| {
            let category = get_general_category(c);
            if matches!(category, NonspacingMark | SpacingMark)
                || matches!(
                    c,
                    '\u{200c}' | '\u{200d}' | '\u{1160}'..='\u{11ff}' | '\u{d7b0}'..='\u{d7ff}'
                )
            {
                joined += 1;
            } else {
                joined = 0;
            }
            joined <= JOINED_SHOWN_MAX
                && !default_ignorable(c)
                && (c == ' '
                    || (lines && c == '\n')
                    || matches!(c, '\u{200c}' | '\u{200d}')
                    || matches!(
                        category,
                        UppercaseLetter
                            | LowercaseLetter
                            | TitlecaseLetter
                            | ModifierLetter
                            | OtherLetter
                            | DecimalNumber
                            | LetterNumber
                            | OtherNumber
                            | ConnectorPunctuation
                            | DashPunctuation
                            | OpenPunctuation
                            | ClosePunctuation
                            | InitialPunctuation
                            | FinalPunctuation
                            | OtherPunctuation
                            | MathSymbol
                            | CurrencySymbol
                            | ModifierSymbol
                            | OtherSymbol
                            | NonspacingMark
                            | SpacingMark
                    ))
        })
}

/// The default-ignorable characters among the categories [`shown_as_written`]
/// takes: the Hangul fillers, which look like letters, the combining grapheme
/// joiner, the Khmer inherent vowels, the Mongolian free variation selectors and
/// every variation selector. The dialog turns most of these into U+FFFD or
/// drops them; the Mongolian selectors and U+FE00..U+FE0D it lets through to a
/// width check not measured here, so they are refused with the rest.
fn default_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{34f}'
            | '\u{115f}'
            | '\u{1160}'
            | '\u{17b4}'
            | '\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{ffa0}'
            | '\u{e0100}'..='\u{e01ef}'
    )
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
        let built = updated_input(&card_input(), &keyboard_choices()).unwrap();
        assert_eq!(built["answers"], keyboard["tool_input"]["answers"]);
        assert_eq!(built["annotations"], keyboard["tool_input"]["annotations"]);
        assert_eq!(built["questions"], keyboard["tool_input"]["questions"]);
        assert!(same_answer(&built, &keyboard["tool_input"]));
    }

    #[test]
    fn several_choices_keep_the_order_they_were_picked_in() {
        let built = updated_input(
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
        let built = updated_input(
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

    #[test]
    fn a_label_is_answered_as_the_dialog_shows_it() {
        let input = json!({"questions": [{"question": "Q",
            "options": [{"label": "  Two   spaces "}, {"label": "B"}]}]});
        let built = updated_input(&input, &[pick(&[0])]).unwrap();
        assert_eq!(built["answers"]["Q"], "Two spaces");
    }

    /// Claude accepts all of these (measured), so refusing them is this module's job.
    #[test]
    fn incomplete_or_foreign_answers_are_refused() {
        let input = card_input();
        let mut partial = keyboard_choices();
        partial.truncate(2);
        assert!(updated_input(&input, &partial)
            .unwrap_err()
            .contains("2 answer(s) for 4"));

        let mut empty = keyboard_choices();
        empty[0] = QuestionAnswer::default();
        assert!(updated_input(&input, &empty)
            .unwrap_err()
            .contains("question 1 has no answer"));

        let mut out_of_range = keyboard_choices();
        out_of_range[0] = pick(&[2]);
        assert!(updated_input(&input, &out_of_range)
            .unwrap_err()
            .contains("no option 3"));

        let mut two_singles = keyboard_choices();
        two_singles[0] = pick(&[0, 1]);
        assert!(updated_input(&input, &two_singles)
            .unwrap_err()
            .contains("takes one answer"));

        let mut label_and_other = keyboard_choices();
        label_and_other[0].other = Some("x".into());
        assert!(updated_input(&input, &label_and_other)
            .unwrap_err()
            .contains("takes one answer"));

        let mut twice = keyboard_choices();
        twice[1] = pick(&[0, 0]);
        assert!(updated_input(&input, &twice).unwrap_err().contains("twice"));

        let mut blank_other = keyboard_choices();
        blank_other[2].other = Some("  ".into());
        assert!(updated_input(&input, &blank_other)
            .unwrap_err()
            .contains("empty"));

        let mut notes_elsewhere = keyboard_choices();
        notes_elsewhere[0].notes = Some("n".into());
        assert!(updated_input(&input, &notes_elsewhere)
            .unwrap_err()
            .contains("takes no notes"));

        let mut too_many = keyboard_choices();
        too_many.push(pick(&[0]));
        assert!(updated_input(&input, &too_many).is_err());
    }

    #[test]
    fn a_question_the_dialog_would_rewrite_is_not_offered() {
        assert!(answerable(&card_input()));
        for input in [
            json!({}),
            json!({"questions": []}),
            json!({"questions": [{"question": "Q", "options": []}]}),
            json!({"questions": [{"question": "Q", "kind": "text", "options": [{"label": "A"}]}]}),
            json!({"questions": [{"question": "Q", "options": [{"label": "A\nB"}]}]}),
            json!({"questions": [{"question": "Q", "options": [{"label": "A\u{1b}[31m"}]}]}),
            json!({"questions": [{"question": "Q", "options": [{"label": "A"}, {"label": "A "}]}]}),
            json!({"questions": [{"question": "Q", "options": [{"label": "A", "preview": "x\ty"}]}]}),
            json!({"questions": [{"question": "Q", "options": [{"label": "A"}]},
                                 {"question": "Q", "options": [{"label": "B"}]}]}),
        ] {
            assert!(!answerable(&input), "{input}");
        }
        // What 2.1.286's dialog shows differently from the text it was given,
        // as measured: zero-width space, BOM, soft hyphen and the Hangul fillers
        // become U+FFFD, a variation selector is dropped, a ninth zero-width
        // character joined to one character is rewritten, and an overlong label
        // is cut.
        let marks = |n: usize| "\u{301}".repeat(n);
        let nine_marks = format!("a{}", marks(9));
        // Joiners and Hangul vowel and final jamo count with the marks.
        let joined = [
            format!("a\u{200c}{}", marks(8)),
            format!("\u{1100}\u{1161}{}", marks(8)),
            format!("a{}\u{200d}{}", marks(4), marks(4)),
            format!("e\u{200d}{}", marks(8)),
            format!("a{}b", "\u{200d}".repeat(20)),
            format!("a{}b", "\u{200c}".repeat(9)),
            format!("a{}\u{200c}", marks(8)),
            format!("x{}", "\u{200d}".repeat(9)),
        ];
        for label in [
            "Zero\u{200b}Width",
            "Bom\u{feff}End",
            "Soft\u{ad}Hyphen",
            "Heart \u{2764}\u{fe0f}",
            "Han\u{3164}gul",
            "Half\u{ffa0}width",
            "\u{115f}\u{1160}",
            "Joiner\u{34f}x",
            &nine_marks,
            &joined[0],
            &joined[1],
            &joined[2],
            &joined[3],
            &joined[4],
            &joined[5],
            &joined[6],
            &joined[7],
            "No\u{a0}break",
            "Tab\tbed",
            "Right\u{202e}left",
            &"x".repeat(LABEL_SHOWN_MAX + 1),
        ] {
            let option = json!({"questions": [{"question": "Q", "options": [{"label": label}]}]});
            assert!(!answerable(&option), "{label:?}");
            let question = json!({"questions": [{"question": label, "options": [{"label": "A"}]}]});
            assert!(!answerable(&question), "{label:?} as the question");
            let preview = json!({"questions": [{"question": "Q",
                "options": [{"label": "A", "preview": label}, {"label": "B"}]}]});
            let annotated = label.encode_utf16().count() <= PREVIEW_SHOWN_MAX;
            assert_eq!(answerable(&preview), !annotated, "{label:?} as a preview");
        }
        // And what it shows exactly as written, as measured: scripts written
        // with combining marks, the zero-width joiner and non-joiner, and 8
        // zero-width characters joined to one character, whichever they are; an
        // emoji modifier or a halfwidth sound mark starts the count again.
        for label in [
            "caf\u{e9} \u{2615}".to_string(),
            "\u{65e5}\u{672c}\u{8a9e}".to_string(),
            "x".repeat(LABEL_SHOWN_MAX),
            "\u{939}\u{93f}\u{902}\u{926}\u{940} \u{92e}\u{947}\u{902}".to_string(),
            "\u{e17}\u{e35}\u{e48}\u{e19}\u{e35}\u{e48}".to_string(),
            "\u{5e9}\u{5b8}\u{5c1}\u{5dc}\u{5d5}\u{5b9}\u{5dd}".to_string(),
            "e\u{301}clair".to_string(),
            format!("a{}", marks(8)),
            format!("a\u{200c}{}", marks(7)),
            format!("\u{1100}\u{1161}{}", marks(7)),
            format!("a{}b", "\u{200d}".repeat(8)),
            format!("a{}\u{200d}{}", marks(3), marks(4)),
            format!("\u{1100}\u{1161}\u{11a8}{}", marks(6)),
            format!("\u{1f600}\u{200d}{}", marks(7)),
            format!("\u{915}\u{94d}\u{200d}{}", marks(6)),
            format!("a\u{1f3fb}{}", marks(8)),
            format!("a\u{ff9e}{}", marks(8)),
            "\u{1f468}\u{200d}\u{1f4bb} Dev".to_string(),
            "\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}".to_string(),
        ] {
            let option = json!({"questions": [{"question": "Q", "options": [
                {"label": label, "preview": "line one\nline two"}, {"label": "B"}]}]});
            assert!(answerable(&option), "{label:?}");
        }
        assert!(answerable(
            &json!({"questions": [{"question": "Q", "kind": "choice", "options": [{"label": "A"}]}]})
        ));
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
        assert!(updated_input(&input, std::slice::from_ref(&other)).is_err());
        let plain = json!({"questions": [{"question": "Q",
            "options": [{"label": "A"}, {"label": "B"}]}]});
        assert!(updated_input(&plain, &[other]).is_ok());
    }

    #[test]
    fn a_preview_the_dialog_withholds_is_not_annotated() {
        let long = "x".repeat(PREVIEW_SHOWN_MAX + 1);
        let input = json!({"questions": [{"question": "Q",
            "options": [{"label": "A", "preview": long}, {"label": "B", "preview": "  "}]}]});
        let built = updated_input(&input, &[pick(&[0])]).unwrap();
        assert_eq!(built["annotations"], json!({}));
        let noted = updated_input(
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

    #[test]
    fn a_keyboard_answer_is_told_apart_from_the_phone_s() {
        let keyboard: Value = serde_json::from_str(KEYBOARD).unwrap();
        let mut other = keyboard_choices();
        other[0] = pick(&[1]);
        let built = updated_input(&card_input(), &other).unwrap();
        assert!(!same_answer(&built, &keyboard["tool_input"]));
    }
}
