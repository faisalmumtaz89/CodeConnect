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

    /// **Test-only.** The sender with both of its network-facing parts stubbed,
    /// so the fan-out `send` decides can be exercised on a machine with no Apple
    /// key and no trust store.
    ///
    /// The provider token is minted over a P-256 key generated in this process —
    /// a `.p8` in the repository would be exactly the secret [`ProviderToken`]
    /// exists to hold — and the TLS client trusts nothing at all. **Neither is
    /// ever used**: both belong to [`Self::deliver`], which nothing but a
    /// per-device worker reaches, and no test built on this constructor lets a
    /// worker run. Compiled out of every shipped binary, so no configuration
    /// file, environment variable or wire message can reach it.
    #[cfg(test)]
    fn without_a_network(registry: Arc<dyn PushRegistry>) -> Self {
        let rng = ring::rand::SystemRandom::new();
        let key = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .expect("a P-256 key");
        let token = ProviderToken::from_pkcs8(
            key.as_ref(),
            push_core::ApnsIdentity {
                key_id: "KEYIDABCDE".into(),
                team_id: "TEAMIDWXYZ".into(),
                topic: "com.example.codeconnect".into(),
            },
        )
        .expect("a provider token over a key this process just generated");
        Self {
            token: Arc::new(token),
            registry,
            tls: TlsConnector::from(Arc::new(
                tokio_rustls::rustls::ClientConfig::builder()
                    .with_root_certificates(tokio_rustls::rustls::RootCertStore::empty())
                    .with_no_client_auth(),
            )),
            queues: std::sync::Mutex::new(HashMap::new()),
        }
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
                target.credential.as_ref(),
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
            registry.correct_environment(
                &target.device_id,
                &target.token,
                target.credential.as_ref(),
                other,
            );
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
        for target in recipients(targets, excluded, &hint.agent) {
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
        //
        // **Deliberately not agent-filtered**, and it does not go through
        // `recipients` for exactly that reason: this push is about the transport,
        // not about any run. A Claude-only phone asking "does push work?" is
        // entitled to the answer.
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
        // **This run's epoch, resolved here.** A feature set confirmed by some
        // earlier process is not evidence about this one, and the read is what
        // turns that rule into the decoded value the fan-out sees.
        match self.store.push_targets(crate::state::feature_epoch()) {
            Ok(rows) => rows
                .into_iter()
                .map(|row| PushTarget {
                    environment: ApnsEnvironment::parse(&row.environment),
                    features: row.features,
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

    fn forget(
        &self,
        device_id: &str,
        refused_token: &str,
        refused_credential: Option<&protocol::secret::Redacted>,
        reason: &str,
    ) -> bool {
        match self.store.clear_push_token(
            device_id,
            refused_token,
            refused_credential.map(protocol::secret::Redacted::expose),
        ) {
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

    fn correct_environment(
        &self,
        device_id: &str,
        token: &str,
        credential: Option<&protocol::secret::Redacted>,
        environment: ApnsEnvironment,
    ) {
        crate::log_info!(
            "push: {device_id} is actually a {} build; remembering that",
            environment.as_str()
        );
        if let Err(err) = self.store.set_push_environment(
            device_id,
            token,
            credential.map(protocol::secret::Redacted::expose),
            environment.as_str(),
        ) {
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

    /// The agent every one of these fleets is about, named once so a test that is
    /// not about agent eligibility does not read as though it were.
    const CLAUDE: protocol::agent::AgentKind = protocol::agent::AgentKind::Claude;

    fn hint(blocked: usize) -> PushHint {
        PushHint {
            project_label: "Aion".into(),
            kind: crate::apns::PushKind::Approval,
            blocked_sessions: blocked,
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            agent: protocol::agent::AgentKind::Claude,
        }
    }

    /// A device that advertised nothing — the legacy shape, and Claude-only.
    fn target(device: &str) -> PushTarget {
        PushTarget {
            token: "aa".into(),
            environment: ApnsEnvironment::Sandbox,
            device_id: device.into(),
            credential: None,
            features: Default::default(),
        }
    }

    /// The fleet a test chose, so this sender's own `send` can run without a
    /// database behind it.
    ///
    /// **The two repair callbacks refuse rather than record.** They are reached
    /// only from [`ApnsPushSender::deliver`], which only a per-device worker
    /// calls — and the tests here deliberately never let one run. A recording
    /// stub would quietly accept a delivery path that had started dialling
    /// Apple; this says so instead.
    struct FakeRegistry(Vec<PushTarget>);

    impl PushRegistry for FakeRegistry {
        fn targets(&self) -> Vec<PushTarget> {
            self.0.clone()
        }

        fn forget(
            &self,
            _device_id: &str,
            _refused_token: &str,
            _refused_credential: Option<&protocol::secret::Redacted>,
            _reason: &str,
        ) -> bool {
            unreachable!("nothing here delivers, so nothing here can be refused")
        }

        fn correct_environment(
            &self,
            _device_id: &str,
            _token: &str,
            _credential: Option<&protocol::secret::Redacted>,
            _environment: ApnsEnvironment,
        ) {
            unreachable!("nothing here delivers, so no environment is corrected")
        }
    }

    #[test]
    fn a_device_that_already_saw_it_live_is_not_rung() {
        let fleet = || vec![target("phone-1"), target("phone-2"), target("phone-3")];
        let ids = |targets: Vec<PushTarget>| -> Vec<String> {
            targets.into_iter().map(|t| t.device_id).collect()
        };

        assert_eq!(
            ids(recipients(fleet(), &["phone-2".to_string()], &CLAUDE)),
            vec!["phone-1".to_string(), "phone-3".to_string()],
            "the one that saw it live is dropped and the others are kept"
        );
        assert_eq!(
            ids(recipients(fleet(), &[], &CLAUDE)),
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
                ],
                &CLAUDE
            )
            .is_empty(),
            "a fleet that has all seen it live is a push with nobody to send to"
        );
    }

    /// **A doorbell reaches only the phones that can open what it is about.**
    ///
    /// Claude is the floor and needs no advertisement — that is the shape every
    /// phone predating the field sends, and taking the floor away from them would
    /// silence the fleet. Every other agent has to have been named, so a Codex
    /// doorbell reaches exactly the devices that said the word and no others.
    ///
    /// The two filters compose rather than override: a Codex-capable phone that
    /// already saw the fact on its live socket is still not rung.
    ///
    /// **And the floor belongs to advertisements only.** A device whose stored set
    /// this run cannot read ([`crate::store::DeviceFeatures::Unconfirmable`]) is
    /// not a device that advertised nothing: it said something, and what it said
    /// cannot be established, so it hears nothing at all until it says it again.
    ///
    /// **And an unknown agent grants nothing even to the device that named it.**
    /// The fleet below therefore includes a phone advertising the very unknown
    /// value the last assertion asks about — the one shape that could possibly
    /// claim it, and the one the old assertion (which asked about an agent
    /// *nobody* had advertised) could not distinguish. `Vec::contains` matched
    /// `Unsupported("gemini")` against itself, so that phone was authorized for an
    /// agent this build cannot name; see [`protocol::ws::ClientFeatures::supports`].
    ///
    /// **Mutations:** decode `Unconfirmable` as the empty set and
    /// `unconfirmable-phone` joins the Claude fan-out; drop the `Unsupported` arm
    /// from `ClientFeatures::supports` and `gemini-phone` joins the gemini fan-out.
    #[test]
    fn only_a_device_that_advertised_the_agent_is_told_about_its_run() {
        let advertising = |device: &str, agents: Vec<protocol::agent::AgentKind>| PushTarget {
            features: crate::store::DeviceFeatures::Advertised(protocol::ws::ClientFeatures {
                agents,
            }),
            ..target(device)
        };
        let codex = protocol::agent::AgentKind::Codex;
        let fleet = || {
            vec![
                // The legacy shape: advertised nothing at all.
                target("silent-phone"),
                advertising(
                    "claude-only-phone",
                    vec![protocol::agent::AgentKind::Claude],
                ),
                advertising(
                    "codex-phone",
                    vec![protocol::agent::AgentKind::Claude, codex.clone()],
                ),
                // A row this run cannot read as the device's word — a set left by
                // an earlier daemon run, or bytes that will not decode. Not the
                // floor, and not silent-phone: this one SAID something, and what
                // it said cannot be established.
                PushTarget {
                    features: crate::store::DeviceFeatures::Unconfirmable,
                    ..target("unconfirmable-phone")
                },
                // A device that advertised an agent this build cannot name. It is
                // the only phone in the fleet that could possibly claim `gemini`,
                // which is what makes it the one worth asking about.
                advertising(
                    "gemini-phone",
                    vec![protocol::agent::AgentKind::Unsupported("gemini".into())],
                ),
            ]
        };
        let ids = |targets: Vec<PushTarget>| -> Vec<String> {
            targets.into_iter().map(|t| t.device_id).collect()
        };

        assert_eq!(
            ids(recipients(fleet(), &[], &codex)),
            vec!["codex-phone".to_string()],
            "a Codex doorbell is unactionable on a phone that cannot render one, and \
             a device that advertised nothing has not asked for it"
        );
        assert_eq!(
            ids(recipients(fleet(), &[], &CLAUDE)),
            vec![
                "silent-phone".to_string(),
                "claude-only-phone".to_string(),
                "codex-phone".to_string()
            ],
            "Claude is the floor: the silent phone gets it without ever having asked, \
             and the unconfirmable one does not — the floor is what an ADVERTISEMENT \
             grants, and a set this run cannot read is not one"
        );
        assert!(
            recipients(fleet(), &["codex-phone".to_string()], &codex).is_empty(),
            "eligible and already-seen compose: the one phone that could open it \
             already has it"
        );
        assert!(
            recipients(
                fleet(),
                &[],
                &protocol::agent::AgentKind::Unsupported("gemini".into())
            )
            .is_empty(),
            "an unknown agent reaches nobody — not even the phone that advertised \
             that very name. Agreeing about a name this build cannot act on is not \
             a capability, and it does not fall back to the floor either"
        );
        assert!(
            !ids(recipients(fleet(), &[], &CLAUDE)).contains(&"gemini-phone".to_string()),
            "and naming an unknown agent does not buy the floor: a non-empty set \
             grants exactly what it names, and this one named nothing this build knows"
        );
    }

    /// **The two halves of push authorization, composed once through the real
    /// parts.**
    ///
    /// Authorization is a projection and a filter, and every test above builds the
    /// projection's *output* by hand. [`StoreRegistry`] — the one thing that turns
    /// a `devices` row into a [`PushTarget`], and the only reader of this run's
    /// [`crate::state::feature_epoch`] on the push path — is constructed nowhere
    /// else in the suite, so its own mapping has never executed under test. A
    /// hand-built fleet cannot catch a projection that hands the filter the wrong
    /// features, because the hand-built fleet *is* the answer the projection was
    /// supposed to produce. This starts from four database rows and ends at the
    /// list a sender iterates, so a mistake anywhere between them is visible here.
    ///
    /// The fleet is the four shapes the column has, and each is a different reason
    /// a device may or may not be rung:
    ///
    /// - `silent` never wrote the column. That is the row every phone predating the
    ///   field has, and it reads as the Claude floor — Claude yes, Codex no.
    /// - `codex-only` advertised Codex and nothing else under this run's epoch, so
    ///   it hears Codex doorbells and is *not* handed Claude back: a non-empty set
    ///   grants exactly what it names.
    /// - `both` named the two, and is the only device in either fan-out twice.
    /// - `stale` wrote a perfectly well-formed Codex set — under some other
    ///   process's epoch. This run cannot vouch for it, so it hears nothing at all,
    ///   which is the one answer a hand-built `Unconfirmable` can state but only a
    ///   real row can *earn*.
    ///
    /// The epoch is `crate::state::feature_epoch()` rather than a fixture string
    /// because [`StoreRegistry::targets`] resolves it itself — it is not injectable
    /// — so a row written under anything else is by construction the `stale` case.
    ///
    /// **Mutations:** in [`crate::store::Store::push_targets`], decode the foreign
    /// epoch as `Advertised(Default::default())` and `stale` joins the Claude
    /// fan-out; in [`crate::push_queue::recipients`], filter on a hard-coded Claude
    /// instead of the hint's agent and the Codex fan-out becomes the Claude one.
    #[test]
    fn the_projection_and_the_filter_agree_about_which_devices_a_codex_run_may_ring() {
        // A database nobody else is using. The counter is load-bearing: tests run
        // in parallel threads and a millisecond timestamp alone lets two of them
        // open the same file.
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ccd-registry-fleet-{}-{}-{}.db",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(crate::store::Store::open(&path).unwrap());

        // Every device is registered for push, because `push_targets` filters on
        // the token: a phone with no token is absent for a reason that has nothing
        // to do with what it can render, and would blur the question being asked.
        for device in ["silent", "codex-only", "both", "stale"] {
            store
                .insert_device(
                    device,
                    // Device names are unique, so each phone needs its own.
                    &format!("iPhone ({device})"),
                    &format!("hash-{device}"),
                    "2026-08-01T00:00:00Z",
                )
                .unwrap();
            store
                .set_push_token(device, &format!("tok-{device}"), "production", None)
                .unwrap();
        }
        let epoch = crate::state::feature_epoch();
        store
            .set_device_features("codex-only", Some(r#"{"agents":["codex"]}"#), epoch)
            .unwrap();
        store
            .set_device_features("both", Some(r#"{"agents":["claude","codex"]}"#), epoch)
            .unwrap();
        // Well-formed, decodable, and stamped by somebody else. The bytes are not
        // what disqualifies it — the stamp is.
        store
            .set_device_features(
                "stale",
                Some(r#"{"agents":["codex"]}"#),
                "a-previous-daemon-run",
            )
            .unwrap();

        let registry = StoreRegistry::new(Arc::clone(&store));
        assert_eq!(
            registry.targets().len(),
            4,
            "the premise: all four rows are registered for push, so anything missing \
             below was refused for what it can render and not for lacking a token"
        );
        // Sorted, because `push_targets` has no ORDER BY: the set of devices is the
        // claim, and asserting an accidental row order would make this test fail for
        // a reason it is not about.
        let rung = |agent: &protocol::agent::AgentKind| -> Vec<String> {
            let mut ids: Vec<String> = recipients(registry.targets(), &[], agent)
                .into_iter()
                .map(|t| t.device_id)
                .collect();
            ids.sort();
            ids
        };

        assert_eq!(
            rung(&protocol::agent::AgentKind::Codex),
            vec!["both".to_string(), "codex-only".to_string()],
            "a Codex doorbell reaches exactly the phones whose own word, confirmed by \
             this run, says they can open one. `silent` failing here means the floor \
             leaked past Claude into an agent nobody advertised; `stale` failing here \
             means a set this process cannot vouch for was read as one it can"
        );
        assert_eq!(
            rung(&CLAUDE),
            vec!["both".to_string(), "silent".to_string()],
            "and Claude is the floor an advertisement grants, not a default the \
             projection hands out. `codex-only` appearing here means a non-empty set \
             stopped meaning exactly what it names; `stale` appearing here means an \
             unconfirmable row was decoded as the floor, which would hand a phone \
             doorbells its own claim never asked for"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The one fleet both fan-out tests below run over: every shape the feature
    /// column has an answer for, so each test's recipient list is a partition of
    /// the same four phones rather than of a fleet chosen to suit it.
    fn a_fleet_of_every_shape() -> Vec<PushTarget> {
        let advertising = |device: &str, agents: Vec<protocol::agent::AgentKind>| PushTarget {
            features: crate::store::DeviceFeatures::Advertised(protocol::ws::ClientFeatures {
                agents,
            }),
            ..target(device)
        };
        let codex = protocol::agent::AgentKind::Codex;
        vec![
            // The legacy shape: advertised nothing at all, which is the Claude
            // floor. It is what a widened Codex filter would sweep in, and what a
            // narrowed Claude filter would drop — and in this phase, where nothing
            // writes the column, it is the shape every real phone has.
            target("claude-floor-phone"),
            advertising("claude-only-phone", vec![CLAUDE]),
            advertising("codex-only-phone", vec![codex.clone()]),
            advertising("both-phone", vec![CLAUDE, codex]),
        ]
    }

    /// Which devices `send` filed a delivery for, sorted.
    ///
    /// **The queues are the observation, and they are downstream of the filter.**
    /// [`ApnsPushSender::enqueue`] is called once per surviving recipient and
    /// nothing else creates a queue, so on a sender this fresh the key set of
    /// `queues` *is* the list `send` iterated. Sorted, because a `HashMap`'s keys
    /// have no order and the set of devices is the claim.
    fn filed(sender: &ApnsPushSender) -> Vec<String> {
        let mut ids: Vec<String> = sender.queues.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// **The direct sender asks the hint's agent, and only a send can tell.**
    ///
    /// Every other assertion about agent eligibility here calls
    /// [`recipients`] itself, which proves what the filter answers and nothing
    /// about what the sender asks it. There are two independent senders, so a
    /// constant hard-coded at either call site is a fleet-wide authorization
    /// change that every direct test of the filter survives. This goes through
    /// [`ApnsPushSender`]'s real [`PushSender::send`] over a heterogeneous fleet,
    /// and its Codex-only recipients are the answer only the hint's own agent
    /// produces.
    ///
    /// **The hint is the shape production can actually raise, which round-8 found
    /// it was not.** It carried an `Approval` with two runs blocked, and that hint
    /// cannot exist with `agent: Codex`: the only production caller of `send` is
    /// `Daemon::dispatch_push`, which passes a [`crate::apns::PushHint::describing`]
    /// result, and `describing` forces the subject to Claude whenever `blocked > 0`
    /// — a Codex approval deck cannot be raised in this build, the Codex approval
    /// contracts being Phase 3. So `blocked` is zero on every Codex-carrying hint,
    /// and the one construction site that sets `agent: Codex`
    /// (`Daemon::push_codex_turn_complete`) sets `kind: Completed`. Asserting the
    /// fan-out over a hint nothing can raise proves the filter is reachable, not
    /// that the reachable path is filtered.
    ///
    /// **Nothing is awaited between the send and the read, and that is
    /// load-bearing.** `enqueue` starts a worker for a device it has not seen
    /// before, and a worker's first act is to dial `api.push.apple.com`. A task
    /// spawned on the current-thread runtime this test runs under is not polled
    /// until the test yields, so reading the queues without yielding observes the
    /// whole fan-out and opens no socket.
    ///
    /// **Mutations:** filter on a hard-coded `AgentKind::Claude` at this sender's
    /// call to [`recipients`], or drop the agent filter from [`recipients`]
    /// altogether, and the two phones that never named Codex join the fan-out.
    #[tokio::test]
    async fn a_codex_doorbell_is_filed_only_for_the_phones_that_advertised_codex() {
        let sender =
            ApnsPushSender::without_a_network(Arc::new(FakeRegistry(a_fleet_of_every_shape())));

        sender.send(
            &PushHint {
                agent: protocol::agent::AgentKind::Codex,
                // The production shape, and the only one: a finished Codex turn,
                // nothing blocked. See the note above.
                kind: crate::apns::PushKind::Completed,
                ..hint(0)
            },
            &[],
        );

        assert_eq!(
            filed(&sender),
            vec!["both-phone".to_string(), "codex-only-phone".to_string()],
            "a Codex doorbell is filed for exactly the phones that said the word. \
             Either Claude phone appearing here means the sender asked about an \
             agent the run is not, and told a device about a session it has no \
             screen to open"
        );
    }

    /// **And the same seam in the direction that carries the traffic: a Claude
    /// doorbell reaches the ordinary fleet.**
    ///
    /// Round-8 found the test above is one-sided, and a one-sided test of a filter
    /// masks the failure that costs more. Hard-code `&AgentKind::Codex` in place of
    /// `&hint.agent` at this sender's call to [`recipients`] and that test passes
    /// unchanged — while every ordinary Claude push is authorized to Codex-capable
    /// phones only. Nothing writes the features column in this phase, so every real
    /// row is the `NULL` Claude floor: the whole fleet stops ringing, silently,
    /// because a suppression produces no error anywhere — it produces a phone that
    /// does not buzz, which nothing but an assertion about *inclusion* can see.
    ///
    /// The two phones that make the difference are `claude-floor-phone`, the shape
    /// every pre-Codex device sends, and `claude-only-phone`, which named Claude
    /// and nothing else. Both are refused by a Codex subject, and both must have a
    /// Claude one.
    ///
    /// The hint is `hint(2)` — an `Approval` speaking for two blocked runs, which
    /// is exactly what `describing` hands the sender when a deck is open, since the
    /// deck is Claude's in this build.
    ///
    /// Same no-network property as the test above: nothing is awaited between the
    /// send and the read, so no worker is ever polled and no socket is opened.
    ///
    /// **Mutation:** hard-code `&protocol::agent::AgentKind::Codex` in place of
    /// `&hint.agent` at [`ApnsPushSender::send`]'s call to [`recipients`]. This
    /// goes red on the two Claude-only phones;
    /// `a_codex_doorbell_is_filed_only_for_the_phones_that_advertised_codex` stays
    /// green.
    #[tokio::test]
    async fn a_claude_doorbell_is_filed_for_the_legacy_fleet_and_not_only_the_codex_capable() {
        let sender =
            ApnsPushSender::without_a_network(Arc::new(FakeRegistry(a_fleet_of_every_shape())));

        sender.send(&hint(2), &[]);

        assert_eq!(
            filed(&sender),
            vec![
                "both-phone".to_string(),
                "claude-floor-phone".to_string(),
                "claude-only-phone".to_string()
            ],
            "a Claude doorbell is filed for every phone that can open a Claude run — \
             the floor included, which is every phone in the field today. A missing \
             one means the sender asked about an agent the run is not, and a device \
             that could have answered was never told"
        );
        assert!(
            !filed(&sender).contains(&"codex-only-phone".to_string()),
            "and a non-empty advertisement still grants exactly what it names: a \
             phone that said Codex and only Codex is not handed the floor back"
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
                        agent: protocol::agent::AgentKind::Claude,
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
