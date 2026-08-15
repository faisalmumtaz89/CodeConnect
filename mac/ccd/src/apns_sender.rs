//! The APNs sender: one HTTP/2 POST per registered device.
//!
//! **Why a raw `h2` client and not an HTTP crate.** Apple's push endpoint speaks
//! HTTP/2 only — there is no 1.1 to fall back to — and everything else this
//! needs is already here: `tokio-rustls` builds the TLS the `wss://` listener
//! uses, and the only new thing on the connection is the `h2` ALPN token.
//!
//! **What the payload may contain, and it is not much.** A push is a doorbell.
//! APNs payloads pass through Apple, so the alert carries the project a run is
//! working in, one canned sentence and a count — never a command, a path, a
//! diff, a tool name, a risk class, or any identifier at all. Beside the words
//! there is one thing: which of four kinds rang, which is what lets a tap choose
//! between the decision list and the fleet without naming anything. Once more
//! than one run is holding a decision the sentence is *replaced* by that count. The phone
//! reconnects and asks the event log what is true. That property is asserted by
//! a test, because it is the kind of thing a well-meaning "make the notification
//! more useful" change quietly destroys.
//!
//! **Delivery is best-effort by construction.** `PushSender::send` is sync and
//! fire-and-forget, so the work is spawned. A push that never arrives costs a
//! delay and never a fact — the log remains authoritative — so a failure is
//! logged and never retried into a queue that could outlive its own relevance.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use push_core::{is_terminal, terminal_refusal, ProviderToken, COLLAPSE_ID, TEST_COLLAPSE_ID};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;

use crate::apns::{alert, PushHint, PushMode, PushSender, TestDelivery};
use crate::push_queue::{
    keep_running, queue_for, recipients, retire, serve_with, Delivery, DeviceQueue,
    CONNECT_TIMEOUT, DELIVERY_DEADLINE,
};

/// Which world a device lives in travels with the device, so the socket handler
/// that registers one names this type too.
pub use push_core::ApnsEnvironment;

/// Where a push is going, and who supplies the list — shared with every other
/// way of reaching a phone, so they are named here as well as where they live.
pub(crate) use crate::push_queue::{PushRegistry, PushTarget};

/// Now, in whole seconds — the unit `apns-expiration` is written in.
fn now_secs() -> i64 {
    protocol::time::now_unix_ms() / 1000
}

pub struct ApnsPushSender {
    token: Arc<ProviderToken>,
    registry: Arc<dyn PushRegistry>,
    tls: TlsConnector,
    queues: std::sync::Mutex<HashMap<String, Arc<DeviceQueue>>>,
}

