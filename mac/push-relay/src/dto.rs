//! The only document the relay will read, and the reason it cannot leak.
//!
//! A privacy promise enforced by review is a promise until somebody adds a
//! field. This one is enforced by the parser: there are four places a string can
//! appear in a request, three of them are closed sets, and the fourth is a hex
//! device token. There is no `title`, no `body`, no `aps`, no project, no
//! session, no path — and unknown fields are refused rather than ignored, so
//! none of those can be smuggled past a relay that would otherwise drop them
//! silently and let a caller believe they arrived.
//!
//! ```text
//! {"schema":1,"token":"<lowercase hex>","environment":"production",
//!  "notification":{"type":"doorbell","kind":"approval","blocked_count":1}}
//! {"schema":1,"token":"<lowercase hex>","environment":"production",
//!  "notification":{"type":"test"}}
//! ```

use push_core::ApnsEnvironment;
use serde::Deserialize;

use crate::payload::PushKind;
use crate::secret::normalize_device_token;

/// The only schema this relay understands.
///
/// Refused rather than ignored when it is anything else: a daemon sending
/// schema 2 believes something about the relay that is not true, and a
/// notification delivered under that belief is worse than one that was refused
/// with a reason.
pub const SCHEMA: u32 = 1;

/// The largest fleet a single notification will describe.
///
/// The number is read by a human off a lock screen and drives the app badge; a
/// phone paired to a thousand simultaneously blocked agents is a bug or an
/// attempt to make the body long, not a state anyone is in. Three digits keeps
/// the composed body short enough that the payload bound is never the thing
/// that decides, and anything above it is a refusal rather than a clamp — a
/// clamp would silently make a wrong count look plausible.
pub const MAX_BLOCKED_COUNT: u32 = 999;

/// The largest request body the relay will read.
///
/// The whole document is a schema number, a hex token, an environment word and
/// a small object. A kilobyte is several times the largest legitimate one and
/// small enough that reading a hostile body costs nothing.
pub const MAX_BODY_BYTES: usize = 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    schema: u32,
    token: String,
    environment: WireEnvironment,
    notification: Notification,
}

/// The environment as a closed set, refused rather than guessed.
///
/// **The measured hole this closes.** Read as a `String` and mapped through a
/// lenient parse, this field accepted the better part of a kilobyte of
/// arbitrary UTF-8 — a project name, a path, a fragment of a diff — and the
/// relay answered `200`. §4 permits no arbitrary string but the token and the
/// credential, and §2's whole claim is that the relay *cannot be told* such
/// content, which a field that swallows anything and shrugs makes untrue.
///
/// **Not in tension with the advisory rule.** §4 says a delivery is never
/// refused because the request's environment disagrees with the binding's —
/// that is about a Mac holding a stale but perfectly valid word. A value that
/// is neither of these two is not stale; it is not an environment, and it is a
/// schema violation like any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WireEnvironment {
    Sandbox,
    Production,
}

impl From<WireEnvironment> for ApnsEnvironment {
    fn from(value: WireEnvironment) -> Self {
        match value {
            WireEnvironment::Sandbox => ApnsEnvironment::Sandbox,
            WireEnvironment::Production => ApnsEnvironment::Production,
        }
    }
}

/// What the notification is, as a closed tagged union.
///
/// `Test` is an **empty struct variant and not a unit variant**, and the
/// difference is load-bearing: serde accepts trailing fields alongside a unit
/// variant in an internally tagged enum, so `{"type":"test","aps":{…}}` would
/// parse and the forbidden-field guarantee would be a comment rather than a
/// rule. Written this way, the same document is refused with "unknown field
/// `aps`".
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Notification {
    Doorbell { kind: PushKind, blocked_count: u32 },
    Test {},
}

