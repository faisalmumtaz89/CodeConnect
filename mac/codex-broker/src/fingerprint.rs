//! Ownership-field validation against the durable launch-policy fingerprint (A4).
//!
//! **Guiding principle (fail closed): categorically REJECT any ownership-adjacent thing
//! the broker cannot FULLY prove equals the launch fingerprint.** We do not try to
//! fingerprint every possible shape — anything unprovable is refused.
//!
//! The fingerprint — `approval_policy`, `approvals_reviewer`, sandbox mode, hook
//! enablement — is asserted on every ownership-carrying request: `thread/start`,
//! `thread/resume`, `thread/fork`, and `turn/start`. Validation is **key-scoped**: the
//! specific ownership keys are checked wherever they appear — a typed field, or anywhere
//! in the `config` map at any nesting — never "reject non-null config" (the real TUI
//! always sends a benign `config`).
//!
//! ## Categorical rejects (cannot be proven ⇒ refuse)
//!
//! * **`notify`** — a command hook that runs a command on turn completion, i.e. a direct
//!   code-exec ownership surface. We never launch Codex with an overridable `notify`, so
//!   its safe value is *unset*: **`notify` present in ANY form ⇒ refuse.**
//! * **`hooks`** — only a bare `hooks: <bool>` equal to the fingerprint is provable.
//!   Any other hooks-adjacent shape (a `hooks` table, a `hooks.*` subkey, `codex_hooks`,
//!   `features.codex_hooks`, a non-bool value) ⇒ refuse.
//! * **`sandbox` object** — a mode-only object is reduced to its `mode`; a sandbox object
//!   carrying ANY field beyond `mode` (writable roots, network, …) is unprovable against
//!   a mode-only fingerprint ⇒ refuse.
//!
//! ## Conflict, and absence
//!
//! **Any** present ownership value that disagrees with the fingerprint is refused, even
//! when a matching typed field would win on this server version (relying on
//! typed-beats-config precedence is fragile). **Absence is not "forward":** on a
//! policy-**setting** method (`thread/start`, `thread/fork`, `turn/start`) each of
//! `approval_policy`, `approvals_reviewer`, `sandbox` must be positively present and
//! matching — an omitted dimension would inherit a server default not proven to equal the
//! fingerprint (INTERCEPTION C11b/C11e: an absent `approval_policy` inherits `on-request`
//! and wrote a file to disk). `hooks`/`notify` absence is the safe state (the thread
//! inherits the launch's hook config; `notify` stays unset). `thread/resume` is exempt
//! from the absence rule (the ccd attach path emits no ownership; overrides on a loaded
//! thread are discarded) — but conflicts and the categorical rejects still apply, and its
//! target thread is bound to the session separately (see `crate::session`).
//!
//! The wire `config` is already structured JSON, so — unlike the argv `-c key=value`
//! path in `codeconnect::codex`, which parses the value as TOML — no TOML grammar is
//! needed here; the ownership keys are matched over the JSON `config` object directly.
//!
//! ## Presence vs conflict (decoy hardening)
//!
//! An ownership dimension's **presence** (which satisfies the policy-setting absence rule)
//! is proven ONLY by a **known-effective** path: the typed field (`params.<field>`) or the
//! effective config **root** key (`params.config.<key>` at the top level of `config`) — and
//! the config root is effective for presence ONLY on methods the bundled schema proves carry
//! a `config` param (see `method_carries_config`). A **conflict** is detected over the WHOLE
//! `config` tree — any matching leaf anywhere (nested objects AND array elements) with a
//! value ≠ fingerprint refuses. So a decoy like
//! `{"config":{"decoy":{"approval_policy":"untrusted", …}}}` can never satisfy presence
//! (the effective root/typed fields stay absent ⇒ Absent refuse), while a decoy that
//! *conflicts* still refuses.
//!
//! ## Dotted-key note (verified against the 0.147 schema; app-server not run)
//!
//! The wire `config` is a **free-form map** (`ThreadStart/Fork/ResumeParams.config` is
//! `type: object, additionalProperties: true` in the 0.147 schema), not a typed struct, so
//! it is *not* proven that codex leaves a literal dotted JSON key (`"hooks.on_turn"`) flat
//! rather than path-expanding it during its config merge — and the gated app-server was not
//! run to test it. We therefore **fail closed categorically**: ANY key containing `.`
//! anywhere under `params.config` (in nested objects or array elements) is refused as
//! Unprovable, regardless of dimension — a dotted key may path-expand onto an owned
//! dimension and cannot be proven either way. On real TUI traffic (which sends nested
//! objects, never dotted string keys) this never fires.

