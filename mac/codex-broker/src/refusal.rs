//! The phone leg's refusal matrix and its pure security-core entrypoint [`classify`].
//!
//! Only the `ccd` leg — CodeConnect's daemon, speaking for a phone — is classified. The
//! keyboard leg is a passthrough ([`crate::relay`]): the person at the machine is as
//! trusted as they are in native codex, and the broker only watches their frames
//! ([`crate::session`]).
//!
//! Refusal is **never uniformly "an error"** — the disposition depends on the message
//! *shape*:
//!
//! | shape | refused disposition |
//! |-------|---------------------|
//! | request with a usable id | synthetic JSON-RPC error on that id, **zero upstream bytes** |
//! | request without a usable id | zero upstream bytes, no error |
//! | notification | zero upstream bytes, no error (no reply channel) |
//! | method-less response (no live capability) | zero upstream bytes, no error (losing-fanout race) |
//! | JSON array | zero upstream bytes (no schema-legal error form; `RequestId` excludes null) |
//! | binary frame | zero upstream bytes |
//! | malformed | zero upstream bytes |
//!
//! Connection-outcome policy by class: a losing-fanout response or a policy-refused
//! request/notification is a normal event — **log, keep the leg open**; a shape that
//! implies a hostile or broken client (malformed, binary, array) → **close that leg**.
//!
//! ## `classify` is pure *except* for three deliberate one-way claims
//!
//! [`classify`] reads its whole world through [`Env`], and three of those reads are
//! **claims** that consume state, because the decision and the claim must be one atomic
//! step or a pipeline race opens between them:
//!
//! * `capabilities.authorize` consumes the one-use response capability at the instant a
//!   method-less approval answer is admitted (fanout: first answer wins).
//! * `threads.try_admit_request` records every about-to-be-forwarded request's id as
//!   outstanding on its connection. A request whose id is already outstanding is
//!   protocol-hostile, so it is dropped with zero upstream bytes, the leg is kept open, and
//!   the event is counted. A REFUSED request never registers an id: the ledger runs last,
//!   and only on a `Forward`.
//! * `threads.try_admit_idle_turn` head-checks a phone's `turn/start` and marks the thread
//!   busy in one step.
//!
//! ## Refusal details are audit-log-safe
//!
//! Every `note` on a [`RelayAction`] is written to a durable `broker.log` that operators and
//! live gates read. The classifier's inputs are attacker-chosen bytes, so a note may carry
//! only fixed vocabulary, counts and shapes — never a raw method, params key, thread id,
//! request id or ownership value. [`crate::redact`] owns that rendering and states the THREE
//! grammar-gated exceptions (a census-grammar method name, a measured UUID thread id, and a
//! plain-identifier request id).
//!
//! ## What a phone's actuation must name
//!
//! A phone's `turn/start`, `turn/steer`, `turn/interrupt` and approval answer each name the
//! **head** — the thread the keyboard is on ([`crate::session::ThreadBinding::bound_thread`]).
//! A thread the keyboard has left stays readable and is never actuable. A phone's turn names
//! no policy of its own: every ownership key is null, so it runs under whatever the keyboard
//! has set on the thread.

use serde_json::json;

use crate::allowlist::{disposition, Disposition, JsonRpcKind, RefuseReason};
use crate::message::{classify_shape, RequestId, Shape, WsPayload};
use crate::redact;
use crate::response_capability::ResponseCapabilityRegistry;
use crate::session::{
    ConnId, IdAdmission, ThreadBinding, TurnAdmission, MAX_ACTIVE_TURNS, TURN_METHOD,
};

/// The runtime policy environment the classifier reads: the one-use response-capability
/// registry, the session's thread state, and the identity of the connection this message
/// arrived on.
pub struct Env<'a> {
    pub capabilities: &'a dyn ResponseCapabilityRegistry,
    pub threads: &'a dyn ThreadBinding,
    /// The relay-minted instance id of the connection this message arrived on. The phone
    /// leg's id ledger is keyed by it.
    pub conn: ConnId,
}

/// JSON-RPC error code for a policy refusal. `ccd` restates this value as
/// `BROKER_POLICY_REFUSED` and matches on it to tell the broker's refusal from the
/// app-server's, which never emits it, so it must not change.
pub const E_POLICY_REFUSED: i64 = -32001;

/// What the relay must do with a classified client→server message. The relay owns the
/// original bytes; `Forward` means "send those exact bytes upstream".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayAction {
    /// Forward the original reassembled bytes upstream unchanged.
    Forward { note: &'static str },
    /// Send this synthetic JSON-RPC error frame back to the client; zero upstream bytes.
    /// The leg stays open.
    SyntheticError { frame: String, note: String },
    /// Zero upstream bytes, no reply; log and keep the leg open (a policy refusal with
    /// no usable id, a refused notification, or a losing-fanout response).
    DropLogKeepOpen { note: String },
    /// Zero upstream bytes, no reply; the shape implies a hostile/broken client — close
    /// this leg.
    DropCloseLeg { note: String },
}

/// The pure security core: classify one whole, reassembled phone-leg payload and decide
/// its fate. No I/O, no async — fully testable against captured frames.
pub fn classify(env: &Env, payload: &WsPayload) -> RelayAction {
    decide(env, classify_shape(payload))
}

/// Decide the fate of an already-classified [`Shape`]. The relay parses each whole
/// message exactly once (multi-MB bodies are not reparsed) and calls this; unit tests
/// call [`classify`], which parses for them.
pub fn decide(env: &Env, shape: Shape) -> RelayAction {
    match shape {
        Shape::Binary => DropCloseLeg_("binary frame has no client→server form"),
        Shape::Array => {
            // No schema-legal error form (RequestId excludes null) — the zero-byte
            // non-forward is the only mechanism; the batch shape is hostile/broken.
            DropCloseLeg_("JSON array (batch) has no schema-legal error form")
        }
        Shape::Malformed(reason) => RelayAction::DropCloseLeg {
            note: format!("malformed frame: {reason}"),
        },
        Shape::Notification { method, .. } => classify_notification(&method),
        Shape::Request { method, id, obj } => {
            classify_request(env, &method, id, obj.get("params").unwrap_or(&json!({})))
        }
        Shape::Response { id, .. } => classify_response(env, &id),
    }
}

fn classify_notification(method: &str) -> RelayAction {
    match disposition(JsonRpcKind::Notification, method) {
        Disposition::Forward => RelayAction::Forward {
            note: "notification allowlisted",
        },
        // A refused notification has no reply channel; zero bytes, keep open. The method
        // is client-chosen, so it is rendered through the audit-log grammar.
        other => RelayAction::DropLogKeepOpen {
            note: format!(
                "notification {} refused ({other:?})",
                redact::method(method)
            ),
        },
    }
}

/// Classify one request, then apply the per-connection id ledger to whatever it decided.
///
/// The ledger runs LAST and only on a `Forward`: a REFUSED request forwards zero bytes and
/// therefore occupies no id.
fn classify_request(
    env: &Env,
    method: &str,
    id: Option<RequestId>,
    params: &serde_json::Value,
) -> RelayAction {
    let action = classify_request_disposition(env, method, id.clone(), params);
    if !matches!(action, RelayAction::Forward { .. }) {
        return action;
    }
    // A request without a usable id is refused by the first check in
    // `classify_request_disposition`, so a `Forward` always carries one.
    let id = id.expect("a forwarded request always carries a usable id");
    // **`turn/start` admits itself**, atomically with its head-check inside
    // `try_admit_idle_turn`; passing it through the generic ledger as well would register
    // the same id twice and be refused as `ReusedInFlight`.
    if method == TURN_METHOD {
        return action;
    }
    match env.threads.try_admit_request(env.conn, &id, method) {
        IdAdmission::Admitted => action,
        verdict => ledger_refusal(method, verdict),
    }
}

/// Render one refusing id-ledger verdict. Shared by the generic request path and by
/// `turn/start`'s atomic admission, so the two never disagree about what a full ledger or
/// a reused id looks like to a client.
///
/// ZERO upstream bytes, leg kept open, event counted. No synthetic error: the id is
/// precisely the thing that cannot be trusted to echo (it may be the over-long id that was
/// just refused), and a client that reuses an in-flight id cannot correlate an answer to
/// it anyway.
fn ledger_refusal(method: &str, verdict: IdAdmission) -> RelayAction {
    let why = match verdict {
        IdAdmission::Admitted => unreachable!("only a refusing verdict is rendered"),
        IdAdmission::ReusedInFlight => {
            "request id is already outstanding on this connection (reused in flight)"
        }
        IdAdmission::Oversized => "request id exceeds the stored-id byte cap and is never stored",
        IdAdmission::AtCapacity => "this connection's outstanding-request ledger is full",
    };
    RelayAction::DropLogKeepOpen {
        note: format!(
            "{}: {why}; zero bytes, leg kept open, counted for the failure-containment seam",
            redact::method(method)
        ),
    }
}

/// **The phone's `turn/start`.**
///
/// Its params are its projection and nothing else: every key but `threadId` and `input`
/// is null, so the turn names no policy of its own and runs under whatever the thread has
/// now — which only the keyboard can change. The projection is pure, so a refusal leaves
/// nothing behind. Then the head-check, the idle rule, the id ledger and the busy mark are
/// one atomic decision in [`ThreadBinding::try_admit_idle_turn`].
fn classify_turn_start(
    env: &Env,
    method: &str,
    id: Option<RequestId>,
    params: &serde_json::Value,
) -> RelayAction {
    if let Err(detail) = check_phone_turn_projection(params) {
        return refuse_request(
            id,
            E_POLICY_REFUSED,
            "turn refused: it is not the shape this session accepts from a phone",
            format!("{}: {detail}", redact::method(method)),
        );
    }
    // A turn without a usable id cannot be admitted (nothing could correlate its answer),
    // and the first check in `classify_request_disposition` already refused that case.
    let Some(turn_id) = id.clone() else {
        return refuse_request(
            id,
            E_POLICY_REFUSED,
            "turn refused: it does not name this session's bound thread",
            "turn/start without a usable request id".to_string(),
        );
    };
    let Some(named) = params.get("threadId").and_then(|t| t.as_str()) else {
        return refuse_request(
            Some(turn_id),
            E_POLICY_REFUSED,
            "turn refused: it does not name this session's bound thread",
            "turn/start without a string threadId".to_string(),
        );
    };
    let refused = |message: &str, detail: String| {
        refuse_request(Some(turn_id.clone()), E_POLICY_REFUSED, message, detail)
    };
    match env.threads.try_admit_idle_turn(
        env.conn,
        &turn_id,
        named,
        params.get("cwd"),
        params.get("runtimeWorkspaceRoots"),
    ) {
        TurnAdmission::Admitted => RelayAction::Forward {
            note: "turn/start: the phone's null-ownership turn, head-checked on an idle thread",
        },
        TurnAdmission::NotTheHead { detail } => refused(
            "turn refused: it does not name this session's bound thread",
            detail,
        ),
        TurnAdmission::WrongWorkspace { detail } => refused(
            "turn refused: it does not run in the workspace bound at its thread's creation",
            detail,
        ),
        TurnAdmission::Moving => refused(
            "turn refused: a thread switch is in progress on this session",
            format!(
                "{}: the keyboard is moving to another thread or has left this one, so no \
                 thread may be named until its move settles",
                redact::method(method)
            ),
        ),
        TurnAdmission::ThreadAlreadyBusy => refused(
            "turn refused: this session is already running a turn",
            format!(
                "{}: a turn is running on this thread, and a start sent now would be \
                 accepted by the app-server as an implicit steer into it with no \
                 expectedTurnId to guard staleness; turn/steer is the method for this case",
                redact::method(method)
            ),
        ),
        TurnAdmission::TooManyActiveTurns => refused(
            "turn refused: too many turns are already in flight",
            format!(
                "{}: this session already holds {MAX_ACTIVE_TURNS} admitted turns whose \
                 terminals have not been observed",
                redact::method(method)
            ),
        ),
        TurnAdmission::Ledger(verdict) => ledger_refusal(method, verdict),
    }
}

fn classify_request_disposition(
    env: &Env,
    method: &str,
    id: Option<RequestId>,
    params: &serde_json::Value,
) -> RelayAction {
    // A request whose id is present but not schema-legal (null/float) is invalid; never
    // forward it, and it cannot be answered — zero bytes.
    if id.is_none() {
        return RelayAction::DropLogKeepOpen {
            note: format!(
                "{}: request has no usable id (schema-invalid); zero bytes",
                redact::method(method)
            ),
        };
    }

    match disposition(JsonRpcKind::Request, method) {
        Disposition::Forward => RelayAction::Forward {
            note: "request allowlisted",
        },
        // See [`Disposition::NullOwnedIdleTurn`].
        Disposition::NullOwnedIdleTurn => classify_turn_start(env, method, id, params),
        // The composer on the phone: the head-check, the running-turn binding and the
        // text-only input rule. See [`Disposition::SteerRunningTurn`].
        Disposition::SteerRunningTurn => match check_steer_binding(env, params) {
            Err(SteerRefusal::Shape(detail)) => refuse_request(
                id,
                E_POLICY_REFUSED,
                "steer refused: it is not the shape this session accepts",
                format!("{}: {detail}", redact::method(method)),
            ),
            Err(SteerRefusal::Binding(detail)) => refuse_request(
                id,
                E_POLICY_REFUSED,
                // **This sentence names no turn, and that is the point.** The app-server's
                // own refusal for the same frame is `-32600 "expected active turn id X but
                // found Y"`, which hands the caller the id of the turn the session is really
                // running. This one reaches a phone, so it carries only what the phone
                // already sent.
                "steer refused: it does not name the turn this session is running",
                format!("{}: {detail}", redact::method(method)),
            ),
            Ok(()) => RelayAction::Forward {
                note: "turn/steer: names this session's running turn, text only",
            },
        },
        Disposition::Refuse(reason) => {
            let (code, msg) = refuse_message(reason);
            // `NotAllowlisted` reaches here for an UNKNOWN, client-chosen method, so the
            // name is rendered through the audit-log grammar.
            refuse_request(
                id,
                code,
                msg,
                format!("{}: refused ({reason:?})", redact::method(method)),
            )
        }
        // The thread-scoped READS. See [`Disposition::ReadSessionThread`] for the measured
        // cross-session leak this closes.
        Disposition::ReadSessionThread => {
            if let Err(detail) = check_read_binding(env, method, params) {
                return refuse_request(
                    id,
                    E_POLICY_REFUSED,
                    "read refused: it does not name a thread of this session",
                    format!("{}: {detail}", redact::method(method)),
                );
            }
            RelayAction::Forward {
                note: "thread-scoped read on a session thread",
            }
        }
        // The phone's attach. See [`Disposition::ResumeSessionThread`].
        Disposition::ResumeSessionThread => {
            if let Err(detail) = check_thread_id_only_shape(params) {
                return refuse_request(
                    id,
                    E_POLICY_REFUSED,
                    "resume refused: it is not the shape this session accepts from a phone",
                    format!("{}: {detail}", redact::method(method)),
                );
            }
            if let Err(detail) = check_resume_binding(env, params) {
                return refuse_request(
                    id,
                    E_POLICY_REFUSED,
                    "resume refused: target thread is not bound to this session",
                    detail,
                );
            }
            RelayAction::Forward {
                note: "thread/resume: names a thread of this session and nothing else",
            }
        }
        // The session's one STOP control. See [`Disposition::InterruptActiveTurn`].
        Disposition::InterruptActiveTurn => {
            if let Err(detail) = check_interrupt_binding(env, params) {
                return refuse_request(
                    id,
                    E_POLICY_REFUSED,
                    "interrupt refused: it does not name a running turn of this session",
                    format!("{}: {detail}", redact::method(method)),
                );
            }
            RelayAction::Forward {
                note: "turn/interrupt: names this session's running turn",
            }
        }
    }
}

/// The exact top-level parameter key set a phone's `thread/resume` is written with.
///
/// Named rather than spelled inline so the refusal below can count against it without
/// repeating it, exactly as [`INTERRUPT_PARAMS`] is.
const THREAD_ID_ONLY_PARAMS: [&str; 1] = ["threadId"];

/// **Params pinned to exactly `{"threadId": "<string>"}`**: a phone's `thread/resume`.
///
/// Refuse-by-default applies to params, not just to methods — the same rule
/// [`PHONE_TURN_PARAMS`] enforces for a turn. ccd's resume (`ccd::codex_link`) is written
/// with this one key and nothing else, and any other key would be a setting the phone
/// chose.
fn check_thread_id_only_shape(params: &serde_json::Value) -> Result<(), String> {
    let Some(obj) = params.as_object() else {
        return Err(format!(
            "params is a {}; the measured value is an object",
            redact::value_shape(Some(params))
        ));
    };
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    if keys != THREAD_ID_ONLY_PARAMS {
        // **The SHAPE, never the names** — the same rule, for the same reason, as
        // [`check_interrupt_binding`]'s. The keys are client-chosen bytes and this
        // detail lands in the durable `broker.log`, so a key called after a credential,
        // or one four kilobytes long, must not be able to write itself there. Counts
        // carry everything an operator needs to tell the cases apart and carry nothing
        // a client chose.
        let supplied = keys.len();
        let missing = THREAD_ID_ONLY_PARAMS
            .iter()
            .filter(|name| !keys.contains(*name))
            .count();
        let unexpected = keys
            .iter()
            .filter(|key| !THREAD_ID_ONLY_PARAMS.contains(*key))
            .count();
        return Err(format!(
            "params carries {supplied} keys: {unexpected} unexpected, and {missing} of \
             the {} the measured frame carries absent. Key names are client-chosen and \
             are not logged",
            THREAD_ID_ONLY_PARAMS.len()
        ));
    }
    if !obj
        .get("threadId")
        .is_some_and(serde_json::Value::is_string)
    {
        return Err("params.threadId is not a string".to_string());
    }
    Ok(())
}

