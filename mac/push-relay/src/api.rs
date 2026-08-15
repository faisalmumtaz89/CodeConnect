//! The HTTP surface: what the relay answers, and what it refuses to answer with.
//!
//! Three routes, and the difference between the first two is the whole design.
//! `/healthz` says the process is running and touches nothing. `/readyz` says
//! whether it can do its job, which means asking the database and the
//! generation floor. Merging them would make a slow disk look like a dead
//! process, and the platform restarts a dead process.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use push_core::ApnsEnvironment;
use rusqlite::Connection;

use crate::apns::{ApnsSigner, ApnsTransport};
use crate::config::RelayConfig;
use crate::dto::MAX_BODY_BYTES;
use crate::logging::RequestLog;
use crate::ratelimit::Limiter;

/// Counters, and deliberately nothing that identifies a caller.
///
/// A per-binding or per-address counter would be an identifier held for as long
/// as the process lives, reachable by anyone who can read the metrics endpoint.
/// These are totals.
#[derive(Default)]
pub struct Metrics {
    liveness_checks: AtomicU64,
    readiness_checks: AtomicU64,
    metrics_scrapes: AtomicU64,
    pub(crate) challenges_issued: AtomicU64,
    pub(crate) credentials_issued: AtomicU64,
    pub(crate) credentials_revoked: AtomicU64,
    /// A credential the relay would not honour, which is the number an operator
    /// watches after raising the generation floor.
    pub(crate) credential_refusals: AtomicU64,
    /// Requests carrying a bearer this relay never minted. §7's abuse runbook
    /// asks for invalid-auth traffic as an aggregate, and this is it: a number
    /// climbing here without a matching climb in enrollments is somebody
    /// guessing rather than a fleet recovering from a restore.
    pub(crate) invalid_auth: AtomicU64,
    /// The share of those the strict rotating-address rule stopped serving. The
    /// two together are how much guessing arrived and how much of it the relay
    /// declined to do any work for.
    pub(crate) invalid_auth_limited: AtomicU64,
    pub(crate) rate_limited: AtomicU64,
    /// Faults in the relay's own state — a database that has gone read-only, a
    /// full disk, a row the relay wrote and cannot read back. Every one of them
    /// is a five hundred somebody has to see, and a log line alone is a thing
    /// nobody is watching.
    pub(crate) internal_errors: AtomicU64,
    /// Notifications Apple took, which is the only number that means a phone
    /// rang.
    pub(crate) pushes_accepted: AtomicU64,
    /// Every other answer `/v1/push` gave, whatever the reason. The reasons are
    /// in the logs; a counter per reason would be a shape an operator has to
    /// keep in step with §4's outcome set.
    pub(crate) pushes_refused: AtomicU64,
    /// Tokens Apple reported as gone, which retire a binding permanently.
    pub(crate) tokens_unregistered: AtomicU64,
    /// Bindings whose environment a `BadDeviceToken` on the other host proved
    /// wrong. A number climbing here is a registration path handing out the
    /// wrong environment, not a phone misbehaving.
    pub(crate) environments_corrected: AtomicU64,
}

/// What the daily backup job last did, as shared state rather than as a log
/// line.
///
/// **A backup that has stopped working has to be visible where an operator is
/// already looking.** Whether the key file parses and the target names a scheme
/// this build serves is decided once, at startup, and is the whole of
/// [`Relay::backup_absence`]; a full disk, a permission that changed, a source
/// the copy cannot read are all conditions that arrive afterwards, and none of
/// them can be seen from configuration. So the job writes what happened here and
/// `/readyz` and `/metrics` read it.
///
/// Nothing has failed before the first run, which is why a fresh process is
/// healthy rather than unknown: the job runs immediately at startup, so the
/// window in which "no run yet" and "the last run worked" differ is the length
/// of one copy.
#[derive(Default)]
pub(crate) struct BackupState {
    /// When the last successful run finished. Zero is "not in this process".
    last_success_ms: AtomicI64,
    failing: AtomicBool,
}

impl BackupState {
    pub(crate) fn succeeded(&self, now_ms: i64) {
        self.last_success_ms.store(now_ms, Ordering::Relaxed);
        self.failing.store(false, Ordering::Relaxed);
    }

    pub(crate) fn failed(&self) {
        self.failing.store(true, Ordering::Relaxed);
    }

    fn healthy(&self) -> bool {
        !self.failing.load(Ordering::Relaxed)
    }

