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
pub fn parse_no_dup_value(s: &str) -> Option<Value> {
    parse_no_dup(s).ok()
}

const DUP_MARKER: &str = "codex-broker/duplicate-key";

// ---------------------------------------------------------------------------
// Cheap top-level header scan (round-3 P1)
// ---------------------------------------------------------------------------

/// The top-level JSON-RPC header of an observed **server→client** frame — everything the
/// per-connection outstanding-request-id ledger ([`crate::session`]) needs in order to
/// decide whether the frame is a response and, if so, whose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameHeader {
    /// The schema-legal top-level `id`, if the frame carries a usable one.
    pub id: Option<RequestId>,
    /// Whether a top-level `method` member is present. A method-bearing frame is a
    /// notification or a server→client request — never the answer to a forwarded request.
    pub has_method: bool,
    /// What the top-level `result`/`error` members PROVE about the frame (round-4 P1).
    /// An id may only be released from the outstanding ledger when this is a response.
    pub response: ResponseKind,
}

/// What a frame's top-level `result`/`error` members prove about it — decided by the header
/// scan, without the body ever becoming a [`Value`] (round-4 P1).
///
/// ## One definition of "a valid response", shared by two rules
///
/// This is the SAME rule as `session::classify_creation_response`'s first two arms,
/// deliberately: the ledger's DRAIN rule ("may this frame release an outstanding id?") and
/// the creation-response CLASSIFICATION rule ("did this frame prove success or failure?")
/// must never disagree about what a response is, or an id could be released by a frame the
/// creation state machine would not accept — which is exactly the hole round-4 P1 closed.
///
/// * [`Self::Result`] ⇔ `classify_creation_response`'s INSTALL precondition (`error` absent,
///   `result` present).
/// * [`Self::Error`] ⇔ its REOPEN arm (`result` absent, and `error` structurally a JSON-RPC
///   error object — an INTEGER `code` AND a STRING `message`, per
///   `session::is_jsonrpc_error_object`).
/// * [`Self::NotAResponse`] ⇔ its indeterminate CLOSED arm.
///
/// `session::response_kind_agrees_with_the_creation_state_machine` pins that equivalence, so
/// the two cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseKind {
    /// Exactly one top-level `result`, and no `error`: a JSON-RPC success response.
    Result,
    /// Exactly one top-level `error` — structurally a JSON-RPC error object — and no
    /// `result`: a JSON-RPC failure response.
    Error,
    /// NOT a provable response: neither member, BOTH members, an `error` that is not a
    /// well-formed error object (`{}`, a partial one, a non-integer `code`, a non-string
    /// `message`, `null`, a scalar, an array), or an `error` whose own `code`/`message` is
    /// duplicated and therefore ambiguous. Releases nothing, correlates to nothing.
    NotAResponse,
}

impl ResponseKind {
    /// Is this frame a response — i.e. may it release an outstanding request id?
    pub(crate) fn is_response(self) -> bool {
        !matches!(self, ResponseKind::NotAResponse)
    }
}

/// Read a frame's top-level JSON-RPC header **without materializing its body**.
///
/// Round-3 P1 makes the session observer look at every s2c frame (a response has to release
/// its outstanding id), which would otherwise mean deserializing a multi-MB `plugin/list`
/// answer into a `Value` tree on the hot path. This scan instead skips every member that is
/// not `id`, `method` or `error` with `serde::de::IgnoredAny`: it walks the bytes once and
/// allocates nothing for the body.
///
/// ## `result` is never deserialized; only `error` is descended into (round-4 P1)
///
/// P1 also has to prove a frame IS a response before its id may be released, which needs
/// presence/exclusivity of `result` vs `error` plus the error's two field TYPES — and
/// nothing more. So `result` keeps being skipped with `IgnoredAny` (its presence is a bool;
/// its 5.76 MB `plugin/list` body is never touched), and only `error` — a handful of small
/// members on the real wire — is descended into, by [`ErrorProbe`], which itself reduces
/// `code`/`message` to a TYPE TAG rather than a value and `IgnoredAny`s every other member
/// including the spec's optional `data`. "Multi-MB frames are not reparsed" is therefore
/// unchanged; see `tests::the_header_scan_skips_a_multi_megabyte_body` and
/// `tests::the_header_scan_distrusts_non_objects_dups_and_garbage` (whose nested-duplicate
/// case is the direct witness that the body never becomes a `Value`).
///
/// Returns `None` — "do not trust this frame" — when it is not a JSON object, is
/// unparseable, has trailing content, or repeats ANY **top-level** member (a repeated
/// top-level member makes the header itself ambiguous between our parse and the
/// app-server's, so no id may be released on it). Nested duplicates are deliberately not
/// inspected: they cannot change the header, and any frame this admits as a *creation
/// response* candidate is re-parsed in full by [`parse_no_dup_value`] before it may install
/// a binding — so the strict whole-frame duplicate discipline is unchanged for binding.
pub(crate) fn scan_frame_header(s: &str) -> Option<FrameHeader> {
    let mut de = serde_json::Deserializer::from_str(s);
    let header = Header::deserialize(&mut de).ok()?;
    de.end().ok()?;
    Some(header.0)
}

