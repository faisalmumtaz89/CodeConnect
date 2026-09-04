//! The **guarded surface**: the part of a codex build this broker actually defends,
//! projected out of that build's own self-description so two builds can be compared.
//!
//! # Why this exists
//!
//! Codex's app-server protocol is experimental and the binary ships roughly weekly.
//! The launcher used to gate on an exact version string
//! ([`protocol::config::CODEX_PINNED_VERSIONS`], now recorded rather than gated), which
//! is a proxy: it refuses every new build, including the overwhelming majority whose
//! wire shape did not move at all in any way this broker cares about. A proxy that says
//! "no" to everything is not a security property, it is an outage on a weekly timer —
//! and the pressure it creates is to bump the number, which is the one change that
//! proves nothing.
//!
//! So the gate pins **what is guarded** instead of **what it is called**. Two surfaces
//! are projected out of the installed binary and compared against a vendored 0.147
//! reference; identical means admit whatever the version says, different means refuse
//! naming the exact token or field that moved.
//!
//! # The two surfaces, and why it takes two
//!
//! * **The wire surface** ([`project_bundle`]) — the app-server JSON-RPC protocol, read
//!   from `codex app-server generate-json-schema`. This is what [`crate::allowlist`]
//!   and [`crate::fingerprint`] guard. **Both** `ClientRequest.json` and
//!   `ClientNotification.json` are projected: `initialized` is an admitted notification,
//!   so its shape is guarded, and it is not a request. Entries are keyed by JSON-RPC
//!   kind *and* method, so a name that is both cannot silently overwrite the other.
//! * **The argv surface** ([`project_argv`]) — the CLI's root subcommand and flag set,
//!   read from `codex completion bash`. This is what
//!   `codeconnect::codex::validate_codex_argv` guards.
//!
//! **A wire-only gate would have been a regression, and that is measured rather than
//! argued.** codex 0.153 adds three top-level subcommands — `agents`, `queue`,
//! `migrate-rollouts` — that `validate_codex_argv` classifies as prompt text and
//! forwards, because its refusal list is a closed table grounded on 0.147. `codex
//! agents` browses sessions on the *shared local app-server daemon*, and `codex queue`
//! injects a message into another session: both step around the broker entirely, which
//! is the one thing the command gate exists to prevent. `generate-json-schema` does not
//! describe the CLI at all, so a schema-only gate would have admitted 0.153 with that
//! door open. The version pin was refusing 0.153 for a bad reason and a real one at
//! once; replacing it has to keep the real one.
//!
//! # The guarded method set is DERIVED, never listed
//!
//! [`is_guarded`] asks [`crate::allowlist::disposition`] — the same total function the
//! relay routes on and `tests/exhaustiveness.rs` proves — whether any `(role, kind)`
//! cell does something other than refuse. A hand-maintained second list of "the methods
//! we care about" would be a list that drifts from the allowlist silently, and a gate
//! reading a stale list is a gate that passes because it looked in the wrong place.
//! Add a method to the allowlist and it enters the guarded surface with no edit here.
//!
//! # What the projection deliberately drops, and where it must NOT
//!
//! `description` and `title` are stripped **as JSON Schema annotation keywords**: at a
//! schema node, they are documentation, codex rewords them constantly, they change no
//! wire shape, and a gate that refuses on a reworded doc comment teaches its operator to
//! bypass it.
//!
//! They are **not** stripped where those spellings are *data*. Inside `properties`,
//! `definitions` and the other name→schema maps they are wire field names, and inside
//! `const`/`default`/`enum`/`examples` they are literal values. Deleting them there
//! deletes real surface: measured on 0.147's `DynamicToolNamespaceTool`, whose function
//! variant genuinely has a required wire field named `description` of type `string` —
//! a context-blind strip removed it from the projection while leaving it in `required`,
//! so any future change to that field would have been invisible to the gate.
//!
//! Everything else is kept — enum variants, type unions, `required`, the request
//! envelope, and `$ref`s, which are kept **as references** with the definitions they
//! reach carried alongside (see [`strip`]; inlining them cost 37 s per launch and proved
//! nothing extra). A nullability widening or a new enum member IS a shape change and
//! must refuse.
//!
//! # Admission is an EXACT match against a patched baseline
//!
//! CodeConnect hosts two builds: 0.147, which every live gate in this repo was proven
//! on, and 0.153, which is installed. Both surfaces are vendored — the 0.147
//! **baseline** and the 0.153 **grounded ceiling** — and [`ADJUDICATED_WIRE`] /
//! [`ADJUDICATED_ARGV`] are the audited bridge between them: pointer-addressed facts,
//! each carrying the measurement that made it safe.
//!
//! The gate builds an *admissible* surface ([`admissible_wire`]) by starting from the
//! baseline and splicing in exactly those adjudicated deltas the installed binary
//! actually exhibits **with the measured value** — then demands the whole result equal
//! the installed projection, with no residual difference of any kind. A build that
//! carries an adjudicated field with a different shape does not exhibit that delta, so
//! it is not spliced, so the equality fails and the refusal names the field. An
//! adjudicated addition can no longer mask an unrelated `required` or nested-definition
//! change beside it, because nothing is filtered out of the comparison: the comparison
//! IS the admission.
//!
//! # Fail direction
//!
//! Closed, in every arm. A schema that will not parse, a method that has vanished, a
//! duplicate variant, a `$ref` that does not resolve, a subcommand that appeared: all
//! refuse. The one thing that admits is a projection that equals the admissible surface
//! exactly.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::allowlist::{disposition, Disposition, JsonRpcKind, Role};

/// JSON Schema annotation keywords carrying human documentation rather than wire shape.
/// Stripped **at schema nodes only** — see the module header and [`strip`].
const DOC_KEYS: [&str; 2] = ["description", "title"];

/// Keywords whose value is a map from a NAME to a schema. The names are wire field
/// names, so every key is carried through verbatim and only the schemas beneath them
/// are stripped.
///
/// Both codex bundles use only `properties` and `definitions` today (measured — none of
/// the others appears at any depth in either release's `ClientRequest.json`). The rest are
/// listed because the rule being stated is a fact about JSON Schema, not about codex: the
/// moment a build emits a `patternProperties` with a field spelled `description`, a list
/// scoped to "what we have seen" would reintroduce exactly the defect this fixes.
const NAMED_SCHEMA_MAPS: [&str; 5] = [
    "properties",
    "patternProperties",
    "definitions",
    "$defs",
    "dependentSchemas",
];

/// Keywords whose value is literal DATA, not a schema. Copied verbatim: a default or an
/// enum member is free to be an object with a `description` key of its own, and that key
/// is part of the wire shape rather than documentation about it.
const OPAQUE_KEYWORDS: [&str; 4] = ["const", "default", "enum", "examples"];

/// The two endpoint roles the allowlist can be asked about.
const ROLES: [Role; 2] = [Role::Tui, Role::Ccd];

/// Is this method guarded **as this JSON-RPC kind** — i.e. does the broker do anything
/// with it on either leg other than refuse it?
///
/// Kind-scoped, because the projection is: a method the allowlist forwards as a request
/// and refuses as a notification is guarded only in its request form, and the
/// notification bundle should not carry it.
fn is_guarded_as(kind: JsonRpcKind, method: &str) -> bool {
    ROLES
        .iter()
        .any(|&role| !matches!(disposition(role, kind, method), Disposition::Refuse(_)))
}

/// Is this method part of the guarded surface in any kind — i.e. does the broker do
/// anything with it other than refuse it on every leg?
///
/// Derived from [`crate::allowlist::disposition`] so there is exactly one statement of
/// what is guarded. A refused method's schema is irrelevant by construction: the bytes
/// never reach the app-server no matter what shape they claim.
pub fn is_guarded(method: &str) -> bool {
    is_guarded_as(JsonRpcKind::Request, method) || is_guarded_as(JsonRpcKind::Notification, method)
}

/// A projection failure. Every variant is a refusal — see the module header's fail
/// direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    /// A bundle document was not the expected `oneOf` envelope.
    Malformed(String),
    /// A `$ref` named a definition the bundle does not contain.
    UnresolvedRef(String),
    /// Two variants claimed the same `(kind, method)`. The second would have overwritten
    /// the first, so a permissive variant followed by a baseline-shaped one would have
    /// compared equal to the baseline.
    DuplicateVariant(String),
    /// A guarded method's `<X>Response` document was not among the ones supplied. The
    /// caller reads exactly what [`guarded_result_types`] names, so this means the bundle
    /// does not contain a response the schema says the method has.
    MissingResult(String),
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectionError::Malformed(what) => {
                write!(
                    f,
                    "the codex schema bundle is not the expected shape: {what}"
                )
            }
            ProjectionError::UnresolvedRef(name) => {
                write!(f, "the codex schema bundle has a dangling $ref to {name:?}")
            }
            ProjectionError::DuplicateVariant(key) => {
                write!(
                    f,
                    "the codex schema bundle describes {key:?} twice; one shape would \
                     have silently replaced the other"
                )
            }
            ProjectionError::MissingResult(name) => {
                write!(
                    f,
                    "the codex schema bundle has no {name:?}, which a guarded method's \
                     params type says is its result"
                )
            }
        }
    }
}

impl std::error::Error for ProjectionError {}

/// One guarded surface: `"<kind> <method>"` → `{"envelope": …, "params": …,
/// "definitions": {…}}`, where `definitions` is the transitive closure of types the
/// envelope and params reach.
///
/// `BTreeMap` and `serde_json`'s sorted `Map` together make the serialized form
/// canonical, so equality of the vendored bytes and the projected bytes is equality of
/// the surface. (This crate does not enable serde_json's `preserve_order`; verified in
/// `Cargo.lock`.)
pub type GuardedSurface = BTreeMap<String, Value>;

/// The surface key for one method in one JSON-RPC kind.
///
/// Keyed by kind as well as name because several documents are projected into one map and
/// a method may exist in more than one form. `initialized` is guarded as a notification
/// only; a future request of the same name must not land on top of it.
pub fn surface_key(kind: JsonRpcKind, method: &str) -> String {
    match kind {
        JsonRpcKind::Request => format!("request {method}"),
        JsonRpcKind::Notification => format!("notification {method}"),
    }
}

/// The surface key for a SERVER→client request.
///
/// A separate namespace from the client keys, because the two directions are different
/// contracts that happen to share a method vocabulary.
pub fn server_request_key(method: &str) -> String {
    format!("server-request {method}")
}

/// The response type a guarded method's RESULT is described by, derived from the schema
/// rather than from the method name.
///
/// The link the schema does give is mechanical: a request's `params` is `{"$ref":
/// "#/definitions/<X>Params"}` and its result is the document `<X>Response`. The link the
/// method NAME would give is not — measured, `account/read`'s response is
/// `GetAccountResponse` and `app/list`'s is `AppsListResponse`, so any PascalCase rule on
/// the method string is a guess.
///
/// `None` for a method whose `params` is not a `$ref` — measured, exactly the two that
/// take no params at all (`account/rateLimits/read` and `configRequirements/read`, both
/// `{"type": "null"}`). Their responses exist but the schema offers no mechanical route to
/// them, which is a recorded gap rather than a silent one: see
/// `the_methods_with_no_projectable_result_are_exactly_the_two_measured`.
fn result_type_of(variant_params: Option<&Value>) -> Option<String> {
    let name = variant_params?
        .get("$ref")?
        .as_str()?
        .rsplit('/')
        .next()
        .unwrap_or_default();
    name.strip_suffix("Params")
        .map(|stem| format!("{stem}Response"))
}

/// Every response type name the guarded methods of this `ClientRequest.json` reach.
///
/// The caller reads exactly these documents and hands them back in [`BundleDocs::results`]
/// — the projector does not do I/O, and the launch gate's reads must all happen inside the
/// held freeze (see `codeconnect::codex::probe_codex`).
pub fn guarded_result_types(client_request: &Value) -> Result<BTreeSet<String>, ProjectionError> {
    let variants = client_request
        .get("oneOf")
        .and_then(Value::as_array)
        .ok_or_else(|| ProjectionError::Malformed("ClientRequest.json has no `oneOf`".into()))?;
    let mut out = BTreeSet::new();
    for variant in variants {
        let Some(method) = variant_method(variant) else {
            continue;
        };
        if !is_guarded_as(JsonRpcKind::Request, method) {
            continue;
        }
        if let Some(name) = result_type_of(variant.get("properties").and_then(|p| p.get("params")))
        {
            out.insert(name);
        }
    }
    Ok(out)
}

/// A vendored reference, read with the same duplicate-member discipline as everything
/// else the gate compares.
fn parse_reference(raw: &str) -> Result<GuardedSurface, ProjectionError> {
    let value = parse_schema(raw)?;
    serde_json::from_value(value)
        .map_err(|e| ProjectionError::Malformed(format!("not a guarded surface: {e}")))
}

