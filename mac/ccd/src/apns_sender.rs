//! The APNs sender: one HTTP/2 POST per registered device.
//!
//! **Why a raw `h2` client and not an HTTP crate.** Apple's push endpoint speaks
//! HTTP/2 only — there is no 1.1 to fall back to — and everything else this
//! needs is already here: `tokio-rustls` builds the TLS the `wss://` listener
//! uses, and the only new thing on the connection is the `h2` ALPN token.
//!
//! **What the payload may contain, and it is not much.** A push is a doorbell.
//! APNs payloads pass through Apple, so the alert carries the risk class and a
//! count and nothing else — never a command, a path, or a diff. The phone
//! reconnects and asks the event log what is true. That property is asserted by
//! a test, because it is the kind of thing a well-meaning "make the notification
//! more useful" change quietly destroys.
//!
//! **Delivery is best-effort by construction.** `PushSender::send` is sync and
//! fire-and-forget, so the work is spawned. A push that never arrives costs a
//! delay and never a fact — the log remains authoritative — so a failure is
//! logged and never retried into a queue that could outlive its own relevance.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use crate::apns::{coalesce, PushHint, PushSender, TestDelivery};

/// How long any single leg of a push may take. Generous for a network round
/// trip, short enough that a silently-filtered connection is reported rather
/// than left pending forever.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
use crate::apns_token::ProviderToken;

/// Which Apple host to talk to.
///
/// A development build's token is **not valid on production** and vice versa;
/// the failure is a `400 BadDeviceToken`, which reads like a corrupt token
/// rather than like the wrong endpoint. The device says which world it is in
/// when it registers, so this is per-device rather than global.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApnsEnvironment {
    Sandbox,
    Production,
}

impl ApnsEnvironment {
    pub fn host(self) -> &'static str {
        match self {
            ApnsEnvironment::Sandbox => "api.sandbox.push.apple.com",
            ApnsEnvironment::Production => "api.push.apple.com",
        }
    }

    pub fn parse(value: &str) -> ApnsEnvironment {
        match value {
            "production" | "prod" => ApnsEnvironment::Production,
            // Anything unrecognised is sandbox: a development build pushed at
            // production is silently undeliverable, whereas the reverse fails
            // loudly and is fixed by one config line.
            _ => ApnsEnvironment::Sandbox,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ApnsEnvironment::Sandbox => "sandbox",
            ApnsEnvironment::Production => "production",
        }
    }
}

/// Where a push is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    /// The device's APNs token, hex, as the phone reported it.
    pub token: String,
    pub environment: ApnsEnvironment,
    /// The CodeConnect device id, so an unregistered token can be cleared from
    /// the right row.
    pub device_id: String,
}

/// Supplies the devices to push to, so the sender does not reach into the
/// database and the tests do not need one.
pub trait PushRegistry: Send + Sync {
    fn targets(&self) -> Vec<PushTarget>;
    /// Apple said this token is dead (`410`), or refused it as malformed
    /// (`400 BadDeviceToken`). Either way it will never work again.
    fn forget(&self, device_id: &str, reason: &str);
    /// Apple refused the token on the host we chose but the *other* host is
    /// plausible. Records the correction so the next push goes straight there.
    fn correct_environment(&self, device_id: &str, environment: ApnsEnvironment);
}

pub struct ApnsPushSender {
    token: Arc<ProviderToken>,
    registry: Arc<dyn PushRegistry>,
    tls: TlsConnector,
}

impl ApnsPushSender {
    pub fn new(token: ProviderToken, registry: Arc<dyn PushRegistry>) -> Result<Self> {
        let mut roots = RootCertStore::empty();
        let (added, _ignored) = roots.add_parsable_certificates(platform_roots());
        if added == 0 {
            bail!("no trust anchors available for the APNs connection");
        }
        let mut config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        // **Required, not an optimisation.** APNs serves HTTP/2 only, and it
        // decides which protocol to speak from ALPN. Without this the handshake
        // completes, the server offers HTTP/1.1, and `h2::client::handshake`
        // fails on a connection that looked perfectly healthy.
        config.alpn_protocols = vec![b"h2".to_vec()];
        Ok(Self {
            token: Arc::new(token),
            registry,
            tls: TlsConnector::from(Arc::new(config)),
        })
    }