use serde_json::Value;

use crate::allowlist::RefuseReason;

/// The durable launch-policy fingerprint, recorded at launch. Wiring this to the real
/// launch policy is the coordinator's job (Phase 2c, deferred); the broker takes it as
/// a construction input so the security core is testable in isolation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchFingerprint {
    pub approval_policy: String,
    pub approvals_reviewer: String,
    /// The sandbox mode (e.g. `read-only`, `workspace-write`, `danger-full-access`).
    pub sandbox: String,
    /// Whether hooks are enabled for the session.
    pub hooks_enabled: bool,
}

/// Why the fingerprint assertion refused a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintRefusal {
    pub kind: FpRefuseKind,
    /// Human-readable detail for the audit log.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpRefuseKind {
    /// A present, comparable ownership value disagrees with the fingerprint.
    Conflict,
    /// A policy-setting method omits a required ownership dimension.
    Absent,
    /// An ownership value is present but cannot be fully proven to match the fingerprint
    /// (a categorical reject: `notify`, an unprovable hooks shape, a rich sandbox object,
    /// or a non-string policy value). Fail closed.
    Unprovable,
}

impl FingerprintRefusal {
    /// The audit-log refuse reason (always `Fingerprint`).
    pub fn reason(&self) -> RefuseReason {
        RefuseReason::Fingerprint
    }
}

fn refusal(kind: FpRefuseKind, detail: impl Into<String>) -> FingerprintRefusal {
    FingerprintRefusal {
        kind,
        detail: detail.into(),
    }
}

/// Assert the fingerprint over an ownership-carrying request's params.
///
/// `Ok(())` ⇒ the ownership fields are provably consistent (forward). `Err(_)` ⇒ refuse.
pub fn assert_fingerprint(
    fp: &LaunchFingerprint,
    method: &str,
    params: &Value,
) -> Result<(), FingerprintRefusal> {
    // 0) Categorically reject any dotted key anywhere under `params.config`. Legitimate TUI
    //    traffic sends nested objects, never dotted config keys; a dotted key can path-expand
    //    onto an owned dimension and cannot be proven either way. Fail closed for ALL
    //    dimensions (see the dotted-key note above).
    reject_dotted_config_keys(params)?;

    // 1) notify: categorical reject anywhere in params (typed field or any config
    //    nesting, including a dotted `notify.*` key — see the dotted-key note above).
    if owned_key_present_anywhere(params, &["notify"]) {
        return Err(refusal(
            FpRefuseKind::Unprovable,
            "notify present: a command hook whose ownership cannot be delegated",
        ));
    }

    // 2) hooks: only a bare bool equal to the fingerprint is provable.
    check_hooks(fp, params)?;

    let policy_setting = matches!(method, "thread/start" | "thread/fork" | "turn/start");
    // Whether a `config`-ROOT key may satisfy the presence rule for this method. Only
    // methods the schema proves carry a `config` param object qualify; otherwise `config`
    // is conflict-only (scanned for conflicts, never presence).
    let config_effective = method_carries_config(method);

    // 3) approval_policy, approvals_reviewer: string dimensions.
    check_string_dimension(
        fp,
        params,
        "approvalPolicy",
        &["approval_policy"],
        &fp.approval_policy,
        policy_setting,
        config_effective,
    )?;
    check_string_dimension(
        fp,
        params,
        "approvalsReviewer",
        &["approvals_reviewer"],
        &fp.approvals_reviewer,
        policy_setting,
        config_effective,
    )?;

    // 4) sandbox: string, or a mode-only object; a rich object is unprovable.
    check_sandbox(fp, params, policy_setting, config_effective)?;

    Ok(())
}