    /// How long ago the last backup succeeded, in seconds, or `-1` for never.
    ///
    /// Negative rather than absent because a gauge that disappears when the
    /// thing it measures goes wrong is a gauge nobody can alert on.
    fn age_seconds(&self, now_ms: i64) -> i64 {
        match self.last_success_ms.load(Ordering::Relaxed) {
            0 => -1,
            last => (now_ms - last).max(0) / 1_000,
        }
    }
}

/// Everything a handler is given.
#[derive(Clone)]
pub struct Relay {
    pub(crate) config: Arc<RelayConfig>,
    /// One connection behind a lock, because the database is one file on one
    /// disk serving one instance. A pool would be more connections contending
    /// for the same write lock, which SQLite resolves by making one of them
    /// wait anyway.
    pub(crate) db: Arc<Mutex<Connection>>,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) limiter: Arc<Limiter>,
    /// One pooled HTTP/2 client per environment, opened once and held for the
    /// life of the process — which is why it lives on the shared state rather
    /// than being built per request.
    pub(crate) apns: Arc<ApnsTransport>,
    /// Why the daily backup cannot run, when it cannot. Reported by `/readyz`
    /// rather than being a startup failure: a relay that refused to serve
    /// because its backup key was missing would turn a recoverable operational
    /// gap into an outage.
    pub(crate) backup_absence: Option<Arc<str>>,
    /// What the last run of that job actually did, which is the half
    /// configuration cannot answer.
    pub(crate) backup: Arc<BackupState>,
    started: Instant,
}

impl Relay {
    pub fn new(config: RelayConfig, db: Connection) -> Self {
        let apns = apns_transport(&config);
        let backup_absence = crate::backup::absence(&config).map(Arc::from);
        Self::with_transport(config, db, apns, backup_absence)
    }

    pub fn with_transport(
        config: RelayConfig,
        db: Connection,
        apns: ApnsTransport,
        backup_absence: Option<Arc<str>>,
    ) -> Self {
        let limiter = Limiter::new(&config.ip_pepper_file);
        Relay {
            config: Arc::new(config),
            db: Arc::new(Mutex::new(db)),
            metrics: Arc::new(Metrics::default()),
            limiter: Arc::new(limiter),
            apns: Arc::new(apns),
            backup_absence,
            backup: Arc::new(BackupState::default()),
            started: Instant::now(),
        }
    }

    /// **Configured *and* working.** A relay whose key file parses and whose
    /// disk is full has backups in the sense that its configuration describes
    /// one, and has none in the sense that matters — so the word this endpoint
    /// and this metric use means the second thing.
    fn backups_enabled(&self) -> bool {
        self.backup_absence.is_none() && self.backup.healthy()
    }
}

/// The transport the deployment's keys describe, or one that can send nothing.
///
/// **A trust store that will not load is reported, not fatal.** It is the one
/// failure `ApnsTransport::from_config` has, and it means a broken image — but
/// exiting here would take down the enrollment endpoints as well, and those are
/// what a phone needs in order to be ready when the image is fixed. Both
/// environments then answer `unavailable` with the reason, which is exactly the
/// state a missing key already produces.
fn apns_transport(config: &RelayConfig) -> ApnsTransport {
    match ApnsTransport::from_config(config) {
        Ok(transport) => transport,
        Err(e) => {
            let why = format!("{e:#}");
            tracing::error!(error = %why, "the APNs transport could not be built");
            ApnsTransport::new(ApnsSigner::Absent(why.clone()), ApnsSigner::Absent(why))
                .expect("a transport with no keys opens no connection")
        }
    }
}

pub fn router(relay: Relay) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .merge(crate::enroll::routes())
        .merge(crate::push::routes())
        // A body limit on the router rather than on a handler, so it is a
        // property of the service and not of whoever remembered to ask for it.
        // The enrollment route raises its own, because an attestation object is
        // several kilobytes and nothing else here is.
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(relay)
}

/// **Liveness, and nothing else.** It must not open the database, read a secret
/// file, or reach Apple.
///
/// The platform pauses traffic after fifteen seconds of failing health checks
/// and restarts the instance after sixty. A check that touched the disk would
/// turn disk pressure — the one condition under which the relay most needs to
/// stay up and answer `/readyz` honestly — into a restart loop, and a restart
/// loop into an outage. A constant is the correct answer to "is the process
/// running".
async fn healthz(State(relay): State<Relay>) -> impl IntoResponse {
    relay
        .metrics
        .liveness_checks
        .fetch_add(1, Ordering::Relaxed);
    (StatusCode::OK, "ok\n")
}

