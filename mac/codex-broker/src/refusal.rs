//! The refusal matrix (A4) and the pure security-core entrypoint [`classify`].
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
//! Connection-outcome policy by class: a losing-fanout response / expected auxiliary
//! close / policy-refused request/notification is a normal event — **log, keep the leg
//! open**; a shape that implies a hostile or broken client (malformed, binary, array)
//! → **close that leg**. (Unknown-**method spam** also closes a leg, but that needs a
//! per-leg counter and belongs to the failure-containment sub-chunk; a *single* unknown
//! method here is refused with an error and the leg is kept open — see the seam note.)

use serde_json::json;

use crate::allowlist::{disposition, Disposition, JsonRpcKind, RefuseReason, Role};
use crate::fingerprint::{assert_fingerprint, LaunchFingerprint};
use crate::message::{classify_shape, RequestId, Shape, WsPayload};
use crate::response_capability::ResponseCapabilityRegistry;
use crate::session::ThreadBinding;

/// The runtime policy environment the classifier reads: the immutable launch
/// fingerprint, the one-use response-capability registry (fanout seam), and the
/// session thread-binding oracle (resume target binding).
pub struct Env<'a> {
    pub fingerprint: &'a LaunchFingerprint,
    pub capabilities: &'a dyn ResponseCapabilityRegistry,
    pub threads: &'a dyn ThreadBinding,
}

/// JSON-RPC error code for a policy refusal (allowlist / fingerprint). Mirrors the
/// Phase-0 broker's `E_POLICY_REFUSED`.
pub const E_POLICY_REFUSED: i64 = -32001;
/// JSON-RPC error code for a method whose disposition machinery is deferred to the
/// switch/fanout sub-chunk (reported to the client as unavailable, not policy-refused).
pub const E_METHOD_UNAVAILABLE: i64 = -32601;

/// What the relay must do with a classified client→server message. The relay owns the
/// original bytes; `Forward` means "send those exact bytes upstream" (this sub-chunk
/// performs no injection, so a fingerprint-accepted message forwards unchanged).
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

/// The pure security core: classify one whole, reassembled client→server payload and
/// decide its fate. No I/O, no async — fully testable against captured frames.
pub fn classify(role: Role, env: &Env, payload: &WsPayload) -> RelayAction {
    decide(role, env, classify_shape(payload))
}

/// Decide the fate of an already-classified [`Shape`]. The relay parses each whole
/// message exactly once (multi-MB bodies are not reparsed) and calls this; unit tests
/// call [`classify`], which parses for them.
pub fn decide(role: Role, env: &Env, shape: Shape) -> RelayAction {
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
        Shape::Notification { method, .. } => classify_notification(role, &method),
        Shape::Request { method, id, obj } => classify_request(
            role,
            env,
            &method,
            id,
            obj.get("params").unwrap_or(&json!({})),
        ),
        Shape::Response { id, is_error, .. } => {
            classify_response(role, env.capabilities, &id, is_error)
        }
    }
}

fn classify_notification(role: Role, method: &str) -> RelayAction {
    match disposition(role, JsonRpcKind::Notification, method) {
        Disposition::Forward => RelayAction::Forward {
            note: "notification allowlisted",
        },
        // A refused notification has no reply channel; zero bytes, keep open. An unknown
        // notification lands here too (bypass prevented). Repeated ones would trip the
        // spam-close counter (seam: failure-containment sub-chunk).
        other => RelayAction::DropLogKeepOpen {
            note: format!("notification {method} refused ({other:?})"),
        },
    }
}