/// Check one string-valued ownership dimension.
///
/// **Presence** (the absence rule) is proven ONLY by a known-effective path — the typed
/// field or the effective config **root** key — never by an arbitrary nested/decoy leaf.
/// **Conflict** is detected over the WHOLE tree: any matching leaf anywhere with a value
/// that disagrees with the fingerprint is refused (a decoy that conflicts still refuses).
fn check_string_dimension(
    _fp: &LaunchFingerprint,
    params: &Value,
    typed_key: &str,
    config_leaves: &[&str],
    expected: &str,
    policy_setting: bool,
    config_effective: bool,
) -> Result<(), FingerprintRefusal> {
    let mut effective_present = false;
    for (path, v, effective) in
        collect_dimension(params, &[typed_key], config_leaves, config_effective)
    {
        match v.as_str() {
            None => {
                return Err(refusal(
                    FpRefuseKind::Unprovable,
                    format!("{path}: ownership value is not a string"),
                ))
            }
            Some(s) if normalize(s) != normalize(expected) => {
                return Err(refusal(
                    FpRefuseKind::Conflict,
                    format!("{path}: {s:?} but fingerprint is {expected:?}"),
                ))
            }
            Some(_) => {}
        }
        if effective {
            effective_present = true;
        }
    }

    if policy_setting && !effective_present {
        return Err(refusal(
            FpRefuseKind::Absent,
            format!(
                "{typed_key}: not asserted on a known-effective path (typed field or config root); \
                 a nested/decoy leaf does not satisfy presence"
            ),
        ));
    }
    Ok(())
}

/// Sandbox: a string mode, or an object whose ONLY key is `mode`. Any richer object is
/// unprovable against a mode-only fingerprint and is refused.
fn check_sandbox(
    fp: &LaunchFingerprint,
    params: &Value,
    policy_setting: bool,
    config_effective: bool,
) -> Result<(), FingerprintRefusal> {
    let mut effective_present = false;
    let values = collect_dimension(
        params,
        &["sandbox", "sandboxPolicy"],
        &["sandbox_mode", "sandbox", "sandbox_policy"],
        config_effective,
    );
    for (path, v, effective) in values {
        let token = match &v {
            Value::String(s) => s.clone(),
            Value::Object(map) => {
                // A sandbox object may only carry `mode`; anything else is a policy field
                // (writable roots, network, …) we cannot prove matches.
                let extra: Vec<&String> = map.keys().filter(|k| k.as_str() != "mode").collect();
                if !extra.is_empty() {
                    return Err(refusal(
                        FpRefuseKind::Unprovable,
                        format!("{path}: sandbox object carries unprovable fields {extra:?}"),
                    ));
                }
                match map.get("mode").and_then(|m| m.as_str()) {
                    Some(m) => m.to_string(),
                    None => {
                        return Err(refusal(
                            FpRefuseKind::Unprovable,
                            format!("{path}: sandbox object has no string mode"),
                        ))
                    }
                }
            }
            _ => {
                return Err(refusal(
                    FpRefuseKind::Unprovable,
                    format!("{path}: sandbox value is neither string nor object"),
                ))
            }
        };
        if normalize(&token) != normalize(&fp.sandbox) {
            return Err(refusal(
                FpRefuseKind::Conflict,
                format!(
                    "{path}: sandbox {token:?} but fingerprint is {:?}",
                    fp.sandbox
                ),
            ));
        }
        if effective {
            effective_present = true;
        }
    }

    if policy_setting && !effective_present {
        return Err(refusal(
            FpRefuseKind::Absent,
            "sandbox: not asserted on a known-effective path (typed field or config root)",
        ));
    }
    Ok(())
}