/// A `thread/resume` may only target a thread bound to this session.
///
/// The requested id is client-chosen, so it is rendered through the measured thread-id
/// grammar before it reaches the audit log.
fn check_resume_binding(env: &Env, params: &serde_json::Value) -> Result<(), String> {
    match params.get("threadId").and_then(|t| t.as_str()) {
        None => Err("resume without a string threadId".to_string()),
        Some(id) if env.threads.is_session_thread(id) => Ok(()),
        Some(id) => Err(format!(
            "thread {} was not observed as a session thread",
            redact::thread_id(id)
        )),
    }
}

/// The exact top-level parameter key set each thread-scoped read was MEASURED carrying.
///
/// Refuse-by-default applies to params, not only to methods — the same rule
/// [`check_thread_id_only_shape`] enforces, and for the same reason: these three moved from
/// "unscoped forward" to "forwarded when bound", so their params became a surface. The
/// sets are the union of every captured frame: `thread/read` from the 0.153 tee runs
/// (five frames, all `{threadId}`), `thread/turns/list` from the 0.153 tee
/// (`fixtures/codex/session-0.153.jsonl`'s sibling captures), `thread/items/list` from
/// `fixtures/codex/thread-switch.jsonl`.
///
/// The 0.153 app-server was measured to IGNORE unknown params on all three, so an extra
/// key is inert today. That is a fact about today's server on a binary that ships weekly,
/// and it is not the reason the set exists: an unmeasured key on a method that names a
/// thread is an unmeasured way to name one.
///
/// Two of the three sets are the whole schema property set. `thread/read`'s is not: its
/// schema also carries an optional `includeTurns`, which no capture has ever exercised in
/// either direction, so it is refused — the same call the `thread/start` boundary makes
/// about the three properties its captures never carried. A schema property is not a
/// measurement.
fn read_captured_params(method: &str) -> &'static [(&'static str, ReadParam)] {
    use ReadParam::*;
    match method {
        "thread/read" => &[("threadId", ThreadId)],
        "thread/turns/list" => &[
            ("cursor", Cursor),
            ("itemsView", Enum(&["notLoaded", "summary", "full"])),
            ("limit", NullableUint32),
            ("sortDirection", Enum(&["asc", "desc"])),
            ("threadId", ThreadId),
        ],
        "thread/items/list" => &[
            ("cursor", Cursor),
            ("limit", NullableUint32),
            ("sortDirection", Enum(&["asc", "desc"])),
            ("threadId", ThreadId),
            ("turnId", NullableString),
        ],
        // Unreachable while `Disposition::ReadSessionThread` names exactly those three —
        // `the_read_binding_covers_every_read_session_thread_method` proves it — and an
        // empty set refuses everything if a fourth is ever added without a capture.
        _ => &[],
    }
}

/// The measured value rule for one captured read parameter.
///
/// A key set alone was not enough. `params` is client-chosen, the positional-array
/// surprise showed that "the shape is obviously an object" is not something to assume, and
/// every one of these fields reaches the app-server unexamined once the frame forwards.
/// So each captured key carries what its value may BE, not only that it may be present.
#[derive(Debug, Clone, Copy)]
enum ReadParam {
    /// The thread selector: required, a non-empty string, and a thread of this session.
    ThreadId,
    /// Absent, null, or the measured cursor structure — see [`check_cursor_thread`].
    Cursor,
    /// Absent, null, or a string. `turnId` is this: MEASURED not to be a thread selector
    /// (a foreign turn id on a session thread returns an empty page; alone it is
    /// `missing field threadId`), so its content is bound by the server to the thread the
    /// binding already proved, and its TYPE is what is left to pin.
    NullableString,
    /// Absent, null, or an integer in the schema's `uint32` range.
    NullableUint32,
    /// Absent, null, or one of these strings.
    ///
    /// The members are the schema's, not merely the ones a capture happened to show: codex
    /// 0.153's `SortDirection` and `TurnItemsView`. A member a later codex adds is refused
    /// until it is listed here, so the value space stays closed. (Captures show
    /// `sortDirection: "desc"` and `itemsView: "full"|"notLoaded"`; the schema's spare
    /// members are `"asc"` and `"summary"`.)
    Enum(&'static [&'static str]),
}

/// The exact members a measured cursor carries, and nothing else.
///
/// Verbatim from the wire on both releases:
/// `{"requestedThreadId":"…","rolloutOrdinal":29,"includeAnchor":false,"scope":{"kind":"turns"}}`
/// (`scope.kind` was also seen as `"itemsByCreatedAtOrdinal"`). A cursor is the second
/// place a thread is NAMED, so an unmeasured member in one is an unmeasured way to name
/// one — checking only `requestedThreadId` and passing the rest through was accepting the
/// structure wholesale.
const CURSOR_MEMBERS: [&str; 4] = [
    "includeAnchor",
    "requestedThreadId",
    "rolloutOrdinal",
    "scope",
];

/// Check one captured parameter's value against its measured rule.
fn check_read_param(
    env: &Env,
    key: &str,
    rule: ReadParam,
    value: &serde_json::Value,
) -> Result<(), String> {
    let shape = || redact::value_shape(Some(value));
    match rule {
        ReadParam::ThreadId => match value.as_str() {
            Some(id) if env.threads.is_session_thread(id) => Ok(()),
            Some(id) => Err(format!(
                "thread {} was not observed as a session thread",
                redact::thread_id(id)
            )),
            None => Err(format!(
                "params.{key} is a {}; it must be a string",
                shape()
            )),
        },
        ReadParam::Cursor => match value {
            serde_json::Value::Null => Ok(()),
            serde_json::Value::String(cursor) => check_cursor_thread(env, cursor),
            _ => Err(format!(
                "params.{key} is a {}; the measured cursor is a string",
                shape()
            )),
        },
        ReadParam::NullableString => match value {
            serde_json::Value::Null | serde_json::Value::String(_) => Ok(()),
            _ => Err(format!(
                "params.{key} is a {}; the measured value is a string or null",
                shape()
            )),
        },
        ReadParam::NullableUint32 => match value {
            serde_json::Value::Null => Ok(()),
            v => match v.as_u64() {
                Some(n) if n <= u64::from(u32::MAX) => Ok(()),
                _ => Err(format!(
                    "params.{key} is a {}; the measured value is a uint32 or null",
                    shape()
                )),
            },
        },
        ReadParam::Enum(members) => match value {
            serde_json::Value::Null => Ok(()),
            serde_json::Value::String(s) if members.contains(&s.as_str()) => Ok(()),
            _ => Err(format!(
                "params.{key} is a {}; the measured values are {members:?} or null",
                shape()
            )),
        },
    }
}

/// A thread-scoped READ may only target a thread bound to this session, and may carry
/// nothing but the parameters it was measured carrying.
///
/// # What the 0.153 app-server was measured to honour as a thread selector
///
/// | parameter | measured behaviour |
/// |---|---|
/// | `params.threadId` (object) | **the selector.** Required; absent ⇒ `missing field threadId`. |
/// | `params[0]` (positional array) | **also a selector** — the same id, in a form no key-based check can read. Refused here by requiring an object. |
/// | `cursor` | **not** a selector: the server rejects a cursor whose `requestedThreadId` differs from `threadId` with `-32600 invalid cursor`, and a cursor alone is `missing field threadId`. |
/// | `turnId` (`thread/items/list`) | **not** a selector: an intra-thread filter applied after `threadId` picks the rollout. A foreign turn id returns `{"data":[]}`; alone it is `missing field threadId`. Bound by the server, so nothing to bind here. |
/// | unknown keys | ignored (no `deny_unknown_fields`) — inert, and refused anyway by the captured set. |
///
/// The cursor is still checked, and that is deliberate rather than superstitious: it is a
/// second place a thread is NAMED, the server enforcing the match is a fact about today's
/// server, and the check costs one parse. It is parsed **structurally** — the measured
/// encoding is a JSON object inside a JSON string, verbatim off the 0.153 wire:
/// `{"requestedThreadId":"…","rolloutOrdinal":18,"includeAnchor":false,"scope":{…}}`. A
/// cursor that is not that is an encoding nobody has measured on a method that names
/// threads, and it refuses rather than being scanned for a substring.
fn check_read_binding(env: &Env, method: &str, params: &serde_json::Value) -> Result<(), String> {
    // Positional params are not a hypothetical: MEASURED, the 0.153 app-server answers
    // `{"method":"thread/read","params":["<any thread id>"]}` with that thread. Every rule
    // below reads named keys, and an array has none — it would pass by finding no
    // violation rather than by proving none.
    let Some(obj) = params.as_object() else {
        return Err(format!(
            "params is a {}; the measured value is an object, and positional params name a \
             thread in a form this binding cannot read",
            redact::value_shape(Some(params))
        ));
    };
    let captured = read_captured_params(method);
    let unknown = obj
        .keys()
        .filter(|k| !captured.iter().any(|(name, _)| name == k))
        .count();
    if unknown > 0 {
        let names: Vec<&str> = captured.iter().map(|(n, _)| *n).collect();
        return Err(format!(
            "params carries {unknown} key(s) of {} outside the measured set for this read; \
             the captured frames carry exactly {names:?}",
            obj.len()
        ));
    }

    // `threadId` is REQUIRED, whatever else is present: a read that names no thread has
    // nothing to bind, and the server answers `missing field threadId` anyway.
    if !obj.contains_key("threadId") {
        return Err("a thread-scoped read without a threadId".to_string());
    }
    // Every captured key that IS present is checked against its measured value rule. An
    // absent optional key is the measured "I am naming nothing" and passes.
    for (key, rule) in captured {
        if let Some(value) = obj.get(*key) {
            check_read_param(env, key, *rule, value)?;
        }
    }
    Ok(())
}

/// **The complete `turn/start` param key set the phone's own producer writes**, sorted.
///
/// `ccd::codex_link::compose_frame` emits exactly these fourteen. Pinned for
/// [`STEER_PARAMS`]' reason: the keyboard's frame may carry twenty-four names, and
/// `model`, `effort`, `serviceTier`, `personality` and `summary` among them would each
/// set something for the turn. Measured over the real relay when this leg shared the
/// keyboard's key set: all five, and a 4096-byte `clientUserMessageId`, forwarded.
///
/// So this leg gets the frame its daemon authors, not the union the keyboard's names
/// admit. Widening it means widening the producer first.
const PHONE_TURN_PARAMS: [&str; 14] = [
    "additionalContext",
    "approvalPolicy",
    "approvalsReviewer",
    "clientUserMessageId",
    "collaborationMode",
    "cwd",
    "environments",
    "input",
    "multiAgentMode",
    "outputSchema",
    "permissions",
    "responsesapiClientMetadata",
    "sandboxPolicy",
    "threadId",
];

/// **Every [`PHONE_TURN_PARAMS`] key but `threadId` and `input`, each required JSON null.**
///
/// A phone's turn names no policy of its own — not the approval policy, the reviewer, the
/// cwd, the sandbox, the permissions or the collaboration mode — so it runs under the
/// settings the thread has now, which only the keyboard can change. A populated value is
/// refused even when it equals the thread's own: the phone has no business choosing it.
const PHONE_TURN_NULL_PARAMS: [&str; 12] = [
    "additionalContext",
    "approvalPolicy",
    "approvalsReviewer",
    "clientUserMessageId",
    "collaborationMode",
    "cwd",
    "environments",
    "multiAgentMode",
    "outputSchema",
    "permissions",
    "responsesapiClientMetadata",
    "sandboxPolicy",
];

/// **Is this `turn/start` the frame the phone's own daemon writes?**
///
/// The whole of the phone arm's params check, run before anything is claimed, so a frame
/// this leg may not author never takes the busy mark.
///
/// Shapes and counts in the detail, never a key name or a value — the keys here are
/// client-chosen and this lands in a durable `broker.log` ([`crate::redact`]).
fn check_phone_turn_projection(params: &serde_json::Value) -> Result<(), String> {
    let Some(obj) = params.as_object() else {
        return Err(format!(
            "params is a {}; the measured value is an object",
            redact::value_shape(Some(params))
        ));
    };
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    if keys != PHONE_TURN_PARAMS {
        let supplied = keys.len();
        let missing = PHONE_TURN_PARAMS
            .iter()
            .filter(|name| !keys.contains(*name))
            .count();
        let unexpected = keys
            .iter()
            .filter(|key| !PHONE_TURN_PARAMS.contains(*key))
            .count();
        return Err(format!(
            "params carries {supplied} keys: {unexpected} unexpected, and {missing} of \
             the {} this leg's own producer writes absent. Key names are client-chosen \
             and are not logged",
            PHONE_TURN_PARAMS.len()
        ));
    }
    for key in PHONE_TURN_NULL_PARAMS {
        match obj.get(key) {
            Some(serde_json::Value::Null) => {}
            other => {
                return Err(format!(
                    "params.{key}: this leg's producer writes JSON null; a {} is a shape \
                     it does not author",
                    redact::value_shape(other)
                ))
            }
        }
    }
    check_phone_input(params)
}

/// **The ceiling on the text a phone may put in the model's mouth, at the boundary.**
///
/// The daemon caps its own compose at `protocol::ws::MAX_COMPOSE_BYTES` and the WebSocket
/// layer caps a frame at a megabyte, so nothing reachable through the shipping daemon
/// exceeds this. The broker is nonetheless the boundary, and this was the one admitted ccd
/// param with no bound of its own — a 2 MB single text item classified `Forward`, measured
/// over the real relay.
///
/// **This crate cannot import `protocol`** (it has no such dependency, deliberately: the
/// security core depends on serde and tokio and nothing of ours). So the number is written
/// here and `ccd` — which can see both — asserts the two are equal, in
/// `the_brokers_phone_text_bound_is_the_protocols`. A boundary looser than the producer's
/// own limit is a rule the producer keeps and the boundary does not, which is the shape
/// this module exists to refuse.
pub const MAX_PHONE_TEXT_BYTES: usize = 8 * 1024;

/// The complete measured `turn/steer` param key set, sorted — the allowlist
/// [`check_steer_binding`] compares against, and counts a deviation from.
///
/// MEASURED on codex 0.153.4 through the frame tee: typing during a running turn sends
/// exactly these six keys (`fixtures/codex/steer-0.153.4.jsonl`). Three of them are
/// required by the schema and three are optional there, and this pin requires all six —
/// the same discipline [`INTERRUPT_PARAMS`] keeps, and for the same reason: the shape a
/// future release would use to introduce a new channel on this method is a key, and a
/// broker that ignored unknown keys would forward that channel unexamined the day it
/// appears. Widening needs a new capture, not an argument.
const STEER_PARAMS: [&str; 6] = [
    "additionalContext",
    "clientUserMessageId",
    "expectedTurnId",
    "input",
    "responsesapiClientMetadata",
    "threadId",
];

/// The three `turn/steer` params measured as JSON null on every captured steer. Each is a
/// map the caller fills in — `additionalContext` carries `{kind, value}` entries the model
/// reads, `responsesapiClientMetadata` reaches the upstream API — and neither was ever
/// observed carrying anything, so a populated one is a shape this broker cannot prove.
/// `clientUserMessageId` is the third, and it was nearly left out on the grounds that it is
/// a UX correlation id rather than a channel — which `turn/start` says too, on its own
/// "deliberately NOT gated" list: leaving it an unbounded client-chosen string would make it
/// the only free-form byte channel on the method. It was measured null
/// (`fixtures/codex/steer-0.153.4.jsonl`), so pinning it costs nothing that has been seen.
///
/// The phone's `turn/start` holds all three to the same rule ([`PHONE_TURN_NULL_PARAMS`]),
/// which is what stops the two methods becoming two different answers to one question.
const STEER_NULL_PARAMS: [&str; 3] = [
    "additionalContext",
    "clientUserMessageId",
    "responsesapiClientMetadata",
];

/// **Why a steer was refused**, in the two categories a caller can act on differently.
///
/// A frame this session never accepts from anybody, and a frame that is fine but names the
/// wrong thing, are different news: the first says "your client is wrong", the second says
/// "you are late". Collapsing them into one sentence — which an earlier form of this did —
/// tells somebody whose steer arrived a moment after the turn ended that their app is
/// broken.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SteerRefusal {
    /// The params are not the measured shape.
    Shape(String),
    /// The shape is right; the thread, the turn or the input is not.
    Binding(String),
}

