//! `POST /v1/push` — the one route that reaches Apple, and the gates in front
//! of it.
//!
//! **The bearer is in `Authorization` and nowhere else** (§4). Not in the URL,
//! where a proxy access log would keep it, and not in the JSON, where a request
//! body captured for debugging would.
//!
//! **The relay answers only after Apple answers** (§4). There is no early
//! `202`, because the daemon's `test_push` means "a notification reached Apple"
//! and a relay that answered before it knew would make that report a guess.
//!
//! **One attempt, plus exactly one more, and never onto a host this binding's
//! namespace may not address.** `BadDeviceToken` buys a single attempt against
//! the opposite APNs host, because a token posted at the wrong one fails exactly
//! that way and the alternative is deleting a working registration. What the
//! namespace decides is which bindings may buy it: a development attestation is
//! minted by anybody with Xcode and reaches the sandbox host only. Nothing else
//! here retries anything, ever: a doorbell delivered twice shows a phone an old
//! state as if it were news.
//!
//! **A refused credential is never an unregistered phone** (§4, Decision 4).
//! The two answers send the daemon in opposite directions — one recovers a
//! credential and keeps the APNs token, the other throws the token away — so
//! every credential fault here is `credential_invalid` and only Apple's `410`
//! is `unregistered`.

use std::sync::atomic::Ordering;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use push_core::{ApnsEnvironment, ApnsOutcome, COLLAPSE_ID, TEST_COLLAPSE_ID};

use crate::api::Relay;
use crate::dto::{self, Invalid, Notification, PushRequest};
use crate::enroll::{
    answer, bearer_from, binding_for_bearer, check_limits, client_address, generation_floor,
    internal, now_ms, permits, refuse, Refusal, Reply,
};
use crate::logging::RequestLog;
use crate::payload;
use crate::ratelimit;
use crate::secret::{bearer_hash, token_hash};

const ROUTE: &str = "/v1/push";

/// The route this module owns, mounted by [`crate::api::router`].
///
/// No body limit of its own: the router's [`crate::dto::MAX_BODY_BYTES`] is
/// already the bound this document is written to, and a second one here would
/// be a second number to keep in step with the parser.
pub fn routes() -> Router<Relay> {
    Router::new().route("/v1/push", post(push_route))
}

/// Every answer this route can give, as §4's closed set.
///
/// A word rather than a status code, because the status is for HTTP and the
/// word is for the daemon: `unavailable` and `rejected` are both failures a
/// person never sees, and `credential_invalid` and `unregistered` are two
/// different repairs the phone has to perform.
///
/// `rate_limited` is the one word not here. It is produced by
/// [`crate::enroll::limited`], which every route in the relay shares — it is
/// the helper that puts `Retry-After` on the refusal, and a second copy would
/// eventually be a 429 without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Accepted,
    Unregistered,
    CredentialInvalid,
    Rejected,
    Unavailable,
}