fn classify_request(
    role: Role,
    env: &Env,
    method: &str,
    id: Option<RequestId>,
    params: &serde_json::Value,
) -> RelayAction {
    // A request whose id is present but not schema-legal (null/float) is invalid; never
    // forward it, and it cannot be answered — zero bytes.
    if id.is_none() {
        return RelayAction::DropLogKeepOpen {
            note: format!("{method}: request has no usable id (schema-invalid); zero bytes"),
        };
    }

    match disposition(role, JsonRpcKind::Request, method) {
        Disposition::Forward => RelayAction::Forward {
            note: "request allowlisted",
        },
        Disposition::FingerprintAssert => {
            // `thread/resume` must target a thread bound to this session before its
            // (absence-benign) fingerprint is even considered.
            if method == "thread/resume" {
                if let Err(detail) = check_resume_binding(env, params) {
                    return refuse_request(
                        id,
                        E_POLICY_REFUSED,
                        "resume refused: target thread is not bound to this session",
                        detail,
                    );
                }
            }
            match assert_fingerprint(env.fingerprint, method, params) {
                Ok(()) => RelayAction::Forward {
                    note: "ownership request: fingerprint asserted",
                },
                Err(refusal) => refuse_request(
                    id,
                    E_POLICY_REFUSED,
                    "request refused by session policy",
                    format!(
                        "{method}: fingerprint refused ({:?}): {}",
                        refusal.kind, refusal.detail
                    ),
                ),
            }
        }
        // Composable: fingerprint-assert THEN head-check. The fingerprint is evaluated
        // (a conflict is a distinct policy refusal), but with the head-check executor
        // deferred a fingerprint-clean request still fails closed — never forwarded.
        Disposition::FingerprintThenHeadCheck => {
            match assert_fingerprint(env.fingerprint, method, params) {
                Ok(()) => refuse_request(
                    id,
                    E_METHOD_UNAVAILABLE,
                    "method not available yet through the broker",
                    format!("{method}: fingerprint ok but D2 head-check deferred (fail closed)"),
                ),
                Err(refusal) => refuse_request(
                    id,
                    E_POLICY_REFUSED,
                    "request refused by session policy",
                    format!(
                        "{method}: fingerprint refused ({:?}): {}",
                        refusal.kind, refusal.detail
                    ),
                ),
            }
        }
        Disposition::Refuse(reason) => {
            let (code, msg) = refuse_message(reason);
            refuse_request(id, code, msg, format!("{method}: refused ({reason:?})"))
        }
        // Deferred dispositions fail closed until the switch/fanout sub-chunk lands.
        Disposition::HoldSerialize | Disposition::HeadCheck | Disposition::ConsumeLocally => {
            refuse_request(
                id,
                E_METHOD_UNAVAILABLE,
                "method not available yet through the broker",
                format!("{method}: disposition deferred to switch sub-chunk"),
            )
        }
    }
}

/// A `thread/resume` may only target a thread bound to this session (finding 5).
fn check_resume_binding(env: &Env, params: &serde_json::Value) -> Result<(), String> {
    match params.get("threadId").and_then(|t| t.as_str()) {
        None => Err("resume without a string threadId".to_string()),
        Some(id) if env.threads.is_session_thread(id) => Ok(()),
        Some(id) => Err(format!("thread {id} was not observed as a session thread")),
    }
}

fn classify_response(
    role: Role,
    capabilities: &dyn ResponseCapabilityRegistry,
    id: &RequestId,
    is_error: bool,
) -> RelayAction {
    if capabilities.authorize(role, id, is_error) {
        RelayAction::Forward {
            note: "response consumed an authorized capability",
        }
    } else {
        // No live authorized capability: losing-fanout / unsolicited / stale — a normal
        // race. Zero bytes, no error (no schema-legal error form), keep the leg open.
        RelayAction::DropLogKeepOpen {
            note: format!("method-less response id={id:?} has no live capability"),
        }
    }
}