struct Header(FrameHeader);

impl<'de> Deserialize<'de> for Header {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(HeaderVisitor).map(Header)
    }
}

struct HeaderVisitor;

impl<'de> Visitor<'de> for HeaderVisitor {
    type Value = FrameHeader;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON-RPC frame object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<FrameHeader, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut id = None;
        let mut has_method = false;
        let mut has_result = false;
        let mut error = ErrorShape::Absent;
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom(format!("{DUP_MARKER}: {key}")));
            }
            match key.as_str() {
                "id" => {
                    let v: Value = map.next_value()?;
                    id = RequestId::from_value(&v);
                }
                "method" => {
                    let _: de::IgnoredAny = map.next_value()?;
                    has_method = true;
                }
                // PRESENCE only — the body is skipped, never materialized. This is the
                // 5.76 MB `plugin/list` answer's path.
                "result" => {
                    let _: de::IgnoredAny = map.next_value()?;
                    has_result = true;
                }
                // The one member the scan descends into, because its two field TYPES are
                // part of "is this a response?". `ErrorProbe` allocates no value for it.
                "error" => {
                    let ErrorProbe(shape) = map.next_value()?;
                    error = shape;
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        // EXCLUSIVE result-or-well-formed-error. `id`-only, partial, both, or neither is
        // NotAResponse — it may not release an id, correlate, reopen, or install.
        let response = match (has_result, error) {
            (true, ErrorShape::Absent) => ResponseKind::Result,
            (false, ErrorShape::WellFormed) => ResponseKind::Error,
            _ => ResponseKind::NotAResponse,
        };
        Ok(FrameHeader {
            id,
            has_method,
            response,
        })
    }
}

/// What the top-level `error` member turned out to be, as seen by [`ErrorProbe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorShape {
    /// No `error` member at all.
    Absent,
    /// An object with an INTEGER `code` AND a STRING `message`, neither of them duplicated.
    WellFormed,
    /// Present, but not a JSON-RPC error object — so it proves nothing.
    Malformed,
}

/// A header-scan probe over the top-level `error` member.
///
/// Decides [`ErrorShape`] **without materializing the value**: every member other than
/// `code`/`message` (the spec's optional `data` included) is skipped with `IgnoredAny`, and
/// `code`/`message` are reduced to a [`JsonKind`] tag rather than a stored value. A
/// non-object `error` — `null`, a scalar, an array — is drained and reported `Malformed`.
///
/// A REPEATED `code` or `message` is `Malformed`: those two members decide the verdict, so a
/// duplicate makes the frame ambiguous between our read and the app-server's, and an
/// ambiguous frame releases nothing. Duplicates of OTHER members are not inspected — they
/// cannot change the verdict — which keeps this a scan; the strict whole-frame duplicate
/// discipline still applies to anything that goes on to BIND (see [`parse_no_dup_value`]).
struct ErrorProbe(ErrorShape);

impl<'de> Deserialize<'de> for ErrorProbe {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer
            .deserialize_any(ErrorProbeVisitor)
            .map(ErrorProbe)
    }
}

struct ErrorProbeVisitor;

