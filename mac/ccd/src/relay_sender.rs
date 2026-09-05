//! Push for a Mac that holds no Apple key.
//!
//! The APNs provider key cannot be given to a customer's machine, so a
//! CodeConnect-operated relay holds it and this daemon becomes its client. What
//! crosses that boundary is fixed by [`docs/push-gateway.md`] §4 and by the
//! relay's own parser: a schema number, the device's APNs token, the word
//! `sandbox` or `production`, and either which of four kinds rang with how many
//! runs are blocked, or a marker saying this one is a test. There is no field
//! for a title, a body, a project, a session, a path, or an `aps` object — the
//! relay refuses unknown keys rather than dropping them, so a future call site
//! cannot smuggle one past it and believe it arrived.
//!
//! **Every decision stays here.** Whether to ring, whom to exclude, which run
//! is the subject, how many are blocked, and which notification supersedes
//! which are all settled on the Mac before a request is composed. The relay
//! chooses words from a closed vocabulary and carries the result to Apple; it
//! is a transport with a payload composer, not a participant.
//!
//! The direct sender in [`crate::apns_sender`] is unaffected and keeps its own
//! project-labelled payload: that notification goes from the user's own machine
//! to Apple and passes through nothing in between.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use protocol::secret::Redacted;
use push_core::{terminal_refusal, ApnsEnvironment, COLLAPSE_ID, TEST_COLLAPSE_ID};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;

use crate::apns::{
    CredentialRefused, NoRegistration, PushHint, PushMode, PushSender, SendRateLimited,
    TestDelivery,
};
use crate::push_queue::{
    keep_running, queue_for, recipients, retire, serve_with, Delivery, DeviceQueue, PushRegistry,
    PushTarget, CONNECT_TIMEOUT, DELIVERY_DEADLINE,
};

/// The relay every customer install sends through.
///
/// **Baked, and deliberately not configurable.** A relay URL in the config file
/// would be a switch that points every notification this Mac sends at a
/// stranger's server, readable and writable by anything that can write a file
/// in the user's home directory — and the notification carries the device's
/// APNs token. `docs/push-gateway.md` §9 names public relay URL configuration
/// as a launch non-goal for that reason. A developer who wants their own path
/// to Apple configures the four `apns_*` keys and bypasses this entirely.
const RELAY_HOST: &str = "codeconnect-push-relay.onrender.com";

/// The versioned push endpoint. Only this path, and only `POST`.
const RELAY_PATH: &str = "/v1/push";

/// The document version this daemon writes. The relay refuses any other rather
/// than ignoring it: a daemon that believed something untrue about the relay
/// would deliver under that belief, which is worse than being told no.
const SCHEMA: u32 = 1;

/// The largest fleet one notification will describe, matching the relay's own
/// bound.
///
/// The relay refuses a larger count rather than clamping it, because a clamp
/// there would make a wrong number look plausible. Here the clamp is correct
/// and the reasoning inverts: a thousand simultaneously blocked runs is not a
/// state anyone is in, and dropping the doorbell entirely over an implausible
/// count would silence a real fleet to protect an arithmetic nicety. It is
/// logged, never silent.
const MAX_BLOCKED_COUNT: usize = 999;

/// The largest answer worth reading. The relay's own replies are a few dozen
/// bytes; anything beyond this is not one of them, and reading a hostile body
/// in full is not a thing a push path needs to do.
const MAX_REPLY_BYTES: usize = 4 * 1024;

/// What the relay said, before any of it is interpreted.
///
/// Split from the interpretation so the whole outcome table can be proven
/// without a network — the mapping from a status and a body to a consequence is
/// where the mistakes live, not in the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayReply {
    pub(crate) status: u16,
    /// `Retry-After`, when the refusal carried one. The relay is the only
    /// party that knows its own budget, so the wait is never invented here.
    pub(crate) retry_after_secs: Option<u32>,
    pub(crate) body: String,
}

/// One request to the relay, over whatever carries it.
///
/// A trait so the outcome table, the queue's ordering and the store's
/// compare-and-swap can all be exercised against a relay that answers
/// instantly and exactly, which no test against the real service could do.
/// There is one production implementation and it reaches exactly one host.
pub(crate) trait RelayTransport: Send + Sync {
    /// The bearer arrives [`Redacted`] and stays that way until the header is
    /// built, so no implementation — production or fake — can print it by
    /// accident on the way in.
    fn post(
        &self,
        credential: Redacted,
        body: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RelayReply>> + Send>>;
}

/// The relay over TLS and HTTP/2, which is what the service speaks.
pub(crate) struct HttpsRelay {
    host: String,
    port: u16,
    tls: TlsConnector,
}

impl HttpsRelay {
    /// The one production endpoint, from the baked constants and nothing else.
    pub(crate) fn new() -> Result<Self> {
        Self::against(RELAY_HOST.to_string(), 443)
    }

    /// **Test-only.** The same transport aimed somewhere a test controls.
    /// Compiled out of every shipped binary, so no configuration file,
    /// environment variable or wire message can reach it.
    #[cfg(test)]
    pub(crate) fn at(host: String, port: u16) -> Result<Self> {
        Self::against(host, port)
    }

    fn against(host: String, port: u16) -> Result<Self> {
        Ok(Self {
            host,
            port,
            tls: crate::tls::outbound_h2("relay")?,
        })
    }
}

impl RelayTransport for HttpsRelay {
    fn post(
        &self,
        credential: Redacted,
        body: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RelayReply>> + Send>> {
        let (host, port, tls) = (self.host.clone(), self.port, self.tls.clone());
        Box::pin(async move { post(host, port, tls, credential, body).await })
    }
}

/// The request, without the socket.
///
/// **The bearer rides in `Authorization` and nowhere else** — never in the URL,
/// never in the document. A URL is written to the access log of every proxy on
/// the path, and a document is what the relay stores and this daemon composes;
/// the header is the one place a secret belongs. Separated from [`post`] so that
/// claim is asserted by a test rather than by this comment.
///
/// **This is the one line that exposes the bearer.** It arrives [`Redacted`] and
/// is unwrapped here, at the header build and nowhere earlier — the single byte
/// the plan permits it in the clear.
fn relay_request(host: &str, credential: &Redacted) -> Result<http::Request<()>> {
    http::Request::builder()
        .method(http::Method::POST)
        .uri(format!("https://{host}{RELAY_PATH}"))
        .header(
            http::header::AUTHORIZATION,
            format!("Bearer {}", credential.expose()),
        )
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(())
        .context("building the relay request")
}

/// One POST, bounded at every leg that could otherwise wait forever.
async fn post(
    host: String,
    port: u16,
    tls: TlsConnector,
    credential: Redacted,
    body: String,
) -> Result<RelayReply> {
    let stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .with_context(|| format!("connecting to {host} timed out after {CONNECT_TIMEOUT:?}"))?
    .with_context(|| format!("connecting to {host}"))?;
    let server_name = ServerName::try_from(host.clone()).context("relay host name")?;
    // Bounded like the legs either side of it. A handshake has no deadline of
    // its own, and a peer that completes a TCP connection and then says nothing
    // would otherwise hold the head of this device's queue for the whole
    // 45-second attempt — with every later push to that phone behind it.
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, tls.connect(server_name, stream))
        .await
        .with_context(|| {
            format!("the TLS handshake with {host} timed out after {CONNECT_TIMEOUT:?}")
        })?
        .with_context(|| format!("TLS handshake with {host}"))?;

    let (mut send_request, connection) =
        tokio::time::timeout(CONNECT_TIMEOUT, h2::client::handshake(stream))
            .await
            .context("HTTP/2 handshake with the relay timed out")?
            .context("HTTP/2 handshake with the relay")?;
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            crate::log_debug!("push: relay connection ended: {err}");
        }
    });

    let request = relay_request(&host, &credential)?;

    let (response, mut request_body) = send_request
        .send_request(request, false)
        .context("sending the relay request")?;
    request_body
        .send_data(body.into(), true)
        .context("sending the relay document")?;

    let response = tokio::time::timeout(CONNECT_TIMEOUT, response)
        .await
        .context("the relay accepted the request and never answered")?
        .context("awaiting the relay response")?;
    let status = response.status().as_u16();
    let retry_after_secs = response
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u32>().ok());

    // **Stop reading, not merely stop appending.** The outcome is already
    // decided by the status; an answer longer than this is not one of the
    // relay's, and going on reading it would hold the head of this device's
    // queue — with every later push to that phone behind it — for as long as a
    // peer cared to dribble bytes.
    let mut reply = String::new();
    let mut stream = response.into_body();
    while let Some(chunk) = stream.data().await {
        let Ok(bytes) = chunk else { break };
        let _ = stream.flow_control().release_capacity(bytes.len());
        let room = MAX_REPLY_BYTES - reply.len();
        if bytes.len() >= room {
            reply.push_str(&String::from_utf8_lossy(&bytes[..room]));
            break;
        }
        reply.push_str(&String::from_utf8_lossy(&bytes));
    }

    Ok(RelayReply {
        status,
        retry_after_secs,
        body: reply,
    })
}