/// Parse one schema document with the **same duplicate-member discipline the c2s
/// classifier uses**.
///
/// `serde_json` accepts a duplicate object member and keeps one of the two values, so two
/// raw schema documents that differ can collapse to the same projected `Value` — and the
/// gate's whole verdict is an equality of projected values. That is the identical
/// parser-differential hazard [`crate::message::classify_shape`] rejects on the wire, and
/// it is rejected here for the identical reason: a document whose meaning depends on which
/// duplicate a parser happens to keep has no single meaning to compare.
///
/// Applied to the vendored references too, not only the generated ones. A reference is
/// only evidence if it says one thing.
pub fn parse_schema(raw: &str) -> Result<Value, ProjectionError> {
    crate::message::parse_no_dup_value(raw).ok_or_else(|| {
        ProjectionError::Malformed(
            "the document is not duplicate-free JSON (unparseable, or a member appears \
             twice at some depth)"
                .into(),
        )
    })
}

/// The client- and server-side documents one bundle is projected from.
///
/// Four, not one, because the broker's contract with codex is not only "what may the
/// client send": it relays and classifies the SERVER's requests too, and it forwards the
/// RESULTS of the methods it admits. A gate that read only `ClientRequest.json` would
/// admit a build whose `thread/read` result grew a field, or whose `item/tool/call`
/// changed shape — both of which the broker acts on.
pub struct BundleDocs<'a> {
    pub client_request: &'a Value,
    pub client_notification: &'a Value,
    pub server_request: &'a Value,
    /// `<X>Response` documents, by type name — exactly the set
    /// [`guarded_result_types`] asked for.
    pub results: &'a BTreeMap<String, Value>,
}

/// Strip annotation keywords from one SCHEMA NODE, keeping `$ref`s intact, and collect
/// the names referenced.
///
/// # Why references are KEPT rather than inlined
///
/// The first version of this inlined every `$ref` and collapsed recursion to a
/// `{"$cycle": …}` marker. It was correct and it was unusably slow: codex's schema is
/// deeply recursive and heavily shared, so inlining re-expanded the same definitions
/// combinatorially — **37 seconds per launch**, measured, against ~150 ms for the three
/// execs that feed it. A gate that adds half a minute to opening a terminal is a gate
/// people turn off.
///
/// Keeping the reference is not a weaker comparison, it is the same one done once.
/// Two schemas agree on a guarded method exactly when its params agree *with `$ref`
/// names in place* AND every definition reachable from it agrees — which is what
/// [`project_bundle`] compares. A renamed type changes a `$ref` string; a changed type
/// changes its entry in the reachable set; a nullability widening changes the leaf that
/// holds it. Nothing that inlining would have caught is lost, and recursion stops being
/// a special case because nothing is expanded.
///
/// # The context rule
///
/// `description` and `title` are dropped **here**, at a schema node, where they are
/// annotations. Under a [`NAMED_SCHEMA_MAPS`] keyword the same spellings are wire field
/// names and every key survives; under an [`OPAQUE_KEYWORDS`] keyword the value is
/// literal data and is copied byte for byte. See the module header for the measurement
/// that forced this.
fn strip(node: &Value, refs: &mut BTreeSet<String>) -> Value {
    match node {
        Value::Object(obj) => {
            if let Some(Value::String(r)) = obj.get("$ref") {
                refs.insert(r.rsplit('/').next().unwrap_or(r).to_string());
            }
            let mut out = Map::new();
            for (k, v) in obj {
                let k = k.as_str();
                if DOC_KEYS.contains(&k) {
                    continue;
                }
                let projected = if OPAQUE_KEYWORDS.contains(&k) {
                    v.clone()
                } else if NAMED_SCHEMA_MAPS.contains(&k) {
                    strip_named_map(v, refs)
                } else {
                    strip(v, refs)
                };
                out.insert(k.to_string(), projected);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| strip(i, refs)).collect()),
        other => other.clone(),
    }
}

/// Strip a map from wire NAME to schema. Every key is kept exactly as it is spelled;
/// only the schemas beneath are stripped.
fn strip_named_map(node: &Value, refs: &mut BTreeSet<String>) -> Value {
    match node {
        Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), strip(v, refs)))
                .collect(),
        ),
        // Not a map — keep it as it is rather than inventing a shape for it. A bundle
        // whose `properties` is not an object is compared as the odd value it is.
        other => other.clone(),
    }
}

/// The transitive closure of definitions reachable from `seed`, each stripped.
///
/// Iterative and visited-guarded, so a recursive type is walked once. A `$ref` naming a
/// definition the bundle does not contain is a refusal — a dangling reference means the
/// schema is not self-consistent and nothing about it can be compared honestly.
fn reachable(
    defs: &Map<String, Value>,
    seed: BTreeSet<String>,
) -> Result<BTreeMap<String, Value>, ProjectionError> {
    let mut out = BTreeMap::new();
    let mut queue: Vec<String> = seed.into_iter().collect();
    while let Some(name) = queue.pop() {
        if out.contains_key(&name) {
            continue;
        }
        let target = defs
            .get(&name)
            .ok_or_else(|| ProjectionError::UnresolvedRef(name.clone()))?;
        let mut found = BTreeSet::new();
        let stripped = strip(target, &mut found);
        out.insert(name, stripped);
        queue.extend(found);
    }
    Ok(out)
}

/// Read the `method` constant off one `oneOf` variant, in either spelling the schema
/// has used (`const`, or a single-member `enum` — 0.147 and 0.153 both emit the latter).
fn variant_method(variant: &Value) -> Option<&str> {
    let m = variant.get("properties")?.get("method")?;
    if let Some(Value::String(c)) = m.get("const") {
        return Some(c);
    }
    match m.get("enum")?.as_array()?.as_slice() {
        [Value::String(one)] => Some(one),
        _ => None,
    }
}

/// Project one bundle document's `oneOf` variants into `surface`.
///
/// `params` is projected as the method's shape; everything else about the variant — the
/// `required` list that says whether `params` may be absent at all, the `id` type, the
/// method constant — is projected as the **envelope**, because those are constraints the
/// broker relies on and dropping them would put them outside the gate.
fn project_variants(
    doc: &Value,
    side: Side,
    what: &str,
    results: &BTreeMap<String, Value>,
    surface: &mut GuardedSurface,
) -> Result<(), ProjectionError> {
    let defs = doc
        .get("definitions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let variants = doc
        .get("oneOf")
        .and_then(Value::as_array)
        .ok_or_else(|| ProjectionError::Malformed(format!("{what} has no `oneOf`")))?;

    for variant in variants {
        let Some(method) = variant_method(variant) else {
            return Err(ProjectionError::Malformed(format!(
                "a {what} `oneOf` variant carries no method constant"
            )));
        };
        let key = match side {
            // Every server→client request is projected. The broker relays all of them and
            // now classifies all of them (an unadmitted one is answered upstream rather
            // than delivered — see `crate::relay`), so the SET is part of the contract:
            // a new one appearing is a re-grounding, not a detail.
            Side::ServerRequest => server_request_key(method),
            Side::Client(kind) => {
                if !is_guarded_as(kind, method) {
                    continue;
                }
                surface_key(kind, method)
            }
        };
        let mut refs = BTreeSet::new();
        let params = match variant.get("properties").and_then(|p| p.get("params")) {
            Some(p) => strip(&p.clone(), &mut refs),
            // A method with no params at all is a real, comparable shape. The
            // `initialized` notification is exactly this.
            None => Value::Null,
        };
        // The params AND every definition they reach, so a change anywhere beneath a
        // `$ref` is still a change to this method's entry. See `strip`.
        let mut entry = json!({
            "envelope": envelope(variant, &mut refs),
            "params": params,
            "definitions": reachable(&defs, refs)?,
        });
        // The RESULT the method answers with, for the client requests the broker admits.
        // `null` where the schema offers no mechanical route to it — see
        // [`result_type_of`] — which is a shape too, and a comparable one.
        if matches!(side, Side::Client(JsonRpcKind::Request)) {
            let params_node = variant.get("properties").and_then(|p| p.get("params"));
            entry["result"] = match result_type_of(params_node) {
                Some(name) => match results.get(&name) {
                    Some(doc) => project_document(doc)?,
                    None => return Err(ProjectionError::MissingResult(name)),
                },
                None => Value::Null,
            };
        }
        if surface.insert(key.clone(), entry).is_some() {
            return Err(ProjectionError::DuplicateVariant(key));
        }
    }
    Ok(())
}

/// Which document a variant came out of. Kept apart from [`JsonRpcKind`], which keys the
/// allowlist and describes the CLIENT side only.
#[derive(Debug, Clone, Copy)]
enum Side {
    Client(JsonRpcKind),
    ServerRequest,
}

/// Project one standalone schema document — a `<X>Response` — into
/// `{"schema": …, "definitions": {…}}`.
///
/// Self-contained rather than merged into the request's definitions: a response document
/// carries its own `definitions` under the same names, and merging two same-named
/// definitions would either collide or silently pick one. Keeping them apart makes the
/// comparison exact on both halves.
fn project_document(doc: &Value) -> Result<Value, ProjectionError> {
    let defs = doc
        .get("definitions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut body = doc.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.remove("definitions");
        // Not shape: the dialect declaration travels with every generated file.
        obj.remove("$schema");
    }
    let mut refs = BTreeSet::new();
    let schema = strip(&body, &mut refs);
    Ok(json!({"schema": schema, "definitions": reachable(&defs, refs)?}))
}

/// The variant with `params` lifted out: `required`, the `id` type, the method constant
/// and anything else the bundle says about the message frame itself.
fn envelope(variant: &Value, refs: &mut BTreeSet<String>) -> Value {
    let mut env = variant.clone();
    if let Some(props) = env
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    {
        props.remove("params");
    }
    strip(&env, refs)
}

/// Project a build's guarded wire surface out of its two client-side bundle documents.
///
/// Only guarded methods are kept ([`is_guarded_as`]). A method the broker refuses on
/// every leg contributes nothing, so codex is free to change it — that freedom is the
/// whole point of gating the guarded surface rather than the version number.
pub fn project_bundle(docs: &BundleDocs) -> Result<GuardedSurface, ProjectionError> {
    let mut surface = GuardedSurface::new();
    project_variants(
        docs.client_request,
        Side::Client(JsonRpcKind::Request),
        "ClientRequest.json",
        docs.results,
        &mut surface,
    )?;
    project_variants(
        docs.client_notification,
        Side::Client(JsonRpcKind::Notification),
        "ClientNotification.json",
        docs.results,
        &mut surface,
    )?;
    project_variants(
        docs.server_request,
        Side::ServerRequest,
        "ServerRequest.json",
        docs.results,
        &mut surface,
    )?;
    Ok(surface)
}

/// The CLI's root argv surface: the subcommand names and flag spellings codex will
/// dispatch on, as the binary itself enumerates them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArgvSurface {
    pub subcommands: BTreeSet<String>,
    pub flags: BTreeSet<String>,
}

/// Project `codex completion bash` output onto the root argv surface.
///
/// Measured shape (both 0.147 and 0.153): the generated completion script has one
/// `opts="…"` line per command node, and the ROOT node's is the first one inside the
/// `codex)` case arm. It lists every root flag (long and short) and every root
/// subcommand name and alias, space separated, with a `[PROMPT]` placeholder for the
/// positional. Parsing that one line is how the 0.147 `is_subcommand` table was built
/// in the first place; this makes the derivation executable instead of a comment.
///
/// **This is the fullest census the binary emits, and it is still not complete** — see
/// [`HIDDEN_ROOT_ALIASES`] for the measurement and for what covers the gap.
///
/// Anything starting `-` is a flag; `[PROMPT]` is dropped; everything else is a
/// subcommand.
pub fn project_argv(completion_script: &str) -> Result<ArgvSurface, ProjectionError> {
    let mut in_root = false;
    for line in completion_script.lines() {
        let t = line.trim();
        if t == "codex)" {
            in_root = true;
            continue;
        }
        if !in_root {
            continue;
        }
        let Some(rest) = t.strip_prefix("opts=\"") else {
            continue;
        };
        let Some(body) = rest.strip_suffix('"') else {
            return Err(ProjectionError::Malformed(
                "the root `opts=` line is not a single quoted string".into(),
            ));
        };
        let mut subcommands = BTreeSet::new();
        let mut flags = BTreeSet::new();
        for tok in body.split_whitespace() {
            if tok == "[PROMPT]" {
                continue;
            }
            if tok.starts_with('-') {
                flags.insert(tok.to_string());
            } else {
                subcommands.insert(tok.to_string());
            }
        }
        if subcommands.is_empty() || flags.is_empty() {
            return Err(ProjectionError::Malformed(
                "the root `opts=` line named no subcommands or no flags".into(),
            ));
        }
        return Ok(ArgvSurface { subcommands, flags });
    }
    Err(ProjectionError::Malformed(
        "no root `opts=` line found in the completion script".into(),
    ))
}

// ------------------------------------------------------- the vendored references

/// The two schema bundles the references cover.
///
/// Both, not just `stable`. The app-server is spawned without `--experimental`, so
/// stable is the live wire — but the allowlist and its exhaustiveness proof span both,
/// and a feature toggle can move a method between them. Gating only the bundle we
/// expect to meet would leave the other unmeasured.
pub const BUNDLES: [&str; 2] = ["stable", "experimental"];