/// **Is this steer one the phone may send?**
///
/// The measured params shape; the head; `expectedTurnId` the turn the head is actually
/// running; and text input only. Every refusal detail names shapes and counts, never a
/// client-chosen key or value — this lands in the durable `broker.log` (see
/// [`crate::redact`]).
fn check_steer_binding(env: &Env, params: &serde_json::Value) -> Result<(), SteerRefusal> {
    let Some(obj) = params.as_object() else {
        return Err(SteerRefusal::Shape(format!(
            "params is a {}; the measured value is an object",
            redact::value_shape(Some(params))
        )));
    };
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    if keys != STEER_PARAMS {
        // The SHAPE, never the names — [`check_interrupt_binding`]'s rule, for the same
        // reason: the keys are attacker-chosen bytes and this detail is durable.
        let supplied = keys.len();
        let missing = STEER_PARAMS
            .iter()
            .filter(|name| !keys.contains(*name))
            .count();
        let unexpected = keys
            .iter()
            .filter(|key| !STEER_PARAMS.contains(*key))
            .count();
        return Err(SteerRefusal::Shape(format!(
            "params carries {supplied} keys: {unexpected} unexpected, and {missing} of \
             the {} the measured frame carries absent. Key names are client-chosen and \
             are not logged",
            STEER_PARAMS.len()
        )));
    }
    for key in STEER_NULL_PARAMS {
        match obj.get(key) {
            Some(serde_json::Value::Null) => {}
            other => {
                return Err(SteerRefusal::Shape(format!(
                    "params.{key}: captured boundary — every measured steer sent this key \
                     as JSON null; a {} was never captured and cannot be proven",
                    redact::value_shape(other)
                )))
            }
        }
    }
    let (Some(thread), Some(expected)) = (
        obj.get("threadId").and_then(|v| v.as_str()),
        obj.get("expectedTurnId").and_then(|v| v.as_str()),
    ) else {
        return Err(SteerRefusal::Shape(
            "params.threadId or params.expectedTurnId is not a string".to_string(),
        ));
    };
    // **The head**, for [`check_interrupt_binding`]'s reason: a steer is an actuation, and
    // a thread the keyboard has left is readable but never actuable.
    if env.threads.bound_thread().as_deref() != Some(thread) {
        return Err(SteerRefusal::Binding(format!(
            "thread {} is not this session's one active thread — a thread this session \
             has left is readable, never actuable",
            redact::thread_id(thread)
        )));
    }
    if !env.threads.is_active_turn(thread, expected) {
        return Err(SteerRefusal::Binding(format!(
            "turn {} is not a running turn of thread {} — a phone may only steer the turn \
             this session is currently running, and the app-server's own refusal for this \
             frame would have named the turn that is",
            redact::thread_id(expected),
            redact::thread_id(thread)
        )));
    }
    check_phone_input(params).map_err(SteerRefusal::Binding)
}

/// **Every `input` item a phone sends is TEXT.**
///
/// The `UserInput` union the pinned schema carries has seven arms, and six of them name
/// something outside the turn: `localImage`, `localAudio`, `skill` and `mention` each carry
/// an absolute filesystem PATH the app-server opens itself — outside the model's sandbox,
/// so no `sandboxPolicy` fences it — and `image`/`audio` each carry a URL. A phone that
/// could name those would hold a read-and-exfiltrate primitive over the whole machine,
/// reached through the one method it is allowed to compose with.
///
/// The operator's own keyboard keeps the whole union, and that asymmetry is deliberate
/// rather than an oversight: the person at the machine drags a file into their own composer
/// and can already read it. The phone cannot read a byte of that machine by any other
/// route, and this is not the place to give it one.
///
/// Shape and counts only in the detail, for [`crate::redact`]'s reason — an item's `path`
/// is exactly the kind of client-chosen string that must not reach the audit log.
fn check_phone_input(params: &serde_json::Value) -> Result<(), String> {
    let Some(items) = params.get("input") else {
        return Err("params.input is absent; a compose with no input composes nothing".into());
    };
    let Some(items) = items.as_array() else {
        return Err(format!(
            "params.input is a {}; the measured value is an array",
            redact::value_shape(Some(items))
        ));
    };
    // **Exactly ONE item.** `compose_frame` writes one, always — a compose is one message.
    // The rule used to be "at least one, all of them text", which admitted a frame with
    // two items and any number more; measured over the real relay as `Forward`. An empty
    // array is the same refusal from the other side: the app-server answers it
    // `-32600 "no active turn to steer"`, a sentence about the TURN for a frame whose
    // problem is that it says nothing.
    if items.len() != 1 {
        return Err(format!(
            "params.input carries {} items; this leg's producer writes exactly one, \
             because a compose is one message",
            items.len()
        ));
    }
    if let Err(why) = check_phone_text_item(&items[0]) {
        return Err(format!("params.input[0]: {why}"));
    }
    Ok(())
}

/// **The measured `text` arm of the `UserInput` union, and only it, exactly.**
///
/// `{"type":"text","text":"…","text_elements":[]}` — the shape every captured client sends
/// and the one `compose_frame` writes. Three rules, and each closes something that was
/// measured forwarding over the real relay:
///
/// * **the key set is exact.** The union's other six arms — `localImage`, `localAudio`,
///   `skill`, `mention` (each an absolute filesystem PATH the app-server opens itself,
///   outside the model's sandbox) and `image`/`audio` (a URL) — are refused by the `type`
///   rule; an extra key beside a legitimate `text` is refused by this one, because the
///   threat this module is written against is a future release giving a key a meaning.
/// * **`text_elements` is exactly `[]`,** not merely an array. Every capture is empty and
///   the producer writes empty; the schema's `TextElement` carries a `placeholder` it does
///   not say is inert, and an arbitrary array of them forwarded.
/// * **`text` is bounded** by [`MAX_PHONE_TEXT_BYTES`]. A 2 MB single item forwarded.
///
/// The operator's keyboard keeps the whole union and no bound: the person at the machine
/// drags a file into their own composer and can already read it.
fn check_phone_text_item(item: &serde_json::Value) -> Result<(), String> {
    let Some(obj) = item.as_object() else {
        return Err(format!(
            "a {} is not the measured text item",
            redact::value_shape(Some(item))
        ));
    };
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    if keys != ["text", "text_elements", "type"] {
        return Err(format!(
            "it carries {} keys; the measured text item carries exactly three. The input \
             union also admits local file paths (localImage, localAudio, skill, mention) \
             and remote URLs (image, audio), which the app-server opens itself and no \
             sandbox policy fences; a phone composes with text, and with nothing beside \
             it. Key names are client-chosen and are not logged",
            keys.len()
        ));
    }
    if obj.get("type").and_then(|t| t.as_str()) != Some("text") {
        return Err("its type is not text".to_string());
    }
    if !obj["text_elements"]
        .as_array()
        .is_some_and(|a| a.is_empty())
    {
        return Err(format!(
            "its text_elements is {}; every measured value is the empty array, and this \
             leg's producer writes one",
            redact::value_shape(obj.get("text_elements"))
        ));
    }
    let Some(text) = obj.get("text").and_then(|t| t.as_str()) else {
        return Err(format!(
            "its text is a {}",
            redact::value_shape(obj.get("text"))
        ));
    };
    if text.len() > MAX_PHONE_TEXT_BYTES {
        return Err(format!(
            "its text is {} bytes; the ceiling this leg's own daemon advertises is {}. \
             The value is client-chosen and is not logged",
            text.len(),
            MAX_PHONE_TEXT_BYTES
        ));
    }
    Ok(())
}

/// The complete measured `turn/interrupt` param key set, sorted — the allowlist
/// [`check_interrupt_binding`] compares against, and counts a deviation from.
const INTERRUPT_PARAMS: [&str; 2] = ["threadId", "turnId"];

/// A `turn/interrupt` may only stop THIS session's RUNNING turn.
///
/// The params are pinned to the capture, exactly as `thread/unsubscribe`'s are: MEASURED
/// off a real 0.153 Ctrl-C, the frame is `{"threadId": …, "turnId": …}` and nothing else.
/// This method moved from "deferred, refuses everything" to "forwarded when bound", so its
/// params became a surface for the first time and refuse-by-default applies to them.
///
/// Two bindings, and each closes a different thing:
///
/// * `threadId` must be the session's ONE ACTIVE thread — not merely a thread it has been
///   on. An interrupt naming another session's thread is the same cross-session reach the
///   reads were bound for; an interrupt naming a thread THIS session has retired is a
///   reach into a visit it has left. The reads are deliberately wider (a retired thread
///   stays readable); an actuation is not.
/// * `turnId` must be an **active** turn of it — one the server announced with
///   `turn/started`, or one this broker admitted whose `turn/start` the server answered
///   with that id — whose terminal has not arrived.
///   Without this an interrupt could name any string and be forwarded; with it, the only
///   turn stoppable is the one this session is actually running.
///
/// The interrupt carries no ownership fields, starts nothing, and steers nothing, so the
/// head and the running turn are the whole authorization question.
fn check_interrupt_binding(env: &Env, params: &serde_json::Value) -> Result<(), String> {
    let Some(obj) = params.as_object() else {
        return Err(format!(
            "params is a {}; the measured value is an object",
            redact::value_shape(Some(params))
        ));
    };
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    if keys != INTERRUPT_PARAMS {
        // **The SHAPE, never the names.** The keys are attacker-chosen bytes and this
        // detail lands in the durable `broker.log`, so a key called after a credential —
        // or one four kilobytes long — must not be able to write itself there. Counts
        // carry everything an operator needs to tell the three cases apart (a key too
        // few, a key too many, the wrong pair entirely) and carry nothing a client chose.
        let supplied = keys.len();
        let missing = INTERRUPT_PARAMS
            .iter()
            .filter(|name| !keys.contains(*name))
            .count();
        let unexpected = keys
            .iter()
            .filter(|key| !INTERRUPT_PARAMS.contains(*key))
            .count();
        return Err(format!(
            "params carries {supplied} keys: {unexpected} unexpected, and {missing} of \
             the {} the measured frame carries absent. Key names are client-chosen and \
             are not logged",
            INTERRUPT_PARAMS.len()
        ));
    }
    let (Some(thread), Some(turn)) = (
        obj.get("threadId").and_then(|v| v.as_str()),
        obj.get("turnId").and_then(|v| v.as_str()),
    ) else {
        return Err("params.threadId or params.turnId is not a string".to_string());
    };
    // **The SOLE session thread, not any thread the session has ever been on.**
    //
    // `is_session_thread` is the READ surface's rule — it answers TRUE for every retired
    // thread, on purpose, so a phone can open a previous thread's timeline. An actuation
    // needs the narrower question, and asking it here is what makes "an interrupt can
    // only ever name THIS session's one thread" true by construction: there is exactly
    // one such id at any instant, and a retired thread is not it. The turn check below
    // would refuse the same frames today, but only through a comparison it happens to
    // make; the scoping does not depend on that.
    if env.threads.bound_thread().as_deref() != Some(thread) {
        return Err(format!(
            "thread {} is not this session's one active thread — a thread this session \
             has left is readable, never actuable",
            redact::thread_id(thread)
        ));
    }
    if !env.threads.is_active_turn(thread, turn) {
        return Err(format!(
            "turn {} is not a running turn of thread {} — only the turn this session is \
             currently running can be interrupted",
            redact::thread_id(turn),
            redact::thread_id(thread)
        ));
    }
    Ok(())
}

/// A cursor names a thread, and it may only name this session's.
///
/// Parsed rather than searched. The previous check split on the literal
/// `"requestedThreadId":"` and was defeated by anything else — whitespace after the
/// colon, an escaped id, a different key order putting the session's own id elsewhere in
/// the string. A parse has no such prefix to miss.
fn check_cursor_thread(env: &Env, cursor: &str) -> Result<(), String> {
    let Ok(serde_json::Value::Object(decoded)) = serde_json::from_str::<serde_json::Value>(cursor)
    else {
        return Err(
            "params.cursor is not the measured encoding (a JSON object serialized into a \
             string), so the thread it names cannot be read"
                .to_string(),
        );
    };
    // The COMPLETE structure, not just the selector. A cursor is the second place a thread
    // is named, so an unmeasured member in one is an unmeasured way to name one — reading
    // `requestedThreadId` and forwarding the rest untouched was accepting the structure
    // wholesale.
    let mut members: Vec<&str> = decoded.keys().map(String::as_str).collect();
    members.sort_unstable();
    if members != CURSOR_MEMBERS {
        return Err(format!(
            "the cursor's member set is {members:?}; every measured cursor carries exactly \
             {CURSOR_MEMBERS:?}"
        ));
    }
    // `requestedThreadId` is REQUIRED at the top level and must be this session's. Absent,
    // it was previously accepted — a cursor with no top-level selector went through
    // unexamined while its other members did whatever they do.
    match decoded.get("requestedThreadId") {
        Some(serde_json::Value::String(named)) if env.threads.is_session_thread(named) => {}
        Some(serde_json::Value::String(named)) => {
            return Err(format!(
                "the cursor names thread {}, which was not observed as a session thread",
                redact::thread_id(named)
            ))
        }
        other => {
            return Err(format!(
                "the cursor's requestedThreadId is a {}, not a session thread's id",
                redact::value_shape(other)
            ))
        }
    }
    if !decoded
        .get("rolloutOrdinal")
        .is_some_and(|v| v.as_u64().is_some_and(|n| n <= u64::from(u32::MAX)))
    {
        return Err("the cursor's rolloutOrdinal is not a uint32".to_string());
    }
    if !decoded
        .get("includeAnchor")
        .is_some_and(serde_json::Value::is_boolean)
    {
        return Err("the cursor's includeAnchor is not a boolean".to_string());
    }
    // `scope` is the one nested object, and it carries exactly one member — the measured
    // values are `{"kind":"turns"}` and `{"kind":"itemsByCreatedAtOrdinal"}`. Anything
    // nested beyond that is a place a selector could hide.
    match decoded.get("scope") {
        Some(serde_json::Value::Object(scope))
            if scope.len() == 1 && scope.get("kind").is_some_and(serde_json::Value::is_string) =>
        {
            Ok(())
        }
        other => Err(format!(
            "the cursor's scope is a {}; every measured scope is an object carrying exactly \
             a string `kind`",
            redact::value_shape(other)
        )),
    }
}

/// A phone's approval answer forwards only by consuming a live capability on the head.
fn classify_response(env: &Env, id: &RequestId) -> RelayAction {
    if env
        .capabilities
        .authorize(id, env.threads.bound_thread().as_deref())
    {
        RelayAction::Forward {
            note: "response consumed an authorized capability",
        }
    } else {
        // No live authorized capability: losing-fanout / unsolicited / stale / a thread the
        // keyboard has left — a normal race. Zero bytes, no error (no schema-legal error
        // form), keep the leg open. The id is client-supplied, so it goes through the
        // audit-log renderer.
        RelayAction::DropLogKeepOpen {
            note: format!(
                "method-less response id={} has no live capability",
                redact::request_id(id)
            ),
        }
    }
}

/// Refuse a request: a usable id gets a synthetic error frame; an unusable id gets a
/// silent zero-byte drop. Either way the leg stays open (a policy/unknown refusal is not
/// hostile on its own).
///
/// The frame is shape-identical to the app-server's own errors: `{"id": …, "error":
/// {"code": …, "message": …}}`, with no `jsonrpc` member (codex omits it in both
/// directions).
fn refuse_request(id: Option<RequestId>, code: i64, message: &str, note: String) -> RelayAction {
    match id {
        Some(id) => {
            let frame = json!({
                "id": id.to_value(),
                "error": { "code": code, "message": message }
            })
            .to_string();
            RelayAction::SyntheticError { frame, note }
        }
        None => RelayAction::DropLogKeepOpen {
            note: format!("{note} (no usable id; zero bytes)"),
        },
    }
}

fn refuse_message(reason: RefuseReason) -> (i64, &'static str) {
    match reason {
        RefuseReason::CodeExecBypass => (
            E_POLICY_REFUSED,
            "method refused: code-execution is not permitted",
        ),
        RefuseReason::OwnershipAdjacent => (
            E_POLICY_REFUSED,
            "method refused: it would change session ownership",
        ),
        RefuseReason::NotAllowlisted => (
            E_POLICY_REFUSED,
            "method refused: not permitted for this connection",
        ),
        RefuseReason::RoleNotPermitted => (
            E_POLICY_REFUSED,
            "method refused: not permitted for this endpoint role",
        ),
        RefuseReason::Malformed => (E_POLICY_REFUSED, "message refused"),
    }
}