/// A request that has been read and normalised, with nothing left to validate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRequest {
    /// Lowercase hex, bounded, and the key everything downstream is measured
    /// against.
    pub token: String,
    /// **Advisory only.** §4 of the plan is explicit: the binding is the single
    /// authority for a token's APNs environment, and this value is what the
    /// caller last believed. A delivery is never refused for disagreeing with
    /// the binding — that refusal is exactly what would stop a second Mac, which
    /// has not yet learned a correction, from delivering at all.
    pub advisory_environment: ApnsEnvironment,
    pub notification: Notification,
}

/// Why a request was not read. Each variant is a different answer to the
/// caller, which is why they are not one string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    /// Refused before parsing, on length alone.
    TooLarge(usize),
    /// Not this schema, or not this shape.
    Malformed(String),
    /// A schema number from a caller that expects a different relay.
    Schema(u32),
    /// The token is not a token.
    Token(String),
    /// A count outside the bound.
    BlockedCount(u32),
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Invalid::TooLarge(len) => write!(
                f,
                "the request body is {len} bytes; the limit is {MAX_BODY_BYTES}"
            ),
            Invalid::Malformed(why) => write!(f, "the request does not parse: {why}"),
            Invalid::Schema(found) => {
                write!(
                    f,
                    "the request declares schema {found}; this relay reads {SCHEMA}"
                )
            }
            Invalid::Token(why) => write!(f, "{why}"),
            Invalid::BlockedCount(found) => write!(
                f,
                "blocked_count is {found}; the limit is {MAX_BLOCKED_COUNT}"
            ),
        }
    }
}

