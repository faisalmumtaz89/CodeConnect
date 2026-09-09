//! Exact (role × JSON-RPC-kind × method) disposition matrix (A4, finding 7).
//!
//! `schema-0.147/disposition-matrix.tsv` is a checked-in expected matrix — an
//! **independent restatement** of the allowlist rules — with one row per pinned method
//! per role (plus the one notification per role). The test asserts:
//!
//! 1. the matrix covers **exactly** the pinned census × both roles (no missing / stale /
//!    misspelled / newly-unlisted entry — on BOTH roles, not just TUI);
//! 2. the live `effective_disposition()` equals the expected value for every row;
//! 3. the four bypass methods and the broad `command/*`/`process/*` families never
//!    Forward on either leg;
//! 4. every row where `effective_disposition()` and the raw `disposition()` cell disagree
//!    — the executor's unconditional refusals — is checked against the REAL classifier.
//!
//! A drift in either direction (a new/renamed Codex method, or a table change) fails.
//!
//! ## Why the matrix states the *effective* disposition, and why (4) exists
//!
//! It used to state the raw `disposition()` cell, and for one row that overstated the
//! boundary the file exists to state: `Tui/Request/thread/fork` read `FingerprintAssert`
//! while the executor refused every fork frame outright. The cell is deliberately
//! non-refusing — `guarded_surface::is_guarded_as` reads the same function to decide which
//! methods the launch gate pins the wire shape of, and a refusing cell drops a method out of
//! that projection (see `allowlist::executor_refusal`) — so the runtime is right and the file
//! was wrong.
//!
//! Restating `effective_disposition()` fixes the claim. It does not, on its own, PIN it:
//! `effective_disposition` and the executor could both be edited to stop refusing and this
//! file would follow them down. (4) is what closes that: it builds a real request frame and
//! runs the real `classify()` over it, so deleting the executor's branch turns this gate red
//! even though nothing in the matrix moved.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use codex_broker::allowlist::{
    disposition, effective_disposition, Disposition, JsonRpcKind, RefuseReason, BYPASS_METHODS,
};
use codex_broker::fingerprint::LaunchFingerprint;
use codex_broker::refusal::{E_METHOD_UNAVAILABLE, E_POLICY_REFUSED};
use codex_broker::response_capability::NoCapabilities;
use codex_broker::session::{ConnId, NoThreads};
use codex_broker::{classify, Env, RelayAction, Role, WsPayload};
use serde_json::Value;

fn manifest(sub: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(sub)
}

fn load_methods(bundle: &str) -> (Vec<String>, Vec<String>) {
    let v: Value = serde_json::from_str(
        &fs::read_to_string(manifest(&format!("schema-0.147/methods-{bundle}.json"))).unwrap(),
    )
    .unwrap();
    let arr = |k: &str| {
        v[k].as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    (arr("client_requests"), arr("client_notifications"))
}

fn census() -> (BTreeSet<String>, BTreeSet<String>) {
    let (sr, sn) = load_methods("stable");
    let (er, en) = load_methods("experimental");
    assert_eq!(sr.len(), 95, "stable client-request count drifted");
    assert_eq!(er.len(), 133, "experimental client-request count drifted");
    assert_eq!(sn, ["initialized"]);
    assert_eq!(en, ["initialized"]);
    let reqs = sr.into_iter().chain(er).collect();
    let nots = sn.into_iter().chain(en).collect();
    (reqs, nots)
}

fn role(s: &str) -> Role {
    match s {
        "Tui" => Role::Tui,
        "Ccd" => Role::Ccd,
        other => panic!("bad role {other}"),
    }
}

fn kind(s: &str) -> JsonRpcKind {
    match s {
        "Request" => JsonRpcKind::Request,
        "Notification" => JsonRpcKind::Notification,
        other => panic!("bad kind {other}"),
    }
}

/// (role, kind, method) -> expected disposition Debug string.
fn load_matrix() -> BTreeMap<(String, String, String), String> {
    let text = fs::read_to_string(manifest("schema-0.147/disposition-matrix.tsv")).unwrap();
    let mut m = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 4, "bad matrix row: {line}");
        let key = (
            cols[0].to_string(),
            cols[1].to_string(),
            cols[2].to_string(),
        );
        // **A duplicated row must not collapse into its last writer.** The matrix is
        // checked against the runtime key by key, so two rows for one key would let the
        // file contradict itself and still pass: the first disposition would be discarded
        // unread and the gate would report agreement with the survivor. `BTreeMap::insert`
        // hands back the displaced value precisely so this can be caught, and dropping it
        // was the whole failure.
        let displaced = m.insert(key.clone(), cols[3].to_string());
        assert!(
            displaced.is_none(),
            "schema-0.147/disposition-matrix.tsv lists {}/{}/{} twice \
             (first said {:?}, then {:?}); a duplicated key silently collapses to the last \
             row, so the matrix could contradict itself and this gate would never say",
            key.0,
            key.1,
            key.2,
            displaced.unwrap_or_default(),
            cols[3],
        );
    }
    m
}

#[test]
fn matrix_covers_every_pinned_method_in_both_kinds_on_both_roles() {
    let (reqs, nots) = census();
    let matrix = load_matrix();

    // Both kinds cover EVERY pinned method (requests ∪ notifications), so a
    // notification-shaped bypass/ownership/dangerous method has an explicit refuse row.
    let all_methods: BTreeSet<String> = reqs.union(&nots).cloned().collect();
    let mut expected_keys: BTreeSet<(String, String, String)> = BTreeSet::new();
    for r in ["Tui", "Ccd"] {
        for k in ["Request", "Notification"] {
            for m in &all_methods {
                expected_keys.insert((r.into(), k.into(), m.clone()));
            }
        }
    }
    let actual_keys: BTreeSet<_> = matrix.keys().cloned().collect();

    let missing: Vec<_> = expected_keys.difference(&actual_keys).collect();
    let extra: Vec<_> = actual_keys.difference(&expected_keys).collect();
    assert!(missing.is_empty(), "matrix missing rows: {missing:?}");
    assert!(extra.is_empty(), "matrix has stale rows: {extra:?}");
}

