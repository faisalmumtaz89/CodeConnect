//! What may be written to `broker.log`, and in what form.
//!
//! Every refusal this broker takes is reported through an audit sink that production wires
//! to a durable `broker.log`, read by operators and by the live gates. The classifier's
//! inputs are **attacker-chosen bytes**: a method name, a params key at any nesting, a
//! request id, a thread id, an ownership value. Interpolating any of them verbatim into a
//! log line is a log-injection channel — a key of
//! `"zzz\n2026-08-25 broker: forward (ownership request: fingerprint asserted)"` writes a
//! forged line into the very file a gate greps.
//!
//! The rule this module enforces is therefore: **a refusal detail carries fixed vocabulary,
//! counts, and shapes — never client-chosen text**, with exactly THREE narrow exceptions,
//! all of which are *grammar-gated* rather than trusted:
//!
//! * [`method`] — every method in the pinned 0.147 census (134 of them, the widest set this
//!   broker will ever be asked about) matches `[a-z][A-Za-z0-9_/]*` and is at most 40 bytes
//!   (longest: `externalAgentConfig/import/readHistories`). A string matching that grammar
//!   cannot contain a newline, a control byte, or a quote, so it cannot forge a log line;
//!   [`MAX_LOGGED_METHOD_BYTES`] adds headroom over the measured maximum. Anything else is
//!   reported by shape only.
//! * [`thread_id`] — MEASURED across every captured frame: a thread id is a strict
//!   LOWERCASE UUID, `8-4-4-4-12` hex with dashes, exactly 36 bytes (e.g.
//!   `01a0127a-c6f4-70d1-b3a3-0742f8fd0d86`). Anything else is reported by shape only.
//! * [`request_id`] — an INTEGER id is a fixed-width `i64` with no representable control
//!   byte, so it is echoed unconditionally. A STRING id is echoed only when it is a plain
//!   identifier — ASCII alphanumerics plus `-`, `_`, `.`, `:` — within
//!   [`MAX_REQUEST_ID_BYTES`], which covers every id form measured on the wire
//!   (`startup-thread-start-<uuid>`, bare integers). Anything else is reported by byte count
//!   only.
//!
//! Keeping the conforming forms readable is deliberate: an operator diagnosing a refusal
//! needs to know *which* method, *which* thread and *which* request, and a value drawn from a
//! grammar with no control bytes cannot be a log-injection vector. Everything outside those
//! three grammars — params keys at any nesting, ownership values, workspace paths — is
//! rendered as a shape and a byte count and nothing else.

use std::borrow::Cow;

use serde_json::Value;

use crate::message::RequestId;
use crate::session::MAX_REQUEST_ID_BYTES;

/// The longest method name this broker will echo into the audit log. The pinned 0.147
/// census tops out at 40 bytes (`externalAgentConfig/import/readHistories`); 64 leaves
/// headroom for a future rename without ever logging an unbounded client string.
pub const MAX_LOGGED_METHOD_BYTES: usize = 64;

/// A JSON value's SHAPE for an audit-log refusal detail: its JSON type plus a size, never
/// the value itself. Workspace paths, ownership tokens and instruction blobs are exactly
/// the things that must not be logged.
pub fn value_shape(v: Option<&Value>) -> String {
    match v {
        None => "absent".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(_)) => "bool".to_string(),
        Some(Value::Number(_)) => "number".to_string(),
        Some(Value::String(s)) => format!("string(len={})", s.len()),
        Some(Value::Array(a)) => format!("array(len={})", a.len()),
        Some(Value::Object(o)) => format!("object(keys={})", o.len()),
    }
}

/// Is `m` a method name drawn from the grammar the pinned census satisfies?
///
/// `[a-z][A-Za-z0-9_/]*`, at most [`MAX_LOGGED_METHOD_BYTES`]. Verified against
/// `schema-0.147/methods-{stable,experimental}.json` by
/// [`tests::every_pinned_method_is_loggable`].
fn is_census_grammar_method(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= MAX_LOGGED_METHOD_BYTES
        && m.starts_with(|c: char| c.is_ascii_lowercase())
        && m.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_')
}

/// A client-supplied METHOD name, safe to write to the audit log.
///
/// A method matching the census grammar is echoed (an operator needs to know which method
/// was refused, and the grammar admits no control byte, quote or newline). Anything else —
/// which by construction is not a method this broker could ever forward — is reported as a
/// shape and a byte count, so an unknown-method refusal cannot carry attacker text.
pub fn method(m: &str) -> Cow<'_, str> {
    if is_census_grammar_method(m) {
        Cow::Borrowed(m)
    } else {
        Cow::Owned(format!("<non-conforming method, {} bytes>", m.len()))
    }
}