/// The vendored **baseline** guarded wire surface — codex 0.147, the build every live
/// gate in this repo was proven on.
///
/// Compiled in rather than read from disk, for the same reason
/// [`protocol::config::CODEX_PINNED_VERSIONS`] was: a reference an operator can edit is
/// a gate an operator can switch off, and this one travels with the code that was
/// tested against it. Panics on a malformed file — it is `include_str!`d, so a failure
/// here is a build that should never have shipped, not a runtime condition.
pub fn baseline_wire(bundle: &str) -> GuardedSurface {
    let raw = match bundle {
        "stable" => include_str!("../schema-0.147/guarded-wire-stable.json"),
        "experimental" => include_str!("../schema-0.147/guarded-wire-experimental.json"),
        other => panic!("no vendored baseline wire surface for bundle {other:?}"),
    };
    parse_reference(raw).expect("the vendored baseline wire surface parses")
}

/// The vendored **grounded ceiling** — codex 0.153, the build installed here, with every
/// difference from the baseline adjudicated in [`ADJUDICATED_WIRE`].
///
/// This is where each delta's *measured post-change fragment* lives. Storing the
/// fragments as a projection rather than as string literals beside the table means they
/// are regenerated by the gate's own projector from a real binary, reviewed as a diff,
/// and checkable offline: `the_adjudicated_table_bridges_the_two_references` proves the
/// table applied to the baseline reproduces this file exactly.
pub fn grounded_wire(bundle: &str) -> GuardedSurface {
    let raw = match bundle {
        "stable" => include_str!("../schema-0.153/guarded-wire-stable.json"),
        "experimental" => include_str!("../schema-0.153/guarded-wire-experimental.json"),
        other => panic!("no vendored grounded wire surface for bundle {other:?}"),
    };
    parse_reference(raw).expect("the vendored grounded wire surface parses")
}

/// The vendored baseline root argv surface (0.147).
pub fn baseline_argv() -> ArgvSurface {
    serde_json::from_value(
        parse_schema(include_str!("../schema-0.147/guarded-argv.json")).expect("parses"),
    )
    .expect("the vendored baseline argv surface parses")
}

/// The vendored grounded root argv surface (0.153).
pub fn grounded_argv() -> ArgvSurface {
    serde_json::from_value(
        parse_schema(include_str!("../schema-0.153/guarded-argv.json")).expect("parses"),
    )
    .expect("the vendored grounded argv surface parses")
}

/// Root subcommand aliases that **dispatch but appear in no enumeration codex emits**.
///
/// MEASURED on both binaries: `cloud-tasks` is a hidden alias of `cloud`
/// (`codex cloud-tasks --help` prints `Usage: codex cloud …`), and it is absent from
/// `codex --help`, from `codex completion {bash,zsh,fish,elvish,powershell}` and
/// therefore from [`project_argv`]. Hidden *subcommands* do appear — `execpolicy`,
/// `responses-api-proxy` and `stdio-to-uds` are in the completion script and not in
/// `--help` — so the completion script is the fullest census available; hidden
/// **aliases** are the one class no self-description carries.
///
/// The argv gate therefore cannot see this class, and a list is the only honest place to
/// put it. `codeconnect::codex::is_subcommand` refuses every entry, and
/// `the_hidden_root_aliases_are_refused_and_dispatch` proves each one still dispatches on
/// a live binary — so the list cannot quietly become a lie in either direction.
pub const HIDDEN_ROOT_ALIASES: [&str; 1] = ["cloud-tasks"];

/// Both surfaces as one codex binary describes them: the root argv surface, and one
/// `(bundle, guarded surface)` pair per [`BUNDLES`] entry.
pub struct ProjectedBinary {
    pub argv: ArgvSurface,
    pub wire: Vec<(&'static str, GuardedSurface)>,
}

/// Project a codex binary's two surfaces by asking it to describe itself.
///
/// # This is the UNPINNED reader, and it is not the gate
///
/// It plain-`Command`s the path it is given. The real launch gate
/// (`codeconnect::codex::ensure_guarded_surface`) must not use this: it freezes and
/// re-verifies the binary's digest before each exec and holds the freeze across it, so
/// that the build which *describes* itself is provably the build that will *run*.
/// Without that, a binary could answer with 0.147's surface and then have something
/// else exec'd under the app-server and the TUI.
///
/// What it is for is **premises**: a live suite asking "is the codex I am about to
/// test one this build is grounded against?". That question is about the harness's own
/// footing, not about containing a hostile binary, and the suites already resolve and
/// exec that path a dozen other ways.
///
/// Returns [`ProjectedBinary`], or a human-readable reason it could not.
pub fn project_from_binary(codex: &std::path::Path) -> Result<ProjectedBinary, String> {
    let run = |args: &[&str]| -> Result<std::process::Output, String> {
        std::process::Command::new(codex)
            .args(args)
            .output()
            .map_err(|e| format!("running {} {}: {e}", codex.display(), args.join(" ")))
            .and_then(|o| {
                if o.status.success() {
                    Ok(o)
                } else {
                    Err(format!(
                        "{} {} exited with {}",
                        codex.display(),
                        args.join(" "),
                        o.status
                    ))
                }
            })
    };

    let completion = run(&["completion", "bash"])?;
    let completion = String::from_utf8(completion.stdout)
        .map_err(|_| "`codex completion bash` did not emit UTF-8".to_string())?;
    let argv = project_argv(&completion).map_err(|e| e.to_string())?;

    let dir = std::env::temp_dir().join(format!(
        "cc-guarded-probe-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;

    let mut wire = Vec::new();
    let outcome = (|| -> Result<(), String> {
        for bundle in BUNDLES {
            let out = dir.join(bundle);
            let out_arg = out.to_string_lossy().into_owned();
            let mut args = vec!["app-server", "generate-json-schema", "--out", &out_arg];
            if bundle == "experimental" {
                args.push("--experimental");
            }
            run(&args)?;
            let read = |path: std::path::PathBuf| -> Result<Value, String> {
                let raw = std::fs::read_to_string(&path)
                    .map_err(|e| format!("reading {}: {e}", path.display()))?;
                parse_schema(&raw).map_err(|e| format!("{}: {e}", path.display()))
            };
            let client_request = read(out.join("ClientRequest.json"))?;
            let client_notification = read(out.join("ClientNotification.json"))?;
            let server_request = read(out.join("ServerRequest.json"))?;
            // The response documents live under `v1/` or `v2/` depending on the method's
            // vintage; the projector asks for names, not paths.
            let mut results = BTreeMap::new();
            for name in guarded_result_types(&client_request).map_err(|e| e.to_string())? {
                let file = format!("{name}.json");
                let path = ["v2", "v1"]
                    .iter()
                    .map(|d| out.join(d).join(&file))
                    .find(|p| p.is_file())
                    .ok_or_else(|| format!("no {file} under {}", out.display()))?;
                results.insert(name, read(path)?);
            }
            let surface = project_bundle(&BundleDocs {
                client_request: &client_request,
                client_notification: &client_notification,
                server_request: &server_request,
                results: &results,
            })
            .map_err(|e| e.to_string())?;
            wire.push((bundle, surface));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    outcome?;
    Ok(ProjectedBinary { argv, wire })
}

/// Every difference an installed codex has from the admissible surface. Empty means this
/// build is one CodeConnect is grounded against.
///
/// The premise helper the live suites use, so "the gate admitted this build" is one
/// statement in one place rather than three copies of a version literal.
pub fn unadjudicated_against_baseline(codex: &std::path::Path) -> Result<Vec<String>, String> {
    let ProjectedBinary { argv, wire } = project_from_binary(codex)?;
    let mut out: Vec<String> = diff_argv(&admissible_argv(&argv), &argv)
        .iter()
        .map(ToString::to_string)
        .collect();
    for (bundle, surface) in wire {
        out.extend(
            diff_wire(&admissible_wire(bundle, &surface), &surface)
                .iter()
                .map(|c| format!("[{bundle}] {c}")),
        );
    }
    Ok(out)
}

/// One difference between the admissible surface and the installed binary, phrased so
/// the refusal names the thing an operator (or the next re-grounding) must look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceChange {
    /// A guarded method the reference has and the installed binary does not.
    ///
    /// **This is the subtle one.** A removal reads as "less surface", but the broker
    /// SENDS several of these fields to constrain codex — `cwd`,
    /// `runtimeWorkspaceRoots`, the captured-null pins. A constraint codex no longer
    /// understands is silently ignored, so the sandbox evaporates while nothing in the
    /// forward direction looks different. Removals refuse exactly like additions.
    MethodRemoved { method: String },
    /// A guarded method the installed binary has and the reference does not.
    MethodAdded { method: String },
    /// A top-level `params` field added on a guarded method.
    FieldAdded { method: String, field: String },
    /// A top-level `params` field removed from a guarded method.
    FieldRemoved { method: String, field: String },
    /// A guarded method whose shape differs below the top level (a type, an enum
    /// member, a nullability, the request envelope) — named with the part that moved.
    FieldChanged { method: String, field: String },
    /// A root subcommand the installed binary dispatches and the reference does not.
    /// The escape class: `validate_codex_argv`'s refusal table cannot know it.
    SubcommandAdded { name: String },
    /// A root subcommand the reference lists and the installed binary does not.
    SubcommandRemoved { name: String },
    /// A root flag the installed binary accepts and the reference does not.
    FlagAdded { name: String },
    /// A root flag the reference lists and the installed binary does not.
    FlagRemoved { name: String },
}

impl std::fmt::Display for SurfaceChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SurfaceChange::MethodRemoved { method } => {
                write!(f, "{method}: the method is gone from this build")
            }
            SurfaceChange::MethodAdded { method } => {
                write!(
                    f,
                    "{method}: a guarded method this reference does not describe"
                )
            }
            SurfaceChange::FieldAdded { method, field } => {
                write!(f, "{method} → params.{field}: added by this build")
            }
            SurfaceChange::FieldRemoved { method, field } => {
                write!(f, "{method} → params.{field}: removed by this build")
            }
            SurfaceChange::FieldChanged { method, field } => {
                write!(
                    f,
                    "{method} → params.{field}: its shape changed in this build"
                )
            }
            SurfaceChange::SubcommandAdded { name } => write!(
                f,
                "`codex {name}`: a subcommand this build dispatches that CodeConnect's \
                 refusal table does not know, so it would be forwarded as prompt text"
            ),
            SurfaceChange::SubcommandRemoved { name } => {
                write!(f, "`codex {name}`: no longer a subcommand in this build")
            }
            SurfaceChange::FlagAdded { name } => {
                write!(
                    f,
                    "`{name}`: a root flag this build accepts and the reference does not"
                )
            }
            SurfaceChange::FlagRemoved { name } => {
                write!(f, "`{name}`: no longer a root flag in this build")
            }
        }
    }
}

// ------------------------------------------------------ the adjudicated delta lists

/// What kind of difference from the baseline an entry describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Added,
    Removed,
    Shape,
}

/// The pseudo-bundle for the root CLI surface, which is not a schema bundle.
pub const ARGV_BUNDLE: &str = "argv";
/// The JSON pointer of a whole surface entry: RFC 6901's empty pointer, the document
/// root. Used by a delta that adds or removes an entire method.
pub const WHOLE_ENTRY: &str = "";

/// One reviewed difference a codex build is allowed to have from the 0.147 baseline's
/// **wire** surface.
///
/// # Why a bridge and not a second reference on its own
///
/// An exact-match gate against one reference admits exactly one build. CodeConnect has
/// to host two: 0.147 is what every live gate in this repo was proven on, and 0.153 is
/// what is installed. Re-vendoring to 0.153 alone would refuse 0.147 — its *absent*
/// `projectId` and `excludeTurns` read as removals — and tolerating removals as a class
/// is the one thing this gate must never do (see [`SurfaceChange::MethodRemoved`]).
///
/// So: two references, plus this list as the audited bridge between them. Each entry is
/// a **pointer** into the projected entry plus the fact that the value there may move
/// from the baseline's to the ceiling's. Nothing is keyed by version, nothing branches on
/// version, and there is no registry — an entry is a *fact about a difference*, and a
/// build either exhibits it exactly or does not exhibit it at all.
///
/// # What an entry costs, and what it does NOT admit
///
/// Each one is a widening of what CodeConnect will host, so each carries the measurement
/// that made it safe rather than an argument that it looks harmless. The discipline is
/// 2e-7c's: a new field is pinned to a captured shape, or refused, before it is listed
/// here. `verdict` is that sentence, and it is what a reviewer reads.
///
/// An entry admits **one value at one pointer**: the value the 0.153 reference carries
/// there. It does not admit "any shape for that field", "any change in that type", or
/// "any addition beside it" — [`admissible_wire`] splices the measured value in and then
/// the whole projection must match, so anything else left over refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireDelta {
    /// `"stable"` or `"experimental"`.
    pub bundle: &'static str,
    /// The surface key — `"request turn/start"`, `"notification initialized"`.
    pub key: &'static str,
    /// An RFC 6901 JSON pointer inside that key's entry, or [`WHOLE_ENTRY`] for the
    /// entry itself.
    pub at: &'static str,
    pub kind: DeltaKind,
    /// The measurement that makes this difference safe to admit. One sentence,
    /// naming what was driven through what.
    pub verdict: &'static str,
}

