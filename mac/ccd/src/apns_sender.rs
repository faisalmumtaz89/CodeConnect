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
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use crate::apns::{alert, PushHint, PushSender, TestDelivery};

/// How long APNs should keep trying to deliver a push to a phone that is off.
///
/// Long enough to survive a commute, short enough that a decision nobody
/// answered is not still buzzing tomorrow.
const PUSH_LIFETIME_SECS: i64 = 3600;

/// Now, in whole seconds — the unit `apns-expiration` is written in.
fn now_secs() -> i64 {
    protocol::time::now_unix_ms() / 1000
}

/// How long the connect and the HTTP/2 handshake may take. Generous for a
/// network round trip, short enough that a silently-filtered connection is
/// reported rather than left pending forever. The legs after it are covered by
/// `DELIVERY_DEADLINE`, which bounds the attempt as a whole.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// This device's queue, or a fresh one.
///
/// **A closed queue is never handed back.** Its worker has returned, so work
/// pushed onto it would sit there for the life of the process — and a device id
/// outlives the token behind it: a phone that reinstalls registers a new token
/// under the same row, and it has to start ringing again.
fn queue_for(
    queues: &mut HashMap<String, Arc<DeviceQueue>>,
    device_id: &str,
) -> (Arc<DeviceQueue>, bool) {
    if let Some(queue) = queues.get(device_id) {
        if !queue.is_closed() {
            return (Arc::clone(queue), false);
        }
    }
    let queue = Arc::new(DeviceQueue {
        waiting: std::sync::Mutex::new(std::collections::VecDeque::new()),
        wake: tokio::sync::Notify::new(),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    queues.insert(device_id.to_string(), Arc::clone(&queue));
    (queue, true)
}

/// Retire this device's queue: its worker ends and whatever was waiting is
/// answered rather than abandoned.
fn retire(queues: &mut HashMap<String, Arc<DeviceQueue>>, device_id: &str) {
    if let Some(queue) = queues.remove(device_id) {
        queue.close(device_id);
    }
}

/// Apple said the app is gone from this device.
///
/// **A fact from Apple, not an inference.** The alternative was asking the
/// registry whether the device was still listed, which cannot tell "revoked"
/// from "the database did not answer just then" — and a database that blinks
/// would have retired every worker that happened to ask.
#[derive(Debug)]
struct DeviceGone;

impl std::fmt::Display for DeviceGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("APNs says the app is gone from this device")
    }
}

impl std::error::Error for DeviceGone {}

/// The one slot an ordinary doorbell occupies on a phone.
///
/// Constant on purpose — see the header's comment in `request`.
const COLLAPSE_ID: &str = "codeconnect";

/// The slot a **test** notification occupies, which is deliberately not the
/// doorbell's.
///
/// The user asked for this one and is watching for it; letting it replace a
/// waiting decision — or be replaced by one — would answer a different
/// question than the one they asked.
const TEST_COLLAPSE_ID: &str = "codeconnect-test";

/// How long one delivery may take in total, retry included.
///
/// **The legs are bounded individually and that is not enough.** A TLS
/// handshake and the read of an error body have no deadline of their own, and a
/// device's deliveries run one after another — so a single stalled attempt
/// would hold up every later push to that phone, and any test notification
/// waiting behind it, for as long as the peer stayed silent. This bounds the
/// whole attempt rather than adding a deadline to each leg one bug at a time.
const DELIVERY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(45);
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
    /// Apple said this token is dead (`410 Unregistered`) — the app is gone
    /// from that device, which never recovers.
    ///
    /// A `400 BadDeviceToken` is deliberately not this: it is far more often
    /// the *wrong host* for a perfectly good token, which is why the delivery
    /// path retries against the other environment and records a correction
    /// before it would ever give up on a device.
    /// `refused` is the token Apple rejected, so a late refusal cannot erase a
    /// token registered since. Returns whether it *was* the registered token:
    /// `false` means the phone has re-registered and this answer is about a
    /// device that is, as far as anything now cares, still there.
    fn forget(&self, device_id: &str, refused: &str, reason: &str) -> bool;
    /// Apple refused the token on the host we chose but the *other* host is
    /// plausible. Records the correction so the next push goes straight there.
    /// Scoped to the token that was corrected, for the same reason as `forget`.
    fn correct_environment(&self, device_id: &str, token: &str, environment: ApnsEnvironment);
}