/// Read one request, or say precisely which rule it broke.
pub fn parse(body: &[u8]) -> Result<PushRequest, Invalid> {
    if body.len() > MAX_BODY_BYTES {
        return Err(Invalid::TooLarge(body.len()));
    }
    let wire: Wire = serde_json::from_slice(body).map_err(|e| Invalid::Malformed(e.to_string()))?;
    if wire.schema != SCHEMA {
        return Err(Invalid::Schema(wire.schema));
    }
    let token = normalize_device_token(&wire.token).map_err(|e| Invalid::Token(e.to_string()))?;
    if let Notification::Doorbell { blocked_count, .. } = wire.notification {
        if blocked_count > MAX_BLOCKED_COUNT {
            return Err(Invalid::BlockedCount(blocked_count));
        }
    }
    Ok(PushRequest {
        token,
        advisory_environment: wire.environment.into(),
        notification: wire.notification,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";

    fn body(notification: &str) -> String {
        format!(
            r#"{{"schema":1,"token":"{TOKEN}","environment":"production","notification":{notification}}}"#
        )
    }

    #[test]
    fn the_two_documented_requests_parse() {
        let doorbell =
            parse(body(r#"{"type":"doorbell","kind":"approval","blocked_count":1}"#).as_bytes())
                .expect("the ordinary request");
        assert_eq!(doorbell.token, TOKEN);
        assert_eq!(doorbell.advisory_environment, ApnsEnvironment::Production);
        assert_eq!(
            doorbell.notification,
            Notification::Doorbell {
                kind: PushKind::Approval,
                blocked_count: 1
            }
        );

        let test = parse(body(r#"{"type":"test"}"#).as_bytes()).expect("the test request");
        assert_eq!(test.notification, Notification::Test {});
    }

    /// **The measured trap.** A unit variant here would accept this document,
    /// and every promise about what the relay cannot be told would be a promise
    /// about a parser that ignores fields rather than one that refuses them.
    #[test]
    fn a_prebuilt_aps_object_cannot_ride_alongside_a_test() {
        let err = parse(
            body(r#"{"type":"test","aps":{"alert":{"title":"anything","body":"anything"}}}"#)
                .as_bytes(),
        )
        .expect_err("an aps object must be refused, not ignored");
        assert!(
            matches!(err, Invalid::Malformed(ref why) if why.contains("aps")),
            "{err}"
        );
    }

    #[test]
    fn every_forbidden_field_is_refused_wherever_it_is_put() {
        let outer = [
            r#"{"schema":1,"token":"TOK","environment":"production","notification":{"type":"test"},"title":"x"}"#,
            r#"{"schema":1,"token":"TOK","environment":"production","notification":{"type":"test"},"body":"x"}"#,
            r#"{"schema":1,"token":"TOK","environment":"production","notification":{"type":"test"},"aps":{}}"#,
            r#"{"schema":1,"token":"TOK","environment":"production","notification":{"type":"test"},"project":"x"}"#,
            r#"{"schema":1,"token":"TOK","environment":"production","notification":{"type":"test"},"session_uid":"x"}"#,
            r#"{"schema":1,"token":"TOK","environment":"production","notification":{"type":"test"},"device_id":"x"}"#,
            r#"{"schema":1,"token":"TOK","environment":"production","notification":{"type":"test"},"path":"/x"}"#,
        ];
        for document in outer {
            let document = document.replace("TOK", TOKEN);
            assert!(
                parse(document.as_bytes()).is_err(),
                "accepted a top-level extra field: {document}"
            );
        }

        let inner = [
            r#"{"type":"doorbell","kind":"approval","blocked_count":1,"title":"x"}"#,
            r#"{"type":"doorbell","kind":"approval","blocked_count":1,"body":"x"}"#,
            r#"{"type":"doorbell","kind":"approval","blocked_count":1,"project_label":"x"}"#,
            r#"{"type":"doorbell","kind":"approval","blocked_count":1,"session_uid":"x"}"#,
            r#"{"type":"test","title":"x"}"#,
            r#"{"type":"test","body":"x"}"#,
        ];
        for notification in inner {
            assert!(
                parse(body(notification).as_bytes()).is_err(),
                "accepted an extra field inside the notification: {notification}"
            );
        }
    }

    #[test]
    fn an_unknown_type_or_kind_is_not_a_notification() {
        for notification in [
            r#"{"type":"custom","kind":"approval","blocked_count":1}"#,
            r#"{"type":"doorbell","kind":"escalation","blocked_count":1}"#,
            r#"{"kind":"approval","blocked_count":1}"#,
        ] {
            assert!(
                parse(body(notification).as_bytes()).is_err(),
                "accepted {notification}"
            );
        }
    }

    #[test]
    fn a_missing_field_is_not_defaulted() {
        for document in [
            format!(
                r#"{{"token":"{TOKEN}","environment":"production","notification":{{"type":"test"}}}}"#
            ),
            r#"{"schema":1,"environment":"production","notification":{"type":"test"}}"#.to_string(),
            format!(r#"{{"schema":1,"token":"{TOKEN}","notification":{{"type":"test"}}}}"#),
            format!(r#"{{"schema":1,"token":"{TOKEN}","environment":"production"}}"#),
            body(r#"{"type":"doorbell","kind":"approval"}"#),
            body(r#"{"type":"doorbell","blocked_count":1}"#),
        ] {
            assert!(parse(document.as_bytes()).is_err(), "accepted {document}");
        }
    }

    #[test]
    fn only_schema_one_is_read() {
        let wrong = body(r#"{"type":"test"}"#).replace(r#""schema":1"#, r#""schema":2"#);
        assert_eq!(parse(wrong.as_bytes()), Err(Invalid::Schema(2)));
        assert!(parse(wrong.as_bytes())
            .unwrap_err()
            .to_string()
            .contains("schema 2"));
    }

    #[test]
    fn the_token_passes_the_same_normalisation_as_everywhere_else() {
        let upper = body(r#"{"type":"test"}"#).replace(TOKEN, &TOKEN.to_ascii_uppercase());
        assert_eq!(parse(upper.as_bytes()).unwrap().token, TOKEN);

        for bad in ["", "nothex", "aabbccddeeff001122334455667788zz"] {
            let document = body(r#"{"type":"test"}"#).replace(TOKEN, bad);
            assert!(
                matches!(parse(document.as_bytes()), Err(Invalid::Token(_))),
                "accepted the token {bad:?}"
            );
        }
    }

    /// The count is bounded, and the boundary itself is on the accepting side.
    #[test]
    fn a_count_above_the_bound_is_refused_rather_than_clamped() {
        let at_limit = body(&format!(
            r#"{{"type":"doorbell","kind":"idle","blocked_count":{MAX_BLOCKED_COUNT}}}"#
        ));
        assert!(parse(at_limit.as_bytes()).is_ok());

        let over = body(&format!(
            r#"{{"type":"doorbell","kind":"idle","blocked_count":{}}}"#,
            MAX_BLOCKED_COUNT + 1
        ));
        assert_eq!(
            parse(over.as_bytes()),
            Err(Invalid::BlockedCount(MAX_BLOCKED_COUNT + 1))
        );

        let negative = body(r#"{"type":"doorbell","kind":"idle","blocked_count":-1}"#);
        assert!(parse(negative.as_bytes()).is_err());
    }

    #[test]
    fn an_oversized_body_is_refused_before_it_is_parsed() {
        let padded = format!(
            r#"{{"schema":1,"token":"{TOKEN}","environment":"{}","notification":{{"type":"test"}}}}"#,
            "p".repeat(MAX_BODY_BYTES)
        );
        assert!(padded.len() > MAX_BODY_BYTES);
        assert_eq!(
            parse(padded.as_bytes()),
            Err(Invalid::TooLarge(padded.len()))
        );
    }

    /// The environment is what the caller last believed. **Both words parse,
    /// including the one that disagrees with a binding** — §4's advisory rule
    /// lives on the push path, and refusing a stale `"sandbox"` here would stop
    /// a second Mac that has not learned a correction from delivering at all.
    #[test]
    fn the_environment_is_read_but_never_a_reason_to_refuse() {
        for (value, expected) in [
            ("production", ApnsEnvironment::Production),
            ("sandbox", ApnsEnvironment::Sandbox),
        ] {
            let document = body(r#"{"type":"test"}"#).replace("production", value);
            assert_eq!(
                parse(document.as_bytes()).unwrap().advisory_environment,
                expected
            );
        }
    }

    /// **The measured hole.** A `String` here accepted the better part of a
    /// kilobyte of arbitrary UTF-8 — a project name, a path, a diff fragment —
    /// and answered `200`, which made "the relay cannot receive such content" a
    /// claim about a field nobody read rather than about the parser.
    #[test]
    fn the_environment_cannot_be_arbitrary_text() {
        for value in [
            "something-else",
            "Production",
            "PRODUCTION",
            "sandbox ",
            "",
            "codeconnect-gateway/src/main.rs",
            "the user asked me to refactor the payment module",
        ] {
            let document = body(r#"{"type":"test"}"#).replace("production", value);
            assert!(
                matches!(parse(document.as_bytes()), Err(Invalid::Malformed(_))),
                "accepted the environment {value:?}"
            );
        }

        // And the length that used to fit inside the body bound: refused by the
        // schema now, rather than stored in a field the relay simply ignores.
        let smuggled = "x".repeat(MAX_BODY_BYTES / 2);
        let document = body(r#"{"type":"test"}"#).replace("production", &smuggled);
        assert!(document.len() <= MAX_BODY_BYTES);
        assert!(matches!(
            parse(document.as_bytes()),
            Err(Invalid::Malformed(_))
        ));
    }

    /// A non-string environment is refused on the same rule, so the closed set
    /// cannot be stepped around with a different JSON type.
    #[test]
    fn the_environment_is_a_word_and_not_a_number_or_an_object() {
        for raw in [
            r#"{"schema":1,"token":"TOK","environment":1,"notification":{"type":"test"}}"#,
            r#"{"schema":1,"token":"TOK","environment":null,"notification":{"type":"test"}}"#,
            r#"{"schema":1,"token":"TOK","environment":{"name":"production"},"notification":{"type":"test"}}"#,
            r#"{"schema":1,"token":"TOK","environment":["production"],"notification":{"type":"test"}}"#,
        ] {
            let document = raw.replace("TOK", TOKEN);
            assert!(parse(document.as_bytes()).is_err(), "accepted {document}");
        }
    }
}
