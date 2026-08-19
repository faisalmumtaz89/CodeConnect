//! Shape-based classification of a whole, reassembled client→server WebSocket
//! message.
//!
//! A4 forces classification by **shape, not method name**: an approval answer is a
//! method-less `{id, result|error}` response that durably widens policy, so it can
//! never be recognized by a method check. The broker first decides *what kind of
//! thing* a frame is (request / notification / response / array / binary /
//! malformed), and only then — for the two method-carrying kinds — consults the
//! allowlist.
//!
//! ## Parser-differential hardening
//!
//! The broker classifies a parsed view but forwards the **original bytes**, so any
//! divergence between our parse and the app-server's parse is a bypass. Two are closed
//! here:
//!
//! * **Duplicate members** — `serde_json` silently keeps the last value for a repeated
//!   key, so `{"approvalPolicy":"untrusted","approvalPolicy":"never"}` could validate
//!   one way and execute another. [`classify_shape`] parses with a visitor that **errors
//!   on any duplicate key at any nesting** ⇒ the message is `Malformed` ⇒ zero bytes.
//! * **Hybrid shapes** — a method-bearing object that also carries a `result`/`error`
//!   response discriminant is not a request; it is `Malformed`.
//!
//! The message is already whole (the transport reassembles fragments and yields one
//! [`WsPayload`] per logical message before any classification runs), so this module is
//! pure and synchronous.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// A whole, reassembled WebSocket data payload handed to the classifier.
///
/// The transport terminates ping/pong/close itself; only data frames reach here.
/// Binary content is never inspected — it has no schema-legal client→server form,
/// so the variant carries nothing.
#[derive(Debug, Clone)]
pub enum WsPayload {
    /// A UTF-8 text frame (the only frame the app-server protocol uses c2s).
    Text(String),
    /// A binary frame. Always refused with zero upstream bytes; the leg is hostile.
    Binary,
}

/// The JSON-RPC id of a request or response, restricted to the schema-legal forms.
///
/// Codex's `RequestId` is `anyOf[string, integer]` — it **excludes null** and
/// floats. A "usable" id is one of these two; anything else cannot be echoed into
/// a schema-legal synthetic error, which is why an array/response refusal forwards
/// zero bytes rather than answering.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Str(String),
    Int(i64),
}

impl RequestId {
    /// Re-materialize the id as a JSON value for a synthetic error response.
    pub fn to_value(&self) -> Value {
        match self {
            RequestId::Str(s) => Value::String(s.clone()),
            RequestId::Int(n) => Value::Number((*n).into()),
        }
    }

    /// Read a schema-legal id (string|int) from a JSON value, or `None` for null /
    /// float / out-of-range. Also used by the response-capability registry to key a
    /// server-request's id from the observed s2c `serverRequest` frame.
    pub fn from_value(v: &Value) -> Option<RequestId> {
        if let Some(s) = v.as_str() {
            Some(RequestId::Str(s.to_string()))
        } else if let Some(n) = v.as_i64() {
            Some(RequestId::Int(n))
        } else if let Some(n) = v.as_u64() {
            // A u64 id above i64::MAX cannot round-trip through i64; refuse to
            // treat it as usable rather than truncate.
            i64::try_from(n).ok().map(RequestId::Int)
        } else {
            None
        }
    }
}