/// One delivery, waiting its turn.
struct Delivery {
    target: PushTarget,
    payload: String,
    /// Which slot on the phone this replaces — see `COLLAPSE_ID`.
    collapse: &'static str,
    /// Set only for the test push, which reports back to whoever asked for it.
    respond: Option<tokio::sync::oneshot::Sender<TestDelivery>>,
}

/// **One device's pushes, in the order the daemon decided them.**
///
/// APNs keeps a single offline notification per app and does not say which, so
/// which push a reader ends up holding is decided by arrival order. Delivering
/// each push on its own task made that order the network's to choose: an
/// approval could reach Apple before the weaker notice it supersedes, and the
/// notice would be the one kept. A device's deliveries therefore go through one
/// worker that finishes each attempt — retry included — before starting the
/// next, so the last push the daemon decided is the last one submitted.
///
/// Submission order is all a sender controls: Apple does not promise to keep
/// the newest of several stored notifications. What makes the newest *win* is
/// the collapse id on the request; ordering here is what makes "newest" mean
/// what the daemon meant by it.
///
/// Ordering is per device and nothing more: two devices never wait on each
/// other.
struct DeviceQueue {
    waiting: std::sync::Mutex<std::collections::VecDeque<Delivery>>,
    wake: tokio::sync::Notify,
    /// Set when the device is gone, so the worker and its watcher end rather
    /// than waiting forever on a phone that will never be pushed to again.
    closed: std::sync::atomic::AtomicBool,
}

/// How many pushes may wait behind an in-flight one before the oldest is
/// dropped.
///
/// A doorbell is worth ringing about the present. If deliveries to a device are
/// backing up — Apple unreachable, a network holding connections open — the
/// pushes at the front describe a state the run has long since left, and
/// keeping them would mean a reader eventually receives the *oldest* stale
/// fact. The drop is logged; it is never silent.
const MAX_WAITING: usize = 8;

impl DeviceQueue {
    fn push(&self, delivery: Delivery, device_id: &str) {
        let mut waiting = self.waiting.lock().unwrap();
        // **Checked under the same lock the drain holds.** A queue can be
        // retired between being handed out and being pushed to, and appending
        // after the drain would leave the work with no worker — and, for a test
        // push, whoever asked for it waiting on a channel with nothing coming.
        // `close` sets this before it drains, so one of the two orders holds:
        // this push lands first and the drain answers it, or the drain has
        // happened and this sees it here.
        if self.is_closed() {
            crate::log_info!("push: {device_id} was retired; dropping a delivery filed for it");
            if let Some(respond) = delivery.respond {
                let _ = respond.send(TestDelivery::NoToken);
            }
            return;
        }
        // **One doorbell waiting at a time, and it is the newest.** A phone
        // holds one, and a later one replaces it — so a queue of them is a
        // queue of facts that will each be overwritten by the next, with only
        // the last surviving. Keeping a backlog would mean a delivery that
        // waited out a slow one describing a fleet that had moved on minutes
        // ago. A test push is not a doorbell and is never coalesced: somebody
        // asked for it and is waiting to be told what happened.
        if delivery.respond.is_none() {
            // **Removed, not overwritten in place.** Replacing it where it sat
            // would send the newer doorbell ahead of a test push filed between
            // them, and the queue's whole promise is that work leaves in the
            // order it was decided.
            if let Some(at) = waiting.iter().position(|d| d.respond.is_none()) {
                crate::log_debug!(
                    "push: a newer doorbell replaced the one waiting for {device_id}"
                );
                waiting.remove(at);
            }
        }
        if waiting.len() >= MAX_WAITING {
            // **The oldest *test*.** Doorbells coalesce, so at most one is ever
            // waiting and it is the only ordinary notification this device is
            // going to get; discarding it to admit a test would trade the
            // message that matters for one somebody can ask for again.
            if let Some(at) = waiting.iter().position(|d| d.respond.is_some()) {
                crate::log_warn!(
                    "push: {device_id} is {MAX_WAITING} deliveries behind; dropping the oldest test",
                );
                // Told, rather than left waiting on a channel nobody answers.
                if let Some(respond) = waiting.remove(at).and_then(|d| d.respond) {
                    let _ = respond.send(TestDelivery::Failed(
                        "the queue for this device is backed up; nothing was sent".into(),
                    ));
                }
            }
        }
        waiting.push_back(delivery);
        self.wake.notify_one();
    }