impl<'de> Visitor<'de> for ErrorProbeVisitor {
    type Value = ErrorShape;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON-RPC error member")
    }

    fn visit_bool<E>(self, _: bool) -> Result<ErrorShape, E> {
        Ok(ErrorShape::Malformed)
    }
    fn visit_i64<E>(self, _: i64) -> Result<ErrorShape, E> {
        Ok(ErrorShape::Malformed)
    }
    fn visit_u64<E>(self, _: u64) -> Result<ErrorShape, E> {
        Ok(ErrorShape::Malformed)
    }
    fn visit_f64<E>(self, _: f64) -> Result<ErrorShape, E> {
        Ok(ErrorShape::Malformed)
    }
    fn visit_str<E>(self, _: &str) -> Result<ErrorShape, E> {
        Ok(ErrorShape::Malformed)
    }
    fn visit_none<E>(self) -> Result<ErrorShape, E> {
        Ok(ErrorShape::Malformed)
    }
    fn visit_unit<E>(self) -> Result<ErrorShape, E> {
        Ok(ErrorShape::Malformed)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<ErrorShape, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<de::IgnoredAny>()?.is_some() {}
        Ok(ErrorShape::Malformed)
    }

    fn visit_map<A>(self, mut map: A) -> Result<ErrorShape, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut code: Option<JsonKind> = None;
        let mut message: Option<JsonKind> = None;
        let mut ambiguous = false;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "code" => {
                    let JsonKindProbe(kind) = map.next_value()?;
                    ambiguous |= code.is_some();
                    code = Some(kind);
                }
                "message" => {
                    let JsonKindProbe(kind) = map.next_value()?;
                    ambiguous |= message.is_some();
                    message = Some(kind);
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        let well_formed =
            !ambiguous && code == Some(JsonKind::Integer) && message == Some(JsonKind::Str);
        Ok(if well_formed {
            ErrorShape::WellFormed
        } else {
            ErrorShape::Malformed
        })
    }
}

/// The only thing the error probe needs to know about `code` and `message`: their JSON type.
///
/// `Integer` matches `session::is_jsonrpc_error_object`'s `is_i64() || is_u64()`
/// exactly — a float or a numeric string is `Other`, not an integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonKind {
    Integer,
    Str,
    Other,
}

/// Reads a value's [`JsonKind`] and throws the value itself away, so even a hostile
/// megabyte-long `message` costs no `Value` allocation.
struct JsonKindProbe(JsonKind);

impl<'de> Deserialize<'de> for JsonKindProbe {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer
            .deserialize_any(JsonKindVisitor)
            .map(JsonKindProbe)
    }
}

struct JsonKindVisitor;

impl<'de> Visitor<'de> for JsonKindVisitor {
    type Value = JsonKind;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<JsonKind, E> {
        Ok(JsonKind::Other)
    }
    fn visit_i64<E>(self, _: i64) -> Result<JsonKind, E> {
        Ok(JsonKind::Integer)
    }
    fn visit_u64<E>(self, _: u64) -> Result<JsonKind, E> {
        Ok(JsonKind::Integer)
    }
    fn visit_f64<E>(self, _: f64) -> Result<JsonKind, E> {
        Ok(JsonKind::Other)
    }
    fn visit_str<E>(self, _: &str) -> Result<JsonKind, E> {
        Ok(JsonKind::Str)
    }
    fn visit_none<E>(self) -> Result<JsonKind, E> {
        Ok(JsonKind::Other)
    }
    fn visit_unit<E>(self) -> Result<JsonKind, E> {
        Ok(JsonKind::Other)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<JsonKind, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<de::IgnoredAny>()?.is_some() {}
        Ok(JsonKind::Other)
    }