/// Readiness: what an operator needs, with nothing an attacker can use.
///
/// The APNs environments report **present or absent** and not why. The reason
/// names a variable or a path, which is a fact about the deployment rather than
/// about the service, and this endpoint answers strangers; the full sentence is
/// logged once at startup where the operator reads it.
async fn readyz(State(relay): State<Relay>) -> impl IntoResponse {
    relay
        .metrics
        .readiness_checks
        .fetch_add(1, Ordering::Relaxed);

    let schema_version = {
        let db = relay.db.lock().unwrap_or_else(|e| e.into_inner());
        crate::db::schema_version(&db)
    };
    let Ok(schema_version) = schema_version else {
        return blocked("database");
    };
    // A floor that cannot be read is not a floor of zero. Answering zero to a
    // permissions problem would re-admit every credential an incident response
    // had just refused.
    let Ok(generation_floor) = relay.config.generation_floor() else {
        return blocked("generation_floor");
    };

    let body = serde_json::json!({
        "ready": true,
        "schema_version": schema_version,
        "generation_floor": generation_floor,
        "send_enabled": relay.config.send_enabled,
        "enrollment_enabled": relay.config.enrollment_enabled,
        "attest_environment": relay.config.attest_environment.as_str(),
        "app_id_configured": relay.config.app_id.is_some(),
        // **Which pepper the address buckets are keyed with.** An ephemeral one
        // still limits, but its buckets do not survive a restart, and that is a
        // deployment fact an operator has to be able to see rather than infer
        // from a mounted path that may not be there.
        "ip_pepper": relay.limiter.pepper_source(),
        "git_sha": relay.config.git_sha.as_deref(),
        "apns": {
            "sandbox": relay.config.slot(ApnsEnvironment::Sandbox).key().is_some(),
            "production": relay.config.slot(ApnsEnvironment::Production).key().is_some(),
        },
        // **Present or absent, like the keys, and for the same reason.** A
        // relay whose backups are silently off is one restore away from the
        // incident §7 exists for, so it is on the endpoint an operator watches;
        // *why* is in the startup log, because the reason names a path. A run
        // that failed counts as off, because a configuration that describes a
        // backup nobody is taking is the state this word exists to deny.
        "backups_enabled": relay.backups_enabled(),
    });
    RequestLog {
        route: "/readyz",
        status: StatusCode::OK.as_u16(),
        outcome: "ready",
        binding_hash: None,
    }
    .emit();
    json(StatusCode::OK, &body)
}

/// The service is up and cannot serve. `blocked` is one word from a closed set,
/// so the answer says which subsystem without describing the deployment.
fn blocked(
    subsystem: &'static str,
) -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
    RequestLog {
        route: "/readyz",
        status: StatusCode::SERVICE_UNAVAILABLE.as_u16(),
        outcome: subsystem,
        binding_hash: None,
    }
    .emit();
    json(
        StatusCode::SERVICE_UNAVAILABLE,
        &serde_json::json!({ "ready": false, "blocked": subsystem }),
    )
}

fn json(
    status: StatusCode,
    body: &serde_json::Value,
) -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        format!("{body}\n"),
    )
}

