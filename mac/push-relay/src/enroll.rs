//! The credential lifecycle: who may hold a bearer, and what it takes to change
//! one.
//!
//! Four routes, and the difference between them is the authority each demands.
//! A challenge costs nothing and proves nothing. An enrollment costs a full
//! attestation, because it is the moment the relay decides a caller is a real
//! installation of the real app. A rebind, a rotation and a deletion cost an
//! assertion, because the App Attest key that signed them is the only thing
//! that ties the request to the installation that already exists. The status
//! endpoint costs a bearer and **cannot change anything at all**.
//!
//! **One `assert` route with an operation field, not three routes.** The
//! operation name is inside the bytes the device signed, so an assertion made to
//! delete a binding cannot be replayed to rotate one. Three routes would put
//! that binding in the URL, where nothing signs it, and the relationship between
//! the signed data and the effect would be a convention instead of a field.
//!
//! **A new credential revokes the previous one in the same transaction**, and
//! the partial unique index in `db.rs` is what enforces "at most one active" —
//! not the order of the statements here. Two enrollments racing on one token
//! both revoke and both insert; the index is what makes only one of those pairs
//! commit.
//!
//! **Every bearer records the generation it was minted under.** A restored
//! backup is a database that believes in credentials the operator has already
//! revoked, and no query against that database can know it. Raising the floor —
//! a number in a file outside the database — refuses all of them at once, and
//! the status endpoint answers `reenroll` so every phone attests again.

use std::sync::atomic::Ordering;

// Anonymous because `ring`'s digest `Context` owns that name in this module and
// only the extension methods are wanted here.
use anyhow::Context as _;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use push_core::ApnsEnvironment;
use ring::digest::{Context, SHA256};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Deserialize;

use crate::api::Relay;
use crate::attest::{self, Aaguid, VerifiedAttestation};
use crate::challenge::{self, ChallengeError};
use crate::config::AttestEnvironment;
use crate::logging::RequestLog;
use crate::ratelimit::{self, Decision};
use crate::secret::{bearer_hash, new_bearer, normalize_device_token, sha256_hex, token_hash};

/// The schema every lifecycle document declares, matching the push DTO's.
const SCHEMA: u32 = 1;

/// Apple's key id is the SHA-256 of the attested public key, base64 — 44
/// characters. The bound is a refusal before any decoding happens.
const MAX_KEY_ID_CHARS: usize = 64;

/// An attestation object is a certificate chain, a receipt and authenticator
/// data. Apple's own is a little under 8 KB of base64; this is comfortably above
/// anything a device produces and far below what an attacker would like to send.
const MAX_ATTESTATION_CHARS: usize = 16 * 1024;

/// An assertion is a signature and 37 bytes of authenticator data.
const MAX_ASSERTION_CHARS: usize = 2 * 1024;

const MAX_TOKEN_CHARS: usize = 256;
const MAX_ENVIRONMENT_CHARS: usize = 16;
const MAX_BUNDLE_VERSION_CHARS: usize = 32;

/// The enrollment body carries an attestation, so it needs a larger allowance
/// than the push route's kilobyte. Applied to the route rather than the router
/// so that every other endpoint keeps the small bound.
const ENROLL_BODY_BYTES: usize = 20 * 1024;
const ASSERT_BODY_BYTES: usize = 4 * 1024;

/// The longest `Authorization: Bearer` value that could be a credential.
///
/// A credential is 43 characters. The bound is here so that a megabyte of
/// header never reaches the hash function.
const MAX_BEARER_CHARS: usize = 128;

/// The address header the deployment's proxy sets.
const FORWARDED_FOR: &str = "x-forwarded-for";

/// The longest string that could be an address, textual IPv6 with a zone
/// included. Anything longer is not one and is not carried into a rate key.
const MAX_ADDRESS_CHARS: usize = 45;

/// Apple's validation categories, under the names its own table gives them.
const TESTFLIGHT_CATEGORY: u32 = 2;
const DEVELOPMENT_IDENTITY_CATEGORY: u32 = 3;
const APP_STORE_CATEGORY: u32 = 4;

/// What a build the relay serves customers may report.
///
/// **Both values, and never the store one alone.** Apple documents that an app
/// shipped through the App Store can still report a TestFlight launch, so a
/// relay that accepted only `4` would refuse a genuine customer whenever Apple
/// felt like saying `2`. Everything else in Apple's table describes a build that
/// did not come through Apple's distribution channels — an operating-system
/// executable, a development identity, an enterprise or ad-hoc profile, a
/// Developer ID binary, the restricted system categories, or any other signing
/// identity — and the relay is not for those.
const DISTRIBUTED_CATEGORIES: &[u32] = &[TESTFLIGHT_CATEGORY, APP_STORE_CATEGORY];

/// What a build in the development namespace may report.
///
/// **A rule rather than none.** The development AAGUID has already established
/// that the key was minted by a build signed with a development identity, and
/// `3` is what such a build reports; a namespace with no rule at all would
/// accept an operating-system executable or a Developer ID binary in the one
/// namespace whose keys anybody able to sign a build can mint. The two sets do
/// not overlap, so nothing about production is widened by this.
const DEVELOPMENT_CATEGORIES: &[u32] = &[DEVELOPMENT_IDENTITY_CATEGORY];

/// The categories a namespace accepts.
///
/// **Checked when present and not required to be.** The extension is absent on
/// every iOS before 27, and refusing an attestation for its absence would refuse
/// a genuine phone for the version of the OS it is running — while the AAGUID
/// has already proved which namespace minted the key. A value that is present
/// and outside the namespace's set is a build distributed some other way, and
/// that is refused.
fn accepted_categories(environment: AttestEnvironment) -> &'static [u32] {
    match environment {
        AttestEnvironment::Development => DEVELOPMENT_CATEGORIES,
        AttestEnvironment::Production => DISTRIBUTED_CATEGORIES,
    }
}

/// The APNs host an attestation from this namespace is allowed to bind.
///
/// **Apple makes the pairing, twice, and the relay only has to agree with it.**
/// A build signed with a development profile is given the development App Attest
/// environment and the sandbox APNs host; a build distributed through TestFlight
/// or the App Store is given the production App Attest environment and the
/// production host. Nothing a phone can install produces one half of either pair
/// with the other half of the other, so a development attestation offered
/// against a production token is either a mistake or somebody using the
/// namespace anyone can mint keys in to take a binding on a customer's phone.
fn bindable_environment(environment: AttestEnvironment) -> ApnsEnvironment {
    match environment {
        AttestEnvironment::Development => ApnsEnvironment::Sandbox,
        AttestEnvironment::Production => ApnsEnvironment::Production,
    }
}

/// The routes this module owns, mounted by [`crate::api::router`].
pub fn routes() -> Router<Relay> {
    Router::new()
        .route("/v1/attest/challenge", post(challenge_route))
        .route(
            "/v1/attest/enroll",
            post(enroll_route).layer(DefaultBodyLimit::max(ENROLL_BODY_BYTES)),
        )
        .route(
            "/v1/attest/assert",
            post(assert_route).layer(DefaultBodyLimit::max(ASSERT_BODY_BYTES)),
        )
        .route("/v1/credential/status", get(status_route))
}

// ---------------------------------------------------------------------------
// Documents.
//
// Every one is `deny_unknown_fields` and every string is bounded. The only
// values a caller can put into the relay are base64, hex, and words from closed
// sets — there is no field here that could carry a project name, a path, or a
// sentence, which is the same promise `dto.rs` makes about the push document.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChallengeRequest {
    schema: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollRequest {
    schema: u32,
    key_id: String,
    attestation: String,
    challenge: String,
    token: String,
    environment: String,
    /// What the app believes it is. **Compared with the attested value and
    /// refused on disagreement**, never stored — which is what keeps it from
    /// being a free-text field a caller can write anything into.
    bundle_version: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssertRequest {
    schema: u32,
    key_id: String,
    assertion: String,
    challenge: String,
    operation: Operation,
    token: String,
    environment: String,
}

/// What an assertion authorises. Closed, and part of what the device signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Operation {
    /// The phone's APNs token changed. The token in the document is the new one.
    Rebind,
    /// The same tuple, a replacement credential — a lost Mac holding the shared
    /// bearer is the case this exists for.
    Rotate,
    /// Push for this tuple ends here.
    Delete,
}

impl Operation {
    fn as_str(self) -> &'static str {
        match self {
            Operation::Rebind => "rebind",
            Operation::Rotate => "rotate",
            Operation::Delete => "delete",
        }
    }
}

/// Why a request was refused, as one word per rule.
///
/// The word is the caller's answer and the log line's outcome, so a refusal
/// cannot be logged as something other than what was returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    Malformed,
    Schema,
    Token,
    Environment,
    KeyId,
    BundleVersion,
    Attestation,
    Assertion,
    ChallengeUnknown,
    ChallengeExpired,
    ChallengeConsumed,
    Installation,
    Binding,
    TokenUnregistered,
    Forbidden,
    Unauthorized,
    EnrollmentDisabled,
    AttestUnconfigured,
    RateLimited,
    Internal,
}

impl Refusal {
    fn word(self) -> &'static str {
        match self {
            Refusal::Malformed => "malformed",
            Refusal::Schema => "schema",
            Refusal::Token => "token",
            Refusal::Environment => "environment",
            Refusal::KeyId => "key_id",
            Refusal::BundleVersion => "bundle_version",
            Refusal::Attestation => "attestation",
            Refusal::Assertion => "assertion",
            Refusal::ChallengeUnknown => "challenge_unknown",
            Refusal::ChallengeExpired => "challenge_expired",
            Refusal::ChallengeConsumed => "challenge_consumed",
            Refusal::Installation => "installation_unknown",
            Refusal::Binding => "binding_unknown",
            Refusal::TokenUnregistered => "token_unregistered",
            Refusal::Forbidden => "forbidden",
            Refusal::Unauthorized => "unauthorized",
            Refusal::EnrollmentDisabled => "enrollment_disabled",
            Refusal::AttestUnconfigured => "attest_unconfigured",
            Refusal::RateLimited => "rate_limited",
            Refusal::Internal => "internal",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Refusal::Malformed
            | Refusal::Schema
            | Refusal::Token
            | Refusal::Environment
            | Refusal::KeyId
            | Refusal::BundleVersion => StatusCode::BAD_REQUEST,
            Refusal::Unauthorized => StatusCode::UNAUTHORIZED,
            // Every refusal of authority is a 403 and none of them says which
            // half was wrong beyond its own word: a caller learning that the
            // challenge was fine but the signature was not is a caller being
            // told how to search.
            Refusal::Attestation
            | Refusal::Assertion
            | Refusal::ChallengeUnknown
            | Refusal::ChallengeExpired
            | Refusal::ChallengeConsumed
            | Refusal::Installation
            | Refusal::Binding
            | Refusal::Forbidden => StatusCode::FORBIDDEN,
            Refusal::TokenUnregistered => StatusCode::CONFLICT,
            Refusal::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Refusal::EnrollmentDisabled | Refusal::AttestUnconfigured => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Refusal::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<ChallengeError> for Refusal {
    fn from(error: ChallengeError) -> Self {
        match error {
            ChallengeError::Malformed => Refusal::Malformed,
            ChallengeError::Unknown => Refusal::ChallengeUnknown,
            ChallengeError::Expired => Refusal::ChallengeExpired,
            ChallengeError::Consumed => Refusal::ChallengeConsumed,
            ChallengeError::Database(_) => Refusal::Internal,
        }
    }
}

/// One answer, and the only way to produce one.
///
/// Built exclusively by [`answer`], [`refuse`] and [`limited`], each of which
/// emits the request's single log line — so a route cannot return without
/// logging, and cannot log something other than what it returned.
pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) body: serde_json::Value,
    pub(crate) retry_after_seconds: Option<u64>,
}