/// MEASURED: every thread id on the live wire is a strict LOWERCASE UUID — `8-4-4-4-12`
/// lowercase hex with dashes, length exactly 36 — verified across every captured frame
/// (`fixtures/codex/*.jsonl`, `fixtures/codex/*.json`).
pub fn is_wire_thread_id(s: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    if s.len() != 36 {
        return false;
    }
    let mut parts = s.split('-');
    for want in GROUPS {
        let Some(part) = parts.next() else {
            return false;
        };
        if part.len() != want || !part.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
            return false;
        }
    }
    parts.next().is_none()
}

/// A THREAD id, safe to write to the audit log.
///
/// A conforming id is echoed; anything else reports only its byte length, never its text —
/// a `threadId` is client-chosen on `turn/start` and `thread/resume`, so it is a direct
/// log-injection channel.
pub fn thread_id(id: &str) -> Cow<'_, str> {
    if is_wire_thread_id(id) {
        Cow::Borrowed(id)
    } else {
        Cow::Owned(format!("<non-conforming id, {} bytes>", id.len()))
    }
}

/// A JSON-RPC REQUEST id, safe to write to the audit log.
///
/// An integer id is a fixed-width `i64` with no representable control byte, so it is echoed.
/// A string id is echoed only when it is a plain identifier — ASCII alphanumerics plus
/// `-`, `_`, `.` and `:` — within [`MAX_REQUEST_ID_BYTES`]; that covers every id form
/// measured on the wire (`startup-thread-start-<uuid>`, bare integers). Anything else
/// reports only a byte count.
pub fn request_id(id: &RequestId) -> String {
    match id {
        RequestId::Int(n) => n.to_string(),
        RequestId::Str(s) => {
            let plain = !s.is_empty()
                && s.len() <= MAX_REQUEST_ID_BYTES
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
            if plain {
                format!("{s:?}")
            } else {
                format!("<non-conforming request id, {} bytes>", s.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The two grammars above are only sound if the real vocabulary satisfies them. This
    /// asserts the METHOD grammar against the pinned census itself, so a future schema bump
    /// that introduced a method outside the grammar fails here rather than silently
    /// degrading every unknown-method refusal to a byte count.
    #[test]
    fn every_pinned_method_is_loggable() {
        for bundle in [
            include_str!("../schema-0.147/methods-stable.json"),
            include_str!("../schema-0.147/methods-experimental.json"),
        ] {
            let v: Value = serde_json::from_str(bundle).expect("census parses");
            for key in ["client_requests", "client_notifications"] {
                for m in v[key].as_array().expect("census array") {
                    let m = m.as_str().expect("census method is a string");
                    assert!(
                        is_census_grammar_method(m),
                        "pinned method {m:?} is outside the loggable grammar"
                    );
                    assert_eq!(method(m), m);
                }
            }
        }
    }

    /// The thread-id grammar against **every committed fixture**, not a subset: the measured
    /// claim is that every thread id the wire has ever shown is a lowercase 36-byte UUID, and
    /// a claim about "every captured frame" is only worth its words if the sweep is total.
    ///
    /// Each fixture is listed with whether it is expected to CARRY thread ids, so a future
    /// capture cannot be added and then silently contribute nothing (which would leave the
    /// grammar unmeasured while the file count grew). `composite_ids.json` is the one
    /// deliberate zero: it is not a captured frame at all but a Rust-generated composite-id
    /// vector, and its `thread_id` field is a synthetic vector label (`th_A`), not a wire
    /// thread id — see `protocol::composite_id`.
    #[test]
    fn every_captured_thread_id_conforms() {
        /// `(name, contents, carries_thread_ids)` for every file in `fixtures/codex/`.
        const FIXTURES: &[(&str, &str, bool)] = &[
            (
                "lifecycle.jsonl",
                include_str!("../../../fixtures/codex/lifecycle.jsonl"),
                true,
            ),
            (
                "command-execution.jsonl",
                include_str!("../../../fixtures/codex/command-execution.jsonl"),
                true,
            ),
            (
                "file-change.jsonl",
                include_str!("../../../fixtures/codex/file-change.jsonl"),
                true,
            ),
            (
                "interrupt.jsonl",
                include_str!("../../../fixtures/codex/interrupt.jsonl"),
                true,
            ),
            (
                "first-turn.jsonl",
                include_str!("../../../fixtures/codex/first-turn.jsonl"),
                true,
            ),
            (
                "resume-populated-answer.json",
                include_str!("../../../fixtures/codex/resume-populated-answer.json"),
                true,
            ),
            (
                "turn-start-request.json",
                include_str!("../../../fixtures/codex/turn-start-request.json"),
                true,
            ),
            (
                "composite_ids.json",
                include_str!("../../../fixtures/codex/composite_ids.json"),
                false,
            ),
            // **The 4b captures, and the steer one is the reason they are here.** The
            // notes `check_steer_binding` writes echo an `expectedTurnId` through
            // [`thread_id`], which is sound only if a real TURN id conforms to the same
            // grammar a thread id does. Nothing asserted that until this row: every file
            // above carries thread ids, and none of them carries a turn id in a position
            // this sweep reads.
            (
                "steer-0.153.4.jsonl",
                include_str!("../../../fixtures/codex/steer-0.153.4.jsonl"),
                true,
            ),
            (
                "compose-0.153.4.jsonl",
                include_str!("../../../fixtures/codex/compose-0.153.4.jsonl"),
                true,
            ),
        ];
        let mut total = 0usize;
        for (name, bundle, carries) in FIXTURES {
            let mut seen = 0usize;
            // A `.jsonl` capture is one frame per line; a `.json` fixture is one document.
            let docs: Vec<&str> = if name.ends_with(".jsonl") {
                bundle.lines().filter(|l| !l.trim().is_empty()).collect()
            } else {
                vec![*bundle]
            };
            for doc in docs {
                let v: Value = serde_json::from_str(doc).unwrap_or_else(|e| {
                    panic!("captured frame in {name} parses: {e}");
                });
                collect_wire_ids(&v, &mut |id| {
                    seen += 1;
                    assert!(
                        is_wire_thread_id(id),
                        "captured thread id {id:?} in {name} is not a lowercase 36-byte UUID"
                    );
                });
            }
            assert_eq!(
                seen > 0,
                *carries,
                "{name}: expected carries_thread_ids={carries}, found {seen}"
            );
            total += seen;
        }
        assert!(total > 0, "the capture must actually carry thread ids");
    }

    /// **Every wire id position this crate renders through [`thread_id`]**: thread ids and
    /// turn ids alike.
    ///
    /// It used to collect thread positions only, which made the two 4b fixture rows pass on
    /// their `threadId` alone — they were added *because* `check_steer_binding` renders an
    /// `expectedTurnId` through the same grammar, and that was the one position the sweep
    /// did not look at. Changing only a capture's `expectedTurnId` to a newline-bearing
    /// string left this green.
    ///
    /// The places, then:
    ///
    /// * a `threadId`, `turnId` or `expectedTurnId` member (the `turn/start`, `turn/steer`
    ///   and `turn/interrupt` requests and every `thread/*` notification);
    /// * a `thread` or `turn` member holding an object with an `id` (`thread/started`,
    ///   `thread/resume`'s result, `turn/started`, `turn/completed`);
    /// * a **bare `Thread` object nested under any other key or none** — e.g. an element of a
    ///   `thread/list` result array. A `Thread` is recognized structurally, by carrying both
    ///   a string `id` and a string `sessionId`, so a Thread that is not under a `thread` key
    ///   is still swept.
    fn collect_wire_ids(v: &Value, f: &mut impl FnMut(&str)) {
        match v {
            Value::Object(map) => {
                // A bare `Thread` — recognized by shape, not by the key it hangs off.
                if let (Some(id), Some(Value::String(_))) = (
                    map.get("id").and_then(Value::as_str),
                    map.get("sessionId").filter(|s| s.is_string()),
                ) {
                    f(id);
                }
                for (k, val) in map {
                    // The id-bearing keys, thread and turn alike. `turn`/`thread` are the
                    // object forms; the rest name an id directly.
                    if matches!(k.as_str(), "threadId" | "turnId" | "expectedTurnId") {
                        if let Some(s) = val.as_str() {
                            f(s);
                        }
                    }
                    if matches!(k.as_str(), "thread" | "turn") {
                        if let Some(s) = val.get("id").and_then(|i| i.as_str()) {
                            f(s);
                        }
                    }
                    collect_wire_ids(val, f);
                }
            }
            Value::Array(items) => items.iter().for_each(|i| collect_wire_ids(i, f)),
            _ => {}
        }
    }

    /// **The sweep looks at TURN id positions, not only thread ones.**
    ///
    /// Its sibling walks the committed captures and asserts every id conforms; this walks
    /// a corrupted COPY and asserts the walk would have caught it. Without that, "the
    /// fixture proves the turn ids conform" rests on the collector visiting a position it
    /// did not visit — which is what the two 4b rows were added for and did not get.
    ///
    /// **Mutation:** drop `expectedTurnId`/`turnId`/`turn` from `collect_wire_ids` and
    /// this goes red while every committed capture stays green.
    #[test]
    fn the_sweep_would_catch_a_non_conforming_turn_id() {
        let hostile = "01a0-not\na uuid";
        for doc in [
            serde_json::json!({"params": {"threadId": "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86",
                                          "expectedTurnId": hostile}}),
            serde_json::json!({"params": {"threadId": "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86",
                                          "turnId": hostile}}),
            serde_json::json!({"params": {"threadId": "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86",
                                          "turn": {"id": hostile}}}),
        ] {
            let mut seen: Vec<String> = Vec::new();
            collect_wire_ids(&doc, &mut |id| seen.push(id.to_string()));
            assert!(
                seen.iter().any(|id| id == hostile),
                "the collector must visit this position: {doc}"
            );
            assert!(
                seen.iter().any(|id| !is_wire_thread_id(id)),
                "and the sweep's own predicate must then refuse it: {seen:?}"
            );
        }
    }

    #[test]
    fn a_non_conforming_thread_id_is_never_echoed() {
        let hostile = "zzz_injected\nFAKE LOG LINE";
        let rendered = thread_id(hostile);
        assert!(!rendered.contains("zzz_injected"), "{rendered}");
        assert!(!rendered.contains('\n'), "{rendered}");
        assert_eq!(rendered, "<non-conforming id, 26 bytes>");
        // The measured form still reads plainly.
        assert_eq!(
            thread_id("01a0127a-c6f4-70d1-b3a3-0742f8fd0d86"),
            "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86"
        );
        // Near misses: uppercase hex, wrong group widths, a trailing group, right length
        // but not hex.
        for bad in [
            "01A0127A-C6F4-70D1-B3A3-0742F8FD0D86",
            "01a0127a-c6f4-70d1-b3a3-0742f8fd0d8",
            "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86-",
            "01a0127a-c6f4-70d1-b3a3-0742f8fd0dzz",
            "01a0127ac6f470d1b3a30742f8fd0d86",
        ] {
            assert!(!is_wire_thread_id(bad), "{bad}");
        }
    }

    #[test]
    fn a_non_conforming_method_is_never_echoed() {
        let hostile = "app/list\ninjected: forward (request allowlisted)";
        let rendered = method(hostile);
        assert!(!rendered.contains("injected"), "{rendered}");
        assert!(!rendered.contains('\n'), "{rendered}");
        assert!(rendered.starts_with("<non-conforming method"), "{rendered}");
        assert_eq!(
            method("thread/increment_elicitation"),
            "thread/increment_elicitation"
        );
        assert!(method(&"a".repeat(MAX_LOGGED_METHOD_BYTES + 1)).starts_with("<non-conforming"));
    }

    #[test]
    fn a_non_conforming_request_id_is_never_echoed() {
        let hostile = RequestId::Str("id\nforged".into());
        let rendered = request_id(&hostile);
        assert!(!rendered.contains("forged"), "{rendered}");
        assert!(!rendered.contains('\n'), "{rendered}");
        assert_eq!(
            request_id(&RequestId::Str(
                "startup-thread-start-9747f04e-f467-466f-96dd-b6872bd77820".into()
            )),
            "\"startup-thread-start-9747f04e-f467-466f-96dd-b6872bd77820\""
        );
        assert_eq!(request_id(&RequestId::Int(-7)), "-7");
        assert!(
            request_id(&RequestId::Str("x".repeat(MAX_REQUEST_ID_BYTES + 1)))
                .starts_with("<non-conforming request id")
        );
    }

    #[test]
    fn value_shape_never_carries_the_value() {
        assert_eq!(value_shape(None), "absent");
        assert_eq!(value_shape(Some(&json!("/tmp/secret"))), "string(len=11)");
        assert_eq!(value_shape(Some(&json!(["a", "b"]))), "array(len=2)");
        assert_eq!(value_shape(Some(&json!({"k": 1}))), "object(keys=1)");
        assert_eq!(value_shape(Some(&json!(null))), "null");
        assert_eq!(value_shape(Some(&json!(true))), "bool");
        assert_eq!(value_shape(Some(&json!(1.5))), "number");
    }
}