#[test]
fn live_disposition_equals_the_expected_matrix() {
    for ((r, k, method), expected) in load_matrix() {
        let actual = effective_disposition(role(&r), kind(&k), &method);
        assert_eq!(
            format!("{actual:?}"),
            expected,
            "disposition drift for {r}/{k}/{method}",
        );
    }
}

/// **The rows where the matrix does not simply restate a table cell, checked against the
/// thing itself.**
///
/// `effective_disposition` composes the allowlist cell with `allowlist::executor_refusal`,
/// and those two are just data: the matrix agreeing with them proves the file and the table
/// tell the same story, not that the story is true. For the handful of rows where the two
/// disagree — a non-refusing cell whose every frame the executor refuses anyway, so that the
/// launch gate keeps pinning the method's wire shape — the only honest pin is the classifier.
///
/// So this drives the real `classify()` with a real request frame and requires an actual
/// refusal carrying the exact code and message the matrix's reason names. Delete the
/// executor's branch and this goes red; leave the branch and delete the table entry and
/// `live_disposition_equals_the_expected_matrix` goes red instead. Neither half can be lost
/// quietly.
#[test]
fn every_executor_only_refusal_in_the_matrix_is_performed_by_the_real_classifier() {
    let fp = LaunchFingerprint {
        approval_policy: "untrusted".into(),
        approvals_reviewer: "user".into(),
        sandbox: "read-only".into(),
        hooks_enabled: true,
        launch_cwd: "/work/proj".into(),
    };
    let mut checked = 0usize;
    for ((r, k, method), expected) in load_matrix() {
        let cell = disposition(role(&r), kind(&k), &method);
        let effective = effective_disposition(role(&r), kind(&k), &method);
        if cell == effective {
            continue;
        }
        checked += 1;
        assert_eq!(k, "Request", "only requests carry an executor-only refusal");
        let Disposition::Refuse(reason) = effective else {
            panic!("{r}/{k}/{method}: an executor-only override must be a refusal");
        };
        assert_eq!(
            format!("{effective:?}"),
            expected,
            "the matrix must state the executor's refusal for {r}/{k}/{method}",
        );

        // A well-formed, fingerprint-matching frame for the method — the strongest case a
        // client can make. It must still come back as a synthetic error and zero upstream
        // bytes.
        let text = format!(
            r#"{{"method":"{method}","id":1,"params":{{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":"read-only"}}}}"#
        );
        let env = Env {
            fingerprint: &fp,
            capabilities: &NoCapabilities,
            threads: &NoThreads,
            conn: ConnId(1),
        };
        let action = classify(role(&r), &env, &WsPayload::Text(text));
        let RelayAction::SyntheticError { frame, .. } = &action else {
            panic!(
                "{r}/{k}/{method}: the matrix says {reason:?} but the classifier said {action:?}"
            );
        };
        // An independent restatement of `refusal::refuse_message`, which is private — the
        // same discipline as the matrix itself. Only the reasons an override can carry are
        // spelled out; a new one is a deliberate stop.
        let (code, message) = match reason {
            RefuseReason::Fingerprint => (E_POLICY_REFUSED, "request refused by session policy"),
            RefuseReason::Deferred => (
                E_METHOD_UNAVAILABLE,
                "method not available yet through the broker",
            ),
            other => panic!("{r}/{k}/{method}: no wire form restated here for {other:?}"),
        };
        let v: Value = serde_json::from_str(frame).unwrap();
        assert_eq!(
            v["error"]["code"], code,
            "{r}/{k}/{method}: wrong refusal code for {reason:?}",
        );
        assert_eq!(
            v["error"]["message"], message,
            "{r}/{k}/{method}: the wire message must be the one {reason:?} spells",
        );
    }
    assert_eq!(
        checked, 1,
        "exactly one row (Tui/Request/thread/fork) is an executor-only refusal today; a \
         change to that set is a boundary change and must be reviewed here",
    );
}

#[test]
fn bypass_and_exec_families_never_forward_in_either_kind() {
    let (reqs, nots) = census();
    let all: BTreeSet<String> = reqs.union(&nots).cloned().collect();
    for m in BYPASS_METHODS {
        assert!(all.contains(m), "bypass method {m} not pinned");
        for r in [Role::Tui, Role::Ccd] {
            // As a request: named CodeExecBypass.
            assert_eq!(
                disposition(r, JsonRpcKind::Request, m),
                Disposition::Refuse(RefuseReason::CodeExecBypass),
                "{m} request on {r:?}",
            );
            // As a notification: refused (never forwarded).
            assert!(
                matches!(
                    disposition(r, JsonRpcKind::Notification, m),
                    Disposition::Refuse(_)
                ),
                "{m} notification on {r:?} must refuse",
            );
        }
    }
    for method in &all {
        let dangerous = method.starts_with("command/")
            || method.starts_with("process/")
            || *method == "thread/shellCommand"
            || *method == "fs/writeFile";
        if !dangerous {
            continue;
        }
        for r in [Role::Tui, Role::Ccd] {
            for k in [JsonRpcKind::Request, JsonRpcKind::Notification] {
                assert!(
                    matches!(disposition(r, k, method), Disposition::Refuse(_)),
                    "{method} ({k:?}) on {r:?} must refuse",
                );
            }
        }
    }
}