/// What the relay's answer means to this daemon.
///
/// Each variant is a different repair, which is why they are not one string:
/// a dead token and a dead credential look alike on a lock screen and are
/// opposites in the store — one clears a registration, the other keeps it and
/// asks the phone for a new bearer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RelayOutcome {
    /// Delivered, with Apple's receipt when there was one and the environment
    /// the binding is authoritative for.
    Accepted {
        apns_id: Option<String>,
        environment: Option<ApnsEnvironment>,
    },
    /// Apple has disowned the app on this device. Terminal for the token.
    Unregistered,
    /// The bearer is not one the relay will act on. **The token is untouched.**
    CredentialInvalid,
    RateLimited {
        retry_after_secs: u32,
    },
    /// Everything else, with the relay's own word for it where it gave one.
    Refused(String),
}

/// Every word the relay's own schema can answer with.
///
/// **A closed set on the way in, matching the closed set on the way out.** The
/// refusal word ends up in this daemon's log and, for a deliberate test, on the
/// user's screen. Echoing whatever arrived would let a relay — or anything that
/// got between this Mac and one — write arbitrary text into both, which is the
/// mirror image of the hole §4's request schema exists to close. A word that is
/// not one of these is not a word this daemon repeats.
const RELAY_WORDS: &[&str] = &[
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
];

/// Apple's receipt, or nothing.
///
/// Bounded and restricted for the same reason as the refusal word: it is
/// rendered to the user beside a successful test. A UUID is 36 characters; this
/// admits a generous superset of that and no character that could reflow a line
/// of log or a line of UI.
fn receipt(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    .then(|| value.to_string())
}

/// Read one answer. Pure, so every row of the table is a test.
fn read(reply: &RelayReply) -> RelayOutcome {
    let document: Option<serde_json::Value> = serde_json::from_str(&reply.body).ok();
    let word = document
        .as_ref()
        .and_then(|d| d.get("error").or_else(|| d.get("outcome")))
        .and_then(|w| w.as_str())
        .filter(|word| RELAY_WORDS.contains(word))
        .unwrap_or("");

    match reply.status {
        200..=299 => {
            // **The environment is read strictly.** A lenient parse would turn
            // a garbled answer into the word `sandbox` and then persist it over
            // a correct production binding, which is a silent way to stop a
            // phone ringing. An unreadable environment corrects nothing.
            let environment = document
                .as_ref()
                .and_then(|d| d.get("environment"))
                .and_then(|e| e.as_str())
                .and_then(environment_named);
            RelayOutcome::Accepted {
                apns_id: document
                    .as_ref()
                    .and_then(|d| d.get("apns_id"))
                    .and_then(|id| id.as_str())
                    .and_then(receipt),
                environment,
            }
        }
        401 | 403 => RelayOutcome::CredentialInvalid,
        410 => RelayOutcome::Unregistered,
        429 => RelayOutcome::RateLimited {
            // A refusal with no wait attached is still a refusal; a minute is
            // the relay's own ceiling and the honest thing to report when it
            // named nothing.
            retry_after_secs: reply.retry_after_secs.unwrap_or(60),
        },
        status => RelayOutcome::Refused(if word.is_empty() {
            format!("the relay answered {status}")
        } else {
            format!("the relay answered {status} {word}")
        }),
    }
}

/// The two words the relay's schema admits, and nothing else.
fn environment_named(value: &str) -> Option<ApnsEnvironment> {
    match value {
        "sandbox" => Some(ApnsEnvironment::Sandbox),
        "production" => Some(ApnsEnvironment::Production),
        _ => None,
    }
}

/// The ordinary doorbell, as the relay's closed schema.
///
/// **The generic fields and no others.** There is no project label here, no
/// session uid, no device id, no exclusion list, no path and no copy — not
/// because a reviewer removed them, but because the relay's parser refuses
/// every key that is not one of these and would answer `400` to a document
/// carrying one.
fn doorbell_document(target: &PushTarget, hint: &PushHint) -> String {
    let blocked = if hint.blocked_sessions > MAX_BLOCKED_COUNT {
        crate::log_warn!(
            "push: {} runs are blocked; the relay is told {MAX_BLOCKED_COUNT}",
            hint.blocked_sessions
        );
        MAX_BLOCKED_COUNT
    } else {
        hint.blocked_sessions
    };
    serde_json::json!({
        "schema": SCHEMA,
        "token": target.token,
        "environment": target.environment.as_str(),
        "notification": {
            "type": "doorbell",
            "kind": hint.kind.tag(),
            "blocked_count": blocked,
        }
    })
    .to_string()
}

/// The deliberate test, which carries no kind and no count at all.
fn test_document(target: &PushTarget) -> String {
    serde_json::json!({
        "schema": SCHEMA,
        "token": target.token,
        "environment": target.environment.as_str(),
        "notification": { "type": "test" }
    })
    .to_string()
}

pub struct RelayPushSender {
    transport: Arc<dyn RelayTransport>,
    registry: Arc<dyn PushRegistry>,
    queues: std::sync::Mutex<HashMap<String, Arc<DeviceQueue>>>,
}