/// The classified shape of a whole client→server message.
///
/// The parsed JSON object (for the request/notification/response kinds) is carried
/// alongside so the allowlist and the fingerprint validator can read `method` /
/// `params` / `id` without reparsing a multi-MB body.
#[derive(Debug, Clone)]
pub enum Shape {
    /// `{method, id, params?}` — the only kind classified against the allowlist by
    /// method. `id` is `Some` when it is schema-legal (string|int), `None` when an
    /// id member is present but unusable (e.g. `null`/float) — such a request is
    /// schema-invalid and never forwarded (see [`crate::refusal`]).
    Request {
        method: String,
        id: Option<RequestId>,
        obj: Value,
    },
    /// `{method, params?}` with **no** id member. Also allowlist-classified by
    /// method; an unknown notification is a bypass, so it is never exempt.
    Notification { method: String, obj: Value },
    /// `{id, result|error}` with **no** method — a method-less capability answer
    /// (approval decision). Recognized by shape; validated against a registered
    /// upstream capability (the fanout registry — a later sub-chunk).
    Response {
        id: RequestId,
        is_error: bool,
        obj: Value,
    },
    /// A top-level JSON array (a JSON-RPC batch). No schema-legal error form exists
    /// (`RequestId` excludes null), so it forwards zero bytes.
    Array,
    /// A binary data frame. No client→server binary form exists.
    Binary,
    /// Unparseable JSON, a duplicate member, a hybrid shape, or a JSON value that is
    /// not a legal JSON-RPC message.
    Malformed(&'static str),
}

/// Classify a whole reassembled payload into its shape.
///
/// Pure and total: every input maps to exactly one [`Shape`]. Parsing rejects
/// duplicate keys (parser-differential hardening) and trailing garbage.
pub fn classify_shape(payload: &WsPayload) -> Shape {
    let text = match payload {
        WsPayload::Binary => return Shape::Binary,
        WsPayload::Text(t) => t,
    };
    let value = match parse_no_dup(text) {
        Ok(v) => v,
        Err(ParseError::Duplicate) => return Shape::Malformed("duplicate JSON member"),
        Err(ParseError::Invalid) => return Shape::Malformed("unparseable JSON"),
    };
    match value {
        Value::Array(_) => Shape::Array,
        Value::Object(_) => classify_object(value),
        _ => Shape::Malformed("JSON scalar is not a JSON-RPC message"),
    }
}

fn classify_object(obj: Value) -> Shape {
    let map = obj.as_object().expect("checked object");
    let has_id = map.contains_key("id");
    let has_result = map.contains_key("result");
    let has_error = map.contains_key("error");
    let method = map
        .get("method")
        .and_then(|m| m.as_str())
        .map(str::to_string);

    match method {
        // A method member that is present but not a string is malformed.
        None if map.contains_key("method") => Shape::Malformed("non-string method"),
        Some(method) => {
            // Hybrid: a method-bearing object that also carries a response
            // discriminant is not a request (parser-differential hardening).
            if has_result || has_error {
                return Shape::Malformed("method-bearing object also carries result/error");
            }
            if !has_id {
                Shape::Notification { method, obj }
            } else {
                // id member present: usable only if string|int, else the request
                // is schema-invalid.
                let id = RequestId::from_value(&map["id"]);
                Shape::Request { method, id, obj }
            }
        }
        None => {
            // Method-less. A legal response carries an id and exactly one of
            // result|error; anything else is malformed.
            if !has_id {
                return Shape::Malformed("method-less frame without id");
            }
            let id = match RequestId::from_value(&map["id"]) {
                Some(id) => id,
                None => return Shape::Malformed("response id is not string|int"),
            };
            match (has_result, has_error) {
                (true, false) => Shape::Response {
                    id,
                    is_error: false,
                    obj,
                },
                (false, true) => Shape::Response {
                    id,
                    is_error: true,
                    obj,
                },
                _ => Shape::Malformed("method-less frame is neither result nor error"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Duplicate-key-rejecting JSON parser
// ---------------------------------------------------------------------------

enum ParseError {
    Duplicate,
    Invalid,
}

/// Parse `s` into a [`Value`], erroring if any object contains a duplicate key at any
/// nesting, or if there is trailing content.
fn parse_no_dup(s: &str) -> Result<Value, ParseError> {
    let mut de = serde_json::Deserializer::from_str(s);
    let parsed = NoDup::deserialize(&mut de);
    match parsed {
        Ok(v) => match de.end() {
            Ok(()) => Ok(v.0),
            Err(_) => Err(ParseError::Invalid),
        },
        Err(e) => {
            if e.to_string().starts_with(DUP_MARKER) {
                Err(ParseError::Duplicate)
            } else {
                Err(ParseError::Invalid)
            }
        }
    }
}

/// Parse an observed **server→client** frame with the same duplicate-member discipline
/// the c2s classifier uses, for the capability/thread observers. Returns `None` — "do not
/// trust this frame" — on a duplicate member (at any nesting), unparseable JSON, or
/// trailing garbage, so an observer that consumes it fails closed (skips registration).
/// The c2s path keeps [`classify_shape`]'s richer `Malformed` reasons; observers only need
/// the trust/don't-trust bit.
pub(crate) fn parse_no_dup_value(s: &str) -> Option<Value> {
    parse_no_dup(s).ok()
}

const DUP_MARKER: &str = "codex-broker/duplicate-key";

struct NoDup(Value);

impl<'de> Deserialize<'de> for NoDup {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(NoDupVisitor).map(NoDup)
    }
}

struct NoDupVisitor;

impl<'de> Visitor<'de> for NoDupVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value with no duplicate object keys")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| de::Error::custom("non-finite float"))
    }
    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_string()))
    }
    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut out = Vec::new();
        while let Some(NoDup(v)) = seq.next_element()? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut obj = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let NoDup(val) = map.next_value()?;
            if obj.contains_key(&key) {
                // Prefixed so parse_no_dup can distinguish a dup from other errors.
                return Err(de::Error::custom(format!("{DUP_MARKER}: {key}")));
            }
            obj.insert(key, val);
        }
        Ok(Value::Object(obj))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> WsPayload {
        WsPayload::Text(s.to_string())
    }

    #[test]
    fn request_with_string_id() {
        let s = classify_shape(&text(r#"{"method":"thread/start","id":"abc","params":{}}"#));
        match s {
            Shape::Request { method, id, .. } => {
                assert_eq!(method, "thread/start");
                assert_eq!(id, Some(RequestId::Str("abc".into())));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn request_with_int_id() {
        let s = classify_shape(&text(r#"{"method":"turn/start","id":7,"params":{}}"#));
        match s {
            Shape::Request { id, .. } => assert_eq!(id, Some(RequestId::Int(7))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn notification_has_no_id() {
        let s = classify_shape(&text(r#"{"method":"initialized","params":{}}"#));
        assert!(matches!(s, Shape::Notification { .. }));
    }

    #[test]
    fn method_less_response_result() {
        let s = classify_shape(&text(
            r#"{"id":0,"result":{"decision":{"acceptWithExecpolicyAmendment":{}}}}"#,
        ));
        match s {
            Shape::Response { id, is_error, .. } => {
                assert_eq!(id, RequestId::Int(0));
                assert!(!is_error);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn method_less_response_error() {
        let s = classify_shape(&text(r#"{"id":"q","error":{"code":-1,"message":"x"}}"#));
        assert!(matches!(s, Shape::Response { is_error: true, .. }));
    }

    #[test]
    fn array_is_its_own_shape() {
        assert!(matches!(classify_shape(&text("[]")), Shape::Array));
        assert!(matches!(
            classify_shape(&text(r#"[{"method":"a"}]"#)),
            Shape::Array
        ));
    }

    #[test]
    fn binary_is_binary() {
        assert!(matches!(classify_shape(&WsPayload::Binary), Shape::Binary));
    }

    #[test]
    fn malformed_variants() {
        assert!(matches!(classify_shape(&text("{")), Shape::Malformed(_)));
        assert!(matches!(classify_shape(&text("42")), Shape::Malformed(_)));
        assert!(matches!(
            classify_shape(&text(r#"{"method":123}"#)),
            Shape::Malformed(_)
        ));
        // id present but null -> unusable request id, not a notification.
        match classify_shape(&text(r#"{"method":"x","id":null}"#)) {
            Shape::Request { id: None, .. } => {}
            other => panic!("{other:?}"),
        }
        // method-less with id but no result/error.
        assert!(matches!(
            classify_shape(&text(r#"{"id":1}"#)),
            Shape::Malformed(_)
        ));
        // response id null.
        assert!(matches!(
            classify_shape(&text(r#"{"id":null,"result":{}}"#)),
            Shape::Malformed(_)
        ));
    }

    #[test]
    fn duplicate_key_is_malformed_at_any_nesting() {
        // Top-level dup of a typed ownership field.
        assert!(matches!(
            classify_shape(&text(
                r#"{"method":"thread/start","id":1,"approvalPolicy":"untrusted","approvalPolicy":"never"}"#
            )),
            Shape::Malformed("duplicate JSON member")
        ));
        // Nested dup inside config.
        assert!(matches!(
            classify_shape(&text(
                r#"{"method":"thread/start","id":1,"params":{"config":{"approval_policy":"untrusted","approval_policy":"never"}}}"#
            )),
            Shape::Malformed("duplicate JSON member")
        ));
        // Dup of method itself.
        assert!(matches!(
            classify_shape(&text(
                r#"{"method":"app/list","method":"command/exec","id":1}"#
            )),
            Shape::Malformed("duplicate JSON member")
        ));
    }

    #[test]
    fn response_with_duplicate_top_level_members_is_malformed() {
        // Finding 5: a method-less response with a duplicate id/result/error member is
        // rejected by the NoDup parser (no Shape::Response, so refusal.rs never authorizes
        // it). Confirms the c2s Response path already fails closed on duplicate members.
        for s in [
            r#"{"id":0,"id":1,"result":{"x":1}}"#,
            r#"{"id":0,"result":{"a":1},"result":{"a":2}}"#,
            r#"{"id":0,"error":{"code":-1},"error":{"code":-2}}"#,
        ] {
            assert!(
                matches!(
                    classify_shape(&text(s)),
                    Shape::Malformed("duplicate JSON member")
                ),
                "{s}",
            );
        }
    }

    #[test]
    fn method_plus_result_hybrid_is_malformed() {
        assert!(matches!(
            classify_shape(&text(r#"{"method":"app/list","id":7,"result":{"x":1}}"#)),
            Shape::Malformed(_)
        ));
        assert!(matches!(
            classify_shape(&text(r#"{"method":"app/list","id":7,"error":{"code":-1}}"#)),
            Shape::Malformed(_)
        ));
    }

    #[test]
    fn trailing_garbage_is_malformed() {
        assert!(matches!(
            classify_shape(&text(r#"{"method":"app/list","id":1}{}"#)),
            Shape::Malformed(_)
        ));
    }
}