/// One reviewed difference a codex build is allowed to have from the 0.147 baseline's
/// **argv** surface. A root token has no interior, so it carries no pointer: it is
/// present or it is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgvDelta {
    /// The subcommand name or flag spelling. A leading `-` makes it a flag, exactly as
    /// [`project_argv`] classifies it.
    pub token: &'static str,
    pub kind: DeltaKind,
    pub verdict: &'static str,
}

/// Every adjudicated difference from the 0.147 argv baseline.
pub const ADJUDICATED_ARGV: &[ArgvDelta] = &[
    ArgvDelta {
        token: "agents",
        kind: DeltaKind::Added,
        verdict: "0.153 adds `codex agents` (browses tasks on the shared local app-server \
                  daemon). MEASURED dispatching on a real 0.153 binary while \
                  validate_codex_argv returned Ok(()) — it is now in is_subcommand's \
                  refusal table (the_subcommands_codex_0153_added_are_refused).",
    },
    ArgvDelta {
        token: "queue",
        kind: DeltaKind::Added,
        verdict: "0.153 adds `codex queue` (queues a message into ANOTHER session). \
                  Measured dispatching; refused by is_subcommand.",
    },
    ArgvDelta {
        token: "migrate-rollouts",
        kind: DeltaKind::Added,
        verdict: "0.153 adds `codex migrate-rollouts` (mutates session history with \
                  --apply). Measured dispatching; refused by is_subcommand.",
    },
    ArgvDelta {
        token: "--psp",
        kind: DeltaKind::Removed,
        verdict: "0.147's hidden global flag is gone in 0.153. ACCEPTED REMOVAL: nothing \
                  depends on it — it was only ever a token the refusal grammar had to \
                  classify, is_subcommand/known_long still refuse or classify it for \
                  older builds, and a flag codex no longer accepts cannot smuggle \
                  anything past a gate. Tokens are only ever added to the refusal table, \
                  never removed, so 0.147 keeps working.",
    },
];

/// Every adjudicated difference from the 0.147 wire baseline.
///
/// Adding a line here widens what CodeConnect hosts.
/// `every_adjudicated_delta_names_a_guarded_method`,
/// `every_adjudicated_delta_addresses_a_real_difference` and
/// `the_adjudicated_table_bridges_the_two_references` keep it honest.
pub const ADJUDICATED_WIRE: &[WireDelta] = &[
    // ---- turn/start: the four 0.153 additions and the types they pull in -----------
    WireDelta {
        bundle: "stable",
        key: "request turn/start",
        at: "/definitions/TurnStartParams/properties/serviceTierForTurn",
        kind: DeltaKind::Added,
        verdict: "MEASURED present-and-null on every turn of a real 0.153 TUI session; \
                  pinned absent-or-null by TURN_START_0153_NULL_PARAMS, populated \
                  refused (the_0153_turn_start_additions_are_pinned_absent_or_null).",
    },
    WireDelta {
        bundle: "stable",
        key: "request turn/start",
        at: "/definitions/TurnStartParams/properties/toolOutput",
        kind: DeltaKind::Added,
        verdict: "MEASURED present-and-null on a real 0.153 TUI turn; pinned \
                  absent-or-null, populated refused. It injects tool results into a \
                  turn, so a populated value is an unmeasured input channel.",
    },
    WireDelta {
        bundle: "stable",
        key: "request turn/start",
        at: "/definitions/TurnStartParams/properties/turnTrigger",
        kind: DeltaKind::Added,
        verdict: "MEASURED present-and-null on a real 0.153 TUI turn; pinned \
                  absent-or-null, populated refused.",
    },
    WireDelta {
        bundle: "stable",
        key: "request turn/start",
        at: "/definitions/TurnToolOutput",
        kind: DeltaKind::Added,
        verdict: "The type `toolOutput` references. Reachable ONLY through that field, \
                  which TURN_START_0153_NULL_PARAMS pins absent-or-null, so no value of \
                  this type can be sent at all.",
    },
    WireDelta {
        bundle: "stable",
        key: "request turn/start",
        at: "/definitions/FunctionCallOutputBody",
        kind: DeltaKind::Added,
        verdict: "Reachable only through TurnToolOutput.output, i.e. only through the \
                  pinned-null `toolOutput`. Unreachable on the admitted wire.",
    },
    WireDelta {
        bundle: "stable",
        key: "request turn/start",
        at: "/definitions/FunctionCallOutputContentItem",
        kind: DeltaKind::Added,
        verdict: "Reachable only through FunctionCallOutputBody, i.e. only through the \
                  pinned-null `toolOutput`. Unreachable on the admitted wire.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request turn/start",
        at: "/definitions/TurnStartParams/properties/serviceTierForTurn",
        kind: DeltaKind::Added,
        verdict: "Same field on the experimental bundle; same pin.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request turn/start",
        at: "/definitions/TurnStartParams/properties/toolOutput",
        kind: DeltaKind::Added,
        verdict: "Same field on the experimental bundle; same pin.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request turn/start",
        at: "/definitions/TurnStartParams/properties/turnTrigger",
        kind: DeltaKind::Added,
        verdict: "Same field on the experimental bundle; same pin.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request turn/start",
        at: "/definitions/TurnStartParams/properties/cyberAccessProgram",
        kind: DeltaKind::Added,
        verdict: "Experimental-bundle addition that the real 0.153 TUI nonetheless sends \
                  on the STABLE wire — MEASURED present-and-null. Pinned absent-or-null; \
                  its effect is entirely unmeasured, so a populated value refuses.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request turn/start",
        at: "/definitions/CyberAccessProgram",
        kind: DeltaKind::Added,
        verdict: "The enum `cyberAccessProgram` references, reachable only through that \
                  pinned-null field. Its three members are recorded here so a fourth \
                  would refuse.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request turn/start",
        at: "/definitions/TurnToolOutput",
        kind: DeltaKind::Added,
        verdict: "Same type on the experimental bundle; same unreachability.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request turn/start",
        at: "/definitions/FunctionCallOutputBody",
        kind: DeltaKind::Added,
        verdict: "Same type on the experimental bundle; same unreachability.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request turn/start",
        at: "/definitions/FunctionCallOutputContentItem",
        kind: DeltaKind::Added,
        verdict: "Same type on the experimental bundle; same unreachability.",
    },
    // ---- thread/resume and thread/fork ---------------------------------------------
    WireDelta {
        bundle: "stable",
        key: "request thread/resume",
        at: "/definitions/ThreadResumeParams/properties/excludeTurns",
        kind: DeltaKind::Added,
        verdict: "Already inside the exhaustive THREAD_RESUME_CAPTURED_PARAMS set (it \
                  existed in 0.147's experimental bundle), so a populated value was \
                  already refused before this delta; the delta records that the STABLE \
                  bundle now carries it too. Not sent on turn/start (measured absent).",
    },
    WireDelta {
        bundle: "stable",
        key: "request thread/fork",
        at: "/definitions/ThreadForkParams/properties/excludeTurns",
        kind: DeltaKind::Added,
        verdict: "thread/fork is refused OUTRIGHT in the executor pre-2e-4c (its \
                  source-thread lineage is unprovable with no captured fork frame), so \
                  no field on it can be reached at all. Shape change only.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request thread/resume",
        at: "/definitions/ResponseItem/oneOf",
        kind: DeltaKind::Shape,
        verdict: "The `function_call_output` variant of `ResponseItem` widens: `call_id` \
                  goes string -> [string,null] AND leaves `required`, and nullable \
                  `name`/`namespace` are added. Reachable from thread/resume only through \
                  `history`, which THREAD_RESUME_CAPTURED_NULL_PARAMS pins absent-or-null \
                  — a populated history mints a NEW thread and defeats the resume binding \
                  — so no value of this type is reachable and none of it can be \
                  exercised. The whole `oneOf` is pinned here, so a change to any OTHER \
                  variant of the same type still refuses.",
    },
    // ---- thread/start ---------------------------------------------------------------
    WireDelta {
        bundle: "experimental",
        key: "request thread/start",
        at: "/definitions/ThreadStartParams/properties/projectId",
        kind: DeltaKind::Added,
        verdict: "MEASURED null on the real 0.153 TUI's creation, and pinned to captured \
                  null by THREAD_START_0153_NULL_PARAMS inside the exhaustive top-level \
                  key union THREAD_START_CAPTURED_PARAMS now enforces.",
    },
    // ---- RESULTS: the answers the guarded methods return ----------------------------
    //
    // 0.153 grew codex's thread-item, error and metadata vocabulary considerably, and the
    // whole `/result` subtree of each method is pinned to the measured shape rather than
    // to a coordinate — so a change beyond what was measured still refuses. Each verdict
    // says what the broker does with that method's result, because that is what decides
    // whether a shape change there is admissible.
    WireDelta {
        bundle: "stable",
        key: "request account/read",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "A bootstrap read the broker forwards byte-exact and constrains nothing in. Its \
                  result reaches no rule here: no field of it is read, rewritten or \
                  bound. 0.153 grew the account/plan vocabulary; the whole result subtree is \
                  pinned to the measured shape, so a change beyond it still refuses.",
    },
    WireDelta {
        bundle: "stable",
        key: "request hooks/list",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "A bootstrap read the broker forwards byte-exact and constrains nothing in. Its \
                  result reaches no rule here: no field of it is read, rewritten or \
                  bound. 0.153 grew the hook-metadata vocabulary; the whole result subtree is \
                  pinned to the measured shape, so a change beyond it still refuses.",
    },
    WireDelta {
        bundle: "stable",
        key: "request model/list",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "A bootstrap read the broker forwards byte-exact and constrains nothing in. Its \
                  result reaches no rule here: no field of it is read, rewritten or \
                  bound. 0.153 grew the model-metadata vocabulary; the whole result subtree is \
                  pinned to the measured shape, so a change beyond it still refuses.",
    },
    WireDelta {
        bundle: "stable",
        key: "request skills/list",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "A bootstrap read the broker forwards byte-exact and constrains nothing in. Its \
                  result reaches no rule here: no field of it is read, rewritten or \
                  bound. 0.153 grew the skill-metadata vocabulary; the whole result subtree is \
                  pinned to the measured shape, so a change beyond it still refuses.",
    },
    WireDelta {
        bundle: "stable",
        key: "request thread/fork",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "thread/fork is refused OUTRIGHT in the executor pre-2e-4c, so no result is ever \
                  produced for it. Pinned anyway, because the day the refusal lifts the \
                  shape must already have been measured rather than discovered.",
    },
    WireDelta {
        bundle: "stable",
        key: "request thread/read",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "The 0.153 thread-item vocabulary. WHOSE content this returns is decided on the \
                  REQUEST — Disposition::ReadSessionThread binds params.threadId to a \
                  session thread — so a richer item shape carries more ABOUT THIS \
                  SESSION'S OWN thread, never another's. The result envelope itself is \
                  byte-identical between the two releases; only nested item types moved.",
    },
    WireDelta {
        bundle: "stable",
        key: "request thread/resume",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict:
            "The attach answer, read by the same binding and unchanged in every field it reads.",
    },
    WireDelta {
        bundle: "stable",
        key: "request thread/start",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "The creation answer the session binding reads. What it reads is unchanged and \
                  still required: result.thread.id, and the top-level cwd / \
                  runtimeWorkspaceRoots the workspace anchor compares — the result \
                  envelope's own `required` list is byte-identical between the releases. \
                  The movement is inside Thread and the item vocabulary, and Thread's \
                  `required` only GAINED projectId.",
    },
    WireDelta {
        bundle: "stable",
        key: "request turn/start",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "The turn answer. The broker forwards it byte-exact; the head-check acts on the \
                  REQUEST. 0.153 grew the item and error vocabulary.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request account/read",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "A bootstrap read the broker forwards byte-exact and constrains nothing in. Its \
                  result reaches no rule here: no field of it is read, rewritten or \
                  bound. 0.153 grew the account/plan vocabulary; the whole result subtree is \
                  pinned to the measured shape, so a change beyond it still refuses.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request hooks/list",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "A bootstrap read the broker forwards byte-exact and constrains nothing in. Its \
                  result reaches no rule here: no field of it is read, rewritten or \
                  bound. 0.153 grew the hook-metadata vocabulary; the whole result subtree is \
                  pinned to the measured shape, so a change beyond it still refuses.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request model/list",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "A bootstrap read the broker forwards byte-exact and constrains nothing in. Its \
                  result reaches no rule here: no field of it is read, rewritten or \
                  bound. 0.153 grew the model-metadata vocabulary; the whole result subtree is \
                  pinned to the measured shape, so a change beyond it still refuses.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request skills/list",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "A bootstrap read the broker forwards byte-exact and constrains nothing in. Its \
                  result reaches no rule here: no field of it is read, rewritten or \
                  bound. 0.153 grew the skill-metadata vocabulary; the whole result subtree is \
                  pinned to the measured shape, so a change beyond it still refuses.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request thread/fork",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "thread/fork is refused OUTRIGHT in the executor pre-2e-4c, so no result is ever \
                  produced for it. Pinned anyway, because the day the refusal lifts the \
                  shape must already have been measured rather than discovered.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request thread/items/list",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "Same vocabulary and same binding as thread/turns/list.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request thread/read",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "The 0.153 thread-item vocabulary. WHOSE content this returns is decided on the \
                  REQUEST — Disposition::ReadSessionThread binds params.threadId to a \
                  session thread — so a richer item shape carries more ABOUT THIS \
                  SESSION'S OWN thread, never another's. The result envelope itself is \
                  byte-identical between the two releases; only nested item types moved.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request thread/resume",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict:
            "The attach answer, read by the same binding and unchanged in every field it reads.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request thread/start",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "The creation answer the session binding reads. What it reads is unchanged and \
                  still required: result.thread.id, and the top-level cwd / \
                  runtimeWorkspaceRoots the workspace anchor compares — the result \
                  envelope's own `required` list is byte-identical between the releases. \
                  The movement is inside Thread and the item vocabulary, and Thread's \
                  `required` only GAINED projectId.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request thread/turns/list",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict:
            "Same vocabulary, same binding: thread/turns/list is bound to a session thread, so \
                  the shape change cannot widen whose turns are returned. This is one of \
                  the two methods MEASURED leaking another session's items before the \
                  binding existed, which is why its result is inside the gate at all.",
    },
    WireDelta {
        bundle: "experimental",
        key: "request turn/start",
        at: "/result",
        kind: DeltaKind::Shape,
        verdict: "The turn answer. The broker forwards it byte-exact; the head-check acts on the \
                  REQUEST. 0.153 grew the item and error vocabulary.",
    },
    WireDelta {
        bundle: "stable",
        key: "server-request item/commandExecution/requestApproval",
        at: "/definitions/CommandExecutionApprovalKind",
        kind: DeltaKind::Added,
        verdict: "The enum 0.153's command-execution approval adds. The broker neither reads nor \
                  rewrites approval params — it relays the request byte-exact and grants a \
                  one-use response capability, which is unchanged in kind and in grant.",
    },
    WireDelta {
        bundle: "stable",
        key: "server-request item/commandExecution/requestApproval",
        at: "/definitions/CommandExecutionRequestApprovalParams/properties/kind",
        kind: DeltaKind::Added,
        verdict:
            "The field that references it. Same reasoning: relayed verbatim to the client that \
                  renders it, and the capability it grants is unchanged.",
    },
    WireDelta {
        bundle: "stable",
        key: "server-request mcpServer/elicitation/request",
        at: "/definitions/McpServerElicitationRequestParams/oneOf",
        kind: DeltaKind::Shape,
        verdict: "An s2c request this broker does not admit: it is answered UPSTREAM with a \
                  synthetic error and never delivered (see crate::relay), so no value of \
                  this shape reaches a client and none can be exercised.",
    },
    WireDelta {
        bundle: "experimental",
        key: "server-request item/commandExecution/requestApproval",
        at: "/definitions/CommandExecutionApprovalKind",
        kind: DeltaKind::Added,
        verdict: "The enum 0.153's command-execution approval adds. The broker neither reads nor \
                  rewrites approval params — it relays the request byte-exact and grants a \
                  one-use response capability, which is unchanged in kind and in grant.",
    },
    WireDelta {
        bundle: "experimental",
        key: "server-request item/commandExecution/requestApproval",
        at: "/definitions/CommandExecutionRequestApprovalParams/properties/kind",
        kind: DeltaKind::Added,
        verdict:
            "The field that references it. Same reasoning: relayed verbatim to the client that \
                  renders it, and the capability it grants is unchanged.",
    },
    WireDelta {
        bundle: "experimental",
        key: "server-request mcpServer/elicitation/request",
        at: "/definitions/McpServerElicitationRequestParams/oneOf",
        kind: DeltaKind::Shape,
        verdict: "An s2c request this broker does not admit: it is answered UPSTREAM with a \
                  synthetic error and never delivered (see crate::relay), so no value of \
                  this shape reaches a client and none can be exercised.",
    },
    // ---- SERVER→CLIENT requests ------------------------------------------------------
    // ---- the two newly-stable list methods -------------------------------------------
    WireDelta {
        bundle: "stable",
        key: "request thread/turns/list",
        at: WHOLE_ENTRY,
        kind: DeltaKind::Added,
        verdict: "Existed in 0.147's experimental bundle; 0.153 promotes it to stable. \
                  Bound like thread/read (Disposition::ReadSessionThread) — MEASURED \
                  leaking another session's turn items to the model via the 0.153 \
                  read_thread tool before that binding existed. The whole entry is pinned, \
                  so a promoted method carrying an extra thread selector refuses.",
    },
    WireDelta {
        bundle: "stable",
        key: "request thread/items/list",
        at: WHOLE_ENTRY,
        kind: DeltaKind::Added,
        verdict: "Existed in 0.147's experimental bundle; 0.153 promotes it to stable. \
                  Bound like thread/read (Disposition::ReadSessionThread). The whole entry \
                  is pinned, so a promoted method carrying an extra thread selector \
                  refuses.",
    },
];