    fn take(&self) -> Option<Delivery> {
        self.waiting.lock().unwrap().pop_front()
    }

    /// This device is gone. Whatever was waiting for it is dropped, and the
    /// worker is told to stop.
    ///
    /// **Not an abort.** Cancelling the tasks would leave a delivery half sent
    /// and a test push unanswered; ending the loop lets it finish what it is
    /// doing and return, and the watcher then sees a worker that stopped on
    /// purpose rather than one that fell over.
    fn close(&self, device_id: &str) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        for stranded in self.waiting.lock().unwrap().drain(..) {
            if let Some(respond) = stranded.respond {
                let _ = respond.send(TestDelivery::NoToken);
            }
        }
        crate::log_info!("push: {device_id} is no longer registered; its queue is closed");
        self.wake.notify_one();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }
}

pub struct ApnsPushSender {
    token: Arc<ProviderToken>,
    registry: Arc<dyn PushRegistry>,
    tls: TlsConnector,
    queues: std::sync::Mutex<HashMap<String, Arc<DeviceQueue>>>,
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
            queues: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// The APNs request, headers and all.
    ///
    /// **Extracted so the headers can be asserted.** The expiry in particular
    /// is invisible to a payload test: Apple reads `apns-expiration` as an
    /// absolute UNIX time, so a literal duration there means January 1970 — a
    /// notification born expired, with no store-and-forward for a phone that is
    /// switched off. A test that
    /// recomputes the arithmetic proves nothing about what is on the wire; this
    /// is what is on the wire.
    fn request(
        host: &str,
        device_token: &str,
        bearer: &str,
        topic: &str,
        now_secs: i64,
        collapse: &str,
    ) -> Result<http::Request<()>> {
        http::Request::builder()
            .method("POST")
            .uri(format!("https://{host}/3/device/{device_token}"))
            .header("authorization", format!("bearer {bearer}"))
            .header("apns-topic", topic)
            .header("apns-push-type", "alert")
            // **One doorbell, replaced rather than queued.** Ordering the
            // sends is all this daemon can do; which notification Apple keeps
            // for a phone that is switched off is Apple's to decide, and it
            // does not promise the newest. A collapse id makes that explicit:
            // a later push *replaces* the earlier one, so a reader who was
            // away comes back to the current state rather than to whichever
            // arrived in the order the network chose.
            //
            // A constant, and deliberately not per run: this header reaches
            // Apple, and anything varying per session would let a device's
            // pushes be grouped and timed. It says only "this is CodeConnect's
            // notification", which is exactly the one slot the aggregate body
            // is written for.
            .header("apns-collapse-id", collapse)
            // 10 = deliver immediately. This is a human waiting on an agent.
            .header("apns-priority", "10")
            // An hour from now, as an absolute time: an approval nobody
            // answered by then is not worth waking anyone for, and the app
            // shows it on next foreground regardless.
            .header(
                "apns-expiration",
                (now_secs + PUSH_LIFETIME_SECS).to_string(),
            )
            .body(())
            .map_err(Into::into)
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
        Self::serve_with(queue, move |target, payload, collapse| {
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

    /// The worker loop, over a world it is handed.
    ///
    /// **Each attempt is awaited whole**, retry included: spawning it and
    /// moving on would put the deliveries back on the network's schedule, which
    /// is the reordering this exists to prevent. The loop is written against
    /// two closures so that ordering, serialisation and the revocation check
    /// can be proven without a network — the thing they guard is *when* work
    /// starts, which no test of a queue's contents can see.
    async fn serve_with<A, Fut>(queue: Arc<DeviceQueue>, attempt: A)
    where
        A: Fn(PushTarget, String, &'static str) -> Fut,
        Fut: std::future::Future<Output = Result<Option<String>>>,
    {
        loop {
            if queue.is_closed() {
                return;
            }
            let Some(delivery) = queue.take() else {
                // A `notify_one` with nobody waiting leaves a permit, so a push
                // filed in the instant between the take above and this line
                // wakes it immediately rather than sitting until the next one.
                queue.wake.notified().await;
                continue;
            };
            let device = delivery.target.device_id.clone();
            let outcome = attempt(delivery.target, delivery.payload, delivery.collapse).await;
            // **Terminal for this worker.** A device leaves once, and a loop
            // that merely logged the failure would park forever on a phone that
            // is never coming back. Closing answers whatever else was waiting
            // rather than abandoning it, and `enqueue` starts a fresh queue if
            // the same device id ever registers a new token.
            let gone = matches!(&outcome, Err(err) if err.chain().any(|e| e.is::<DeviceGone>()));
            match (outcome, delivery.respond) {
                (Ok(apns_id), Some(respond)) => {
                    crate::log_info!("push: test accepted for device {device}");
                    let _ = respond.send(TestDelivery::Accepted { apns_id });
                }
                (Err(err), Some(respond)) => {
                    crate::log_warn!("push: test to {device} failed: {err:#}");
                    let _ = respond.send(TestDelivery::Failed(format!("{err:#}")));
                }
                (Ok(_), None) => crate::log_info!("push: delivered to device {device}"),
                (Err(err), None) => crate::log_warn!("push: {device}: {err:#}"),
            }
            if gone {
                queue.close(&device);
                return;
            }
        }
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

        let request = Self::request(
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

/// Keep a task running until it stops on purpose.
///
/// A worker returns normally exactly twice: when its queue is retired, and when
/// Apple says the app is gone from the device. Anything else — a panic — leaves
/// a phone that quietly stops ringing while its queue goes on accepting pushes
/// nothing will ever take, so that ending starts another worker and this one
/// does not.
async fn keep_running<F>(mut start: F, device_id: &str)
where
    F: FnMut() -> tokio::task::JoinHandle<()>,
{
    loop {
        match start().await {
            // Stopped on purpose: the device is retired, or Apple has
            // disowned it. There is nothing left to serve.
            Ok(()) => return,
            Err(err) if err.is_panic() => {
                crate::log_warn!("push: the worker for {device_id} died; starting another");
            }
            // Cancelled, which is a shutdown, not a fault.
            Err(_) => return,
        }
    }
}

/// Whether this push is for this device.
///
/// **A function, so the seen-filter can be tested.** A device whose live socket
/// already carried the fact must not also be rung, and that is the whole of the
/// gate — worth asserting directly rather than inferring from a payload.
///
/// **The list, not a predicate.** `send` iterates exactly what this returns, so
/// a test of this function is a test of who gets rung — which a per-target
/// predicate could only be if every call site were also read.
fn recipients(targets: Vec<PushTarget>, excluded: &[String]) -> Vec<PushTarget> {
    targets
        .into_iter()
        .filter(|target| {
            if excluded.contains(&target.device_id) {
                // This device's live socket already carried the fact. Ringing
                // it again is the noise the gate exists to stop.
                crate::log_debug!("push: {} already saw this live; skipping", target.device_id);
                return false;
            }
            true
        })
        .collect()
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

/// The platform trust store, read once.
///
/// Apple's endpoint presents a certificate chaining to a public root, so the
/// system store is exactly right and vendoring a root set would be a second
/// thing to keep current.
fn platform_roots() -> Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>> {
    rustls_native_certs::load_native_certs().certs
}

/// What Apple's refusal means for this device, as one decision.
///
/// **Classification and consequence together.** Read apart, a build could go on
/// deciding `410` is terminal while the refusal it produced was an ordinary
/// error — and the worker, which retires on the *typed* one, would go on
/// ringing a phone the app had been deleted from.
fn refusal(status: u16, reason: &str) -> anyhow::Error {
    let described = format!("APNs refused the push: {status} {reason}");
    if status == 410 {
        // `410 Unregistered` is Apple saying the app is gone from that device,
        // which never recovers.
        return anyhow::Error::new(DeviceGone).context(described);
    }
    anyhow::Error::msg(described)
}

/// The same refusal, once it is known whether it was about the token the device
/// is actually using.
///
/// **Only a refusal about the live token retires the device.** A late `410` for
/// a token the phone has already replaced says nothing about the one it is
/// using now, and typing it as a departure would close a queue holding that
/// token's work and answer its test push as though the phone were gone.
fn terminal_refusal(status: u16, reason: &str, was_current: bool) -> anyhow::Error {
    if was_current {
        return refusal(status, reason);
    }
    anyhow::Error::msg(format!(
        "APNs refused a token this device has already replaced: {status} {reason}"
    ))
}

/// Whether that refusal means this device token will never work again.
fn is_terminal(status: u16, reason: &str) -> bool {
    refusal(status, reason)
        .chain()
        .any(|cause| cause.is::<DeviceGone>())
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
        }
    }

    /// **The header on the wire, against a fixed clock.**
    ///
    /// Apple reads `apns-expiration` as an absolute UNIX time, so a literal
    /// duration there means January 1970: every notification born expired, and
    /// nothing stored for a phone that is switched off. A test that recomputes
    /// the arithmetic would pass against that too, which is why this reads the
    /// built request.
    #[test]
    fn the_expiry_header_is_an_absolute_time_an_hour_ahead() {
        let now = 1_800_000_000i64;
        let request = ApnsPushSender::request(
            "api.push.apple.com",
            "aa",
            "bearer",
            "topic",
            now,
            COLLAPSE_ID,
        )
        .unwrap();
        let expiry = request.headers()["apns-expiration"].to_str().unwrap();

        assert_eq!(expiry, (now + 3600).to_string());
        assert!(
            expiry.parse::<i64>().unwrap() > now,
            "an expiry in the past is a push APNs will never store"
        );
        assert_ne!(expiry, "3600", "3600 is 1970, not an hour from now");
    }

    #[test]
    fn the_request_addresses_one_device_and_says_it_is_an_alert() {
        let request = ApnsPushSender::request(
            "api.push.apple.com",
            "dev-token",
            "b",
            "topic",
            0,
            COLLAPSE_ID,
        )
        .unwrap();
        assert_eq!(request.uri().path(), "/3/device/dev-token");
        assert_eq!(request.headers()["apns-push-type"], "alert");
        assert_eq!(request.headers()["apns-priority"], "10");
        assert_eq!(
            request.headers()["apns-collapse-id"],
            "codeconnect",
            "a later doorbell replaces the earlier one rather than racing it"
        );
        assert!(
            !request.headers()["apns-collapse-id"]
                .to_str()
                .unwrap()
                .contains(|c: char| c.is_ascii_digit()),
            "and it carries no run, device or request identity through Apple"
        );
    }

    /// Await something a regression would never deliver, without hanging.
    ///
    /// **Every wait in these tests is bounded.** The failures they guard
    /// against are answers that never come, so an unbounded await turns a red
    /// test into a stuck one — and CI runs plain `cargo test`, with no
    /// per-test deadline to catch it.
    async fn within<F: std::future::Future>(what: &str, f: F) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(5), f)
            .await
            .unwrap_or_else(|_| panic!("{what} never arrived"))
    }

    fn queue() -> DeviceQueue {
        DeviceQueue {
            waiting: std::sync::Mutex::new(std::collections::VecDeque::new()),
            wake: tokio::sync::Notify::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn delivery(body: &str) -> Delivery {
        Delivery {
            target: target("phone"),
            payload: body.to_string(),
            collapse: COLLAPSE_ID,
            respond: None,
        }
    }

    fn drained(queue: &DeviceQueue) -> Vec<String> {
        std::iter::from_fn(|| queue.take())
            .map(|d| d.payload)
            .collect()
    }

    /// **The worker starts work in order, one at a time, and only the newest
    /// doorbell is ever waiting.**
    ///
    /// The queue's contents cannot show the first two: a loop that spawned
    /// every delivery would drain it in the same order and still let the
    /// network decide who reaches Apple first. What is asserted here is the
    /// order work *started* in, that each attempt finished before the next
    /// began, and that a doorbell filed while one is in flight replaces the one
    /// waiting rather than joining a backlog.
    ///
    /// Nothing here waits out a delay: each attempt announces itself and then
    /// blocks until the test lets it go, so the interleaving is one the test
    /// chose rather than one it hoped for.
    #[tokio::test]
    async fn deliveries_start_in_order_and_only_the_newest_waits() {
        let queue = Arc::new(queue());
        let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();

        let (seen, busy, held) = (
            Arc::clone(&log),
            Arc::clone(&in_flight),
            Arc::clone(&release),
        );
        let worker = tokio::spawn(ApnsPushSender::serve_with(
            Arc::clone(&queue),
            move |_target, payload, _collapse| {
                let (seen, busy, held, started) = (
                    Arc::clone(&seen),
                    Arc::clone(&busy),
                    Arc::clone(&held),
                    started.clone(),
                );
                async move {
                    assert_eq!(
                        busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                        0,
                        "a second delivery began while one was still in flight"
                    );
                    let _ = started.send(payload.clone());
                    held.notified().await;
                    seen.lock().unwrap().push(payload);
                    busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(None)
                }
            },
        ));

        queue.push(delivery("first"), "phone");
        assert_eq!(
            within("the first delivery's start", starts.recv())
                .await
                .unwrap(),
            "first",
            "the first one starts"
        );

        // Both arrive while the first is still in flight. The second never
        // reaches Apple: the third is the state of the world by then.
        queue.push(delivery("second"), "phone");
        queue.push(delivery("third"), "phone");
        release.notify_one();
        assert_eq!(
            within("the replacement's start", starts.recv())
                .await
                .unwrap(),
            "third",
            "the newest doorbell replaced the one waiting"
        );
        release.notify_one();

        for _ in 0..200 {
            if log.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        worker.abort();
        assert_eq!(*log.lock().unwrap(), ["first", "third"]);
    }

    /// Retiring a device ends its worker and answers what it was holding.
    #[tokio::test]
    async fn retiring_a_device_closes_its_queue_and_answers_its_work() {
        let mut queues: HashMap<String, Arc<DeviceQueue>> = HashMap::new();
        let (queue, _) = queue_for(&mut queues, "phone");
        let (respond, rx) = tokio::sync::oneshot::channel();
        queue.push(
            Delivery {
                target: target("phone"),
                payload: "test".into(),
                collapse: TEST_COLLAPSE_ID,
                respond: Some(respond),
            },
            "phone",
        );

        retire(&mut queues, "phone");
        assert!(queue.is_closed(), "the worker is told to stop");
        assert!(queues.is_empty(), "and the device is forgotten");
        assert!(
            matches!(
                within("the retired work's answer", rx).await,
                Ok(TestDelivery::NoToken)
            ),
            "whoever was waiting is told, rather than left on a dead channel"
        );
    }

    /// **A delivery filed for a queue that has just been retired is answered,
    /// not stranded.** The queue is handed out while open and pushed to a
    /// moment later; a worker can retire it in between. Appending after the
    /// drain would leave a test push waiting on a channel with nothing coming,
    /// and the socket handler that asked for it waiting with it.
    #[tokio::test]
    async fn a_delivery_filed_after_retirement_is_answered_rather_than_stranded() {
        let queue = queue();
        // Handed out open, retired before the push lands.
        queue.close("phone");

        let (respond, rx) = tokio::sync::oneshot::channel();
        queue.push(
            Delivery {
                target: target("phone"),
                payload: "test".into(),
                collapse: TEST_COLLAPSE_ID,
                respond: Some(respond),
            },
            "phone",
        );
        // **Bounded.** The failure this guards against is an answer that never
        // comes, so an unbounded await would hang here rather than fail — and a
        // suite that hangs is worse than one that goes red.
        assert!(
            matches!(within("an answer", rx).await, Ok(TestDelivery::NoToken)),
            "whoever asked is told, rather than left waiting on a dead channel"
        );

        queue.push(delivery("doorbell"), "phone");
        assert!(
            drained(&queue).is_empty(),
            "and nothing is left behind for a worker that is gone"
        );
    }

    /// **A refusal about a token the phone has already replaced is not a
    /// departure.** Typed as one, it would retire a queue holding the *new*
    /// token's work and answer its test push as though the phone were gone.
    #[test]
    fn only_a_refusal_about_the_live_token_retires_the_device() {
        let gone = |err: anyhow::Error| err.chain().any(|cause| cause.is::<DeviceGone>());
        assert!(
            gone(terminal_refusal(410, "{\"reason\":\"Unregistered\"}", true)),
            "the token it is using was disowned: the device is gone"
        );
        assert!(
            !gone(terminal_refusal(
                410,
                "{\"reason\":\"Unregistered\"}",
                false
            )),
            "a token it has already replaced says nothing about the one it uses now"
        );
    }

    /// **A device that comes back gets a working queue.** A device id outlives
    /// the token behind it — a phone that reinstalls registers a new one under
    /// the same row — and handing back the retired queue would file its pushes
    /// somewhere nothing drains.
    #[test]
    fn a_device_that_registers_again_is_not_handed_its_retired_queue() {
        let mut queues: HashMap<String, Arc<DeviceQueue>> = HashMap::new();

        let (first, is_new) = queue_for(&mut queues, "phone");
        assert!(is_new, "the first ask starts one");
        let (again, is_new) = queue_for(&mut queues, "phone");
        assert!(!is_new, "and the second is the same queue, still running");
        assert!(Arc::ptr_eq(&first, &again));

        first.close("phone");
        let (fresh, is_new) = queue_for(&mut queues, "phone");
        assert!(is_new, "a retired queue is replaced, not reused");
        assert!(!Arc::ptr_eq(&first, &fresh));
        assert!(!fresh.is_closed());
    }

    /// **Apple saying the app is gone retires the worker.**
    ///
    /// A device leaves once, and a loop that only logged the refusal would park
    /// forever on a phone that is never coming back. What is left waiting is
    /// answered rather than abandoned, and nothing more is sent.
    #[tokio::test]
    async fn a_device_apple_says_is_gone_retires_its_worker() {
        let queue = Arc::new(queue());
        queue.push(delivery("doorbell"), "phone");
        let (respond, rx) = tokio::sync::oneshot::channel();
        queue.push(
            Delivery {
                target: target("phone"),
                payload: "test".into(),
                collapse: TEST_COLLAPSE_ID,
                respond: Some(respond),
            },
            "phone",
        );

        let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&sent);
        let worker = tokio::spawn(ApnsPushSender::serve_with(
            Arc::clone(&queue),
            move |_target, payload, _collapse| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock().unwrap().push(payload);
                    Err(anyhow::Error::new(DeviceGone).context("410 Unregistered"))
                }
            },
        ));

        // The worker ends rather than parking: awaiting it is the assertion.
        tokio::time::timeout(std::time::Duration::from_secs(5), worker)
            .await
            .expect("the worker for a departed device ends")
            .expect("and does so without panicking");
        assert_eq!(
            *sent.lock().unwrap(),
            ["doorbell"],
            "it stops at the refusal rather than working through the queue"
        );
        assert!(
            matches!(
                within("the test push's answer", rx).await,
                Ok(TestDelivery::NoToken)
            ),
            "and the test push behind it is answered, not abandoned"
        );
        assert!(queue.is_closed(), "the queue is retired with the worker");
    }

    /// **A worker that dies is replaced without waiting for the next push.**
    ///
    /// The queue would go on accepting deliveries that nothing took, which on
    /// a phone is indistinguishable from a daemon that has stopped noticing —
    /// and the next push that would have revealed it might be hours away.
    #[tokio::test]
    async fn a_worker_that_dies_is_started_again() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        // Bounded like every other wait here: the regression this guards
        // against is a supervisor that never returns, which would hang the
        // suite rather than fail it.
        within(
            "the supervisor's last worker",
            keep_running(
                move || {
                    let counted = Arc::clone(&counted);
                    tokio::spawn(async move {
                        // The first two die the way a worker must not: a panic
                        // inside the loop. The third returns the way a retired
                        // one does, which is how this test ends.
                        if counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                            panic!("the worker fell over");
                        }
                    })
                },
                "phone",
            ),
        )
        .await;
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "each death is followed by another worker"
        );
    }

    /// **The bound is for test pushes, which are the only thing that can queue
    /// up.** Doorbells replace each other, so they cannot accumulate; a test
    /// push is somebody's explicit request and is never coalesced away. If a
    /// device falls far enough behind that even those pile up, what goes is the
    /// *oldest* — and whoever asked for it is told, rather than left waiting on
    /// a channel with nothing coming.
    #[tokio::test]
    async fn a_backed_up_device_loses_its_stalest_test_push_and_says_so() {
        let queue = queue();
        let mut waiting = Vec::new();
        for n in 0..MAX_WAITING + 2 {
            let (respond, rx) = tokio::sync::oneshot::channel();
            waiting.push(rx);
            queue.push(
                Delivery {
                    target: target("phone"),
                    payload: format!("test-{n}"),
                    collapse: TEST_COLLAPSE_ID,
                    respond: Some(respond),
                },
                "phone",
            );
        }

        for (n, rx) in waiting.drain(..2).enumerate() {
            assert!(
                matches!(
                    within("the dropped test's answer", rx).await,
                    Ok(TestDelivery::Failed(_))
                ),
                "test-{n} was dropped, so it is answered rather than abandoned"
            );
        }
        let left = drained(&queue);
        assert_eq!(left.len(), MAX_WAITING, "the queue is bounded");
        assert_eq!(left.first().unwrap(), "test-2", "the two stalest are gone");
        assert_eq!(
            left.last().unwrap(),
            &format!("test-{}", MAX_WAITING + 1),
            "the newest is always kept"
        );
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
        // **And that verdict is what the worker acts on.** The worker retires
        // on the typed refusal, so a build that classified `410` as terminal
        // while producing an ordinary error would go on ringing a phone the
        // app had been deleted from.
        let gone = |status, reason| {
            refusal(status, reason)
                .chain()
                .any(|cause| cause.is::<DeviceGone>())
        };
        assert!(gone(410, "{\"reason\":\"Unregistered\"}"));
        assert!(!gone(400, "{\"reason\":\"BadDeviceToken\"}"));
        assert!(!gone(503, "{\"reason\":\"ServiceUnavailable\"}"));
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