/// Refuse a request: a usable id gets a synthetic error frame; an unusable id gets a
/// silent zero-byte drop. Either way the leg stays open (a policy/unknown refusal is not
/// hostile on its own).
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
        RefuseReason::Fingerprint => (E_POLICY_REFUSED, "request refused by session policy"),
        RefuseReason::Malformed => (E_POLICY_REFUSED, "message refused"),
        RefuseReason::Deferred => (
            E_METHOD_UNAVAILABLE,
            "method not available yet through the broker",
        ),
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

    fn fp() -> LaunchFingerprint {
        LaunchFingerprint {
            approval_policy: "untrusted".into(),
            approvals_reviewer: "user".into(),
            sandbox: "read-only".into(),
            hooks_enabled: true,
        }
    }

    fn go(role: Role, text: &str) -> RelayAction {
        go_env(role, &NoThreads, text)
    }

    fn go_env(role: Role, threads: &dyn ThreadBinding, text: &str) -> RelayAction {
        let fp = fp();
        let env = Env {
            fingerprint: &fp,
            capabilities: &NoCapabilities,
            threads,
        };
        classify(role, &env, &WsPayload::Text(text.to_string()))
    }

    /// A full, fingerprint-matching thread/start params body (satisfies the presence rule).
    const OK_START: &str = r#"{"method":"thread/start","id":"s","params":{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":"read-only"}}"#;

    #[test]
    fn bypass_request_gets_synthetic_error_zero_bytes() {
        let a = go(
            Role::Tui,
            r#"{"method":"command/exec","id":5,"params":{"cmd":"rm -rf /"}}"#,
        );
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
        let a = go(
            Role::Tui,
            r#"{"method":"fs/writeFile","id":null,"params":{}}"#,
        );
        assert!(matches!(a, RelayAction::DropLogKeepOpen { .. }));
    }

    #[test]
    fn allowlisted_read_forwards() {
        assert!(matches!(
            go(Role::Tui, r#"{"method":"app/list","id":1,"params":{}}"#),
            RelayAction::Forward { .. }
        ));
    }

    #[test]
    fn ownership_request_matching_forwards() {
        assert!(matches!(
            go(Role::Tui, OK_START),
            RelayAction::Forward { .. }
        ));
    }

    #[test]
    fn ownership_conflict_synthetic_error() {
        let a = go(
            Role::Tui,
            r#"{"method":"turn/start","id":9,"params":{"approvalPolicy":"never","approvalsReviewer":"user","sandbox":"read-only"}}"#,
        );
        // turn/start with a conflicting policy -> fingerprint conflict -> policy error.
        match a {
            RelayAction::SyntheticError { frame, .. } => {
                let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
                assert_eq!(v["error"]["code"], E_POLICY_REFUSED);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn turn_start_fingerprint_clean_still_fails_closed_until_headcheck() {
        let a = go(
            Role::Tui,
            r#"{"method":"turn/start","id":11,"params":{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandboxPolicy":"read-only"}}"#,
        );
        match a {
            RelayAction::SyntheticError { frame, .. } => {
                let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
                assert_eq!(v["error"]["code"], E_METHOD_UNAVAILABLE);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn request_with_unusable_id_never_forwards() {
        // Even an allowlisted method with a null id is schema-invalid: zero bytes.
        let a = go(Role::Tui, r#"{"method":"app/list","id":null,"params":{}}"#);
        assert!(matches!(a, RelayAction::DropLogKeepOpen { .. }));
    }

    #[test]
    fn ccd_start_fork_turn_are_role_refused() {
        for m in ["thread/start", "thread/fork", "turn/start"] {
            let text = format!(
                r#"{{"method":"{m}","id":1,"params":{{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":"read-only"}}}}"#
            );
            let a = go(Role::Ccd, &text);
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
        let threads = SessionThreads::new();
        threads.note("01a0-session-thread");
        // Unknown thread -> refused.
        let bad = go_env(
            Role::Ccd,
            &threads,
            r#"{"method":"thread/resume","id":1,"params":{"threadId":"99-not-ours"}}"#,
        );
        assert!(matches!(bad, RelayAction::SyntheticError { .. }));
        // Session thread -> forwarded (ownership-free resume is absence-benign).
        let ok = go_env(
            Role::Ccd,
            &threads,
            r#"{"method":"thread/resume","id":2,"params":{"threadId":"01a0-session-thread"}}"#,
        );
        assert!(matches!(ok, RelayAction::Forward { .. }));
    }

    #[test]
    fn notification_refused_zero_bytes_keep_open() {
        let a = go(Role::Ccd, r#"{"method":"thread/started","params":{}}"#);
        assert!(matches!(a, RelayAction::DropLogKeepOpen { .. }));
    }

    #[test]
    fn notification_shaped_dangerous_methods_are_refused() {
        // A method-only frame (no id) naming a dangerous/ownership method is a
        // notification; it must forward zero bytes on both legs, never execute.
        for m in [
            "command/exec",
            "fs/writeFile",
            "process/spawn",
            "thread/start",
        ] {
            for role in [Role::Tui, Role::Ccd] {
                let text = format!(r#"{{"method":"{m}","params":{{}}}}"#);
                assert!(
                    matches!(go(role, &text), RelayAction::DropLogKeepOpen { .. }),
                    "{m} notification on {role:?}",
                );
            }
        }
    }

    #[test]
    fn initialized_notification_forwards() {
        assert!(matches!(
            go(Role::Tui, r#"{"method":"initialized","params":{}}"#),
            RelayAction::Forward { .. }
        ));
    }

    #[test]
    fn method_less_response_no_capability_drops_zero_bytes() {
        let a = go(Role::Ccd, r#"{"id":0,"result":{"decision":{"accept":{}}}}"#);
        assert!(matches!(a, RelayAction::DropLogKeepOpen { .. }));
    }

    #[test]
    fn array_binary_malformed_close_leg() {
        assert!(matches!(
            go(Role::Tui, "[]"),
            RelayAction::DropCloseLeg { .. }
        ));
        assert!(matches!(
            go(Role::Tui, "{"),
            RelayAction::DropCloseLeg { .. }
        ));
        let fp = fp();
        let env = Env {
            fingerprint: &fp,
            capabilities: &NoCapabilities,
            threads: &NoThreads,
        };
        assert!(matches!(
            classify(Role::Tui, &env, &WsPayload::Binary),
            RelayAction::DropCloseLeg { .. }
        ));
    }

    #[test]
    fn deferred_switch_marker_fails_closed() {
        let a = go(
            Role::Tui,
            r#"{"method":"thread/unsubscribe","id":3,"params":{}}"#,
        );
        match a {
            RelayAction::SyntheticError { frame, .. } => {
                let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
                assert_eq!(v["error"]["code"], E_METHOD_UNAVAILABLE);
            }
            other => panic!("{other:?}"),
        }
    }
}