/// Hooks: only `hooks: <bool>` equal to the fingerprint is provable. Any other
/// hooks-adjacent shape at any nesting is a categorical reject.
fn check_hooks(fp: &LaunchFingerprint, node: &Value) -> Result<(), FingerprintRefusal> {
    match node {
        Value::Object(map) => {
            for (k, v) in map {
                let seg = first_segment(k);
                let hooks_adjacent = seg == "hooks" || seg == "codex_hooks";
                if hooks_adjacent {
                    // Only a bare `hooks` key (exact, no dot) with a matching bool is
                    // provable; everything else hooks-adjacent (a `hooks` table, a dotted
                    // `hooks.*`, or the `codex_hooks` alias) is a categorical reject.
                    if k == "hooks" {
                        match v.as_bool() {
                            Some(b) if b == fp.hooks_enabled => {}
                            Some(_) => {
                                return Err(refusal(
                                    FpRefuseKind::Conflict,
                                    format!(
                                        "hooks enablement {v} but fingerprint is {}",
                                        fp.hooks_enabled
                                    ),
                                ))
                            }
                            None => {
                                return Err(refusal(
                                    FpRefuseKind::Unprovable,
                                    "hooks present as a non-bool (a hook table cannot be proven)",
                                ))
                            }
                        }
                    } else {
                        return Err(refusal(
                            FpRefuseKind::Unprovable,
                            format!("{k}: hooks-adjacent (alias/dotted/table) cannot be proven"),
                        ));
                    }
                } else {
                    // Recurse to catch nested `features.hooks`, `features.codex_hooks`, etc.
                    check_hooks(fp, v)?;
                }
            }
            Ok(())
        }
        Value::Array(items) => {
            for it in items {
                check_hooks(fp, it)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Collect the values for one ownership dimension as `(path, value, effective)`.
///
/// * A **typed** field (`params.<typed_key>`) is effective.
/// * A config **root** key (a direct child `params.config.<leaf>`) is effective ONLY when
///   `config_effective` — i.e. the method's schema carries a `config` param; otherwise the
///   root key is conflict-only.
/// * A **nested** matching leaf (`config.<anything>.<leaf>`), including matches inside array
///   elements, is collected but marked non-effective — it can prove a CONFLICT but never
///   PRESENCE (a decoy subtree must not satisfy the absence rule).
///
/// Dotted config keys never reach here as matches: they are refused categorically upstream
/// (`reject_dotted_config_keys`).
fn collect_dimension(
    params: &Value,
    typed_keys: &[&str],
    config_leaves: &[&str],
    config_effective: bool,
) -> Vec<(String, Value, bool)> {
    let mut out = Vec::new();
    for tk in typed_keys {
        if let Some(v) = params.get(*tk) {
            out.push((format!("params.{tk}"), v.clone(), true));
        }
    }
    if let Some(cfg) = params.get("config") {
        walk_dim(cfg, "config", config_leaves, config_effective, &mut out);
    }
    out
}

/// Walk a `config` subtree collecting matching leaves. `root_effective` is true only at the
/// top level of a config-carrying method; nested objects and every array element recurse
/// with `false`, so they can prove a CONFLICT but never PRESENCE.
fn walk_dim(
    node: &Value,
    path: &str,
    leaves: &[&str],
    root_effective: bool,
    out: &mut Vec<(String, Value, bool)>,
) {
    match node {
        Value::Object(map) => {
            for (k, v) in map {
                let child = format!("{path}.{k}");
                if leaves.contains(&k.as_str()) {
                    out.push((child.clone(), v.clone(), root_effective));
                }
                walk_dim(v, &child, leaves, false, out);
            }
        }
        Value::Array(items) => {
            for (i, it) in items.iter().enumerate() {
                walk_dim(it, &format!("{path}[{i}]"), leaves, false, out);
            }
        }
        _ => {}
    }
}

/// Whether a method's params carry a top-level `config` object per the bundled 0.147 schema.
/// Only such methods let a `config`-ROOT key satisfy the presence rule; for every other
/// method `config` is conflict-only (scanned for conflicts, never presence).
///
/// The vendored `schema-0.147/methods-{stable,experimental}.json` census enumerates method
/// *names* only (not param schemas), so config carriage is fixed here as a small static
/// table rather than interpreted. `thread/start`, `thread/fork`, `thread/resume` carry
/// `config`; `turn/start` does NOT — so a `params.config.*` alias never satisfies presence
/// on `turn/start` (which would otherwise inherit an unproven server default). Any method
/// not listed defaults to `false` (config conflict-only): over-refusal is safe, under-refusal
/// is the bug.
fn method_carries_config(method: &str) -> bool {
    matches!(method, "thread/start" | "thread/fork" | "thread/resume")
}

/// Refuse if any key containing `.` appears anywhere under `params.config` (in nested
/// objects OR array elements). A dotted config key never appears in legitimate TUI traffic
/// (which sends nested objects) and may path-expand onto an owned dimension during codex's
/// config merge, so it cannot be proven either way. Fail closed for ALL dimensions.
fn reject_dotted_config_keys(params: &Value) -> Result<(), FingerprintRefusal> {
    if let Some(cfg) = params.get("config") {
        if let Some(path) = first_dotted_key(cfg, "config") {
            return Err(refusal(
                FpRefuseKind::Unprovable,
                format!(
                    "{path}: dotted config key — the TUI sends nested objects, never dotted \
                     keys; a dotted key may path-expand onto an owned dimension and cannot be \
                     proven"
                ),
            ));
        }
    }
    Ok(())
}

/// The path of the first object key containing `.` found anywhere in `node` (objects and
/// array elements), or `None`.
fn first_dotted_key(node: &Value, path: &str) -> Option<String> {
    match node {
        Value::Object(map) => {
            for (k, v) in map {
                if k.contains('.') {
                    return Some(format!("{path}.{k}"));
                }
                if let Some(p) = first_dotted_key(v, &format!("{path}.{k}")) {
                    return Some(p);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, it)| first_dotted_key(it, &format!("{path}[{i}]"))),
        _ => None,
    }
}

/// Does any object anywhere in `node` have a key equal to — or whose first dotted segment
/// equals — one of `names`? Used for the `notify` categorical reject; dotted-aware because
/// the wire `config` is a free-form map codex may path-expand (see the dotted-key note).
fn owned_key_present_anywhere(node: &Value, names: &[&str]) -> bool {
    match node {
        Value::Object(map) => {
            map.keys()
                .any(|k| names.contains(&k.as_str()) || names.contains(&first_segment(k)))
                || map.values().any(|v| owned_key_present_anywhere(v, names))
        }
        Value::Array(items) => items.iter().any(|v| owned_key_present_anywhere(v, names)),
        _ => false,
    }
}

/// The portion of a (possibly dotted) config key before the first `.`.
fn first_segment(key: &str) -> &str {
    key.split('.').next().unwrap_or(key)
}

/// Canonicalize an ownership token so `workspace-write`, `workspaceWrite` and
/// `workspace_write` compare equal: lowercase, drop `-`/`_`.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '-' || c == '_' {
            continue;
        }
        for lc in c.to_lowercase() {
            out.push(lc);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fp() -> LaunchFingerprint {
        LaunchFingerprint {
            approval_policy: "untrusted".into(),
            approvals_reviewer: "user".into(),
            sandbox: "read-only".into(),
            hooks_enabled: true,
        }
    }

    fn full_start(extra: Value) -> Value {
        // A policy-setting request that satisfies the presence rule, plus `extra`.
        let mut base = json!({
            "approvalPolicy": "untrusted",
            "approvalsReviewer": "user",
            "sandbox": "read-only"
        });
        if let Value::Object(e) = extra {
            for (k, v) in e {
                base.as_object_mut().unwrap().insert(k, v);
            }
        }
        base
    }

    #[test]
    fn matching_full_policy_passes() {
        let p = full_start(json!({"config": {"model_reasoning_effort": "high"}}));
        assert!(assert_fingerprint(&fp(), "thread/start", &p).is_ok());
    }

    #[test]
    fn typed_conflict_refuses() {
        let p =
            json!({"approvalPolicy": "never", "approvalsReviewer":"user", "sandbox": "read-only"});
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &p)
                .unwrap_err()
                .kind,
            FpRefuseKind::Conflict
        );
    }

    #[test]
    fn config_scoped_conflict_refuses_even_when_typed_would_win() {
        let p = full_start(json!({"config": {"approval_policy": "never"}}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &p)
                .unwrap_err()
                .kind,
            FpRefuseKind::Conflict
        );
    }

    #[test]
    fn sandbox_camel_vs_kebab_normalizes() {
        let mut f = fp();
        f.sandbox = "workspace-write".into();
        let p = json!({"approvalPolicy":"untrusted","approvalsReviewer":"user","sandboxPolicy":"workspaceWrite"});
        assert!(assert_fingerprint(&f, "turn/start", &p).is_ok());
    }

    #[test]
    fn sandbox_mode_only_object_ok() {
        let p = full_start(json!({"sandbox": {"mode":"read-only"}}));
        assert!(assert_fingerprint(&fp(), "thread/start", &p).is_ok());
    }

    #[test]
    fn sandbox_object_with_extra_field_is_unprovable() {
        // A matching mode must NOT conceal a differing policy.
        let p = full_start(json!({"sandbox": {"mode":"read-only","writableRoots":["/etc"]}}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &p)
                .unwrap_err()
                .kind,
            FpRefuseKind::Unprovable
        );
    }

    #[test]
    fn notify_present_is_refused_typed_and_nested() {
        let typed = full_start(json!({"notify": ["cmd"]}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &typed)
                .unwrap_err()
                .kind,
            FpRefuseKind::Unprovable
        );
        let nested = full_start(json!({"config": {"notify": ["osascript", "-e", "x"]}}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &nested)
                .unwrap_err()
                .kind,
            FpRefuseKind::Unprovable
        );
    }

    #[test]
    fn hooks_bool_matches_or_conflicts() {
        // Matching bool ok.
        let ok = full_start(json!({"config": {"hooks": true}}));
        assert!(assert_fingerprint(&fp(), "thread/start", &ok).is_ok());
        // Disable conflicts.
        let bad = full_start(json!({"config": {"features": {"hooks": false}}}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &bad)
                .unwrap_err()
                .kind,
            FpRefuseKind::Conflict
        );
    }

    #[test]
    fn hooks_table_and_dotted_and_alias_are_unprovable() {
        // A hooks table (hook definitions).
        let table = full_start(json!({"config": {"hooks": {"on_turn": "run x"}}}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &table)
                .unwrap_err()
                .kind,
            FpRefuseKind::Unprovable
        );
        // The codex_hooks alias.
        let alias = full_start(json!({"config": {"features": {"codex_hooks": true}}}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &alias)
                .unwrap_err()
                .kind,
            FpRefuseKind::Unprovable
        );
    }

    #[test]
    fn dotted_hooks_and_notify_keys_are_caught() {
        // A literal dotted hooks key (would path-expand to hooks.on_turn under codex's
        // config merge if it path-expands JSON string keys — fail closed regardless).
        let dotted_hooks = full_start(json!({"config": {"hooks.on_turn": "run x"}}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &dotted_hooks)
                .unwrap_err()
                .kind,
            FpRefuseKind::Unprovable
        );
        // A dotted notify key.
        let dotted_notify = full_start(json!({"config": {"notify.command": ["x"]}}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &dotted_notify)
                .unwrap_err()
                .kind,
            FpRefuseKind::Unprovable
        );
    }

    #[test]
    fn decoy_subtree_does_not_satisfy_presence() {
        // All three ownership dimensions appear under a NESTED decoy key while the
        // effective root/typed fields stay absent -> must be refused as Absent, not
        // forwarded, on both policy-setting thread ops.
        let decoy = json!({"config": {"decoy": {
            "approval_policy": "untrusted",
            "approvals_reviewer": "user",
            "sandbox": "read-only"
        }}});
        for method in ["thread/start", "thread/fork"] {
            assert_eq!(
                assert_fingerprint(&fp(), method, &decoy).unwrap_err().kind,
                FpRefuseKind::Absent,
                "{method}: nested decoy must not satisfy presence"
            );
        }
    }

    #[test]
    fn effective_config_root_satisfies_presence() {
        // The effective config ROOT keys (not typed) prove presence and match.
        let p = json!({"config": {
            "approval_policy": "untrusted",
            "approvals_reviewer": "user",
            "sandbox_mode": "read-only"
        }});
        assert!(assert_fingerprint(&fp(), "thread/start", &p).is_ok());
    }

    #[test]
    fn nested_decoy_that_conflicts_still_refuses() {
        // A nested decoy leaf never proves presence, but a CONFLICTING nested leaf still
        // refuses (conflict is detected over the whole tree).
        let p = json!({
            "approvalPolicy": "untrusted",
            "approvalsReviewer": "user",
            "sandbox": "read-only",
            "config": {"nested": {"approval_policy": "never"}}
        });
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &p)
                .unwrap_err()
                .kind,
            FpRefuseKind::Conflict
        );
    }

    #[test]
    fn absence_per_dimension_refused_on_policy_setting() {
        // Missing approvals_reviewer.
        let p = json!({"approvalPolicy":"untrusted","sandbox":"read-only"});
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &p)
                .unwrap_err()
                .kind,
            FpRefuseKind::Absent
        );
        // Missing sandbox.
        let p2 = json!({"approvalPolicy":"untrusted","approvalsReviewer":"user"});
        assert_eq!(
            assert_fingerprint(&fp(), "turn/start", &p2)
                .unwrap_err()
                .kind,
            FpRefuseKind::Absent
        );
        // Missing approval_policy.
        let p3 = json!({"approvalsReviewer":"user","sandbox":"read-only"});
        assert_eq!(
            assert_fingerprint(&fp(), "thread/fork", &p3)
                .unwrap_err()
                .kind,
            FpRefuseKind::Absent
        );
    }

    // GAP 1a — a dotted key whose first segment is NOT an owned name (so the old
    // owned-prefix walk never collected it) is now refused categorically. Without the fix
    // `features.codex_hooks: false` would silently disable hooks with no refusal.
    #[test]
    fn dotted_unowned_prefix_key_is_categorically_refused() {
        let p = full_start(json!({"config": {"features.codex_hooks": false}}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &p)
                .unwrap_err()
                .kind,
            FpRefuseKind::Unprovable,
            "a dotted config key must be refused categorically"
        );
    }

    // GAP 1b — a dotted decoy whose first segment is unowned (`decoy.approval_policy`) used
    // to escape conflict detection entirely; now refused categorically even though the
    // effective approval_policy is correct.
    #[test]
    fn dotted_decoy_key_is_categorically_refused() {
        let p = full_start(json!({"config": {"decoy.approval_policy": "never"}}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &p)
                .unwrap_err()
                .kind,
            FpRefuseKind::Unprovable,
            "a dotted decoy config key must be refused categorically"
        );
    }

    // GAP 2 — `config` is not carried by turn/start per the bundled schema, so a config-root
    // assertion (no typed field) must NOT satisfy presence on turn/start: refuse as Absent.
    #[test]
    fn config_root_does_not_satisfy_presence_on_turn_start() {
        let p = json!({"config": {
            "approval_policy": "untrusted",
            "approvals_reviewer": "user",
            "sandbox_mode": "read-only"
        }});
        assert_eq!(
            assert_fingerprint(&fp(), "turn/start", &p)
                .unwrap_err()
                .kind,
            FpRefuseKind::Absent,
            "config root is not schema-effective for turn/start"
        );
        // The same config-root assertion DOES satisfy presence on thread/start (which
        // carries config) — guards against over-refusing the legitimate path.
        assert!(assert_fingerprint(&fp(), "thread/start", &p).is_ok());
    }

    // GAP 3 — a conflicting leaf nested inside an ARRAY under config used to escape the
    // whole-tree conflict scan (walk_dim only descended objects). Now refused as Conflict
    // even though the effective approval_policy is correct.
    #[test]
    fn array_nested_conflict_is_detected() {
        let p = full_start(json!({"config": {
            "decoys": [{"approval_policy": "never"}]
        }}));
        assert_eq!(
            assert_fingerprint(&fp(), "thread/start", &p)
                .unwrap_err()
                .kind,
            FpRefuseKind::Conflict,
            "a conflicting leaf inside a config array must be detected"
        );
    }

    #[test]
    fn resume_absence_is_benign_but_conflict_and_categoricals_still_apply() {
        // ccd attach: no ownership fields at all.
        assert!(assert_fingerprint(&fp(), "thread/resume", &json!({"threadId":"t"})).is_ok());
        // Conflict still refused.
        assert_eq!(
            assert_fingerprint(
                &fp(),
                "thread/resume",
                &json!({"threadId":"t","approvalPolicy":"never"})
            )
            .unwrap_err()
            .kind,
            FpRefuseKind::Conflict
        );
        // notify still categorically refused on resume.
        assert_eq!(
            assert_fingerprint(
                &fp(),
                "thread/resume",
                &json!({"threadId":"t","notify":["x"]})
            )
            .unwrap_err()
            .kind,
            FpRefuseKind::Unprovable
        );
    }
}