impl ApnsPushSender {
    pub fn new(token: ProviderToken, registry: Arc<dyn PushRegistry>) -> Result<Self> {
        Ok(Self {
            token: Arc::new(token),
            registry,
            tls: crate::tls::outbound_h2("APNs")?,
            queues: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// The alert JSON. Deliberately austere — see the module note.
    ///
    /// `aps`, plus one word saying which kind of doorbell rang.
    ///
    /// **No routing, session, request or device identifier.** Not a uid, not a
    /// request id, not a token — nothing that could be reversed into a command
    /// or left pointing at a decision somebody has since answered. The kind is a
    /// closed set of four words and is stable for the life of the notification,
    /// which is why a tap can act on it: only an approval has a card in the
    /// phone's decision list, so only an approval opens it.
    fn payload(hint: &PushHint) -> String {
        let (title, body) = alert(hint);
        serde_json::json!({
            "aps": {
                "alert": { "title": title, "body": body },
                "sound": "default",
                "badge": hint.blocked_sessions,
                "interruption-level": "time-sensitive",
            },
            "codeconnect": { "kind": hint.kind.tag() }
        })
        .to_string()
    }

    /// **A device that is gone takes its worker with it.** A queue and the two
    /// tasks behind it are per device and would otherwise outlive every phone
    /// that ever paired — and re-pairing mints a new device id, so the set only
    /// grows.
    fn retire_device(&self, device_id: &str) {
        retire(&mut self.queues.lock().unwrap(), device_id);
    }

    /// Hand a delivery to this device's worker, starting one if it is the
    /// first push this device has ever been sent.
    fn enqueue(&self, delivery: Delivery) {
        let device_id = delivery.target.device_id.clone();
        let queue = {
            let mut queues = self.queues.lock().unwrap();
            let (queue, is_new) = queue_for(&mut queues, &device_id);
            if is_new {
                // **Supervised from here, not on the next push.** Checking
                // the worker's health when work arrives would leave the
                // delivery that raced its death sitting in a queue nothing
                // drains — and the next push might be hours away. The
                // watcher outlives the worker and starts another the
                // instant one ends.
                let (started, tls, token, registry) = (
                    Arc::clone(&queue),
                    self.tls.clone(),
                    Arc::clone(&self.token),
                    Arc::clone(&self.registry),
                );
                let named = device_id.clone();
                tokio::spawn(async move {
                    keep_running(
                        || {
                            tokio::spawn(Self::serve(
                                Arc::clone(&started),
                                tls.clone(),
                                Arc::clone(&token),
                                Arc::clone(&registry),
                            ))
                        },
                        &named,
                    )
                    .await
                });
            }
            queue
        };
        queue.push(delivery, &device_id);
    }

    /// One device's deliveries, one at a time, forever.
    async fn serve(
        queue: Arc<DeviceQueue>,
        tls: TlsConnector,
        token: Arc<ProviderToken>,
        registry: Arc<dyn PushRegistry>,
    ) {
        serve_with(queue, move |target, payload, collapse| {
            let tls = tls.clone();
            let token = Arc::clone(&token);
            let registry = Arc::clone(&registry);
            async move {
                match tokio::time::timeout(
                    DELIVERY_DEADLINE,
                    Self::deliver(tls, token, registry, target, payload, collapse, false),
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => bail!("delivery timed out after {DELIVERY_DEADLINE:?}"),
                }
            }
        })
        .await
    }

    async fn deliver(
        tls: TlsConnector,
        token: Arc<ProviderToken>,
        registry: Arc<dyn PushRegistry>,
        target: PushTarget,
        payload: String,
        collapse: &'static str,
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

        let request = push_core::request(
            host,
            &target.token,
            &bearer,
            token.topic(),
            now_secs(),
            collapse,
        )
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
            // **Only a refusal about the token in use retires the device.** A
            // late `410` for a token the phone has already replaced says
            // nothing about the one it is using now, and treating it as a
            // departure would close a queue holding that token's work.
            let was_current = registry.forget(
                &target.device_id,
                &target.token,
                &format!("{status}: {reason}"),
            );
            return Err(terminal_refusal(status.as_u16(), &reason, was_current));
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
                collapse,
                true,
            ))
            .await?;
            registry.correct_environment(&target.device_id, &target.token, other);
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
                hint.session_uid
            );
            return;
        }
        let payload = Self::payload(hint);
        for target in recipients(targets, excluded) {
            self.enqueue(Delivery {
                target,
                payload: payload.clone(),
                collapse: COLLAPSE_ID,
                respond: None,
            });
        }
    }

    fn retire(&self, device_id: &str) {
        self.retire_device(device_id);
    }

    fn mode(&self) -> PushMode {
        PushMode::Direct
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
        // Through the same per-device queue as every other push: a test the
        // user asked for is still a notification competing for the one slot
        // Apple keeps.
        self.enqueue(Delivery {
            target,
            payload,
            collapse: TEST_COLLAPSE_ID,
            respond: Some(respond),
        });
        rx
    }
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
                .map(|row| PushTarget {
                    environment: ApnsEnvironment::parse(&row.environment),
                    token: row.token,
                    device_id: row.device_id,
                    credential: row.credential,
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

    fn forget(&self, device_id: &str, refused: &str, reason: &str) -> bool {
        match self.store.clear_push_token(device_id, refused) {
            Ok(true) => {
                crate::log_info!("push: cleared the token for {device_id} — Apple said {reason}");
                true
            }
            Ok(false) => {
                // The phone re-registered while this push was in flight. The
                // refusal is about a token nothing uses any more, and acting on
                // it would retire a device that is present and working.
                crate::log_info!(
                    "push: {device_id} answered {reason} for a token it has already replaced"
                );
                false
            }
            Err(err) => {
                crate::log_error!("push: could not clear the token for {device_id}: {err:#}");
                false
            }
        }
    }

    fn correct_environment(&self, device_id: &str, token: &str, environment: ApnsEnvironment) {
        crate::log_info!(
            "push: {device_id} is actually a {} build; remembering that",
            environment.as_str()
        );
        if let Err(err) = self
            .store
            .set_push_environment(device_id, token, environment.as_str())
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

    let identity = push_core::ApnsIdentity {
        key_id: key_id.clone(),
        team_id: team_id.clone(),
        topic: topic.clone(),
    };
    let expanded = shellexpand_home(path);
    match ProviderToken::load(std::path::Path::new(&expanded), identity)
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
            project_label: "Aion".into(),
            kind: crate::apns::PushKind::Approval,
            blocked_sessions: blocked,
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
        }
    }

    fn target(device: &str) -> PushTarget {
        PushTarget {
            token: "aa".into(),
            environment: ApnsEnvironment::Sandbox,
            device_id: device.into(),
            credential: None,
        }
    }

    #[test]
    fn a_device_that_already_saw_it_live_is_not_rung() {
        let fleet = || vec![target("phone-1"), target("phone-2"), target("phone-3")];
        let ids = |targets: Vec<PushTarget>| -> Vec<String> {
            targets.into_iter().map(|t| t.device_id).collect()
        };

        assert_eq!(
            ids(recipients(fleet(), &["phone-2".to_string()])),
            vec!["phone-1".to_string(), "phone-3".to_string()],
            "the one that saw it live is dropped and the others are kept"
        );
        assert_eq!(
            ids(recipients(fleet(), &[])),
            vec![
                "phone-1".to_string(),
                "phone-2".to_string(),
                "phone-3".to_string()
            ],
            "nothing excluded, everyone rings"
        );
        assert!(
            recipients(
                fleet(),
                &[
                    "phone-1".to_string(),
                    "phone-2".to_string(),
                    "phone-3".to_string()
                ]
            )
            .is_empty(),
            "a fleet that has all seen it live is a push with nobody to send to"
        );
    }

    #[test]
    fn every_ordinary_payload_is_exactly_the_document_it_is_supposed_to_be() {
        let mut checked = 0;
        for &kind in crate::apns::PushKind::ALL {
            for blocked in [0usize, 1, 2, 4] {
                for label in ["Aion", ""] {
                    let hint = PushHint {
                        project_label: label.into(),
                        kind,
                        blocked_sessions: blocked,
                        session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
                    };
                    let payload = ApnsPushSender::payload(&hint);
                    let parsed: serde_json::Value =
                        serde_json::from_str(&payload).expect("valid JSON");

                    let title = if label.is_empty() {
                        "CodeConnect"
                    } else {
                        label
                    };
                    let body = if blocked > 1 {
                        format!("{blocked} agents need you")
                    } else {
                        kind.sentence().to_string()
                    };
                    assert_eq!(
                        parsed,
                        serde_json::json!({
                            "aps": {
                                "alert": { "title": title, "body": body },
                                "sound": "default",
                                "badge": blocked,
                                "interruption-level": "time-sensitive",
                            },
                            "codeconnect": { "kind": kind.tag() }
                        }),
                        "{payload}"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(
            checked,
            crate::apns::PushKind::ALL.len() * 4 * 2,
            "the whole matrix, not a sample"
        );
    }

    /// No identifier travels through Apple, and there is none to travel.
    ///
    /// `session_uid` is on the hint for the log, and it is a ULID whose leading
    /// bits are the run's start time — a notification carrying one would let
    /// Apple group a device's pushes into sessions and time them.
    #[test]
    fn the_payload_carries_no_identifier_that_could_be_correlated_or_reversed() {
        let hint = hint(1);
        let payload = ApnsPushSender::payload(&hint);
        assert!(!payload.contains(&hint.session_uid), "a ULID dates the run");
        for forbidden in [
            "/Users/",
            "git push",
            "rm -rf",
            "diff --git",
            "Bash",
            "risk",
        ] {
            assert!(
                !payload.contains(forbidden),
                "push payload must not carry {forbidden}: {payload}"
            );
        }
    }

    /// Apple rejects anything over 4KB with `413`.
    ///
    /// **Through the real resolver, not a hand-picked string.** Hard-coding a
    /// 40-character label would keep passing if the resolver's own cap were
    /// raised, while production quietly outgrew the limit. This feeds it a
    /// pathological directory name and lets it decide how long a label can be.
    #[test]
    fn the_payload_stays_well_inside_the_apns_limit() {
        let absurd = "𝔞".repeat(4000);
        let label = crate::project_label::project_label(&format!("/Users/dev/{absurd}"));
        let mut wordy = hint(9);
        wordy.project_label = label;
        let payload = ApnsPushSender::payload(&wordy);
        assert!(payload.len() < 4096, "{} bytes", payload.len());
    }

    #[test]
    fn many_blocked_sessions_coalesce_to_a_count() {
        let payload = ApnsPushSender::payload(&hint(4));
        let parsed: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");
        assert_eq!(parsed["aps"]["badge"], 4);
        let body = parsed["aps"]["alert"]["body"].as_str().unwrap_or_default();
        assert!(body.contains('4'), "the count is the useful fact: {body}");
    }
}