impl Outcome {
    fn word(self) -> &'static str {
        match self {
            Outcome::Accepted => "accepted",
            Outcome::Unregistered => "unregistered",
            Outcome::CredentialInvalid => "credential_invalid",
            Outcome::Rejected => "rejected",
            Outcome::Unavailable => "unavailable",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Outcome::Accepted => StatusCode::OK,
            // Apple's own word for a token that will never work again, so a
            // proxy or a client library reading only the status draws the same
            // conclusion the body states.
            Outcome::Unregistered => StatusCode::GONE,
            Outcome::CredentialInvalid => StatusCode::UNAUTHORIZED,
            // The relay is well and something upstream refused, which is
            // exactly what a gateway status says. A `400` would blame the
            // daemon for a request it composed correctly.
            Outcome::Rejected => StatusCode::BAD_GATEWAY,
            Outcome::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

async fn push_route(State(relay): State<Relay>, headers: HeaderMap, body: Bytes) -> Reply {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    push(
        &relay,
        authorization,
        client_address(&headers),
        &body,
        now_ms(),
    )
    .await
}

/// One push, from the bytes on the wire to Apple's verdict.
///
/// Taken apart from the handler so the gates can be exercised without a socket
/// and with a clock the test chooses.
pub(crate) async fn push(
    relay: &Relay,
    authorization: Option<&str>,
    ip: Option<&str>,
    body: &[u8],
    now: i64,
) -> Reply {
    let request = match dto::parse(body) {
        Ok(request) => request,
        Err(invalid) => {
            relay.metrics.pushes_refused.fetch_add(1, Ordering::Relaxed);
            return refuse(ROUTE, schema_refusal(&invalid), None);
        }
    };

    let binding = match authorize(relay, authorization, ip, &request, now) {
        Ok(binding) => binding,
        Err(reply) => return reply,
    };
    let id = Some(binding.token_hash.as_str());

    // **The send kill switch, checked before anything is composed.** It exists
    // for the incident where the relay must stop reaching Apple at all, so the
    // one property that matters is that no request is emitted — not that the
    // response says so. Placed after authentication so an operator turning it
    // off does not also turn the relay into an open endpoint that tells
    // strangers which credentials are real.
    if !relay.config.send_enabled {
        return refused(relay, Outcome::Unavailable, id);
    }
    // An environment whose key has not been issued answers rather than
    // borrowing the other environment's key, which would sign for the wrong
    // topic and come back as a token error.
    if relay.apns.absence(binding.environment).is_some() {
        return refused(relay, Outcome::Unavailable, id);
    }

    let (payload, collapse) = compose(&request.notification);

    // **§4, verbatim:** "The relay binding is the single authority for a
    // token's APNs environment, and the environment in a push request is
    // advisory. The relay addresses APNs by the binding's environment
    // regardless of the advisory value and returns the authoritative
    // environment in every accepted response — a delivery is never refused for
    // a stale advisory environment."
    //
    // So `request.advisory_environment` is not read here and does not appear in
    // any comparison. Two Macs share one phone's credential; the second may
    // still hold the environment from before a correction, and refusing it
    // would be refusing the delivery that the correction was supposed to make
    // work.

    // **The whole request's time, opened once and spent by every attempt.** The
    // correction below is a second send, and giving it a clock of its own would
    // let one push run to twice the transport's bound — outside the 45 seconds
    // the daemon waits, which is the daemon abandoning work the relay is still
    // doing.
    let budget = relay.apns.budget();
    let sent = relay
        .apns
        .send(
            &budget,
            binding.environment,
            &request.token,
            &payload,
            collapse,
        )
        .await;

    match verdict(relay, &binding, binding.environment, sent, now) {
        Verdict::Answered(reply) => reply,
        Verdict::BadDeviceToken => {
            correction(relay, &binding, &request, &payload, collapse, &budget, now).await
        }
    }
}

/// What one attempt settled, or that it settled nothing.
enum Verdict {
    Answered(Reply),
    /// Apple does not know this token **on this host**, which on the binding's
    /// own host is indistinguishable from the token belonging to the other one.
    BadDeviceToken,
}

/// Apple's answer to one attempt, in §4's words.
///
/// **Both attempts are read by these rules, and that is the point of the
/// function.** A `410` from the opposite host is the same fact about the phone
/// as a `410` from the binding's host and has to retire the binding either way;
/// Apple being busy is Apple's own state wherever it is met, and reporting it as
/// `rejected` would tell the daemon a working token will never work again.
/// `BadDeviceToken` is the single answer that is not a conclusion, so it is
/// handed back for the caller to decide what a second host would prove.
fn verdict(
    relay: &Relay,
    binding: &Authorized,
    environment: ApnsEnvironment,
    sent: Result<ApnsOutcome, crate::apns::SendError>,
    now: i64,
) -> Verdict {
    let id = Some(binding.token_hash.as_str());
    match sent {
        Ok(ApnsOutcome::Accepted { apns_id }) => {
            // An acceptance anywhere but the binding's own host is the
            // correction Decision 4 exists to find.
            if environment != binding.environment {
                correct_environment(relay, binding, environment, now);
            }
            Verdict::Answered(accepted(relay, binding, apns_id, environment))
        }
        // §7 and Decision 4: `410` is the single answer that retires a token,
        // and it retires the binding rather than merely this request.
        Ok(ApnsOutcome::Unregistered) => {
            retire(relay, binding, now);
            Verdict::Answered(refused(relay, Outcome::Unregistered, id))
        }
        Ok(ApnsOutcome::BadDeviceToken) => Verdict::BadDeviceToken,
        // Apple is busy or unwell, which says nothing about the phone. The
        // daemon drops an ordinary doorbell and reports a test honestly.
        Ok(ApnsOutcome::Retryable { status, reason }) => {
            tracing::warn!(status, reason, "apns is refusing traffic");
            Verdict::Answered(refused(relay, Outcome::Unavailable, id))
        }
        // Logged with Apple's own words, which name neither a caller nor a
        // token: a `403 InvalidProviderToken` is the relay's key being wrong
        // for every user at once, and it has to be visible as that.
        Ok(ApnsOutcome::Rejected { status, reason }) => {
            tracing::warn!(status, reason, "apns rejected the notification");
            Verdict::Answered(refused(relay, Outcome::Rejected, id))
        }
        // Both halves of a failed attempt answer `unavailable`: nothing about
        // this phone was learned, and neither the daemon nor the relay retries.
        Err(e) => {
            tracing::warn!(error = %e, "the notification did not reach apns");
            Verdict::Answered(refused(relay, Outcome::Unavailable, id))
        }
    }
}

/// A credential that may push, and the environment it may push to.
struct Authorized {
    /// The digest every log line and rate bucket for this request is keyed on.
    token_hash: String,
    /// The row this bearer authenticated, named by the value the database has a
    /// unique index on — so an update aimed at it cannot land on another.
    bearer_hash: String,
    /// **The single authority for this token's APNs environment** (§4), read
    /// from the binding and never from the request.
    environment: ApnsEnvironment,
}

/// The bearer, the binding it names, and every reason to refuse both.
///
/// The database lock is held for the whole of it and released on the way out:
/// everything here is a read or a counter, and the push that follows is an
/// await that must not be holding a mutex over one connection.
fn authorize(
    relay: &Relay,
    authorization: Option<&str>,
    ip: Option<&str>,
    request: &PushRequest,
    now: i64,
) -> Result<Authorized, Reply> {
    let bearer = bearer_from(authorization);
    let db = relay.db.lock().unwrap_or_else(|e| e.into_inner());

    let found = match &bearer {
        Some(bearer) => match binding_for_bearer(&db, &bearer_hash(bearer)) {
            Ok(found) => found,
            Err(e) => return Err(internal(relay, ROUTE, None, e)),
        },
        None => None,
    };

    // **A bearer nobody minted meets the strict rotating-address rule, and the
    // rule refuses.** §3 asks for it, and without an enforced one this is the
    // only route where an unauthenticated caller meets no limit at all — every
    // attempt taking the one database lock the whole service shares and writing
    // a counter row behind it.
    //
    // **The refusal is still `credential_invalid` and never a 429.** A guesser
    // told to come back in sixty seconds has been told the guess was worth
    // repeating, and `Retry-After` on a wrong credential is an instruction to
    // keep trying. So what the exhausted bucket buys is not a different answer
    // but a cheaper one: the counter is peeked rather than spent, and once it is
    // empty the relay stops writing a row for every guess.
    //
    // **After the lookup and not before it.** A bucket consulted first would
    // have to refuse this address's *valid* credentials too, and the fleet
    // recovering from a restore — every phone holding a bearer the restored
    // database has never heard of, then re-enrolling and pushing again from the
    // same address — is exactly the traffic that would meet it.
    let Some(row) = found else {
        relay.metrics.invalid_auth.fetch_add(1, Ordering::Relaxed);
        if charge_address(relay, &db, ip, now) {
            relay
                .metrics
                .invalid_auth_limited
                .fetch_add(1, Ordering::Relaxed);
        }
        return Err(refused(relay, Outcome::CredentialInvalid, None));
    };

    let id = row.token_hash.clone();
    let bucket = ratelimit::binding_key(&row.token_hash);

    // A floor that cannot be read is a five hundred and never a floor of zero:
    // answering zero to a permissions problem would re-admit every credential
    // an incident response had just refused.
    let floor = match generation_floor(relay) {
        Ok(floor) => floor,
        Err(e) => return Err(internal(relay, ROUTE, Some(&id), format!("{e:#}"))),
    };
    // Read here and answered for further down: a word this relay does not know
    // is a fault in its own state rather than a credential fault, and it keeps
    // the five hundred it has always had — after the limits, so a corrupt row
    // cannot be replayed for an unbounded number of them.
    let environment = binding_environment(&row.environment);
    // **The namespace is part of being honoured, not a separate answer.** Apple
    // issues a different App Attest namespace to a development build than to a
    // distributed one, and a relay serves exactly one of them. A binding proved
    // in the other namespace is authority this relay is not the relay for — so
    // an operator who moves a deployment from one to the other must not leave
    // every credential minted before the move still able to push, when the same
    // credential can no longer rotate, rebind, delete, or ask for its status.
    //
    // **And so is the host that namespace may reach** ([`permits`]). The first
    // attempt is addressed by the row's own environment, so a row that names a
    // host its attestation cannot authorise would reach that host before any
    // correction had a say. Enrollment cannot write one; a database restored
    // from a build that corrected a binding across the namespaces carries them.
    //
    // A word this relay cannot read is left honoured here on purpose, so that it
    // meets its own answer below rather than being reported as a dead
    // credential — it is the relay's state that is wrong, not the caller's.
    let honoured = row.status == "active"
        && row.terminal_reason.is_none()
        && row.generation >= floor
        && row.attest_environment == relay.config.attest_environment.as_str()
        && environment
            .is_none_or(|environment| permits(relay.config.attest_environment, environment));

    // **The credential must be bound to the token in the request.** A bearer is
    // authority over one phone. Without this line it is authority over whichever
    // phone the caller names, and every stolen APNs token in the world becomes
    // addressable by whoever holds any credential at all.
    let bound = token_hash(&request.token) == row.token_hash;

    if !honoured {
        // **Charged to the address, and never to the phone.** This bearer is
        // dead — revoked, retired, below the floor an incident raised, or proved
        // in a namespace this relay does not serve — and the replacement
        // credential the same phone is issued spends from the
        // token's bucket, which is the one this row names. A lost Mac replaying
        // the old bearer would otherwise drain the day's five hundred and leave
        // the rotation that was supposed to contain it answering `429`, which is
        // §7's remedy defeating itself. So a dead bearer meets the strict
        // rotating-address rule instead, peeked before it is spent so an
        // exhausted address costs no write.
        //
        // Still `credential_invalid` and still no `Retry-After`: a caller told
        // when to come back has been told the credential was worth repeating,
        // when what it needs is to attest again.
        charge_address(relay, &db, ip, now);
        return Err(refused(relay, Outcome::CredentialInvalid, Some(&id)));
    }
    if !bound {
        // A live credential naming another phone is charged to its own budget,
        // which is the row it authenticated and nobody else's.
        let _ = relay.limiter.check(&db, &bucket, ratelimit::BINDING, now);
        return Err(refused(relay, Outcome::CredentialInvalid, Some(&id)));
    }

    if let Some(reply) = check_limits(
        relay,
        &db,
        ROUTE,
        Some(&id),
        &[(bucket.as_str(), ratelimit::BINDING)],
        now,
    ) {
        return Err(reply);
    }

    let Some(environment) = environment else {
        return Err(internal(
            relay,
            ROUTE,
            Some(&id),
            "a binding carries an apns environment this relay does not know",
        ));
    };
    Ok(Authorized {
        token_hash: row.token_hash,
        bearer_hash: bearer
            .map(|bearer| bearer_hash(&bearer))
            .unwrap_or_default(),
        environment,
    })
}

/// Charge one refused credential to the address it came from.
///
/// **The bucket a caller cannot pick.** Every credential this route will not
/// honour lands here — a bearer nobody minted and a bearer that has been
/// revoked alike — because both are the same behaviour from the relay's side and
/// neither may spend a phone's budget. Answers whether the address had already
/// spent its allowance, in which case nothing is written: a sustained flood then
/// costs no database row at all, which is what makes the rule an enforcement
/// rather than a count.
fn charge_address(relay: &Relay, db: &rusqlite::Connection, ip: Option<&str>, now: i64) -> bool {
    let address = relay.limiter.ip_key("auth", ip, now);
    if relay
        .limiter
        .spent(db, &address, ratelimit::INVALID_AUTH_IP, now)
    {
        return true;
    }
    let _ = relay
        .limiter
        .check(db, &address, ratelimit::INVALID_AUTH_IP, now);
    false
}

/// The one retry Decision 4 keeps: the opposite APNs host, tried exactly once,
/// and only by a binding whose namespace may address it.
///
/// A token that is not valid at the host it was posted to fails as
/// `400 BadDeviceToken`, which reads like a corrupt token rather than like the
/// wrong host. Retiring the binding on that evidence would delete a working
/// registration, so the other host is tried before anything is concluded — but
/// only where [`permits`] allows this attestation to reach it.
///
/// **Two attempts share one budget.** This one takes what the first left of the
/// request's [`crate::apns::Budget`] rather than starting a clock of its own, so
/// a slow first attempt and this one together stay inside the envelope the
/// daemon waits on; a budget the first attempt spent refuses here before
/// anything is submitted, and that is `unavailable`.
async fn correction(
    relay: &Relay,
    binding: &Authorized,
    request: &PushRequest,
    payload: &str,
    collapse: &'static str,
    budget: &crate::apns::Budget,
    now: i64,
) -> Reply {
    let id = Some(binding.token_hash.as_str());
    let opposite = match binding.environment {
        ApnsEnvironment::Sandbox => ApnsEnvironment::Production,
        ApnsEnvironment::Production => ApnsEnvironment::Sandbox,
    };
    // **A correction the namespace forbids is not attempted at all**, so a
    // development attestation never addresses a customer's phone — not even
    // once, and not even to be told no. It is the same `rejected` as the two
    // refusals below on purpose: three different reasons to have learned
    // nothing more, answered with one word, so the answer is not an oracle
    // telling a caller which of them it met.
    if !permits(relay.config.attest_environment, opposite) {
        return refused(relay, Outcome::Rejected, id);
    }
    // Without that environment's key there is nothing to learn, and Apple's
    // refusal is the only evidence in hand.
    if relay.apns.absence(opposite).is_some() {
        return refused(relay, Outcome::Rejected, id);
    }
    let sent = relay
        .apns
        .send(budget, opposite, &request.token, payload, collapse)
        .await;
    match verdict(relay, binding, opposite, sent, now) {
        Verdict::Answered(reply) => reply,
        // Refused at both hosts is a token that is simply not good, and one
        // extra attempt is all Decision 4 allows.
        Verdict::BadDeviceToken => refused(relay, Outcome::Rejected, id),
    }
}

/// Write the environment Apple just proved, without overwriting a replacement.
///
/// **Compare-and-set against the row that was read**, on the bearer that
/// authenticated it *and* the token and environment it carried. An
/// unconditional update would race an enrollment that revoked this binding and
/// bound the token somewhere else in the meantime — and the correction would
/// then be written onto a credential nobody holds, or onto a phone that has
/// already moved. Zero rows changed is that race, resolved in favour of
/// whoever committed.
///
/// **And the pairing is checked here as well as at the attempt.** [`correction`]
/// refuses to ask Apple about a host the namespace forbids, which is the rule
/// that matters to a phone; this is the rule that matters to the database, and
/// it is the last line before the `UPDATE`. Reaching it with a forbidden
/// environment is a fault in this file rather than anything a caller did, so it
/// is counted and logged as one and nothing is written.
fn correct_environment(relay: &Relay, binding: &Authorized, corrected: ApnsEnvironment, now: i64) {
    if !permits(relay.config.attest_environment, corrected) {
        relay
            .metrics
            .internal_errors
            .fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            environment = corrected.as_str(),
            "an apns environment this attest namespace cannot bind was not stored"
        );
        return;
    }
    let db = relay.db.lock().unwrap_or_else(|e| e.into_inner());
    let updated = db.execute(
        "UPDATE bindings SET environment = ?4, updated_ms = ?5
         WHERE bearer_hash = ?1 AND token_hash = ?2 AND environment = ?3 AND status = 'active'",
        rusqlite::params![
            binding.bearer_hash,
            binding.token_hash,
            binding.environment.as_str(),
            corrected.as_str(),
            now
        ],
    );
    match updated {
        Ok(1) => {
            relay
                .metrics
                .environments_corrected
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(_) => {}
        Err(e) => {
            relay
                .metrics
                .internal_errors
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(error = %e, "a corrected apns environment could not be stored");
        }
    }
}

/// §7: mark the binding terminal, so the token is never reissued a credential.
///
/// Aimed at the bearer that authenticated this request, so a binding that was
/// already replaced while the push was in flight is left alone — a late `410`
/// about the old token must not retire the phone's new one.
fn retire(relay: &Relay, binding: &Authorized, now: i64) {
    let db = relay.db.lock().unwrap_or_else(|e| e.into_inner());
    let retired = db.execute(
        "UPDATE bindings SET status = 'revoked', terminal_reason = 'unregistered', updated_ms = ?2
         WHERE bearer_hash = ?1 AND status = 'active'",
        rusqlite::params![binding.bearer_hash, now],
    );
    match retired {
        Ok(_) => {
            relay
                .metrics
                .tokens_unregistered
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            relay
                .metrics
                .internal_errors
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(error = %e, "a token apple reported as gone could not be retired");
        }
    }
}

fn accepted(
    relay: &Relay,
    binding: &Authorized,
    apns_id: Option<String>,
    environment: ApnsEnvironment,
) -> Reply {
    relay
        .metrics
        .pushes_accepted
        .fetch_add(1, Ordering::Relaxed);
    let mut body = serde_json::json!({
        "outcome": Outcome::Accepted.word(),
        // **The authoritative environment, on every accepted answer** (§4).
        // A second Mac that has not learned a correction delivers on the
        // binding's environment and reads the truth out of this field, which is
        // what it CAS-persists; without it a correction would only ever reach
        // the Mac that happened to provoke it.
        "environment": environment.as_str(),
    });
    // Apple's receipt for this exact notification, present only when Apple sent
    // one — an `apns_id` of `null` would be a receipt a reader could quote.
    if let Some(apns_id) = apns_id {
        body["apns_id"] = serde_json::Value::String(apns_id);
    }
    answer(
        ROUTE,
        Outcome::Accepted.word(),
        Some(&binding.token_hash),
        body,
    )
}

/// The refusal, its log line and its counter, in one place — so an answer
/// cannot be returned without being logged as the same word.
fn refused(relay: &Relay, outcome: Outcome, binding: Option<&str>) -> Reply {
    relay.metrics.pushes_refused.fetch_add(1, Ordering::Relaxed);
    if outcome == Outcome::CredentialInvalid {
        relay
            .metrics
            .credential_refusals
            .fetch_add(1, Ordering::Relaxed);
    }
    let status = outcome.status();
    RequestLog {
        route: ROUTE,
        status: status.as_u16(),
        outcome: outcome.word(),
        binding_hash: binding,
    }
    .emit();
    Reply {
        status,
        body: serde_json::json!({ "error": outcome.word() }),
        retry_after_seconds: None,
    }
}

/// A schema violation is a `400`, named by the rule it broke.
fn schema_refusal(invalid: &Invalid) -> Refusal {
    match invalid {
        Invalid::Schema(_) => Refusal::Schema,
        Invalid::Token(_) => Refusal::Token,
        Invalid::TooLarge(_) | Invalid::Malformed(_) | Invalid::BlockedCount(_) => {
            Refusal::Malformed
        }
    }
}

/// The document and the slot it occupies on the phone.
fn compose(notification: &Notification) -> (String, &'static str) {
    match *notification {
        Notification::Doorbell {
            kind,
            blocked_count,
        } => (payload::doorbell(kind, blocked_count), COLLAPSE_ID),
        // A test the user is watching for must not replace a waiting decision,
        // so it has its own slot.
        Notification::Test {} => (payload::test(), TEST_COLLAPSE_ID),
    }
}

/// The environment a binding is on, refused rather than guessed.
///
/// **Deliberately not `ApnsEnvironment::parse`.** That reads anything it does
/// not recognise as sandbox, which is right for a hint and wrong for the
/// authority: a row whose word was corrupted would quietly post a production
/// phone's notification to the sandbox host and answer every push with a token
/// error. Enrollment writes one of two words, so a third is a fault in this
/// process's own state.
///
/// Enrollment refuses an unknown environment for the same reason — there the
/// value *establishes* the binding — while §4's advisory rule governs only the
/// environment a push *request* carries. The two look contradictory read apart:
/// one is the truth being written down, the other is a caller's stale belief
/// about it, and a stale belief is never a reason to refuse a delivery.
fn binding_environment(stored: &str) -> Option<ApnsEnvironment> {
    match stored {
        "sandbox" => Some(ApnsEnvironment::Sandbox),
        "production" => Some(ApnsEnvironment::Production),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering as AtomicOrdering;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use rusqlite::Connection;
    use tower::ServiceExt;

    use super::*;
    use crate::api::router;
    use crate::apns::fake_apple::{apple, signing_key, Plan, TOPIC};
    use crate::apns::{ApnsEndpoint, ApnsSigner, ApnsTransport};
    use crate::config::RelayConfig;
    use crate::payload::PushKind;
    use crate::secret::{bearer_hash, new_bearer, Secret};

    const TOKEN: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    const OTHER_TOKEN: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const NOW: i64 = 1_700_000_000_000;

    /// A directory under the OS temp dir. The process id is in the name because
    /// two `cargo test` invocations can overlap on one machine, and a fixed path
    /// means one of them deleting the other's fixture halfway through.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("push-relay-push-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A transport whose two environments post wherever the test says, or have
    /// no key at all.
    fn transport(sandbox: Option<&str>, production: Option<&str>) -> ApnsTransport {
        let signer = |base: Option<&str>, name: &str| match base {
            Some(base) => ApnsSigner::ready(signing_key(), ApnsEndpoint::parse(base).unwrap()),
            None => ApnsSigner::Absent(format!("no {name} key in tests")),
        };
        ApnsTransport::new(signer(sandbox, "sandbox"), signer(production, "production")).unwrap()
    }

    /// A relay with one attested installation and one active binding, plus the
    /// bearer that was minted for it.
    struct Fixture {
        relay: Relay,
        bearer: Secret,
        floor_file: PathBuf,
        dir: PathBuf,
    }

    impl Fixture {
        fn authorization(&self) -> String {
            format!("Bearer {}", self.bearer.expose())
        }

        async fn push(&self, body: &str) -> Reply {
            self.push_as(&self.authorization(), body).await
        }

        async fn push_as(&self, authorization: &str, body: &str) -> Reply {
            push(
                &self.relay,
                Some(authorization),
                Some("203.0.113.7"),
                body.as_bytes(),
                NOW,
            )
            .await
        }

        fn binding(&self) -> (String, String) {
            let db = self.relay.db.lock().unwrap();
            db.query_row(
                "SELECT environment, status FROM bindings WHERE bearer_hash = ?1",
                [bearer_hash(&self.bearer)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        }

        /// Why the binding is terminal, which is the half of §7's rule that a
        /// status of `revoked` alone does not say.
        fn terminal_reason(&self) -> Option<String> {
            let db = self.relay.db.lock().unwrap();
            db.query_row(
                "SELECT terminal_reason FROM bindings WHERE bearer_hash = ?1",
                [bearer_hash(&self.bearer)],
                |row| row.get(0),
            )
            .unwrap()
        }

        /// What one bucket has spent today, or nothing at all if it has never
        /// been written — which is the state that matters when the question is
        /// whether a refusal touched a budget it had no business touching.
        fn day_count(&self, bucket: &str) -> Option<i64> {
            let db = self.relay.db.lock().unwrap();
            db.query_row(
                "SELECT day_count FROM rate_buckets WHERE bucket_key = ?1",
                [bucket],
                |row| row.get(0),
            )
            .ok()
        }

        fn drop_scratch(&self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The ordinary relay: the App Attest namespace [`RelayConfig`] defaults to,
    /// so a test that changes it is testing the change rather than a fixture
    /// that disagreed with the relay from the start.
    fn fixture(
        name: &str,
        environment: &str,
        apns: ApnsTransport,
        extra: &[(&str, &str)],
    ) -> Fixture {
        attested(name, "development", environment, apns, extra)
    }

    /// A relay whose binding may take Decision 4's second attempt: attested in
    /// the production namespace, which is the one Apple only issues to a build
    /// it distributed, and stored on the production host enrollment paired it
    /// with. The opposite host is the sandbox, and this namespace may reach it.
    fn correcting(name: &str, apns: ApnsTransport) -> Fixture {
        attested(
            name,
            "production",
            "production",
            apns,
            &[("RELAY_ATTEST_ENVIRONMENT", "production")],
        )
    }

    fn attested(
        name: &str,
        attest: &str,
        environment: &str,
        apns: ApnsTransport,
        extra: &[(&str, &str)],
    ) -> Fixture {
        let dir = scratch(name);
        let floor_file = dir.join("generation-floor");
        let mut pairs: Vec<(String, String)> = vec![(
            "RELAY_GENERATION_FLOOR_FILE".into(),
            floor_file.to_string_lossy().into_owned(),
        )];
        pairs.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        );
        let map: HashMap<String, String> = pairs.into_iter().collect();
        let config = RelayConfig::read(move |key| map.get(key).cloned()).unwrap();

        let db = crate::db::open_in_memory().unwrap();
        let bearer = seed(&db, attest, TOKEN, environment, 0);
        Fixture {
            relay: Relay::with_transport(config, db, apns, None),
            bearer,
            floor_file,
            dir,
        }
    }

    /// One installation and one active binding, straight into the tables — the
    /// state a completed attestation leaves behind.
    fn seed(
        db: &Connection,
        attest: &str,
        token: &str,
        environment: &str,
        generation: i64,
    ) -> Secret {
        let bearer = new_bearer().unwrap();
        db.execute(
            "INSERT INTO installations
                (key_id_hash, public_key, receipt, attest_environment, counter, counter_trusted,
                 bundle_version, validation_category, created_ms, updated_ms)
             VALUES (?1, X'0102', NULL, ?3, 0, 1, '1.0', 1, ?2, ?2)
             ON CONFLICT (key_id_hash) DO NOTHING",
            rusqlite::params![format!("key-id-hash-for-{token}"), NOW, attest],
        )
        .unwrap();
        let installation: i64 = db
            .query_row(
                "SELECT id FROM installations WHERE key_id_hash = ?1",
                [format!("key-id-hash-for-{token}")],
                |row| row.get(0),
            )
            .unwrap();
        db.execute(
            "INSERT INTO bindings
                (installation_id, token_hash, environment, bearer_hash, generation,
                 status, terminal_reason, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', NULL, ?6, ?6)",
            rusqlite::params![
                installation,
                token_hash(token),
                environment,
                bearer_hash(&bearer),
                generation,
                NOW
            ],
        )
        .unwrap();
        bearer
    }

    fn doorbell(token: &str, environment: &str) -> String {
        format!(
            r#"{{"schema":1,"token":"{token}","environment":"{environment}","notification":{{"type":"doorbell","kind":"approval","blocked_count":1}}}}"#
        )
    }

    fn test_request(token: &str, environment: &str) -> String {
        format!(
            r#"{{"schema":1,"token":"{token}","environment":"{environment}","notification":{{"type":"test"}}}}"#
        )
    }

    fn bad_token_plan() -> Plan {
        Plan::answering(400, r#"{"reason":"BadDeviceToken"}"#)
    }

    // -----------------------------------------------------------------------
    // The accepted path and its exact shape.
    // -----------------------------------------------------------------------

    /// **The documented request and the documented answer**, both of them
    /// asserted whole so an added field is a failing build rather than a
    /// surprise on the daemon's side.
    #[tokio::test]
    async fn a_doorbell_is_accepted_and_the_answer_carries_the_receipt_and_the_environment() {
        let mut plan = Plan::answering(200, "");
        plan.apns_id = Some("8B2A6F1C-0000-4E2B-9F4C-2C1D3E4F5A6B".into());
        let (base, ledger) = apple(plan).await;
        let fixture = fixture("accepted", "sandbox", transport(Some(&base), None), &[]);

        let reply = fixture.push(&doorbell(TOKEN, "sandbox")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.body,
            serde_json::json!({
                "outcome": "accepted",
                "environment": "sandbox",
                "apns_id": "8B2A6F1C-0000-4E2B-9F4C-2C1D3E4F5A6B",
            })
        );
        assert_eq!(reply.retry_after_seconds, None);
        assert_eq!(ledger.streams(), 1);
        fixture.drop_scratch();
    }

    /// Apple does not always send a receipt, and a `null` one would be a
    /// receipt a reader could quote back.
    #[tokio::test]
    async fn an_accepted_answer_without_a_receipt_omits_the_field_rather_than_nulling_it() {
        let (base, _ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("no-receipt", "sandbox", transport(Some(&base), None), &[]);

        let reply = fixture.push(&test_request(TOKEN, "sandbox")).await;
        assert_eq!(
            reply.body,
            serde_json::json!({"outcome": "accepted", "environment": "sandbox"})
        );
        fixture.drop_scratch();
    }

    /// **What Apple actually receives**: the generic document this relay
    /// composed, at the device's own path, in the slot its kind belongs to —
    /// and with nothing from the request but the token.
    #[tokio::test]
    async fn the_composed_notification_is_generic_and_lands_in_the_right_slot() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("composed", "sandbox", transport(Some(&base), None), &[]);

        fixture.push(&doorbell(TOKEN, "sandbox")).await;
        fixture.push(&test_request(TOKEN, "sandbox")).await;

        let requests = ledger.requests.lock().unwrap();
        assert_eq!(requests[0].path, format!("/3/device/{TOKEN}"));
        assert_eq!(requests[0].header("apns-collapse-id"), COLLAPSE_ID);
        assert_eq!(requests[0].header("apns-topic"), TOPIC);
        assert_eq!(requests[0].body, payload::doorbell(PushKind::Approval, 1));
        assert_eq!(requests[1].header("apns-collapse-id"), TEST_COLLAPSE_ID);
        assert_eq!(requests[1].body, payload::test());
        assert_ne!(
            requests[0].header("apns-collapse-id"),
            requests[1].header("apns-collapse-id"),
            "a test the user is watching for must not replace a waiting decision"
        );
        drop(requests);
        fixture.drop_scratch();
    }

    // -----------------------------------------------------------------------
    // The advisory environment rule (§4).
    // -----------------------------------------------------------------------

    /// **The rule, in one test.** The binding says sandbox and the caller says
    /// production. The push is delivered on the binding's host, is not refused,
    /// and the answer carries the authoritative word so the caller can persist
    /// the truth it did not have.
    #[tokio::test]
    async fn a_stale_advisory_environment_delivers_and_is_answered_with_the_authority() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("advisory", "sandbox", transport(Some(&base), None), &[]);

        for advisory in ["production", "sandbox"] {
            let reply = fixture.push(&doorbell(TOKEN, advisory)).await;
            assert_eq!(reply.status, StatusCode::OK, "advisory {advisory}");
            assert_eq!(
                reply.body["environment"], "sandbox",
                "the answer must carry the binding's environment, not the request's"
            );
        }
        // Both went to the sandbox host, which is the only one with a key here;
        // a relay that had addressed Apple by the advisory value would have
        // answered `unavailable` for the first.
        assert_eq!(ledger.streams(), 2);
        assert_eq!(fixture.binding().0, "sandbox");
        fixture.drop_scratch();
    }

    /// And the reverse: a binding on production is not delivered to sandbox
    /// because a Mac said so.
    #[tokio::test]
    async fn the_advisory_value_cannot_move_a_push_to_the_other_host() {
        let (sandbox, sandbox_ledger) = apple(Plan::answering(200, "")).await;
        let (production, production_ledger) = apple(Plan::answering(200, "")).await;
        let fixture = correcting(
            "advisory-reverse",
            transport(Some(&sandbox), Some(&production)),
        );

        let reply = fixture.push(&doorbell(TOKEN, "sandbox")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.body["environment"], "production");
        assert_eq!(production_ledger.streams(), 1);
        assert_eq!(
            sandbox_ledger.streams(),
            0,
            "the advisory value chose a host"
        );
        fixture.drop_scratch();
    }

    // -----------------------------------------------------------------------
    // The credential gates.
    // -----------------------------------------------------------------------

    /// **The named gate.** A bearer is authority over one phone; without this
    /// line it is authority over whichever phone the caller names, and every
    /// stolen APNs token becomes addressable by anyone holding any credential.
    #[tokio::test]
    async fn a_credential_cannot_push_to_a_token_it_is_not_bound_to() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("wrong-token", "sandbox", transport(Some(&base), None), &[]);

        let reply = fixture.push(&doorbell(OTHER_TOKEN, "sandbox")).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.body["error"], "credential_invalid");
        assert_eq!(ledger.connections(), 0, "nothing may reach Apple");

        // The same credential and its own token still works, so the refusal is
        // the binding and not the credential.
        assert_eq!(
            fixture.push(&doorbell(TOKEN, "sandbox")).await.status,
            StatusCode::OK
        );
        fixture.drop_scratch();
    }

    /// **Every credential fault is `credential_invalid` and none of them is
    /// `unregistered`** (§4, Decision 4). The two send the daemon in opposite
    /// directions: one recovers a credential and keeps the APNs token, the
    /// other throws a working token away.
    #[tokio::test]
    async fn absent_revoked_terminal_and_below_floor_credentials_are_all_credential_invalid() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("credentials", "sandbox", transport(Some(&base), None), &[]);
        let body = doorbell(TOKEN, "sandbox");

        // No header at all, a header that is not a bearer, and a bearer nobody
        // ever minted.
        for authorization in [
            None,
            Some("Basic abcdef"),
            Some("Bearer aNeverIssuedCredential"),
        ] {
            let reply = push(&fixture.relay, authorization, None, body.as_bytes(), NOW).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{authorization:?}");
            assert_eq!(reply.body["error"], "credential_invalid");
        }

        // Revoked.
        let revoke = |reason: &str| {
            let db = fixture.relay.db.lock().unwrap();
            db.execute(
                "UPDATE bindings SET status = 'revoked', terminal_reason = ?2 WHERE bearer_hash = ?1",
                rusqlite::params![bearer_hash(&fixture.bearer), reason],
            )
            .unwrap();
        };
        revoke("rotated");
        assert_eq!(
            fixture.push(&body).await.body["error"],
            "credential_invalid"
        );

        // Terminal, which is the state a `410` leaves behind.
        revoke("unregistered");
        assert_eq!(
            fixture.push(&body).await.body["error"],
            "credential_invalid"
        );

        // And below the floor, with the row itself perfectly healthy.
        {
            let db = fixture.relay.db.lock().unwrap();
            db.execute(
                "UPDATE bindings SET status = 'active', terminal_reason = NULL WHERE bearer_hash = ?1",
                [bearer_hash(&fixture.bearer)],
            )
            .unwrap();
        }
        std::fs::write(&fixture.floor_file, "1\n").unwrap();
        let reply = fixture.push(&body).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.body["error"], "credential_invalid");

        assert_eq!(ledger.connections(), 0, "no refusal may reach Apple");
        fixture.drop_scratch();
    }

    /// **The strict rotating-address rule, enforced.** Without it this is the
    /// only route where an unauthenticated caller meets no limit at all, and
    /// every guess takes the one database lock the service shares and writes a
    /// counter row behind it.
    ///
    /// The answer never changes and never becomes a `429`: a guesser told when
    /// to come back has been told the guess was worth repeating. What the spent
    /// bucket buys is a cheaper refusal — the counter stops being written.
    #[tokio::test]
    async fn a_flood_of_unknown_bearers_from_one_address_stops_being_counted_and_never_gets_a_429()
    {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("invalid-auth", "sandbox", transport(Some(&base), None), &[]);
        let body = doorbell(TOKEN, "sandbox");
        let address = "203.0.113.9";
        let attempts = 60;

        for attempt in 0..attempts {
            let reply = push(
                &fixture.relay,
                Some("Bearer aNeverIssuedCredentialValue"),
                Some(address),
                body.as_bytes(),
                NOW,
            )
            .await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
            assert_eq!(reply.body["error"], "credential_invalid");
            assert_eq!(
                reply.retry_after_seconds, None,
                "a guesser must not be told the guess was worth repeating"
            );
        }
        assert_eq!(ledger.connections(), 0, "nothing may reach Apple");

        // **The enforcement, in the table.** The bucket took the burst and then
        // stopped being written to, so the flood after it cost no write at all
        // — which is the whole reason a rule that only counted was not one.
        let key = fixture.relay.limiter.ip_key("auth", Some(address), NOW);
        let counted: i64 = {
            let db = fixture.relay.db.lock().unwrap();
            db.query_row(
                "SELECT day_count FROM rate_buckets WHERE bucket_key = ?1",
                [&key],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(counted, i64::from(crate::ratelimit::INVALID_AUTH_IP.burst));

        // And §7's runbook has the two numbers to inspect: how much guessing
        // arrived, and how much of it the relay declined to do work for.
        let metrics = &fixture.relay.metrics;
        assert_eq!(metrics.invalid_auth.load(AtomicOrdering::Relaxed), attempts);
        assert_eq!(
            metrics.invalid_auth_limited.load(AtomicOrdering::Relaxed),
            attempts - u64::from(crate::ratelimit::INVALID_AUTH_IP.burst)
        );

        // **A real credential is untouched — including from the same address.**
        // The rule is consulted after the binding is looked up and not before,
        // because a fleet re-enrolling after a restore presents unknown bearers
        // and then valid ones from the addresses it was already using.
        for address in ["198.51.100.4", "203.0.113.9"] {
            let reply = push(
                &fixture.relay,
                Some(&fixture.authorization()),
                Some(address),
                body.as_bytes(),
                NOW,
            )
            .await;
            assert_eq!(reply.status, StatusCode::OK, "from {address}");
        }
        fixture.drop_scratch();
    }

    /// **A database that has gone wrong is the incident §7's restore runbook
    /// exists for, so it cannot be a silent five hundred.** The message is
    /// SQLite's own and names a table; a token, a bearer and a challenge only
    /// ever reach it as bound parameters, and a bound parameter is never in one.
    #[tokio::test]
    async fn a_database_fault_is_logged_with_the_table_it_names_and_counted() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("db-fault", "sandbox", transport(Some(&base), None), &[]);
        let body = doorbell(TOKEN, "sandbox");
        let bearer = fixture.authorization();
        {
            let db = fixture.relay.db.lock().unwrap();
            db.execute_batch("DROP TABLE bindings").unwrap();
        }

        crate::logging::enable_every_callsite();
        let sink = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(sink.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let reply = fixture.push(&body).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body["error"], "internal");
        drop(_guard);

        assert_eq!(
            fixture
                .relay
                .metrics
                .internal_errors
                .load(AtomicOrdering::Relaxed),
            1,
            "a five hundred nobody counted is a five hundred nobody sees"
        );
        assert_eq!(ledger.connections(), 0);

        let logged = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("bindings"), "{logged}");
        assert!(logged.contains(r#""route":"/v1/push""#), "{logged}");
        // What names the fault is the schema; what must not appear is anything
        // that could reach a phone.
        for secret in [TOKEN, fixture.bearer.expose(), bearer.as_str(), "Bearer"] {
            assert!(!logged.contains(secret), "the log carries {secret:?}");
        }
        fixture.drop_scratch();
    }

    /// A floor that cannot be read is a five hundred and never a floor of zero:
    /// answering zero to a permissions problem would re-admit every credential
    /// an incident response had just refused.
    #[tokio::test]
    async fn an_unreadable_generation_floor_refuses_rather_than_reading_as_zero() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("floor-broken", "sandbox", transport(Some(&base), None), &[]);
        std::fs::write(&fixture.floor_file, "not a number").unwrap();

        let reply = fixture.push(&doorbell(TOKEN, "sandbox")).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(ledger.connections(), 0);
        fixture.drop_scratch();
    }

    // -----------------------------------------------------------------------
    // The schema.
    // -----------------------------------------------------------------------

    /// A document that breaks the schema is a `400` and never a delivery — and
    /// it is refused before the credential is even looked at, so a caller
    /// cannot use malformed bodies to learn which bearers are real.
    #[tokio::test]
    async fn a_schema_violation_is_a_four_hundred_and_reaches_neither_apple_nor_the_bindings() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("schema", "sandbox", transport(Some(&base), None), &[]);

        let cases = [
            (
                doorbell(TOKEN, "sandbox").replace(r#""schema":1"#, r#""schema":2"#),
                "schema",
            ),
            (doorbell("nothex", "sandbox"), "token"),
            (
                doorbell(TOKEN, "codeconnect-gateway/src/main.rs"),
                "malformed",
            ),
            (
                test_request(TOKEN, "sandbox").replace(
                    r#"{"type":"test"}"#,
                    r#"{"type":"test","aps":{"alert":{"title":"anything"}}}"#,
                ),
                "malformed",
            ),
            (
                doorbell(TOKEN, "sandbox")
                    .replace(r#""blocked_count":1"#, r#""blocked_count":1000"#),
                "malformed",
            ),
        ];
        for (body, expected) in cases {
            let reply = fixture.push(&body).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(reply.body["error"], expected, "{body}");
        }
        // Even with no credential at all, which is the ordering that matters.
        let reply = push(&fixture.relay, None, None, b"not json at all", NOW).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(ledger.connections(), 0);
        fixture.drop_scratch();
    }

    // -----------------------------------------------------------------------
    // The gates in front of Apple.
    // -----------------------------------------------------------------------

    /// **The send kill switch, and the only property that matters about it: no
    /// APNs request is emitted.** A switch that answered `unavailable` while
    /// still reaching Apple would be useless in the incident it exists for.
    #[tokio::test]
    async fn the_send_kill_switch_emits_no_apns_request_at_all() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture(
            "kill-switch",
            "sandbox",
            transport(Some(&base), None),
            &[("RELAY_SEND_ENABLED", "false")],
        );

        for _ in 0..5 {
            let reply = fixture.push(&doorbell(TOKEN, "sandbox")).await;
            assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(reply.body["error"], "unavailable");
        }
        assert_eq!(
            ledger.connections(),
            0,
            "the kill switch let a connection through"
        );
        assert_eq!(ledger.streams(), 0);
        fixture.drop_scratch();
    }

    /// An environment with no key answers rather than borrowing the other
    /// environment's key, which would sign for the wrong topic.
    #[tokio::test]
    async fn a_binding_on_an_environment_without_a_key_answers_unavailable() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        // Production attested and production bound, with only a sandbox key
        // loaded: the binding is honoured and the environment it names has no
        // key, which is the one state this test is about.
        let fixture = correcting("no-key", transport(Some(&base), None));

        let reply = fixture.push(&doorbell(TOKEN, "production")).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.body["error"], "unavailable");
        assert_eq!(ledger.connections(), 0);
        fixture.drop_scratch();
    }

    /// The plan's per-binding rule, and the `Retry-After` it requires.
    #[tokio::test]
    async fn the_binding_budget_answers_a_429_that_says_when_to_come_back() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("limited", "sandbox", transport(Some(&base), None), &[]);
        let body = doorbell(TOKEN, "sandbox");

        for attempt in 0..crate::ratelimit::BINDING.burst {
            assert_eq!(
                fixture.push(&body).await.status,
                StatusCode::OK,
                "attempt {attempt} of the burst"
            );
        }
        let refused = fixture.push(&body).await;
        assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refused.body["error"], "rate_limited");
        let retry = refused
            .retry_after_seconds
            .expect("a 429 carries Retry-After");
        assert!((1..=60).contains(&retry), "{retry}");
        assert_eq!(
            ledger.streams(),
            crate::ratelimit::BINDING.burst as usize,
            "the refused push must not have reached Apple"
        );
        fixture.drop_scratch();
    }

