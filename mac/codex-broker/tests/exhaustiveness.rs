//! The phone leg's (JSON-RPC-kind × method) disposition table, restated.
//!
//! [`PHONE_TABLE`] is an **independent restatement** of every cell the phone leg's
//! allowlist names; every other method refuses as `NotAllowlisted`. The keyboard leg has no
//! table: its frames pass through. The tests assert:
//!
//! 1. the live `disposition()` equals the restated value for every named cell;
//! 2. every client method codex's schema declares ([`CLIENT_METHODS`], 0.147.0 and
//!    0.155.1) or the committed captures carry that the table does not name refuses, as a
//!    request and as a notification, and so does every named request method sent as a
//!    notification (only `initialized` forwards);
//! 3. the four bypass methods and the broad `command/*`/`process/*` families never
//!    Forward.
//!
//! A table change fails (1). A cell added to the allowlist without restating it here
//! fails (2) only when its method name is in [`CLIENT_METHODS`] or a capture: a method
//! the list does not name is not checked, so the list must follow codex.
//!
//! # Regenerating the method list for a new codex
//!
//! ```text
//! codex app-server generate-json-schema --out /tmp/s
//! codex app-server generate-json-schema --experimental --out /tmp/s-exp
//! ```
//!
//! then add to `tests/codex-client-methods.txt` every `properties.method.enum` value in
//! both bundles' `ClientRequest.json` and `ClientNotification.json`, keep the names
//! already there (older codex builds still run), sort, and update the count asserted in
//! `client_methods()`. Run it with a scratch `CODEX_HOME`; it starts no session.

use std::collections::BTreeSet;

use codex_broker::allowlist::{
    disposition, Disposition, JsonRpcKind, RefuseReason, BYPASS_METHODS,
};
use serde_json::Value;

/// `(kind, method, expected disposition as Debug)` for every cell the allowlist names.
const PHONE_TABLE: &[(JsonRpcKind, &str, &str)] = &[
    (
        JsonRpcKind::Request,
        "command/exec",
        "Refuse(CodeExecBypass)",
    ),
    (
        JsonRpcKind::Request,
        "fs/writeFile",
        "Refuse(CodeExecBypass)",
    ),
    (JsonRpcKind::Request, "initialize", "Forward"),
    (JsonRpcKind::Notification, "initialized", "Forward"),
    (
        JsonRpcKind::Request,
        "process/spawn",
        "Refuse(CodeExecBypass)",
    ),
    (
        JsonRpcKind::Request,
        "thread/fork",
        "Refuse(RoleNotPermitted)",
    ),
    (
        JsonRpcKind::Request,
        "thread/items/list",
        "ReadSessionThread",
    ),
    (JsonRpcKind::Request, "thread/loaded/list", "Forward"),
    (JsonRpcKind::Request, "thread/read", "ReadSessionThread"),
    (JsonRpcKind::Request, "thread/resume", "ResumeSessionThread"),
    (
        JsonRpcKind::Request,
        "thread/settings/update",
        "Refuse(OwnershipAdjacent)",
    ),
    (
        JsonRpcKind::Request,
        "thread/shellCommand",
        "Refuse(CodeExecBypass)",
    ),
    (
        JsonRpcKind::Request,
        "thread/start",
        "Refuse(RoleNotPermitted)",
    ),
    (
        JsonRpcKind::Request,
        "thread/turns/list",
        "ReadSessionThread",
    ),
    (
        JsonRpcKind::Request,
        "turn/interrupt",
        "InterruptActiveTurn",
    ),
    (JsonRpcKind::Request, "turn/start", "NullOwnedIdleTurn"),
    (JsonRpcKind::Request, "turn/steer", "SteerRunningTurn"),
];

/// Every client method name codex's schema declares, one per line; `#` lines are notes.
const CLIENT_METHODS: &str = include_str!("codex-client-methods.txt");

fn client_methods() -> BTreeSet<String> {
    let methods: BTreeSet<String> = CLIENT_METHODS
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    assert_eq!(methods.len(), 165, "the method list changed size");
    methods
}

/// Every method name the committed captures carry, in either direction of the wire.
fn captured_methods() -> BTreeSet<String> {
    const CAPTURES: [&str; 4] = [
        include_str!("../../../fixtures/codex/session-0.153.jsonl"),
        include_str!("../../../fixtures/codex/thread-switch.jsonl"),
        include_str!("../../../fixtures/codex/side-fork-0.153.4.jsonl"),
        include_str!("../../../fixtures/codex/approval-0.153.jsonl"),
    ];
    let mut methods = BTreeSet::new();
    for capture in CAPTURES {
        for line in capture.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).unwrap();
            if let Some(m) = v["frame"]["method"].as_str() {
                methods.insert(m.to_string());
            }
        }
    }
    assert!(methods.len() > 20, "the captures carried only {methods:?}");
    methods
}

#[test]
fn live_disposition_equals_the_restated_table() {
    let mut seen = BTreeSet::new();
    for (kind, method, expected) in PHONE_TABLE {
        assert!(
            seen.insert((format!("{kind:?}"), *method)),
            "the table lists {kind:?}/{method} twice"
        );
        assert_eq!(
            format!("{:?}", disposition(*kind, method)),
            *expected,
            "disposition drift for {kind:?}/{method}",
        );
    }
}

#[test]
fn every_method_the_table_does_not_name_refuses_in_both_kinds() {
    let named: BTreeSet<(String, &str)> = PHONE_TABLE
        .iter()
        .map(|(kind, method, _)| (format!("{kind:?}"), *method))
        .collect();
    let schema = client_methods();
    for (_, method, _) in PHONE_TABLE {
        assert!(
            schema.contains(*method),
            "{method} is not a codex client method"
        );
    }
    let mut methods = schema;
    methods.extend(captured_methods());
    methods.insert("a/method/no/codex/has".into());
    for method in &methods {
        for kind in [JsonRpcKind::Request, JsonRpcKind::Notification] {
            if named.contains(&(format!("{kind:?}"), method.as_str())) {
                continue;
            }
            assert_eq!(
                disposition(kind, method),
                Disposition::Refuse(RefuseReason::NotAllowlisted),
                "{kind:?}/{method} is not in the table, so it must refuse",
            );
        }
    }
}

#[test]
fn bypass_and_exec_families_never_forward_in_either_kind() {
    for m in BYPASS_METHODS {
        assert_eq!(
            disposition(JsonRpcKind::Request, m),
            Disposition::Refuse(RefuseReason::CodeExecBypass),
            "{m} request",
        );
        assert!(
            matches!(
                disposition(JsonRpcKind::Notification, m),
                Disposition::Refuse(_)
            ),
            "{m} notification must refuse",
        );
    }
    let mut all = client_methods();
    all.extend(captured_methods());
    all.extend(BYPASS_METHODS.iter().map(|m| m.to_string()));
    all.extend(["command/exec/write", "process/kill"].map(String::from));
    for method in &all {
        let dangerous = method.starts_with("command/")
            || method.starts_with("process/")
            || *method == "thread/shellCommand"
            || *method == "fs/writeFile";
        if !dangerous {
            continue;
        }
        for k in [JsonRpcKind::Request, JsonRpcKind::Notification] {
            assert!(
                matches!(disposition(k, method), Disposition::Refuse(_)),
                "{method} ({k:?}) must refuse",
            );
        }
    }
}