// ------------------------------------------------------ building the admissible surface

/// Follow an RFC 6901 pointer, returning `None` if any step is missing.
///
/// Only the two escapes RFC 6901 defines (`~1` for `/`, `~0` for `~`) are honoured; the
/// pointers in [`ADJUDICATED_WIRE`] address schema keys, and a schema key containing
/// either character would otherwise be unaddressable.
fn pointer<'a>(root: &'a Value, at: &str) -> Option<&'a Value> {
    if at.is_empty() {
        return Some(root);
    }
    root.pointer(at)
}

/// Splice `value` in at `at`, creating nothing: every parent step must already exist.
/// Returns whether the splice happened.
fn splice(root: &mut Value, at: &str, value: Value) -> bool {
    if at.is_empty() {
        *root = value;
        return true;
    }
    match root.pointer_mut(at) {
        Some(slot) => {
            *slot = value;
            true
        }
        None => {
            // The leaf does not exist yet — that is the ADDED case. Address the parent
            // and insert the last segment into it.
            let (parent, last) = match at.rsplit_once('/') {
                Some(split) => split,
                None => return false,
            };
            let last = last.replace("~1", "/").replace("~0", "~");
            match root.pointer_mut(parent).and_then(Value::as_object_mut) {
                Some(obj) => {
                    obj.insert(last, value);
                    true
                }
                None => false,
            }
        }
    }
}

/// Remove whatever is at `at`. Returns whether something was removed.
fn unsplice(root: &mut Value, at: &str) -> bool {
    let Some((parent, last)) = at.rsplit_once('/') else {
        return false;
    };
    let last = last.replace("~1", "/").replace("~0", "~");
    root.pointer_mut(parent)
        .and_then(Value::as_object_mut)
        .map(|obj| obj.remove(&last).is_some())
        .unwrap_or(false)
}

/// The surface `installed` is allowed to be: the 0.147 baseline with exactly those
/// adjudicated deltas this build **exhibits at their measured value** spliced in.
///
/// A delta is exhibited when the installed projection carries, at the delta's pointer,
/// precisely what the 0.153 grounded reference carries there (or, for a removal, carries
/// nothing). Anything else — a different shape at that pointer, a partial adoption, an
/// addition beside it — is simply not spliced, so the caller's exact comparison against
/// this surface reports it.
///
/// This is the whole of the adjudication mechanism. There is no filtering step after the
/// diff: what the diff finds is what refuses.
pub fn admissible_wire(bundle: &str, installed: &GuardedSurface) -> GuardedSurface {
    let mut out = baseline_wire(bundle);
    let grounded = grounded_wire(bundle);
    for d in ADJUDICATED_WIRE.iter().filter(|d| d.bundle == bundle) {
        match d.kind {
            DeltaKind::Added | DeltaKind::Shape => {
                let Some(want) = grounded.get(d.key).and_then(|e| pointer(e, d.at)) else {
                    // A delta the grounded reference does not describe covers nothing;
                    // `every_adjudicated_delta_addresses_a_real_difference` fails the
                    // build on it. At runtime it simply admits nothing extra.
                    continue;
                };
                let exhibited = installed.get(d.key).and_then(|e| pointer(e, d.at)) == Some(want);
                if !exhibited {
                    continue;
                }
                if d.at.is_empty() {
                    out.insert(d.key.to_string(), want.clone());
                } else if let Some(entry) = out.get_mut(d.key) {
                    splice(entry, d.at, want.clone());
                }
            }
            DeltaKind::Removed => {
                let gone = match installed.get(d.key) {
                    None => true,
                    Some(entry) => pointer(entry, d.at).is_none(),
                };
                if !gone {
                    continue;
                }
                if d.at.is_empty() {
                    out.remove(d.key);
                } else if let Some(entry) = out.get_mut(d.key) {
                    unsplice(entry, d.at);
                }
            }
        }
    }
    out
}

/// The root argv surface `installed` is allowed to be: the 0.147 baseline with exactly
/// those adjudicated tokens this build exhibits.
///
/// A token has no interior, so "exhibited at its measured value" is just "present" (or,
/// for a removal, "absent"). The comparison after this is still exact.
pub fn admissible_argv(installed: &ArgvSurface) -> ArgvSurface {
    let mut out = baseline_argv();
    for d in ADJUDICATED_ARGV {
        let token = d.token.to_string();
        let set = if d.token.starts_with('-') {
            &mut out.flags
        } else {
            &mut out.subcommands
        };
        let present = if d.token.starts_with('-') {
            installed.flags.contains(&token)
        } else {
            installed.subcommands.contains(&token)
        };
        match d.kind {
            DeltaKind::Added if present => {
                set.insert(token);
            }
            DeltaKind::Removed if !present => {
                set.remove(&token);
            }
            // `Shape` is meaningless for a token and is rejected by
            // `every_argv_delta_is_a_presence_fact`; a non-exhibited delta splices
            // nothing.
            _ => {}
        }
    }
    out
}

/// Compare an admissible wire surface against a projected one.
///
/// Empty means identical, which is the only verdict that admits.
pub fn diff_wire(admissible: &GuardedSurface, installed: &GuardedSurface) -> Vec<SurfaceChange> {
    let mut changes = Vec::new();
    for method in admissible.keys() {
        if !installed.contains_key(method) {
            changes.push(SurfaceChange::MethodRemoved {
                method: method.clone(),
            });
        }
    }
    for method in installed.keys() {
        if !admissible.contains_key(method) {
            changes.push(SurfaceChange::MethodAdded {
                method: method.clone(),
            });
        }
    }
    for (method, want) in admissible {
        let Some(got) = installed.get(method) else {
            continue;
        };
        if want == got {
            continue;
        }
        changes.extend(diff_entry(method, want, got));
    }
    changes
}

/// The name of the definition a `params` node references, when it is a bare `$ref`.
fn params_type(entry: &Value) -> Option<&str> {
    entry
        .get("params")?
        .get("$ref")?
        .as_str()
        .map(|r| r.rsplit('/').next().unwrap_or(r))
}

/// One object with its `properties` removed, so two nodes can be compared on everything
/// *except* the named fields that are diffed separately.
fn without_properties(node: Option<&Value>) -> Value {
    let mut v = node.cloned().unwrap_or(Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.remove("properties");
    }
    v
}