    // -----------------------------------------------------------------------
    // Apple's answers.
    // -----------------------------------------------------------------------

    /// §7: `410` retires the binding, and the next push with the same bearer is
    /// a credential fault rather than a second `410`.
    #[tokio::test]
    async fn a_410_retires_the_binding_and_answers_unregistered() {
        let (base, _ledger) = apple(Plan::answering(410, r#"{"reason":"Unregistered"}"#)).await;
        let fixture = fixture("gone", "sandbox", transport(Some(&base), None), &[]);

        let reply = fixture.push(&doorbell(TOKEN, "sandbox")).await;
        assert_eq!(reply.status, StatusCode::GONE);
        assert_eq!(reply.body["error"], "unregistered");
        assert_eq!(fixture.binding().1, "revoked");

        let again = fixture.push(&doorbell(TOKEN, "sandbox")).await;
        assert_eq!(again.body["error"], "credential_invalid");

        // And §7's other half: the token is never reissued a credential.
        let db = fixture.relay.db.lock().unwrap();
        let terminal: String = db
            .query_row(
                "SELECT terminal_reason FROM bindings WHERE bearer_hash = ?1",
                [bearer_hash(&fixture.bearer)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(terminal, "unregistered");
        drop(db);
        fixture.drop_scratch();
    }

    /// **A development attestation may never walk a binding onto the production
    /// host.** Anybody with Xcode can mint a key in the development App Attest
    /// namespace, and Apple pairs that namespace with the sandbox host and
    /// nothing else. A `BadDeviceToken` there is a token that is not good — it
    /// is not evidence that the phone is a customer's, and a relay that tried
    /// production on it would hand the one namespace anybody can enter the
    /// authority to address a real phone and to keep it, since an acceptance is
    /// written back to the row.
    ///
    /// The negative half is the point of the test: **Apple's production host is
    /// never asked at all**, so the refusal is not a request that happened to
    /// fail. And the word is `rejected`, the same one a token refused at both
    /// hosts and a missing key already answer, so a caller cannot tell the three
    /// apart.
    #[tokio::test]
    async fn a_development_attested_binding_is_never_corrected_onto_the_production_host() {
        let (sandbox, sandbox_ledger) = apple(bad_token_plan()).await;
        let (production, production_ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture(
            "correction-forbidden",
            "sandbox",
            transport(Some(&sandbox), Some(&production)),
            &[],
        );

        let reply = fixture.push(&doorbell(TOKEN, "sandbox")).await;
        assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
        assert_eq!(reply.body["error"], "rejected");
        assert_eq!(
            sandbox_ledger.streams(),
            1,
            "the one attempt the pairing allows"
        );
        assert_eq!(
            production_ledger.connections(),
            0,
            "a forbidden correction must not reach Apple's production host at all"
        );

        // And nothing was stored: the binding is where enrollment put it, on the
        // host its namespace pairs with.
        assert_eq!(fixture.binding(), ("sandbox".into(), "active".into()));
        assert_eq!(fixture.terminal_reason(), None);
        assert_eq!(
            fixture
                .relay
                .metrics
                .environments_corrected
                .load(AtomicOrdering::Relaxed),
            0
        );
        assert_eq!(
            fixture
                .relay
                .metrics
                .internal_errors
                .load(AtomicOrdering::Relaxed),
            0,
            "a refusal the pairing requires is not a fault"
        );
        fixture.drop_scratch();
    }

    /// **Decision 4's one retry, on the namespace that may take it.** A
    /// production attestation is one Apple issues only to a build it
    /// distributed, so the opposite host is open to it: `BadDeviceToken` on
    /// production buys one attempt on the sandbox, and an acceptance corrects
    /// the binding rather than retiring a working registration.
    #[tokio::test]
    async fn a_bad_device_token_is_corrected_on_the_other_host_and_the_binding_is_updated() {
        let (sandbox, sandbox_ledger) = apple(Plan::answering(200, "")).await;
        let (production, production_ledger) = apple(bad_token_plan()).await;
        let fixture = correcting("correction", transport(Some(&sandbox), Some(&production)));

        let reply = fixture.push(&doorbell(TOKEN, "production")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.body["environment"], "sandbox",
            "the corrected environment is what the daemon CAS-persists"
        );
        assert_eq!(production_ledger.streams(), 1, "exactly one attempt each");
        assert_eq!(sandbox_ledger.streams(), 1);
        assert_eq!(fixture.binding().0, "sandbox");
        assert_eq!(
            fixture
                .relay
                .metrics
                .environments_corrected
                .load(AtomicOrdering::Relaxed),
            1
        );

        // The correction sticks: the next push goes straight to the corrected
        // host and does not pay for the wrong one again.
        let again = fixture.push(&doorbell(TOKEN, "production")).await;
        assert_eq!(again.body["environment"], "sandbox");
        assert_eq!(production_ledger.streams(), 1);
        assert_eq!(sandbox_ledger.streams(), 2);
        fixture.drop_scratch();
    }

    /// **The row is bounded by the namespace too, not only the correction.** A
    /// development-attested binding that names the production host is what the
    /// relay wrote before this rule existed, and it is carried into any database
    /// restored from that build. The first attempt is addressed by the row, so
    /// without this the old rows would go on reaching customers' phones with no
    /// correction involved at all.
    ///
    /// It is refused exactly as every other unhonoured bearer is — the same
    /// word, no `Retry-After`, charged to the address and not to the phone — so
    /// the refusal says nothing about which rule refused it.
    #[tokio::test]
    async fn a_development_attested_row_that_names_production_never_reaches_it() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = attested(
            "drifted-row",
            "development",
            "production",
            transport(None, Some(&base)),
            &[],
        );
        let bucket = crate::ratelimit::binding_key(&token_hash(TOKEN));

        let reply = fixture.push(&doorbell(TOKEN, "production")).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.body["error"], "credential_invalid");
        assert_eq!(reply.retry_after_seconds, None);
        assert_eq!(ledger.connections(), 0, "nothing may reach Apple");

        // The row is left alone — this relay refuses the authority, it does not
        // retire a phone — and the phone's own budget was never opened.
        assert_eq!(fixture.binding(), ("production".into(), "active".into()));
        assert_eq!(fixture.terminal_reason(), None);
        assert_eq!(fixture.day_count(&bucket), None);
        fixture.drop_scratch();
    }

    /// **The rule itself, over every pair there is.** Three of the four are
    /// allowed and exactly one is not, and the one that is not is the direction
    /// that matters: the namespace anybody may mint a key in must never reach a
    /// customer's phone. Written out rather than derived so that changing the
    /// table is a deliberate act.
    #[test]
    fn only_a_distributed_build_may_reach_the_production_host() {
        use crate::config::AttestEnvironment::{Development, Production as Distributed};
        use ApnsEnvironment::{Production, Sandbox};

        assert!(permits(Development, Sandbox), "its own pairing");
        assert!(permits(Distributed, Production), "its own pairing");
        assert!(
            permits(Distributed, Sandbox),
            "Decision 4's opposite host, for a build Apple distributed"
        );
        assert!(
            !permits(Development, Production),
            "a development attestation may never address a customer's phone"
        );
    }

    /// **The last line before the `UPDATE`.** A correction is refused before
    /// Apple is asked, so nothing reaches here with a forbidden environment —
    /// and if this file ever changes so that something does, the row is still
    /// not written and the fault is counted as the relay's own.
    #[test]
    fn a_forbidden_environment_is_not_stored_even_when_the_write_is_asked_for() {
        let fixture = fixture("forbidden-write", "sandbox", transport(None, None), &[]);

        correct_environment(
            &fixture.relay,
            &Authorized {
                token_hash: token_hash(TOKEN),
                bearer_hash: bearer_hash(&fixture.bearer),
                environment: ApnsEnvironment::Sandbox,
            },
            ApnsEnvironment::Production,
            NOW,
        );

        assert_eq!(fixture.binding(), ("sandbox".into(), "active".into()));
        assert_eq!(
            fixture
                .relay
                .metrics
                .environments_corrected
                .load(AtomicOrdering::Relaxed),
            0
        );
        assert_eq!(
            fixture
                .relay
                .metrics
                .internal_errors
                .load(AtomicOrdering::Relaxed),
            1,
            "reaching the write with a forbidden environment is a fault in this file"
        );
        fixture.drop_scratch();
    }

    /// A binding replaced while the push was in flight is not overwritten by a
    /// correction meant for the credential that has already been retired.
    #[tokio::test]
    async fn a_correction_does_not_clobber_a_binding_that_was_replaced_underneath_it() {
        let (sandbox, _s) = apple(bad_token_plan()).await;
        let (production, _p) = apple(Plan::answering(200, "")).await;
        let fixture = correcting(
            "correction-race",
            transport(Some(&sandbox), Some(&production)),
        );

        // The authorised binding, then the replacement an enrollment would have
        // committed while Apple was being asked.
        let replacement = {
            let db = fixture.relay.db.lock().unwrap();
            db.execute(
                "UPDATE bindings SET status = 'revoked', terminal_reason = 'superseded'
                 WHERE bearer_hash = ?1",
                [bearer_hash(&fixture.bearer)],
            )
            .unwrap();
            seed(&db, "production", TOKEN, "sandbox", 0)
        };

        // The correction is aimed at the bearer it authenticated, which is no
        // longer active, so nothing is written.
        correct_environment(
            &fixture.relay,
            &Authorized {
                token_hash: token_hash(TOKEN),
                bearer_hash: bearer_hash(&fixture.bearer),
                environment: ApnsEnvironment::Sandbox,
            },
            ApnsEnvironment::Production,
            NOW,
        );

        let db = fixture.relay.db.lock().unwrap();
        let live: String = db
            .query_row(
                "SELECT environment FROM bindings WHERE bearer_hash = ?1",
                [bearer_hash(&replacement)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(live, "sandbox", "the replacement was overwritten");
        drop(db);
        assert_eq!(
            fixture
                .relay
                .metrics
                .environments_corrected
                .load(AtomicOrdering::Relaxed),
            0
        );
        // **And the race is what refused it, not the namespace.** Every
        // assertion above would hold just as well if the pairing guard had
        // turned the write away before the `UPDATE` ran, and then this would be
        // a test of the guard wearing a race's name.
        assert_eq!(
            fixture
                .relay
                .metrics
                .internal_errors
                .load(AtomicOrdering::Relaxed),
            0,
            "the write reached the compare-and-set"
        );
        fixture.drop_scratch();
    }

    /// Refused at both hosts is a token that is simply not good, and Decision 4
    /// allows no further attempt.
    #[tokio::test]
    async fn a_bad_device_token_at_both_hosts_is_rejected_and_tried_no_further() {
        let (sandbox, sandbox_ledger) = apple(bad_token_plan()).await;
        let (production, production_ledger) = apple(bad_token_plan()).await;
        let fixture = correcting("both-bad", transport(Some(&sandbox), Some(&production)));

        let reply = fixture.push(&doorbell(TOKEN, "production")).await;
        assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
        assert_eq!(reply.body["error"], "rejected");
        assert_eq!(production_ledger.streams(), 1);
        assert_eq!(sandbox_ledger.streams(), 1);
        // And the binding is left exactly as it was: a rejection is not a
        // reason to retire a phone.
        assert_eq!(fixture.binding(), ("production".into(), "active".into()));
        fixture.drop_scratch();
    }

    /// With no key for the other host there is nothing to learn, so the one
    /// piece of evidence in hand stands.
    #[tokio::test]
    async fn a_bad_device_token_with_no_opposite_key_is_rejected_without_a_second_attempt() {
        let (base, ledger) = apple(bad_token_plan()).await;
        let fixture = correcting("no-opposite", transport(None, Some(&base)));

        let reply = fixture.push(&doorbell(TOKEN, "production")).await;
        assert_eq!(reply.body["error"], "rejected");
        assert_eq!(ledger.streams(), 1);
        fixture.drop_scratch();
    }

    /// **§7 does not care which host said it.** A `410` is the app being gone
    /// from the phone, and a `410` met on the corrected host is the same fact:
    /// the binding is terminal, the answer is `unregistered`, and the daemon
    /// throws the token away rather than recovering a credential.
    #[tokio::test]
    async fn a_410_on_the_opposite_host_retires_the_binding_and_answers_unregistered() {
        let (sandbox, _s) = apple(Plan::answering(410, r#"{"reason":"Unregistered"}"#)).await;
        let (production, _p) = apple(bad_token_plan()).await;
        let fixture = correcting(
            "correction-gone",
            transport(Some(&sandbox), Some(&production)),
        );

        let reply = fixture.push(&doorbell(TOKEN, "production")).await;
        assert_eq!(reply.status, StatusCode::GONE);
        assert_eq!(reply.body["error"], "unregistered");
        assert_eq!(fixture.binding(), ("production".into(), "revoked".into()));
        assert_eq!(
            fixture.terminal_reason().as_deref(),
            Some("unregistered"),
            "a token apple reported as gone must never be reissued a credential"
        );
        assert_eq!(
            fixture
                .relay
                .metrics
                .tokens_unregistered
                .load(AtomicOrdering::Relaxed),
            1
        );
        fixture.drop_scratch();
    }

    /// **Apple's own state is Apple's own state on either host.** A busy or
    /// unwell APNs met by the second attempt is `unavailable` — transient,
    /// nothing delivered, nothing learned — and never `rejected`, which tells
    /// the daemon a working token will never work again.
    #[tokio::test]
    async fn apples_own_state_on_the_opposite_host_is_unavailable_and_leaves_the_binding_alone() {
        for (status, reason) in [(429, "TooManyRequests"), (503, "ServiceUnavailable")] {
            let (sandbox, _s) = apple(Plan::answering(
                status,
                &format!(r#"{{"reason":"{reason}"}}"#),
            ))
            .await;
            let (production, _p) = apple(bad_token_plan()).await;
            let fixture = correcting(
                &format!("correction-{status}"),
                transport(Some(&sandbox), Some(&production)),
            );

            let reply = fixture.push(&doorbell(TOKEN, "production")).await;
            assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{reason}");
            assert_eq!(reply.body["error"], "unavailable", "{reason}");
            assert_eq!(
                fixture.binding(),
                ("production".into(), "active".into()),
                "{reason} is not a reason to give up on a phone"
            );
            assert_eq!(fixture.terminal_reason(), None, "{reason}");
            fixture.drop_scratch();
        }
    }

    /// An attempt that never reached Apple proved nothing about the token, so
    /// the second one that cannot reach it is `unavailable` for the same reason
    /// the first would have been.
    #[tokio::test]
    async fn an_unreachable_opposite_host_is_unavailable_and_never_rejected() {
        let (production, _p) = apple(bad_token_plan()).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = listener.local_addr().unwrap();
        drop(listener);
        let fixture = correcting(
            "correction-unreachable",
            transport(Some(&format!("http://{dead}")), Some(&production)),
        );

        let reply = fixture.push(&doorbell(TOKEN, "production")).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.body["error"], "unavailable");
        assert_eq!(fixture.binding(), ("production".into(), "active".into()));
        fixture.drop_scratch();
    }

    /// **The two attempts share one clock.**
    ///
    /// A first attempt that runs nearly to the bound leaves the correction only
    /// what is left of it, so the whole request stays inside the envelope the
    /// daemon's 45-second outer bound is built on. Given a deadline of its own
    /// the correction here would finish and answer `accepted` at close to twice
    /// the budget — long after the daemon stopped waiting.
    ///
    /// A spent budget is `unavailable` and not `rejected`: nothing was
    /// delivered, and the relay learned nothing about the token.
    #[tokio::test]
    async fn a_slow_attempt_and_its_correction_share_one_budget() {
        let budget = Duration::from_millis(1_000);
        let hold = Duration::from_millis(800);
        let mut refusing = bad_token_plan();
        refusing.hold = hold;
        let mut accepting = Plan::answering(200, "");
        accepting.hold = hold;
        let (production, production_ledger) = apple(refusing).await;
        let (sandbox, _s) = apple(accepting).await;
        let fixture = correcting(
            "one-budget",
            transport(Some(&sandbox), Some(&production)).with_deadline(budget),
        );

        let started = std::time::Instant::now();
        let reply = fixture.push(&doorbell(TOKEN, "production")).await;
        let elapsed = started.elapsed();

        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.body["error"], "unavailable");
        assert!(
            elapsed < budget + Duration::from_millis(500),
            "the correction started a clock of its own: {elapsed:?}"
        );
        assert_eq!(production_ledger.streams(), 1, "exactly one attempt each");
        assert_eq!(fixture.binding(), ("production".into(), "active".into()));
        fixture.drop_scratch();
    }

    /// **A relay serves one App Attest namespace, and a binding proved in the
    /// other one is authority it is not the relay for.**
    ///
    /// Apple issues a different namespace to a development build than to a
    /// distributed one, and they are two namespaces rather than a strict and a
    /// lenient one. An operator who moves a deployment across must not leave
    /// every credential minted before the move still pushing, when the same
    /// credential can no longer rotate, rebind, delete, or ask for its status.
    ///
    /// **And the refusal is the same one every dead bearer gets.** A namespace
    /// that answered differently — a `429`, a different word, a spend on the
    /// phone's own bucket — would be an oracle telling a caller which of the two
    /// namespaces its credential came from.
    #[tokio::test]
    async fn a_binding_from_the_other_attest_namespace_cannot_push() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture(
            "foreign-namespace",
            "sandbox",
            transport(Some(&base), None),
            &[("RELAY_ATTEST_ENVIRONMENT", "production")],
        );
        let body = doorbell(TOKEN, "sandbox");
        let bucket = crate::ratelimit::binding_key(&token_hash(TOKEN));

        for attempt in 0..20 {
            let reply = fixture.push(&body).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
            assert_eq!(reply.body["error"], "credential_invalid");
            assert_eq!(
                reply.retry_after_seconds, None,
                "a namespace this relay does not serve is not a rate limit"
            );
        }
        assert_eq!(ledger.connections(), 0, "nothing may reach Apple");

        // The binding is left exactly as it was: this relay refuses the
        // authority, it does not retire a phone the other relay still serves.
        assert_eq!(fixture.binding(), ("sandbox".into(), "active".into()));
        assert_eq!(fixture.terminal_reason(), None);

        // And charged where every other dead bearer is charged.
        assert_eq!(fixture.day_count(&bucket), None);
        let address = fixture
            .relay
            .limiter
            .ip_key("auth", Some("203.0.113.7"), NOW);
        assert_eq!(
            fixture.day_count(&address),
            Some(i64::from(crate::ratelimit::INVALID_AUTH_IP.burst))
        );
        fixture.drop_scratch();
    }

    /// **A dead bearer must not spend the budget its replacement will need.**
    ///
    /// §7's remedy for a lost Mac is rotation: revoke the old credential and
    /// issue a new one for the same phone. The rate key is the token, so both
    /// credentials name one bucket — and a relay that charged the refusal of the
    /// old bearer to that bucket would let whoever holds the lost Mac spend the
    /// phone's five hundred a day on requests it never served, leaving the
    /// replacement answering `429`. The containment would be the outage.
    ///
    /// Every way a bearer can be dead is here, because all three read the same
    /// row and all three refuse.
    #[tokio::test]
    async fn a_revoked_bearer_cannot_spend_the_budget_its_replacement_needs() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("dead-bearer", "sandbox", transport(Some(&base), None), &[]);
        let body = doorbell(TOKEN, "sandbox");
        let bucket = crate::ratelimit::binding_key(&token_hash(TOKEN));

        let dead = fixture.authorization();
        let set = |status: &str, reason: Option<&str>| {
            let db = fixture.relay.db.lock().unwrap();
            db.execute(
                "UPDATE bindings SET status = ?2, terminal_reason = ?3
                 WHERE bearer_hash = ?1",
                rusqlite::params![bearer_hash(&fixture.bearer), status, reason],
            )
            .unwrap();
        };
        // Every way a bearer can be dead, replayed between them far past the
        // five hundred a phone gets in a day. The floor is §7's containment for
        // a broad compromise and is the one that leaves the row looking alive.
        for state in ["below the floor", "revoked", "terminal"] {
            match state {
                "below the floor" => std::fs::write(&fixture.floor_file, "1\n").unwrap(),
                "revoked" => {
                    std::fs::write(&fixture.floor_file, "0\n").unwrap();
                    set("revoked", Some("rotated"));
                }
                _ => set("revoked", Some("unregistered")),
            }
            for attempt in 0..200 {
                let reply = fixture.push_as(&dead, &body).await;
                assert_eq!(
                    reply.status,
                    StatusCode::UNAUTHORIZED,
                    "{state}, attempt {attempt}"
                );
                assert_eq!(reply.body["error"], "credential_invalid");
                assert_eq!(
                    reply.retry_after_seconds, None,
                    "a dead credential needs to attest again, not to come back later"
                );
            }
        }

        // The rotation §7 prescribes: a new credential for the same phone, in
        // the place the revoked one held.
        let replacement = {
            let db = fixture.relay.db.lock().unwrap();
            seed(&db, "development", TOKEN, "sandbox", 0)
        };

        // **The phone's budget was never opened.** Six hundred replays of a
        // credential nobody honours, and the token has not spent one of its five
        // hundred.
        assert_eq!(fixture.day_count(&bucket), None);
        assert_eq!(ledger.connections(), 0, "nothing may reach Apple");

        // It was not free either: the refusals were charged to the address,
        // which stops being written to once its own allowance is gone.
        let address = fixture
            .relay
            .limiter
            .ip_key("auth", Some("203.0.113.7"), NOW);
        assert_eq!(
            fixture.day_count(&address),
            Some(i64::from(crate::ratelimit::INVALID_AUTH_IP.burst))
        );

        // And the replacement has its whole allowance, which is the property the
        // rotation exists to preserve.
        let authorization = format!("Bearer {}", replacement.expose());
        for attempt in 0..crate::ratelimit::BINDING.burst {
            assert_eq!(
                fixture.push_as(&authorization, &body).await.status,
                StatusCode::OK,
                "the replacement was refused on attempt {attempt}"
            );
        }
        assert_eq!(
            fixture.day_count(&bucket),
            Some(i64::from(crate::ratelimit::BINDING.burst)),
            "the only spending on this token is the replacement's own"
        );
        fixture.drop_scratch();
    }

    /// A `.p8` that is mounted and is not a key can sign nothing, and the push
    /// says so rather than reaching Apple with a signature it cannot make.
    ///
    /// The same state `/readyz` and the metric report as absent, met from the
    /// other end — which is the point: an operator watching readiness has to be
    /// watching the thing that decides this answer.
    #[tokio::test]
    async fn a_key_file_that_is_not_a_key_answers_unavailable() {
        let dir = scratch("malformed-key");
        let key_file = dir.join("apns-sandbox.p8");
        std::fs::write(&key_file, b"a file that exists").unwrap();
        let pairs: HashMap<String, String> = [
            ("RELAY_APNS_SANDBOX_KEY_ID", "SANDKEYID1"),
            ("RELAY_APNS_SANDBOX_TEAM_ID", "TEAMID1234"),
            ("RELAY_APNS_SANDBOX_TOPIC", "com.example.app"),
            ("RELAY_APNS_SANDBOX_KEY_FILE", key_file.to_str().unwrap()),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
        let config = RelayConfig::read(move |key| pairs.get(key).cloned()).unwrap();
        let fixture = fixture(
            "malformed-key-push",
            "sandbox",
            ApnsTransport::from_config(&config).unwrap(),
            &[],
        );

        let reply = fixture.push(&doorbell(TOKEN, "sandbox")).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.body["error"], "unavailable");
        assert_eq!(fixture.binding(), ("sandbox".into(), "active".into()));

        let _ = std::fs::remove_dir_all(&dir);
        fixture.drop_scratch();
    }

    /// Apple's own state is `unavailable`; Apple refusing this request forever
    /// is `rejected`. The daemon drops an ordinary doorbell either way and
    /// reports a test honestly.
    #[tokio::test]
    async fn apples_answers_map_to_the_documented_typed_outcomes() {
        for (status, reason, expected, expected_status) in [
            (
                429,
                "TooManyRequests",
                "unavailable",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                503,
                "ServiceUnavailable",
                "unavailable",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                403,
                "InvalidProviderToken",
                "rejected",
                StatusCode::BAD_GATEWAY,
            ),
            (413, "PayloadTooLarge", "rejected", StatusCode::BAD_GATEWAY),
        ] {
            let (base, _ledger) = apple(Plan::answering(
                status,
                &format!(r#"{{"reason":"{reason}"}}"#),
            ))
            .await;
            let fixture = fixture(
                &format!("apns-{status}"),
                "sandbox",
                transport(Some(&base), None),
                &[],
            );
            let reply = fixture.push(&doorbell(TOKEN, "sandbox")).await;
            assert_eq!(reply.body["error"], expected, "{status} {reason}");
            assert_eq!(reply.status, expected_status, "{status} {reason}");
            assert_eq!(
                fixture.binding(),
                ("sandbox".into(), "active".into()),
                "only a 410 retires a binding"
            );
            fixture.drop_scratch();
        }
    }

    /// A transport that cannot reach Apple at all is `unavailable`, and the
    /// answer arrives rather than the request hanging.
    #[tokio::test]
    async fn an_unreachable_apns_is_unavailable_and_never_a_fact_about_the_phone() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let fixture = fixture(
            "unreachable",
            "sandbox",
            transport(Some(&format!("http://{address}")), None),
            &[],
        );

        let reply = fixture.push(&doorbell(TOKEN, "sandbox")).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.body["error"], "unavailable");
        assert_eq!(fixture.binding(), ("sandbox".into(), "active".into()));
        fixture.drop_scratch();
    }

    /// **The relay answers only after Apple answers** (§4). A relay that
    /// replied early would make `test_push` a guess rather than a check.
    #[tokio::test]
    async fn the_answer_waits_for_apple() {
        let mut plan = Plan::answering(200, "");
        plan.hold = Duration::from_millis(300);
        let (base, _ledger) = apple(plan).await;
        let fixture = fixture("waits", "sandbox", transport(Some(&base), None), &[]);

        let started = std::time::Instant::now();
        let reply = fixture.push(&doorbell(TOKEN, "sandbox")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "the relay answered before Apple did: {:?}",
            started.elapsed()
        );
        fixture.drop_scratch();
    }

    // -----------------------------------------------------------------------
    // The route, and the words.
    // -----------------------------------------------------------------------

    /// The bearer travels in `Authorization` and the route is mounted there
    /// only — a token in a URL is a token in every proxy's access log.
    #[tokio::test]
    async fn the_route_reads_the_bearer_from_the_header_and_answers_json() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("route", "sandbox", transport(Some(&base), None), &[]);

        let response = router(fixture.relay.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/push")
                    .header("authorization", fixture.authorization())
                    .header("content-type", "application/json")
                    .body(Body::from(doorbell(TOKEN, "sandbox")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["outcome"], "accepted");
        assert_eq!(ledger.streams(), 1);

        // Without the header it is a credential fault, not a delivery.
        let refused = router(fixture.relay.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/push")
                    .body(Body::from(doorbell(TOKEN, "sandbox")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(ledger.streams(), 1);
        fixture.drop_scratch();
    }

    /// A body above the schema's bound is refused by the router before a
    /// handler sees it.
    #[tokio::test]
    async fn an_oversized_body_never_reaches_the_handler() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("oversized", "sandbox", transport(Some(&base), None), &[]);

        let response = router(fixture.relay.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/push")
                    .header("authorization", fixture.authorization())
                    .body(Body::from("x".repeat(crate::dto::MAX_BODY_BYTES + 1)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(ledger.connections(), 0);
        fixture.drop_scratch();
    }

    /// §4's typed outcomes, spelled the way the plan spells them, so a renamed
    /// variant is a failing build rather than a word the daemon does not know.
    ///
    /// It says nothing about which of them the route reaches — that is
    /// [`every_word_this_route_answers_is_one_the_plan_names`], which drives the
    /// route instead of the enum.
    #[test]
    fn the_typed_outcomes_are_spelled_the_way_the_plan_spells_them() {
        let words: Vec<&str> = [
            Outcome::Accepted,
            Outcome::Unregistered,
            Outcome::CredentialInvalid,
            Outcome::Rejected,
            Outcome::Unavailable,
        ]
        .iter()
        .map(|outcome| outcome.word())
        .collect();
        assert_eq!(
            words,
            [
                "accepted",
                "unregistered",
                "credential_invalid",
                "rejected",
                "unavailable"
            ]
        );
        // The sixth is `rate_limited`, produced by the shared helper that also
        // puts `Retry-After` on the refusal.
        assert_eq!(
            crate::enroll::limited(ROUTE, 30, None).body["error"],
            "rate_limited"
        );
    }

    /// **Every word the route can actually answer, collected by taking every
    /// path to one.** §4's five typed outcomes are not the whole vocabulary: a
    /// document that breaks the schema, a body that is not one, and a fault in
    /// the relay's own state each produce a word of their own, and a daemon
    /// reading `error` meets all of them. Anything new here has to be added to
    /// this list on purpose.
    #[tokio::test]
    async fn every_word_this_route_answers_is_one_the_plan_names() {
        let word = |reply: &Reply| -> String {
            reply.body["outcome"]
                .as_str()
                .or_else(|| reply.body["error"].as_str())
                .unwrap_or_else(|| panic!("an answer with neither word: {}", reply.body))
                .to_string()
        };
        let mut said = std::collections::BTreeSet::new();

        // Everything reachable against an Apple that accepts.
        let (base, _ledger) = apple(Plan::answering(200, "")).await;
        let accepting = fixture("words", "sandbox", transport(Some(&base), None), &[]);
        let body = doorbell(TOKEN, "sandbox");
        said.insert(word(&accepting.push(&body).await));
        for broken in [
            body.replace(r#""schema":1"#, r#""schema":2"#),
            doorbell("nothex", "sandbox"),
            r#"{"schema":1,"title":"x"}"#.to_string(),
        ] {
            said.insert(word(&accepting.push(&broken).await));
        }
        said.insert(word(
            &accepting
                .push_as("Bearer aNeverIssuedCredentialValue", &body)
                .await,
        ));
        // The burst, and then the refusal that follows it.
        for _ in 0..=crate::ratelimit::BINDING.burst {
            said.insert(word(&accepting.push(&body).await));
        }
        accepting.drop_scratch();

        // Apple's own refusals, each on the fixture whose plan produces it.
        let (gone, _ledger) = apple(Plan::answering(410, r#"{"reason":"Unregistered"}"#)).await;
        let retiring = fixture("words-gone", "sandbox", transport(Some(&gone), None), &[]);
        said.insert(word(&retiring.push(&body).await));
        retiring.drop_scratch();

        let (bad, _ledger) = apple(bad_token_plan()).await;
        let refusing = fixture("words-bad", "sandbox", transport(Some(&bad), None), &[]);
        said.insert(word(&refusing.push(&body).await));
        refusing.drop_scratch();

        let switched_off = fixture(
            "words-off",
            "sandbox",
            transport(Some(&base), None),
            &[("RELAY_SEND_ENABLED", "false")],
        );
        said.insert(word(&switched_off.push(&body).await));
        switched_off.drop_scratch();

        // And the relay's own state going wrong, which is a word too.
        let broken = fixture(
            "words-internal",
            "sandbox",
            transport(Some(&base), None),
            &[],
        );
        {
            let db = broken.relay.db.lock().unwrap();
            db.execute_batch("DROP TABLE bindings").unwrap();
        }
        said.insert(word(&broken.push(&body).await));
        broken.drop_scratch();

        let expected: std::collections::BTreeSet<String> = [
            "accepted",
            "credential_invalid",
            "internal",
            "malformed",
            "rate_limited",
            "rejected",
            "schema",
            "token",
            "unavailable",
            "unregistered",
        ]
        .iter()
        .map(|word| (*word).to_string())
        .collect();
        assert_eq!(said, expected);
    }

    /// A binding row whose environment is neither word is a fault in this
    /// process's own state, not a push to be guessed at.
    #[test]
    fn a_binding_environment_is_one_of_two_words_or_it_is_nothing() {
        assert_eq!(
            binding_environment("sandbox"),
            Some(ApnsEnvironment::Sandbox)
        );
        assert_eq!(
            binding_environment("production"),
            Some(ApnsEnvironment::Production)
        );
        for stored in ["", "prod", "Production", "staging"] {
            assert_eq!(binding_environment(stored), None, "{stored:?}");
        }
    }

    // -----------------------------------------------------------------------
    // Logs.
    // -----------------------------------------------------------------------

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

        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    /// **The acceptance gate, on the path that handles a raw device token on
    /// every single request:** no authorization header, no raw token, no bearer
    /// and no address reaches a log line — through a delivery, a refused
    /// credential and a rate-limited refusal alike.
    ///
    /// Captured at `TRACE` so the assertion covers what a deployment turned all
    /// the way up would print, not only what `info` prints.
    #[tokio::test]
    async fn no_authorization_header_token_bearer_or_address_reaches_the_log() {
        let (base, _ledger) = apple(Plan::answering(200, "")).await;
        let fixture = fixture("redaction", "sandbox", transport(Some(&base), None), &[]);
        let address = "203.0.113.7";
        let body = doorbell(TOKEN, "sandbox");

        crate::logging::enable_every_callsite();
        let sink = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(sink.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        assert_eq!(fixture.push(&body).await.status, StatusCode::OK);
        let refused = push(
            &fixture.relay,
            Some("Bearer aNeverIssuedCredentialValue"),
            Some(address),
            body.as_bytes(),
            NOW,
        )
        .await;
        assert_eq!(refused.body["error"], "credential_invalid");
        // Spend the rest of the budget so the third line is a 429.
        let mut limited = None;
        for _ in 0..crate::ratelimit::BINDING.burst + 2 {
            let reply = fixture.push(&body).await;
            if reply.status == StatusCode::TOO_MANY_REQUESTS {
                limited = Some(reply);
            }
        }
        assert!(limited.is_some(), "the budget was never exhausted");
        drop(_guard);

        let logged = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(!logged.is_empty(), "nothing was logged at all");
        for secret in [
            TOKEN,
            fixture.bearer.expose(),
            "aNeverIssuedCredentialValue",
            "Bearer",
            "authorization",
            address,
        ] {
            assert!(
                !logged.contains(secret),
                "the log carries {secret:?}: {logged}"
            );
        }
        // What it does carry is eight characters of a hash, which follows one
        // caller through an incident and is nothing on its own.
        let id = &token_hash(TOKEN)[..8];
        assert!(logged.contains(id), "{logged}");
        assert!(logged.contains(r#""outcome":"accepted""#), "{logged}");
        assert!(
            logged.contains(r#""outcome":"credential_invalid""#),
            "{logged}"
        );
        assert!(logged.contains(r#""outcome":"rate_limited""#), "{logged}");
        fixture.drop_scratch();
    }
}
