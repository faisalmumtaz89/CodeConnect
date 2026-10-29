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
                "badge": hint.decisions,
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
        let gate = Arc::clone(&registry);
        Self::serve_attempting(queue, gate, move |target, payload, collapse, recheck| {
            let tls = tls.clone();
            let token = Arc::clone(&token);
            let registry = Arc::clone(&registry);
            async move {
                match tokio::time::timeout(
                    DELIVERY_DEADLINE,
                    Self::deliver(tls, token, registry, target, payload, collapse, recheck),
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

    /// The worker loop this sender runs, with its transport handed in.
    ///
    /// Separated from [`ApnsPushSender::serve`] so the loop's own behaviour can
    /// be driven without a network. What it holds that the generic loop cannot is
    /// the WIRING: which registry this sender re-reads authorization from, and
    /// that it re-reads one at all. A sender that passed no gate would still
    /// deliver every push in order, so nothing about ordering can catch it.
    async fn serve_attempting<A, Fut>(
        queue: Arc<DeviceQueue>,
        registry: Arc<dyn PushRegistry>,
        attempt: A,
    ) where
        A: Fn(PushTarget, String, &'static str, bool) -> Fut,
        Fut: std::future::Future<Output = Result<Option<String>>>,
    {
        serve_with(queue, attempt, move |device_id| {
            let registry = Arc::clone(&registry);
            async move { crate::push_queue::still_authorized(&*registry, &device_id).await }
        })
        .await
    }

    /// One POST to Apple, and nothing decided about it.
    ///
    /// **The network half, separated from the policy half.** What to do about a
    /// refusal — retire the device, try the other host, refuse — is
    /// [`ApnsPushSender::deliver_with`]'s subject, and it is the half with the
    /// interesting rules: a retry is a SECOND post, so it has to be authorized a
    /// second time. Those rules can only be driven if the thing they decide about
    /// can be faked, and a function that dials Apple cannot be. This is the seam,
    /// on exactly the same terms as [`ApnsPushSender::serve_attempting`].
    async fn post(
        tls: TlsConnector,
        token: Arc<ProviderToken>,
        target: PushTarget,
        payload: String,
        collapse: &'static str,
    ) -> Result<Posted> {
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
        body.send_data(payload.into(), true)
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
            return Ok(Posted::Accepted(apns_id));
        }

        let mut reason = String::new();
        let mut stream = response.into_body();
        while let Some(chunk) = stream.data().await {
            if let Ok(bytes) = chunk {
                reason.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        Ok(Posted::Refused { status, reason })
    }

    async fn deliver(
        tls: TlsConnector,
        token: Arc<ProviderToken>,
        registry: Arc<dyn PushRegistry>,
        target: PushTarget,
        payload: String,
        collapse: &'static str,
        recheck: bool,
    ) -> Result<Option<String>> {
        Self::deliver_with(
            registry,
            target,
            payload,
            collapse,
            recheck,
            move |target, payload, collapse| {
                Self::post(tls.clone(), Arc::clone(&token), target, payload, collapse)
            },
        )
        .await
    }

    /// What a refusal means, over a transport handed in.
    ///
    /// **Two posts at most, and the second is a decision.** `BadDeviceToken`
    /// usually means the right token on the wrong host — a build's APNs world is
    /// decided by how it was *signed*, and the app has to infer that, so an App
    /// Store build with no provisioning profile to read can get it wrong. Rather
    /// than leave push silently dead until someone reads a log, the other host is
    /// tried once and the answer remembered. Measured: a TestFlight install
    /// reported `sandbox`, held a production token, and every notification
    /// vanished.
    ///
    /// **Written as two steps rather than a guarded recursion**, because the
    /// second step is not the first one again. It asks the authorization question
    /// the loop asked before the first post, and the recursion this replaced could
    /// not: it re-entered below [`serve_with`], so the second post to a phone that
    /// was withdrawn while the first host was answering went out anyway.
    async fn deliver_with<P, Fut>(
        registry: Arc<dyn PushRegistry>,
        target: PushTarget,
        payload: String,
        collapse: &'static str,
        recheck: bool,
        post: P,
    ) -> Result<Option<String>>
    where
        P: Fn(PushTarget, String, &'static str) -> Fut,
        Fut: std::future::Future<Output = Result<Posted>>,
    {
        // **Only a refusal about the token in use retires the device.** A late
        // `410` for a token the phone has already replaced says nothing about the
        // one it is using now, and treating it as a departure would close a queue
        // holding that token's work.
        let retire = |status: http::StatusCode, reason: &str| {
            let was_current = registry.forget(
                &target.device_id,
                &target.token,
                target.credential.as_ref(),
                &format!("{status}: {reason}"),
            );
            terminal_refusal(status.as_u16(), reason, was_current)
        };

        let refusal = match post(target.clone(), payload.clone(), collapse).await? {
            Posted::Accepted(apns_id) => return Ok(apns_id),
            Posted::Refused { status, reason } => (status, reason),
        };
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
        let (status, reason) = refusal;
        if is_terminal(status.as_u16(), &reason) {
            return Err(retire(status, &reason));
        }
        if !reason.contains("BadDeviceToken") {
            bail!("APNs refused the push: {status} {reason}");
        }

        let other = match target.environment {
            ApnsEnvironment::Sandbox => ApnsEnvironment::Production,
            ApnsEnvironment::Production => ApnsEnvironment::Sandbox,
        };
        // **The retry is a second post, so it is a second authorization.** The
        // first host's silence is unbounded in principle and slow in practice — a
        // TLS handshake, a request, a response body — and a withdrawal landing in
        // that window is exactly the case the dequeue check exists for. Asking
        // again here is the same question [`serve_with`] asks, asked at the only
        // other moment this daemon is about to hand a notification to Apple.
        if recheck && !crate::push_queue::still_authorized(&*registry, &target.device_id).await {
            bail!(
                "APNs refused the push on {}, and {} stopped being registered while that \
                 answer was on its way; the other host is not tried",
                target.environment.as_str(),
                target.device_id
            );
        }
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
        let apns_id = match post(corrected, payload, collapse).await? {
            Posted::Accepted(apns_id) => apns_id,
            // The other host refused it too. A `410` there is still a departure;
            // anything else — a second `BadDeviceToken` included — is the honest
            // end of the road, because there is no third host to try.
            Posted::Refused { status, reason } if is_terminal(status.as_u16(), &reason) => {
                return Err(retire(status, &reason))
            }
            Posted::Refused { status, reason } => {
                bail!("APNs refused the push: {status} {reason}")
            }
        };
        registry.correct_environment(
            &target.device_id,
            &target.token,
            target.credential.as_ref(),
            other,
        );
        Ok(apns_id)
    }
}

/// What one POST to Apple came back with, before anything is decided about it.
///
/// A refusal is a value here rather than an `Err`, because the interesting ones
/// are not failures of the post: a `410` is a fact about the device and a
/// `BadDeviceToken` is usually a fact about the host. An `Err` from
/// [`ApnsPushSender::post`] is the post itself failing — a connect, a handshake,
/// a socket — which no policy can improve on.
enum Posted {
    /// Apple took it, with the receipt it sent, when it sent one.
    Accepted(Option<String>),
    /// Apple answered and said no, with the status and the reason body it gave.
    Refused {
        status: http::StatusCode,
        reason: String,
    },
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
                // The registration the fan-out above just read, asked again by
                // the worker immediately before it posts.
                recheck: true,
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
        // It does not go through `recipients`: this push is about the transport,
        // not about any run, and no live socket makes it redundant.
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
            // Not about any run, so there is nothing to re-check: a phone asking
            // "does push work?" is entitled to the answer.
            recheck: false,
        });
        rx
    }
}

/// The database as a push registry.
///
/// **Two answers, two boundaries, and the difference is which caller has a
/// runtime.** The fleet list is synchronous on purpose: `PushSender::send` is
/// sync, and the alternative — blocking on the async pool from inside it —
/// would put database latency on the path that publishes an event. The
/// per-device re-check has an `async` caller (a worker loop, immediately before
/// a network round trip), so it goes through [`crate::db`] and off the runtime,
/// where a read that misses the page cache costs a blocking thread rather than a
/// Tokio worker.
pub struct StoreRegistry {
    store: Arc<crate::store::Store>,
    /// The same store across the blocking-pool boundary, for the one question a
    /// worker asks while it is on the runtime.
    db: crate::db::Db,
}

impl StoreRegistry {
    pub fn new(store: Arc<crate::store::Store>) -> Self {
        Self {
            db: crate::db::Db::new(Arc::clone(&store)),
            store,
        }
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

    fn eligible<'a>(
        &'a self,
        device_id: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match self.db.push_target_for(device_id.clone()).await {
                Ok(Some(_)) => true,
                // No row to offer: the device is revoked, has no token, or is
                // gone. Nothing to vouch for, so nothing is vouched for.
                Ok(None) => false,
                Err(err) => {
                    // Fail closed and say so. A database error is not "the phone
                    // is fine"; posting on an unreadable row would be a push
                    // authorized by a question nobody answered.
                    crate::log_error!(
                        "push: could not re-read the registration for {device_id}: {err:#}; \
                         the delivery waiting on it is not posted"
                    );
                    false
                }
            }
        })
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

    fn hint(blocked: usize) -> PushHint {
        PushHint {
            project_label: "Aion".into(),
            kind: crate::apns::PushKind::Approval,
            decisions: blocked,
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
        }
    }

    /// A registered device.
    fn target(device: &str) -> PushTarget {
        PushTarget {
            token: "aa".into(),
            environment: ApnsEnvironment::Sandbox,
            device_id: device.into(),
            credential: None,
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

        /// The per-device answer, read off the same fleet this fake was built
        /// from — one device, not the list, because that is the question.
        fn eligible<'a>(
            &'a self,
            device_id: String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
            let answer = self.0.iter().any(|target| target.device_id == device_id);
            Box::pin(std::future::ready(answer))
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
                ],
            )
            .is_empty(),
            "a fleet that has all seen it live is a push with nobody to send to"
        );
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

    /// How many deliveries are waiting for one device — `0` when the fan-out
    /// never reached it, which is a queue that was never created.
    ///
    /// [`filed`] answers *which* devices, and a per-device COUNT is a different
    /// claim: a doorbell filed twice coalesces to one waiting delivery, so a
    /// reader that could see only the key set would read a duplicate and the
    /// coalescing that prevents it identically.
    fn queued(sender: &ApnsPushSender, device: &str) -> usize {
        sender
            .queues
            .lock()
            .unwrap()
            .get(device)
            .map_or(0, |queue| queue.depth())
    }

    /// **The same approval, rung again, does not queue a second time.**
    ///
    /// A reconnecting phone replays from an older sequence and a re-delivered
    /// request is filed again, so "the same doorbell twice" is an ordinary event
    /// rather than a fault. Two layers stop it becoming two buzzes, and this is
    /// the lower one: the per-device queue holds at most one doorbell, and a
    /// newer one replaces the one waiting instead of joining it. (The upper
    /// layer, which stops a second doorbell being dispatched at all, is
    /// `state.rs`\'s ring-once assertion over a re-delivery.)
    ///
    /// **Mutation:** delete the doorbell-replacement arm in
    /// [`super::super::push_queue::DeviceQueue::push`] and each phone holds
    /// three.
    #[tokio::test]
    async fn the_same_approval_doorbell_filed_again_does_not_queue_a_second_time() {
        let sender = ApnsPushSender::without_a_network(Arc::new(FakeRegistry(vec![
            target("phone-1"),
            target("phone-2"),
        ])));

        for _ in 0..3 {
            sender.send(&hint(1), &[]);
        }

        for (device, expected) in [("phone-1", 1), ("phone-2", 1)] {
            assert_eq!(
                queued(&sender, device),
                expected,
                "{device} after three sends of one approval\'s doorbell"
            );
        }
    }

    /// **The registry is read at the fan-out, so a device that leaves it between
    /// one doorbell and the next is not rung.**
    ///
    /// The recipient list is worth nothing if it is computed once and reused: a
    /// phone that was revoked or lost its token would keep hearing about runs.
    /// The registry here answers differently on its second call, which is the
    /// only way to tell a list read afresh from one captured earlier.
    ///
    /// Two senders, because one sender\'s queue survives its own first send and
    /// a second send into it would read `1` whether it was refused or coalesced
    /// — the number this test needs is the number of devices the second fan-out
    /// reached, and an empty queue map is the only unambiguous form of it.
    ///
    /// **Mutation:** hoist the `registry.targets()` call in
    /// [`ApnsPushSender::send`] into a value computed once at construction and
    /// the second fan-out rings the withdrawn phone.
    #[tokio::test]
    async fn a_device_that_leaves_the_registry_between_doorbells_is_not_rung_again() {
        /// Registered the first time it is asked and gone the second.
        struct Withdrawing(std::sync::Mutex<usize>);
        impl PushRegistry for Withdrawing {
            fn eligible<'a>(
                &'a self,
                _device_id: String,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>
            {
                // This test is about the FAN-OUT, and it never awaits, so no
                // worker is ever polled. Answering here would also disturb the
                // call count below, which is the whole instrument.
                unreachable!("no worker runs in this test, so nothing re-checks")
            }
            fn targets(&self) -> Vec<PushTarget> {
                let mut calls = self.0.lock().unwrap();
                *calls += 1;
                if *calls == 1 {
                    vec![target("withdrawing-phone")]
                } else {
                    Vec::new()
                }
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

        let registry = Arc::new(Withdrawing(std::sync::Mutex::new(0)));
        let sender =
            ApnsPushSender::without_a_network(Arc::clone(&registry) as Arc<dyn PushRegistry>);

        sender.send(&hint(1), &[]);
        assert_eq!(
            filed(&sender),
            vec!["withdrawing-phone".to_string()],
            "the premise: while it was registered it was rung"
        );

        // **The queue is retired between the two, so the second fan-out is
        // observable.** A device's queue outlives its first delivery, and a
        // second doorbell into a queue that already holds one COALESCES — so
        // the depth reads `1` whether the second fan-out refused the phone or
        // merely replaced what was waiting, and the two are the opposite
        // findings. With no queue in the map, "was one created" is the answer,
        // and it is unambiguous.
        sender.retire("withdrawing-phone");
        assert!(filed(&sender).is_empty(), "the retirement took the queue");

        sender.send(&hint(1), &[]);
        assert!(
            filed(&sender).is_empty(),
            "the doorbell after the withdrawal reached {:?}; the registry must be \
             read at the fan-out, not captured before it",
            filed(&sender)
        );
        assert_eq!(queued(&sender, "withdrawing-phone"), 0);
    }

    /// **And the registration is read again at the DEQUEUE, because the
    /// fan-out's answer can go stale before the push leaves.**
    ///
    /// A device's deliveries are served one at a time and each attempt is
    /// awaited whole, so a doorbell filed while a phone was eligible can wait
    /// behind an in-flight attempt for as long as that attempt takes — a slow
    /// network, a retry — and the world can move in that window. Re-reading only
    /// at the fan-out would post it anyway.
    ///
    /// **Two halves, because they are two files apart.** First: the fan-out files
    /// the re-check flag beside the delivery, which is what makes a second check
    /// happen at all — a sender that filed none would pass every ordering test
    /// and simply never re-check anything. Second: this sender's own worker loop
    /// asks again, and the order is the claim — the worker is BLOCKED inside its
    /// first attempt, the second doorbell is filed while the phone is still
    /// registered, the registration is withdrawn, and only then is the first
    /// attempt released.
    ///
    /// **The queue is driven directly rather than through `send`, and that is
    /// load-bearing.** `send` starts this sender's own worker, which would then
    /// race the one below for the same deliveries — and a race whose winner
    /// decides the count is a test that passes for whichever reason it likes.
    /// (It did: the first draft of this test survived its own named mutation.)
    /// The fan-out half above is what covers `send`; this half covers the loop.
    ///
    /// **Mutation:** drop the gate argument from
    /// [`ApnsPushSender::serve_attempting`] (pass one that always answers `true`)
    /// and the withdrawn doorbell is posted; stop setting `recheck` at the
    /// fan-out and the first half goes red.
    #[tokio::test]
    async fn a_doorbell_that_waited_out_a_withdrawal_is_not_posted() {
        /// Registered until a test says otherwise.
        struct Switchable(std::sync::atomic::AtomicBool);
        impl Switchable {
            /// Whether this phone is registered right now — the one fact both
            /// answers are built from, so the fan-out list and the per-device
            /// re-check cannot disagree about the switch.
            fn registered(&self) -> bool {
                self.0.load(std::sync::atomic::Ordering::SeqCst)
            }
        }
        impl PushRegistry for Switchable {
            fn targets(&self) -> Vec<PushTarget> {
                if self.registered() {
                    vec![target("waiting-phone")]
                } else {
                    Vec::new()
                }
            }
            fn eligible<'a>(
                &'a self,
                device_id: String,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>
            {
                let answer = device_id == "waiting-phone" && self.registered();
                Box::pin(std::future::ready(answer))
            }
            fn forget(
                &self,
                _device_id: &str,
                _refused_token: &str,
                _refused_credential: Option<&protocol::secret::Redacted>,
                _reason: &str,
            ) -> bool {
                unreachable!("the attempt below is a fake; nothing is refused")
            }
            fn correct_environment(
                &self,
                _device_id: &str,
                _token: &str,
                _credential: Option<&protocol::secret::Redacted>,
                _environment: ApnsEnvironment,
            ) {
                unreachable!("the attempt below is a fake; no environment is corrected")
            }
        }

        let registry = Arc::new(Switchable(std::sync::atomic::AtomicBool::new(true)));

        // Half one: the fan-out files the re-check flag beside the delivery.
        {
            let sender =
                ApnsPushSender::without_a_network(Arc::clone(&registry) as Arc<dyn PushRegistry>);
            sender.send(&hint(1), &[]);
            let queues = sender.queues.lock().unwrap();
            let queue = queues.get("waiting-phone").expect("the queue was created");
            assert_eq!(
                queue.waiting_rechecks(),
                vec![true],
                "the fan-out must file the re-check, or nothing re-checks it"
            );
        }

        // Half two: this sender's worker loop, over a queue nothing else serves.
        let mut queues = HashMap::new();
        let (queue, _) = queue_for(&mut queues, "waiting-phone");
        let doorbell = || Delivery {
            target: target("waiting-phone"),
            payload: "{}".into(),
            collapse: COLLAPSE_ID,
            respond: None,
            recheck: true,
        };
        queue.push(doorbell(), "waiting-phone");

        let posted = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let release = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let worker = tokio::spawn({
            let (queue, posted, release, started, registry) = (
                Arc::clone(&queue),
                Arc::clone(&posted),
                Arc::clone(&release),
                Arc::clone(&started),
                Arc::clone(&registry) as Arc<dyn PushRegistry>,
            );
            async move {
                ApnsPushSender::serve_attempting(
                    queue,
                    registry,
                    move |target, _payload, _c, _recheck| {
                        let (posted, release, started) = (
                            Arc::clone(&posted),
                            Arc::clone(&release),
                            Arc::clone(&started),
                        );
                        async move {
                            posted.lock().unwrap().push(target.device_id.clone());
                            started.notify_one();
                            release.notified().await;
                            Ok(None)
                        }
                    },
                )
                .await
            }
        });

        // The worker is now inside its first attempt and cannot get to a second.
        started.notified().await;
        assert_eq!(posted.lock().unwrap().len(), 1, "the premise");

        // A second doorbell, filed while the phone is STILL registered: this is
        // the fan-out's answer, which the dequeue check is there to outlive.
        queue.push(doorbell(), "waiting-phone");
        assert_eq!(queue.depth(), 1, "it is waiting its turn");

        // The world moves while it waits.
        registry.0.store(false, std::sync::atomic::Ordering::SeqCst);

        release.notify_waiters();
        for _ in 0..400 {
            if queue.depth() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            posted.lock().unwrap().as_slice(),
            ["waiting-phone".to_string()],
            "the doorbell that waited out the withdrawal must not be posted; \
             the registration is read at the dequeue, not only at the fan-out"
        );
        worker.abort();
    }

    /// **The other-host retry is a SECOND post, and it is authorized like one.**
    ///
    /// `BadDeviceToken` sends this sender at the other Apple host once, and that
    /// retry hands a notification to Apple exactly as the first attempt did — but
    /// it happens *inside* one turn of the worker loop, so the dequeue check the
    /// loop makes cannot cover it. The window is not theoretical: the first host's
    /// answer costs a connect, a TLS handshake, a request and a response body, and
    /// a phone that is revoked or loses its token in that time is precisely the
    /// case the dequeue check exists for.
    ///
    /// The withdrawal is staged *by the first post itself*, so the ordering is not
    /// a matter of timing: the answer that triggers the retry is the same event
    /// that revokes the authorization for it.
    ///
    /// **Driven over the post seam, because the alternative is dialling Apple.**
    /// [`ApnsPushSender::deliver_with`] is the policy — retire, retry, refuse —
    /// with the transport handed in; [`ApnsPushSender::post`] is the transport.
    /// The refusal a real APNs `400` produces is reproduced by value, so what is
    /// asserted is what the policy DOES about it.
    ///
    /// **Mutation:** drop the `still_authorized` check in
    /// [`ApnsPushSender::deliver_with`] and the withdrawn phone is posted to on
    /// the other host. The other half of the wiring — that a delivery's re-check
    /// flag reaches the transport at all, without which this check would be
    /// handed `false` and ask nothing for ever — is one file up and asserted
    /// there, in `push_queue`'s `the_attempt_is_handed_the_recheck_the_delivery_carries`.
    #[tokio::test]
    async fn the_other_host_retry_is_not_posted_to_a_phone_that_was_withdrawn() {
        /// Registered until something turns it off, and records repairs.
        struct Retrying {
            registered: std::sync::atomic::AtomicBool,
            corrected: std::sync::Mutex<Vec<ApnsEnvironment>>,
        }
        impl PushRegistry for Retrying {
            fn targets(&self) -> Vec<PushTarget> {
                unreachable!("the dequeue check is per device; nothing here asks for a fleet")
            }
            fn eligible<'a>(
                &'a self,
                _device_id: String,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>
            {
                let answer = self.registered.load(std::sync::atomic::Ordering::SeqCst);
                Box::pin(std::future::ready(answer))
            }
            fn forget(
                &self,
                _device_id: &str,
                _refused_token: &str,
                _refused_credential: Option<&protocol::secret::Redacted>,
                _reason: &str,
            ) -> bool {
                unreachable!("a 400 is not a departure; nothing here is retired")
            }
            fn correct_environment(
                &self,
                _device_id: &str,
                _token: &str,
                _credential: Option<&protocol::secret::Redacted>,
                environment: ApnsEnvironment,
            ) {
                self.corrected.lock().unwrap().push(environment);
            }
        }

        /// The refusal a real APNs `400` for the wrong host produces.
        fn wrong_host() -> Posted {
            Posted::Refused {
                status: http::StatusCode::BAD_REQUEST,
                reason: r#"{"reason":"BadDeviceToken"}"#.to_string(),
            }
        }

        // Runs the two-post policy over a transport that refuses the first host,
        // reporting every environment it was actually asked to post at.
        let run = |registry: Arc<Retrying>, withdraw_on_first_answer: bool| async move {
            let posted = Arc::new(std::sync::Mutex::new(Vec::<ApnsEnvironment>::new()));
            let outcome = ApnsPushSender::deliver_with(
                Arc::clone(&registry) as Arc<dyn PushRegistry>,
                target("retrying-phone"),
                "{}".into(),
                COLLAPSE_ID,
                true,
                {
                    let (posted, registry) = (Arc::clone(&posted), Arc::clone(&registry));
                    move |target: PushTarget, _payload, _collapse| {
                        let (posted, registry) = (Arc::clone(&posted), Arc::clone(&registry));
                        async move {
                            let mut seen = posted.lock().unwrap();
                            seen.push(target.environment);
                            if seen.len() == 1 {
                                // **The world moves while the first host is
                                // answering** — and it is this very answer that
                                // moves it, so no sleep decides the order.
                                if withdraw_on_first_answer {
                                    registry
                                        .registered
                                        .store(false, std::sync::atomic::Ordering::SeqCst);
                                }
                                return Ok(wrong_host());
                            }
                            Ok(Posted::Accepted(Some("apns-id-from-the-other-host".into())))
                        }
                    }
                },
            )
            .await;
            let seen = posted.lock().unwrap().clone();
            (outcome, seen)
        };

        // The premise: while the phone is still a recipient, the retry is exactly
        // the repair it is there to be — the other host is tried and remembered.
        let still_here = Arc::new(Retrying {
            registered: std::sync::atomic::AtomicBool::new(true),
            corrected: std::sync::Mutex::new(Vec::new()),
        });
        let (outcome, posted) = run(Arc::clone(&still_here), false).await;
        assert_eq!(
            posted,
            vec![ApnsEnvironment::Sandbox, ApnsEnvironment::Production],
            "the premise: a BadDeviceToken sends an authorized push at the other host"
        );
        assert_eq!(
            outcome.unwrap().as_deref(),
            Some("apns-id-from-the-other-host")
        );
        assert_eq!(
            still_here.corrected.lock().unwrap().as_slice(),
            [ApnsEnvironment::Production],
            "and the correction is remembered, so the next push goes straight there"
        );

        // The finding: withdrawn while the first host was answering.
        let withdrawn = Arc::new(Retrying {
            registered: std::sync::atomic::AtomicBool::new(true),
            corrected: std::sync::Mutex::new(Vec::new()),
        });
        let (outcome, posted) = run(Arc::clone(&withdrawn), true).await;
        assert_eq!(
            posted,
            vec![ApnsEnvironment::Sandbox],
            "the phone stopped being registered while the first host was answering; \
             the retry is a second post to Apple and must be authorized like one"
        );
        assert!(
            outcome.is_err(),
            "and the delivery ends honestly rather than reporting a push it did not make"
        );
        assert!(
            withdrawn.corrected.lock().unwrap().is_empty(),
            "nothing was posted at the other host, so there is no environment to correct"
        );
    }

    /// **The dequeue check reads ONE DEVICE, and it does not scan the fleet.**
    ///
    /// The check runs once per delivery attempt, on the runtime, for a question
    /// about a single device id. Answering it by listing every registered phone
    /// makes the cost of one push proportional to the size of the fleet, and the
    /// store-backed registry maps that list to a whole-table `SELECT` — so a
    /// handful of devices with work in flight would put a database read for every
    /// phone in the fleet in front of every attempt, on the pool the daemon
    /// documents as the reason the store is never called from a runtime worker.
    ///
    /// The shape is the assertion, because the cost is invisible in any test of
    /// the outcome: a fleet scan and a per-device read AGREE on who is authorized,
    /// and differ only in what they made the database do to say so. Counted here,
    /// so a later edit that reaches for the convenient list is caught by the count
    /// rather than by a profile taken after it shipped.
    ///
    /// **Mutation:** answer [`crate::push_queue::still_authorized`] out of
    /// `PushRegistry::targets` again — find the device in the fleet list — and the
    /// fleet count reads `1`.
    #[tokio::test]
    async fn the_dequeue_check_reads_one_device_and_never_the_fleet() {
        /// Counts what it was asked for, separately.
        struct Counting {
            fleet_reads: std::sync::atomic::AtomicUsize,
            device_reads: std::sync::Mutex<Vec<String>>,
        }
        impl PushRegistry for Counting {
            fn targets(&self) -> Vec<PushTarget> {
                self.fleet_reads
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                vec![target("counted-phone")]
            }
            fn eligible<'a>(
                &'a self,
                device_id: String,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>
            {
                self.device_reads.lock().unwrap().push(device_id);
                Box::pin(std::future::ready(true))
            }
            fn forget(
                &self,
                _device_id: &str,
                _refused_token: &str,
                _refused_credential: Option<&protocol::secret::Redacted>,
                _reason: &str,
            ) -> bool {
                unreachable!("the attempt below is a fake; nothing is refused")
            }
            fn correct_environment(
                &self,
                _device_id: &str,
                _token: &str,
                _credential: Option<&protocol::secret::Redacted>,
                _environment: ApnsEnvironment,
            ) {
                unreachable!("the attempt below is a fake; no environment is corrected")
            }
        }

        let registry = Arc::new(Counting {
            fleet_reads: std::sync::atomic::AtomicUsize::new(0),
            device_reads: std::sync::Mutex::new(Vec::new()),
        });

        // The target is built here rather than asked of the registry, so the
        // counters read the WORKER's questions and nothing else.
        let mut queues = HashMap::new();
        let (queue, _) = queue_for(&mut queues, "counted-phone");
        queue.push(
            Delivery {
                target: target("counted-phone"),
                payload: "{}".into(),
                collapse: COLLAPSE_ID,
                respond: None,
                recheck: true,
            },
            "counted-phone",
        );

        let posted = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let worker = tokio::spawn({
            let (queue, posted, gate) = (
                Arc::clone(&queue),
                Arc::clone(&posted),
                Arc::clone(&registry) as Arc<dyn PushRegistry>,
            );
            async move {
                ApnsPushSender::serve_attempting(
                    queue,
                    gate,
                    move |target, _payload, _c, _recheck| {
                        let posted = Arc::clone(&posted);
                        async move {
                            posted.lock().unwrap().push(target.device_id.clone());
                            Ok(None)
                        }
                    },
                )
                .await
            }
        });

        for _ in 0..400 {
            if !posted.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            posted.lock().unwrap().as_slice(),
            ["counted-phone".to_string()],
            "the premise: the delivery was re-checked and posted"
        );
        assert_eq!(
            registry
                .fleet_reads
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the dequeue check asked for the whole fleet; it has one device id \
             and must ask about that device"
        );
        assert_eq!(
            registry.device_reads.lock().unwrap().as_slice(),
            ["counted-phone".to_string()],
            "and it asked exactly once, about the device it was about to post to"
        );
        worker.abort();
    }

    /// **Every paired phone is filed a doorbell, and a device that leaves the
    /// fleet takes its queued doorbell with it.**
    ///
    /// The device rows are the real ones, through the real registry: each phone
    /// is paired and registered exactly as a shipping app does it — a device row,
    /// then a token — and nothing else. A doorbell carries no agent, so an
    /// approval and a finished turn are filed for every one of them whichever
    /// agent raised it. The relayed half of the same claim is `relay_sender`'s
    /// `a_paired_phone_is_rung_for_an_approval_and_a_finished_turn`.
    ///
    /// Each kind gets a fresh sender, because a second doorbell into a queue that
    /// already holds one coalesces and would read `1` whether or not it was filed.
    /// Nothing is awaited between a send and its read, so no worker is polled and
    /// no socket is opened.
    ///
    /// **Mutation:** return early from [`ApnsPushSender::send`] for a finished
    /// turn and its counts read `0`.
    #[tokio::test]
    async fn a_doorbell_is_filed_for_every_paired_phone_and_a_departing_device_takes_its_own() {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ccd-paired-fleet-{}-{}-{}.db",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(crate::store::Store::open(&path).unwrap());
        for device in ["phone-1", "phone-2"] {
            store
                .insert_device(
                    device,
                    &format!("iPhone ({device})"),
                    &format!("hash-{device}"),
                    "2026-08-01T00:00:00Z",
                )
                .unwrap();
            store
                .set_push_token(device, &format!("tok-{device}"), "production", None)
                .unwrap();
        }
        let turn_end = PushHint {
            kind: crate::apns::PushKind::Completed,
            ..hint(0)
        };

        for (what, doorbell) in [("an approval", hint(1)), ("a finished turn", turn_end)] {
            let sender =
                ApnsPushSender::without_a_network(Arc::new(StoreRegistry::new(Arc::clone(&store))));
            sender.send(&doorbell, &[]);
            for device in ["phone-1", "phone-2"] {
                assert_eq!(
                    queued(&sender, device),
                    1,
                    "{what} must be filed once for {device}"
                );
            }
        }

        // **And when a device leaves the fleet, what was queued for it goes with
        // it.** Retirement is the one path by which a phone leaves — a revoked
        // device, or one whose token another device claimed — and it takes the
        // queue with it rather than leaving a doorbell waiting for a socket nobody
        // will open.
        let sender =
            ApnsPushSender::without_a_network(Arc::new(StoreRegistry::new(Arc::clone(&store))));
        sender.send(&hint(1), &[]);
        assert_eq!(queued(&sender, "phone-1"), 1, "the premise");
        sender.retire("phone-1");
        assert_eq!(
            queued(&sender, "phone-1"),
            0,
            "a retired device is not left holding an undelivered doorbell"
        );

        let _ = std::fs::remove_file(&path);
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
                        decisions: blocked,
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
                        format!("{blocked} waiting for you")
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
    fn many_decisions_coalesce_to_a_count() {
        let payload = ApnsPushSender::payload(&hint(4));
        let parsed: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");
        assert_eq!(parsed["aps"]["badge"], 4);
        let body = parsed["aps"]["alert"]["body"].as_str().unwrap_or_default();
        assert!(body.contains('4'), "the count is the useful fact: {body}");
    }
}