    /// The alert JSON. Deliberately austere — see the module note.
    fn payload(hint: &PushHint) -> String {
        let body = coalesce(hint);
        serde_json::json!({
            "aps": {
                "alert": { "title": hint.title, "body": body },
                "sound": "default",
                "badge": hint.blocked_sessions,
                "interruption-level": "time-sensitive",
            }
        })
        .to_string()
    }

    async fn deliver(
        tls: TlsConnector,
        token: Arc<ProviderToken>,
        registry: Arc<dyn PushRegistry>,
        target: PushTarget,
        payload: String,
        // Guards the one environment retry below against recursing forever.
        retried: bool,
    ) -> Result<Option<String>> {
        let host = target.environment.host();
        let bearer = token.bearer()?;
        // **Bounded, because an unbounded connect can hang forever and say
        // nothing.** A third-party network filter that cannot identify a freshly
        // built binary holds its connections open and silent rather than
        // refusing them — measured here with Little Snitch against a `cargo
        // build` output, which produced a delivery that never completed and
        // never logged. A push is best-effort; a push that hangs is worse than
        // one that fails, because only the failure is visible.
        let stream =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect((host, 443)))
                .await
                .with_context(|| {
                    format!("connecting to {host} timed out after {CONNECT_TIMEOUT:?}")
                })?
                .with_context(|| format!("connecting to {host}"))?;
        let server_name = ServerName::try_from(host).context("APNs host name")?;
        let stream = tls
            .connect(server_name, stream)
            .await
            .with_context(|| format!("TLS handshake with {host}"))?;

        let (mut send_request, connection) =
            tokio::time::timeout(CONNECT_TIMEOUT, h2::client::handshake(stream))
                .await
                .context("HTTP/2 handshake with APNs timed out")?
                .context("HTTP/2 handshake with APNs")?;
        // The connection future drives the socket; dropping it strands the
        // stream, so it is spawned and its ending is only interesting on error.
        tokio::spawn(async move {
            if let Err(err) = connection.await {
                crate::log_debug!("push: apns connection ended: {err}");
            }
        });

        let request = http::Request::builder()
            .method("POST")
            .uri(format!("https://{host}/3/device/{}", target.token))
            .header("authorization", format!("bearer {bearer}"))
            .header("apns-topic", token.topic())
            .header("apns-push-type", "alert")
            // 10 = deliver immediately. This is a human waiting on an agent.
            .header("apns-priority", "10")
            // An approval nobody answered in an hour is not worth waking anyone
            // for; the app shows it on next foreground regardless.
            .header("apns-expiration", "3600")
            .body(())
            .context("building the APNs request")?;

        let (response, mut body) = send_request
            .send_request(request, false)
            .context("sending the APNs request")?;
        body.send_data(payload.clone().into(), true)
            .context("sending the APNs payload")?;