impl RelayPushSender {
    pub(crate) fn new(transport: Arc<dyn RelayTransport>, registry: Arc<dyn PushRegistry>) -> Self {
        Self {
            transport,
            registry,
            queues: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// One attempt, whole: compose nothing, send what the caller composed, and
    /// turn the answer into a consequence.
    ///
    /// **There is no retry.** A relay that answered `429` or `503` is refusing
    /// on purpose or is unwell, and a daemon that retried would multiply the
    /// load that produced the refusal. A doorbell is best-effort by
    /// construction and the phone reconciles from the event log; the outer
    /// deadline is the only bound that matters.
    async fn deliver(
        transport: Arc<dyn RelayTransport>,
        registry: Arc<dyn PushRegistry>,
        target: PushTarget,
        document: String,
    ) -> Result<Option<String>> {
        let Some(credential) = target.credential.clone() else {
            // Registered without a bearer — a phone that paired to this Mac
            // while it was in direct mode, or one that never enrolled. **This
            // is a missing tuple, not a refusal**: nothing was sent and nothing
            // was refused, so the plan's table calls for `NoRegisteredToken`,
            // which asks the phone to register — not `credential_invalid`,
            // which would name a relay conversation that never happened and
            // send it enrolling for a replacement bearer instead.
            return Err(anyhow::Error::new(NoRegistration)
                .context("this device registered no relay credential"));
        };

        let reply = transport.post(credential.clone(), document).await?;
        match read(&reply) {
            RelayOutcome::Accepted {
                apns_id,
                environment,
            } => {
                // **The binding is the authority and this is how its correction
                // travels.** A second Mac that still believes the old
                // environment delivers anyway — the relay addresses Apple by
                // the binding, not by what it was told — and learns the truth
                // from the answer. Persisting it is a compare-and-swap against
                // the token it was about, so a correction for a token the phone
                // has already replaced changes nothing.
                if let Some(environment) = environment {
                    if environment != target.environment {
                        crate::log_info!(
                            "push: the relay reports {} for {}; correcting from {}",
                            environment.as_str(),
                            target.device_id,
                            target.environment.as_str()
                        );
                        // Scoped to the tuple this attempt carried, so a
                        // correction snapshotted under `(T, C1)` cannot move the
                        // environment of a row the phone has since rotated.
                        registry.correct_environment(
                            &target.device_id,
                            &target.token,
                            Some(&credential),
                            environment,
                        );
                    }
                }
                Ok(apns_id)
            }
            RelayOutcome::Unregistered => {
                // Apple's `410`, relayed. Terminal for this token and only for
                // this token: a late refusal about one the phone has already
                // replaced clears nothing and retires nobody.
                let was_current = registry.forget(
                    &target.device_id,
                    &target.token,
                    Some(&credential),
                    "410: the relay reports this device unregistered",
                );
                Err(terminal_refusal(
                    410,
                    "the relay reports this device unregistered",
                    was_current,
                ))
            }
            RelayOutcome::CredentialInvalid => Err(anyhow::Error::new(CredentialRefused)
                .context("the relay refused this token's credential")),
            RelayOutcome::RateLimited { retry_after_secs } => {
                Err(anyhow::Error::new(SendRateLimited { retry_after_secs })
                    .context("the relay refused this push"))
            }
            RelayOutcome::Refused(why) => bail!("{why}"),
        }
    }

    fn enqueue(&self, delivery: Delivery) {
        let device_id = delivery.target.device_id.clone();
        let queue = {
            let mut queues = self.queues.lock().unwrap();
            let (queue, is_new) = queue_for(&mut queues, &device_id);
            if is_new {
                let (started, transport, registry) = (
                    Arc::clone(&queue),
                    Arc::clone(&self.transport),
                    Arc::clone(&self.registry),
                );
                let named = device_id.clone();
                tokio::spawn(async move {
                    keep_running(
                        || {
                            tokio::spawn(Self::serve(
                                Arc::clone(&started),
                                Arc::clone(&transport),
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

    async fn serve(
        queue: Arc<DeviceQueue>,
        transport: Arc<dyn RelayTransport>,
        registry: Arc<dyn PushRegistry>,
    ) {
        let gate = Arc::clone(&registry);
        serve_with(
            queue,
            // The relay makes exactly one post per delivery — there is no retry
            // here, deliberately — so the subject the loop already checked has
            // nothing further to guard and is not carried into the attempt.
            move |target, document, _collapse, _authorized_for| {
                let transport = Arc::clone(&transport);
                let registry = Arc::clone(&registry);
                async move {
                    // The same outer bound the direct sender keeps. The relay does
                    // its own APNs work inside it and answers only once Apple has;
                    // this is what stops one stalled attempt from holding up every
                    // later push to the same phone.
                    match tokio::time::timeout(
                        DELIVERY_DEADLINE,
                        Self::deliver(transport, registry, target, document),
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(_) => bail!("the relay did not answer within {DELIVERY_DEADLINE:?}"),
                    }
                }
            },
            move |device_id, agent| {
                let gate = Arc::clone(&gate);
                async move { crate::push_queue::still_authorized(&*gate, &device_id, &agent).await }
            },
        )
        .await
    }
}

impl PushSender for RelayPushSender {
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
        for target in recipients(targets, excluded, &hint.agent) {
            let document = doorbell_document(&target, hint);
            // The collapse id is passed for the queue's vocabulary and applied
            // by the relay, which is the party that talks to Apple.
            self.enqueue(Delivery {
                target,
                payload: document,
                collapse: COLLAPSE_ID,
                respond: None,
                // The subject the fan-out above just authorized, carried so the
                // worker can ask again immediately before it posts.
                authorized_for: Some(hint.agent.clone()),
            });
        }
    }

    fn send_test(&self, device_id: &str) -> tokio::sync::oneshot::Receiver<TestDelivery> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let Some(target) = self
            .registry
            .targets()
            .into_iter()
            .find(|t| t.device_id == device_id)
        else {
            let _ = tx.send(TestDelivery::NoToken);
            return rx;
        };
        let document = test_document(&target);
        self.enqueue(Delivery {
            target,
            payload: document,
            collapse: TEST_COLLAPSE_ID,
            respond: Some(tx),
            // Not about any run; see the direct sender's twin of this.
            authorized_for: None,
        });
        rx
    }

    fn retire(&self, device_id: &str) {
        retire(&mut self.queues.lock().unwrap(), device_id);
    }

    fn mode(&self) -> PushMode {
        PushMode::Relay
    }
}

/// The relay sender, or the stub when this machine cannot even build a TLS
/// client.
///
/// **A trust store that will not load is the one failure here**, and it means a
/// broken system rather than a missing configuration — so it is reported as
/// what it is and push goes off, rather than being reported as "not
/// configured", which would send somebody looking at their config file.
pub fn build(store: Arc<crate::store::Store>) -> Arc<dyn PushSender> {
    match HttpsRelay::new() {
        Ok(transport) => {
            crate::log_info!("push: relay mode, sending through {RELAY_HOST}");
            Arc::new(RelayPushSender::new(
                Arc::new(transport),
                Arc::new(crate::apns_sender::StoreRegistry::new(store)),
            ))
        }
        Err(err) => {
            crate::log_error!("push: staying off — {err:#}");
            Arc::new(crate::apns::LoggingPushSender::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apns::PushKind;

    const TOKEN: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";

    fn target() -> PushTarget {
        PushTarget {
            token: TOKEN.into(),
            environment: ApnsEnvironment::Sandbox,
            device_id: "phone".into(),
            credential: Some(Redacted::from("bearer-value")),
            features: Default::default(),
        }
    }

    fn hint(blocked: usize) -> PushHint {
        PushHint {
            project_label: "Aion".into(),
            kind: PushKind::Approval,
            blocked_sessions: blocked,
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            agent: protocol::agent::AgentKind::Claude,
        }
    }

    /// The same doorbell, raised by a Codex run.
    ///
    /// Beside [`hint`] rather than a parameter on it, so every test that is not
    /// about agent eligibility goes on reading as though it were not.
    fn codex_hint(blocked: usize) -> PushHint {
        PushHint {
            agent: protocol::agent::AgentKind::Codex,
            ..hint(blocked)
        }
    }

    fn reply(status: u16, body: &str) -> RelayReply {
        RelayReply {
            status,
            retry_after_secs: None,
            body: body.into(),
        }
    }

    /// **The exact document, byte for byte.** The relay refuses unknown fields,
    /// so a key added here is a `400` rather than a leak — but the point of
    /// asserting the whole object is the other direction: nothing that names
    /// the project, the run, the device or the machine may ever appear, and a
    /// test that checked only for the absence of today's field names would not
    /// notice tomorrow's.
    #[test]
    fn the_document_carries_the_generic_fields_and_nothing_else() {
        for (kind, tag) in [
            (PushKind::Approval, "approval"),
            (PushKind::NeedsInput, "input"),
            (PushKind::Completed, "done"),
            (PushKind::Idle, "idle"),
        ] {
            let mut hint = hint(2);
            hint.kind = kind;
            let document: serde_json::Value =
                serde_json::from_str(&doorbell_document(&target(), &hint)).unwrap();
            assert_eq!(
                document,
                serde_json::json!({
                    "schema": 1,
                    "token": TOKEN,
                    "environment": "sandbox",
                    "notification": {
                        "type": "doorbell",
                        "kind": tag,
                        "blocked_count": 2,
                    }
                }),
                "the {tag} document"
            );
        }

        let test: serde_json::Value = serde_json::from_str(&test_document(&target())).unwrap();
        assert_eq!(
            test,
            serde_json::json!({
                "schema": 1,
                "token": TOKEN,
                "environment": "sandbox",
                "notification": { "type": "test" }
            })
        );
    }

    /// The project label is the one thing the direct sender *does* put on a
    /// lock screen, so it is the one most likely to be carried here by
    /// accident.
    #[test]
    fn nothing_identifying_survives_into_the_document() {
        let mut hint = hint(1);
        hint.project_label = "a-very-identifying-project".into();
        hint.session_uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into();
        let document = doorbell_document(&target(), &hint);
        for forbidden in [
            "a-very-identifying-project",
            "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "phone",
            "project",
            "session",
            "label",
            "title",
            "body",
            "aps",
            "device_id",
            "path",
        ] {
            assert!(
                !document.contains(forbidden),
                "{forbidden:?} reached the relay document: {document}"
            );
        }
    }

    /// A count beyond the relay's bound is clamped rather than sent, because a
    /// `400` would drop a doorbell that a real fleet is waiting on.
    #[test]
    fn an_implausible_fleet_is_clamped_to_the_relay_bound() {
        let document: serde_json::Value =
            serde_json::from_str(&doorbell_document(&target(), &hint(5_000))).unwrap();
        assert_eq!(document["notification"]["blocked_count"], 999);
    }

    /// **Every answer the relay documents, mapped to its consequence.** These
    /// are the rows a mistake hides in: `401` and `410` differ by one digit and
    /// by whether a working registration is thrown away.
    #[test]
    fn every_documented_answer_maps_to_its_own_consequence() {
        assert_eq!(
            read(&reply(
                200,
                r#"{"outcome":"accepted","environment":"production","apns_id":"A1"}"#
            )),
            RelayOutcome::Accepted {
                apns_id: Some("A1".into()),
                environment: Some(ApnsEnvironment::Production),
            }
        );
        assert_eq!(
            read(&reply(
                200,
                r#"{"outcome":"accepted","environment":"sandbox"}"#
            )),
            RelayOutcome::Accepted {
                apns_id: None,
                environment: Some(ApnsEnvironment::Sandbox),
            }
        );
        assert_eq!(
            read(&reply(401, r#"{"error":"credential_invalid"}"#)),
            RelayOutcome::CredentialInvalid
        );
        assert_eq!(
            read(&reply(410, r#"{"error":"unregistered"}"#)),
            RelayOutcome::Unregistered
        );
        assert_eq!(
            read(&RelayReply {
                status: 429,
                retry_after_secs: Some(7),
                body: r#"{"error":"rate_limited"}"#.into(),
            }),
            RelayOutcome::RateLimited {
                retry_after_secs: 7
            }
        );

        for (status, word) in [
            (400, "malformed"),
            (400, "schema"),
            (400, "token"),
            (500, "internal"),
            (502, "rejected"),
            (503, "unavailable"),
        ] {
            let body = format!(r#"{{"error":"{word}"}}"#);
            match read(&reply(status, &body)) {
                RelayOutcome::Refused(why) => {
                    assert!(why.contains(word), "{why}");
                    assert!(why.contains(&status.to_string()), "{why}");
                }
                other => panic!("{status} {word} must be a plain refusal, got {other:?}"),
            }
        }
    }

    /// A `429` with no `Retry-After` is still a `429`. Inventing a shorter wait
    /// than the refuser meant would earn another one.
    #[test]
    fn a_refusal_that_names_no_wait_still_names_a_wait() {
        assert_eq!(
            read(&reply(429, r#"{"error":"rate_limited"}"#)),
            RelayOutcome::RateLimited {
                retry_after_secs: 60
            }
        );
    }

    /// A relay that answers what a test tells it to, and remembers what it was
    /// asked — including, deliberately, the bearer, so a test can prove where
    /// the bearer did and did not appear.
    struct FakeRelay {
        replies: std::sync::Mutex<std::collections::VecDeque<RelayReply>>,
        asked: std::sync::Mutex<Vec<(String, String)>>,
        /// Held until a test releases it, for the one property that is about
        /// *when* an answer is given rather than what it says.
        gate: Option<Arc<tokio::sync::Notify>>,
    }

    impl FakeRelay {
        fn answering(replies: Vec<RelayReply>) -> Arc<Self> {
            Arc::new(FakeRelay {
                replies: std::sync::Mutex::new(replies.into()),
                asked: std::sync::Mutex::new(Vec::new()),
                gate: None,
            })
        }
    }

    impl RelayTransport for FakeRelay {
        fn post(
            &self,
            credential: Redacted,
            body: String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RelayReply>> + Send>>
        {
            // The fake exposes it to record it, which is exactly what lets a
            // test assert the bearer reached the transport and stayed out of the
            // body.
            self.asked
                .lock()
                .unwrap()
                .push((credential.expose().to_string(), body));
            let reply = self.replies.lock().unwrap().pop_front();
            let gate = self.gate.clone();
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                reply.ok_or_else(|| anyhow::Error::msg("the relay was asked one time too many"))
            })
        }
    }

    /// A registry over one or more devices, recording what the delivery path
    /// told it.
    /// One recorded `correct_environment`: `(device, token, credential, env)`.
    type Correction = (String, String, Option<String>, ApnsEnvironment);

    struct FakeRegistry {
        targets: Vec<PushTarget>,
        /// Each `forget`, as `device:token:credential`, so a test can prove the
        /// whole tuple reached the store rather than the token alone.
        forgotten: std::sync::Mutex<Vec<String>>,
        corrected: std::sync::Mutex<Vec<Correction>>,
        /// What `forget` answers — `false` is "the phone has re-registered".
        was_current: bool,
    }

    impl FakeRegistry {
        fn over(target: PushTarget) -> Arc<Self> {
            Self::over_all(vec![target])
        }

        fn over_all(targets: Vec<PushTarget>) -> Arc<Self> {
            Arc::new(FakeRegistry {
                targets,
                forgotten: std::sync::Mutex::new(Vec::new()),
                corrected: std::sync::Mutex::new(Vec::new()),
                was_current: true,
            })
        }

        fn only(&self) -> PushTarget {
            self.targets.first().expect("one target").clone()
        }
    }

    impl PushRegistry for FakeRegistry {
        fn targets(&self) -> Vec<PushTarget> {
            self.targets.clone()
        }
        /// The per-device answer, read off the same fleet this fake was built
        /// from — one device, not the list, because that is the question.
        fn eligible<'a>(
            &'a self,
            device_id: String,
            agent: protocol::agent::AgentKind,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
            let answer = self
                .targets
                .iter()
                .find(|target| target.device_id == device_id)
                .is_some_and(|target| target.features.supports(&agent));
            Box::pin(std::future::ready(answer))
        }
        fn forget(
            &self,
            device_id: &str,
            refused_token: &str,
            refused_credential: Option<&Redacted>,
            _reason: &str,
        ) -> bool {
            let credential = refused_credential
                .map(|c| c.expose().to_string())
                .unwrap_or_else(|| "<none>".into());
            self.forgotten
                .lock()
                .unwrap()
                .push(format!("{device_id}:{refused_token}:{credential}"));
            self.was_current
        }
        fn correct_environment(
            &self,
            device_id: &str,
            token: &str,
            credential: Option<&Redacted>,
            environment: ApnsEnvironment,
        ) {
            self.corrected.lock().unwrap().push((
                device_id.to_string(),
                token.to_string(),
                credential.map(|c| c.expose().to_string()),
                environment,
            ));
        }
    }

    async fn attempt(relay: Arc<FakeRelay>, registry: Arc<FakeRegistry>) -> Result<Option<String>> {
        let target = registry.only();
        let document = test_document(&target);
        RelayPushSender::deliver(relay, registry, target, document).await
    }

    /// **The correction the binding is authoritative for is written down.**
    ///
    /// A second Mac that still believes the old environment delivers anyway —
    /// the relay addresses Apple by its binding, not by what it was told — and
    /// this is the only way it ever learns the truth. Without the persist the
    /// correction would reach exactly the Mac that provoked it and no other.
    #[tokio::test]
    async fn an_accepted_answer_that_names_another_environment_corrects_the_row() {
        let registry = FakeRegistry::over(target());
        let relay = FakeRelay::answering(vec![reply(
            200,
            r#"{"outcome":"accepted","environment":"production","apns_id":"A1"}"#,
        )]);
        let apns_id = attempt(Arc::clone(&relay), Arc::clone(&registry))
            .await
            .expect("an accepted push");
        assert_eq!(apns_id.as_deref(), Some("A1"));
        assert_eq!(
            *registry.corrected.lock().unwrap(),
            vec![(
                "phone".to_string(),
                TOKEN.to_string(),
                Some("bearer-value".to_string()),
                ApnsEnvironment::Production
            )],
            "the correction names the whole tuple, so a CAS can refuse a stale one"
        );
        assert!(registry.forgotten.lock().unwrap().is_empty());
    }

    /// An answer that agrees writes nothing. A correction on every accepted
    /// push would be a database write per notification, for no change.
    #[tokio::test]
    async fn an_accepted_answer_that_agrees_corrects_nothing() {
        let registry = FakeRegistry::over(target());
        let relay = FakeRelay::answering(vec![reply(
            200,
            r#"{"outcome":"accepted","environment":"sandbox"}"#,
        )]);
        attempt(Arc::clone(&relay), Arc::clone(&registry))
            .await
            .expect("an accepted push");
        assert!(registry.corrected.lock().unwrap().is_empty());
    }

    /// **A refused credential must not look like a dead phone.** Clearing the
    /// token here would make the phone ask Apple for a new one, which would
    /// change nothing and lose a registration that was perfectly good.
    #[tokio::test]
    async fn a_refused_credential_keeps_the_token_and_says_which_repair_is_needed() {
        for status in [401, 403] {
            let registry = FakeRegistry::over(target());
            let relay =
                FakeRelay::answering(vec![reply(status, r#"{"error":"credential_invalid"}"#)]);
            let err = attempt(Arc::clone(&relay), Arc::clone(&registry))
                .await
                .expect_err("a refused credential is a failed attempt");
            assert!(
                registry.forgotten.lock().unwrap().is_empty(),
                "{status} must not clear the token"
            );
            assert_eq!(
                TestDelivery::from_error(&err),
                TestDelivery::CredentialInvalid
            );
        }
    }

    /// `410` is Apple's word, relayed, and it is terminal for the token — so it
    /// clears the row and ends the worker, exactly as the direct path does.
    #[tokio::test]
    async fn an_unregistered_answer_clears_the_token_and_marks_the_device_gone() {
        let registry = FakeRegistry::over(target());
        let relay = FakeRelay::answering(vec![reply(410, r#"{"error":"unregistered"}"#)]);
        let err = attempt(Arc::clone(&relay), Arc::clone(&registry))
            .await
            .expect_err("a departed device is a failed attempt");
        assert_eq!(
            *registry.forgotten.lock().unwrap(),
            vec![format!("phone:{TOKEN}:bearer-value")],
            "the clear names the whole tuple, never merely the device or the token"
        );
        assert!(
            err.chain().any(|e| e.is::<push_core::DeviceGone>()),
            "the queue retires a worker on this marker and nothing else"
        );
    }

    /// A late `410` about a token the phone has already replaced clears nothing
    /// — and must not retire the worker holding the *new* token's work.
    #[tokio::test]
    async fn a_late_departure_about_a_replaced_token_is_not_marked_gone() {
        let mut registry = FakeRegistry::over(target());
        Arc::get_mut(&mut registry).unwrap().was_current = false;
        let relay = FakeRelay::answering(vec![reply(410, r#"{"error":"unregistered"}"#)]);
        let err = attempt(relay, Arc::clone(&registry))
            .await
            .expect_err("still a failed attempt");
        assert!(
            !err.chain().any(|e| e.is::<push_core::DeviceGone>()),
            "a refusal about a token nobody holds any more is not a departure"
        );
    }

    /// A budget refusal carries the wait the relay named, so the phone can say
    /// when to try again rather than inventing a number.
    #[tokio::test]
    async fn a_budget_refusal_reports_the_wait_the_relay_named() {
        let registry = FakeRegistry::over(target());
        let relay = FakeRelay::answering(vec![RelayReply {
            status: 429,
            retry_after_secs: Some(21),
            body: r#"{"error":"rate_limited"}"#.into(),
        }]);
        let err = attempt(relay, registry)
            .await
            .expect_err("a refusal is a failed attempt");
        assert_eq!(
            TestDelivery::from_error(&err),
            TestDelivery::RateLimited {
                retry_after_secs: 21
            }
        );
    }

    /// **A row with no bearer never reaches the network, and is a *missing
    /// tuple* — not a refusal.** It is a phone that paired while this Mac was in
    /// direct mode, or one that never enrolled: nothing was sent and nothing was
    /// refused, so the plan's table (`docs/push-gateway.md`) calls for
    /// `NoRegisteredToken`, which asks the phone to register. Reporting
    /// `credential_invalid` would name a relay refusal that never happened and
    /// send the phone enrolling for a replacement bearer instead of registering
    /// the one it is missing.
    #[tokio::test]
    async fn a_device_with_no_credential_is_a_missing_tuple_not_a_relay_refusal() {
        let mut bare = target();
        bare.credential = None;
        let registry = FakeRegistry::over(bare);
        let relay = FakeRelay::answering(vec![reply(200, r#"{"outcome":"accepted"}"#)]);
        let err = attempt(Arc::clone(&relay), Arc::clone(&registry))
            .await
            .expect_err("there is nothing to authorise the send");
        assert_eq!(
            TestDelivery::from_error(&err),
            TestDelivery::NoToken,
            "an incomplete registration reports a missing tuple, never a refusal the \
             relay never made"
        );
        assert!(
            relay.asked.lock().unwrap().is_empty(),
            "no request may be composed without a bearer to carry it — the relay is \
             never contacted, so it cannot have refused anything"
        );
        assert!(registry.forgotten.lock().unwrap().is_empty());
    }

    /// The bearer travels in the `Authorization` header and nowhere else. A URL
    /// is logged by every proxy on the path, and a document is stored.
    #[tokio::test]
    async fn the_bearer_authorises_the_request_and_never_appears_in_the_document() {
        let registry = FakeRegistry::over(target());
        let relay = FakeRelay::answering(vec![reply(
            200,
            r#"{"outcome":"accepted","environment":"sandbox"}"#,
        )]);
        attempt(Arc::clone(&relay), registry).await.unwrap();
        let asked = relay.asked.lock().unwrap();
        let (credential, document) = asked.first().expect("one request");
        assert_eq!(credential, "bearer-value");
        assert!(
            !document.contains("bearer-value"),
            "the bearer must not be in the body: {document}"
        );
    }

    /// **`accepted` is not said before the answer arrives.** The relay talks to
    /// Apple and answers afterwards; a sender that reported success on
    /// submission would tell the phone its doorbell works on the strength of a
    /// request nobody had yet replied to.
    #[tokio::test]
    async fn nothing_is_accepted_until_the_relay_has_answered() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let relay = Arc::new(FakeRelay {
            replies: std::sync::Mutex::new(
                vec![reply(
                    200,
                    r#"{"outcome":"accepted","environment":"sandbox"}"#,
                )]
                .into(),
            ),
            asked: std::sync::Mutex::new(Vec::new()),
            gate: Some(Arc::clone(&gate)),
        });
        let registry = FakeRegistry::over(target());
        let sender = RelayPushSender::new(relay, registry);
        let mut waiting = sender.send_test("phone");

        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            assert!(
                waiting.try_recv().is_err(),
                "an outcome was reported before the relay answered"
            );
        }
        gate.notify_waiters();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("the answer must arrive once the relay speaks")
            .expect("the sender must answer whoever asked");
        assert_eq!(outcome, TestDelivery::Accepted { apns_id: None });
    }

    /// A test for a device the daemon holds no token for is answered, not left
    /// on a channel nobody writes to.
    #[tokio::test]
    async fn a_test_for_an_unknown_device_is_answered_rather_than_stranded() {
        let sender = RelayPushSender::new(
            FakeRelay::answering(Vec::new()),
            FakeRegistry::over(target()),
        );
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            sender.send_test("some-other-phone"),
        )
        .await
        .expect("an answer, not a hang")
        .expect("the sender must answer whoever asked");
        assert_eq!(outcome, TestDelivery::NoToken);
    }

    /// **An outage is survived without restarting the daemon.**
    ///
    /// This is the whole reason a boot-time probe is forbidden. The sender holds
    /// no memory of having failed: an unreachable relay produces a failed test
    /// that says so, and the very next test over the same sender succeeds the
    /// moment the service is back. A sender that had latched — or a boot that
    /// had chosen the logging stub because one request failed — would leave push
    /// dead until somebody noticed and restarted `ccd`.
    #[tokio::test]
    async fn an_outage_fails_honestly_and_the_next_test_works_without_a_restart() {
        let relay = FakeRelay::answering(vec![
            reply(503, r#"{"error":"unavailable"}"#),
            reply(200, r#"{"outcome":"accepted","environment":"sandbox"}"#),
        ]);
        let sender = RelayPushSender::new(relay, FakeRegistry::over(target()));
        assert_eq!(
            sender.mode(),
            PushMode::Relay,
            "the mode never depends on whether the relay is answering"
        );

        let during =
            tokio::time::timeout(std::time::Duration::from_secs(5), sender.send_test("phone"))
                .await
                .expect("an answer, not a hang")
                .expect("the sender must answer whoever asked");
        match during {
            TestDelivery::Failed(reason) => assert!(
                reason.contains("unavailable"),
                "the failure must say what happened: {reason}"
            ),
            unexpected => panic!("an outage is a failure with a reason, got {unexpected:?}"),
        }
        assert_eq!(
            sender.mode(),
            PushMode::Relay,
            "and it is still the mode afterwards"
        );

        let after =
            tokio::time::timeout(std::time::Duration::from_secs(5), sender.send_test("phone"))
                .await
                .expect("an answer, not a hang")
                .expect("the sender must answer whoever asked");
        assert_eq!(
            after,
            TestDelivery::Accepted { apns_id: None },
            "recovery costs nothing — no restart, no reconstruction"
        );
    }

    /// **The doorbell path, through the real queue and the real worker.**
    ///
    /// Every other relay test here calls `deliver` directly or goes through
    /// `send_test`; this is the one that proves what a trigger actually
    /// produces — one document per surviving recipient, composed after the
    /// seen-filter has had its say, and none at all for a device whose live
    /// socket already carried the fact.
    #[tokio::test]
    async fn a_doorbell_reaches_every_recipient_the_gate_left_and_no_other() {
        let fleet: Vec<PushTarget> = ["phone-1", "phone-2", "phone-3"]
            .iter()
            .map(|device| PushTarget {
                token: TOKEN.into(),
                environment: ApnsEnvironment::Sandbox,
                device_id: (*device).into(),
                credential: Some(Redacted::from(format!("bearer-{device}"))),
                features: Default::default(),
            })
            .collect();
        let relay = FakeRelay::answering(vec![
            reply(200, r#"{"outcome":"accepted","environment":"sandbox"}"#),
            reply(200, r#"{"outcome":"accepted","environment":"sandbox"}"#),
        ]);
        let sender = RelayPushSender::new(
            Arc::clone(&relay) as Arc<dyn RelayTransport>,
            FakeRegistry::over_all(fleet),
        );

        sender.send(&hint(2), &["phone-2".to_string()]);

        for _ in 0..200 {
            if relay.asked.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let asked = relay.asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "one document per surviving recipient");

        let bearers: Vec<&str> = asked.iter().map(|(bearer, _)| bearer.as_str()).collect();
        assert!(bearers.contains(&"bearer-phone-1"));
        assert!(bearers.contains(&"bearer-phone-3"));
        assert!(
            !bearers.contains(&"bearer-phone-2"),
            "the excluded device's socket already carried this"
        );

        for (_, document) in asked.iter() {
            let document: serde_json::Value = serde_json::from_str(document).unwrap();
            assert_eq!(
                document,
                serde_json::json!({
                    "schema": 1,
                    "token": TOKEN,
                    "environment": "sandbox",
                    "notification": {
                        "type": "doorbell",
                        "kind": "approval",
                        "blocked_count": 2,
                    }
                })
            );
        }
    }

    /// **The relayed doorbell asks the hint's agent, and this is the only test
    /// that can tell.**
    ///
    /// Every assertion about agent eligibility lives beside
    /// [`crate::push_queue::recipients`] and calls it directly, which proves what
    /// the filter answers and nothing about what the sender asks it. There are two
    /// independent senders, so a constant hard-coded at either call site is a
    /// fleet-wide authorization change that every direct test of the filter
    /// survives — and the doorbell test above cannot see it either, because its
    /// hint is Claude and its fleet is the Claude floor throughout. This one raises
    /// a Codex doorbell over a fleet where the answer differs, and reads the
    /// recipients off the requests the relay was actually asked to carry.
    ///
    /// The relay is given exactly as many answers as there are eligible phones:
    /// the count is a claim, not a convenience.
    ///
    /// **The hint is a shape production actually raises**, and it is deliberately
    /// the completed turn rather than the approval. It is no longer the only one —
    /// a Codex approval deck is raisable now, and `describing` keeps the ringing
    /// run's own agent rather than forcing the subject to Claude — so the approval
    /// shape has a matrix of its own
    /// ([`a_codex_approval_doorbell_is_relayed_with_a_count_per_device`]) and the
    /// two doorbell kinds are asserted separately. Keeping this one on the shape
    /// with zero blocked is what makes the pair worth having: a sender that chose
    /// Claude *except* when `blocked_sessions == 0` would pass one of them and
    /// misroute the other.
    ///
    /// **Mutations:** filter on a hard-coded `AgentKind::Claude` at this sender's
    /// call to [`crate::push_queue::recipients`], filter on Claude only when
    /// `blocked_sessions == 0`, or drop the agent filter from `recipients`
    /// altogether, and the two phones that never named Codex join the fan-out.
    #[tokio::test]
    async fn a_codex_doorbell_is_relayed_only_to_the_phones_that_advertised_codex() {
        let phone = |device: &str, features: crate::store::DeviceFeatures| PushTarget {
            token: TOKEN.into(),
            environment: ApnsEnvironment::Sandbox,
            device_id: device.into(),
            credential: Some(Redacted::from(format!("bearer-{device}"))),
            features,
        };
        let advertising = |agents: Vec<protocol::agent::AgentKind>| {
            crate::store::DeviceFeatures::Advertised(protocol::ws::ClientFeatures { agents })
        };
        let claude = protocol::agent::AgentKind::Claude;
        let codex = protocol::agent::AgentKind::Codex;
        let relay = FakeRelay::answering(vec![
            reply(200, r#"{"outcome":"accepted","environment":"sandbox"}"#),
            reply(200, r#"{"outcome":"accepted","environment":"sandbox"}"#),
        ]);
        let sender = RelayPushSender::new(
            Arc::clone(&relay) as Arc<dyn RelayTransport>,
            FakeRegistry::over_all(vec![
                // The legacy shape: advertised nothing at all, which is the Claude
                // floor and exactly what a widened filter would sweep back in.
                phone("claude-floor", Default::default()),
                phone("claude-only", advertising(vec![claude.clone()])),
                phone("codex-only", advertising(vec![codex.clone()])),
                phone("both", advertising(vec![claude, codex.clone()])),
            ]),
        );

        sender.send(
            &PushHint {
                // The production shape, and the only one: a finished Codex turn,
                // nothing blocked. See the note above.
                kind: PushKind::Completed,
                ..codex_hint(0)
            },
            &[],
        );

        for _ in 0..200 {
            if relay.asked.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // **Settled before it is read.** `send` files every recipient in one
        // synchronous pass, so a widened filter puts four workers on the runtime
        // rather than two — and stopping at the first instant two requests exist
        // could read the wrong two and call that a pass.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Sorted, because four workers race and the set of devices is the claim.
        let mut rung: Vec<String> = relay
            .asked
            .lock()
            .unwrap()
            .iter()
            .map(|(bearer, _)| bearer.clone())
            .collect();
        rung.sort();
        assert_eq!(
            rung,
            vec!["bearer-both".to_string(), "bearer-codex-only".to_string()],
            "a Codex doorbell is relayed to exactly the phones that said the word. \
             Either Claude phone appearing here means the sender asked about an agent \
             the run is not, and paid a relay to tell a device about a session it has \
             no screen to open"
        );
    }

    /// **The Codex APPROVAL doorbell over the relay, with a per-device count.**
    ///
    /// Two independent senders carry every doorbell, and a filter narrowed at one
    /// of them is a fleet-wide authorization change the other's tests survive
    /// untouched — which is why the direct sender's approval matrix is not enough
    /// on its own. This is the relay half, and it counts POSTS rather than
    /// queues: the relay fake records one entry per request it is actually asked
    /// to make, so `0` here means no bytes were ever addressed to that phone.
    ///
    /// The counts run 0 → 1 → many over the same fleet. The second doorbell is
    /// sent only once the first has been posted, and that ordering is the claim
    /// rather than a convenience: a doorbell filed while another is still waiting
    /// REPLACES it, on purpose — the phone has one notification slot and the
    /// newer doorbell describes the world better — so two sent back to back are
    /// one buzz and would say nothing about routing. Two decisions, each rung in
    /// its own right, is what "many" honestly means here. The unauthorized phones
    /// stay at `0` through both.
    ///
    /// **This matrix is deliberately thinner than the direct sender's.** It has
    /// the fleet shapes and the counts, and it does not repeat the boundary cases
    /// that are about the fan-out filter itself rather than about this transport —
    /// those live once, beside the filter, in `apns_sender`. What has to be
    /// duplicated here is only what a change to THIS sender could break on its
    /// own: which phones it addresses, and how many times.
    ///
    /// **Mutation:** hard-code `&AgentKind::Claude` at this sender's call to
    /// [`recipients`] and the two Claude phones join, taking the reply deque past
    /// its end and failing the post.
    #[tokio::test]
    async fn a_codex_approval_doorbell_is_relayed_with_a_count_per_device() {
        let phone = |device: &str, features: crate::store::DeviceFeatures| PushTarget {
            token: TOKEN.into(),
            environment: ApnsEnvironment::Sandbox,
            device_id: device.into(),
            credential: Some(Redacted::from(format!("bearer-{device}"))),
            features,
        };
        let advertising = |agents: Vec<protocol::agent::AgentKind>| {
            crate::store::DeviceFeatures::Advertised(protocol::ws::ClientFeatures { agents })
        };
        let claude = protocol::agent::AgentKind::Claude;
        let codex = protocol::agent::AgentKind::Codex;
        // Four accepted replies: two authorized phones, two doorbells each. A
        // fifth request — which is what a widened filter produces — is refused by
        // the fake with "asked one time too many".
        let relay = FakeRelay::answering(
            (0..4)
                .map(|_| reply(200, r#"{"outcome":"accepted","environment":"sandbox"}"#))
                .collect(),
        );
        let sender = RelayPushSender::new(
            Arc::clone(&relay) as Arc<dyn RelayTransport>,
            FakeRegistry::over_all(vec![
                phone("claude-floor", Default::default()),
                phone("claude-only", advertising(vec![claude.clone()])),
                phone("codex-only", advertising(vec![codex.clone()])),
                phone("both", advertising(vec![claude, codex])),
            ]),
        );

        let approval = |uid: &str| PushHint {
            kind: PushKind::Approval,
            session_uid: uid.into(),
            ..codex_hint(1)
        };
        let counts = |expected: usize| {
            let mut per_device: std::collections::BTreeMap<String, usize> = [
                ("bearer-claude-floor", 0usize),
                ("bearer-claude-only", 0),
                ("bearer-codex-only", 0),
                ("bearer-both", 0),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
            for (bearer, _) in relay.asked.lock().unwrap().iter() {
                *per_device.entry(bearer.clone()).or_default() += 1;
            }
            assert_eq!(
                per_device,
                [
                    ("bearer-both".to_string(), expected),
                    ("bearer-claude-floor".to_string(), 0),
                    ("bearer-claude-only".to_string(), 0),
                    ("bearer-codex-only".to_string(), expected),
                ]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>(),
                "a Codex approval doorbell reaches exactly the two phones that said \
                 the word; a Claude phone at anything but zero is a phone paid for \
                 and told about a session it has no screen to open"
            );
        };
        let settle = |target: usize| {
            let relay = Arc::clone(&relay);
            async move {
                for _ in 0..400 {
                    if relay.asked.lock().unwrap().len() >= target {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                // **Settled before it is read.** `send` files every recipient in
                // one synchronous pass, so a widened filter puts four workers on
                // the runtime rather than two — and stopping at the first instant
                // `target` requests exist could read the wrong ones and call that
                // a pass.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
        };

        sender.send(&approval("01K1B3XQ8ZC0DE5FGH7JKMNPQR"), &[]);
        settle(2).await;
        counts(1);

        // **A second doorbell is a second post only once the first has left.**
        // The per-device queue holds one doorbell at a time and a newer one
        // replaces the one waiting, so two filed back to back are one buzz on
        // purpose — the phone has one notification slot and the newer doorbell
        // describes the world better. Waiting for the first to be posted is what
        // makes this "a second approval, later" rather than "the same news
        // twice", and it is the only way the count reaches two.
        sender.send(&approval("01K1B3XQ8ZC0DE5FGH7JKMNPQS"), &[]);
        settle(4).await;
        counts(2);
    }

    /// **A relayed doorbell that waited out a withdrawal is not posted.**
    ///
    /// The relay is the second of two independent senders, and a dequeue-time
    /// check added at one of them is a fleet-wide authorization property the
    /// other's tests survive untouched — which is why the direct sender's twin of
    /// this is not enough on its own. This one goes through the real transport
    /// seam: the relay's own post is HELD, so the worker is genuinely stuck
    /// inside its first attempt while the second doorbell is filed and the
    /// authorization is withdrawn, and `asked` counts the requests the relay was
    /// actually made.
    ///
    /// **Mutation:** pass a gate that always answers `true` from
    /// `RelayPushSender::serve` and the relay is asked a second time, for a phone
    /// that stopped being a recipient while it waited.
    #[tokio::test]
    async fn a_relayed_doorbell_that_waited_out_a_withdrawal_is_not_posted() {
        /// Advertises Codex until a test says otherwise.
        struct Switchable(std::sync::atomic::AtomicBool);
        impl Switchable {
            /// What this phone can render right now — the one fact both answers
            /// are built from, so the fan-out list and the per-device re-check
            /// cannot disagree about the switch.
            fn features(&self) -> crate::store::DeviceFeatures {
                if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                    crate::store::DeviceFeatures::Advertised(protocol::ws::ClientFeatures {
                        agents: vec![protocol::agent::AgentKind::Codex],
                    })
                } else {
                    Default::default()
                }
            }
        }
        impl PushRegistry for Switchable {
            fn targets(&self) -> Vec<PushTarget> {
                vec![PushTarget {
                    token: TOKEN.into(),
                    environment: ApnsEnvironment::Sandbox,
                    device_id: "waiting-phone".into(),
                    credential: Some(Redacted::from("bearer-waiting-phone")),
                    features: self.features(),
                }]
            }
            fn eligible<'a>(
                &'a self,
                device_id: String,
                agent: protocol::agent::AgentKind,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>
            {
                let answer = device_id == "waiting-phone" && self.features().supports(&agent);
                Box::pin(std::future::ready(answer))
            }
            fn forget(
                &self,
                _device_id: &str,
                _refused_token: &str,
                _refused_credential: Option<&Redacted>,
                _reason: &str,
            ) -> bool {
                unreachable!("the relay below accepts; nothing is refused")
            }
            fn correct_environment(
                &self,
                _device_id: &str,
                _token: &str,
                _credential: Option<&Redacted>,
                _environment: ApnsEnvironment,
            ) {
                unreachable!("the relay below accepts; no environment is corrected")
            }
        }

        let hold = Arc::new(tokio::sync::Notify::new());
        // Two replies are stocked. If only one delivery is posted the second is
        // never taken, and if two are the count says so — the fake refuses a
        // third, so a wider filter fails loudly rather than silently.
        let relay = Arc::new(FakeRelay {
            replies: std::sync::Mutex::new(
                (0..2)
                    .map(|_| reply(200, r#"{"outcome":"accepted","environment":"sandbox"}"#))
                    .collect(),
            ),
            asked: std::sync::Mutex::new(Vec::new()),
            gate: Some(Arc::clone(&hold)),
        });
        let registry = Arc::new(Switchable(std::sync::atomic::AtomicBool::new(true)));
        let sender = RelayPushSender::new(
            Arc::clone(&relay) as Arc<dyn RelayTransport>,
            Arc::clone(&registry) as Arc<dyn PushRegistry>,
        );

        let approval = |uid: &str| PushHint {
            kind: PushKind::Approval,
            session_uid: uid.into(),
            ..codex_hint(1)
        };

        sender.send(&approval("01K1B3XQ8ZC0DE5FGH7JKMNPQR"), &[]);
        // The worker has taken the first delivery and is parked inside the
        // relay's post, which is what makes the rest of this deterministic.
        for _ in 0..400 {
            if relay.asked.lock().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(relay.asked.lock().unwrap().len(), 1, "the premise");

        // A second doorbell, filed while the phone is STILL eligible.
        sender.send(&approval("01K1B3XQ8ZC0DE5FGH7JKMNPQS"), &[]);
        // The world moves while it waits its turn.
        registry.0.store(false, std::sync::atomic::Ordering::SeqCst);

        hold.notify_waiters();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(
            relay.asked.lock().unwrap().len(),
            1,
            "the doorbell that waited out the withdrawal must never be posted; \
             authorization is read at the dequeue, not only at the fan-out"
        );
    }

    /// A fleet with nobody left after the gate composes nothing at all.
    #[tokio::test]
    async fn a_doorbell_everyone_has_already_seen_reaches_no_one() {
        let relay = FakeRelay::answering(Vec::new());
        let sender = RelayPushSender::new(
            Arc::clone(&relay) as Arc<dyn RelayTransport>,
            FakeRegistry::over(target()),
        );
        sender.send(&hint(1), &["phone".to_string()]);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(relay.asked.lock().unwrap().is_empty());
    }

    /// **The endpoint is baked, and no file the user can write moves it.**
    ///
    /// The document a push carries includes the device's APNs token, so a relay
    /// URL that a config file could set would be a switch redirecting every
    /// notification this Mac sends to a stranger — writable by anything that can
    /// write in the home directory. `docs/push-gateway.md` §9 names public relay
    /// URL configuration as a launch non-goal, and this is that rule asserted
    /// rather than intended: `Config` has no such field, so a file naming one
    /// parses cleanly and is simply not read.
    #[test]
    fn the_endpoint_is_baked_and_a_config_file_cannot_move_it() {
        let production = HttpsRelay::new().expect("the production endpoint builds");
        assert_eq!(production.host, RELAY_HOST);
        assert_eq!(production.port, 443);

        let named_elsewhere: protocol::config::Config = serde_json::from_str(
            r#"{"relay_url":"https://not-ours.example",
                "push_relay_url":"https://not-ours.example",
                "push_url":"https://not-ours.example",
                "apns":{"url":"https://not-ours.example"}}"#,
        )
        .expect("an unknown key is ignored, as in every config this daemon reads");
        assert!(
            named_elsewhere.push_enabled,
            "the file said nothing this daemon acts on"
        );
        assert_eq!(
            HttpsRelay::new().expect("still builds").host,
            RELAY_HOST,
            "and the endpoint is where it always was"
        );

        // The seam that exists so a test can aim the transport somewhere it
        // controls. It is `#[cfg(test)]`, so it is not in a shipped binary at
        // all — there is no release-build path, public or private, that reaches
        // it.
        let elsewhere = HttpsRelay::at("127.0.0.1".into(), 1).expect("the test seam builds");
        assert_eq!(elsewhere.host, "127.0.0.1");
        assert_eq!(elsewhere.port, 1);
    }

    /// **The request the production transport actually builds**, rather than
    /// the one a fake was handed.
    ///
    /// This is the only code that decides which header carries the bearer, which
    /// URL is dialled and which method is used, and every other test on this
    /// path goes through a fake that is given the two values already separated.
    #[test]
    fn the_production_request_puts_the_bearer_in_the_header_and_nowhere_else() {
        let bearer = Redacted::from("a-relay-bearer");
        let request = relay_request(RELAY_HOST, &bearer).expect("a request");
        assert_eq!(request.method(), http::Method::POST);
        assert_eq!(
            request.uri().to_string(),
            format!("https://{RELAY_HOST}/v1/push"),
            "the versioned endpoint, and the host nothing can move"
        );
        assert!(
            !request.uri().to_string().contains("a-relay-bearer"),
            "a bearer in a URL is a bearer in every proxy log on the path"
        );
        assert_eq!(
            request
                .headers()
                .get(http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer a-relay-bearer")
        );
        assert_eq!(
            request
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    /// **A credential that could forge a header cannot reach the wire**, and it
    /// is refused twice: `checked_credential` rejects it at registration, and
    /// `http`'s own builder rejects it here — so a row written by an older build
    /// still cannot inject one.
    #[test]
    fn a_header_forging_credential_is_refused_by_the_builder_as_well() {
        for hostile in ["line\r\nAuthorization: Bearer other", "with\nnewline"] {
            assert!(
                relay_request(RELAY_HOST, &Redacted::from(hostile)).is_err(),
                "the builder accepted {hostile:?}"
            );
        }
    }

    /// **A relay that is not there fails inside its bounds and says so.** This is
    /// the one test that uses the real transport over a real socket — through the
    /// test-only endpoint seam — so the connect leg is exercised rather than
    /// reasoned about.
    #[tokio::test]
    async fn a_relay_that_refuses_the_connection_fails_honestly_and_promptly() {
        // Bind and drop, so the port is one nothing is listening on.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let relay = HttpsRelay::at("127.0.0.1".into(), port).expect("the transport builds");
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(
            DELIVERY_DEADLINE,
            relay.post("a-relay-bearer".into(), "{}".into()),
        )
        .await
        .expect("it must fail well inside the delivery deadline")
        .expect_err("nothing is listening");
        assert!(
            started.elapsed() < CONNECT_TIMEOUT,
            "a refused connection is immediate, not a timeout: {:?}",
            started.elapsed()
        );
        let reason = format!("{err:#}");
        assert!(
            reason.contains("127.0.0.1"),
            "the failure must name what it could not reach: {reason}"
        );
        assert!(
            !reason.contains("a-relay-bearer"),
            "and must not carry the bearer into a log line: {reason}"
        );
    }

    /// **The answer is a closed set too.**
    ///
    /// §4 fixes what the relay may be *told*; nothing fixed what it may say
    /// back, and the refusal word travels straight into this daemon's log and
    /// onto the user's screen beside a failed test. A relay — or anything that
    /// got between this Mac and one, which §Decision 5 lists as a live risk —
    /// could otherwise write whatever it liked into both. A word that is not one
    /// the relay's own schema can produce is dropped, and the status stands
    /// alone.
    #[test]
    fn a_word_the_relay_could_not_have_said_is_not_repeated() {
        for hostile in [
            "your account is suspended, visit http://not-ours.example",
            "\n\nWARNING: ",
            &"x".repeat(4000),
            "",
        ] {
            let body = serde_json::json!({ "error": hostile }).to_string();
            match read(&reply(500, &body)) {
                RelayOutcome::Refused(why) => {
                    assert_eq!(
                        why, "the relay answered 500",
                        "only the status may be repeated, got {why:?}"
                    );
                }
                other => panic!("a 500 is a plain refusal, got {other:?}"),
            }
        }

        // And every word it genuinely can say survives, so the closed set is a
        // filter rather than a gag.
        for word in RELAY_WORDS {
            let body = serde_json::json!({ "error": word }).to_string();
            match read(&reply(500, &body)) {
                RelayOutcome::Refused(why) => assert!(why.contains(word), "{why}"),
                other => panic!("{word}: {other:?}"),
            }
        }
    }

    /// Apple's receipt is rendered next to a successful test, so it is bounded
    /// and restricted on the same reasoning as the refusal word.
    #[test]
    fn a_receipt_that_is_not_a_receipt_is_dropped() {
        for hostile in [
            "not a uuid: click http://not-ours.example",
            &"a".repeat(65),
            "with space",
            "line\nbreak",
            "",
        ] {
            let body = serde_json::json!({
                "outcome": "accepted",
                "environment": "sandbox",
                "apns_id": hostile,
            })
            .to_string();
            assert_eq!(
                read(&reply(200, &body)),
                RelayOutcome::Accepted {
                    apns_id: None,
                    environment: Some(ApnsEnvironment::Sandbox)
                },
                "accepted the receipt {hostile:?}"
            );
        }

        let real = serde_json::json!({
            "outcome": "accepted",
            "environment": "sandbox",
            "apns_id": "8B2A6F1C-3D4E-4F50-9A1B-2C3D4E5F6071",
        })
        .to_string();
        assert_eq!(
            read(&reply(200, &real)),
            RelayOutcome::Accepted {
                apns_id: Some("8B2A6F1C-3D4E-4F50-9A1B-2C3D4E5F6071".into()),
                environment: Some(ApnsEnvironment::Sandbox)
            }
        );
    }

    /// **An unreadable answer corrects nothing.** `ApnsEnvironment::parse` is
    /// lenient by design — anything it does not recognise is `sandbox` — and
    /// using it here would let a truncated or garbled `200` overwrite a correct
    /// production binding and silently stop a phone ringing.
    #[test]
    fn an_unreadable_environment_is_no_environment_at_all() {
        for body in [
            r#"{"outcome":"accepted"}"#,
            r#"{"outcome":"accepted","environment":"Production"}"#,
            r#"{"outcome":"accepted","environment":"prod"}"#,
            r#"{"outcome":"accepted","environment":7}"#,
            "not json at all",
            "",
        ] {
            assert_eq!(
                read(&reply(200, body)),
                RelayOutcome::Accepted {
                    apns_id: None,
                    environment: None
                },
                "{body}"
            );
        }
    }
}