impl IntoResponse for Reply {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            [(header::CONTENT_TYPE, "application/json")],
            format!("{}\n", self.body),
        )
            .into_response();
        if let Some(seconds) = self.retry_after_seconds {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

pub(crate) fn answer(
    route: &'static str,
    outcome: &'static str,
    binding_hash: Option<&str>,
    body: serde_json::Value,
) -> Reply {
    RequestLog {
        route,
        status: StatusCode::OK.as_u16(),
        outcome,
        binding_hash,
    }
    .emit();
    Reply {
        status: StatusCode::OK,
        body,
        retry_after_seconds: None,
    }
}

pub(crate) fn refuse(route: &'static str, refusal: Refusal, binding_hash: Option<&str>) -> Reply {
    let status = refusal.status();
    RequestLog {
        route,
        status: status.as_u16(),
        outcome: refusal.word(),
        binding_hash,
    }
    .emit();
    Reply {
        status,
        body: serde_json::json!({ "error": refusal.word() }),
        retry_after_seconds: None,
    }
}

/// A fault in the relay's own state: said out loud, counted, and answered with
/// a five hundred.
///
/// **The error value is the point.** A database that has gone read-only or hit
/// `SQLITE_FULL` is exactly the incident §7's restore runbook exists for, and a
/// refusal that discards what SQLite said turns it into a bare five hundred in
/// no log and no metric — visible only as customers reporting that push stopped.
///
/// **Printing it is safe because of what the messages are made of.** SQLite
/// names tables, columns and constraints: `UNIQUE constraint failed:
/// bindings.bearer_hash`, `attempt to write a readonly database`. A raw token, a
/// bearer and a challenge only ever reach the database as bound parameters, and
/// a bound parameter is never in an error string.
pub(crate) fn internal(
    relay: &Relay,
    route: &'static str,
    binding_hash: Option<&str>,
    error: impl std::fmt::Display,
) -> Reply {
    relay
        .metrics
        .internal_errors
        .fetch_add(1, Ordering::Relaxed);
    tracing::error!(route, error = %error, "the relay could not answer");
    refuse(route, Refusal::Internal, binding_hash)
}

/// The 429, which the plan requires to carry `Retry-After`.
pub(crate) fn limited(route: &'static str, seconds: u64, binding_hash: Option<&str>) -> Reply {
    let mut reply = refuse(route, Refusal::RateLimited, binding_hash);
    reply.retry_after_seconds = Some(seconds);
    reply
}

/// Spend one unit of every rule that applies, or the answer that says which.
///
/// **Called before the document is parsed** on the address rules, because the
/// traffic an address limit exists to stop is traffic that never parses. The
/// binding rule cannot be: its key is the token, and the token is in the body.
pub(crate) fn check_limits(
    relay: &Relay,
    conn: &Connection,
    route: &'static str,
    binding_hash: Option<&str>,
    rules: &[(&str, ratelimit::Limit)],
    now: i64,
) -> Option<Reply> {
    for (key, limit) in rules {
        match relay.limiter.check(conn, key, *limit, now) {
            Ok(Decision::Allowed) => {}
            Ok(Decision::Limited {
                retry_after_seconds,
            }) => {
                relay.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
                return Some(limited(route, retry_after_seconds, binding_hash));
            }
            Err(e) => {
                return Some(internal(relay, route, binding_hash, format!("{e:#}")));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Routes.
// ---------------------------------------------------------------------------

async fn challenge_route(State(relay): State<Relay>, headers: HeaderMap, body: Bytes) -> Reply {
    issue_challenge(&relay, &body, client_address(&headers), now_ms())
}

async fn enroll_route(State(relay): State<Relay>, headers: HeaderMap, body: Bytes) -> Reply {
    enroll(&relay, &body, client_address(&headers), now_ms())
}

async fn assert_route(State(relay): State<Relay>, headers: HeaderMap, body: Bytes) -> Reply {
    lifecycle(&relay, &body, client_address(&headers), now_ms())
}

async fn status_route(State(relay): State<Relay>, headers: HeaderMap) -> Reply {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    credential_status(&relay, authorization, client_address(&headers), now_ms())
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

/// The caller's address, for a rate key and for nothing else.
///
/// **The last entry of the last line, and neither of those is the first.** A
/// client that sends its own `X-Forwarded-For` has the deployment's proxy add to
/// it, and a proxy adds in one of two ways: another entry on the same line, or
/// another line with the same name. HTTP says the two are equivalent, and a
/// reader that handled only the first would be a limiter an attacker resets by
/// choosing the shape the proxy does not merge — so both are read from the
/// right. What ends up here is the address the deployment's own proxy observed,
/// which is the one value nobody upstream of it chose. Where the proxy replaces
/// the header outright there is one line with one entry and every reading
/// agrees.
///
/// **This value never reaches a log line or a column.** It is hashed with the
/// day and the pepper by [`ratelimit::Limiter::ip_key`] the moment it is used,
/// and `RequestLog` has no field that could carry one.
pub(crate) fn client_address(headers: &HeaderMap) -> Option<&str> {
    let forwarded = headers
        .get_all(FORWARDED_FOR)
        .iter()
        .next_back()?
        .to_str()
        .ok()?;
    let observed = forwarded.rsplit(',').next()?.trim();
    if observed.is_empty() || observed.len() > MAX_ADDRESS_CHARS {
        return None;
    }
    Some(observed)
}

// ---------------------------------------------------------------------------
// The challenge.
// ---------------------------------------------------------------------------

fn issue_challenge(relay: &Relay, body: &[u8], ip: Option<&str>, now: i64) -> Reply {
    const ROUTE: &str = "/v1/attest/challenge";

    let db = relay.db.lock().unwrap_or_else(|e| e.into_inner());
    let address = relay.limiter.ip_key("challenge", ip, now);
    if let Some(reply) = check_limits(
        relay,
        &db,
        ROUTE,
        None,
        &[
            (address.as_str(), ratelimit::CHALLENGE_IP),
            (ratelimit::CHALLENGE_GLOBAL_KEY, ratelimit::CHALLENGE_GLOBAL),
        ],
        now,
    ) {
        return reply;
    }

    let request: ChallengeRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(_) => return refuse(ROUTE, Refusal::Malformed, None),
    };
    if request.schema != SCHEMA {
        return refuse(ROUTE, Refusal::Schema, None);
    }

    match challenge::issue(&db, now) {
        Ok(issued) => {
            relay
                .metrics
                .challenges_issued
                .fetch_add(1, Ordering::Relaxed);
            answer(
                ROUTE,
                "issued",
                None,
                serde_json::json!({
                    "challenge": issued.challenge.expose(),
                    "expires_in_seconds": issued.expires_in_seconds,
                }),
            )
        }
        Err(e) => internal(relay, ROUTE, None, format!("{e:#}")),
    }
}

// ---------------------------------------------------------------------------
// Enrollment.
// ---------------------------------------------------------------------------

fn enroll(relay: &Relay, body: &[u8], ip: Option<&str>, now: i64) -> Reply {
    enroll_accepting(
        relay,
        body,
        ip,
        now,
        accepted_categories(relay.config.attest_environment),
    )
}

/// Enrollment, and the one thing a test is allowed to differ about.
///
/// The seam exists for the reason `attest.rs`'s does: the only attestation
/// object Apple has ever published was made by an operating-system executable
/// and reports category 1, no test can re-sign it into something else, and a
/// suite that drove it through the real policy could therefore only ever assert
/// a refusal. The route passes its namespace's own set, the policy is asserted
/// directly against every category in Apple's table, and the tests that need
/// Apple's object to reach the far side of enrollment pass the category it was
/// made with.
fn enroll_accepting(
    relay: &Relay,
    body: &[u8],
    ip: Option<&str>,
    now: i64,
    accepted: &[u32],
) -> Reply {
    const ROUTE: &str = "/v1/attest/enroll";

    // The kill switch is checked before anything is parsed, so that turning
    // enrollment off costs an attacker a refusal rather than a verification.
    if !relay.config.enrollment_enabled {
        return refuse(ROUTE, Refusal::EnrollmentDisabled, None);
    }
    let Some(app_id) = relay.config.app_id.as_deref() else {
        return refuse(ROUTE, Refusal::AttestUnconfigured, None);
    };

    let mut guard = relay.db.lock().unwrap_or_else(|e| e.into_inner());
    let address = relay.limiter.ip_key("enroll", ip, now);
    if let Some(reply) = check_limits(
        relay,
        &guard,
        ROUTE,
        None,
        &[
            (address.as_str(), ratelimit::ENROLL_IP),
            (ratelimit::ENROLL_GLOBAL_KEY, ratelimit::ENROLL_GLOBAL),
        ],
        now,
    ) {
        return reply;
    }

    let request: EnrollRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(_) => return refuse(ROUTE, Refusal::Malformed, None),
    };
    if request.schema != SCHEMA {
        return refuse(ROUTE, Refusal::Schema, None);
    }
    if request.key_id.len() > MAX_KEY_ID_CHARS || request.attestation.len() > MAX_ATTESTATION_CHARS
    {
        return refuse(ROUTE, Refusal::Malformed, None);
    }
    if request.token.len() > MAX_TOKEN_CHARS || request.environment.len() > MAX_ENVIRONMENT_CHARS {
        return refuse(ROUTE, Refusal::Malformed, None);
    }
    let Ok(token) = normalize_device_token(&request.token) else {
        return refuse(ROUTE, Refusal::Token, None);
    };
    let Some(environment) = bindable(&request.environment, relay.config.attest_environment) else {
        return refuse(ROUTE, Refusal::Environment, None);
    };
    let Some(key_id) = decode_base64(&request.key_id) else {
        return refuse(ROUTE, Refusal::KeyId, None);
    };
    let Some(attestation) = decode_base64(&request.attestation) else {
        return refuse(ROUTE, Refusal::Malformed, None);
    };
    if let Some(claimed) = request.bundle_version.as_deref() {
        if !is_version(claimed) {
            return refuse(ROUTE, Refusal::BundleVersion, None);
        }
    }

    let token_hash = token_hash(&token);
    let binding = Some(token_hash.as_str());

    // **The bucket for a caller that has proved nothing yet.** The token here is
    // a name in a body: anybody can write somebody else's, and until the chain
    // verifies there is no reason to believe this caller is that phone. Charging
    // the phone's own five hundred would let a stolen APNs token — which §5 says
    // alone cannot use the relay — silence it for a day. The expensive path is
    // still not free, because this rule is the address rule's size and the
    // address rule has already been spent above.
    let unverified = ratelimit::unverified_key(&token_hash);
    if let Some(reply) = check_limits(
        relay,
        &guard,
        ROUTE,
        binding,
        &[(unverified.as_str(), ratelimit::UNVERIFIED_TOKEN)],
        now,
    ) {
        return reply;
    }

    let generation = match generation_floor(relay) {
        Ok(generation) => generation,
        Err(e) => return internal(relay, ROUTE, binding, format!("{e:#}")),
    };
    let expected_aaguid = match relay.config.attest_environment {
        AttestEnvironment::Development => Aaguid::DEVELOPMENT,
        AttestEnvironment::Production => Aaguid::PRODUCTION,
    };

    let tx = match guard.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(e) => return internal(relay, ROUTE, binding, e),
    };

    // **In the same transaction as the enrollment it authorises.** Consumed in
    // one and enrolled in another, a crash between them either burns a challenge
    // a phone still needs or leaves one spendable after the credential it paid
    // for already exists.
    let client_data = match challenge::consume(&tx, &request.challenge, now) {
        Ok(bytes) => bytes,
        Err(ChallengeError::Database(why)) => return internal(relay, ROUTE, binding, why),
        Err(e) => return refuse(ROUTE, Refusal::from(e), binding),
    };

    let verified = match attest::verify_attestation(
        &attestation,
        &client_data,
        app_id,
        expected_aaguid,
        &key_id,
        unix_time(now),
    ) {
        Ok(verified) => verified,
        Err(_) => return refuse(ROUTE, Refusal::Attestation, binding),
    };
    if let Some(refusal) = distribution_policy(
        relay,
        accepted,
        verified.validation_category,
        verified.bundle_version.as_deref(),
    ) {
        return refuse(ROUTE, refusal, binding);
    }
    // What the app believes it is has to be what the device attested, or one of
    // the two is describing a build that is not running.
    if let Some(claimed) = request.bundle_version.as_deref() {
        if verified.bundle_version.as_deref() != Some(claimed) {
            return refuse(ROUTE, Refusal::BundleVersion, binding);
        }
    }

    // **And now the token's own budget**, because the attestation has just made
    // this caller the installation it claimed to be. §3's rule is untouched: the
    // key is still the token hash, so the credential this is about to mint
    // cannot be rotated into a fresh five hundred.
    let bucket = ratelimit::binding_key(&token_hash);
    if let Some(reply) = check_limits(
        relay,
        &tx,
        ROUTE,
        binding,
        &[(bucket.as_str(), ratelimit::BINDING)],
        now,
    ) {
        return reply;
    }

    // §7: a token Apple has reported as unregistered is not reissued a
    // credential. The phone gets a new token from APNs and enrols that instead.
    match token_is_unregistered(&tx, &token_hash) {
        Ok(true) => return refuse(ROUTE, Refusal::TokenUnregistered, binding),
        Ok(false) => {}
        Err(e) => return internal(relay, ROUTE, binding, e),
    }

    let Ok(bearer) = new_bearer() else {
        return internal(
            relay,
            ROUTE,
            binding,
            "the system random number generator refused",
        );
    };
    let key_id_hash = sha256_hex(&key_id);
    if let Err(e) = record_enrollment(
        &tx,
        &Enrollment {
            verified: &verified,
            key_id_hash: &key_id_hash,
            attest_environment: relay.config.attest_environment,
            token_hash: &token_hash,
            environment,
            bearer_hash: &bearer_hash(&bearer),
            generation,
        },
        now,
    ) {
        return internal(relay, ROUTE, binding, e);
    }
    if let Err(e) = tx.commit() {
        return internal(relay, ROUTE, binding, e);
    }

    relay
        .metrics
        .credentials_issued
        .fetch_add(1, Ordering::Relaxed);
    answer(
        ROUTE,
        "enrolled",
        binding,
        credential_body(&bearer, environment, generation),
    )
}

fn credential_body(
    bearer: &crate::secret::Secret,
    environment: ApnsEnvironment,
    generation: i64,
) -> serde_json::Value {
    serde_json::json!({
        "credential": bearer.expose(),
        "environment": environment.as_str(),
        "generation": generation,
    })
}

/// The distribution policy, which is the relay's and not the parser's.
///
/// **Applied wherever authority is exercised and not only where it is granted.**
/// An App Attest key outlives the build that minted it: an installation that was
/// acceptable the day it attested keeps a working private key for as long as the
/// phone holds it, so a minimum the operator raises afterwards — or a category
/// that stops being accepted — has to be met by the assertion that rebinds or
/// rotates as much as by the attestation that enrolled. Enforcing it only at
/// enrollment leaves every installation already in the database with permanent
/// lifecycle authority over its own binding.
fn distribution_policy(
    relay: &Relay,
    accepted: &[u32],
    category: Option<u32>,
    version: Option<&str>,
) -> Option<Refusal> {
    if let Some(category) = category {
        if !accepted.contains(&category) {
            return Some(Refusal::Attestation);
        }
    }
    if let Some(minimum) = relay.config.min_bundle_version.as_deref() {
        match version {
            Some(found) if version_is_at_least(found, minimum) => {}
            // An absent version cannot be shown to meet a minimum, and a
            // deployment that set one asked for it to be met.
            _ => return Some(Refusal::BundleVersion),
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Assertion-authorised lifecycle.
// ---------------------------------------------------------------------------

fn lifecycle(relay: &Relay, body: &[u8], ip: Option<&str>, now: i64) -> Reply {
    const ROUTE: &str = "/v1/attest/assert";

    let Some(app_id) = relay.config.app_id.as_deref() else {
        return refuse(ROUTE, Refusal::AttestUnconfigured, None);
    };

    let mut guard = relay.db.lock().unwrap_or_else(|e| e.into_inner());
    let address = relay.limiter.ip_key("enroll", ip, now);
    if let Some(reply) = check_limits(
        relay,
        &guard,
        ROUTE,
        None,
        &[
            (address.as_str(), ratelimit::ENROLL_IP),
            (ratelimit::ENROLL_GLOBAL_KEY, ratelimit::ENROLL_GLOBAL),
        ],
        now,
    ) {
        return reply;
    }

    let request: AssertRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(_) => return refuse(ROUTE, Refusal::Malformed, None),
    };
    if request.schema != SCHEMA {
        return refuse(ROUTE, Refusal::Schema, None);
    }
    // **Deletion survives the kill switch.** Enrollment and rotation mint
    // credentials and are what the switch is for; refusing a revocation would
    // keep a credential alive during the incident that turned the switch off.
    if request.operation != Operation::Delete && !relay.config.enrollment_enabled {
        return refuse(ROUTE, Refusal::EnrollmentDisabled, None);
    }
    if request.key_id.len() > MAX_KEY_ID_CHARS || request.assertion.len() > MAX_ASSERTION_CHARS {
        return refuse(ROUTE, Refusal::Malformed, None);
    }
    if request.token.len() > MAX_TOKEN_CHARS || request.environment.len() > MAX_ENVIRONMENT_CHARS {
        return refuse(ROUTE, Refusal::Malformed, None);
    }
    let Ok(token) = normalize_device_token(&request.token) else {
        return refuse(ROUTE, Refusal::Token, None);
    };
    let Some(environment) = bindable(&request.environment, relay.config.attest_environment) else {
        return refuse(ROUTE, Refusal::Environment, None);
    };
    let Some(key_id) = decode_base64(&request.key_id) else {
        return refuse(ROUTE, Refusal::KeyId, None);
    };
    let Some(assertion) = decode_base64(&request.assertion) else {
        return refuse(ROUTE, Refusal::Malformed, None);
    };

    let token_hash = token_hash(&token);
    let binding = Some(token_hash.as_str());

    // The bucket a caller spends from while the token in its body is still only
    // a claim. The signature has not been checked yet, so anyone can name
    // anyone's token here, and the phone's own budget is not what pays for that.
    let unverified = ratelimit::unverified_key(&token_hash);
    if let Some(reply) = check_limits(
        relay,
        &guard,
        ROUTE,
        binding,
        &[(unverified.as_str(), ratelimit::UNVERIFIED_TOKEN)],
        now,
    ) {
        return reply;
    }

    let floor = match generation_floor(relay) {
        Ok(floor) => floor,
        Err(e) => return internal(relay, ROUTE, binding, format!("{e:#}")),
    };

    let tx = match guard.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(e) => return internal(relay, ROUTE, binding, e),
    };

    let challenge_bytes = match challenge::consume(&tx, &request.challenge, now) {
        Ok(bytes) => bytes,
        Err(ChallengeError::Database(why)) => return internal(relay, ROUTE, binding, why),
        Err(e) => return refuse(ROUTE, Refusal::from(e), binding),
    };

    let key_id_hash = sha256_hex(&key_id);
    let installation = match find_installation(&tx, &key_id_hash, relay.config.attest_environment) {
        Ok(Some(installation)) => installation,
        Ok(None) => return refuse(ROUTE, Refusal::Installation, binding),
        Err(e) => return internal(relay, ROUTE, binding, e),
    };

    // §7: a restored backup's counters describe a moment that has passed, so
    // they are untrusted until one successful assertion re-establishes them.
    // Measuring against zero accepts the next assertion whatever its counter and
    // then holds every later one to it.
    let stored_counter = if installation.counter_trusted {
        installation.counter
    } else {
        0
    };
    let client_data = assertion_client_data(
        request.operation,
        &request.challenge,
        challenge_bytes.as_slice(),
        &token,
        environment,
    );
    let asserted = match attest::verify_assertion(
        &assertion,
        &client_data,
        &installation.public_key,
        app_id,
        stored_counter,
    ) {
        Ok(asserted) => asserted,
        Err(_) => return refuse(ROUTE, Refusal::Assertion, binding),
    };

    // **The assertion's own account of the build, when it has one, outranks the
    // attestation's.** From iOS 27 a device appends the distribution extensions
    // to every assertion, and those describe the build running now rather than
    // the one that enrolled — so a phone that has been updated can meet a raised
    // minimum with the key it already holds instead of being sent through an
    // attestation it does not need. Every earlier iOS appends nothing, and what
    // it enrolled with is then all there is.
    let category = asserted
        .validation_category
        .or(installation.validation_category);
    let version = asserted
        .bundle_version
        .clone()
        .or_else(|| installation.bundle_version.clone());
    if let Err(e) = record_assertion(
        &tx,
        installation.id,
        asserted.counter,
        category,
        version.as_deref(),
        now,
    ) {
        return internal(relay, ROUTE, binding, e);
    }

    // **And now the token's own budget**, because the signature has just proved
    // the caller is the installation that holds this token. §3's rule is
    // untouched: the key is still the token hash, so the replacement credential
    // this may mint cannot buy a fresh five hundred.
    let bucket = ratelimit::binding_key(&token_hash);
    if let Some(reply) = check_limits(
        relay,
        &tx,
        ROUTE,
        binding,
        &[(bucket.as_str(), ratelimit::BINDING)],
        now,
    ) {
        return reply;
    }

    // **A phone may always give up its credential.** §7 has a lost Mac answered
    // by rotating the shared bearer and an incident answered by revoking it, and
    // a distribution rule that stood in the way of a revocation would leave the
    // credential the operator is trying to retire alive for as long as the phone
    // sits below the policy. Everything that mints one is held to it.
    if request.operation != Operation::Delete {
        if let Some(refusal) = distribution_policy(
            relay,
            accepted_categories(relay.config.attest_environment),
            category,
            version.as_deref(),
        ) {
            return refuse(ROUTE, refusal, binding);
        }
    }

    let outcome = match apply(
        &tx,
        request.operation,
        &installation,
        &token_hash,
        environment,
        floor,
        now,
    ) {
        Ok(outcome) => outcome,
        Err(Fault::Refused(refusal)) => return refuse(ROUTE, refusal, binding),
        Err(Fault::Internal(why)) => return internal(relay, ROUTE, binding, why),
    };
    if let Err(e) = tx.commit() {
        return internal(relay, ROUTE, binding, e);
    }

    match outcome {
        Applied::Credential(bearer) => {
            relay
                .metrics
                .credentials_issued
                .fetch_add(1, Ordering::Relaxed);
            answer(
                ROUTE,
                request.operation.as_str(),
                binding,
                credential_body(&bearer, environment, floor),
            )
        }
        Applied::Deleted => {
            relay
                .metrics
                .credentials_revoked
                .fetch_add(1, Ordering::Relaxed);
            answer(
                ROUTE,
                request.operation.as_str(),
                binding,
                serde_json::json!({ "status": "deleted" }),
            )
        }
    }
}

enum Applied {
    Credential(crate::secret::Secret),
    Deleted,
}

/// Why an operation did not happen: a rule the caller broke, or a fault in the
/// relay's own state.
///
/// **Kept apart because the two are answered differently.** A rule is a word for
/// the caller and nothing an operator has to know about; a database that has
/// gone read-only is a five hundred, a log line carrying SQLite's own complaint,
/// and a counter somebody is watching. Collapsing them — which a
/// `map_err(|_| Refusal::Internal)` does — turns the second into the first with
/// the evidence thrown away.
enum Fault {
    Refused(Refusal),
    Internal(String),
}

impl From<Refusal> for Fault {
    fn from(refusal: Refusal) -> Self {
        Fault::Refused(refusal)
    }
}

impl From<rusqlite::Error> for Fault {
    fn from(error: rusqlite::Error) -> Self {
        Fault::Internal(error.to_string())
    }
}

fn apply(
    tx: &Transaction<'_>,
    operation: Operation,
    installation: &Installation,
    token_hash: &str,
    environment: ApnsEnvironment,
    floor: i64,
    now: i64,
) -> Result<Applied, Fault> {
    // **A floor bump is an instruction to attest again, not to rotate.** An
    // assertion that could replace a below-floor credential would make the floor
    // something a phone steps over with the key it already has, and §7 is
    // explicit that a raised floor sends every phone through fresh attestation.
    // Revocation is exempt: refusing to retire a credential during the incident
    // the floor was raised for is the wrong direction.
    if operation != Operation::Delete && holds_a_credential_below(tx, installation.id, floor)? {
        return Err(Refusal::Binding.into());
    }
    match operation {
        Operation::Rotate => {
            let existing =
                active_binding(tx, token_hash)?.ok_or(Fault::Refused(Refusal::Binding))?;
            if existing.installation_id != installation.id {
                return Err(Refusal::Forbidden.into());
            }
            revoke_active_for_token(tx, token_hash, "rotated", now)?;
            issue(tx, installation.id, token_hash, environment, floor, now)
        }
        Operation::Rebind => {
            if let Some(existing) = active_binding(tx, token_hash)? {
                // An assertion is authority over this installation, never over
                // somebody else's token. Taking a live token from another
                // installation needs a fresh attestation, which is a different
                // route with a different cost.
                if existing.installation_id != installation.id {
                    return Err(Refusal::Forbidden.into());
                }
            }
            if token_is_unregistered(tx, token_hash)? {
                return Err(Refusal::TokenUnregistered.into());
            }
            // Everything this installation still holds, which is the old token
            // as well as any live row on the new one.
            revoke_active_for_installation(tx, installation.id, "rebound", now)?;
            issue(tx, installation.id, token_hash, environment, floor, now)
        }
        Operation::Delete => {
            // Idempotent: a second deletion is the state the caller asked for,
            // and answering "no such binding" would leave an app that lost the
            // first response unable to tell whether push is off.
            revoke_installation_token(tx, installation.id, token_hash, "deleted", now)?;
            Ok(Applied::Deleted)
        }
    }
}

fn issue(
    tx: &Transaction<'_>,
    installation_id: i64,
    token_hash: &str,
    environment: ApnsEnvironment,
    generation: i64,
    now: i64,
) -> Result<Applied, Fault> {
    let bearer = new_bearer()
        .map_err(|_| Fault::Internal("the system random number generator refused".into()))?;
    insert_binding(
        tx,
        installation_id,
        token_hash,
        environment,
        &bearer_hash(&bearer),
        generation,
        now,
    )?;
    Ok(Applied::Credential(bearer))
}

/// The bytes an assertion signs over.
///
/// **The operation and the tuple are inside them.** An assertion is a signature
/// over whatever the app chose to hash; if that were only the challenge, one
/// captured on its way to `delete` would authorise a `rebind` to an attacker's
/// token. The challenge is present as the string the relay issued *and* as the
/// bytes it decodes to, so that neither encoding is the thing being agreed on.
fn assertion_client_data(
    operation: Operation,
    challenge: &str,
    challenge_bytes: &[u8],
    token: &str,
    environment: ApnsEnvironment,
) -> [u8; 32] {
    let mut context = Context::new(&SHA256);
    context.update(b"codeconnect-relay/1/assert\0");
    context.update(operation.as_str().as_bytes());
    context.update(b"\0");
    context.update(challenge.as_bytes());
    context.update(b"\0");
    context.update(challenge_bytes);
    context.update(b"\0");
    context.update(token.as_bytes());
    context.update(b"\0");
    context.update(environment.as_str().as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(context.finish().as_ref());
    out
}

// ---------------------------------------------------------------------------
// Status.
// ---------------------------------------------------------------------------

fn credential_status(
    relay: &Relay,
    authorization: Option<&str>,
    ip: Option<&str>,
    now: i64,
) -> Reply {
    const ROUTE: &str = "/v1/credential/status";

    let Some(bearer) = bearer_from(authorization) else {
        return refuse(ROUTE, Refusal::Unauthorized, None);
    };
    let hash = bearer_hash(&bearer);

    let db = relay.db.lock().unwrap_or_else(|e| e.into_inner());
    let found = match binding_for_bearer(&db, &hash) {
        Ok(found) => found,
        Err(e) => return internal(relay, ROUTE, None, e),
    };
    // A binding from the other App Attest namespace is not this relay's to
    // answer for, and is not found rather than answered about: the phone is told
    // to attest again, which is the only thing that can make it real here.
    let found = found
        .filter(|binding| binding.attest_environment == relay.config.attest_environment.as_str());

    // A bearer nobody minted is guessing, and it is counted against the strict
    // rotating-address rule rather than against a binding it does not have.
    let (key, limit, binding) = match &found {
        Some(binding) => (
            ratelimit::binding_key(&binding.token_hash),
            ratelimit::BINDING,
            Some(binding.token_hash.clone()),
        ),
        None => (
            relay.limiter.ip_key("auth", ip, now),
            ratelimit::INVALID_AUTH_IP,
            None,
        ),
    };
    if let Some(reply) = check_limits(
        relay,
        &db,
        ROUTE,
        binding.as_deref(),
        &[(key.as_str(), limit)],
        now,
    ) {
        return reply;
    }

    let floor = match generation_floor(relay) {
        Ok(floor) => floor,
        Err(e) => return internal(relay, ROUTE, binding.as_deref(), format!("{e:#}")),
    };

    // The four states of Decision 5, and every path to each of them.
    //
    // A bearer this relay has never heard of answers `reenroll` rather than a
    // bare 401: after a restore the relay has forgotten credentials that are
    // perfectly real on the phone, and an app that cannot tell "attest again"
    // from "the relay is broken" waits for a person instead of recovering.
    let (state, environment) = match &found {
        None => ("reenroll", None),
        Some(binding) if binding.generation < floor => ("reenroll", None),
        Some(binding) if binding.status == "active" => {
            ("active", Some(binding.environment.clone()))
        }
        Some(binding) if binding.terminal_reason.as_deref() == Some("unregistered") => {
            ("token_invalid", None)
        }
        Some(_) => ("reissue", None),
    };
    if state != "active" {
        relay
            .metrics
            .credential_refusals
            .fetch_add(1, Ordering::Relaxed);
    }

    let mut body = serde_json::json!({ "status": state });
    if let Some(environment) = environment {
        body["environment"] = serde_json::Value::String(environment);
    }
    answer(ROUTE, state, binding.as_deref(), body)
}

/// The credential out of an `Authorization` header, or nothing.
///
/// Case-insensitive on the scheme because that is what the specification says,
/// and bounded before the value is hashed.
pub(crate) fn bearer_from(authorization: Option<&str>) -> Option<crate::secret::Secret> {
    let value = authorization?;
    let (scheme, credential) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let credential = credential.trim();
    if credential.is_empty() || credential.len() > MAX_BEARER_CHARS {
        return None;
    }
    Some(crate::secret::Secret::new(credential))
}

// ---------------------------------------------------------------------------
// The database, and the transaction that makes a replacement atomic.
// ---------------------------------------------------------------------------

struct Installation {
    id: i64,
    public_key: Vec<u8>,
    counter: u32,
    counter_trusted: bool,
    /// What the attestation that admitted this installation said the build was.
    /// The policy is applied to these on every later assertion, so an
    /// installation cannot outlive the rule it was admitted under.
    validation_category: Option<u32>,
    bundle_version: Option<String>,
}

pub(crate) struct BindingRow {
    pub(crate) installation_id: i64,
    pub(crate) token_hash: String,
    pub(crate) environment: String,
    pub(crate) generation: i64,
    pub(crate) status: String,
    pub(crate) terminal_reason: Option<String>,
    /// The App Attest namespace the installation behind this binding proved
    /// itself in. **Part of the row rather than only of the configuration**, so
    /// that a relay pointed at the other namespace can refuse authority it is
    /// not the relay for instead of honouring whatever the file happens to hold.
    pub(crate) attest_environment: String,
}

/// The installation a key id names **in this namespace**.
///
/// **The namespace is part of the identity and not a startup switch.** A key id
/// looked up on its own is found whichever world it was minted in, so changing
/// the relay's configuration would leave every record it had ever admitted still
/// able to rebind, rotate and delete — the configuration would decide which
/// attestations are accepted from now on and nothing about the authority already
/// in the file. Scoped, a record from the other namespace is simply not there.
fn find_installation(
    conn: &Connection,
    key_id_hash: &str,
    attest_environment: AttestEnvironment,
) -> rusqlite::Result<Option<Installation>> {
    conn.query_row(
        "SELECT id, public_key, counter, counter_trusted, validation_category, bundle_version
         FROM installations WHERE key_id_hash = ?1 AND attest_environment = ?2",
        rusqlite::params![key_id_hash, attest_environment.as_str()],
        |row| {
            Ok(Installation {
                id: row.get(0)?,
                public_key: row.get(1)?,
                counter: row.get::<_, i64>(2)? as u32,
                counter_trusted: row.get::<_, i64>(3)? != 0,
                validation_category: row.get::<_, Option<i64>>(4)?.map(|value| value as u32),
                bundle_version: row.get(5)?,
            })
        },
    )
    .optional()
}

/// The live binding on a token, **whichever namespace holds it**.
///
/// Deliberately not scoped: the caller compares the row's installation with the
/// one the assertion proved, so a token another namespace's installation is
/// holding is answered `forbidden` rather than looking free and then colliding
/// with `bindings_one_active_per_token` as a five hundred.
fn active_binding(conn: &Connection, token_hash: &str) -> rusqlite::Result<Option<BindingRow>> {
    conn.query_row(
        &binding_query("b.token_hash = ?1 AND b.status = 'active'"),
        [token_hash],
        read_binding,
    )
    .optional()
}

pub(crate) fn binding_for_bearer(
    conn: &Connection,
    bearer_hash: &str,
) -> rusqlite::Result<Option<BindingRow>> {
    conn.query_row(
        &binding_query("b.bearer_hash = ?1"),
        [bearer_hash],
        read_binding,
    )
    .optional()
}

fn binding_query(predicate: &str) -> String {
    format!(
        "SELECT b.installation_id, b.token_hash, b.environment, b.generation, b.status,
                b.terminal_reason, i.attest_environment
         FROM bindings b JOIN installations i ON i.id = b.installation_id
         WHERE {predicate}"
    )
}

fn read_binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<BindingRow> {
    Ok(BindingRow {
        installation_id: row.get(0)?,
        token_hash: row.get(1)?,
        environment: row.get(2)?,
        generation: row.get(3)?,
        status: row.get(4)?,
        terminal_reason: row.get(5)?,
        attest_environment: row.get(6)?,
    })
}

/// Whether this installation is still holding a credential the relay would no
/// longer honour.
fn holds_a_credential_below(
    conn: &Connection,
    installation_id: i64,
    floor: i64,
) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM bindings
         WHERE installation_id = ?1 AND status = 'active' AND generation < ?2",
        rusqlite::params![installation_id, floor],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Whether Apple has told the relay this token is gone.
fn token_is_unregistered(conn: &Connection, token_hash: &str) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM bindings WHERE token_hash = ?1 AND terminal_reason = 'unregistered'",
        [token_hash],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn revoke_active_for_token(
    tx: &Transaction<'_>,
    token_hash: &str,
    reason: &str,
    now: i64,
) -> rusqlite::Result<usize> {
    tx.execute(
        "UPDATE bindings SET status = 'revoked', terminal_reason = ?2, updated_ms = ?3
         WHERE token_hash = ?1 AND status = 'active'",
        rusqlite::params![token_hash, reason, now],
    )
}

fn revoke_active_for_installation(
    tx: &Transaction<'_>,
    installation_id: i64,
    reason: &str,
    now: i64,
) -> rusqlite::Result<usize> {
    tx.execute(
        "UPDATE bindings SET status = 'revoked', terminal_reason = ?2, updated_ms = ?3
         WHERE installation_id = ?1 AND status = 'active'",
        rusqlite::params![installation_id, reason, now],
    )
}

fn revoke_installation_token(
    tx: &Transaction<'_>,
    installation_id: i64,
    token_hash: &str,
    reason: &str,
    now: i64,
) -> rusqlite::Result<usize> {
    tx.execute(
        "UPDATE bindings SET status = 'revoked', terminal_reason = ?3, updated_ms = ?4
         WHERE installation_id = ?1 AND token_hash = ?2 AND status = 'active'",
        rusqlite::params![installation_id, token_hash, reason, now],
    )
}

fn insert_binding(
    tx: &Transaction<'_>,
    installation_id: i64,
    token_hash: &str,
    environment: ApnsEnvironment,
    bearer_hash: &str,
    generation: i64,
    now: i64,
) -> rusqlite::Result<()> {
    // Issuing a credential is the moment the retention policy is enforced, the
    // way issuing a challenge is: every road that writes a binding comes
    // through here, so the thirty days are a fact about the file rather than
    // about whether a timer fired. The installation this is about was written or
    // touched a statement ago, which is what keeps the same sweep from taking it
    // for an orphan.
    crate::db::prune_terminal_records(tx, now)?;
    tx.execute(
        "INSERT INTO bindings
            (installation_id, token_hash, environment, bearer_hash, generation,
             status, terminal_reason, created_ms, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 'active', NULL, ?6, ?6)",
        rusqlite::params![
            installation_id,
            token_hash,
            environment.as_str(),
            bearer_hash,
            generation,
            now
        ],
    )?;
    Ok(())
}

/// Everything one accepted assertion changes about its installation: the
/// counter it reached, and what it said the build is.
fn record_assertion(
    tx: &Transaction<'_>,
    installation_id: i64,
    counter: u32,
    validation_category: Option<u32>,
    bundle_version: Option<&str>,
    now: i64,
) -> rusqlite::Result<usize> {
    tx.execute(
        "UPDATE installations
            SET counter = ?2, counter_trusted = 1, validation_category = ?3,
                bundle_version = ?4, updated_ms = ?5
          WHERE id = ?1",
        rusqlite::params![
            installation_id,
            i64::from(counter),
            validation_category.map(i64::from),
            bundle_version,
            now
        ],
    )
}

/// Record an installation and the credential its attestation authorised.
///
/// **One transaction, and the previous credential for this token dies in it.**
/// The revocation and the insertion are two statements; what makes them atomic
/// in the sense that matters is `bindings_one_active_per_token`, which refuses to
/// commit an interleaving that left two rows active.
struct Enrollment<'a> {
    verified: &'a VerifiedAttestation,
    key_id_hash: &'a str,
    attest_environment: AttestEnvironment,
    token_hash: &'a str,
    environment: ApnsEnvironment,
    bearer_hash: &'a str,
    generation: i64,
}

fn record_enrollment(
    tx: &Transaction<'_>,
    enrollment: &Enrollment<'_>,
    now: i64,
) -> rusqlite::Result<()> {
    let Enrollment {
        verified,
        key_id_hash,
        attest_environment,
        token_hash,
        environment,
        bearer_hash,
        generation,
    } = *enrollment;
    // **The counter is never lowered.** A fresh attestation carries counter
    // zero, and writing that over a key whose assertions have already reached
    // forty would re-admit every assertion in between.
    tx.execute(
        "INSERT INTO installations
            (key_id_hash, public_key, receipt, attest_environment, counter, counter_trusted,
             bundle_version, validation_category, created_ms, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?8)
         ON CONFLICT (key_id_hash) DO UPDATE SET
             public_key = excluded.public_key,
             receipt = excluded.receipt,
             attest_environment = excluded.attest_environment,
             counter_trusted = 1,
             bundle_version = excluded.bundle_version,
             validation_category = excluded.validation_category,
             updated_ms = excluded.updated_ms",
        rusqlite::params![
            key_id_hash,
            verified.public_key,
            verified.receipt,
            attest_environment.as_str(),
            i64::from(verified.counter),
            verified.bundle_version,
            verified.validation_category.map(i64::from),
            now
        ],
    )?;
    let installation_id: i64 = tx.query_row(
        "SELECT id FROM installations WHERE key_id_hash = ?1",
        [key_id_hash],
        |row| row.get(0),
    )?;

    revoke_active_for_token(tx, token_hash, "superseded", now)?;
    insert_binding(
        tx,
        installation_id,
        token_hash,
        environment,
        bearer_hash,
        generation,
        now,
    )
}

// ---------------------------------------------------------------------------
// Small rules.
// ---------------------------------------------------------------------------

/// The environment, refused rather than guessed.
///
/// §4 makes the environment on a *push* advisory, because a binding already
/// exists and is the authority. Here the value **establishes** that binding, so
/// falling back to sandbox — which is what `ApnsEnvironment::parse` does — would
/// quietly bind a production phone to the wrong Apple host and answer every
/// later push with a token error.
fn strict_environment(value: &str) -> Option<ApnsEnvironment> {
    match value {
        "production" => Some(ApnsEnvironment::Production),
        "sandbox" => Some(ApnsEnvironment::Sandbox),
        _ => None,
    }
}

/// The environment a document names, when this namespace is allowed to bind it.
fn bindable(value: &str, attest_environment: AttestEnvironment) -> Option<ApnsEnvironment> {
    strict_environment(value).filter(|&named| named == bindable_environment(attest_environment))
}

/// Standard base64, which is what `DCAppAttestService` hands the app.
fn decode_base64(value: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(value).ok()
}

/// The generation a bearer minted right now records, read fresh.
///
/// An error is a floor that cannot be read, which every caller answers with a
/// five hundred rather than with zero — treating a permissions problem as "no
/// incident has happened" would re-admit every credential the floor was raised
/// to refuse. It is carried rather than discarded because it names the file, and
/// the file is what an operator has to go and look at.
pub(crate) fn generation_floor(relay: &Relay) -> anyhow::Result<i64> {
    let floor = relay.config.generation_floor()?;
    i64::try_from(floor).with_context(|| {
        format!("the generation floor is {floor}, which is more than this relay can count to")
    })
}

fn unix_time(now_ms: i64) -> rustls_pki_types::UnixTime {
    rustls_pki_types::UnixTime::since_unix_epoch(std::time::Duration::from_secs(
        now_ms.max(0) as u64 / 1_000,
    ))
}

/// A bundle version is dotted digits, bounded, and nothing else — so the one
/// field that looks like free text cannot be one.
fn is_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_BUNDLE_VERSION_CHARS
        && value.split('.').all(|part| {
            !part.is_empty() && part.len() <= 9 && part.bytes().all(|b| b.is_ascii_digit())
        })
}

/// Dotted-numeric ordering, because `"10"` is above `"9"` and string ordering
/// says otherwise — a minimum that compared as text would let an older build in
/// on the day the version reached two digits.
fn version_is_at_least(found: &str, minimum: &str) -> bool {
    if !is_version(found) || !is_version(minimum) {
        return false;
    }
    let parts = |value: &str| -> Vec<u64> {
        value
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (found, minimum) = (parts(found), parts(minimum));
    for index in 0..found.len().max(minimum.len()) {
        let a = found.get(index).copied().unwrap_or(0);
        let b = minimum.get(index).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::Request;
    use base64::Engine;
    use ciborium::value::Value;
    use http_body_util::BodyExt;
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, KeyPair as _, ECDSA_P256_SHA256_ASN1_SIGNING};
    use tower::ServiceExt;

    use super::*;
    use crate::config::RelayConfig;

    /// Apple's own published attestation object, and the only chain in existence
    /// that Apple actually signed. Its leaf lived three days, so every test that
    /// drives the real enrollment path pins the clock inside that window — which
    /// is why the core functions take a time rather than reading one.
    const REAL_OBJECT: &str = include_str!("../../../fixtures/appattest/attestation-object.b64");
    const REAL_APP_ID: &str = "1234567890.com.example.myapp";
    const REAL_CHALLENGE: &[u8] = b"example_server_challenge";
    const REAL_KEY_ID: &str = "zgSY9YSD+7TaDXssY6WlOPVS1K3Lmk+pFhlcSWE+ZV0=";
    /// Inside the fixture leaf's window, in milliseconds.
    const REAL_CLOCK_MS: i64 = 1_776_800_000_000;

    const TOKEN: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    const OTHER_TOKEN: &str = "0011223344556677889900112233445566778899001122334455667788990011";

    /// A directory under the OS temp dir. The process id is in the name because
    /// two `cargo test` invocations can overlap on one machine, and a fixed path
    /// means one of them deleting the other's fixture halfway through.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("push-relay-enroll-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config(pairs: &[(&str, &str)]) -> RelayConfig {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        RelayConfig::read(move |key| map.get(key).cloned()).unwrap()
    }

    /// A relay configured for the real fixture's app and namespace.
    fn relay(extra: &[(&str, &str)]) -> Relay {
        let mut pairs = vec![
            ("RELAY_APP_ID", REAL_APP_ID),
            ("RELAY_ATTEST_ENVIRONMENT", "production"),
        ];
        pairs.extend_from_slice(extra);
        Relay::new(config(&pairs), crate::db::open_in_memory().unwrap())
    }

    /// The same relay in the other App Attest namespace.
    fn development_relay(extra: &[(&str, &str)]) -> Relay {
        let mut pairs = vec![
            ("RELAY_APP_ID", REAL_APP_ID),
            ("RELAY_ATTEST_ENVIRONMENT", "development"),
        ];
        pairs.extend_from_slice(extra);
        Relay::new(config(&pairs), crate::db::open_in_memory().unwrap())
    }

    /// The category Apple's published object reports: an operating-system
    /// executable, which no namespace accepts and which no test can re-sign into
    /// anything else. The tests below drive enrollment with that category named,
    /// so that what they are about — challenges, atomicity, storage, limits — is
    /// what they measure; the policy itself is asserted against Apple's whole
    /// table, and the route's own set is asserted against this object.
    const REAL_OBJECT_CATEGORY: u32 = 1;

    fn enrol(relay: &Relay, body: &[u8], ip: Option<&str>, now: i64) -> Reply {
        enroll_accepting(relay, body, ip, now, &[REAL_OBJECT_CATEGORY])
    }

    fn real_attestation() -> String {
        REAL_OBJECT.split_whitespace().collect()
    }

    fn real_challenge() -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(REAL_CHALLENGE)
    }

    /// Put Apple's own challenge into the table so the real object can be
    /// enrolled through the production path.
    fn seed_real_challenge(relay: &Relay, now: i64) -> String {
        let challenge = real_challenge();
        let db = relay.db.lock().unwrap();
        challenge::store(&db, &challenge, now).unwrap();
        challenge
    }

    fn issued_challenge(relay: &Relay, now: i64) -> String {
        let db = relay.db.lock().unwrap();
        let issued = challenge::issue(&db, now).unwrap();
        issued.challenge.expose().to_string()
    }

    fn enroll_body(challenge: &str, token: &str) -> String {
        serde_json::json!({
            "schema": 1,
            "key_id": REAL_KEY_ID,
            "attestation": real_attestation(),
            "challenge": challenge,
            "token": token,
            "environment": "production",
        })
        .to_string()
    }

    fn body_of(reply: &Reply) -> serde_json::Value {
        reply.body.clone()
    }

    fn credential_of(reply: &Reply) -> String {
        body_of(reply)["credential"].as_str().unwrap().to_string()
    }

    fn active_count(relay: &Relay, token: &str) -> i64 {
        let db = relay.db.lock().unwrap();
        db.query_row(
            "SELECT COUNT(*) FROM bindings WHERE token_hash = ?1 AND status = 'active'",
            [token_hash(token)],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn status_of(relay: &Relay, credential: &str, now: i64) -> (StatusCode, serde_json::Value) {
        let reply = credential_status(relay, Some(&format!("Bearer {credential}")), None, now);
        (reply.status, reply.body)
    }

    // -----------------------------------------------------------------------
    // Enrollment against the one real attestation object.
    // -----------------------------------------------------------------------

    #[test]
    fn the_documented_enrollment_issues_one_credential_and_spends_its_challenge() {
        let relay = relay(&[]);
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);

        let reply = enrol(
            &relay,
            enroll_body(&challenge, TOKEN).as_bytes(),
            Some("203.0.113.7"),
            REAL_CLOCK_MS,
        );
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

        let body = body_of(&reply);
        let credential = credential_of(&reply);
        // 32 bytes, base64url, unpadded — and returned exactly once.
        assert_eq!(credential.len(), 43);
        assert!(!credential.contains('='));
        assert_eq!(body["environment"], "production");
        assert_eq!(body["generation"], 0);

        let db = relay.db.lock().unwrap();
        let (stored_hash, status, generation): (String, String, i64) = db
            .query_row(
                "SELECT bearer_hash, status, generation FROM bindings WHERE token_hash = ?1",
                [token_hash(TOKEN)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            stored_hash,
            bearer_hash(&crate::secret::Secret::new(credential.clone()))
        );
        assert_eq!(status, "active");
        assert_eq!(generation, 0);

        // The installation Apple's object described, including what only a
        // verified attestation can supply.
        let (category, version, trusted): (i64, String, i64) = db
            .query_row(
                "SELECT validation_category, bundle_version, counter_trusted FROM installations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(category, 1);
        assert_eq!(version, "1");
        assert_eq!(trusted, 1);

        let consumed: Option<i64> = db
            .query_row("SELECT consumed_ms FROM challenges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(consumed, Some(REAL_CLOCK_MS));
    }

    /// **Challenge replay.** The same document a second time is the same
    /// attestation with a challenge that has been spent.
    #[test]
    fn a_replayed_challenge_cannot_enrol_a_second_time() {
        let relay = relay(&[]);
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);
        let body = enroll_body(&challenge, TOKEN);

        assert_eq!(
            enrol(&relay, body.as_bytes(), None, REAL_CLOCK_MS).status,
            StatusCode::OK
        );
        let replay = enrol(&relay, body.as_bytes(), None, REAL_CLOCK_MS);
        assert_eq!(replay.status, StatusCode::FORBIDDEN);
        assert_eq!(replay.body["error"], "challenge_consumed");
        assert_eq!(active_count(&relay, TOKEN), 1);
    }

    #[test]
    fn a_challenge_past_its_ten_minutes_is_not_an_enrollment() {
        let relay = relay(&[]);
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);

        let late = enrol(
            &relay,
            enroll_body(&challenge, TOKEN).as_bytes(),
            None,
            REAL_CLOCK_MS + challenge::LIFETIME_MS,
        );
        assert_eq!(late.status, StatusCode::FORBIDDEN);
        assert_eq!(late.body["error"], "challenge_expired");
        assert_eq!(active_count(&relay, TOKEN), 0);
    }

    /// A challenge the relay never issued authorises nothing, even carrying a
    /// genuine attestation object.
    #[test]
    fn an_unissued_challenge_authorises_nothing() {
        let relay = relay(&[]);
        let refused = enrol(
            &relay,
            enroll_body(&real_challenge(), TOKEN).as_bytes(),
            None,
            REAL_CLOCK_MS,
        );
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(refused.body["error"], "challenge_unknown");
    }

    /// **Separate namespaces.** The same object, the same challenge, a relay
    /// configured for the development namespace — and it is refused, because a
    /// development key can be minted by anybody who can sign a build. The token
    /// is offered against the sandbox host, which is the only one a development
    /// namespace may bind, so what refuses it is the AAGUID and nothing earlier.
    #[test]
    fn development_and_production_attestations_are_separate_namespaces() {
        let relay = development_relay(&[]);
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);
        let mut document: serde_json::Value =
            serde_json::from_str(&enroll_body(&challenge, TOKEN)).unwrap();
        document["environment"] = serde_json::json!("sandbox");

        let refused = enrol(&relay, document.to_string().as_bytes(), None, REAL_CLOCK_MS);
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(refused.body["error"], "attestation");
    }

    #[test]
    fn a_relay_without_an_app_id_cannot_verify_anything_and_says_so() {
        let relay = Relay::new(config(&[]), crate::db::open_in_memory().unwrap());
        let refused = enrol(
            &relay,
            enroll_body("x", TOKEN).as_bytes(),
            None,
            REAL_CLOCK_MS,
        );
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.body["error"], "attest_unconfigured");
    }

    #[test]
    fn a_bundle_version_the_app_claims_has_to_be_the_one_the_device_attested() {
        let relay = relay(&[]);
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);
        let mut document: serde_json::Value =
            serde_json::from_str(&enroll_body(&challenge, TOKEN)).unwrap();
        document["bundle_version"] = serde_json::json!("9");

        let refused = enrol(&relay, document.to_string().as_bytes(), None, REAL_CLOCK_MS);
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert_eq!(refused.body["error"], "bundle_version");
    }

    #[test]
    fn a_minimum_bundle_version_is_compared_as_numbers_and_not_as_text() {
        assert!(version_is_at_least("10", "9"));
        assert!(version_is_at_least("1.2.3", "1.2.3"));
        assert!(version_is_at_least("1.2.10", "1.2.9"));
        assert!(!version_is_at_least("1.2", "1.2.1"));
        assert!(!version_is_at_least("9", "10"));
        assert!(!version_is_at_least("beta", "1"));

        let relay = relay(&[("RELAY_MIN_BUNDLE_VERSION", "2")]);
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);
        let refused = enrol(
            &relay,
            enroll_body(&challenge, TOKEN).as_bytes(),
            None,
            REAL_CLOCK_MS,
        );
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert_eq!(refused.body["error"], "bundle_version");
        assert_eq!(active_count(&relay, TOKEN), 0);
    }

    // -----------------------------------------------------------------------
    // The distribution policy, against Apple's whole table.
    // -----------------------------------------------------------------------

    /// **Every category Apple publishes, and what each one means for a relay
    /// serving customers.** Only a TestFlight or App Store launch is a build the
    /// relay is for; an operating-system executable, a development identity, an
    /// enterprise or ad-hoc profile, a Developer ID binary, the restricted
    /// system categories, and any other signing identity are not — and neither
    /// is `0`, which Apple names invalid.
    #[test]
    fn the_only_builds_production_serves_are_testflight_and_the_app_store() {
        let relay = relay(&[]);
        let accepted = accepted_categories(AttestEnvironment::Production);
        let decide = |category: Option<u32>| distribution_policy(&relay, accepted, category, None);

        for (category, what) in [
            (
                TESTFLIGHT_CATEGORY,
                "an executable distributed through TestFlight",
            ),
            (
                APP_STORE_CATEGORY,
                "an executable distributed through the App Store",
            ),
        ] {
            assert_eq!(decide(Some(category)), None, "{category}: {what}");
        }
        for (category, what) in [
            (0, "invalid"),
            (1, "an operating system executable"),
            (
                3,
                "an executable signed by a development code signing identity",
            ),
            (5, "an enterprise universal provisioning profile, or ad-hoc"),
            (6, "signed using Developer ID"),
            (7, "a restricted system-generated category"),
            (8, "a restricted system-generated category"),
            (9, "a restricted system-generated category"),
            (10, "any other code signing identity"),
        ] {
            assert_eq!(
                decide(Some(category)),
                Some(Refusal::Attestation),
                "{category}: {what}"
            );
        }

        // **And absence is not a refusal.** No iOS before 27 writes the
        // extension at all, so a relay that required it would refuse every phone
        // in the world for the version of the OS it is running.
        assert_eq!(decide(None), None);
    }

    /// The other namespace has a rule of its own rather than none: the
    /// development AAGUID is minted by a build signed with a development
    /// identity, and that is the one category it may report.
    #[test]
    fn the_development_namespace_accepts_a_development_identity_and_nothing_else() {
        let relay = development_relay(&[]);
        let accepted = accepted_categories(AttestEnvironment::Development);
        let decide = |category: Option<u32>| distribution_policy(&relay, accepted, category, None);

        assert_eq!(decide(Some(DEVELOPMENT_IDENTITY_CATEGORY)), None);
        assert_eq!(decide(None), None);
        for category in [0, 1, TESTFLIGHT_CATEGORY, APP_STORE_CATEGORY, 5, 6, 10] {
            assert_eq!(
                decide(Some(category)),
                Some(Refusal::Attestation),
                "{category}"
            );
        }
        // The two sets do not overlap, so nothing here is reachable from a
        // production relay.
        assert!(!accepted_categories(AttestEnvironment::Production)
            .contains(&DEVELOPMENT_IDENTITY_CATEGORY));
    }

    /// **The route's own set, against the one object Apple signed.** Its
    /// authenticator data says an operating-system executable made it, so a
    /// relay serving customers refuses it — which is exactly what the seam every
    /// other test uses exists to work around.
    #[test]
    fn apples_published_object_is_refused_for_the_build_it_says_made_it() {
        let relay = relay(&[]);
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);
        let refused = enroll(
            &relay,
            enroll_body(&challenge, TOKEN).as_bytes(),
            None,
            REAL_CLOCK_MS,
        );
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
        assert_eq!(refused.body["error"], "attestation");
        assert_eq!(active_count(&relay, TOKEN), 0);
    }

    // -----------------------------------------------------------------------
    // Replacement, atomicity, and the index that enforces it.
    // -----------------------------------------------------------------------

    fn synthetic(counter: u32, public_key: Vec<u8>) -> VerifiedAttestation {
        VerifiedAttestation {
            public_key,
            key_id: vec![9u8; 32],
            receipt: b"an opaque receipt".to_vec(),
            counter,
            aaguid: Aaguid::PRODUCTION,
            validation_category: Some(APP_STORE_CATEGORY),
            bundle_version: Some("1".to_string()),
        }
    }

    fn record(conn: &mut Connection, key_id_hash: &str, token: &str, bearer: &str, now: i64) {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        record_enrollment(
            &tx,
            &Enrollment {
                verified: &synthetic(0, vec![4u8; 65]),
                key_id_hash,
                attest_environment: AttestEnvironment::Production,
                token_hash: &token_hash(token),
                environment: ApnsEnvironment::Production,
                bearer_hash: &sha256_hex(bearer.as_bytes()),
                generation: 0,
            },
            now,
        )
        .unwrap();
        tx.commit().unwrap();
    }

    /// **A new credential for the same tuple revokes the previous one, in one
    /// transaction.** The attestation path cannot be driven twice — Apple's one
    /// object is bound to one challenge — so the transaction is driven directly,
    /// which is the level the property lives at anyway.
    #[test]
    fn a_replacement_credential_revokes_the_previous_one_atomically() {
        let mut conn = crate::db::open_in_memory().unwrap();
        record(&mut conn, "key-a", TOKEN, "first-bearer", 1);
        record(&mut conn, "key-a", TOKEN, "second-bearer", 2);

        let rows: Vec<(String, String, Option<String>)> = conn
            .prepare("SELECT bearer_hash, status, terminal_reason FROM bindings ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, "revoked");
        assert_eq!(rows[0].2.as_deref(), Some("superseded"));
        assert_eq!(rows[1].1, "active");
        assert_eq!(rows[1].0, sha256_hex(b"second-bearer"));
    }

    /// **The thirty days are enforced where credentials are issued.** A row
    /// retired a month before the next issuance is not in the file afterwards,
    /// and one retired yesterday still is.
    #[test]
    fn a_binding_revoked_a_month_before_the_next_issuance_is_no_longer_in_the_file() {
        let mut conn = crate::db::open_in_memory().unwrap();
        record(&mut conn, "key-a", TOKEN, "old-bearer", NOW);
        conn.execute(
            "UPDATE bindings SET status = 'revoked', terminal_reason = 'deleted', updated_ms = ?1",
            [NOW],
        )
        .unwrap();

        let day = 24 * 60 * 60 * 1_000;
        record(&mut conn, "key-a", OTHER_TOKEN, "recent-bearer", NOW + day);
        conn.execute(
            "UPDATE bindings SET status = 'revoked', terminal_reason = 'deleted', updated_ms = ?1
             WHERE bearer_hash = ?2",
            rusqlite::params![NOW + day, sha256_hex(b"recent-bearer")],
        )
        .unwrap();

        record(
            &mut conn,
            "key-a",
            TOKEN,
            "new-bearer",
            NOW + crate::db::REVOKED_RETENTION_MS,
        );

        let rows: Vec<(String, String)> = conn
            .prepare("SELECT bearer_hash, status FROM bindings ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                (sha256_hex(b"recent-bearer"), "revoked".to_string()),
                (sha256_hex(b"new-bearer"), "active".to_string()),
            ]
        );
    }

    /// **Two enrollments racing on one token cannot leave two live bearers.**
    /// Two connections to one file, because a single connection behind a mutex
    /// would prove the mutex rather than the index.
    #[test]
    fn a_concurrent_second_enrollment_cannot_leave_two_active_rows() {
        let dir = scratch("concurrent");
        let path = dir.join("relay.sqlite");
        crate::db::open(&path).unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for (index, key) in ["key-a", "key-b"].into_iter().enumerate() {
            let path = path.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                let mut conn = crate::db::open(&path).unwrap();
                barrier.wait();
                record(&mut conn, key, TOKEN, &format!("bearer-{index}"), 10);
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let conn = crate::db::open(&path).unwrap();
        let active: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bindings WHERE token_hash = ?1 AND status = 'active'",
                [token_hash(TOKEN)],
                |r| r.get(0),
            )
            .unwrap();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM bindings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(active, 1, "two live bearers for one phone");
        assert_eq!(total, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §7: a token Apple reported gone is not handed another credential.
    #[test]
    fn a_token_apple_called_unregistered_is_not_enrolled_again() {
        let relay = relay(&[]);
        {
            let mut db = relay.db.lock().unwrap();
            record(&mut db, "key-a", TOKEN, "first-bearer", 1);
            db.execute(
                "UPDATE bindings SET status = 'revoked', terminal_reason = 'unregistered'",
                [],
            )
            .unwrap();
        }
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);
        let refused = enrol(
            &relay,
            enroll_body(&challenge, TOKEN).as_bytes(),
            None,
            REAL_CLOCK_MS,
        );
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(refused.body["error"], "token_unregistered");
    }

    // -----------------------------------------------------------------------
    // Assertion-authorised lifecycle.
    //
    // Apple's device key cannot sign anything here, so these installations are
    // recorded with a key minted in-process and the assertions are made with it.
    // What is under test is the lifecycle, not the attestation that admits one.
    // -----------------------------------------------------------------------

    struct Phone {
        key: EcdsaKeyPair,
        key_id: Vec<u8>,
    }

    impl Phone {
        fn new() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
            let key =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            let key_id = sha256(key.public_key().as_ref()).to_vec();
            Phone { key, key_id }
        }

        fn key_id_b64(&self) -> String {
            base64::engine::general_purpose::STANDARD.encode(&self.key_id)
        }

        fn assertion(
            &self,
            operation: Operation,
            challenge: &str,
            token: &str,
            environment: ApnsEnvironment,
            counter: u32,
        ) -> String {
            self.assertion_saying(operation, challenge, token, environment, counter, None)
        }

        /// The same assertion from a phone new enough to append the
        /// distribution extensions to it, naming a category and a version.
        fn assertion_saying(
            &self,
            operation: Operation,
            challenge: &str,
            token: &str,
            environment: ApnsEnvironment,
            counter: u32,
            says: Option<(u32, &str)>,
        ) -> String {
            let mut auth_data = sha256(REAL_APP_ID.as_bytes()).to_vec();
            auth_data.push(0);
            auth_data.extend_from_slice(&counter.to_be_bytes());
            if let Some((category, version)) = says {
                let extensions = Value::Map(vec![
                    (
                        Value::Text("validationCategory".to_string()),
                        Value::Bytes(category.to_le_bytes().to_vec()),
                    ),
                    (
                        Value::Text("bundleVersion".to_string()),
                        Value::Text(version.to_string()),
                    ),
                ]);
                let mut encoded = Vec::new();
                ciborium::ser::into_writer(&extensions, &mut encoded).unwrap();
                auth_data.extend_from_slice(&encoded);
            }

            let challenge_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(challenge)
                .unwrap();
            let client_data =
                assertion_client_data(operation, challenge, &challenge_bytes, token, environment);
            let mut message = auth_data.clone();
            message.extend_from_slice(&client_data);
            let signature = self
                .key
                .sign(&SystemRandom::new(), &sha256(&message))
                .unwrap();

            let mut cbor = Vec::new();
            ciborium::ser::into_writer(
                &Value::Map(vec![
                    (
                        Value::Text("signature".to_string()),
                        Value::Bytes(signature.as_ref().to_vec()),
                    ),
                    (
                        Value::Text("authenticatorData".to_string()),
                        Value::Bytes(auth_data),
                    ),
                ]),
                &mut cbor,
            )
            .unwrap();
            base64::engine::general_purpose::STANDARD.encode(cbor)
        }
    }

    fn sha256(bytes: &[u8]) -> [u8; 32] {
        let mut context = Context::new(&SHA256);
        context.update(bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(context.finish().as_ref());
        out
    }

    /// Record an installation and one active binding, the way an enrollment
    /// would have, and hand back the bearer it minted.
    fn enrolled(relay: &Relay, phone: &Phone, token: &str, now: i64) -> String {
        enrolled_in(relay, phone, token, now, AttestEnvironment::Production)
    }

    /// The same, in whichever App Attest namespace — and therefore against
    /// whichever APNs host Apple pairs with it.
    fn enrolled_in(
        relay: &Relay,
        phone: &Phone,
        token: &str,
        now: i64,
        attest_environment: AttestEnvironment,
    ) -> String {
        let bearer = new_bearer().unwrap();
        let mut db = relay.db.lock().unwrap();
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let mut verified = synthetic(0, phone.key.public_key().as_ref().to_vec());
        verified.validation_category = Some(accepted_categories(attest_environment)[0]);
        record_enrollment(
            &tx,
            &Enrollment {
                verified: &verified,
                key_id_hash: &sha256_hex(&phone.key_id),
                attest_environment,
                token_hash: &token_hash(token),
                environment: bindable_environment(attest_environment),
                bearer_hash: &bearer_hash(&bearer),
                generation: 0,
            },
            now,
        )
        .unwrap();
        tx.commit().unwrap();
        bearer.expose().to_string()
    }

    fn assert_body(
        phone: &Phone,
        operation: Operation,
        challenge: &str,
        token: &str,
        counter: u32,
    ) -> String {
        assert_body_in(
            phone,
            operation,
            challenge,
            token,
            counter,
            ApnsEnvironment::Production,
        )
    }

    fn assert_body_in(
        phone: &Phone,
        operation: Operation,
        challenge: &str,
        token: &str,
        counter: u32,
        environment: ApnsEnvironment,
    ) -> String {
        serde_json::json!({
            "schema": 1,
            "key_id": phone.key_id_b64(),
            "assertion": phone.assertion(operation, challenge, token, environment, counter),
            "challenge": challenge,
            "operation": operation.as_str(),
            "token": token,
            "environment": environment.as_str(),
        })
        .to_string()
    }

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn an_assertion_rotates_a_credential_and_kills_the_previous_one() {
        let relay = relay(&[]);
        let phone = Phone::new();
        let old = enrolled(&relay, &phone, TOKEN, NOW);

        let challenge = issued_challenge(&relay, NOW);
        let reply = lifecycle(
            &relay,
            assert_body(&phone, Operation::Rotate, &challenge, TOKEN, 1).as_bytes(),
            None,
            NOW,
        );
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let new = credential_of(&reply);
        assert_ne!(new, old);
        assert_eq!(active_count(&relay, TOKEN), 1);

        // The previous bearer is a credential the relay still recognises and no
        // longer honours, which is exactly what drives `reissue`.
        assert_eq!(status_of(&relay, &old, NOW).1["status"], "reissue");
        assert_eq!(status_of(&relay, &new, NOW).1["status"], "active");
    }

    #[test]
    fn an_assertion_rebinds_a_changed_token_and_retires_the_old_one() {
        let relay = relay(&[]);
        let phone = Phone::new();
        let old = enrolled(&relay, &phone, TOKEN, NOW);

        let challenge = issued_challenge(&relay, NOW);
        let reply = lifecycle(
            &relay,
            assert_body(&phone, Operation::Rebind, &challenge, OTHER_TOKEN, 1).as_bytes(),
            None,
            NOW,
        );
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(active_count(&relay, TOKEN), 0);
        assert_eq!(active_count(&relay, OTHER_TOKEN), 1);
        assert_eq!(status_of(&relay, &old, NOW).1["status"], "reissue");
        assert_eq!(
            status_of(&relay, &credential_of(&reply), NOW).1["status"],
            "active"
        );
    }

    #[test]
    fn an_assertion_deletes_a_binding_and_a_second_deletion_is_the_same_answer() {
        let relay = relay(&[]);
        let phone = Phone::new();
        let bearer = enrolled(&relay, &phone, TOKEN, NOW);

        for counter in 1..=2 {
            let challenge = issued_challenge(&relay, NOW);
            let reply = lifecycle(
                &relay,
                assert_body(&phone, Operation::Delete, &challenge, TOKEN, counter).as_bytes(),
                None,
                NOW,
            );
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
            assert_eq!(reply.body["status"], "deleted");
        }
        assert_eq!(active_count(&relay, TOKEN), 0);
        assert_eq!(status_of(&relay, &bearer, NOW).1["status"], "reissue");
    }

    /// **The counter is what makes an assertion unrepeatable.** Even with a
    /// fresh challenge, the same signed authenticator data does not advance.
    #[test]
    fn an_assertion_counter_that_does_not_advance_is_refused() {
        let relay = relay(&[]);
        let phone = Phone::new();
        enrolled(&relay, &phone, TOKEN, NOW);

        let first = issued_challenge(&relay, NOW);
        assert_eq!(
            lifecycle(
                &relay,
                assert_body(&phone, Operation::Rotate, &first, TOKEN, 5).as_bytes(),
                None,
                NOW,
            )
            .status,
            StatusCode::OK
        );

        let second = issued_challenge(&relay, NOW);
        let refused = lifecycle(
            &relay,
            assert_body(&phone, Operation::Rotate, &second, TOKEN, 5).as_bytes(),
            None,
            NOW,
        );
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(refused.body["error"], "assertion");
    }

    /// **The operation is inside the signature.** An assertion made to delete a
    /// binding is not an authorisation to rebind it somewhere else.
    #[test]
    fn an_assertion_for_one_operation_does_not_authorise_another() {
        let relay = relay(&[]);
        let phone = Phone::new();
        enrolled(&relay, &phone, TOKEN, NOW);

        let challenge = issued_challenge(&relay, NOW);
        let signed_for_delete = phone.assertion(
            Operation::Delete,
            &challenge,
            TOKEN,
            ApnsEnvironment::Production,
            1,
        );
        let document = serde_json::json!({
            "schema": 1,
            "key_id": phone.key_id_b64(),
            "assertion": signed_for_delete,
            "challenge": challenge,
            "operation": "rotate",
            "token": TOKEN,
            "environment": "production",
        })
        .to_string();

        let refused = lifecycle(&relay, document.as_bytes(), None, NOW);
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(refused.body["error"], "assertion");
        assert_eq!(active_count(&relay, TOKEN), 1);
    }

    #[test]
    fn an_assertion_cannot_take_over_another_installations_token() {
        let relay = relay(&[]);
        let owner = Phone::new();
        let stranger = Phone::new();
        enrolled(&relay, &owner, TOKEN, NOW);
        enrolled(&relay, &stranger, OTHER_TOKEN, NOW);

        for operation in [Operation::Rotate, Operation::Rebind] {
            let challenge = issued_challenge(&relay, NOW);
            let refused = lifecycle(
                &relay,
                assert_body(&stranger, operation, &challenge, TOKEN, 1).as_bytes(),
                None,
                NOW,
            );
            assert_eq!(refused.status, StatusCode::FORBIDDEN, "{operation:?}");
            assert_eq!(refused.body["error"], "forbidden");
        }
        assert_eq!(active_count(&relay, TOKEN), 1);
    }

    #[test]
    fn an_assertion_from_an_installation_the_relay_has_never_seen_is_refused() {
        let relay = relay(&[]);
        let phone = Phone::new();
        let challenge = issued_challenge(&relay, NOW);
        let refused = lifecycle(
            &relay,
            assert_body(&phone, Operation::Rotate, &challenge, TOKEN, 1).as_bytes(),
            None,
            NOW,
        );
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(refused.body["error"], "installation_unknown");
    }

    /// §7: after a restore the counters are untrusted, and the next assertion
    /// re-establishes them rather than being refused for going backwards.
    #[test]
    fn a_restored_counter_is_untrusted_until_one_assertion_re_establishes_it() {
        let relay = relay(&[]);
        let phone = Phone::new();
        enrolled(&relay, &phone, TOKEN, NOW);
        {
            let db = relay.db.lock().unwrap();
            db.execute(
                "UPDATE installations SET counter = 900, counter_trusted = 0",
                [],
            )
            .unwrap();
        }

        let challenge = issued_challenge(&relay, NOW);
        let accepted = lifecycle(
            &relay,
            assert_body(&phone, Operation::Rotate, &challenge, TOKEN, 3).as_bytes(),
            None,
            NOW,
        );
        assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.body);

        let db = relay.db.lock().unwrap();
        let (counter, trusted): (i64, i64) = db
            .query_row(
                "SELECT counter, counter_trusted FROM installations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(counter, 3);
        assert_eq!(trusted, 1, "the next assertion is measured against three");
    }

    // -----------------------------------------------------------------------
    // The distribution policy on the far side of enrollment.
    // -----------------------------------------------------------------------

    /// **A minimum raised afterwards reaches the installations that are already
    /// there.** An App Attest key outlives the build that minted it, so an
    /// installation admitted under the old minimum would otherwise keep full
    /// lifecycle authority over its binding for as long as the phone held the
    /// key — and the operator who raised the minimum would have changed nothing
    /// except which phones may enrol next.
    #[test]
    fn a_raised_minimum_bundle_version_reaches_an_installation_that_already_attested() {
        let relay = relay(&[("RELAY_MIN_BUNDLE_VERSION", "2")]);
        let phone = Phone::new();
        let bearer = enrolled(&relay, &phone, TOKEN, NOW);

        for (operation, token) in [(Operation::Rotate, TOKEN), (Operation::Rebind, OTHER_TOKEN)] {
            let challenge = issued_challenge(&relay, NOW);
            let refused = lifecycle(
                &relay,
                assert_body(&phone, operation, &challenge, token, 1).as_bytes(),
                None,
                NOW,
            );
            assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{operation:?}");
            assert_eq!(refused.body["error"], "bundle_version");
        }
        assert_eq!(active_count(&relay, TOKEN), 1);
        assert_eq!(status_of(&relay, &bearer, NOW).1["status"], "active");

        // **And a phone may still give up its credential.** §7 answers a lost
        // Mac by rotating the shared bearer and an incident by revoking it, so a
        // rule that stood in the way of a revocation would keep alive exactly
        // the credential an operator is trying to retire.
        let challenge = issued_challenge(&relay, NOW);
        let deleted = lifecycle(
            &relay,
            assert_body(&phone, Operation::Delete, &challenge, TOKEN, 1).as_bytes(),
            None,
            NOW,
        );
        assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
        assert_eq!(active_count(&relay, TOKEN), 0);
    }

    /// The same rule for the category: an installation whose recorded build is
    /// not one the relay serves cannot mint another credential with the key it
    /// already holds.
    #[test]
    fn an_installation_whose_recorded_category_is_not_served_cannot_mint_another_credential() {
        let relay = relay(&[]);
        let phone = Phone::new();
        enrolled(&relay, &phone, TOKEN, NOW);
        {
            let db = relay.db.lock().unwrap();
            db.execute(
                "UPDATE installations SET validation_category = ?1",
                [i64::from(DEVELOPMENT_IDENTITY_CATEGORY)],
            )
            .unwrap();
        }

        for (operation, token) in [(Operation::Rotate, TOKEN), (Operation::Rebind, OTHER_TOKEN)] {
            let challenge = issued_challenge(&relay, NOW);
            let refused = lifecycle(
                &relay,
                assert_body(&phone, operation, &challenge, token, 1).as_bytes(),
                None,
                NOW,
            );
            assert_eq!(refused.status, StatusCode::FORBIDDEN, "{operation:?}");
            assert_eq!(refused.body["error"], "attestation");
        }
        assert_eq!(active_count(&relay, TOKEN), 1);

        let challenge = issued_challenge(&relay, NOW);
        let deleted = lifecycle(
            &relay,
            assert_body(&phone, Operation::Delete, &challenge, TOKEN, 1).as_bytes(),
            None,
            NOW,
        );
        assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
        assert_eq!(active_count(&relay, TOKEN), 0);
    }

    /// **A phone that has been updated says so, and is believed.** iOS 27
    /// appends the distribution extensions to every assertion, and those describe
    /// the build running now — so the phone meets a minimum raised after it
    /// enrolled with the key it already holds, instead of being sent through an
    /// attestation that would tell the relay nothing it is not being told here.
    #[test]
    fn an_assertion_that_names_a_newer_build_meets_a_minimum_raised_after_enrollment() {
        let relay = relay(&[("RELAY_MIN_BUNDLE_VERSION", "2")]);
        let phone = Phone::new();
        enrolled(&relay, &phone, TOKEN, NOW);

        let challenge = issued_challenge(&relay, NOW);
        let document = serde_json::json!({
            "schema": 1,
            "key_id": phone.key_id_b64(),
            "assertion": phone.assertion_saying(
                Operation::Rotate,
                &challenge,
                TOKEN,
                ApnsEnvironment::Production,
                1,
                Some((APP_STORE_CATEGORY, "3")),
            ),
            "challenge": challenge,
            "operation": "rotate",
            "token": TOKEN,
            "environment": "production",
        })
        .to_string();

        let reply = lifecycle(&relay, document.as_bytes(), None, NOW);
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(active_count(&relay, TOKEN), 1);

        // And the row now says what the phone said, so the next assertion does
        // not have to repeat it.
        let db = relay.db.lock().unwrap();
        let (category, version): (i64, String) = db
            .query_row(
                "SELECT validation_category, bundle_version FROM installations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(category, i64::from(APP_STORE_CATEGORY));
        assert_eq!(version, "3");
    }

    // -----------------------------------------------------------------------
    // Namespaces.
    // -----------------------------------------------------------------------

    /// **A record from the other namespace is not there.** Without this the
    /// configuration decides only which attestations are accepted from now on,
    /// and every installation the relay ever admitted keeps rebind, rotate and
    /// delete over its binding whichever world the relay is now serving.
    #[test]
    fn an_installation_from_the_other_namespace_is_not_found_and_cannot_act() {
        for (configured, recorded) in [
            (
                AttestEnvironment::Production,
                AttestEnvironment::Development,
            ),
            (
                AttestEnvironment::Development,
                AttestEnvironment::Production,
            ),
        ] {
            let relay = match configured {
                AttestEnvironment::Production => relay(&[]),
                AttestEnvironment::Development => development_relay(&[]),
            };
            let phone = Phone::new();
            let bearer = enrolled_in(&relay, &phone, TOKEN, NOW, recorded);
            let environment = bindable_environment(configured);

            for operation in [Operation::Rotate, Operation::Rebind, Operation::Delete] {
                let challenge = issued_challenge(&relay, NOW);
                let refused = lifecycle(
                    &relay,
                    assert_body_in(&phone, operation, &challenge, TOKEN, 1, environment).as_bytes(),
                    None,
                    NOW,
                );
                assert_eq!(refused.status, StatusCode::FORBIDDEN, "{configured:?}");
                assert_eq!(refused.body["error"], "installation_unknown");
            }

            // And its bearer is not a credential this relay answers for either:
            // the phone is told to attest again, which is the only thing that
            // can make it real here.
            assert_eq!(status_of(&relay, &bearer, NOW).1["status"], "reenroll");

            // The row is untouched — it is unreachable, not deleted, so pointing
            // the relay back at its own namespace restores it.
            assert_eq!(active_count(&relay, TOKEN), 1);
        }
    }

    /// **The pairing Apple makes twice.** A development profile is given the
    /// development App Attest environment and the sandbox APNs host; TestFlight
    /// and the App Store are given the production environment and the production
    /// host. A relay that let one half of either pair meet the other would let a
    /// key anybody can mint take a binding on a customer's phone.
    #[test]
    fn a_namespace_may_bind_only_the_apns_host_apple_pairs_it_with() {
        assert_eq!(
            bindable_environment(AttestEnvironment::Development),
            ApnsEnvironment::Sandbox
        );
        assert_eq!(
            bindable_environment(AttestEnvironment::Production),
            ApnsEnvironment::Production
        );

        // The development relay, offered the production host.
        let development = development_relay(&[]);
        let challenge = seed_real_challenge(&development, REAL_CLOCK_MS);
        let refused = enrol(
            &development,
            enroll_body(&challenge, TOKEN).as_bytes(),
            None,
            REAL_CLOCK_MS,
        );
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
        assert_eq!(refused.body["error"], "environment");

        // And the production relay, offered the sandbox one.
        let relay = relay(&[]);
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);
        let mut document: serde_json::Value =
            serde_json::from_str(&enroll_body(&challenge, TOKEN)).unwrap();
        document["environment"] = serde_json::json!("sandbox");
        let refused = enrol(&relay, document.to_string().as_bytes(), None, REAL_CLOCK_MS);
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
        assert_eq!(refused.body["error"], "environment");

        // A rebind cannot walk a binding across the pair either.
        let phone = Phone::new();
        enrolled(&relay, &phone, TOKEN, NOW);
        let challenge = issued_challenge(&relay, NOW);
        let refused = lifecycle(
            &relay,
            assert_body_in(
                &phone,
                Operation::Rebind,
                &challenge,
                OTHER_TOKEN,
                1,
                ApnsEnvironment::Sandbox,
            )
            .as_bytes(),
            None,
            NOW,
        );
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert_eq!(refused.body["error"], "environment");
        assert_eq!(active_count(&relay, OTHER_TOKEN), 0);
    }

    // -----------------------------------------------------------------------
    // The generation floor.
    // -----------------------------------------------------------------------

    /// **The mechanism that makes a database restore fail closed.** A credential
    /// minted under a floor the operator has since raised is refused, and the
    /// status endpoint sends the phone through App Attest rather than through a
    /// rotation it could perform with a key the relay may no longer know.
    #[test]
    fn a_bearer_below_the_floor_is_refused_and_the_phone_is_told_to_re_enrol() {
        let dir = scratch("floor");
        let floor_file = dir.join("generation-floor");
        std::fs::write(&floor_file, "7\n").unwrap();

        let relay = Relay::new(
            config(&[
                ("RELAY_APP_ID", REAL_APP_ID),
                ("RELAY_ATTEST_ENVIRONMENT", "production"),
                ("RELAY_GENERATION_FLOOR_FILE", floor_file.to_str().unwrap()),
            ]),
            crate::db::open_in_memory().unwrap(),
        );
        let phone = Phone::new();
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);
        let reply = enrol(
            &relay,
            enroll_body(&challenge, TOKEN).as_bytes(),
            None,
            REAL_CLOCK_MS,
        );
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(reply.body["generation"], 7);
        let credential = credential_of(&reply);
        assert_eq!(
            status_of(&relay, &credential, REAL_CLOCK_MS).1["status"],
            "active"
        );

        // The incident: the floor goes up, on the next request and not on the
        // next restart.
        std::fs::write(&floor_file, "8\n").unwrap();
        let (status, body) = status_of(&relay, &credential, REAL_CLOCK_MS);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "reenroll");

        // And an assertion cannot step over the floor either: the phone attests
        // again, which is the whole point of raising it.
        {
            let mut db = relay.db.lock().unwrap();
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            // The phone that will sign the assertions, reporting a category the
            // route accepts — so that what refuses it below is the floor.
            tx.execute(
                "UPDATE installations
                    SET public_key = ?1, key_id_hash = ?2, validation_category = ?3",
                rusqlite::params![
                    phone.key.public_key().as_ref().to_vec(),
                    sha256_hex(&phone.key_id),
                    i64::from(APP_STORE_CATEGORY)
                ],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        for (operation, token) in [(Operation::Rotate, TOKEN), (Operation::Rebind, OTHER_TOKEN)] {
            let challenge = issued_challenge(&relay, REAL_CLOCK_MS);
            let refused = lifecycle(
                &relay,
                assert_body(&phone, operation, &challenge, token, 1).as_bytes(),
                None,
                REAL_CLOCK_MS,
            );
            assert_eq!(refused.status, StatusCode::FORBIDDEN, "{operation:?}");
            assert_eq!(refused.body["error"], "binding_unknown");
        }
        // The one operation a raised floor does not stand in the way of.
        let challenge = issued_challenge(&relay, REAL_CLOCK_MS);
        let deleted = lifecycle(
            &relay,
            assert_body(&phone, Operation::Delete, &challenge, TOKEN, 1).as_bytes(),
            None,
            REAL_CLOCK_MS,
        );
        assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
        assert_eq!(active_count(&relay, TOKEN), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // The kill switch.
    // -----------------------------------------------------------------------

    /// Enrollment and rotation stop; a credential that already exists keeps
    /// working, the status endpoint keeps answering, and a revocation still runs
    /// — refusing that would keep a compromised bearer alive during the incident
    /// the switch was thrown for.
    #[test]
    fn the_enrollment_kill_switch_stops_issuance_and_nothing_else() {
        let relay = relay(&[("RELAY_ENROLLMENT_ENABLED", "false")]);
        let phone = Phone::new();
        let bearer = enrolled(&relay, &phone, TOKEN, NOW);

        let refused = enrol(&relay, enroll_body("anything", TOKEN).as_bytes(), None, NOW);
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.body["error"], "enrollment_disabled");

        let challenge = issued_challenge(&relay, NOW);
        for operation in [Operation::Rotate, Operation::Rebind] {
            let refused = lifecycle(
                &relay,
                assert_body(&phone, operation, &challenge, TOKEN, 1).as_bytes(),
                None,
                NOW,
            );
            assert_eq!(
                refused.status,
                StatusCode::SERVICE_UNAVAILABLE,
                "{operation:?}"
            );
            assert_eq!(refused.body["error"], "enrollment_disabled");
        }

        assert_eq!(status_of(&relay, &bearer, NOW).1["status"], "active");

        let deleted = lifecycle(
            &relay,
            assert_body(&phone, Operation::Delete, &challenge, TOKEN, 1).as_bytes(),
            None,
            NOW,
        );
        assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
        assert_eq!(active_count(&relay, TOKEN), 0);
    }

    // -----------------------------------------------------------------------
    // Status.
    // -----------------------------------------------------------------------

    #[test]
    fn the_status_endpoint_answers_exactly_the_four_documented_states() {
        let relay = relay(&[]);
        let phone = Phone::new();
        let bearer = enrolled(&relay, &phone, TOKEN, NOW);
        assert_eq!(status_of(&relay, &bearer, NOW).1["status"], "active");
        assert_eq!(
            status_of(&relay, &bearer, NOW).1["environment"],
            "production"
        );

        // A bearer the relay never minted, which is what a restored database
        // looks like from the phone's side.
        assert_eq!(
            status_of(&relay, "aNeverIssuedCredentialValue", NOW).1["status"],
            "reenroll"
        );

        {
            let db = relay.db.lock().unwrap();
            db.execute(
                "UPDATE bindings SET status = 'revoked', terminal_reason = 'rotated'",
                [],
            )
            .unwrap();
        }
        assert_eq!(status_of(&relay, &bearer, NOW).1["status"], "reissue");

        {
            let db = relay.db.lock().unwrap();
            db.execute("UPDATE bindings SET terminal_reason = 'unregistered'", [])
                .unwrap();
        }
        assert_eq!(status_of(&relay, &bearer, NOW).1["status"], "token_invalid");
    }

    #[test]
    fn a_missing_or_shapeless_authorization_header_is_the_only_401() {
        let relay = relay(&[]);
        for header in [None, Some("Basic abc"), Some("Bearer"), Some("Bearer ")] {
            let reply = credential_status(&relay, header, None, NOW);
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{header:?}");
            assert_eq!(reply.body["error"], "unauthorized");
        }
        // The scheme is case-insensitive, as the specification says.
        let phone = Phone::new();
        let bearer = enrolled(&relay, &phone, TOKEN, NOW);
        let reply = credential_status(&relay, Some(&format!("bearer {bearer}")), None, NOW);
        assert_eq!(reply.body["status"], "active");
    }

    /// **The status bearer is read-only.** It answers a question and cannot
    /// rebind, revoke, reset a generation, extend a lifetime, or mint anything:
    /// every row outside the rate table is byte for byte what it was.
    #[test]
    fn the_status_bearer_cannot_change_anything() {
        let relay = relay(&[]);
        let phone = Phone::new();
        let bearer = enrolled(&relay, &phone, TOKEN, NOW);

        let snapshot = |relay: &Relay| -> Vec<String> {
            let db = relay.db.lock().unwrap();
            let mut rows = Vec::new();
            for table in ["bindings", "installations", "challenges"] {
                let mut statement = db
                    .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
                    .unwrap();
                let columns = statement.column_count();
                let mapped = statement
                    .query_map([], |row| {
                        let mut line = String::new();
                        for index in 0..columns {
                            line.push_str(&format!("{:?}|", row.get_ref(index).unwrap()));
                        }
                        Ok(line)
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                rows.extend(mapped);
            }
            rows
        };

        let before = snapshot(&relay);
        for _ in 0..3 {
            assert_eq!(status_of(&relay, &bearer, NOW).1["status"], "active");
        }
        assert_eq!(before, snapshot(&relay));

        // And it is not a lifecycle credential: the assert route reads no
        // bearer at all, so holding one buys nothing there.
        let document = serde_json::json!({
            "schema": 1,
            "key_id": phone.key_id_b64(),
            "assertion": bearer,
            "challenge": issued_challenge(&relay, NOW),
            "operation": "rotate",
            "token": TOKEN,
            "environment": "production",
        })
        .to_string();
        let refused = lifecycle(&relay, document.as_bytes(), None, NOW);
        assert!(refused.status.is_client_error(), "{}", refused.status);
        assert_eq!(active_count(&relay, TOKEN), 1);
        assert_eq!(status_of(&relay, &bearer, NOW).1["status"], "active");
    }

    // -----------------------------------------------------------------------
    // Abuse limits.
    // -----------------------------------------------------------------------

    /// **The acceptance gate: rotation cannot reset the token's budget.** The
    /// rate key is the token's hash, so the credential the caller holds — which
    /// a rotation replaces — is not part of it.
    #[test]
    fn rotating_a_credential_does_not_reset_the_tokens_budget() {
        let relay = relay(&[]);
        let phone = Phone::new();
        enrolled(&relay, &phone, TOKEN, NOW);
        let key = ratelimit::binding_key(&token_hash(TOKEN));

        let spent = |relay: &Relay| -> i64 {
            let db = relay.db.lock().unwrap();
            db.query_row(
                "SELECT day_count FROM rate_buckets WHERE bucket_key = ?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap_or(0)
        };

        let challenge = issued_challenge(&relay, NOW);
        let before = spent(&relay);
        let reply = lifecycle(
            &relay,
            assert_body(&phone, Operation::Rotate, &challenge, TOKEN, 1).as_bytes(),
            None,
            NOW,
        );
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let after = spent(&relay);

        assert!(after > before, "the rotation spent from the token's budget");
        // One bucket, and it is the token's: a new credential did not create a
        // second one to spend from.
        let db = relay.db.lock().unwrap();
        let buckets: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM rate_buckets WHERE bucket_key LIKE 'binding:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(buckets, 1);
    }

    /// **Naming somebody's token does not spend it.** §5 says a stolen APNs
    /// token "alone cannot use the relay" — and a caller that could charge the
    /// phone's five hundred by putting its token in an enrolment nothing
    /// verifies could still silence it for a day, from a handful of addresses.
    /// So the pre-verification path has its own bucket and the phone's is
    /// untouched by every attempt that proves nothing.
    #[test]
    fn an_unverifiable_enrolment_or_assertion_naming_a_token_never_spends_its_budget() {
        let relay = relay(&[]);
        let phone = Phone::new();
        let victim = enrolled(&relay, &phone, TOKEN, NOW);
        let spent = |key: String| -> i64 {
            let db = relay.db.lock().unwrap();
            db.query_row(
                "SELECT day_count FROM rate_buckets WHERE bucket_key = ?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap_or(0)
        };
        let before = spent(ratelimit::binding_key(&token_hash(TOKEN)));

        // The flood, from rotated addresses so the address rule is not what
        // stops it — which is the attacker's whole advantage here.
        let enrolment = enroll_body("never-issued", TOKEN);
        let stranger = Phone::new();
        for index in 0..40u32 {
            let address = format!("198.51.100.{}", index % 200);
            let refused = enrol(&relay, enrolment.as_bytes(), Some(&address), NOW);
            assert_ne!(refused.status, StatusCode::OK, "{}", refused.body);
            let challenge = issued_challenge(&relay, NOW);
            let refused = lifecycle(
                &relay,
                assert_body(&stranger, Operation::Rotate, &challenge, TOKEN, 1).as_bytes(),
                Some(&address),
                NOW,
            );
            assert_ne!(refused.status, StatusCode::OK, "{}", refused.body);
        }

        assert_eq!(
            spent(ratelimit::binding_key(&token_hash(TOKEN))),
            before,
            "the victim's budget paid for somebody else's guessing"
        );
        // What did pay is the bucket for a token nobody has proved they hold.
        assert!(spent(ratelimit::unverified_key(&token_hash(TOKEN))) > 0);

        // And the phone can still spend the day it was never charged for.
        for attempt in 0..ratelimit::BINDING.burst {
            let reply = credential_status(
                &relay,
                Some(&format!("Bearer {victim}")),
                Some("203.0.113.7"),
                NOW,
            );
            assert_eq!(reply.body["status"], "active", "attempt {attempt}");
        }
    }

    /// **A verified operation still spends the token's own budget**, so §3's
    /// rule that a rotation cannot buy a fresh five hundred is unchanged: the
    /// bucket is charged on the far side of the signature rather than not at
    /// all.
    #[test]
    fn a_verified_rotation_still_spends_the_tokens_budget() {
        let relay = relay(&[]);
        let phone = Phone::new();
        enrolled(&relay, &phone, TOKEN, NOW);
        let key = ratelimit::binding_key(&token_hash(TOKEN));
        let spent = || -> i64 {
            let db = relay.db.lock().unwrap();
            db.query_row(
                "SELECT day_count FROM rate_buckets WHERE bucket_key = ?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap_or(0)
        };

        let challenge = issued_challenge(&relay, NOW);
        let before = spent();
        let reply = lifecycle(
            &relay,
            assert_body(&phone, Operation::Rotate, &challenge, TOKEN, 1).as_bytes(),
            None,
            NOW,
        );
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(spent(), before + 1);
    }

    #[test]
    fn a_flood_of_challenges_from_one_address_answers_429_with_retry_after() {
        let relay = relay(&[]);
        let document = br#"{"schema":1}"#;
        for _ in 0..ratelimit::CHALLENGE_IP.burst {
            assert_eq!(
                issue_challenge(&relay, document, Some("203.0.113.7"), NOW).status,
                StatusCode::OK
            );
        }
        let refused = issue_challenge(&relay, document, Some("203.0.113.7"), NOW);
        assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refused.body["error"], "rate_limited");
        assert!(refused.retry_after_seconds.unwrap() > 0);

        // Another address has its own budget, which is what makes the limit a
        // limit rather than an outage.
        assert_eq!(
            issue_challenge(&relay, document, Some("198.51.100.4"), NOW).status,
            StatusCode::OK
        );
    }

    #[test]
    fn a_flood_of_enrollments_from_one_address_answers_429_with_retry_after() {
        let relay = relay(&[]);
        let document = enroll_body("never-issued", TOKEN);
        for _ in 0..ratelimit::ENROLL_IP.burst {
            let reply = enrol(&relay, document.as_bytes(), Some("203.0.113.7"), NOW);
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
        }
        let refused = enrol(&relay, document.as_bytes(), Some("203.0.113.7"), NOW);
        assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(refused.retry_after_seconds.unwrap() > 0);
    }

    /// Guessing bearers is counted against the strict rotating-address rule and
    /// not against a binding the guesser does not have.
    #[test]
    fn guessing_bearers_meets_the_stricter_invalid_auth_limit() {
        let relay = relay(&[]);
        for attempt in 0..ratelimit::INVALID_AUTH_IP.burst {
            let reply = credential_status(&relay, Some("Bearer guess"), Some("203.0.113.7"), NOW);
            assert_eq!(reply.status, StatusCode::OK, "attempt {attempt}");
            assert_eq!(reply.body["status"], "reenroll");
        }
        let refused = credential_status(&relay, Some("Bearer guess"), Some("203.0.113.7"), NOW);
        assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(refused.retry_after_seconds.unwrap() > 0);

        // A real credential from the same address is unaffected: the two rules
        // are different buckets.
        let phone = Phone::new();
        let bearer = enrolled(&relay, &phone, TOKEN, NOW);
        let reply = credential_status(
            &relay,
            Some(&format!("Bearer {bearer}")),
            Some("203.0.113.7"),
            NOW,
        );
        assert_eq!(reply.body["status"], "active");
    }

    // -----------------------------------------------------------------------
    // Strictness.
    // -----------------------------------------------------------------------

    #[test]
    fn every_lifecycle_document_is_strict_about_what_it_will_read() {
        let relay = relay(&[]);
        // Address limits are spent before a document is parsed, which is the
        // point of them, so each case here comes from its own address.
        let caller = std::cell::Cell::new(0u32);
        let from = || {
            caller.set(caller.get() + 1);
            format!("198.51.100.{}", caller.get())
        };

        for document in [
            r#"{"schema":1,"title":"x"}"#,
            r#"{"schema":2}"#,
            r#"{}"#,
            r#"not json"#,
        ] {
            let reply = issue_challenge(&relay, document.as_bytes(), Some(&from()), NOW);
            assert_ne!(reply.status, StatusCode::OK, "{document}");
        }

        let base: serde_json::Value =
            serde_json::from_str(&enroll_body("a-challenge", TOKEN)).unwrap();
        let refusal = |document: String| -> String {
            let reply = enrol(&relay, document.as_bytes(), Some(&from()), NOW);
            reply.body["error"].as_str().unwrap().to_string()
        };
        let with = |field: &str, value: serde_json::Value| {
            let mut document = base.clone();
            document[field] = value;
            document.to_string()
        };
        let without = |field: &str| {
            let mut document = base.clone();
            document.as_object_mut().unwrap().remove(field);
            document.to_string()
        };

        // Nothing that is not in the document may be added to it.
        for field in ["title", "body", "aps", "project", "session_uid", "path"] {
            assert_eq!(
                refusal(with(field, serde_json::json!("x"))),
                "malformed",
                "{field}"
            );
        }
        // And nothing that is in it may be left out.
        for field in [
            "schema",
            "key_id",
            "attestation",
            "challenge",
            "token",
            "environment",
        ] {
            assert_eq!(refusal(without(field)), "malformed", "{field}");
        }

        assert_eq!(refusal(with("schema", serde_json::json!(2))), "schema");
        for bad in ["", "nothex", "AABBCC"] {
            assert_eq!(
                refusal(with("token", serde_json::json!(bad))),
                "token",
                "{bad}"
            );
        }
        // An environment the relay does not know is refused rather than guessed,
        // because this value establishes the binding.
        for bad in ["", "prod", "staging", "Production"] {
            assert_eq!(
                refusal(with("environment", serde_json::json!(bad))),
                "environment",
                "{bad}"
            );
        }
        assert_eq!(
            refusal(with("key_id", serde_json::json!("not base64!"))),
            "key_id"
        );
        assert_eq!(
            refusal(with(
                "attestation",
                serde_json::json!("x".repeat(MAX_ATTESTATION_CHARS + 1))
            )),
            "malformed"
        );

        // An operation outside the closed set is not an operation.
        for operation in ["revoke", "reset", "ROTATE"] {
            let document = serde_json::json!({
                "schema": 1,
                "key_id": "AAAA",
                "assertion": "AAAA",
                "challenge": "AAAA",
                "operation": operation,
                "token": TOKEN,
                "environment": "production",
            })
            .to_string();
            assert_eq!(
                lifecycle(&relay, document.as_bytes(), Some(&from()), NOW).body["error"],
                "malformed",
                "{operation}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // The routes, as the service actually exposes them.
    // -----------------------------------------------------------------------

    async fn call(relay: Relay, request: Request<Body>) -> (StatusCode, String) {
        let response = crate::api::router(relay).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn the_lifecycle_routes_are_mounted_with_the_methods_they_document() {
        let (status, body) = call(
            relay(&[]),
            Request::builder()
                .method("POST")
                .uri("/v1/attest/challenge")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"schema":1}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["expires_in_seconds"], 600);
        assert_eq!(parsed["challenge"].as_str().unwrap().len(), 43);

        // A method the route does not offer is not a route that does not exist.
        for (method, path) in [
            ("GET", "/v1/attest/challenge"),
            ("GET", "/v1/attest/enroll"),
            ("GET", "/v1/attest/assert"),
            ("POST", "/v1/credential/status"),
        ] {
            let (status, _) = call(
                relay(&[]),
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
        }

        let (status, _) = call(
            relay(&[]),
            Request::builder()
                .uri("/v1/attest/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// **The acceptance gate says a 429 carries `Retry-After`, and a header is
    /// not a field.** Every other rate-limit test reads `retry_after_seconds`
    /// — the number on the way to the response — so deleting the one line in
    /// `IntoResponse` that turns it into a header would leave the whole suite
    /// green and the wire contract broken. This drives a real refusal through
    /// the router and reads the response Apple's caller would.
    #[tokio::test]
    async fn a_429_carries_the_retry_after_header_on_the_response_and_not_only_in_the_reply() {
        let relay = relay(&[]);
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/v1/attest/challenge")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"schema":1}"#))
                .unwrap()
        };

        let mut refused = None;
        for _ in 0..=ratelimit::CHALLENGE_IP.burst {
            let response = crate::api::router(relay.clone())
                .oneshot(request())
                .await
                .unwrap();
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                refused = Some(response);
                break;
            }
        }
        let refused = refused.expect("the address budget was never exhausted");

        let seconds: u64 = refused
            .headers()
            .get(header::RETRY_AFTER)
            .expect("a 429 without Retry-After is a 429 the plan does not allow")
            .to_str()
            .expect("Retry-After is ASCII")
            .parse()
            .expect("Retry-After is a whole number of seconds");
        assert!(seconds > 0, "{seconds}");

        // And a response that is not a refusal does not carry one, which is what
        // keeps the header meaningful.
        let allowed = crate::api::router(relay)
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.headers().get(header::RETRY_AFTER), None);
    }

    /// **The enrollment route needs a larger body than the push route, and only
    /// the enrollment route gets one.** A single router-wide limit would either
    /// refuse every attestation or let a kilobyte of push become twenty.
    #[tokio::test]
    async fn only_the_enrollment_route_accepts_a_body_larger_than_a_kilobyte() {
        let long = "A".repeat(2_000);
        let document = serde_json::json!({
            "schema": 1,
            "key_id": REAL_KEY_ID,
            "attestation": long,
            "challenge": "AAAA",
            "token": TOKEN,
            "environment": "production",
        })
        .to_string();
        assert!(document.len() > crate::dto::MAX_BODY_BYTES);

        let (status, _) = call(
            relay(&[]),
            Request::builder()
                .method("POST")
                .uri("/v1/attest/enroll")
                .header("content-type", "application/json")
                .body(Body::from(document.clone()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "the body was read");

        let (status, _) = call(
            relay(&[]),
            Request::builder()
                .method("POST")
                .uri("/v1/attest/challenge")
                .header("content-type", "application/json")
                .body(Body::from(document))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

        let (status, _) = call(
            relay(&[]),
            Request::builder()
                .method("POST")
                .uri("/v1/attest/enroll")
                .header("content-type", "application/json")
                .body(Body::from("A".repeat(ENROLL_BODY_BYTES + 1)))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// **A caller cannot choose its own rate bucket.** The entries it can write
    /// are to the left of the one the deployment's proxy appended, so the
    /// rightmost is the only one read — otherwise the limit is reset by a header.
    #[test]
    fn the_client_address_is_the_entry_the_proxy_observed() {
        let header = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(FORWARDED_FOR, HeaderValue::from_str(value).unwrap());
            headers
        };
        assert_eq!(
            client_address(&header("198.51.100.1, 203.0.113.7")),
            Some("203.0.113.7")
        );
        assert_eq!(
            client_address(&header("  203.0.113.7  ")),
            Some("203.0.113.7")
        );
        // The forged prefix changes nothing, which is the property.
        let forged = header("anything-i-like, 203.0.113.7");
        let plain = header("203.0.113.7");
        assert_eq!(client_address(&forged), client_address(&plain));

        assert_eq!(client_address(&header("")), None);
        assert_eq!(client_address(&header(&"9".repeat(46))), None);
        assert_eq!(client_address(&HeaderMap::new()), None);

        // **And the same header on a second line, which is the shape a proxy
        // that appends rather than merges produces.** HTTP says the two are the
        // same message, so a reader that took the first line would be one an
        // attacker resets by sending its own line — and whether the deployment's
        // proxy merges or appends is not a thing this relay gets to assume.
        let lines = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(FORWARDED_FOR, HeaderValue::from_str(value).unwrap());
            }
            headers
        };
        assert_eq!(
            client_address(&lines(&["198.51.100.1", "203.0.113.7"])),
            Some("203.0.113.7")
        );
        assert_eq!(
            client_address(&lines(&["anything-i-like, 192.0.2.1", "203.0.113.7"])),
            Some("203.0.113.7")
        );
        assert_eq!(
            client_address(&lines(&["192.0.2.1", "198.51.100.1, 203.0.113.7"])),
            Some("203.0.113.7")
        );
        // Three lines of forgery in front of it changes nothing either.
        assert_eq!(
            client_address(&lines(&["a", "b", "c", "203.0.113.7"])),
            client_address(&header("203.0.113.7"))
        );
    }

    // -----------------------------------------------------------------------
    // The two promises of absence.
    // -----------------------------------------------------------------------

    /// **Nothing in the file is a token, a bearer, or a challenge.** Asserted
    /// about every text and blob column of every table after a real enrollment,
    /// rather than about the code that wrote them.
    #[test]
    fn no_raw_token_bearer_or_challenge_reaches_any_column() {
        let relay = relay(&[]);
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);
        let reply = enrol(
            &relay,
            enroll_body(&challenge, TOKEN).as_bytes(),
            Some("203.0.113.7"),
            REAL_CLOCK_MS,
        );
        assert_eq!(reply.status, StatusCode::OK);
        let credential = credential_of(&reply);

        let db = relay.db.lock().unwrap();
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();

        let mut inspected = 0;
        for table in &tables {
            let mut statement = db.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let columns = statement.column_count();
            let cells = statement
                .query_map([], |row| {
                    let mut values = Vec::new();
                    for index in 0..columns {
                        let text = match row.get_ref(index).unwrap() {
                            rusqlite::types::ValueRef::Text(bytes) => {
                                String::from_utf8_lossy(bytes).to_string()
                            }
                            rusqlite::types::ValueRef::Blob(bytes) => {
                                String::from_utf8_lossy(bytes).to_string()
                            }
                            _ => String::new(),
                        };
                        values.push(text);
                    }
                    Ok(values)
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            for row in cells {
                for value in row {
                    inspected += 1;
                    for secret in [
                        TOKEN,
                        credential.as_str(),
                        challenge.as_str(),
                        "203.0.113.7",
                    ] {
                        assert!(
                            !value.contains(secret),
                            "{table} holds a raw value: {value:?}"
                        );
                    }
                }
            }
        }
        assert!(inspected > 0, "nothing was inspected");

        // The hashes are there, which is what makes the absence meaningful.
        let bound: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM bindings WHERE token_hash = ?1 AND bearer_hash = ?2",
                rusqlite::params![
                    token_hash(TOKEN),
                    bearer_hash(&crate::secret::Secret::new(credential))
                ],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bound, 1);
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// **Nothing in the log is a secret either.** An enrollment and a credential
    /// refusal are exercised with the subscriber captured, and the captured text
    /// is searched for every value that must not be in it.
    #[test]
    fn no_authorization_header_token_bearer_attestation_or_address_reaches_the_log() {
        let relay = relay(&[]);
        let challenge = seed_real_challenge(&relay, REAL_CLOCK_MS);
        let attestation = real_attestation();
        let address = "203.0.113.7";
        let credential = Arc::new(Mutex::new(String::new()));

        crate::logging::enable_every_callsite();
        let sink = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(sink.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let reply = enrol(
                &relay,
                enroll_body(&challenge, TOKEN).as_bytes(),
                Some(address),
                REAL_CLOCK_MS,
            );
            assert_eq!(reply.status, StatusCode::OK);
            let issued = reply.body["credential"].as_str().unwrap().to_string();
            *credential.lock().unwrap() = issued.clone();

            // A credential that is refused: the path that most wants to print
            // the value it did not accept.
            let refused = credential_status(
                &relay,
                Some("Bearer aNeverIssuedCredentialValue"),
                Some(address),
                REAL_CLOCK_MS,
            );
            assert_eq!(refused.body["status"], "reenroll");
            let _ = credential_status(
                &relay,
                Some(&format!("Bearer {issued}")),
                Some(address),
                REAL_CLOCK_MS,
            );
        });

        let logged = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(!logged.is_empty(), "nothing was logged at all");
        let credential = credential.lock().unwrap().clone();
        for secret in [
            TOKEN,
            credential.as_str(),
            challenge.as_str(),
            address,
            "aNeverIssuedCredentialValue",
            "Bearer",
            REAL_KEY_ID,
            &attestation[..64],
        ] {
            assert!(
                !logged.contains(secret),
                "the log carries {secret:?}: {logged}"
            );
        }
        // What it does carry is eight characters of a hash, which follows one
        // caller through an incident and is nothing on its own.
        assert!(
            logged.contains(&token_hash(TOKEN)[..8]),
            "a line about a binding must be identifiable: {logged}"
        );
        assert!(!logged.contains(&token_hash(TOKEN)), "{logged}");
    }
}