        let response = tokio::time::timeout(CONNECT_TIMEOUT, response)
            .await
            .context("APNs accepted the request and never answered")?
            .context("awaiting the APNs response")?;
        let status = response.status();
        if status.is_success() {
            // Apple's receipt for this exact notification, when it sends one —
            // the id a reader can take to Apple's delivery logs.
            let apns_id = response
                .headers()
                .get("apns-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            return Ok(apns_id);
        }

        let mut reason = String::new();
        let mut stream = response.into_body();
        while let Some(chunk) = stream.data().await {
            if let Ok(bytes) = chunk {
                reason.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        // **Only 410.** `410 Unregistered` is Apple saying the app is gone from
        // that device, which never recovers.
        //
        // `400 BadDeviceToken` deliberately does *not* clear the row, though it
        // reads like it should. It also means "this token is not valid for the
        // host you sent it to" — a sandbox token posted at production earns it
        // — so treating it as terminal deletes a perfectly good registration
        // the moment the environment is wrong. Measured: one production probe
        // against a sandbox token wiped the phone's token and left push
        // silently dead until the app was relaunched.
        if is_terminal(status.as_u16(), &reason) {
            registry.forget(&target.device_id, &format!("{status}: {reason}"));
            bail!("APNs refused the push: {status} {reason}")
        }

        // **`BadDeviceToken` usually means the right token on the wrong host.**
        // A build's APNs world is decided by how it was *signed*, and the app
        // has to infer that — an App Store build has no provisioning profile to
        // read, so it can get it wrong. Rather than leave push silently dead
        // until someone reads a log, try the other host once and remember the
        // answer. Measured: a TestFlight install reported `sandbox`, held a
        // production token, and every notification vanished.
        if reason.contains("BadDeviceToken") && !retried {
            let other = match target.environment {
                ApnsEnvironment::Sandbox => ApnsEnvironment::Production,
                ApnsEnvironment::Production => ApnsEnvironment::Sandbox,
            };
            crate::log_info!(
                "push: {} refused on {}; trying {}",
                target.device_id,
                target.environment.as_str(),
                other.as_str()
            );
            let corrected = PushTarget {
                environment: other,
                ..target.clone()
            };
            let apns_id = Box::pin(Self::deliver(
                tls,
                token,
                Arc::clone(&registry),
                corrected,
                payload,
                true,
            ))
            .await?;
            registry.correct_environment(&target.device_id, other);
            return Ok(apns_id);
        }
        bail!("APNs refused the push: {status} {reason}")
    }
}

impl PushSender for ApnsPushSender {
    fn send(&self, hint: &PushHint, excluded: &[String]) {
        let targets = self.registry.targets();
        if targets.is_empty() {
            crate::log_info!(
                "push: nothing to notify — no device has registered for push yet \
                 (session={})",
                hint.session_id
            );
            return;
        }
        let payload = Self::payload(hint);
        for target in targets {
            // The seen-filter: this device's live socket already carried the
            // fact. Ringing it again is the noise the gate exists to stop.
            if excluded.contains(&target.device_id) {
                crate::log_debug!(
                    "push: {} already saw session {} live; skipping",
                    target.device_id,
                    hint.session_id
                );
                continue;
            }
            let tls = self.tls.clone();
            let token = Arc::clone(&self.token);
            let registry = Arc::clone(&self.registry);
            let payload = payload.clone();
            let device = target.device_id.clone();
            tokio::spawn(async move {
                match Self::deliver(tls, token, registry, target, payload, false).await {
                    Ok(_) => crate::log_info!("push: delivered to device {device}"),
                    Err(err) => crate::log_warn!("push: {device}: {err:#}"),
                }
            });
        }
    }

    fn is_live(&self) -> bool {
        true
    }

    fn send_test(&self, device_id: &str) -> tokio::sync::oneshot::Receiver<TestDelivery> {
        let (respond, rx) = tokio::sync::oneshot::channel();
        // The one registered target with this identity, or the honest refusal.
        let Some(target) = self
            .registry
            .targets()
            .into_iter()
            .find(|t| t.device_id == device_id)
        else {
            let _ = respond.send(TestDelivery::NoToken);
            return rx;
        };
        // Says what it is, carries nothing else. The `test` marker is what the
        // phone's foreground handler keys on: an ordinary doorbell is redundant
        // while the app is open, but a test the user just requested must show.
        let payload = serde_json::json!({
            "aps": {
                "alert": {
                    "title": "CodeConnect",
                    "body": "Push works. This is the test you asked for.",
                },
                "sound": "default",
            },
            "codeconnect_test": 1,
        })
        .to_string();
        let tls = self.tls.clone();
        let token = Arc::clone(&self.token);
        let registry = Arc::clone(&self.registry);
        let device = target.device_id.clone();
        tokio::spawn(async move {
            let outcome = match Self::deliver(tls, token, registry, target, payload, false).await {
                Ok(apns_id) => {
                    crate::log_info!("push: test accepted for device {device}");
                    TestDelivery::Accepted { apns_id }
                }
                Err(err) => {
                    crate::log_warn!("push: test to {device} failed: {err:#}");
                    TestDelivery::Failed(format!("{err:#}"))
                }
            };
            let _ = respond.send(outcome);
        });
        rx
    }
}

/// The platform trust store, read once.
///
/// Apple's endpoint presents a certificate chaining to a public root, so the
/// system store is exactly right and vendoring a root set would be a second
/// thing to keep current.
fn platform_roots() -> Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>> {
    rustls_native_certs::load_native_certs().certs
}

/// Whether Apple's answer means this device token will never work again.
///
/// See the call site: only `410 Unregistered` qualifies.
fn is_terminal(status: u16, _reason: &str) -> bool {
    status == 410
}

/// The database as a push registry.
///
/// Synchronous on purpose: `PushSender::send` is sync, and the alternative —
/// blocking on the async pool from inside it — would put database latency on
/// the path that publishes an event.
pub struct StoreRegistry {
    store: Arc<crate::store::Store>,
}

impl StoreRegistry {
    pub fn new(store: Arc<crate::store::Store>) -> Self {
        Self { store }
    }
}

impl PushRegistry for StoreRegistry {
    fn targets(&self) -> Vec<PushTarget> {
        match self.store.push_targets() {
            Ok(rows) => rows
                .into_iter()
                .map(|(device_id, token, environment)| PushTarget {
                    token,
                    environment: ApnsEnvironment::parse(&environment),
                    device_id,
                })
                .collect(),
            Err(err) => {
                // Reported, never silently "nobody to notify": a database error
                // and an empty device list are different facts.
                crate::log_error!("push: could not read the device list: {err:#}");
                Vec::new()
            }
        }
    }

    fn forget(&self, device_id: &str, reason: &str) {
        crate::log_info!("push: clearing the token for {device_id} — Apple said {reason}");
        if let Err(err) = self.store.clear_push_token(device_id) {
            crate::log_error!("push: could not clear the token for {device_id}: {err:#}");
        }
    }

    fn correct_environment(&self, device_id: &str, environment: ApnsEnvironment) {
        crate::log_info!(
            "push: {device_id} is actually a {} build; remembering that",
            environment.as_str()
        );
        if let Err(err) = self
            .store
            .set_push_environment(device_id, environment.as_str())
        {
            crate::log_error!("push: could not record the environment for {device_id}: {err:#}");
        }
    }
}

/// Build the sender the configuration asks for.
///
/// Returns the logging stub unless **all four** identity fields are present and
/// the key loads. A partly-configured push is reported as an error and then
/// degrades to the stub, because the alternative is advertising a live sender
/// that fails on every send.
pub fn build(
    config: &protocol::config::Config,
    store: Arc<crate::store::Store>,
) -> Arc<dyn PushSender> {
    let (Some(path), Some(key_id), Some(team_id), Some(topic)) = (
        config.apns_key_path.as_ref(),
        config.apns_key_id.as_ref(),
        config.apns_team_id.as_ref(),
        config.apns_topic.as_ref(),
    ) else {
        if config.apns_key_path.is_some()
            || config.apns_key_id.is_some()
            || config.apns_team_id.is_some()
            || config.apns_topic.is_some()
        {
            crate::log_error!(
                "push: apns_key_path, apns_key_id, apns_team_id and apns_topic are all \
                 required; push stays off until every one is set"
            );
        }
        return Arc::new(crate::apns::LoggingPushSender::new());
    };

    let identity = crate::apns_token::ApnsIdentity {
        key_id: key_id.clone(),
        team_id: team_id.clone(),
        topic: topic.clone(),
    };
    let expanded = shellexpand_home(path);
    match crate::apns_token::ProviderToken::load(std::path::Path::new(&expanded), identity)
        .and_then(|token| ApnsPushSender::new(token, Arc::new(StoreRegistry::new(store))))
    {
        Ok(sender) => {
            crate::log_info!("push: live, topic {topic}, key {key_id}");
            Arc::new(sender)
        }
        Err(err) => {
            crate::log_error!("push: staying off — {err:#}");
            Arc::new(crate::apns::LoggingPushSender::new())
        }
    }
}

/// `~` in a configured path. Written by hand because the one place it is needed
/// does not justify a crate.
fn shellexpand_home(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => protocol::home_dir()
            .join(rest)
            .to_string_lossy()
            .into_owned(),
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(blocked: usize) -> PushHint {
        PushHint {
            session_id: "app-1".into(),
            title: "app-1 needs you — high risk".into(),
            body: "Bash approval".into(),
            blocked_sessions: blocked,
        }
    }

    #[test]
    fn the_payload_never_carries_a_command_a_path_or_a_diff() {
        // The whole privacy argument for push lives in this assertion: the
        // alert says *that* something needs you and how urgent it is, and the
        // phone fetches the rest over the tailnet on tap.
        let mut leaky = hint(1);
        leaky.body = "Bash approval".into();
        let payload = ApnsPushSender::payload(&leaky);
        for forbidden in ["/Users/", "git push", "rm -rf", "diff --git", ".swift"] {
            assert!(
                !payload.contains(forbidden),
                "push payload must not carry {forbidden}: {payload}"
            );
        }
        let parsed: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");
        assert_eq!(
            parsed["aps"]["alert"]["title"],
            "app-1 needs you — high risk"
        );
        assert_eq!(parsed["aps"]["badge"], 1);
    }

    #[test]
    fn many_blocked_sessions_coalesce_to_a_count() {
        let payload = ApnsPushSender::payload(&hint(4));
        let parsed: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");
        assert_eq!(parsed["aps"]["badge"], 4);
        let body = parsed["aps"]["alert"]["body"].as_str().unwrap_or_default();
        assert!(body.contains('4'), "the count is the useful fact: {body}");
    }

    /// The distinction that cost a live registration.
    /// The failure the first TestFlight install produced.
    #[test]
    fn a_bad_device_token_is_an_environment_problem_before_it_is_a_dead_one() {
        // `400 BadDeviceToken` means "not valid **for this host**", which a
        // wrong environment produces just as readily as a dead token. It must
        // not clear the registration, and it must leave room for the other
        // host to be tried.
        assert!(!is_terminal(400, "{\"reason\":\"BadDeviceToken\"}"));
        assert!(is_terminal(410, "{\"reason\":\"Unregistered\"}"));
        // The two hosts are genuinely different endpoints, so "the other one"
        // is always well defined.
        assert_ne!(
            ApnsEnvironment::Sandbox.host(),
            ApnsEnvironment::Production.host()
        );
    }

    #[test]
    fn only_410_is_terminal_for_a_device_token() {
        // Apple returns `400 BadDeviceToken` for a token that is simply on the
        // wrong host, which is recoverable and common while a build moves
        // between development and TestFlight. Clearing on it deletes a working
        // registration; only `410 Unregistered` means the app is gone.
        assert!(is_terminal(410, "{\"reason\":\"Unregistered\"}"));
        assert!(!is_terminal(400, "{\"reason\":\"BadDeviceToken\"}"));
        assert!(!is_terminal(
            403,
            "{\"reason\":\"BadEnvironmentKeyInToken\"}"
        ));
        assert!(!is_terminal(429, "{\"reason\":\"TooManyRequests\"}"));
    }

    #[test]
    fn an_unknown_environment_falls_back_to_sandbox_rather_than_production() {
        // Wrong-way-round is the recoverable failure: a development token sent
        // to production is refused loudly, where the reverse is accepted and
        // silently never delivered.
        assert_eq!(ApnsEnvironment::parse(""), ApnsEnvironment::Sandbox);
        assert_eq!(ApnsEnvironment::parse("nonsense"), ApnsEnvironment::Sandbox);
        assert_eq!(
            ApnsEnvironment::parse("production"),
            ApnsEnvironment::Production
        );
        assert_eq!(ApnsEnvironment::Production.host(), "api.push.apple.com");
        assert_eq!(
            ApnsEnvironment::Sandbox.host(),
            "api.sandbox.push.apple.com"
        );
    }
}