/// Small constructor so the two hostile-close call sites read cleanly with a `'static`
/// note (the malformed case builds its own owned note).
#[allow(non_snake_case)]
fn DropCloseLeg_(note: &str) -> RelayAction {
    RelayAction::DropCloseLeg {
        note: note.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response_capability::NoCapabilities;
    use crate::session::{NoThreads, SessionThreads, ThreadBinding};

    /// **The refusals a switch producer on the phone leg was actually given, read back
    /// from the capture and pinned by FRAME ID.**
    ///
    /// `fixtures/codex/approval-refusals-0.153.jsonl` holds the refusals the ccd leg was
    /// given while an approval was pending: `thread/start` (id 8100) and
    /// `thread/unsubscribe` (id 8101), both [`refuse_message`] refusals. Only the ccd-leg
    /// rows are pinned: the keyboard leg is a passthrough and this module writes it no
    /// refusal. What makes the frames evidence rather than anecdote is that each is
    /// compared, BY THE REQUEST ID IT ANSWERED, against what its producer writes today.
    ///
    /// **Keyed on the id, never on zip position**: keying error frames by request id
    /// rather than by capture order keeps a reordering — or a fourth refusal — from
    /// silently re-pairing them. The id is the request the broker answered, and it must
    /// appear exactly once: a switch refusal is answered a single time, so a second
    /// error frame for the same id is a fault this rejects rather than collapses.
    ///
    /// **If a refusal is reworded, update the const at its producer — never edit the
    /// .jsonl.** The .jsonl is measured bytes; the producer is the source of truth, and
    /// this test exists to fail when they drift so the producer is what moves.
    ///
    /// It pins the CODE and the MESSAGE together, because a client reads the message
    /// and the code is what it branches on.
    #[test]
    fn the_captured_switch_refusals_are_the_ones_this_module_still_writes() {
        let capture = include_str!("../../../fixtures/codex/approval-refusals-0.153.jsonl");
        // id -> the (code, message) the producer that answered it still writes. Pinned
        // per producer, not by a message that several refusals happen to share.
        let expected: &[(i64, (i64, &str))] = &[
            (8100, refuse_message(RefuseReason::RoleNotPermitted)),
            (8101, refuse_message(RefuseReason::NotAllowlisted)),
        ];
        // Built rejecting duplicates: a `HashMap` collect would overwrite a second
        // frame for an id and let a capture with two errors for one id pass the count
        // check below, so "exactly one error per id" would not be enforced at all.
        let mut errors: std::collections::HashMap<i64, serde_json::Value> =
            std::collections::HashMap::new();
        for frame in capture
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|row| row["conn"].as_str().is_some_and(|c| c.starts_with("ccd")))
            .filter_map(|row| row.get("frame").cloned())
            .filter(|frame| frame.get("error").is_some())
        {
            let id = frame
                .get("id")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or_else(|| panic!("an error frame with no numeric id: {frame}"));
            assert!(
                errors.insert(id, frame).is_none(),
                "the capture holds a second error for id {id}; a switch refusal is \
                 answered once, so one-per-id must hold — update the producer, never \
                 the .jsonl"
            );
        }
        assert_eq!(
            errors.len(),
            expected.len(),
            "the refusals capture holds exactly one error per pinned id; a change in \
             shape means update the producer, never the .jsonl: {errors:#?}"
        );
        for (id, (code, message)) in expected {
            let frame = errors
                .get(id)
                .unwrap_or_else(|| panic!("no refusal frame for id {id} in the capture"));
            assert_eq!(
                frame["error"]["message"].as_str(),
                Some(*message),
                "id {id} was given a different refusal than this build writes — update \
                 the producer, never edit the .jsonl: {frame}"
            );
            assert_eq!(
                frame["error"]["code"].as_i64(),
                Some(*code),
                "and a client branches on the code: {frame}"
            );
        }
    }

    /// Two distinct phone connections. `CONN_A` is the one every single-connection test
    /// uses; `CONN_B` is a sibling the connection-scoped tests drive.
    const CONN_A: ConnId = ConnId(1);
    const CONN_B: ConnId = ConnId(2);

    fn go(text: &str) -> RelayAction {
        go_env(&NoThreads, text)
    }

    fn go_env(threads: &dyn ThreadBinding, text: &str) -> RelayAction {
        go_conn(CONN_A, threads, text)
    }

    fn go_conn(conn: ConnId, threads: &dyn ThreadBinding, text: &str) -> RelayAction {
        let env = Env {
            capabilities: &NoCapabilities,
            threads,
            conn,
        };
        classify(&env, &WsPayload::Text(text.to_string()))
    }

    /// The captured `turn/start` shape class naming `thread`, in the workspace
    /// `cwd` / `roots`. Every turn/start test starts from this and perturbs ONE thing, so a
    /// test aimed at one rule is never silently answered by another.
    fn turn_frame(thread: &str, cwd: serde_json::Value, roots: serde_json::Value) -> String {
        json!({
            "method": "turn/start",
            "id": 11,
            "params": {
                "threadId": thread,
                "input": [{"type": "text", "text": "do the thing", "text_elements": []}],
                // Measured on every captured turn, and the phone's producer writes it too.
                "clientUserMessageId": null,
                "approvalPolicy": "untrusted",
                "approvalsReviewer": "user",
                "sandboxPolicy": null,
                "cwd": cwd,
                "runtimeWorkspaceRoots": roots,
                "permissions": null,
                "environments": null,
                "multiAgentMode": null,
                "responsesapiClientMetadata": null,
                "additionalContext": null,
                "outputSchema": null,
                "collaborationMode": null
            }
        })
        .to_string()
    }

    /// The captured `turn/steer` params naming `thread` and the turn it was composed
    /// against. MEASURED on codex 0.153.4: six keys, three of them required by the
    /// schema, the two metadata fields null (`fixtures/codex/steer-0.153.4.jsonl`).
    fn steer_params(thread: &str, expected_turn: &str) -> serde_json::Value {
        json!({
            "threadId": thread,
            "expectedTurnId": expected_turn,
            "input": [{"type": "text", "text": "also say HELLO", "text_elements": []}],
            "clientUserMessageId": null,
            "responsesapiClientMetadata": null,
            "additionalContext": null
        })
    }

    /// The captured steer as a whole frame.
    fn steer(thread: &str, expected_turn: &str) -> String {
        json!({"method": "turn/steer", "id": 12, "params": steer_params(thread, expected_turn)})
            .to_string()
    }

    /// **The turn a PHONE authors**: the captured shape minus the one key its leg may not
    /// send (`fixtures/codex/compose-refusals-0.153.4.txt`), with every ownership key null
    /// so the turn runs under the thread's own settings.
    fn phone_turn(thread: &str) -> String {
        let mut frame: serde_json::Value =
            serde_json::from_str(&turn(thread)).expect("the captured turn parses");
        let params = frame["params"]
            .as_object_mut()
            .expect("params is an object");
        params.remove("runtimeWorkspaceRoots");
        for key in ["approvalPolicy", "approvalsReviewer", "cwd"] {
            params.insert(key.to_string(), serde_json::Value::Null);
        }
        frame.to_string()
    }

    /// The captured turn on the bound thread, in the bound workspace.
    fn turn(thread: &str) -> String {
        turn_frame(thread, json!(BOUND_CWD), json!([BOUND_ROOT]))
    }

    const BOUND_CWD: &str = "/work/proj";
    /// The session's one workspace root, as a keyboard's `runtimeWorkspaceRoots` carries it.
    const BOUND_ROOT: &str = BOUND_CWD;

    /// The keyboard's connection.
    const KEYBOARD: ConnId = ConnId(9);

    /// Show the session one keyboard frame, as the relay does before forwarding it.
    fn keyboard(threads: &SessionThreads, text: &str) {
        threads.observe_tui_request(
            KEYBOARD,
            &crate::message::classify_shape(&WsPayload::Text(text.to_string())),
        );
    }

    /// The keyboard's `turn/start` on `thread` (request id 11), answered with `turn`.
    fn keyboard_turn(threads: &SessionThreads, thread: &str, turn_id: &str) {
        keyboard(threads, &turn(thread));
        threads.observe_server_frame(
            KEYBOARD,
            &json!({"id": 11, "result": {"turn": {"id": turn_id}}}).to_string(),
        );
    }

    /// The keyboard moves the head to `thread`: its `thread/start` (request id `s2`), then
    /// the correlated answer.
    fn keyboard_switch(threads: &SessionThreads, thread: &str) {
        keyboard(
            threads,
            r#"{"method":"thread/start","id":"s2","params":{}}"#,
        );
        threads.observe_server_frame(
            KEYBOARD,
            &json!({"id": "s2", "result": {"thread": {"id": thread}}}).to_string(),
        );
    }

    /// A [`SessionThreads`] whose head is `thread`: the keyboard's `thread/start`, then its
    /// correlated answer on the same connection.
    fn bound_session(thread: &str) -> SessionThreads {
        let threads = SessionThreads::new();
        keyboard(
            &threads,
            r#"{"method":"thread/start","id":"start-1","params":{"cwd":null}}"#,
        );
        threads.observe_server_frame(
            KEYBOARD,
            &json!({"id": "start-1", "result": {"thread": {"id": thread}, "cwd": BOUND_CWD}})
                .to_string(),
        );
        assert_eq!(threads.bound_thread().as_deref(), Some(thread));
        threads
    }

    #[test]
    fn bypass_request_gets_synthetic_error_zero_bytes() {
        let a = go(r#"{"method":"command/exec","id":5,"params":{"cmd":"rm -rf /"}}"#);
        match a {
            RelayAction::SyntheticError { frame, .. } => {
                let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
                assert_eq!(v["id"], 5);
                assert_eq!(v["error"]["code"], E_POLICY_REFUSED);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bypass_request_without_usable_id_drops_zero_bytes() {
        let a = go(r#"{"method":"fs/writeFile","id":null,"params":{}}"#);
        assert!(matches!(a, RelayAction::DropLogKeepOpen { .. }));
    }

    #[test]
    fn allowlisted_read_forwards() {
        assert!(matches!(
            go(r#"{"method":"thread/loaded/list","id":1,"params":{}}"#),
            RelayAction::Forward { .. }
        ));
    }

    // -----------------------------------------------------------------
    // Thread creation: the admitted-creation root of the lineage.
    // -----------------------------------------------------------------

    fn refused_code(a: &RelayAction) -> i64 {
        match a {
            RelayAction::SyntheticError { frame, .. } => {
                let v: serde_json::Value = serde_json::from_str(frame).unwrap();
                v["error"]["code"].as_i64().unwrap()
            }
            other => panic!("expected a synthetic error, got {other:?}"),
        }
    }

    /// The CLIENT-visible message of a refusal — the sentence that reaches the phone or
    /// the pane, as distinct from [`refused_note`], which is the audit-log detail and never
    /// leaves this machine.
    fn refused_message(a: &RelayAction) -> String {
        match a {
            RelayAction::SyntheticError { frame, .. } => {
                let v: serde_json::Value = serde_json::from_str(frame).unwrap();
                v["error"]["message"].as_str().unwrap().to_string()
            }
            other => panic!("expected a synthetic error, got {other:?}"),
        }
    }

    fn refused_note(a: &RelayAction) -> String {
        match a {
            RelayAction::SyntheticError { note, .. } => note.clone(),
            other => panic!("expected a synthetic error, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // The thread-scoped reads (the 0.153 cross-session leak).
    // -----------------------------------------------------------------

    /// The measured leak, closed: a read naming a thread this session never bound is
    /// refused on BOTH legs.
    ///
    /// This is the regression guard for a real, proven exposure — not a hypothetical.
    /// The 0.153 TUI hands the model a `read_thread` tool; the model called it on a
    /// foreign thread id; the TUI issued `thread/read` then `thread/turns/list`; both
    /// forwarded; and the other session's prompt text and turn items came back and were
    /// given to the model. All three methods carry a caller-chosen `threadId` and every
    /// CodeConnect session on a machine shares one `~/.codex` thread store.
    #[test]
    fn a_thread_scoped_read_naming_a_foreign_thread_is_refused_on_both_legs() {
        for method in ["thread/read", "thread/turns/list", "thread/items/list"] {
            let threads = bound_session("01a0-head");
            let a = go_env(
                &threads,
                &json!({"method":method,"id":method.to_string(),
                        "params":{"threadId":"01a0-a-stranger-s-thread"}})
                .to_string(),
            );
            assert_eq!(
                refused_code(&a),
                E_POLICY_REFUSED,
                "{method} must refuse a foreign thread"
            );
            assert!(
                refused_note(&a).contains("not observed as a session thread"),
                "{}",
                refused_note(&a)
            );
        }
    }

    /// …and the session's OWN thread still reads, or the fix would have broken the
    /// picker and the resume path it exists to serve.
    #[test]
    fn a_thread_scoped_read_on_the_sessions_own_thread_forwards() {
        for method in ["thread/read", "thread/turns/list", "thread/items/list"] {
            let threads = bound_session("01a0-head");
            let a = go_env(
                &threads,
                &json!({"method":method,"id":format!("ok-{method}"),
                        "params":{"threadId":"01a0-head"}})
                .to_string(),
            );
            assert!(
                matches!(a, RelayAction::Forward { .. }),
                "{method} on the bound thread must forward: {a:?}"
            );
        }
    }

    /// One cursor in the MEASURED structure, naming `thread`.
    ///
    /// Verbatim off both releases' wire; the tests build from this rather than from an
    /// abbreviation so a structural rule cannot be satisfied by a shape codex never sent.
    fn cursor_for(thread: &str) -> String {
        json!({"requestedThreadId": thread, "rolloutOrdinal": 29,
               "includeAnchor": false, "scope": {"kind": "turns"}})
        .to_string()
    }

    /// The cursor is a SECOND place a thread can be named. The 0.153 app-server was
    /// measured to ignore it (a bogus `threadId` with a real thread's cursor answered
    /// "thread not loaded: <the bogus id>"), but that is a fact about today's server on a
    /// binary that changes weekly, so a cursor naming a foreign thread is refused too.
    #[test]
    fn a_cursor_naming_a_foreign_thread_is_refused() {
        let threads = bound_session("01a0-head");
        let cursor = cursor_for("01a0-a-stranger");
        let a = go_env(
            &threads,
            &json!({"method":"thread/turns/list","id":"c1",
                    "params":{"threadId":"01a0-head","cursor":cursor}})
            .to_string(),
        );
        assert_eq!(refused_code(&a), E_POLICY_REFUSED);
        assert!(
            refused_note(&a).contains("cursor names thread"),
            "{}",
            refused_note(&a)
        );
    }

    /// The TUI's own paging cursor — which names the thread it is paging — still works.
    #[test]
    fn the_tuis_own_cursor_on_its_own_thread_forwards() {
        let threads = bound_session("01a0-head");
        let cursor = cursor_for("01a0-head");
        let a = go_env(
            &threads,
            &json!({"method":"thread/items/list","id":"c2",
                    "params":{"threadId":"01a0-head","cursor":cursor,"limit":100}})
            .to_string(),
        );
        assert!(matches!(a, RelayAction::Forward { .. }), "{a:?}");
    }

    /// **Positional params.** MEASURED on a real 0.153 app-server:
    /// `{"method":"thread/read","params":["<any thread id>"]}` is answered with that
    /// thread, and all three reads accept an array with the id at index 0. A binding that
    /// reads `params.threadId` sees nothing in one, so an array has to refuse outright.
    #[test]
    fn a_positional_params_array_is_refused_on_every_thread_scoped_read() {
        for method in ["thread/read", "thread/turns/list", "thread/items/list"] {
            let threads = bound_session("01a0-head");
            let a = go_env(
                &threads,
                &json!({"method":method,"id":format!("arr-{method}"),
                        "params":["01a0-a-stranger", null, null, 5, "desc"]})
                .to_string(),
            );
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "{method}: {a:?}");
            assert!(
                refused_note(&a).contains("positional params"),
                "{}",
                refused_note(&a)
            );
        }
    }

    /// Refuse-by-default applies to a read's params. An unmeasured key is refused even
    /// though the 0.153 server was measured to ignore it — "inert today" is a fact about
    /// today's server, and the audit detail names no client text.
    #[test]
    fn a_read_carrying_an_unmeasured_param_is_refused_without_naming_it() {
        let threads = bound_session("01a0-head");
        let a = go_env(
            &threads,
            &json!({"method":"thread/read","id":"x1",
                    "params":{"threadId":"01a0-head","path":"/etc/passwd"}})
            .to_string(),
        );
        assert_eq!(refused_code(&a), E_POLICY_REFUSED);
        assert!(
            refused_note(&a).contains("outside the measured set"),
            "{}",
            refused_note(&a)
        );
        assert!(
            !refused_note(&a).contains("passwd"),
            "the audit detail must not carry the client's own text: {}",
            refused_note(&a)
        );
        // …and a key that IS measured on one read but not another does not leak sideways:
        // `turnId` is `thread/items/list`'s alone.
        let a = go_env(
            &threads,
            &json!({"method":"thread/turns/list","id":"x2",
                    "params":{"threadId":"01a0-head","turnId":"01a0-t"}})
            .to_string(),
        );
        assert_eq!(refused_code(&a), E_POLICY_REFUSED);
    }

    /// The measured frames themselves, key for key, must still forward — or the captured
    /// set would be refusing the TUI it was taken from.
    #[test]
    fn the_measured_read_frames_forward_unchanged() {
        let threads = bound_session("01a0-head");
        let cursor = json!({"requestedThreadId":"01a0-head","rolloutOrdinal":18,
                            "includeAnchor":false,"scope":{"kind":"turns"}})
        .to_string();
        for params in [
            json!({"threadId":"01a0-head"}),
            json!({"cursor":null,"itemsView":"full","limit":1,"sortDirection":"desc",
                   "threadId":"01a0-head"}),
            json!({"cursor":cursor,"limit":100,"sortDirection":"desc",
                   "threadId":"01a0-head","turnId":null}),
        ]
        .into_iter()
        .zip(["thread/read", "thread/turns/list", "thread/items/list"])
        {
            let (params, method) = params;
            let a = go_env(
                &threads,
                &json!({"method":method,"id":format!("m-{method}"),"params":params}).to_string(),
            );
            assert!(
                matches!(a, RelayAction::Forward { .. }),
                "{method}: the captured frame must forward: {a:?}"
            );
        }
    }

    /// **The cursor is parsed, not searched.** The old check split on the literal
    /// `"requestedThreadId":"`, which a single space after the colon walks straight past.
    /// A parse has no prefix to miss.
    #[test]
    fn a_foreign_cursor_is_refused_however_it_is_spelled() {
        let threads = bound_session("01a0-head");
        for cursor in [
            // The measured spelling.
            cursor_for("01a0-a-stranger"),
            // One space after the colon, which the old substring scan did not survive.
            cursor_for("01a0-a-stranger").replace("\":\"01a0", "\": \"01a0"),
            // Reordered, with the session's own id present elsewhere in the string — the
            // shape the old `cursor.contains(id)` short-circuit waved through entirely.
            json!({"scope":{"kind":"turns"},"includeAnchor":true,"rolloutOrdinal":1,
                   "requestedThreadId":"01a0-a-stranger"})
            .to_string(),
        ] {
            let a = go_env(
                &threads,
                &json!({"method":"thread/turns/list","id":"c1",
                        "params":{"threadId":"01a0-head","cursor":cursor}})
                .to_string(),
            );
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "{cursor}");
            assert!(
                refused_note(&a).contains("cursor names thread"),
                "{}",
                refused_note(&a)
            );
        }
    }

    /// **Every captured field is type-checked, not merely named.**
    ///
    /// A key set alone let `itemsView`, `limit`, `sortDirection` and `turnId` carry any
    /// JSON at all — an object, an array, a nested selector — as long as the KEY was one
    /// the captures had shown. Given the positional-array surprise, "the value is
    /// obviously the kind of thing the schema says" is not something to assume about a
    /// client-chosen frame.
    #[test]
    fn every_captured_read_field_is_type_checked() {
        let threads = bound_session("01a0-head");
        // A fresh id per frame: a FORWARDED request occupies its id on the connection, so
        // reusing one would trip the in-flight ledger rather than the rule under test.
        let seq = std::cell::Cell::new(0u32);
        let drive = |method: &str, params: &serde_json::Value| {
            seq.set(seq.get() + 1);
            go_env(
                &threads,
                &json!({"method": method, "id": format!("tc-{}", seq.get()), "params": params})
                    .to_string(),
            )
        };
        let refuse = |method: &str, params: serde_json::Value| {
            let a = drive(method, &params);
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "{method} {params}");
        };
        let ok = |method: &str, params: serde_json::Value| {
            let a = drive(method, &params);
            assert!(
                matches!(a, RelayAction::Forward { .. }),
                "{method} {params}: {a:?}"
            );
        };

        // An enum outside its measured member set, and one that is not a string at all.
        refuse(
            "thread/turns/list",
            json!({"threadId": "01a0-head", "itemsView": "everything"}),
        );
        refuse(
            "thread/turns/list",
            json!({"threadId": "01a0-head", "itemsView": {"kind": "full"}}),
        );
        refuse(
            "thread/turns/list",
            json!({"threadId": "01a0-head", "sortDirection": "sideways"}),
        );
        // …and the schema's members, which the projection bounds, still forward.
        for view in ["notLoaded", "summary", "full"] {
            ok(
                "thread/turns/list",
                json!({"threadId": "01a0-head", "itemsView": view}),
            );
        }
        for dir in ["asc", "desc"] {
            ok(
                "thread/turns/list",
                json!({"threadId": "01a0-head", "sortDirection": dir}),
            );
        }

        // `limit` is a uint32: not a string, not negative, not fractional, not oversized.
        for bad in [
            json!("10"),
            json!(-1),
            json!(1.5),
            json!(u64::from(u32::MAX) + 1),
        ] {
            refuse(
                "thread/items/list",
                json!({"threadId": "01a0-head", "limit": bad}),
            );
        }
        ok(
            "thread/items/list",
            json!({"threadId": "01a0-head", "limit": 100}),
        );

        // `turnId` is a string or null — MEASURED not to be a thread selector, so its
        // type is what is left to pin.
        refuse(
            "thread/items/list",
            json!({"threadId": "01a0-head", "turnId": {"threadId": "01a0-a-stranger"}}),
        );
        ok(
            "thread/items/list",
            json!({"threadId": "01a0-head", "turnId": "01a0-t"}),
        );

        // `threadId` itself must be a string, not an object that happens to contain one.
        refuse("thread/read", json!({"threadId": {"id": "01a0-head"}}));
    }

    /// **The cursor's COMPLETE structure, not just its selector.**
    ///
    /// A cursor is the second place a thread is named, so reading `requestedThreadId` and
    /// forwarding the rest untouched was accepting the structure wholesale — including a
    /// cursor carrying no top-level selector at all, which used to pass.
    #[test]
    fn a_cursor_must_carry_the_whole_measured_structure() {
        let threads = bound_session("01a0-head");
        let seq = std::cell::Cell::new(0u32);
        let refuse = |cursor: serde_json::Value| {
            seq.set(seq.get() + 1);
            let a = go_env(
                &threads,
                &json!({"method":"thread/items/list","id":format!("cs-{}", seq.get()),
                        "params":{"threadId":"01a0-head","cursor":cursor.to_string()}})
                .to_string(),
            );
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "{cursor}");
        };
        // No top-level selector: previously accepted wholesale.
        refuse(json!({"rolloutOrdinal": 1, "includeAnchor": true, "scope": {"kind": "turns"}}));
        // A member nobody measured — a place a selector could hide.
        refuse(
            json!({"requestedThreadId": "01a0-head", "rolloutOrdinal": 1,
                      "includeAnchor": true, "scope": {"kind": "turns"},
                      "alsoRead": "01a0-a-stranger"}),
        );
        // A missing member.
        refuse(
            json!({"requestedThreadId": "01a0-head", "rolloutOrdinal": 1,
                      "includeAnchor": true}),
        );
        // Wrong types for the members that are not the selector.
        refuse(
            json!({"requestedThreadId": "01a0-head", "rolloutOrdinal": "1",
                      "includeAnchor": true, "scope": {"kind": "turns"}}),
        );
        refuse(
            json!({"requestedThreadId": "01a0-head", "rolloutOrdinal": 1,
                      "includeAnchor": "yes", "scope": {"kind": "turns"}}),
        );
        // A `scope` with anything nested beyond the measured single string `kind`.
        refuse(
            json!({"requestedThreadId": "01a0-head", "rolloutOrdinal": 1,
                      "includeAnchor": true,
                      "scope": {"kind": "turns", "thread": "01a0-a-stranger"}}),
        );
        refuse(
            json!({"requestedThreadId": "01a0-head", "rolloutOrdinal": 1,
                      "includeAnchor": true, "scope": {"kind": {"of": "turns"}}}),
        );
        // …and the measured cursor still forwards, or this would pass by refusing all.
        let a = go_env(
            &threads,
            &json!({"method":"thread/items/list","id":"cs-ok",
                    "params":{"threadId":"01a0-head","cursor":cursor_for("01a0-head")}})
            .to_string(),
        );
        assert!(matches!(a, RelayAction::Forward { .. }), "{a:?}");
    }

    /// A cursor in an encoding nobody has measured refuses, rather than being scanned.
    #[test]
    fn a_cursor_that_is_not_the_measured_encoding_is_refused() {
        let threads = bound_session("01a0-head");
        for cursor in ["eyJyZXF1ZXN0ZWRUaHJlYWRJZCI6IngifQ==", "opaque", ""] {
            let a = go_env(
                &threads,
                &json!({"method":"thread/items/list","id":"c2",
                        "params":{"threadId":"01a0-head","cursor":cursor}})
                .to_string(),
            );
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "{cursor:?}");
            assert!(
                refused_note(&a).contains("not the measured encoding"),
                "{}",
                refused_note(&a)
            );
        }
    }

    /// `turnId` is NOT a second thread selector, and that is measured rather than assumed:
    /// against a real 0.153 app-server a foreign turn id on a session thread returns
    /// `{"data":[]}`, and a turn id with no `threadId` is `missing field threadId`. So it
    /// is admitted as the intra-thread filter it is, with the thread binding doing the
    /// scoping — there is nothing here for a binding to add.
    #[test]
    fn a_turn_id_is_an_intra_thread_filter_and_needs_no_binding() {
        let threads = bound_session("01a0-head");
        let a = go_env(
            &threads,
            &json!({"method":"thread/items/list","id":"t1",
                    "params":{"threadId":"01a0-head","turnId":"01a0-a-strangers-turn"}})
            .to_string(),
        );
        assert!(matches!(a, RelayAction::Forward { .. }), "{a:?}");
        // …and it cannot stand in for the thread it does not name.
        let a = go_env(
            &threads,
            &json!({"method":"thread/items/list","id":"t2",
                    "params":{"turnId":"01a0-a-strangers-turn"}})
            .to_string(),
        );
        assert_eq!(refused_code(&a), E_POLICY_REFUSED);
    }

    /// **A top-level JSON-RPC batch cannot bypass classification.** An array carries no
    /// method, so no allowlist cell would ever be consulted for what is inside it; the
    /// relay drops the leg with zero bytes forwarded. (The 0.153 app-server was separately
    /// measured to ignore batches entirely, but a shape the broker cannot classify must
    /// not depend on the server declining it.)
    #[test]
    fn a_top_level_batch_forwards_zero_bytes_and_closes_the_leg() {
        let threads = bound_session("01a0-head");
        let batch = json!([
            {"id":1,"method":"thread/read","params":{"threadId":"01a0-a-stranger"}},
            {"id":2,"method":"turn/start","params":{"threadId":"01a0-head"}}
        ])
        .to_string();
        let a = go_env(&threads, &batch);
        assert!(
            matches!(a, RelayAction::DropCloseLeg { .. }),
            "a batch must close the leg, not be walked into: {a:?}"
        );
    }

    /// The captured-parameter table must cover every method the allowlist binds this way,
    /// or a fourth `ReadSessionThread` method would silently get the empty set. Walked over
    /// every client method codex's schema declares.
    #[test]
    fn the_read_binding_covers_every_read_session_thread_method() {
        let methods = include_str!("../tests/codex-client-methods.txt")
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty());
        let mut bound = 0;
        for method in methods {
            if disposition(JsonRpcKind::Request, method) == Disposition::ReadSessionThread {
                assert!(
                    !read_captured_params(method).is_empty(),
                    "{method} is bound as a thread-scoped read but has no captured \
                     parameter set, so every frame of it would refuse"
                );
                bound += 1;
            }
        }
        assert_eq!(
            bound, 3,
            "the schema's methods reached only {bound} bound reads"
        );
    }

    /// `thread/loaded/list` stays an unscoped forward — not because it names no thread,
    /// but because of what it was measured to return, and the scope is the HOST PROCESS
    /// rather than the calling connection. See
    /// [`crate::allowlist::Disposition::ReadSessionThread`] for the two-session canary and
    /// for the principal stated exactly: the broker accepts many legs into one thread
    /// domain by design, and what bounds that domain is the per-session host, broker and
    /// app-server. The real TUI also sends it at startup, so refusing it would refuse the
    /// boot.
    #[test]
    fn thread_loaded_list_carries_no_thread_and_stays_unscoped() {
        let threads = bound_session("01a0-head");
        let a = go_env(
            &threads,
            r#"{"method":"thread/loaded/list","id":"l1","params":{"limit":20}}"#,
        );
        assert!(matches!(a, RelayAction::Forward { .. }), "{a:?}");
    }

    // -----------------------------------------------------------------
    // The phone's turn/start, through the CLASSIFIER.
    // -----------------------------------------------------------------

    /// **The exact frame the daemon authors for a phone's start is admitted.**
    ///
    /// The two halves of this feature live in two crates, and this is the pin that keeps
    /// them one thing. `ccd::codex_link::compose_frame` builds the phone's `turn/start`,
    /// and [`check_phone_turn_projection`] then has to admit it. A rule this file could
    /// satisfy in principle but that frame could not is a feature that compiles and cannot
    /// start a turn — which is exactly the failure the `serviceTier` measurement found on
    /// the TUI's own frame.
    ///
    /// So the frame below is a transcription of the one the daemon emits, and its ccd-side
    /// twin (`the_frame_a_phone_start_authors_is_the_one_the_broker_admits`) asserts the
    /// daemon really emits it and that this classifier admits it. Neither test is worth
    /// much alone.
    ///
    /// **Mutation:** drop any of the twelve null params from the daemon's frame and this
    /// goes red — the pin is the exact key set.
    #[test]
    fn the_frame_the_daemon_authors_for_a_phones_start_is_admitted() {
        let threads = bound_session("01a0-head");
        // Verbatim from `compose_frame`'s `turn/start` arm: fourteen keys, every one but
        // `threadId` and `input` null.
        let authored = json!({
            "method": "turn/start",
            "id": 3,
            "params": {
                "threadId": "01a0-head",
                "input": [{"type": "text", "text": "do the thing", "text_elements": []}],
                "approvalPolicy": null,
                "approvalsReviewer": null,
                "cwd": null,
                // **No `runtimeWorkspaceRoots`, and that is MEASURED rather than tidy.**
                // codex 0.153.4 refuses the key on a `turn/start` from any client that
                // did not declare `experimentalApi` at `initialize` — which the TUI does
                // and the ccd control link does not
                // (`fixtures/codex/compose-refusals-0.153.4.txt`). The alternative was to
                // declare that capability on the phone's leg, which buys one field by
                // handing it the experimental parameter surface of the whole API.
                "sandboxPolicy": null,
                "permissions": null,
                "environments": null,
                "multiAgentMode": null,
                "responsesapiClientMetadata": null,
                "additionalContext": null,
                "outputSchema": null,
                "collaborationMode": null,
                "clientUserMessageId": null
            }
        });
        let a = go_env(&threads, &authored.to_string());
        assert!(
            matches!(a, RelayAction::Forward { .. }),
            "the frame the daemon actually authors must be admitted: {a:?}"
        );
    }

    /// **The ccd `turn/start` the app-server actually received, out of the fixture.**
    ///
    /// The live compose gate writes this row itself, from
    /// `crate::codex_link::compose_frame`'s own output rather than a copy of it, so the
    /// bytes here are the bytes that leg put on the socket. `cwd` is scrubbed to
    /// [`BOUND_CWD`] in the capture — it is the only value in the frame that names the
    /// machine it ran on — which is why this session's own cwd needs no substitution
    /// below.
    fn recorded_ccd_turn_start() -> serde_json::Value {
        const CAPTURE: &str = include_str!("../../../fixtures/codex/compose-0.153.4.jsonl");
        CAPTURE
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|row| {
                row["conn"] == "ccd"
                    && row["dir"] == "c2s"
                    && row["frame"]["method"] == "turn/start"
            })
            .expect("the compose capture carries the ccd leg's turn/start")["frame"]
            .clone()
    }

    /// **The fourteen keys, pinned to the frame the wire carried rather than to a third
    /// copy of them.**
    ///
    /// [`the_frame_the_daemon_authors_for_a_phones_start_is_admitted`] states the frame in
    /// Rust here, and its twin in `ccd` states it again from the encoder's side. Both are
    /// worth having — but two hand-written statements agreeing with each other is not
    /// evidence about a wire. A key added to [`PHONE_TURN_PARAMS`] and to `compose_frame`
    /// in one edit would leave every one of those assertions green while the admission
    /// widened.
    ///
    /// So the list is pinned here to a recorded frame instead: the `ccd` c2s row of
    /// `fixtures/codex/compose-0.153.4.jsonl`, which the live gate wrote out of
    /// `compose_frame`'s own output and the real app-server received on a busy thread —
    /// the run that answered it `-32001 turn refused: this session is already running a
    /// turn`, recorded beside it. A key with no counterpart in that frame has nothing
    /// behind it.
    ///
    /// **Mutation:** add any key to [`PHONE_TURN_PARAMS`] — this goes red naming it, and
    /// the widened cell stays green.
    #[test]
    fn the_phones_fourteen_keys_are_the_ones_the_wire_carried() {
        let recorded = recorded_ccd_turn_start();
        let params = recorded["params"]
            .as_object()
            .expect("a recorded turn/start has params");
        let mut recorded_keys: Vec<&str> = params.keys().map(String::as_str).collect();
        recorded_keys.sort_unstable();
        assert_eq!(
            recorded_keys, PHONE_TURN_PARAMS,
            "the projection admits exactly the keys the daemon put on the wire; a \
             difference either way is a key admitted with no frame behind it or a frame \
             this broker would refuse"
        );

        // The recorded run carried the launch's `approvalPolicy`, `approvalsReviewer` and
        // `cwd`. A phone names no policy of its own, so those bytes are refused now, and the
        // same frame with the three set to null is admitted. The session's own thread id is
        // substituted because it is a property of the run that recorded the frame rather
        // than of the frame's shape — every other byte is the recorded one.
        let mut frame = recorded.clone();
        frame["id"] = json!(3);
        frame["params"]["threadId"] = json!("01a0-head");
        let launch_valued = go_env(&bound_session("01a0-head"), &frame.to_string());
        assert_eq!(
            refused_message(&launch_valued),
            "turn refused: it is not the shape this session accepts from a phone"
        );
        for key in ["approvalPolicy", "approvalsReviewer", "cwd"] {
            assert!(recorded["params"][key].is_string(), "params.{key}");
            frame["params"][key] = serde_json::Value::Null;
        }
        let admitted = go_env(&bound_session("01a0-head"), &frame.to_string());
        assert!(
            matches!(admitted, RelayAction::Forward { .. }),
            "the recorded frame with its ownership values null must be admitted: {admitted:?}"
        );
    }

    /// **A phone's turn names no policy of its own: every key but `threadId` and `input` is
    /// null, and that turn forwards.**
    ///
    /// It runs under whatever the thread currently has, which only the keyboard can change.
    ///
    #[test]
    fn a_phones_null_ownership_turn_is_forwarded() {
        let threads = bound_session("01a0-head");
        let frame: serde_json::Value =
            serde_json::from_str(&phone_turn("01a0-head")).expect("the phone's turn parses");
        let params = frame["params"].as_object().expect("params is an object");
        let nulls = params
            .iter()
            .filter(|(key, _)| *key != "threadId" && *key != "input")
            .inspect(|(key, value)| assert!(value.is_null(), "params.{key} is {value}"))
            .count();
        assert_eq!((params.len(), nulls), (14, 12));

        let a = go_env(&threads, &frame.to_string());
        assert!(
            matches!(a, RelayAction::Forward { .. }),
            "a null-ownership phone turn on the idle bound thread must forward: {a:?}"
        );
    }

    /// **A phone's turn that names ANY policy of its own is refused as not the phone's
    /// shape**, including a value equal to the one the session was launched with.
    ///
    /// **Mutation:** drop any key from [`PHONE_TURN_NULL_PARAMS`] and its row forwards.
    #[test]
    fn a_phone_turn_naming_any_policy_of_its_own_is_refused() {
        for (key, value) in [
            ("approvalPolicy", json!("never")),
            ("approvalPolicy", json!("untrusted")),
            ("approvalsReviewer", json!("auto_review")),
            ("approvalsReviewer", json!("user")),
            ("cwd", json!("/")),
            ("cwd", json!(BOUND_CWD)),
            ("sandboxPolicy", json!({"type": "dangerFullAccess"})),
            ("permissions", json!({})),
            (
                "collaborationMode",
                json!({"mode": "default", "settings": {"model": "gpt-5", "reasoning_effort": null, "developer_instructions": null}}),
            ),
            ("environments", json!([])),
            ("multiAgentMode", json!("enabled")),
            ("outputSchema", json!({"type": "object"})),
            ("additionalContext", json!("more")),
            ("responsesapiClientMetadata", json!({})),
            ("clientUserMessageId", json!("m-1")),
        ] {
            // Its own session per row: a row that wrongly forwarded would mark the thread
            // busy and answer the next row with the idle rule instead of this one.
            let threads = bound_session("01a0-head");
            let mut frame: serde_json::Value =
                serde_json::from_str(&phone_turn("01a0-head")).expect("the phone's turn parses");
            frame["params"][key] = value.clone();
            let a = go_env(&threads, &frame.to_string());
            assert_eq!(
                refused_message(&a),
                "turn refused: it is not the shape this session accepts from a phone",
                "params.{key} = {value}"
            );
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "params.{key} = {value}");
        }
    }

    /// **A phone's `thread/resume` is exactly `{"threadId": <string>}` naming a session
    /// thread.** ccd sends that frame and nothing else, so any other key is refused, even
    /// one whose value matches the launch.
    ///
    /// **Mutation:** accept any key whose value matches the launch and those rows forward.
    #[test]
    fn a_phones_resume_is_exactly_the_thread_it_names() {
        let threads = bound_session("01a0-head");
        for (id, params) in [
            (1, json!({"threadId": "01a0-head", "sandbox": "read-only"})),
            (
                2,
                json!({"threadId": "01a0-head", "approvalPolicy": "untrusted"}),
            ),
            (
                3,
                json!({"threadId": "01a0-head", "approvalsReviewer": "user"}),
            ),
            (
                4,
                json!({"threadId": "01a0-head", "runtimeWorkspaceRoots": [BOUND_ROOT]}),
            ),
            (5, json!({"threadId": "01a0-head", "cwd": BOUND_CWD})),
            (6, json!({"threadId": "01a0-head", "cwd": null})),
            (7, json!({"threadId": 7})),
            (8, json!({})),
            (9, json!(["01a0-head"])),
        ] {
            let frame = json!({"method": "thread/resume", "id": id, "params": params});
            let a = go_env(&threads, &frame.to_string());
            assert_eq!(
                refused_message(&a),
                "resume refused: it is not the shape this session accepts from a phone",
                "{frame}"
            );
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "{frame}");
            assert!(
                !refused_note(&a).contains("sandbox") && !refused_note(&a).contains("/work"),
                "key names and values are client-chosen and must not reach the log: {}",
                refused_note(&a)
            );
        }
        // The right shape naming a thread this session does not have.
        let stranger = go_env(
            &threads,
            r#"{"method":"thread/resume","id":10,"params":{"threadId":"99-not-ours"}}"#,
        );
        assert_eq!(
            refused_message(&stranger),
            "resume refused: target thread is not bound to this session"
        );
        // And the frame ccd sends forwards.
        let ours = go_env(
            &threads,
            r#"{"method":"thread/resume","id":11,"params":{"threadId":"01a0-head"}}"#,
        );
        assert!(matches!(ours, RelayAction::Forward { .. }), "{ours:?}");
    }

    /// **A phone that names a workspace is refused, and naming the RIGHT one does not
    /// help.**
    ///
    /// Its own session, because the rule under test is about the roots and a session with
    /// a turn already running would refuse this frame for being busy instead — which is
    /// how an earlier form of this assertion passed while the rule it names was not being
    /// exercised at all.
    ///
    /// **Mutation:** admit only the ABSENCE (`leg == Phone && roots.is_none()`) rather
    /// than refusing every presence, and this goes red: the equal-value frame forwards,
    /// the app-server then refuses it `requires experimentalApi capability`, and the phone
    /// has spent a durable claim and an upstream write to be told what this broker knew.
    #[test]
    fn a_phone_that_names_a_workspace_is_refused_even_when_it_names_the_right_one() {
        let threads = bound_session("01a0-head");
        for roots in [
            json!([BOUND_ROOT]),
            json!(["/work/other"]),
            json!([]),
            serde_json::Value::Null,
        ] {
            let mut named: serde_json::Value =
                serde_json::from_str(&phone_turn("01a0-head")).expect("the phone's turn parses");
            named["params"]["runtimeWorkspaceRoots"] = roots.clone();
            let a = go_env(&threads, &named.to_string());
            assert_eq!(
                refused_code(&a),
                E_POLICY_REFUSED,
                "the phone may not name a workspace: {roots}"
            );
            // **Refused by the PROJECTION**, before anything asks what the value says:
            // `runtimeWorkspaceRoots` is not among the fourteen keys this leg's producer
            // writes, so the frame is refused for not being the phone's frame.
            assert!(
                refused_message(&a).contains("shape this session accepts from a phone"),
                "{}",
                refused_message(&a)
            );
            // And the note names the SHAPE, never the path.
            assert!(
                !refused_note(&a).contains("/work"),
                "the workspace value leaked into the audit log: {}",
                refused_note(&a)
            );
        }
    }

    /// **The exact frame the daemon authors for a phone's steer is admitted.**
    ///
    /// [`the_frame_the_daemon_authors_for_a_phones_start_is_admitted`]'s twin, and the
    /// tighter of the two: the steer's param set is pinned to exactly six keys, so a
    /// daemon that sent five or seven would be refused.
    #[test]
    fn the_frame_the_daemon_authors_for_a_phones_steer_is_admitted() {
        let threads = bound_session("01a0-head");
        keyboard_turn(&threads, "01a0-head", "01a0-turn");
        // Verbatim from `compose_turn`'s `turn/steer` arm.
        let authored = json!({
            "method": "turn/steer",
            "id": 4,
            "params": {
                "threadId": "01a0-head",
                "expectedTurnId": "01a0-turn",
                "input": [{"type": "text", "text": "also say HELLO", "text_elements": []}],
                "clientUserMessageId": null,
                "responsesapiClientMetadata": null,
                "additionalContext": null
            }
        });
        let a = go_env(&threads, &authored.to_string());
        assert!(
            matches!(a, RelayAction::Forward { .. }),
            "the frame the daemon actually authors must be admitted: {a:?}"
        );
    }

    /// **The phone's `turn/start` is pinned to the phone's OWN frame, not the
    /// keyboard's twenty-four names.**
    ///
    /// A phone cell checked against the keyboard's parameter set, plus an input check,
    /// admits every name the operator's frame may carry. Reproduced over the real
    /// relay with the upstream bytes recorded: `model`, `effort`, `serviceTier`,
    /// `personality`, `summary`, a 4096-byte `clientUserMessageId`, arbitrary
    /// `text_elements` and a 2 MB text ALL forwarded from this leg. Five of those names
    /// have no value rule at all — `model`/`effort` are cross-checked only inside
    /// `check_collaboration_mode`, which never runs when `collaborationMode` is null,
    /// which is exactly what the phone sends.
    ///
    /// The report and `fixtures/codex/compose-refusals-0.153.4.txt` both state the
    /// narrowness as a property of the CHANGE. It was a property of the daemon's encoder.
    /// Every other cell this leg can actuate pins its params exhaustively
    /// ([`STEER_PARAMS`], [`INTERRUPT_PARAMS`]); this was the one that did not, and it was
    /// the widest.
    ///
    /// **Mutation:** admit `serviceTier`, or a second input item, or a non-empty
    /// `text_elements`, or drop the size cap — each turns a row below green.
    #[test]
    fn the_phones_turn_start_is_pinned_to_the_phones_own_frame() {
        let threads = bound_session("01a0-head");
        let seq = std::cell::Cell::new(0u32);
        let with = |mutate: &dyn Fn(&mut serde_json::Value)| {
            seq.set(seq.get() + 1);
            let mut frame: serde_json::Value =
                serde_json::from_str(&phone_turn("01a0-head")).expect("the phone's turn parses");
            frame["id"] = json!(format!("p1-{}", seq.get()));
            mutate(&mut frame);
            go_env(&threads, &frame.to_string())
        };

        // The frame the daemon authors is admitted, unchanged.
        assert!(
            matches!(with(&|_| {}), RelayAction::Forward { .. }),
            "the daemon's own frame must still forward"
        );

        // **Every TUI-only name is refused**, including the preference fields the keyboard
        // sends. `serviceTier` is the sharpest: only the person at the machine sets it, and
        // that argument is about the OTHER leg.
        for key in [
            "model",
            "effort",
            "serviceTier",
            "serviceTierForTurn",
            "personality",
            "summary",
            "toolOutput",
            "turnTrigger",
            "cyberAccessProgram",
            "runtimeWorkspaceRoots",
            "aFutureKeyNobodyHasMeasured",
        ] {
            let a = with(&|f| f["params"][key] = json!("anything"));
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "params.{key}");
            assert!(
                !refused_note(&a).contains(key),
                "the key name is client-chosen and must not reach the log: {}",
                refused_note(&a)
            );
        }
        // And a MISSING key is refused too: the pin is the exact set, not a subset.
        for key in ["cwd", "sandboxPolicy", "outputSchema", "collaborationMode"] {
            let a = with(&|f| {
                f["params"]
                    .as_object_mut()
                    .expect("params is an object")
                    .remove(key);
            });
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "missing params.{key}");
        }

        // **`clientUserMessageId` and `collaborationMode` are exactly null**, which is the
        // parity `STEER_NULL_PARAMS`' own comment claimed `turn/start` already had and did
        // not: a 4096-byte id forwarded from this leg.
        for (key, value) in [
            ("clientUserMessageId", json!("x".repeat(4096))),
            ("clientUserMessageId", json!("m-1")),
            ("collaborationMode", json!({"mode": "default"})),
        ] {
            let a = with(&|f| f["params"][key] = value.clone());
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "params.{key}");
        }
    }

    /// **A phone composes ONE text item, of the measured shape, within the bound its
    /// own daemon advertises.**
    ///
    /// Split from the key-set gate because these are three sub-values a key-set check
    /// cannot fence: any number of items, any `text_elements` array, and no size bound at all —
    /// a 2 MB text forwarded. The daemon caps at `MAX_COMPOSE_BYTES` and the broker is the
    /// boundary; a rule the producer keeps and the boundary does not is the shape this
    /// module exists to refuse.
    #[test]
    fn a_phone_composes_one_measured_text_item_within_the_bound() {
        let threads = bound_session("01a0-head");
        keyboard_turn(&threads, "01a0-head", "01a0-turn");
        let seq = std::cell::Cell::new(0u32);
        // Driven on the STEER cell, which is idle-independent, so the same input rule can
        // be asserted without a fresh session per row. The start cell shares the rule.
        let with_input = |input: serde_json::Value| {
            seq.set(seq.get() + 1);
            let mut params = steer_params("01a0-head", "01a0-turn");
            params["input"] = input;
            go_env(
                &threads,
                &json!({"method":"turn/steer","id":format!("i-{}", seq.get()),
                        "params":params})
                .to_string(),
            )
        };
        let one = |text: serde_json::Value| json!([{"type":"text","text":text,"text_elements":[]}]);

        assert!(matches!(
            with_input(one(json!("hello"))),
            RelayAction::Forward { .. }
        ));

        // TWO items: the daemon writes one, always.
        assert_eq!(
            refused_code(&with_input(json!([
                {"type":"text","text":"first","text_elements":[]},
                {"type":"text","text":"second","text_elements":[]}
            ]))),
            E_POLICY_REFUSED
        );
        // A NON-EMPTY `text_elements`: every capture is `[]`, and `TextElement` carries a
        // `placeholder` the schema does not say is inert.
        assert_eq!(
            refused_code(&with_input(json!([{
                "type":"text","text":"hi",
                "text_elements":[{"byteRange":{"start":0,"end":2},
                                  "placeholder":"/Users/someone/.ssh/id_rsa"}]
            }]))),
            E_POLICY_REFUSED
        );
        // An ABSENT `text_elements`: the daemon writes it, so its absence is not this
        // producer's frame.
        assert_eq!(
            refused_code(&with_input(json!([{"type":"text","text":"hi"}]))),
            E_POLICY_REFUSED
        );
        // Over the bound the phone's own daemon advertises.
        let too_long = "x".repeat(MAX_PHONE_TEXT_BYTES + 1);
        let a = with_input(one(json!(too_long)));
        assert_eq!(refused_code(&a), E_POLICY_REFUSED);
        assert!(
            !refused_note(&a).contains("xxxx"),
            "the text must not reach the log: {}",
            refused_note(&a)
        );
        // Exactly at the bound is fine.
        assert!(matches!(
            with_input(one(json!("x".repeat(MAX_PHONE_TEXT_BYTES)))),
            RelayAction::Forward { .. }
        ));
    }

    /// **A turn the phone starts is admitted only while the thread is IDLE.**
    ///
    /// The head-check opens a ccd path, and this is the rule that keeps it from
    /// being the one the app-server would have applied. MEASURED on 0.153.4 against the
    /// server's own socket: a byte-identical `turn/start` sent while a turn is running is
    /// ACCEPTED and answered with the RUNNING turn's id — an implicit steer with no
    /// `expectedTurnId` and therefore no staleness guard. So a start refuses while a turn
    /// is busy, and `turn/steer` — which carries the guard natively — is the method for
    /// that case.
    ///
    #[test]
    fn the_phone_starts_a_turn_only_on_an_idle_thread() {
        let threads = bound_session("01a0-head");

        // Idle: the phone's start is admitted.
        let idle = go_env(&threads, &phone_turn("01a0-head"));
        assert!(
            matches!(idle, RelayAction::Forward { .. }),
            "an idle thread admits the phone's turn: {idle:?}"
        );
        // That admission marked the thread busy.
        threads.observe_server_frame(
            CONN_A,
            &json!({"id": 11, "result": {"turn": {"id": "01a0-turn"}}}).to_string(),
        );

        // Busy: refused HERE, with zero bytes, rather than becoming the server's
        // unguarded implicit steer.
        let mut busy_frame: serde_json::Value =
            serde_json::from_str(&phone_turn("01a0-head")).expect("the phone's turn parses");
        busy_frame["id"] = json!(12);
        let busy = go_env(&threads, &busy_frame.to_string());
        assert_eq!(refused_code(&busy), E_POLICY_REFUSED);
        assert!(
            refused_message(&busy).contains("already running a turn"),
            "the refusal must say which condition failed: {}",
            refused_message(&busy)
        );

        // **And a keyboard turn in flight makes it busy too**: the phone's start would
        // otherwise become an implicit steer into the operator's turn.
        let threads = bound_session("01a0-head");
        keyboard(&threads, &turn("01a0-head"));
        let refused = go_env(&threads, &phone_turn("01a0-head"));
        assert!(
            refused_message(&refused).contains("already running a turn"),
            "{}",
            refused_message(&refused)
        );
    }

    /// **The phone's turn names no policy and no workspace, and only the session's head.**
    ///
    /// Admitting the method by role is not admitting the frame: an approval policy, a
    /// thread this session did not create, or a workspace each forwards zero bytes. These
    /// are the rows a widening mutation turns green.
    ///
    /// The HEAD assertion is the positive control, and it is the row a *narrowing* mutation
    /// turns red. Every other assertion here says "this is refused", so a mutation that makes
    /// the classifier refuse ccd's `turn/start` unconditionally — deleting the
    /// `NullOwnedIdleTurn` arm, or fusing the whole leg into a blanket policy refusal —
    /// satisfies all of them while breaking the phone completely. The phone's own turn on
    /// this session's bound idle thread MUST forward; that is the mutation this row catches.
    #[test]
    fn the_phones_turn_is_null_owned_head_checked_and_names_no_workspace() {
        // The positive control. It gets its OWN binding because admitting a turn marks that
        // thread busy, and a busy thread would answer the rows below with the wrong refusal
        // — each of those is aimed at one rule and must not be satisfied by another.
        let admitted = bound_session("01a0-head");
        let forwarded = go_env(&admitted, &phone_turn("01a0-head"));
        assert!(
            matches!(forwarded, RelayAction::Forward { .. }),
            "the phone's own turn, on its own bound idle thread, must forward — without \
             this row a blanket refusal satisfies every assertion below: {forwarded:?}"
        );

        let threads = bound_session("01a0-head");
        // The turn names an approval policy of its own.
        let mut foreign: serde_json::Value =
            serde_json::from_str(&phone_turn("01a0-head")).expect("the phone's turn parses");
        foreign["params"]["approvalPolicy"] = json!("never");
        assert_eq!(
            refused_code(&go_env(&threads, &foreign.to_string())),
            E_POLICY_REFUSED
        );
        // A thread this session does not have bound.
        assert_eq!(
            refused_code(&go_env(&threads, &phone_turn("01a0-a-stranger"))),
            E_POLICY_REFUSED
        );
        // The right thread, naming a workspace.
        let mut elsewhere: serde_json::Value =
            serde_json::from_str(&phone_turn("01a0-head")).expect("the phone's turn parses");
        elsewhere["params"]["cwd"] = json!("/work/other");
        let elsewhere = elsewhere.to_string();
        assert_eq!(
            refused_code(&go_env(&threads, &elsewhere)),
            E_POLICY_REFUSED
        );
    }

    #[test]
    fn request_with_unusable_id_never_forwards() {
        // Even an allowlisted method with a null id is schema-invalid: zero bytes.
        let a = go(r#"{"method":"app/list","id":null,"params":{}}"#);
        assert!(matches!(a, RelayAction::DropLogKeepOpen { .. }));
    }

    #[test]
    fn ccd_start_fork_turn_are_role_refused() {
        // `turn/start` is not in this list — it is admitted on an idle thread under
        // its own disposition. Thread CREATION stays refused by role: ccd attaches to the
        // thread the operator's session made and never makes one.
        for m in ["thread/start", "thread/fork"] {
            let text = format!(
                r#"{{"method":"{m}","id":1,"params":{{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":"read-only"}}}}"#
            );
            let a = go(&text);
            match a {
                RelayAction::SyntheticError { frame, .. } => {
                    let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
                    assert_eq!(v["error"]["code"], E_POLICY_REFUSED, "{m}");
                }
                other => panic!("{m}: {other:?}"),
            }
        }
    }

    #[test]
    fn ccd_resume_of_unknown_thread_refused_known_thread_ok() {
        let threads = bound_session("01a0-session-thread");
        // Unknown thread -> refused.
        let bad = go_env(
            &threads,
            r#"{"method":"thread/resume","id":1,"params":{"threadId":"99-not-ours"}}"#,
        );
        assert!(matches!(bad, RelayAction::SyntheticError { .. }));
        // Session thread -> forwarded (ownership-free resume is absence-benign).
        let ok = go_env(
            &threads,
            r#"{"method":"thread/resume","id":2,"params":{"threadId":"01a0-session-thread"}}"#,
        );
        assert!(matches!(ok, RelayAction::Forward { .. }));
    }

    #[test]
    fn notification_refused_zero_bytes_keep_open() {
        let a = go(r#"{"method":"thread/started","params":{}}"#);
        assert!(matches!(a, RelayAction::DropLogKeepOpen { .. }));
    }

    #[test]
    fn notification_shaped_dangerous_methods_are_refused() {
        // A method-only frame (no id) naming a dangerous/ownership method is a
        // notification; it must forward zero bytes, never execute.
        for m in [
            "command/exec",
            "fs/writeFile",
            "process/spawn",
            "thread/start",
        ] {
            let text = format!(r#"{{"method":"{m}","params":{{}}}}"#);
            assert!(
                matches!(go(&text), RelayAction::DropLogKeepOpen { .. }),
                "{m} notification",
            );
        }
    }

    #[test]
    fn initialized_notification_forwards() {
        assert!(matches!(
            go(r#"{"method":"initialized","params":{}}"#),
            RelayAction::Forward { .. }
        ));
    }

    #[test]
    fn method_less_response_no_capability_drops_zero_bytes() {
        let a = go(r#"{"id":0,"result":{"decision":{"accept":{}}}}"#);
        assert!(matches!(a, RelayAction::DropLogKeepOpen { .. }));
    }

    #[test]
    fn array_binary_malformed_close_leg() {
        assert!(matches!(go("[]"), RelayAction::DropCloseLeg { .. }));
        assert!(matches!(go("{"), RelayAction::DropCloseLeg { .. }));
        let env = Env {
            capabilities: &NoCapabilities,
            threads: &NoThreads,
            conn: CONN_A,
        };
        assert!(matches!(
            classify(&env, &WsPayload::Binary),
            RelayAction::DropCloseLeg { .. }
        ));
    }

    // -----------------------------------------------------------------
    // The total id ledger and audit-log hygiene, through the CLASSIFIER.
    // -----------------------------------------------------------------

    fn dropped_note(a: &RelayAction) -> String {
        match a {
            RelayAction::DropLogKeepOpen { note } => note.clone(),
            other => panic!("expected a zero-byte drop, got {other:?}"),
        }
    }

    fn read(id: &str) -> String {
        format!(r#"{{"method":"thread/loaded/list","id":"{id}","params":{{}}}}"#)
    }

    // A forwarded request whose id is ALREADY outstanding on the connection is dropped
    // (zero upstream bytes), the leg is kept open, and the event is counted.
    #[test]
    fn a_request_reusing_an_in_flight_id_is_dropped_and_counted() {
        let threads = SessionThreads::new();
        assert!(matches!(
            go_env(&threads, &read("dup")),
            RelayAction::Forward { .. }
        ));
        let a = go_env(&threads, &read("dup"));
        assert!(
            matches!(a, RelayAction::DropLogKeepOpen { .. }),
            "a reused in-flight id is dropped, never answered and never forwarded: {a:?}"
        );
        assert!(
            dropped_note(&a).contains("already outstanding"),
            "{}",
            dropped_note(&a)
        );
        assert_eq!(threads.id_ledger_counts().reused_in_flight, 1);
        // The SAME id on a different connection is fine (ids are per-connection).
        assert!(matches!(
            go_conn(CONN_B, &threads, &read("dup")),
            RelayAction::Forward { .. }
        ));
        // And once the response lands, the id is usable again on CONN_A.
        threads.observe_server_frame(CONN_A, r#"{"id":"dup","result":{"data":[]}}"#);
        assert!(matches!(
            go_env(&threads, &read("dup")),
            RelayAction::Forward { .. }
        ));
    }

    // An over-long request id is refused (zero bytes) and never stored, so it cannot
    // grow the ledger.
    #[test]
    fn an_over_long_request_id_is_dropped_and_never_stored() {
        let threads = SessionThreads::new();
        let long = "x".repeat(crate::session::MAX_REQUEST_ID_BYTES + 1);
        let a = go_env(&threads, &read(&long));
        assert!(matches!(a, RelayAction::DropLogKeepOpen { .. }), "{a:?}");
        assert!(
            dropped_note(&a).contains("byte cap"),
            "{}",
            dropped_note(&a)
        );
        // The note must not echo the id itself.
        assert!(!dropped_note(&a).contains(&long), "{}", dropped_note(&a));
        assert_eq!(threads.id_ledger_counts().oversized, 1);
    }

    // A client-chosen thread id is never echoed into the audit log unless it matches the
    // MEASURED wire grammar (a lowercase 36-byte UUID).
    #[test]
    fn a_non_conforming_thread_id_is_never_echoed_in_a_refusal() {
        const INJECTED: &str = "zzz_injected\nFAKE LOG LINE";
        // The turn head-check.
        let note = refused_note(&go_env(&bound_session("01a0-head"), &phone_turn(INJECTED)));
        assert!(!note.contains("zzz_injected"), "{note}");
        assert!(!note.contains("FAKE LOG LINE"), "{note}");
        assert!(!note.contains('\n'), "{note:?}");
        assert!(note.contains("<non-conforming id, 26 bytes>"), "{note}");
        // The BOUND head goes through the same renderer, so the session's own non-UUID test
        // thread is withheld too.
        assert!(!note.contains("01a0-head"), "{note}");

        // The resume-binding check.
        let note = refused_note(&go_env(
            &bound_session("01a0-head"),
            &format!(
                r#"{{"method":"thread/resume","id":1,"params":{{"threadId":{}}}}}"#,
                json!(INJECTED)
            ),
        ));
        assert!(!note.contains("zzz_injected"), "{note}");
        assert!(!note.contains('\n'), "{note:?}");

        // A CONFORMING id is still readable, which is the point of grammar-gating rather
        // than blanket withholding.
        const REAL: &str = "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86";
        let note = refused_note(&go_env(&bound_session("01a0-head"), &phone_turn(REAL)));
        assert!(note.contains(REAL), "{note}");
    }

    // A client-chosen METHOD is never echoed unless it matches the census grammar, and
    // a client-chosen RESPONSE id is never echoed unless it is a plain identifier.
    #[test]
    fn a_non_conforming_method_or_response_id_is_never_echoed() {
        const INJECTED_METHOD: &str = "zzz\nFAKE: forward (request allowlisted)";
        // An unknown method (refuse-by-default) as a REQUEST…
        let note = refused_note(&go(&format!(
            r#"{{"method":{},"id":1,"params":{{}}}}"#,
            json!(INJECTED_METHOD)
        )));
        assert!(!note.contains("FAKE"), "{note}");
        assert!(!note.contains('\n'), "{note:?}");
        // …and as a NOTIFICATION (no id, so a zero-byte drop).
        let note = dropped_note(&go(&format!(
            r#"{{"method":{},"params":{{}}}}"#,
            json!(INJECTED_METHOD)
        )));
        assert!(!note.contains("FAKE"), "{note}");
        assert!(!note.contains('\n'), "{note:?}");
        // A real method is still named in full.
        let note = refused_note(&go(
            r#"{"method":"future/method/nobody/pinned","id":1,"params":{}}"#,
        ));
        assert!(note.contains("future/method/nobody/pinned"), "{note}");

        // An unsolicited method-less response with a hostile string id.
        let note = dropped_note(&go(&format!(
            r#"{{"id":{},"result":{{"decision":{{"accept":{{}}}}}}}}"#,
            json!("id\nFAKE: forward (request allowlisted)")
        )));
        assert!(!note.contains("FAKE"), "{note}");
        assert!(!note.contains('\n'), "{note:?}");
    }

    /// **A phone's unsubscribe is refused, and takes nobody off the head.** Only the
    /// keyboard's unsubscribe of the head means the keyboard has left it.
    #[test]
    fn a_phone_unsubscribe_is_refused_and_moves_nothing() {
        let threads = bound_session("01a0-head");
        let unsub = r#"{"method":"thread/unsubscribe","id":3,"params":{"threadId":"01a0-head"}}"#;
        // The allowlist refuses it on this leg — `thread/unsubscribe` has no phone cell —
        // and only the keyboard's unsubscribe takes the head off the phone.
        assert_eq!(refused_code(&go_env(&threads, unsub)), E_POLICY_REFUSED);
        assert_eq!(threads.bound_thread().as_deref(), Some("01a0-head"));
    }

    /// **The phone steers the turn this session is running, and no other.**
    ///
    /// The extra rule the ccd leg carries, and why it is not tidiness. MEASURED on
    /// 0.153.4: the app-server answers a stale `expectedTurnId` with
    /// `-32600 "expected active turn id X but found Y"` — it hands the caller the REAL
    /// running turn's id. A phone is not looking at the pane and has no business learning
    /// that from a refusal, so a stale steer is answered here, with zero bytes upstream,
    /// by the same `is_active_turn` predicate that binds the stop control.
    ///
    /// A mutation that widens this arm to a plain forward turns every refused row below
    /// green.
    #[test]
    fn the_phone_steers_only_the_turn_this_session_is_running() {
        let threads = bound_session("01a0-head");
        let seq = std::cell::Cell::new(0u32);
        let from_the_phone = |params: serde_json::Value| {
            seq.set(seq.get() + 1);
            go_env(
                &threads,
                &json!({"method":"turn/steer","id":format!("s-{}", seq.get()),
                        "params":params})
                .to_string(),
            )
        };

        // Nothing is running, so there is nothing to steer. The server would have said
        // "no active turn to steer"; this says it without spending a byte.
        assert_eq!(
            refused_code(&from_the_phone(steer_params("01a0-head", "01a0-turn"))),
            E_POLICY_REFUSED
        );

        // A turn the TUI started and the server answered.
        keyboard_turn(&threads, "01a0-head", "01a0-turn");
        assert!(
            matches!(
                from_the_phone(steer_params("01a0-head", "01a0-turn")),
                RelayAction::Forward { .. }
            ),
            "the phone must be able to steer the turn this session is running"
        );

        // A turn that is not the running one, and a thread this session is not on.
        for params in [
            steer_params("01a0-head", "01a0-some-other-turn"),
            steer_params("01a0-a-stranger", "01a0-turn"),
        ] {
            assert_eq!(
                refused_code(&from_the_phone(params.clone())),
                E_POLICY_REFUSED,
                "{params}"
            );
        }

        // **The refusal names no turn.** The whole reason this gate is here is that the
        // server's own answer would have named the running one.
        let stale = from_the_phone(steer_params("01a0-head", "01a0-some-other-turn"));
        assert!(
            !refused_message(&stale).contains("01a0-turn"),
            "a refusal that reaches the phone must not carry an id it did not send: {}",
            refused_message(&stale)
        );

        // After the terminal there is nothing to steer again.
        threads.observe_server_frame(
            CONN_A,
            &json!({"method":"turn/completed",
                    "params":{"threadId":"01a0-head","turn":{"id":"01a0-turn"}}})
            .to_string(),
        );
        let ended = from_the_phone(steer_params("01a0-head", "01a0-turn"));
        assert_eq!(refused_code(&ended), E_POLICY_REFUSED);
        assert!(
            matches!(ended, RelayAction::SyntheticError { .. }),
            "every refusal above is composed here; the upstream socket is never written"
        );
    }

    /// **A phone may steer with TEXT and nothing else.**
    ///
    /// The `UserInput` union the schema pins also carries `localImage`, `localAudio`,
    /// `skill` and `mention` — each naming an absolute filesystem PATH the app-server
    /// opens itself, outside the model's sandbox — and `image`/`audio`, which name a URL.
    /// A phone that could send those would have a read-and-exfiltrate primitive no sandbox
    /// policy fences: it does not run in the workspace and cannot otherwise read a byte of
    /// it.
    #[test]
    fn the_phone_may_steer_with_text_and_nothing_else() {
        let threads = bound_session("01a0-head");
        keyboard_turn(&threads, "01a0-head", "01a0-turn");

        let with_input = |id: &str, input: serde_json::Value| {
            let mut params = steer_params("01a0-head", "01a0-turn");
            params["input"] = input;
            go_env(
                &threads,
                &json!({"method":"turn/steer","id":id,"params":params}).to_string(),
            )
        };

        for (n, item) in [
            json!({"type":"localImage","path":"/Users/someone/.ssh/id_rsa"}),
            json!({"type":"localAudio","path":"/work/proj/.env"}),
            json!({"type":"skill","name":"x","path":"/work/proj/.env"}),
            json!({"type":"mention","name":"x","path":"/work/proj/.env"}),
            json!({"type":"image","url":"https://example.invalid/x.png"}),
            json!({"type":"audio","url":"https://example.invalid/x.wav"}),
        ]
        .into_iter()
        .enumerate()
        {
            let a = with_input(&format!("i-{n}"), json!([item.clone()]));
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "{item}");
        }

        // **A `text` item carrying an extra key is refused too**, which is the difference
        // between reading `item["type"]` and reading the item. The union's text arm is
        // `{type, text, text_elements}`; anything beside them is a key this build has
        // never measured, and the threat this module is written against is a future
        // release giving one a meaning.
        for odd in [
            json!({"type":"text","text":"hi","text_elements":[],"path":"/Users/someone/.ssh/id_rsa"}),
            json!({"type":"text","text":"hi","url":"https://example.invalid/x"}),
            json!({"type":"text","text_elements":[]}),
            json!({"type":"text","text":42}),
            json!("just a string"),
        ] {
            let a = with_input("odd", json!([odd.clone()]));
            assert_eq!(refused_code(&a), E_POLICY_REFUSED, "{odd}");
        }
        // An EMPTY input composes nothing, and is answered here rather than upstream —
        // where the app-server's own sentence is about the turn, not about the frame.
        assert_eq!(
            refused_code(&with_input("empty", json!([]))),
            E_POLICY_REFUSED
        );

        // A text item mixed in with a path item is still refused: the rule is over every
        // item, not over the first one.
        let mixed = with_input(
            "mixed",
            json!([
                {"type":"text","text":"look at this","text_elements":[]},
                {"type":"localImage","path":"/Users/someone/.ssh/id_rsa"}
            ]),
        );
        assert_eq!(refused_code(&mixed), E_POLICY_REFUSED);

        // And the shape the phone actually sends forwards.
        assert!(matches!(
            with_input(
                "ok",
                json!([{"type":"text","text":"stop and summarise","text_elements":[]}])
            ),
            RelayAction::Forward { .. }
        ));
    }

    /// **A phone write cannot land on a thread the keyboard is leaving.**
    ///
    /// While the keyboard's move is in flight, and once it has unsubscribed from the head,
    /// there is no head: the phone's start and steer both refuse, and the start says why.
    #[test]
    fn a_phone_write_cannot_land_on_a_thread_the_keyboard_is_leaving() {
        let threads = bound_session("01a0-head");
        keyboard_turn(&threads, "01a0-head", "01a0-turn");
        threads.observe_server_frame(
            CONN_A,
            &json!({"method":"turn/completed",
                    "params":{"threadId":"01a0-head","turn":{"id":"01a0-turn"}}})
            .to_string(),
        );
        for leaving in [
            r#"{"method":"thread/unsubscribe","id":"u-1","params":{"threadId":"01a0-head"}}"#,
            r#"{"method":"thread/start","id":"s-2","params":{}}"#,
        ] {
            keyboard(&threads, leaving);
            let refused_start = go_env(&threads, &phone_turn("01a0-head"));
            assert_eq!(refused_code(&refused_start), E_POLICY_REFUSED, "{leaving}");
            assert!(
                refused_message(&refused_start).contains("switch is in progress"),
                "{}",
                refused_message(&refused_start)
            );
            let refused_steer = go_env(&threads, &steer("01a0-head", "01a0-turn"));
            assert_eq!(refused_code(&refused_steer), E_POLICY_REFUSED, "{leaving}");
        }
    }

    /// **The session's stop control is bound to the turn it is running, and to nothing
    /// else.**
    ///
    /// Refusing every interrupt was not a safe terminal state — it left a session with no
    /// way out of a hung command or work the user needed to stop. Admitting one is only
    /// safe if it can reach exactly one turn: this session's running one.
    #[test]
    fn the_interrupt_is_bound_to_the_running_turn() {
        let threads = bound_session("01a0-head");
        let seq = std::cell::Cell::new(0u32);
        let drive = |params: serde_json::Value| {
            seq.set(seq.get() + 1);
            go_env(
                &threads,
                &json!({"method":"turn/interrupt","id":format!("i-{}", seq.get()),
                        "params":params})
                .to_string(),
            )
        };

        // No turn has been admitted yet, so there is nothing running to interrupt.
        assert_eq!(
            refused_code(&drive(json!({"threadId":"01a0-head","turnId":"01a0-turn"}))),
            E_POLICY_REFUSED
        );

        // Run a turn and let the server answer it: NOW there is an active turn.
        keyboard_turn(&threads, "01a0-head", "01a0-turn");
        assert!(matches!(
            drive(json!({"threadId":"01a0-head","turnId":"01a0-turn"})),
            RelayAction::Forward { .. }
        ));

        // A turn that is not the running one, and a thread that is not this session's.
        for params in [
            json!({"threadId":"01a0-head","turnId":"01a0-some-other-turn"}),
            json!({"threadId":"01a0-a-stranger","turnId":"01a0-turn"}),
        ] {
            assert_eq!(refused_code(&drive(params)), E_POLICY_REFUSED);
        }
        // The params are pinned to the capture: exactly the two keys, both strings.
        for params in [
            json!({"threadId":"01a0-head"}),
            json!({"turnId":"01a0-turn"}),
            json!({"threadId":"01a0-head","turnId":"01a0-turn","force":true}),
            json!({"threadId":"01a0-head","turnId":{"id":"01a0-turn"}}),
            json!(["01a0-head", "01a0-turn"]),
        ] {
            assert_eq!(
                refused_code(&drive(params.clone())),
                E_POLICY_REFUSED,
                "{params}"
            );
        }
        // …and the phone reaches the same turn under the same rule. The ccd leg's
        // own binding is proven from a clean session below; this one line is here so
        // that a reader of the TUI rule sees the two legs answer alike.
        assert!(matches!(
            go_env(
                &threads,
                &json!({"method":"turn/interrupt","id":"ccd-1",
                        "params":{"threadId":"01a0-head","turnId":"01a0-turn"}})
                .to_string()
            ),
            RelayAction::Forward { .. }
        ));
    }

    /// **A refused interrupt's detail says how MANY keys arrived, never what they were
    /// called.**
    ///
    /// The key names in a `turn/interrupt` are attacker-chosen bytes and the detail is
    /// written to the durable `broker.log` an operator reads. A client that names its
    /// extra key after a credential puts that string in the log verbatim; a client that
    /// names it with four kilobytes puts four kilobytes there. Neither is information the
    /// refusal needs — the SHAPE (how many keys, how many of the two required ones are
    /// missing, how many are unexpected) is what tells an operator what happened.
    #[test]
    fn an_interrupts_refusal_detail_carries_the_key_shape_and_no_key_name() {
        let threads = bound_session("01a0-head");
        const SENTINEL: &str = "sk-live-SENTINEL-DO-NOT-LOG";
        let four_kilobyte_name = "n".repeat(4096);
        let seq = std::cell::Cell::new(0u32);

        for hostile in [SENTINEL, four_kilobyte_name.as_str()] {
            // Both places a hostile name can sit: alongside the measured pair, and
            // instead of it.
            for measured_pair_present in [true, false] {
                let mut params = serde_json::Map::new();
                if measured_pair_present {
                    params.insert("threadId".into(), json!("01a0-head"));
                    params.insert("turnId".into(), json!("01a0-turn"));
                }
                params.insert(hostile.to_string(), json!(1));
                seq.set(seq.get() + 1);
                let refused = go_env(
                    &threads,
                    &json!({"method":"turn/interrupt","id":format!("r-{}", seq.get()),
                            "params":serde_json::Value::Object(params)})
                    .to_string(),
                );

                assert_eq!(
                    refused_code(&refused),
                    E_POLICY_REFUSED,
                    "the refusal decision is unchanged: an unmeasured key set is refused"
                );
                let note = refused_note(&refused);
                assert!(
                    !note.contains(hostile),
                    "a client-chosen key name reached the durable log: {note}"
                );
                assert!(
                    note.len() <= 240,
                    "the detail must be bounded by its own vocabulary, not by the \
                     client's params; it was {} bytes",
                    note.len()
                );
                // Not vacuously green: the detail still tells the operator the shape.
                assert!(
                    note.contains(if measured_pair_present { "3" } else { "1" }),
                    "the detail must still report how many keys arrived: {note}"
                );
            }
        }
    }

    /// **An interrupt may name this session's ONE active thread, and no other — including
    /// a thread this session merely used to be on.**
    ///
    /// A retired thread is readable for ever, so "is it a session thread" is the wrong
    /// question to ask of an actuation: it says yes to every thread the session has ever
    /// bound. The stop control is scoped to the SOLE session thread instead, so a frame
    /// pairing a thread the session has left with the id of the turn it is currently
    /// running is refused on the thread alone — before the turn is even considered.
    #[test]
    fn an_interrupt_naming_a_thread_this_session_no_longer_solely_owns_is_refused() {
        let threads = bound_session("01a0-head");
        // Switch: the TUI unsubscribes from the head and creates a second thread, so
        // `01a0-head` retires and `01a0-next` becomes the session's one active thread.
        keyboard(
            &threads,
            r#"{"method":"thread/unsubscribe","id":7,"params":{"threadId":"01a0-head"}}"#,
        );
        keyboard_switch(&threads, "01a0-next");
        // A turn runs on the new head, and the server answers it.
        keyboard_turn(&threads, "01a0-next", "01a0-next-turn");

        let seq = std::cell::Cell::new(0u32);
        let drive = |thread: &str, turn: &str| {
            seq.set(seq.get() + 1);
            go_env(
                &threads,
                &json!({"method":"turn/interrupt","id":format!("x-{}", seq.get()),
                        "params":{"threadId":thread,"turnId":turn}})
                .to_string(),
            )
        };

        // The retired thread paired with the LIVE turn's id — the pairing that reaches
        // furthest, because every other half of it is genuine.
        assert_eq!(
            refused_code(&drive("01a0-head", "01a0-next-turn")),
            E_POLICY_REFUSED,
            "a thread this session has left is readable, never actuable"
        );
        // A thread this session never bound at all.
        assert_eq!(
            refused_code(&drive("01a0-a-stranger", "01a0-next-turn")),
            E_POLICY_REFUSED
        );
        // And the one pair that names the session's own running turn still forwards, so
        // the refusals above are not vacuous.
        assert!(matches!(
            drive("01a0-next", "01a0-next-turn"),
            RelayAction::Forward { .. }
        ));
    }

    /// **The phone may stop the turn this session is running, and may reach no other
    /// turn and no other thread.**
    ///
    /// The observing leg holds one actuation, and this is the whole of what it can
    /// do. Every other shape is answered here, locally, and **zero bytes go
    /// upstream** — which is not a tidiness claim but the thing that keeps a
    /// measured hang unreachable: a real app-server answers an interrupt naming a
    /// turn that has already ended with nothing at all, for ever, so a frame this
    /// gate lets through is a caller waiting on an answer that is never coming.
    /// `RelayAction::SyntheticError` is the proof of the zero, because a synthetic
    /// error is composed in this process and the upstream socket is never written.
    ///
    /// **Mutation:** widen the new `ccd_request` arm from `InterruptActiveTurn` to
    /// `Forward` and every row below turns into a forward — the stale turn, the
    /// stranger's thread and the retired thread's turn all reach the app-server,
    /// and the stale one is the frame that never comes back.
    #[test]
    fn the_phone_stops_the_running_turn_and_reaches_no_other() {
        let threads = bound_session("01a0-head");
        let seq = std::cell::Cell::new(0u32);
        let from_the_phone = |params: serde_json::Value| {
            seq.set(seq.get() + 1);
            go_env(
                &threads,
                &json!({"method":"turn/interrupt","id":format!("p-{}", seq.get()),
                        "params":params})
                .to_string(),
            )
        };
        // Nothing is running yet, so there is nothing for a phone to stop.
        let idle = from_the_phone(json!({"threadId":"01a0-head","turnId":"01a0-turn"}));
        assert_eq!(refused_code(&idle), E_POLICY_REFUSED);

        // A turn the TUI started and the server answered: now one is running.
        keyboard_turn(&threads, "01a0-head", "01a0-turn");
        assert!(
            matches!(
                from_the_phone(json!({"threadId":"01a0-head","turnId":"01a0-turn"})),
                RelayAction::Forward { .. }
            ),
            "the phone must be able to stop the turn this session is running"
        );

        // A turn this session is not running, and a thread that is not this
        // session's. Both are answered here.
        for params in [
            json!({"threadId":"01a0-head","turnId":"01a0-some-other-turn"}),
            json!({"threadId":"01a0-a-stranger","turnId":"01a0-turn"}),
        ] {
            let refused = from_the_phone(params.clone());
            assert_eq!(refused_code(&refused), E_POLICY_REFUSED, "{params}");
        }

        // The same closed param shape the TUI leg is held to: exactly two string
        // keys, and a phone gets no wider frame than the terminal does.
        for params in [
            json!({"threadId":"01a0-head"}),
            json!({"turnId":"01a0-turn"}),
            json!({"threadId":"01a0-head","turnId":"01a0-turn","force":true}),
            json!({"threadId":"01a0-head","turnId":{"id":"01a0-turn"}}),
            json!(["01a0-head", "01a0-turn"]),
        ] {
            assert_eq!(
                refused_code(&from_the_phone(params.clone())),
                E_POLICY_REFUSED,
                "{params}"
            );
        }

        // **The turn ends, and the id that worked a moment ago stops working.** This
        // is the replayed-interrupt shape — a phone re-sending an ask whose turn is
        // already over — and it is the one the app-server never answers.
        threads.observe_server_frame(
            CONN_A,
            &json!({"method":"turn/completed",
                    "params":{"threadId":"01a0-head",
                              "turn":{"id":"01a0-turn","status":"interrupted"}}})
            .to_string(),
        );
        let stale = from_the_phone(json!({"threadId":"01a0-head","turnId":"01a0-turn"}));
        assert_eq!(
            refused_code(&stale),
            E_POLICY_REFUSED,
            "an interrupt naming a turn that has ended must be answered here, because              the app-server answers it nowhere"
        );

        // **And a retired thread's turn, after the head has moved.** The session
        // switches to a second thread and runs a turn on it; the old thread's turn id
        // is then a live-looking id belonging to a visit the session has left.
        keyboard(
            &threads,
            r#"{"method":"thread/unsubscribe","id":7,"params":{"threadId":"01a0-head"}}"#,
        );
        keyboard_switch(&threads, "01a0-next");
        keyboard_turn(&threads, "01a0-next", "01a0-next-turn");
        let retired = from_the_phone(json!({"threadId":"01a0-head","turnId":"01a0-turn"}));
        assert_eq!(
            refused_code(&retired),
            E_POLICY_REFUSED,
            "a turn on the thread this session has LEFT is not the turn it is running"
        );
        // The phone follows the session, though: the new head's turn is stoppable.
        assert!(matches!(
            from_the_phone(json!({"threadId":"01a0-next","turnId":"01a0-next-turn"})),
            RelayAction::Forward { .. }
        ));

        // **Every refusal above was answered in this process.** Restated as one
        // count rather than left implied by `refused_code`'s panic: a
        // `SyntheticError` is composed locally and the upstream socket is not
        // written, so this is the zero-bytes claim in the form a reader can check.
        for params in [
            json!({"threadId":"01a0-head","turnId":"01a0-turn"}),
            json!({"threadId":"01a0-a-stranger","turnId":"01a0-next-turn"}),
        ] {
            assert!(
                matches!(
                    from_the_phone(params.clone()),
                    RelayAction::SyntheticError { .. }
                ),
                "{params} must be answered locally, never forwarded"
            );
        }
    }

    /// **A turn the broker never admitted is still a running turn: the server said so.**
    ///
    /// The keyboard's frames will reach the app-server without this broker admitting
    /// them, so the only evidence of such a turn is the server's own `turn/started`, which
    /// reaches every subscribed leg. The frames are captured (codex 0.153,
    /// `fixtures/codex/interrupt-0.153.jsonl`): the thread reads busy from that
    /// notification alone, its turn is interruptible from the phone, a phone `turn/start`
    /// is refused rather than joining it, and its `turn/completed` makes the thread idle.
    #[test]
    fn a_turn_the_server_announces_is_running_until_the_server_completes_it() {
        let captured: Vec<serde_json::Value> =
            include_str!("../../../fixtures/codex/interrupt-0.153.jsonl")
                .lines()
                .map(|l| serde_json::from_str(l).expect("the capture parses"))
                .collect();
        let frame = |method: &str| -> serde_json::Value {
            captured
                .iter()
                .find(|l| l["frame"]["method"] == method)
                .unwrap_or_else(|| panic!("the capture holds a {method}"))["frame"]
                .clone()
        };
        let started = frame("turn/started");
        let completed = frame("turn/completed");
        let thread = started["params"]["threadId"].as_str().expect("threadId");
        let turn_id = started["params"]["turn"]["id"].as_str().expect("turn id");
        assert_eq!(completed["params"]["turn"]["id"].as_str(), Some(turn_id));

        let threads = bound_session(thread);
        threads.observe_server_frame(CONN_B, &started.to_string());

        assert!(threads.is_active_turn(thread, turn_id));
        let interrupt = json!({"method":"turn/interrupt","id":"ccd-i",
                               "params":{"threadId":thread,"turnId":turn_id}})
        .to_string();
        assert!(matches!(
            go_env(&threads, &interrupt),
            RelayAction::Forward { .. }
        ));
        let busy = go_env(&threads, &phone_turn(thread));
        assert!(
            matches!(busy, RelayAction::SyntheticError { .. }),
            "{busy:?}"
        );
        assert_eq!(
            refused_message(&busy),
            "turn refused: this session is already running a turn"
        );

        threads.observe_server_frame(CONN_B, &completed.to_string());
        assert!(!threads.is_active_turn(thread, turn_id));
        assert!(matches!(
            go_env(&threads, &phone_turn(thread)),
            RelayAction::Forward { .. }
        ));
    }
}
