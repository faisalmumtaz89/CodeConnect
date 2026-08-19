//! Exact (role × JSON-RPC-kind × method) disposition matrix (A4, finding 7).
//!
//! `schema-0.147/disposition-matrix.tsv` is a checked-in expected matrix — an
//! **independent restatement** of the allowlist rules — with one row per pinned method
//! per role (plus the one notification per role). The test asserts:
//!
//! 1. the matrix covers **exactly** the pinned census × both roles (no missing / stale /
//!    misspelled / newly-unlisted entry — on BOTH roles, not just TUI);
//! 2. the live `disposition()` equals the expected value for every row;
//! 3. the four bypass methods and the broad `command/*`/`process/*` families never
//!    Forward on either leg.
//!
//! A drift in either direction (a new/renamed Codex method, or a table change) fails.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use codex_broker::allowlist::{
    disposition, Disposition, JsonRpcKind, RefuseReason, BYPASS_METHODS,
};
use codex_broker::Role;
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
        m.insert(
            (
                cols[0].to_string(),
                cols[1].to_string(),
                cols[2].to_string(),
            ),
            cols[3].to_string(),
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
        let actual = disposition(role(&r), kind(&k), &method);
        assert_eq!(
            format!("{actual:?}"),
            expected,
            "disposition drift for {r}/{k}/{method}",
        );
    }
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