/// Counters in the text exposition format, with no identifier in any label.
async fn metrics(State(relay): State<Relay>) -> impl IntoResponse {
    relay
        .metrics
        .metrics_scrapes
        .fetch_add(1, Ordering::Relaxed);
    let m = &relay.metrics;
    let config = &relay.config;
    let body = format!(
        "codeconnect_relay_uptime_seconds {}\n\
         codeconnect_relay_liveness_checks_total {}\n\
         codeconnect_relay_readiness_checks_total {}\n\
         codeconnect_relay_metrics_scrapes_total {}\n\
         codeconnect_relay_challenges_issued_total {}\n\
         codeconnect_relay_credentials_issued_total {}\n\
         codeconnect_relay_credentials_revoked_total {}\n\
         codeconnect_relay_credential_refusals_total {}\n\
         codeconnect_relay_invalid_auth_total {}\n\
         codeconnect_relay_invalid_auth_limited_total {}\n\
         codeconnect_relay_rate_limited_total {}\n\
         codeconnect_relay_internal_errors_total {}\n\
         codeconnect_relay_pushes_accepted_total {}\n\
         codeconnect_relay_pushes_refused_total {}\n\
         codeconnect_relay_tokens_unregistered_total {}\n\
         codeconnect_relay_environments_corrected_total {}\n\
         codeconnect_relay_send_enabled {}\n\
         codeconnect_relay_enrollment_enabled {}\n\
         codeconnect_relay_backups_enabled {}\n\
         codeconnect_relay_backup_age_seconds {}\n\
         codeconnect_relay_apns_key_present{{environment=\"sandbox\"}} {}\n\
         codeconnect_relay_apns_key_present{{environment=\"production\"}} {}\n",
        relay.started.elapsed().as_secs(),
        m.liveness_checks.load(Ordering::Relaxed),
        m.readiness_checks.load(Ordering::Relaxed),
        m.metrics_scrapes.load(Ordering::Relaxed),
        m.challenges_issued.load(Ordering::Relaxed),
        m.credentials_issued.load(Ordering::Relaxed),
        m.credentials_revoked.load(Ordering::Relaxed),
        m.credential_refusals.load(Ordering::Relaxed),
        m.invalid_auth.load(Ordering::Relaxed),
        m.invalid_auth_limited.load(Ordering::Relaxed),
        m.rate_limited.load(Ordering::Relaxed),
        m.internal_errors.load(Ordering::Relaxed),
        m.pushes_accepted.load(Ordering::Relaxed),
        m.pushes_refused.load(Ordering::Relaxed),
        m.tokens_unregistered.load(Ordering::Relaxed),
        m.environments_corrected.load(Ordering::Relaxed),
        u8::from(config.send_enabled),
        u8::from(config.enrollment_enabled),
        u8::from(relay.backups_enabled()),
        relay.backup.age_seconds(crate::enroll::now_ms()),
        u8::from(config.slot(ApnsEnvironment::Sandbox).key().is_some()),
        u8::from(config.slot(ApnsEnvironment::Production).key().is_some()),
    );
    RequestLog {
        route: "/metrics",
        status: StatusCode::OK.as_u16(),
        outcome: "scraped",
        binding_hash: None,
    }
    .emit();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    fn relay(env: &[(&str, &str)]) -> Relay {
        let map: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let config = RelayConfig::read(move |key| map.get(key).cloned()).unwrap();
        Relay::new(config, crate::db::open_in_memory().unwrap())
    }

    /// A directory under the OS temp dir, cleaned up by the caller. The process
    /// id is in the name because two `cargo test` invocations can overlap on one
    /// machine, and a fixed path means one of them deleting the other's fixture
    /// halfway through.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("push-relay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn get_route(relay: Relay, path: &str) -> (StatusCode, String) {
        let response = router(relay)
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// **Liveness takes no database lock.** Another thread holds the
    /// connection's mutex for the whole call, so a handler that reached for it
    /// would not return at all — which is what a stalled disk would do to a
    /// health check that touched the database, and the platform restarts a
    /// process whose health check stops answering.
    ///
    /// It proves the absence of the lock and not the absence of every wait: the
    /// mutex is held on an ordinary thread, so the runtime always has a worker
    /// free to run the handler.
    #[tokio::test]
    async fn health_answers_without_taking_the_database_lock() {
        let relay = relay(&[]);
        let held = relay.db.clone();
        let (locked, is_locked) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            locked.send(()).unwrap();
            let _ = released.recv();
        });
        is_locked.recv().unwrap();

        let (status, body) = get_route(relay.clone(), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok\n");

        release.send(()).unwrap();
        holder.join().unwrap();
    }

    #[tokio::test]
    async fn readiness_reports_the_state_an_operator_has_to_act_on() {
        let dir = scratch("readyz");
        let sandbox_key = dir.join("apns-sandbox.p8");
        std::fs::write(&sandbox_key, b"a file that exists").unwrap();
        std::fs::write(dir.join("generation-floor"), "12\n").unwrap();
        std::fs::write(dir.join("ip-pepper"), b"a mounted secret file").unwrap();

        let relay = relay(&[
            ("RELAY_APNS_SANDBOX_KEY_ID", "SANDKEYID1"),
            ("RELAY_APNS_SANDBOX_TEAM_ID", "TEAMID1234"),
            ("RELAY_APNS_SANDBOX_TOPIC", "com.example.app"),
            ("RELAY_APNS_SANDBOX_KEY_FILE", sandbox_key.to_str().unwrap()),
            (
                "RELAY_IP_PEPPER_FILE",
                dir.join("ip-pepper").to_str().unwrap(),
            ),
            (
                "RELAY_GENERATION_FLOOR_FILE",
                dir.join("generation-floor").to_str().unwrap(),
            ),
            ("RELAY_SEND_ENABLED", "false"),
            ("RELAY_GIT_SHA", "0123456789abcdef"),
            ("RELAY_ATTEST_ENVIRONMENT", "production"),
        ]);

        let (status, body) = get_route(relay, "/readyz").await;
        assert_eq!(status, StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["ready"], true);
        assert_eq!(parsed["generation_floor"], 12);
        assert_eq!(parsed["send_enabled"], false);
        assert_eq!(parsed["enrollment_enabled"], true);
        assert_eq!(parsed["attest_environment"], "production");
        assert_eq!(parsed["app_id_configured"], false);
        assert_eq!(parsed["git_sha"], "0123456789abcdef");
        assert_eq!(parsed["ip_pepper"], "file");
        assert_eq!(parsed["apns"]["sandbox"], true);
        assert_eq!(parsed["apns"]["production"], false);
        assert!(parsed["schema_version"].as_u64().unwrap() > 0);

        // Nothing about where a secret lives, and nothing that is one.
        assert!(!body.contains("/etc/secrets"), "{body}");
        assert!(!body.contains("SANDKEYID1"), "{body}");
        assert!(!body.contains(sandbox_key.to_str().unwrap()), "{body}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A relay with no keys at all is a running relay, not a failed one.
    #[tokio::test]
    async fn a_relay_with_no_apns_keys_still_reports_ready() {
        let (status, body) = get_route(relay(&[]), "/readyz").await;
        assert_eq!(status, StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["ready"], true);
        assert_eq!(parsed["apns"]["sandbox"], false);
        assert_eq!(parsed["apns"]["production"], false);
        assert_eq!(parsed["generation_floor"], 0);
        assert_eq!(parsed["git_sha"], serde_json::Value::Null);
        // No pepper file mounted: the limiter minted one for this process and
        // says so rather than silently stopping counting.
        assert_eq!(parsed["ip_pepper"], "ephemeral");
    }

    /// An unreadable generation floor is the one thing that must not read as
    /// zero, so readiness fails instead.
    #[tokio::test]
    async fn an_unreadable_generation_floor_is_not_a_floor_of_zero() {
        let dir = scratch("floor-unreadable");
        let floor = dir.join("generation-floor");
        std::fs::write(&floor, "not a number").unwrap();

        let relay = relay(&[("RELAY_GENERATION_FLOOR_FILE", floor.to_str().unwrap())]);
        let (status, body) = get_route(relay, "/readyz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["ready"], false);
        assert_eq!(parsed["blocked"], "generation_floor");
        assert!(!body.contains(floor.to_str().unwrap()), "{body}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn metrics_count_what_happened_and_name_nobody() {
        let relay = relay(&[("RELAY_ENROLLMENT_ENABLED", "false")]);
        let _ = get_route(relay.clone(), "/healthz").await;
        let _ = get_route(relay.clone(), "/healthz").await;
        let _ = get_route(relay.clone(), "/readyz").await;

        let (status, body) = get_route(relay, "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("codeconnect_relay_liveness_checks_total 2"),
            "{body}"
        );
        assert!(
            body.contains("codeconnect_relay_readiness_checks_total 1"),
            "{body}"
        );
        assert!(body.contains("codeconnect_relay_send_enabled 1"), "{body}");
        assert!(
            body.contains("codeconnect_relay_enrollment_enabled 0"),
            "{body}"
        );
        assert!(
            body.contains("codeconnect_relay_apns_key_present{environment=\"sandbox\"} 0"),
            "{body}"
        );
    }

    /// **A plain 404 for everything the service does not offer.** A path that
    /// answered anything other than "no such route" would be a claim about a
    /// service the caller is not talking to — and an unversioned alias of a
    /// versioned route is exactly that claim.
    #[tokio::test]
    async fn nothing_answers_outside_the_operational_and_versioned_routes() {
        for path in [
            "/",
            // `/v1/push` exists; the unversioned alias of it must not.
            "/push",
            "/v1/push/",
            "/v2/push",
            "/enroll",
            "/challenge",
            "/attest/challenge",
            "/v1/attest",
            "/v2/attest/challenge",
            "/v1/credential",
        ] {
            let (status, _) = get_route(relay(&[]), path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    /// And the versioned route is there — a `405` rather than a `404`, because
    /// it exists and answers `POST`. Asserted alongside the list above so the
    /// two cannot drift into agreeing that nothing is mounted.
    #[tokio::test]
    async fn the_versioned_push_route_exists_and_takes_a_post() {
        let (status, _) = get_route(relay(&[]), "/v1/push").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }
}