    fn visit_map<A>(self, mut map: A) -> Result<JsonKind, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map
            .next_entry::<de::IgnoredAny, de::IgnoredAny>()?
            .is_some()
        {}
        Ok(JsonKind::Other)
    }
}

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

    // -----------------------------------------------------------------
    // The cheap top-level header scan (round-3 P1).
    // -----------------------------------------------------------------

    #[test]
    fn the_header_scan_reads_id_and_method_without_the_body() {
        let h = scan_frame_header(r#"{"id":7,"result":{"data":[1,2,3]}}"#).unwrap();
        assert_eq!(h.id, Some(RequestId::Int(7)));
        assert!(!h.has_method);

        let h = scan_frame_header(r#"{"id":"abc","error":{"code":-1,"message":"x"}}"#).unwrap();
        assert_eq!(h.id, Some(RequestId::Str("abc".into())));
        assert!(!h.has_method);

        // Method-bearing frames are recognized as such whichever order the members appear in.
        let h = scan_frame_header(r#"{"method":"thread/started","params":{"a":1}}"#).unwrap();
        assert_eq!(h.id, None);
        assert!(h.has_method);
        let h =
            scan_frame_header(r#"{"id":0,"params":{},"method":"item/x/requestApproval"}"#).unwrap();
        assert_eq!(h.id, Some(RequestId::Int(0)));
        assert!(h.has_method);

        // A schema-illegal id is not a usable id.
        assert_eq!(
            scan_frame_header(r#"{"id":null,"result":{}}"#).unwrap().id,
            None
        );
        assert_eq!(
            scan_frame_header(r#"{"id":1.5,"result":{}}"#).unwrap().id,
            None
        );
    }

    #[test]
    fn the_header_scan_distrusts_non_objects_dups_and_garbage() {
        for bad in [
            "[]",
            "42",
            "\"s\"",
            "{",
            r#"{"id":1,"result":{}}{}"#,
            // A repeated TOP-LEVEL member makes the header itself ambiguous.
            r#"{"id":1,"id":2,"result":{}}"#,
            r#"{"method":"a","method":"b"}"#,
        ] {
            assert!(scan_frame_header(bad).is_none(), "{bad}");
        }
        // A NESTED duplicate cannot change the header, so the scan admits it — and the
        // strict whole-frame parser is what refuses it before anything may be bound.
        let h = scan_frame_header(r#"{"id":1,"result":{"a":1,"a":2}}"#).unwrap();
        assert_eq!(h.id, Some(RequestId::Int(1)));
        assert!(parse_no_dup_value(r#"{"id":1,"result":{"a":1,"a":2}}"#).is_none());
    }

    #[test]
    fn the_header_scan_skips_a_multi_megabyte_body() {
        // The property the scan exists for: a 6 MB `plugin/list`-class answer yields its
        // header without the body ever becoming a `Value`. Round-4 P1 added result/error
        // VALIDATION to the same scan, and this test is what pins that the validation stayed
        // a header scan: `result` is still only tested for PRESENCE, with `IgnoredAny`.
        let big = "A".repeat(6 * 1024 * 1024);
        let frame = format!(r#"{{"id":42,"result":{{"pad":"{big}"}}}}"#);
        let h = scan_frame_header(&frame).unwrap();
        assert_eq!(h.id, Some(RequestId::Int(42)));
        assert!(!h.has_method);
        assert_eq!(h.response, ResponseKind::Result);

        // The direct WITNESS that the body never became a `Value`: the same multi-MB body
        // carrying a nested duplicate member is still scanned (a `Value` build would have
        // been rejected by `parse_no_dup_value`, which is exactly what the second assert
        // shows).
        let dup = format!(r#"{{"id":42,"result":{{"pad":"{big}","pad":"x"}}}}"#);
        assert_eq!(
            scan_frame_header(&dup).unwrap().response,
            ResponseKind::Result
        );
        assert!(parse_no_dup_value(&dup).is_none());

        // The `error` side: only `code`/`message` are looked at, so even a multi-MB `data`
        // — the spec's optional member — is skipped, not materialized.
        let fat_error = format!(
            r#"{{"id":42,"error":{{"code":-1,"message":"boom","data":{{"pad":"{big}"}}}}}}"#
        );
        assert_eq!(
            scan_frame_header(&fat_error).unwrap().response,
            ResponseKind::Error
        );
    }

    // ROUND-4 P1 — DRAIN VALIDATION. The scan proves EXCLUSIVE result-or-well-formed-error
    // before an id may be released; `id`-only, partial, both, or neither is `NotAResponse`.
    #[test]
    fn the_header_scan_proves_exclusive_result_or_well_formed_error() {
        use ResponseKind::{Error, NotAResponse, Result as Ok_};
        for (frame, want) in [
            // Valid responses.
            (r#"{"id":1,"result":{"a":1}}"#, Ok_),
            (r#"{"id":1,"result":null}"#, Ok_),
            (r#"{"id":1,"error":{"code":-1,"message":"m"}}"#, Error),
            (r#"{"id":1,"error":{"code":0,"message":""}}"#, Error),
            // `data` is part of the spec and cannot make the frame less of an error.
            (
                r#"{"id":1,"error":{"code":-32601,"message":"m","data":{"any":true}}}"#,
                Error,
            ),
            // Member ORDER is irrelevant.
            (r#"{"error":{"message":"m","code":-1},"id":1}"#, Error),
            // The exploit frame: a bare method-less id.
            (r#"{"id":1}"#, NotAResponse),
            (r#"{"id":1,"jsonrpc":"2.0"}"#, NotAResponse),
            // BOTH members.
            (
                r#"{"id":1,"result":{"a":1},"error":{"code":-1,"message":"m"}}"#,
                NotAResponse,
            ),
            (r#"{"id":1,"result":null,"error":null}"#, NotAResponse),
            // A present but unprovable `error`.
            (r#"{"id":1,"error":{}}"#, NotAResponse),
            (r#"{"id":1,"error":null}"#, NotAResponse),
            (r#"{"id":1,"error":"boom"}"#, NotAResponse),
            (r#"{"id":1,"error":7}"#, NotAResponse),
            (
                r#"{"id":1,"error":[{"code":-1,"message":"m"}]}"#,
                NotAResponse,
            ),
            (r#"{"id":1,"error":{"code":-1}}"#, NotAResponse),
            (r#"{"id":1,"error":{"message":"m"}}"#, NotAResponse),
            // Non-INTEGER code.
            (
                r#"{"id":1,"error":{"code":"-1","message":"m"}}"#,
                NotAResponse,
            ),
            (
                r#"{"id":1,"error":{"code":-1.5,"message":"m"}}"#,
                NotAResponse,
            ),
            (
                r#"{"id":1,"error":{"code":null,"message":"m"}}"#,
                NotAResponse,
            ),
            (
                r#"{"id":1,"error":{"code":{"n":-1},"message":"m"}}"#,
                NotAResponse,
            ),
            // Non-STRING message.
            (r#"{"id":1,"error":{"code":-1,"message":5}}"#, NotAResponse),
            (
                r#"{"id":1,"error":{"code":-1,"message":null}}"#,
                NotAResponse,
            ),
            (
                r#"{"id":1,"error":{"code":-1,"message":["m"]}}"#,
                NotAResponse,
            ),
            // AMBIGUOUS: `code`/`message` decide the verdict, so a duplicate of either makes
            // the frame unreadable — our first-wins read and serde's last-wins read disagree.
            (
                r#"{"id":1,"error":{"code":"x","code":-1,"message":"m"}}"#,
                NotAResponse,
            ),
            (
                r#"{"id":1,"error":{"code":-1,"message":"m","message":7}}"#,
                NotAResponse,
            ),
            // The last-VALID ordering is the load-bearing one: serde's last-wins read
            // lands on a well-typed value, so only duplicate DETECTION can refuse it.
            (
                r#"{"id":1,"error":{"code":-1,"message":7,"message":"m"}}"#,
                NotAResponse,
            ),
            // A duplicate of some OTHER member cannot change the verdict, so it is not
            // inspected — the scan stays a scan. (Anything that goes on to BIND is still
            // re-parsed by the strict whole-frame parser.)
            (
                r#"{"id":1,"error":{"code":-1,"message":"m","data":1,"data":2}}"#,
                Error,
            ),
            // A method-bearing frame is never a response, whatever else it carries.
            (r#"{"id":1,"method":"x","params":{}}"#, NotAResponse),
        ] {
            assert_eq!(
                scan_frame_header(frame).expect("scans").response,
                want,
                "{frame}"
            );
        }
        // A repeated TOP-LEVEL `result`/`error` makes the whole header untrustworthy, so the
        // scan yields nothing at all rather than a verdict.
        for ambiguous in [
            r#"{"id":1,"result":{"a":1},"result":{"a":2}}"#,
            r#"{"id":1,"error":{"code":-1,"message":"m"},"error":{"code":-2,"message":"n"}}"#,
        ] {
            assert!(scan_frame_header(ambiguous).is_none(), "{ambiguous}");
        }
    }

    #[test]
    fn trailing_garbage_is_malformed() {
        assert!(matches!(
            classify_shape(&text(r#"{"method":"app/list","id":1}{}"#)),
            Shape::Malformed(_)
        ));
    }
}