/// The named `properties` of a method's params — direct, or one `$ref` hop away.
///
/// `params` is usually a `$ref` into `definitions` (that is the shape codex emits), so
/// the named fields are one hop away. Following that hop is what keeps a refusal able to
/// say `request turn/start → params.toolOutput` instead of "something changed".
fn params_props(entry: &Value) -> Map<String, Value> {
    let params = entry.get("params");
    if let Some(direct) = params
        .and_then(|p| p.get("properties"))
        .and_then(Value::as_object)
    {
        return direct.clone();
    }
    params_type(entry)
        .and_then(|name| entry.get("definitions").and_then(|d| d.get(name)))
        .and_then(|t| t.get("properties"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Name every way one method's entry differs — **exhaustively**.
///
/// The four places a difference can live are all reported, and none of them short-circuit
/// the others. That matters: an adjudicated field addition sitting beside an
/// unadjudicated `required` move or a changed nested type used to hide the second one,
/// because the definition sweep only ran when the field sweep found nothing.
///
/// Reporting is non-duplicating: the params type's own `properties` are reported as named
/// fields, and that same definition is then compared with `properties` removed, so a
/// field change is named once rather than twice.
fn diff_entry(method: &str, want: &Value, got: &Value) -> Vec<SurfaceChange> {
    let mut changes = Vec::new();
    let named = |field: &str| SurfaceChange::FieldChanged {
        method: method.to_string(),
        field: field.to_string(),
    };

    // (1) The named params fields.
    let (w, g) = (params_props(want), params_props(got));
    for k in g.keys() {
        if !w.contains_key(k) {
            changes.push(SurfaceChange::FieldAdded {
                method: method.to_string(),
                field: k.clone(),
            });
        }
    }
    for (k, wv) in &w {
        match g.get(k) {
            None => changes.push(SurfaceChange::FieldRemoved {
                method: method.to_string(),
                field: k.clone(),
            }),
            Some(gv) if gv != wv => changes.push(named(k)),
            Some(_) => {}
        }
    }

    // (2) The params node itself apart from those fields — an inline `required`, an
    // `additionalProperties`, or a `$ref` that now names a different type.
    if without_properties(want.get("params")) != without_properties(got.get("params")) {
        changes.push(named("(the params envelope)"));
    }

    // (3) The reachable definitions. The params type is compared without its
    // `properties`, which (1) already covered; every other type is compared whole.
    let defs = |v: &Value| {
        v.get("definitions")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    let (wd, gd) = (defs(want), defs(got));
    let pt = params_type(want).map(str::to_string);
    let names: BTreeSet<&String> = wd.keys().chain(gd.keys()).collect();
    for name in names {
        if wd.get(name) == gd.get(name) {
            continue;
        }
        if Some(name.as_str()) == pt.as_deref() {
            if without_properties(wd.get(name)) != without_properties(gd.get(name)) {
                changes.push(named("(params metadata, e.g. `required`)"));
            }
        } else {
            changes.push(named(&format!("(type {name})")));
        }
    }

    // (4) The JSON-RPC envelope: whether `params` may be absent at all, the `id` type,
    // the method constant. A method whose params became optional is a shape change the
    // broker's absence rules depend on.
    if want.get("envelope") != got.get("envelope") {
        changes.push(named("(the JSON-RPC envelope, e.g. `required`)"));
    }

    // (5) The backstop. `diff_wire` only calls this when the entries differ, so an empty
    // result here would be a difference with no name — which would admit. It must not be
    // possible, and if it ever is, it refuses loudly rather than silently.
    if changes.is_empty() {
        changes.push(named(
            "(this method's entry, in a way this diff cannot name)",
        ));
    }
    changes
}

/// Compare an admissible argv surface against a projected one.
///
/// **Additions and removals both refuse, and the reasons differ.** An added subcommand
/// is the escape class — `validate_codex_argv` would forward it as prompt text and
/// codex would dispatch it. A removed one, or any flag movement, means the vendored
/// reference no longer describes this binary; the refusal table may then be refusing
/// tokens that no longer exist and, more to the point, CodeConnect cannot claim to have
/// been grounded against a CLI it has not seen. Both are re-groundings.
pub fn diff_argv(admissible: &ArgvSurface, installed: &ArgvSurface) -> Vec<SurfaceChange> {
    let mut changes = Vec::new();
    for name in installed.subcommands.difference(&admissible.subcommands) {
        changes.push(SurfaceChange::SubcommandAdded { name: name.clone() });
    }
    for name in admissible.subcommands.difference(&installed.subcommands) {
        changes.push(SurfaceChange::SubcommandRemoved { name: name.clone() });
    }
    for name in installed.flags.difference(&admissible.flags) {
        changes.push(SurfaceChange::FlagAdded { name: name.clone() });
    }
    for name in admissible.flags.difference(&installed.flags) {
        changes.push(SurfaceChange::FlagRemoved { name: name.clone() });
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An entry in the projection's shape.
    fn entry(params: Value, defs: Value) -> Value {
        json!({"envelope": {}, "params": params, "definitions": defs})
    }

    /// A one-variant ClientNotification document with nothing guarded in it, for the
    /// request-only tests.
    fn no_notifications() -> Value {
        json!({"oneOf": []})
    }

    /// Project a hand-written pair of client documents, with no server requests and no
    /// result documents — the shape most of these tests care about.
    fn project_only(
        requests: &Value,
        notifications: &Value,
    ) -> Result<GuardedSurface, ProjectionError> {
        let none = json!({"oneOf": []});
        let results = BTreeMap::new();
        project_bundle(&BundleDocs {
            client_request: requests,
            client_notification: notifications,
            server_request: &none,
            results: &results,
        })
    }

    #[test]
    fn guarded_is_exactly_what_the_allowlist_does_not_refuse() {
        // Spot the two ends rather than restating the table: a forwarded read and an
        // ownership-carrying write are guarded; a code-exec bypass and an unknown
        // future method are not.
        assert!(is_guarded("turn/start"));
        assert!(is_guarded("thread/start"));
        assert!(is_guarded("thread/items/list"));
        assert!(is_guarded("initialized"));
        assert!(!is_guarded("command/exec"));
        assert!(!is_guarded("thread/shellCommand"));
        assert!(!is_guarded("thread/settings/update"));
        assert!(!is_guarded("future/method/nobody/pinned"));
    }

    /// Guarding is per KIND. `initialized` is admitted as a notification and refused as
    /// a request, so the request bundle must not carry it and the notification bundle
    /// must.
    #[test]
    fn guarding_is_scoped_to_the_json_rpc_kind() {
        assert!(is_guarded_as(JsonRpcKind::Notification, "initialized"));
        assert!(!is_guarded_as(JsonRpcKind::Request, "initialized"));
        assert!(is_guarded_as(JsonRpcKind::Request, "turn/start"));
        assert!(!is_guarded_as(JsonRpcKind::Notification, "turn/start"));
    }

    /// A refused method changing shape must NOT enter the projection — that freedom is
    /// the reason this gate can admit a new codex at all.
    #[test]
    fn a_refused_method_is_not_projected() {
        let doc = json!({
            "oneOf": [
                {"properties": {"method": {"enum": ["command/exec"]},
                                "params": {"properties": {"cmd": {"type": "string"}}}}},
                {"properties": {"method": {"enum": ["thread/read"]},
                                "params": {"properties": {"id": {"type": "string"}}}}}
            ]
        });
        let s = project_only(&doc, &no_notifications()).unwrap();
        assert!(s.contains_key("request thread/read"));
        assert!(!s.contains_key("request command/exec"));
    }

    /// The admitted NOTIFICATION is inside the gate, keyed apart from any request of the
    /// same name.
    #[test]
    fn the_admitted_notification_is_projected_and_keyed_by_kind() {
        let notifications = json!({
            "oneOf": [
                {"properties": {"method": {"enum": ["initialized"]}},
                 "required": ["method"], "type": "object"},
                {"properties": {"method": {"enum": ["some/other"]},
                                "params": {"properties": {"x": {"type": "string"}}}}}
            ]
        });
        let requests = json!({
            "oneOf": [{"properties": {"method": {"enum": ["thread/read"]},
                                      "params": {"properties": {"threadId": {"type": "string"}}}},
                       "required": ["id", "method", "params"]}]
        });
        let s = project_only(&requests, &notifications).unwrap();
        assert!(s.contains_key("notification initialized"));
        assert!(s.contains_key("request thread/read"));
        // A notification the allowlist refuses contributes nothing.
        assert!(!s.contains_key("notification some/other"));
        // No params on the wire is a real, comparable shape.
        assert_eq!(s["notification initialized"]["params"], Value::Null);
        // The envelope is kept: `initialized` does not carry params at all.
        assert_eq!(
            s["notification initialized"]["envelope"]["required"],
            json!(["method"])
        );
    }

    /// **The projector carries the server side and the results, not only the requests.**
    ///
    /// Asserted against the PROJECTOR rather than against the vendored files, and that
    /// distinction is the point: the references are this function's own output, so a
    /// projector that stopped emitting a half would still compare equal to a reference
    /// regenerated from it. Only the live re-derivation would notice, and only when a real
    /// binary is present. This makes both halves provable offline.
    #[test]
    fn the_projection_carries_the_server_side_and_the_results() {
        let requests = json!({
            "oneOf": [{"properties": {"method": {"enum": ["thread/read"]},
                                      "params": {"$ref": "#/definitions/ThreadReadParams"}}}],
            "definitions": {"ThreadReadParams": {"properties": {"threadId": {"type": "string"}}}}
        });
        let server = json!({
            "oneOf": [{"properties": {"method": {"enum": ["item/tool/call"]},
                                      "params": {"type": "object"}},
                       "required": ["id", "method", "params"]}]
        });
        let results = [(
            "ThreadReadResponse".to_string(),
            json!({"$schema": "x", "properties": {"thread": {"$ref": "#/definitions/Thread"}},
                   "definitions": {"Thread": {"properties": {"id": {"type": "string"}}}}}),
        )]
        .into_iter()
        .collect();
        let s = project_bundle(&BundleDocs {
            client_request: &requests,
            client_notification: &json!({"oneOf": []}),
            server_request: &server,
            results: &results,
        })
        .unwrap();

        // The server→client request is projected, under its own key namespace.
        let server_entry = s
            .get("server-request item/tool/call")
            .expect("every server request is projected");
        assert_eq!(
            server_entry["envelope"]["required"],
            json!(["id", "method", "params"])
        );

        // The guarded method's RESULT rides in its entry, with its own reachable
        // definitions kept apart from the request's.
        let read = &s["request thread/read"];
        assert_eq!(
            read["result"]["schema"],
            json!({"properties": {"thread": {"$ref": "#/definitions/Thread"}}}),
            "the result schema is projected (and `$schema` dropped)"
        );
        assert_eq!(
            read["result"]["definitions"]["Thread"],
            json!({"properties": {"id": {"type": "string"}}})
        );
        // A server request carries no result: the direction has none.
        assert!(server_entry.get("result").is_none());
    }

    /// A guarded method whose `<X>Response` was not supplied REFUSES — it does not project
    /// a method with no result and call that a comparable shape.
    #[test]
    fn a_missing_result_document_refuses() {
        let requests = json!({
            "oneOf": [{"properties": {"method": {"enum": ["thread/read"]},
                                      "params": {"$ref": "#/definitions/ThreadReadParams"}}}],
            "definitions": {"ThreadReadParams": {"properties": {"threadId": {"type": "string"}}}}
        });
        assert_eq!(
            project_bundle(&BundleDocs {
                client_request: &requests,
                client_notification: &json!({"oneOf": []}),
                server_request: &json!({"oneOf": []}),
                results: &BTreeMap::new(),
            }),
            Err(ProjectionError::MissingResult("ThreadReadResponse".into()))
        );
        // …and the two methods whose params are `{"type": "null"}` ask for no result at
        // all, which is the measured exception rather than a silent skip.
        let null_params = json!({
            "oneOf": [{"properties": {"method": {"enum": ["account/rateLimits/read"]},
                                      "params": {"type": "null"}}}]
        });
        assert!(guarded_result_types(&null_params).unwrap().is_empty());
        let s = project_bundle(&BundleDocs {
            client_request: &null_params,
            client_notification: &json!({"oneOf": []}),
            server_request: &json!({"oneOf": []}),
            results: &BTreeMap::new(),
        })
        .unwrap();
        assert_eq!(s["request account/rateLimits/read"]["result"], Value::Null);
    }

    /// The two methods with no mechanical route to their result are EXACTLY the two
    /// measured ones, in both references — so a third appearing is a build failure rather
    /// than a quietly ungated result.
    #[test]
    fn the_methods_with_no_projectable_result_are_exactly_the_two_measured() {
        for load in [
            baseline_wire as fn(&str) -> GuardedSurface,
            grounded_wire as fn(&str) -> GuardedSurface,
        ] {
            for bundle in BUNDLES {
                let unmapped: BTreeSet<String> = load(bundle)
                    .iter()
                    .filter(|(k, e)| {
                        k.starts_with("request ") && e.get("result") == Some(&Value::Null)
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                assert_eq!(
                    unmapped,
                    [
                        "request account/rateLimits/read".to_string(),
                        "request configRequirements/read".to_string()
                    ]
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
                    "{bundle}: the schema gives no `<X>Params` for these, so their results \
                     are outside the gate — a recorded gap, and it must stay exactly these two"
                );
            }
        }
    }

    /// Two variants claiming one method must REFUSE, never overwrite. A permissive
    /// variant followed by a baseline-shaped one would otherwise compare equal to the
    /// baseline while the permissive shape is what the server accepts.
    #[test]
    fn a_duplicate_method_variant_refuses() {
        let doc = json!({
            "oneOf": [
                {"properties": {"method": {"enum": ["thread/read"]},
                                "params": {"properties": {"anything": true}}}},
                {"properties": {"method": {"enum": ["thread/read"]},
                                "params": {"properties": {"threadId": {"type": "string"}}}}}
            ]
        });
        assert_eq!(
            project_only(&doc, &no_notifications()),
            Err(ProjectionError::DuplicateVariant(
                "request thread/read".into()
            ))
        );
    }

    #[test]
    fn refs_resolve_and_docs_are_dropped() {
        let doc = json!({
            "definitions": {"P": {"description": "prose", "properties": {"a": {"type": "string"}}}},
            "oneOf": [{"properties": {"method": {"enum": ["thread/read"], "title": "x"},
                                      "params": {"$ref": "#/definitions/P"}}}]
        });
        let s = project_only(&doc, &no_notifications()).unwrap();
        // The `$ref` is KEPT and the definition it names is carried alongside, with
        // documentation stripped from both. See `strip`.
        assert_eq!(
            s["request thread/read"]["params"],
            json!({"$ref": "#/definitions/P"})
        );
        assert_eq!(
            s["request thread/read"]["definitions"]["P"],
            json!({"properties": {"a": {"type": "string"}}})
        );
    }

    /// **A wire field NAMED `description` or `title` is not documentation.**
    ///
    /// Measured on 0.147's `DynamicToolNamespaceTool`: its function variant requires a
    /// field literally called `description`, of type `string`. A context-blind strip
    /// deleted it from `properties` while leaving it in `required`, so the gate could not
    /// have seen it change. The same applies to a literal `default` or `enum` member that
    /// happens to be an object with those keys.
    #[test]
    fn a_wire_field_named_like_an_annotation_survives_the_strip() {
        let doc = json!({
            "definitions": {"P": {
                "title": "prose",
                "properties": {
                    "description": {"type": "string", "description": "prose about it"},
                    "title": {"type": "string"},
                    "mode": {"default": {"title": "kept", "description": "kept"},
                             "enum": [{"title": "kept"}, "plain"]}
                },
                "required": ["description", "title"]
            }},
            "oneOf": [{"properties": {"method": {"enum": ["thread/read"]},
                                      "params": {"$ref": "#/definitions/P"}}}]
        });
        let p = &project_only(&doc, &no_notifications()).unwrap()["request thread/read"]
            ["definitions"]["P"];
        // The FIELD NAMES survive; the annotation ON each field does not.
        assert_eq!(p["properties"]["description"], json!({"type": "string"}));
        assert_eq!(p["properties"]["title"], json!({"type": "string"}));
        // `required` still names them, and now it names things the projection has.
        assert_eq!(p["required"], json!(["description", "title"]));
        // The definition's OWN `title` is an annotation and is gone.
        assert!(p.get("title").is_none());
        // Literal data is copied byte for byte, keys included.
        assert_eq!(
            p["properties"]["mode"]["default"],
            json!({"title": "kept", "description": "kept"})
        );
        assert_eq!(
            p["properties"]["mode"]["enum"],
            json!([{"title": "kept"}, "plain"])
        );
    }

    /// The schema is recursive; a naive inliner would not terminate.
    #[test]
    fn a_recursive_ref_terminates_with_a_stable_marker() {
        let doc = json!({
            "definitions": {"Item": {"properties": {"child": {"$ref": "#/definitions/Item"}}}},
            "oneOf": [{"properties": {"method": {"enum": ["thread/read"]},
                                      "params": {"$ref": "#/definitions/Item"}}}]
        });
        let s = project_only(&doc, &no_notifications()).unwrap();
        // Nothing is expanded, so a self-referential type is just carried once —
        // recursion stopped being a special case when inlining went away.
        assert_eq!(
            s["request thread/read"]["definitions"]["Item"],
            json!({"properties": {"child": {"$ref": "#/definitions/Item"}}})
        );
    }

    /// **A schema document with a duplicate member is refused, not normalised.**
    ///
    /// `serde_json` accepts a duplicate object member and keeps one of the two values, so
    /// two documents that genuinely differ can collapse to the same projected `Value` —
    /// and the gate's verdict IS an equality of projected values. That is the same
    /// parser-differential the c2s classifier rejects on the wire, and it is rejected here
    /// for the same reason: a document whose meaning depends on which duplicate a parser
    /// keeps has no single meaning to compare.
    #[test]
    fn a_duplicate_member_in_a_schema_document_is_refused() {
        // The two collapse to the same `Value` under a permissive parser, and to two
        // different projections under an honest reading — which is the whole hazard.
        let dup = r#"{"oneOf":[{"properties":{"method":{"enum":["thread/read"]},
                     "params":{"type":"string"},"params":{"type":"object"}}}]}"#;
        assert!(
            matches!(parse_schema(dup), Err(ProjectionError::Malformed(_))),
            "a duplicate member must refuse"
        );
        // Nested, at any depth.
        let nested = r#"{"oneOf":[{"properties":{"method":{"enum":["x"],"enum":["y"]}}}]}"#;
        assert!(matches!(
            parse_schema(nested),
            Err(ProjectionError::Malformed(_))
        ));
        // …and an ordinary document still parses, or this would pass by refusing all.
        assert!(parse_schema(r#"{"oneOf":[]}"#).is_ok());
    }

    #[test]
    fn a_dangling_ref_refuses() {
        let doc = json!({
            "oneOf": [{"properties": {"method": {"enum": ["thread/read"]},
                                      "params": {"$ref": "#/definitions/Nope"}}}]
        });
        assert_eq!(
            project_only(&doc, &no_notifications()),
            Err(ProjectionError::UnresolvedRef("Nope".into()))
        );
    }

    #[test]
    fn a_malformed_bundle_refuses_rather_than_projecting_nothing() {
        // The dangerous failure is an empty projection comparing equal to an empty
        // vendored reference; a bundle with no `oneOf` must be an error, not `{}`.
        assert!(matches!(
            project_only(&json!({"definitions": {}}), &no_notifications()),
            Err(ProjectionError::Malformed(_))
        ));
        assert!(matches!(
            project_only(&json!({"oneOf": []}), &json!({"definitions": {}})),
            Err(ProjectionError::Malformed(_))
        ));
    }

    #[test]
    fn added_and_removed_fields_are_named() {
        let want: GuardedSurface = [(
            "request turn/start".to_string(),
            entry(
                json!({"properties": {"cwd": {"type": "string"}, "old": {"type": "string"}}}),
                json!({}),
            ),
        )]
        .into_iter()
        .collect();
        let got: GuardedSurface = [(
            "request turn/start".to_string(),
            entry(
                json!({"properties": {"cwd": {"type": "string"},
                                      "toolOutput": {"type": "string"}}}),
                json!({}),
            ),
        )]
        .into_iter()
        .collect();
        let d = diff_wire(&want, &got);
        assert!(d.contains(&SurfaceChange::FieldAdded {
            method: "request turn/start".into(),
            field: "toolOutput".into()
        }));
        assert!(d.contains(&SurfaceChange::FieldRemoved {
            method: "request turn/start".into(),
            field: "old".into()
        }));
    }

    /// A nullability widening keeps the field name and changes the meaning — the case a
    /// name-only projection would wave through. Measured on codex 0.153's
    /// `thread/resume` (`call_id` went `string` → `["string","null"]`).
    #[test]
    fn a_nullability_widening_is_caught() {
        let want: GuardedSurface = [(
            "request thread/resume".to_string(),
            entry(
                json!({"properties": {"call_id": {"type": "string"}}}),
                json!({}),
            ),
        )]
        .into_iter()
        .collect();
        let got: GuardedSurface = [(
            "request thread/resume".to_string(),
            entry(
                json!({"properties": {"call_id": {"type": ["string", "null"]}}}),
                json!({}),
            ),
        )]
        .into_iter()
        .collect();
        assert_eq!(
            diff_wire(&want, &got),
            vec![SurfaceChange::FieldChanged {
                method: "request thread/resume".into(),
                field: "call_id".into()
            }]
        );
    }

    /// **The masking case.** A field addition and an unrelated `required` move on the
    /// same method must BOTH be reported; the definition sweep used to run only when the
    /// field sweep found nothing.
    #[test]
    fn a_field_addition_does_not_mask_a_metadata_or_nested_change() {
        let base = |extra_prop: bool, required: Value, nested: Value| {
            let mut props = json!({"cwd": {"type": "string"}});
            if extra_prop {
                props["toolOutput"] = json!({"type": "string"});
            }
            let e: GuardedSurface = [(
                "request turn/start".to_string(),
                json!({
                    "envelope": {"required": ["id", "method", "params"]},
                    "params": {"$ref": "#/definitions/TurnStartParams"},
                    "definitions": {
                        "TurnStartParams": {"properties": props, "required": required},
                        "Nested": nested,
                    }
                }),
            )]
            .into_iter()
            .collect();
            e
        };
        let want = base(false, json!(["cwd"]), json!({"type": "string"}));
        let got = base(true, json!([]), json!({"type": ["string", "null"]}));
        let d = diff_wire(&want, &got);
        assert!(d.contains(&SurfaceChange::FieldAdded {
            method: "request turn/start".into(),
            field: "toolOutput".into()
        }));
        assert!(
            d.contains(&SurfaceChange::FieldChanged {
                method: "request turn/start".into(),
                field: "(params metadata, e.g. `required`)".into()
            }),
            "the `required` move must be reported beside the addition: {d:?}"
        );
        assert!(
            d.contains(&SurfaceChange::FieldChanged {
                method: "request turn/start".into(),
                field: "(type Nested)".into()
            }),
            "the nested type change must be reported beside the addition: {d:?}"
        );
    }

    /// The JSON-RPC envelope is inside the gate: a method whose `params` became optional
    /// changed a constraint the broker's absence rules rest on.
    #[test]
    fn an_envelope_change_is_caught() {
        let mk = |required: Value| -> GuardedSurface {
            [(
                "request thread/read".to_string(),
                json!({"envelope": {"required": required},
                       "params": {"properties": {"threadId": {"type": "string"}}},
                       "definitions": {}}),
            )]
            .into_iter()
            .collect()
        };
        assert_eq!(
            diff_wire(
                &mk(json!(["id", "method", "params"])),
                &mk(json!(["id", "method"]))
            ),
            vec![SurfaceChange::FieldChanged {
                method: "request thread/read".into(),
                field: "(the JSON-RPC envelope, e.g. `required`)".into()
            }]
        );
    }

    /// The compiled-in references must be real and non-empty. An empty reference
    /// compares equal to an empty projection, which admits every codex ever built —
    /// the one failure mode a diff-based gate has to be loud about.
    #[test]
    fn the_vendored_references_are_present_and_non_vacuous() {
        for load in [
            baseline_wire as fn(&str) -> GuardedSurface,
            grounded_wire as fn(&str) -> GuardedSurface,
        ] {
            for bundle in BUNDLES {
                let s = load(bundle);
                assert!(s.len() >= 15, "{bundle}: only {} guarded methods", s.len());
                assert!(s.contains_key("request turn/start"));
                assert!(s.contains_key("request thread/start"));
                assert!(s.contains_key("request thread/resume"));
                assert!(s.contains_key("notification initialized"));
                for (m, e) in &s {
                    assert!(e.is_object(), "{bundle} {m}: the entry is not an object");
                    assert!(e.get("envelope").is_some(), "{bundle} {m}: no envelope");
                    assert!(
                        e.get("definitions").is_some(),
                        "{bundle} {m}: no definitions"
                    );
                }
            }
        }
        let argv = baseline_argv();
        assert!(argv.subcommands.contains("exec"));
        assert!(argv.subcommands.contains("resume"));
        assert!(argv.flags.contains("--config"));
        // 0.147 has it; 0.153 does not. Its presence here is what makes the removal
        // detectable at all.
        assert!(argv.flags.contains("--psp"));
        assert!(!argv.subcommands.contains("agents"));
        let grounded = grounded_argv();
        assert!(grounded.subcommands.contains("agents"));
        assert!(!grounded.flags.contains("--psp"));
    }

    /// **The bridge is exact.** Applying every adjudicated delta to the 0.147 baseline
    /// must reproduce the 0.153 reference EXACTLY — no residue in either direction.
    ///
    /// This is what makes the delta list a measurement rather than a coordinate. A delta
    /// too narrow leaves a difference behind; a delta too broad, or one nobody needs,
    /// leaves one in the other direction. Both fail here, in an ordinary suite run, with
    /// no binary installed.
    #[test]
    fn the_adjudicated_table_bridges_the_two_references() {
        for bundle in BUNDLES {
            let grounded = grounded_wire(bundle);
            // `admissible_wire` splices a delta only when the installed side exhibits it;
            // handing it the grounded reference itself exhibits all of them.
            let bridged = admissible_wire(bundle, &grounded);
            let changes = diff_wire(&bridged, &grounded);
            assert!(
                changes.is_empty(),
                "{bundle}: the adjudicated table does not fully explain 0.147 → 0.153: {}",
                changes
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        let grounded = grounded_argv();
        let changes = diff_argv(&admissible_argv(&grounded), &grounded);
        assert!(
            changes.is_empty(),
            "the adjudicated argv table does not fully explain 0.147 → 0.153: {}",
            changes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    /// The other direction: the baseline itself is still admissible, so 0.147 keeps
    /// working. A delta that spliced unconditionally would break this.
    #[test]
    fn the_baseline_is_still_admissible() {
        for bundle in BUNDLES {
            let baseline = baseline_wire(bundle);
            assert!(
                diff_wire(&admissible_wire(bundle, &baseline), &baseline).is_empty(),
                "{bundle}: the 0.147 baseline is no longer admissible"
            );
        }
        let baseline = baseline_argv();
        assert!(diff_argv(&admissible_argv(&baseline), &baseline).is_empty());
    }

    /// Every wire delta must address a place where the two references actually differ.
    ///
    /// An entry that names an identical pointer covers nothing and would sit in the table
    /// looking reviewed; an entry whose pointer does not resolve in the reference it
    /// claims to describe is a typo that silently admits nothing.
    #[test]
    fn every_adjudicated_delta_addresses_a_real_difference() {
        for d in ADJUDICATED_WIRE {
            let baseline = baseline_wire(d.bundle);
            let grounded = grounded_wire(d.bundle);
            let at_baseline = baseline.get(d.key).and_then(|e| pointer(e, d.at));
            let at_grounded = grounded.get(d.key).and_then(|e| pointer(e, d.at));
            assert_ne!(
                at_baseline, at_grounded,
                "{d:?}: the two references agree here, so this entry adjudicates nothing"
            );
            match d.kind {
                DeltaKind::Added => {
                    assert!(
                        at_baseline.is_none(),
                        "{d:?}: the baseline already has this"
                    );
                    assert!(at_grounded.is_some(), "{d:?}: 0.153 does not add this");
                }
                DeltaKind::Removed => {
                    assert!(at_baseline.is_some(), "{d:?}: nothing to remove");
                    assert!(at_grounded.is_none(), "{d:?}: 0.153 still has this");
                }
                DeltaKind::Shape => {
                    assert!(at_baseline.is_some() && at_grounded.is_some(), "{d:?}");
                }
            }
        }
    }

    /// An adjudicated delta must name something the broker actually guards.
    ///
    /// This is the single-source-of-truth tie for the delta list. A delta for a method
    /// the allowlist refuses on every leg is either a typo or a widening of something
    /// that is not widenable — and either way it would sit in the table looking
    /// reviewed, silently covering nothing (or, worse, covering a method somebody later
    /// adds to the allowlist without re-measuring).
    #[test]
    fn every_adjudicated_delta_names_a_guarded_method() {
        for d in ADJUDICATED_WIRE {
            assert!(
                !d.verdict.trim().is_empty(),
                "{d:?}: an adjudicated delta must carry the measurement that made it safe"
            );
            assert!(
                BUNDLES.contains(&d.bundle),
                "{d:?}: {:?} is not a schema bundle",
                d.bundle
            );
            let (kind, method) = d
                .key
                .split_once(' ')
                .unwrap_or_else(|| panic!("{d:?}: the key is not `<kind> <method>`"));
            match kind {
                // A CLIENT-side delta must name a method the allowlist admits. One for a
                // method refused on every leg is either a typo or a widening of something
                // that is not widenable — and either way it would sit in the table looking
                // reviewed while covering nothing.
                "request" | "notification" => assert!(
                    is_guarded(method),
                    "{d:?}: the allowlist refuses {method:?} on every leg, so its shape is \
                     not something this gate guards"
                ),
                // A SERVER-side delta names a method the app-server sends, which the
                // allowlist has no opinion about — it keys the client direction only. The
                // tie that matters is that the projection carries it, which
                // `every_adjudicated_delta_addresses_a_real_difference` enforces by
                // resolving the pointer in both references.
                "server-request" => assert!(
                    !method.is_empty(),
                    "{d:?}: a server-request delta must name a method"
                ),
                other => panic!("{d:?}: {other:?} is not a surface kind"),
            }
            assert!(
                d.at.is_empty() || d.at.starts_with('/'),
                "{d:?}: `at` must be an RFC 6901 pointer"
            );
        }
        for d in ADJUDICATED_ARGV {
            assert!(!d.verdict.trim().is_empty(), "{d:?}: no measurement");
        }
    }

    /// A root token is present or absent; it has no interior and therefore no shape.
    #[test]
    fn every_argv_delta_is_a_presence_fact() {
        for d in ADJUDICATED_ARGV {
            assert!(
                matches!(d.kind, DeltaKind::Added | DeltaKind::Removed),
                "{d:?}: a root CLI token is present or absent; it has no shape"
            );
        }
    }

    /// No two entries may describe the same difference.
    ///
    /// A duplicate is how a delta survives the deletion the mutation test performs: one
    /// copy removed, the other still admitting, and the entry looks load-bearing when it
    /// is not.
    #[test]
    fn adjudicated_deltas_are_unique() {
        let mut seen = BTreeSet::new();
        for d in ADJUDICATED_WIRE {
            assert!(
                seen.insert((d.bundle, d.key, d.at, d.kind as u8)),
                "{d:?} is listed twice"
            );
        }
        let mut seen = BTreeSet::new();
        for d in ADJUDICATED_ARGV {
            assert!(
                seen.insert((d.token, d.kind as u8)),
                "{d:?} is listed twice"
            );
        }
    }

    /// The negative arms: a surface differing from the admissible one in a way NOBODY
    /// adjudicated is refused, and the refusal names the thing that moved.
    ///
    /// Driven against the real vendored references and the real tables, so these are
    /// statements about the gate that actually ships.
    #[test]
    fn an_unadjudicated_change_is_refused_and_named() {
        // The named fields live in the definition the params `$ref` points at — the
        // shape codex actually emits. This helper reaches it the same way `diff_entry`
        // does, so the test perturbs what the gate reads.
        fn props<'a>(s: &'a mut GuardedSurface, key: &str) -> &'a mut Map<String, Value> {
            let entry = s.get_mut(key).expect("method in the reference");
            let name = entry["params"]["$ref"]
                .as_str()
                .expect("params is a $ref")
                .rsplit('/')
                .next()
                .unwrap()
                .to_string();
            entry["definitions"][&name]["properties"]
                .as_object_mut()
                .expect("the params type has properties")
        }
        let left = |installed: &GuardedSurface| {
            diff_wire(&admissible_wire("stable", installed), installed)
        };

        // (1) An ADDED field nobody adjudicated.
        let mut installed = baseline_wire("stable");
        props(&mut installed, "request turn/start")
            .insert("someFutureChannel".into(), json!({"type": "string"}));
        let d = left(&installed);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(
            d[0],
            SurfaceChange::FieldAdded {
                method: "request turn/start".into(),
                field: "someFutureChannel".into()
            }
        );
        assert!(d[0].to_string().contains("someFutureChannel"));

        // (2) A REMOVED guarded field. The subtle one: the broker SENDS `cwd` to
        // constrain codex, so a build that stopped understanding it would silently
        // drop the constraint while nothing in the forward direction looked different.
        let mut installed = baseline_wire("stable");
        props(&mut installed, "request turn/start")
            .remove("cwd")
            .expect("the baseline has turn/start.cwd");
        assert_eq!(
            left(&installed),
            vec![SurfaceChange::FieldRemoved {
                method: "request turn/start".into(),
                field: "cwd".into()
            }],
            "a removed guarded field must refuse, never be tolerated as 'less surface'"
        );

        // (3) An added root subcommand — the escape class.
        let mut installed = baseline_argv();
        installed.subcommands.insert("teleport".into());
        let d = diff_argv(&admissible_argv(&installed), &installed);
        assert_eq!(
            d,
            vec![SurfaceChange::SubcommandAdded {
                name: "teleport".into()
            }]
        );
        assert!(d[0].to_string().contains("prompt text"));
    }

    /// **An adjudicated delta admits one measured value, not a coordinate.**
    ///
    /// Each arm takes the 0.153 surface — which the gate admits — and perturbs exactly
    /// one adjudicated place. All four must refuse, because the perturbed value is not
    /// the value that was measured, so the delta is not exhibited and the splice does not
    /// happen.
    #[test]
    fn an_adjudicated_delta_does_not_admit_a_different_shape_there() {
        let left = |bundle: &str, installed: &GuardedSurface| {
            diff_wire(&admissible_wire(bundle, installed), installed)
        };
        fn entry_mut<'a>(s: &'a mut GuardedSurface, key: &str) -> &'a mut Value {
            s.get_mut(key)
                .unwrap_or_else(|| panic!("{key} is in the reference"))
        }

        // (a) An adjudicated ADDED field with a different shape.
        let mut installed = grounded_wire("stable");
        entry_mut(&mut installed, "request turn/start")["definitions"]["TurnStartParams"]
            ["properties"]["toolOutput"] = json!({"type": "string"});
        let d = left("stable", &installed);
        assert_eq!(
            d,
            vec![SurfaceChange::FieldAdded {
                method: "request turn/start".into(),
                field: "toolOutput".into()
            }],
            "an adjudicated addition must not admit an arbitrary shape for that field"
        );

        // (b) `ResponseItem` changed in a way OTHER than the one measured.
        let mut installed = grounded_wire("experimental");
        entry_mut(&mut installed, "request thread/resume")["definitions"]["ResponseItem"]
            ["oneOf"][0]["properties"]["id"] = json!({"type": "integer"});
        let d = left("experimental", &installed);
        assert_eq!(
            d,
            vec![SurfaceChange::FieldChanged {
                method: "request thread/resume".into(),
                field: "(type ResponseItem)".into()
            }],
            "a `Shape` delta must pin the measured value, not the type's name"
        );

        // (c) A whole-method addition, widened.
        let mut installed = grounded_wire("stable");
        entry_mut(&mut installed, "request thread/turns/list")["definitions"]
            ["ThreadTurnsListParams"]["properties"]["sessionId"] = json!({"type": "string"});
        let d = left("stable", &installed);
        assert_eq!(
            d,
            vec![SurfaceChange::MethodAdded {
                method: "request thread/turns/list".into()
            }],
            "a whole-method delta must pin the whole entry, not the method name"
        );

        // (d) An adjudicated addition beside an unrelated nested change: the nested one
        // must still be named.
        let mut installed = grounded_wire("stable");
        entry_mut(&mut installed, "request turn/start")["definitions"]["TurnStartParams"]
            ["required"] = json!(["input"]);
        let d = left("stable", &installed);
        assert_eq!(
            d,
            vec![SurfaceChange::FieldChanged {
                method: "request turn/start".into(),
                field: "(params metadata, e.g. `required`)".into()
            }],
            "an adjudicated addition must not mask an unadjudicated `required` move"
        );
    }

    #[test]
    fn identical_surfaces_admit() {
        let s: GuardedSurface = [(
            "request turn/start".to_string(),
            entry(json!({"properties": {"a": {}}}), json!({})),
        )]
        .into_iter()
        .collect();
        assert!(diff_wire(&s, &s.clone()).is_empty());
    }

    #[test]
    fn the_root_opts_line_parses_into_subcommands_and_flags() {
        let script = "_codex() {\n    case cmd in\n        codex)\n            \
                      opts=\"-c --config --help [PROMPT] exec resume help\"\n            ;;\n";
        let s = project_argv(script).unwrap();
        assert!(s.subcommands.contains("exec"));
        assert!(s.subcommands.contains("resume"));
        assert!(!s.subcommands.contains("[PROMPT]"));
        assert!(s.flags.contains("--config"));
        assert!(s.flags.contains("-c"));
        assert!(!s.flags.contains("exec"));
    }

    #[test]
    fn a_completion_script_with_no_root_opts_refuses() {
        assert!(matches!(
            project_argv("_codex() {\n  echo hi\n}\n"),
            Err(ProjectionError::Malformed(_))
        ));
    }

    /// The hidden-alias list is the one part of the argv surface no projection can see,
    /// so it must at least be non-empty and disjoint from what the projection DOES see —
    /// an entry that the completion script already enumerates is a stale note, not a
    /// gap-filler.
    #[test]
    fn the_hidden_root_alias_list_covers_only_what_the_projection_cannot_see() {
        assert!(!HIDDEN_ROOT_ALIASES.is_empty());
        let seen = baseline_argv();
        let grounded = grounded_argv();
        for alias in HIDDEN_ROOT_ALIASES {
            assert!(
                !seen.subcommands.contains(alias) && !grounded.subcommands.contains(alias),
                "{alias:?} IS enumerated by the completion script; the argv diff already \
                 covers it and listing it here hides that"
            );
        }
    }

    #[test]
    fn a_new_subcommand_is_named_as_the_escape_class() {
        let want = ArgvSurface {
            subcommands: ["exec".to_string()].into_iter().collect(),
            flags: ["--config".to_string()].into_iter().collect(),
        };
        let got = ArgvSurface {
            subcommands: ["exec".to_string(), "agents".to_string()]
                .into_iter()
                .collect(),
            flags: ["--config".to_string()].into_iter().collect(),
        };
        let d = diff_argv(&want, &got);
        assert_eq!(
            d,
            vec![SurfaceChange::SubcommandAdded {
                name: "agents".into()
            }]
        );
        assert!(d[0].to_string().contains("prompt text"));
    }

    #[test]
    fn a_removed_flag_is_reported_too() {
        let want = ArgvSurface {
            subcommands: ["exec".to_string()].into_iter().collect(),
            flags: ["--config".to_string(), "--psp".to_string()]
                .into_iter()
                .collect(),
        };
        let got = ArgvSurface {
            subcommands: ["exec".to_string()].into_iter().collect(),
            flags: ["--config".to_string()].into_iter().collect(),
        };
        assert_eq!(
            diff_argv(&want, &got),
            vec![SurfaceChange::FlagRemoved {
                name: "--psp".into()
            }]
        );
    }
}
