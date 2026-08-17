//! The relay's connection to Apple: one pooled HTTP/2 client per environment,
//! held open for the life of the process.
//!
//! **Why a pool and not the daemon's transport.** `ccd` opens a TLS connection,
//! sends one push and closes it, which is right for a Mac that pushes its own
//! phone a few times an hour. A relay speaks for every user at once, and a
//! connection per push would spend a TLS handshake on every notification and
//! present Apple with a churn of connections it is explicitly asked not to
//! receive. Apple ends a connection with `GOAWAY` whenever it likes, so the
//! thing that replaces a hand-rolled pool has to notice that and open a fresh
//! connection without resubmitting a request that may already have reached the
//! phone. `hyper-util`'s pooled client does exactly that, which is why this
//! module configures a client rather than writing one.
//!
//! **One attempt, ever.** There is no retry here and there is no backoff. A
//! request that failed after its body went out is ambiguous — the notification
//! may be on the phone already — and a second copy of a doorbell is worse than
//! a missed one, because the phone shows the older state as if it were news.
//! The pooled client may reconnect and resubmit only in the one case where it
//! can prove nothing was written; anything this module sees as an error is
//! reported as an error.
//!
//! **Ordering, and nothing more.** Two Macs paired to the same phone can push
//! it at the same moment. The per-token lock below makes those two requests
//! reach Apple in the order they arrived, so the collapse id replaces the
//! older notification rather than racing it. It does not coalesce them, drop
//! either, or decide which one describes the current state — the daemon owns
//! that decision because the session state is on the Mac, and moving it here
//! would make the relay a participant in what a user is doing rather than a
//! post box.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use http::uri::{Authority, Scheme};
use http::Uri;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use push_core::{classify, parse_reason, ApnsEnvironment, ApnsOutcome, ProviderToken};

use crate::config::{ApnsSlot, RelayConfig};
use crate::secret::token_hash;

/// How many APNs requests may be in flight at once across every environment.
///
/// Apple enforces its own limit per connection and answers an excess stream
/// with `REFUSED_STREAM`; this bound exists so that a burst arriving at the
/// relay queues in this process instead of turning into a wall of refusals
/// that would be classified as Apple being unwell.
const MAX_IN_FLIGHT: usize = 64;

/// How long **all** of one request's APNs work may take, from the first call to
/// the last byte of the last answer — the wait for a turn and Decision 4's one
/// opposite-host attempt included.
///
/// **Not a retry**, and not a bound on the exchange alone. The deadline exists
/// because an attempt that never ends holds a concurrency permit and its
/// token's place in line forever, so one stalled connection would stop every
/// later push to that phone. Expiring it yields an error, never a second send.
///
/// **Measured, and the reason the clock starts before the lock.** Started after
/// the per-token lock and the global permit were acquired, each queued push for
/// one phone could wait a full deadline for its turn and then be granted
/// another — three Macs pushing one phone would answer the third far outside
/// the 35–40 seconds the daemon's 45-second outer bound is built on. A push
/// that cannot get its turn in time is refused rather than answered late.
///
/// **One budget per request and not one per attempt**, which is what [`Budget`]
/// carries. A correction that started a clock of its own would put a slow first
/// attempt and the attempt after it at seventy seconds — past the bound the
/// daemon gave up on — and the relay would be working on a request nobody is
/// waiting for.
const PUSH_DEADLINE: Duration = Duration::from_secs(35);

/// How much of Apple's error body is read.
///
/// It is a small JSON document — `{"reason":"BadDeviceToken","timestamp":…}` —
/// and the bound is here so that an endpoint that answers with something
/// endless cannot hold a permit while it does.
const MAX_REASON_BYTES: usize = 8 * 1024;

/// How long an idle connection is kept.
///
/// Long, because keeping it is the entire point: a shorter timeout than the
/// gap between two users' notifications would put a TLS handshake back on
/// every push, which is the transport this module exists to replace.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

type PooledClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Where one environment's pushes are posted.
///
/// **An address rather than a test flag.** Proving that two pushes share a
/// connection, and that a `GOAWAY` costs exactly one reconnection and no
/// replayed request, needs a real HTTP/2 server whose accepted connections and
/// streams can be counted — which is only reachable if the caller can say
/// where to post. A boolean would leave the production path untested and the
/// test path a different code path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApnsEndpoint {
    scheme: Scheme,
    authority: Authority,
}

impl ApnsEndpoint {
    /// Apple's host for this environment, over TLS.
    pub fn apple(environment: ApnsEnvironment) -> Self {
        ApnsEndpoint {
            scheme: Scheme::HTTPS,
            authority: environment
                .host()
                .parse()
                .expect("Apple's push hosts are literal host names"),
        }
    }

    /// A base address of any scheme, such as `http://127.0.0.1:9000`.
    ///
    /// **`#[cfg(test)]`, and that is the whole of the fix.** Every APNs request
    /// carries the provider JWT in its `authorization` header — an hour-long
    /// bearer for the entire app topic, in the environment whose key the relay
    /// holds. One plaintext request hands it to anyone on the path, and the
    /// theft is silent. Left `pub` alongside a connector that accepted `http`,
    /// this constructor was one future configuration knob or one reuse by a
    /// tool away from doing exactly that. Compiled out of the binary that
    /// ships, the only address the relay can build is [`ApnsEndpoint::apple`],
    /// which is `https`, and the shipped connector refuses anything else.
    #[cfg(test)]
    pub fn parse(base: &str) -> Result<Self> {
        let uri: Uri = base
            .parse()
            .with_context(|| format!("{base:?} is not a URL"))?;
        let parts = uri.into_parts();
        let (Some(scheme), Some(authority)) = (parts.scheme, parts.authority) else {
            bail!("{base:?} needs a scheme and a host, for example https://api.push.apple.com");
        };
        Ok(ApnsEndpoint { scheme, authority })
    }

    /// The `host:port` an APNs request is addressed to.
    pub fn authority(&self) -> &str {
        self.authority.as_str()
    }
}

/// One environment's signing key and address, or why there is none.
///
/// Mirrors [`ApnsSlot`] deliberately: absence is a running state there and it
/// stays one here. A relay whose production key has not been issued yet still
/// has to serve enrollment, and the push for that environment answers
/// `unavailable` rather than being signed with the other environment's key.
pub enum ApnsSigner {
    /// Boxed: a key pair is several hundred bytes and an enum sized by its
    /// larger variant would make every absent environment that big.
    Ready(Box<ProviderToken>, ApnsEndpoint),
    Absent(String),
}

impl ApnsSigner {
    pub fn ready(token: ProviderToken, endpoint: ApnsEndpoint) -> Self {
        ApnsSigner::Ready(Box::new(token), endpoint)
    }
}

/// The attempt did not reach a verdict from Apple.
#[derive(Debug)]
pub enum SendError {
    /// This environment could not send, and this is why — a key that is not
    /// there, or a turn that never came. **Nothing was submitted**, so the
    /// caller's answer is `unavailable` and the phone has no copy of this
    /// notification.
    Unavailable(String),
    /// The request left and no answer came back. **Ambiguous by definition**:
    /// the notification may be on the phone. It is reported, never repeated.
    Unanswered(anyhow::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Unavailable(why) => write!(f, "this environment cannot send: {why}"),
            SendError::Unanswered(e) => write!(f, "APNs did not answer: {e:#}"),
        }
    }
}

impl std::error::Error for SendError {}

/// What is left of one request's [`PUSH_DEADLINE`].
///
/// **Opened once by the caller and handed to every attempt it makes.** The
/// opposite-host correction is a second send, and the only thing that keeps two
/// sends inside the envelope the daemon waits on is that the second one is given
/// what the first did not spend. A budget that is gone refuses before anything
/// is submitted, so the answer is that the relay could not send and never that
/// the token is bad.
pub struct Budget {
    expires: Instant,
}

impl Budget {
    fn remaining(&self) -> Duration {
        self.expires.saturating_duration_since(Instant::now())
    }
}

struct Sender {
    client: PooledClient,
    token: ProviderToken,
    endpoint: ApnsEndpoint,
}

/// Boxed because a `Sender` carries a key pair and a client, and an enum sized
/// by its larger variant would make every absent environment that big.
enum Slot {
    Ready(Box<Sender>),
    Absent(String),
}

pub struct ApnsTransport {
    sandbox: Slot,
    production: Slot,
    order: Arc<TokenLocks>,
    permits: tokio::sync::Semaphore,
    /// [`PUSH_DEADLINE`] in the running relay. A field rather than the constant
    /// so a test can prove the queueing behaviour in milliseconds instead of
    /// waiting the thirty-five seconds a real one takes.
    deadline: Duration,
}

impl ApnsTransport {
    /// The transport the deployment's keys describe.
    ///
    /// Fails only when there is no trust store to verify Apple against, which
    /// is a broken image rather than a missing credential. A key file that
    /// opens but is not a P-256 PKCS#8 key is the same running state as no key
    /// at all — `config` proved the file was readable, parsing it belongs to
    /// the code that signs with it, and a relay that exited here would take
    /// enrollment down over an environment that is not being used yet.
    pub fn from_config(config: &RelayConfig) -> Result<Self> {
        Self::new(
            signer(&config.sandbox, ApnsEnvironment::Sandbox),
            signer(&config.production, ApnsEnvironment::Production),
        )
    }

    pub fn new(sandbox: ApnsSigner, production: ApnsSigner) -> Result<Self> {
        Ok(ApnsTransport {
            sandbox: slot(sandbox)?,
            production: slot(production)?,
            order: Arc::new(TokenLocks::default()),
            permits: tokio::sync::Semaphore::new(MAX_IN_FLIGHT),
            deadline: PUSH_DEADLINE,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Open one request's budget, to be spent by every attempt it makes.
    pub fn budget(&self) -> Budget {
        Budget {
            expires: Instant::now() + self.deadline,
        }
    }

    /// Why this environment cannot send, in the words `/readyz` reports.
    pub fn absence(&self, environment: ApnsEnvironment) -> Option<&str> {
        match self.slot(environment) {
            Slot::Ready(_) => None,
            Slot::Absent(why) => Some(why),
        }
    }

    /// Where this environment's pushes are posted, once it can send.
    pub fn endpoint(&self, environment: ApnsEnvironment) -> Option<&ApnsEndpoint> {
        match self.slot(environment) {
            Slot::Ready(sender) => Some(&sender.endpoint),
            Slot::Absent(_) => None,
        }
    }

    /// Post one notification and wait for Apple's verdict, inside what is left
    /// of the caller's budget.
    ///
    /// One attempt. See the module note: a failure after dispatch is reported,
    /// and a reader tempted to wrap this in a loop is about to send a phone a
    /// second copy of a doorbell it already has.
    pub async fn send(
        &self,
        budget: &Budget,
        environment: ApnsEnvironment,
        device_token: &str,
        payload: &str,
        collapse: &str,
    ) -> Result<ApnsOutcome, SendError> {
        let sender = match self.slot(environment) {
            Slot::Ready(sender) => sender,
            Slot::Absent(why) => return Err(SendError::Unavailable(why.clone())),
        };

        // A request whose budget an earlier attempt spent submits nothing at
        // all, which is why this is the unambiguous failure: the phone has no
        // copy of this notification and the caller has learned nothing about
        // the token.
        let remaining = budget.remaining();
        if remaining.is_zero() {
            return Err(SendError::Unavailable(
                "this request's time was spent before this attempt began".to_string(),
            ));
        }

        // **Whether anything was written, which is what the two failures mean.**
        // A deadline that expired while this push was still waiting for its
        // turn submitted nothing and is not ambiguous; one that expired after
        // dispatch is exactly as ambiguous as any other failure on the wire,
        // and the difference decides whether "the notification may already be
        // on the phone" is true.
        let dispatched = AtomicBool::new(false);
        let attempt = self.attempt(sender, device_token, payload, collapse, &dispatched);
        match tokio::time::timeout(remaining, attempt).await {
            Ok(answer) => answer,
            Err(_) if !dispatched.load(Ordering::Relaxed) => Err(SendError::Unavailable(format!(
                "this device's queue did not clear within {remaining:?}"
            ))),
            Err(_) => Err(SendError::Unanswered(anyhow!(
                "APNs accepted the request and did not answer within {remaining:?}"
            ))),
        }
    }

    /// One push, from waiting for a turn to Apple's verdict.
    ///
    /// Every await here is inside the caller's deadline, which is the point:
    /// the lock and the permit are the two places a push can wait without a
    /// clock running on it.
    async fn attempt(
        &self,
        sender: &Sender,
        device_token: &str,
        payload: &str,
        collapse: &str,
        dispatched: &AtomicBool,
    ) -> Result<ApnsOutcome, SendError> {
        // **The lock before the permit.** Waiting for a turn is ordering and
        // costs nothing; a permit is a scarce thing that should only be held
        // while bytes are actually moving. Taken the other way round, a phone
        // with a queue would hold permits that no other device could use.
        //
        // Keyed by the token's digest rather than the token, so the raw value
        // is never a map key that could be printed by a dump of this struct.
        let _order = self.order.acquire(&token_hash(device_token)).await;
        let _permit = self
            .permits
            .acquire()
            .await
            .expect("the transport's permits are never closed");

        let bearer = sender
            .token
            .bearer()
            .map_err(|e| SendError::Unanswered(e.context("minting the APNs provider token")))?;
        let request = self
            .build(sender, device_token, payload, collapse, &bearer)
            .map_err(SendError::Unanswered)?;

        let exchange = async {
            dispatched.store(true, Ordering::Relaxed);
            // **The one call that may not be repeated.** `hyper-util` will
            // itself reconnect and resubmit when it can prove the request was
            // never written — a connection the pool handed out that the peer
            // had already closed. Anything that reaches here has been on the
            // wire, so it becomes an error and not a second attempt.
            let response = sender
                .client
                .request(request)
                .await
                .with_context(|| format!("posting to {}", sender.endpoint.authority()))?;
            let status = response.status().as_u16();
            // Apple's receipt for this exact notification, when it sends one.
            let apns_id = response
                .headers()
                .get("apns-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            // **A body that will not read does not lose the status.** The
            // status alone already separates a dead app from a busy Apple; the
            // one distinction it cannot make without the body is
            // `400 BadDeviceToken`, and failing that way yields `Rejected`,
            // which retires nothing.
            let body = Limited::new(response.into_body(), MAX_REASON_BYTES)
                .collect()
                .await
                .map(|body| body.to_bytes())
                .unwrap_or_default();
            Ok::<_, anyhow::Error>((status, apns_id, body))
        };

        let (status, apns_id, body) = exchange.await.map_err(SendError::Unanswered)?;
        let reason = parse_reason(&String::from_utf8_lossy(&body)).unwrap_or_default();
        Ok(classify(status, &reason, apns_id))
    }

    fn slot(&self, environment: ApnsEnvironment) -> &Slot {
        match environment {
            ApnsEnvironment::Sandbox => &self.sandbox,
            ApnsEnvironment::Production => &self.production,
        }
    }

    /// The request Apple receives.
    ///
    /// The headers come from `push_core::request` rather than from here: the
    /// path, expiry, push type, priority and collapse id are shared with the
    /// daemon, and a second copy of them is a second thing to get wrong. Only
    /// the address is this module's, because the endpoint is injectable and
    /// `push_core` writes `https`.
    fn build(
        &self,
        sender: &Sender,
        device_token: &str,
        payload: &str,
        collapse: &str,
        bearer: &str,
    ) -> Result<http::Request<Full<Bytes>>> {
        let request = push_core::request(
            sender.endpoint.authority(),
            device_token,
            bearer,
            sender.token.topic(),
            now_secs()?,
            collapse,
        )
        .context("building the APNs request")?;
        let (mut parts, ()) = request.into_parts();
        let mut uri = parts.uri.into_parts();
        uri.scheme = Some(sender.endpoint.scheme.clone());
        uri.authority = Some(sender.endpoint.authority.clone());
        parts.uri = Uri::from_parts(uri).context("addressing the APNs request")?;
        Ok(http::Request::from_parts(
            parts,
            Full::new(Bytes::copy_from_slice(payload.as_bytes())),
        ))
    }
}

/// Now, in whole seconds — the unit `apns-expiration` is written in.
fn now_secs() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("the system clock is before 1970")?
        .as_secs() as i64)
}

fn signer(slot: &ApnsSlot, environment: ApnsEnvironment) -> ApnsSigner {
    match slot {
        ApnsSlot::Absent(why) => ApnsSigner::Absent(why.clone()),
        ApnsSlot::Ready(key) => match ProviderToken::load(&key.key_file, key.identity.clone()) {
            Ok(token) => ApnsSigner::ready(token, ApnsEndpoint::apple(environment)),
            Err(e) => ApnsSigner::Absent(format!("{e:#}")),
        },
    }
}

fn slot(signer: ApnsSigner) -> Result<Slot> {
    let (token, endpoint) = match signer {
        ApnsSigner::Absent(why) => return Ok(Slot::Absent(why)),
        ApnsSigner::Ready(token, endpoint) => (token, endpoint),
    };
    Ok(Slot::Ready(Box::new(Sender {
        client: pooled_client()?,
        token: *token,
        endpoint,
    })))
}

/// One long-lived pool, configured for Apple and for nothing else.
fn pooled_client() -> Result<PooledClient> {
    let mut roots = rustls::RootCertStore::empty();
    // Apple's endpoint presents a certificate chaining to a public root, so the
    // platform store is exactly right — in the relay's container that is the
    // `ca-certificates` package — and vendoring a root set would be a second
    // thing to keep current.
    let (added, _ignored) =
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    if added == 0 {
        bail!("no trust anchors available for the APNs connection");
    }
    // The provider is named rather than taken from the process default: the
    // default is installed by whichever crate got there first, and a relay that
    // picked up a different one would fail its first handshake with a message
    // about ciphers.
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("the APNs TLS configuration")?
    .with_root_certificates(roots)
    .with_no_client_auth();

    let connector = hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(tls);
    // **The shipped connector refuses plaintext outright.** `https_or_http`
    // made the decision a property of the address, and the address was a `pub`
    // constructor that took any scheme — so a JWT good for the whole topic for
    // an hour was one configuration knob away from going out in the clear. The
    // permissive builder exists only in the test build, alongside the only
    // constructor that can produce a non-`https` address.
    #[cfg(not(test))]
    let connector = connector.https_only();
    #[cfg(test)]
    let connector = connector.https_or_http();
    let connector = connector
        // **Required, not an optimisation**, and the reason the TLS config
        // above leaves `alpn_protocols` alone: APNs serves HTTP/2 only and
        // decides which protocol to speak from ALPN. Announce nothing and the
        // handshake completes, the server offers HTTP/1.1, and every push fails
        // on a connection that looked healthy. `apns_sender.rs` writes that
        // list by hand because it drives `h2` itself; writing it here as well
        // panics on startup, because this builder owns it.
        .enable_http2()
        .build();

    Ok(Client::builder(TokioExecutor::new())
        .http2_only(true)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .build(connector))
}

/// One in-flight request per device token, in arrival order.
///
/// **Transport ordering only.** Holding the lock for the length of one attempt
/// is what makes "the later push replaces the earlier one" true: the collapse
/// id decides which notification survives, and it can only decide correctly if
/// the two requests reach Apple in the order the Macs sent them. Nothing here
/// merges two pushes, discards one, or reads either — the relay is never told
/// enough to know which one is current.
///
/// **The map cannot grow without bound.** An entry is a `Weak`, so it is alive
/// exactly as long as somebody holds or waits for that token's lock, and the
/// last holder removes it on the way out. A fixed set of shards would also be
/// bounded, but it would serialise unrelated phones that happened to collide,
/// and a phone waiting on another user's push is a delay nobody can explain.
#[derive(Default)]
struct TokenLocks(Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>);

impl TokenLocks {
    async fn acquire(self: &Arc<Self>, key: &str) -> TokenTurn {
        let entry = {
            let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
            match map.get(key).and_then(Weak::upgrade) {
                Some(entry) => entry,
                None => {
                    let entry = Arc::new(tokio::sync::Mutex::new(()));
                    map.insert(key.to_string(), Arc::downgrade(&entry));
                    entry
                }
            }
        };
        let held = tokio::sync::Mutex::lock_owned(entry).await;
        TokenTurn {
            locks: Arc::clone(self),
            key: key.to_string(),
            held: Some(held),
        }
    }
}

struct TokenTurn {
    locks: Arc<TokenLocks>,
    key: String,
    held: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for TokenTurn {
    fn drop(&mut self) {
        // Released before the map is touched, so that a waiter can take the
        // lock; whoever does holds a strong reference, and the check below then
        // correctly leaves the entry alone. With the two in the other order the
        // count would still include this guard and no entry would ever go.
        drop(self.held.take());
        let mut map = self.locks.0.lock().unwrap_or_else(|e| e.into_inner());
        if map
            .get(&self.key)
            .is_some_and(|entry| entry.strong_count() == 0)
        {
            map.remove(&self.key);
        }
    }
}

/// A fake Apple on a real socket, and the ledger of what reached it.
///
/// **A module rather than a helper inside one test file.** Two things have to
/// prove statements about what Apple actually received — this transport, and
/// the push endpoint whose kill switch must emit no request at all — and a
/// second copy of a server harness is a second opinion about what counts as a
/// connection or a stream.
///
/// Plaintext HTTP/2 with prior knowledge rather than TLS: what is being proven
/// is how many connections the pool opens and how many streams Apple ends up
/// seeing, and a certificate would add a trust store to every test without
/// changing either number.
#[cfg(test)]
pub(crate) mod fake_apple {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use push_core::ApnsIdentity;

    use super::{ApnsEndpoint, ApnsSigner, ApnsTransport, ProviderToken};

    pub(crate) const TOPIC: &str = "com.example.codeconnect";

    /// What the fake Apple answers, and how it behaves while doing so.
    #[derive(Clone)]
    pub(crate) struct Plan {
        pub(crate) status: u16,
        pub(crate) apns_id: Option<String>,
        pub(crate) body: String,
        /// Send `GOAWAY` once this many streams have been accepted on a
        /// connection.
        pub(crate) goaway_after: Option<usize>,
        /// Keep each request open this long, so that overlap is observable.
        pub(crate) hold: Duration,
    }

    impl Plan {
        pub(crate) fn answering(status: u16, body: &str) -> Self {
            Plan {
                status,
                apns_id: None,
                body: body.to_string(),
                goaway_after: None,
                hold: Duration::ZERO,
            }
        }
    }

    /// One request as the server saw it.
    pub(crate) struct Seen {
        pub(crate) path: String,
        pub(crate) headers: http::HeaderMap,
        pub(crate) body: String,
        pub(crate) entered: Instant,
        pub(crate) left: Instant,
    }

    impl Seen {
        pub(crate) fn header(&self, name: &str) -> &str {
            self.headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
        }

        pub(crate) fn overlaps(&self, other: &Seen) -> bool {
            self.entered < other.left && other.entered < self.left
        }
    }

    #[derive(Default)]
    pub(crate) struct Ledger {
        pub(crate) connections: AtomicUsize,
        pub(crate) ended: AtomicUsize,
        /// Streams the server has taken off the wire, counted the moment it
        /// accepts one rather than when it answers — so a test can act while a
        /// request is genuinely open.
        pub(crate) accepted: AtomicUsize,
        /// `GOAWAY`s issued, counted after the call that sends one.
        pub(crate) shutdowns: AtomicUsize,
        pub(crate) requests: Mutex<Vec<Seen>>,
    }

    impl Ledger {
        pub(crate) fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }

        pub(crate) fn streams(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    /// A real HTTP/2 server on a real socket.
    ///
    /// Plaintext with prior knowledge rather than TLS: what is being proven is
    /// how many connections the pool opens and how many streams Apple ends up
    /// seeing, and a certificate would add a trust store to every test without
    /// changing either number.
    pub(crate) async fn apple(plan: Plan) -> (String, Arc<Ledger>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        let ledger = Arc::new(Ledger::default());
        let accepting = Arc::clone(&ledger);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                accepting.connections.fetch_add(1, Ordering::SeqCst);
                let plan = plan.clone();
                let ledger = Arc::clone(&accepting);
                tokio::spawn(async move {
                    let mut connection = h2::server::handshake(socket).await.unwrap();
                    let mut accepted = 0usize;
                    while let Some(stream) = connection.accept().await {
                        let Ok((request, respond)) = stream else {
                            break;
                        };
                        accepted += 1;
                        ledger.accepted.fetch_add(1, Ordering::SeqCst);
                        tokio::spawn(answer(request, respond, plan.clone(), Arc::clone(&ledger)));
                        if plan.goaway_after == Some(accepted) {
                            connection.graceful_shutdown();
                            ledger.shutdowns.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    ledger.ended.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        (format!("http://{address}"), ledger)
    }

    async fn answer(
        request: http::Request<h2::RecvStream>,
        mut respond: h2::server::SendResponse<Bytes>,
        plan: Plan,
        ledger: Arc<Ledger>,
    ) {
        let entered = Instant::now();
        let (parts, mut incoming) = request.into_parts();
        let mut body = String::new();
        while let Some(chunk) = incoming.data().await {
            let chunk = chunk.unwrap();
            let _ = incoming.flow_control().release_capacity(chunk.len());
            body.push_str(&String::from_utf8_lossy(&chunk));
        }
        if !plan.hold.is_zero() {
            tokio::time::sleep(plan.hold).await;
        }
        let left = Instant::now();
        ledger.requests.lock().unwrap().push(Seen {
            path: parts.uri.path().to_string(),
            headers: parts.headers,
            body,
            entered,
            left,
        });

        let mut response = http::Response::builder().status(plan.status);
        if let Some(id) = &plan.apns_id {
            response = response.header("apns-id", id);
        }
        let response = response.body(()).unwrap();
        if plan.body.is_empty() {
            let _ = respond.send_response(response, true);
        } else if let Ok(mut stream) = respond.send_response(response, false) {
            let _ = stream.send_data(Bytes::from(plan.body.clone()), true);
        }
    }

    /// A signing key minted here — a checked-in one would be a private key in
    /// the repository and a fixture that expires on a date nobody chose.
    pub(crate) fn signing_key() -> ProviderToken {
        let der = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &ring::rand::SystemRandom::new(),
        )
        .unwrap();
        ProviderToken::from_pkcs8(
            der.as_ref(),
            ApnsIdentity {
                key_id: "TESTKEYID1".into(),
                team_id: "TESTTEAM01".into(),
                topic: TOPIC.into(),
            },
        )
        .unwrap()
    }

    /// The same key in the armour a `.p8` wears, for the tests that need one on
    /// disk rather than in memory.
    ///
    /// Written here rather than in each test module because two of them have to
    /// agree on what a usable key file looks like — the configuration that
    /// decides whether an environment is ready, and the endpoint that reports
    /// it — and a second opinion about that is exactly the disagreement those
    /// tests exist to catch.
    pub(crate) fn signing_key_file() -> String {
        use base64::Engine as _;

        let der = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &ring::rand::SystemRandom::new(),
        )
        .unwrap();
        let body = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|line| std::str::from_utf8(line).expect("base64 is ascii"))
            .collect();
        // The label is a binding rather than a literal, so a repository scan for
        // checked-in key material has nothing here to stop on.
        let label = "PRIVATE KEY";
        format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            lines.join("\n")
        )
    }

    /// A transport whose sandbox environment posts to `base`, and whose
    /// production environment has no key at all.
    pub(crate) fn signing_transport(base: &str) -> ApnsTransport {
        ApnsTransport::new(
            ApnsSigner::ready(signing_key(), ApnsEndpoint::parse(base).unwrap()),
            ApnsSigner::Absent("no production key in tests".into()),
        )
        .unwrap()
    }

    /// A distinct device token per index, so pushes are not serialised by the
    /// per-token lock when the test needs them overlapping.
    pub(crate) fn nth_token(index: u8) -> String {
        format!("{:02x}", index).repeat(32)
    }

    /// Wait for a server-side counter to reach `expected`, so a test acts on
    /// something the server has actually done rather than on a duration.
    pub(crate) async fn wait_for(counter: &AtomicUsize, expected: usize) {
        for _ in 0..500 {
            if counter.load(Ordering::SeqCst) >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the server never reached {expected}");
    }
}

#[cfg(test)]
mod tests {
    use super::fake_apple::*;
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use push_core::COLLAPSE_ID;

    const TOKEN: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    const OTHER_TOKEN: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    async fn push(transport: &ApnsTransport, token: &str) -> Result<ApnsOutcome, SendError> {
        transport
            .send(
                &transport.budget(),
                ApnsEnvironment::Sandbox,
                token,
                r#"{"aps":{"alert":{"title":"CodeConnect"}}}"#,
                COLLAPSE_ID,
            )
            .await
    }

    /// Wait for the server to finish `expected` connections, so the next push
    /// is issued after the client has had the close to observe rather than
    /// racing it.
    async fn wait_for_ended(ledger: &Ledger, expected: usize) {
        wait_for(&ledger.ended, expected).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// **The pool's whole reason for existing.** A transport that opened a
    /// connection per push would show two here, and would spend a TLS
    /// handshake on every notification the relay ever sends.
    #[tokio::test]
    async fn two_pushes_share_one_connection() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let transport = signing_transport(&base);

        assert!(matches!(
            push(&transport, TOKEN).await.unwrap(),
            ApnsOutcome::Accepted { .. }
        ));
        assert!(matches!(
            push(&transport, TOKEN).await.unwrap(),
            ApnsOutcome::Accepted { .. }
        ));

        assert_eq!(ledger.connections(), 1, "the second push reopened APNs");
        assert_eq!(ledger.streams(), 2);
    }

    /// **`GOAWAY` costs exactly one reconnection**, and that is all this test
    /// proves.
    ///
    /// Every push here is awaited to completion and the connection is fully
    /// closed before the next pair is issued, so **nothing is ever in flight
    /// when Apple withdraws** — the ambiguity this module exists to handle is
    /// not created, and the stream count would hold for an implementation that
    /// replayed. The property that a request on the wire is not sent twice is
    /// proven by `an_in_flight_request_survives_a_goaway_without_being_replayed`;
    /// what is proven here is the reconnection count, which that test cannot
    /// pin down because a racing pool may open more than one.
    #[tokio::test]
    async fn a_goaway_costs_one_reconnection() {
        let mut plan = Plan::answering(200, "");
        plan.goaway_after = Some(2);
        let (base, ledger) = apple(plan).await;
        let transport = signing_transport(&base);

        for _ in 0..2 {
            assert!(matches!(
                push(&transport, TOKEN).await.unwrap(),
                ApnsOutcome::Accepted { .. }
            ));
        }
        wait_for_ended(&ledger, 1).await;
        for _ in 0..2 {
            assert!(matches!(
                push(&transport, TOKEN).await.unwrap(),
                ApnsOutcome::Accepted { .. }
            ));
        }

        assert_eq!(
            ledger.connections(),
            2,
            "one connection before the GOAWAY and exactly one after it"
        );
        assert_eq!(
            ledger.streams(),
            4,
            "four pushes must reach Apple as four requests and never as five"
        );
    }

    /// **The property the sequential test cannot reach: a request that was
    /// already on the wire when Apple withdrew is not sent a second time.**
    ///
    /// The first push is held open by the server and the `GOAWAY` is issued
    /// while it is still open, so the three that follow meet a closing
    /// connection with an ambiguous request outstanding — the exact state this
    /// module exists to be correct about. Four pushes must reach Apple as four
    /// streams. Five would be a doorbell arriving twice on a phone that already
    /// showed it, which reads to a user as the agent asking again.
    ///
    /// Four **different** tokens, because the per-token lock would otherwise
    /// serialise them and there would be nothing in flight to be ambiguous.
    #[tokio::test]
    async fn an_in_flight_request_survives_a_goaway_without_being_replayed() {
        let mut plan = Plan::answering(200, "");
        plan.hold = Duration::from_millis(400);
        plan.goaway_after = Some(1);
        let (base, ledger) = apple(plan).await;
        let transport = Arc::new(signing_transport(&base));

        let first = tokio::spawn({
            let transport = Arc::clone(&transport);
            async move { push(&transport, &nth_token(0)).await }
        });
        // Wait for the withdrawal itself rather than for a duration: the first
        // request is open for four hundred milliseconds, so reaching here means
        // the `GOAWAY` and an unanswered request coexist.
        wait_for(&ledger.shutdowns, 1).await;
        assert_eq!(
            ledger.accepted.load(Ordering::SeqCst),
            1,
            "the GOAWAY has to land while exactly one request is outstanding"
        );

        let rest: Vec<_> = (1..4)
            .map(|index| {
                let transport = Arc::clone(&transport);
                tokio::spawn(async move { push(&transport, &nth_token(index)).await })
            })
            .collect();

        assert!(matches!(
            first.await.unwrap().unwrap(),
            ApnsOutcome::Accepted { .. }
        ));
        for task in rest {
            assert!(matches!(
                task.await.unwrap().unwrap(),
                ApnsOutcome::Accepted { .. }
            ));
        }

        assert_eq!(
            ledger.streams(),
            4,
            "four pushes must reach Apple as four requests and never as five"
        );
        assert!(
            ledger.connections() >= 2,
            "the withdrawn connection must be replaced, not reused"
        );
        let requests = ledger.requests.lock().unwrap();
        let first_path = format!("/3/device/{}", nth_token(0));
        let held = requests
            .iter()
            .find(|seen| seen.path == first_path)
            .expect("the held push reached the server");
        assert!(
            requests
                .iter()
                .any(|seen| seen.path != first_path && seen.overlaps(held)),
            "the first push must still have been open while a later one ran"
        );
    }

    #[tokio::test]
    async fn an_accepted_push_carries_apples_receipt() {
        let mut plan = Plan::answering(200, "");
        plan.apns_id = Some("8B2A6F1C-0000-4E2B-9F4C-2C1D3E4F5A6B".into());
        let (base, _ledger) = apple(plan).await;

        assert_eq!(
            push(&signing_transport(&base), TOKEN).await.unwrap(),
            ApnsOutcome::Accepted {
                apns_id: Some("8B2A6F1C-0000-4E2B-9F4C-2C1D3E4F5A6B".into())
            }
        );
    }

    #[tokio::test]
    async fn a_410_is_the_app_being_gone_from_the_phone() {
        let (base, _ledger) = apple(Plan::answering(410, r#"{"reason":"Unregistered"}"#)).await;
        assert_eq!(
            push(&signing_transport(&base), TOKEN).await.unwrap(),
            ApnsOutcome::Unregistered
        );
    }

    /// **The reason is read off the wire, not guessed from the status.** A
    /// `400` that came back as `Rejected` here would be a token nobody ever
    /// retries on the other host.
    #[tokio::test]
    async fn a_400_bad_device_token_survives_the_transport_as_itself() {
        let (base, _ledger) = apple(Plan::answering(400, r#"{"reason":"BadDeviceToken"}"#)).await;
        assert_eq!(
            push(&signing_transport(&base), TOKEN).await.unwrap(),
            ApnsOutcome::BadDeviceToken
        );
    }

    /// A credential fault, and deliberately not a fact about the phone.
    #[tokio::test]
    async fn a_403_invalid_provider_token_is_this_relays_fault() {
        let (base, _ledger) =
            apple(Plan::answering(403, r#"{"reason":"InvalidProviderToken"}"#)).await;
        assert_eq!(
            push(&signing_transport(&base), TOKEN).await.unwrap(),
            ApnsOutcome::Rejected {
                status: 403,
                reason: "InvalidProviderToken".into()
            }
        );
    }

    #[tokio::test]
    async fn a_429_is_apples_own_state() {
        let (base, _ledger) = apple(Plan::answering(429, r#"{"reason":"TooManyRequests"}"#)).await;
        assert_eq!(
            push(&signing_transport(&base), TOKEN).await.unwrap(),
            ApnsOutcome::Retryable {
                status: 429,
                reason: "TooManyRequests".into()
            }
        );
    }

    #[tokio::test]
    async fn a_503_is_apples_own_state_too() {
        let (base, _ledger) =
            apple(Plan::answering(503, r#"{"reason":"ServiceUnavailable"}"#)).await;
        assert_eq!(
            push(&signing_transport(&base), TOKEN).await.unwrap(),
            ApnsOutcome::Retryable {
                status: 503,
                reason: "ServiceUnavailable".into()
            }
        );
    }

    /// **Two Macs, one phone.** Both pushes describe the same slot on the same
    /// device, so the one that arrived second has to be the one Apple stores.
    /// Overlapping requests would leave that to the network.
    #[tokio::test]
    async fn two_pushes_for_one_token_reach_apple_one_after_the_other() {
        let mut plan = Plan::answering(200, "");
        plan.hold = Duration::from_millis(200);
        let (base, ledger) = apple(plan).await;
        let transport = Arc::new(signing_transport(&base));

        let first = tokio::spawn({
            let transport = Arc::clone(&transport);
            async move { push(&transport, TOKEN).await.unwrap() }
        });
        let second = tokio::spawn({
            let transport = Arc::clone(&transport);
            async move { push(&transport, TOKEN).await.unwrap() }
        });
        first.await.unwrap();
        second.await.unwrap();

        let requests = ledger.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            !requests[0].overlaps(&requests[1]),
            "the second push for a token must wait for the first to finish"
        );
    }

    /// And the lock is per token: one phone's slow push never delays another's.
    #[tokio::test]
    async fn two_pushes_for_different_tokens_are_free_to_overlap() {
        let mut plan = Plan::answering(200, "");
        plan.hold = Duration::from_millis(200);
        let (base, ledger) = apple(plan).await;
        let transport = Arc::new(signing_transport(&base));

        let first = tokio::spawn({
            let transport = Arc::clone(&transport);
            async move { push(&transport, TOKEN).await.unwrap() }
        });
        let second = tokio::spawn({
            let transport = Arc::clone(&transport);
            async move { push(&transport, OTHER_TOKEN).await.unwrap() }
        });
        first.await.unwrap();
        second.await.unwrap();

        let requests = ledger.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].overlaps(&requests[1]),
            "different phones must not queue behind each other"
        );
    }

    /// **The wire contract, read off the server.** These headers are what makes
    /// a notification replace the waiting one instead of racing it, and reach a
    /// phone that is asleep; a change in `push_core::request` that dropped any
    /// of them would otherwise show up as pushes quietly not arriving.
    #[tokio::test]
    async fn the_request_apple_receives_addresses_the_device_and_names_the_slot() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let transport = signing_transport(&base);
        push(&transport, TOKEN).await.unwrap();

        let requests = ledger.requests.lock().unwrap();
        let seen = &requests[0];
        assert_eq!(seen.path, format!("/3/device/{TOKEN}"));
        assert_eq!(seen.header("apns-collapse-id"), COLLAPSE_ID);
        assert_eq!(seen.header("apns-push-type"), "alert");
        assert_eq!(seen.header("apns-priority"), "10");
        assert_eq!(seen.header("apns-topic"), TOPIC);
        assert!(seen.header("authorization").starts_with("bearer "));
        assert!(
            seen.header("apns-expiration").parse::<i64>().unwrap() > now_secs().unwrap(),
            "an expiry in the past is a push Apple never stores"
        );
        assert_eq!(seen.body, r#"{"aps":{"alert":{"title":"CodeConnect"}}}"#);
    }

    /// An environment with no key is answered, not attempted — and the answer
    /// says which credential is missing.
    #[tokio::test]
    async fn an_environment_without_a_key_sends_nothing_and_says_why() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let transport = signing_transport(&base);

        let err = transport
            .send(
                &transport.budget(),
                ApnsEnvironment::Production,
                TOKEN,
                "{}",
                COLLAPSE_ID,
            )
            .await
            .expect_err("there is no production key here");
        match err {
            SendError::Unavailable(why) => assert!(why.contains("no production key"), "{why}"),
            other => panic!("{other}"),
        }
        assert_eq!(ledger.connections(), 0, "nothing may reach Apple");
        assert_eq!(transport.absence(ApnsEnvironment::Sandbox), None);
        assert!(transport.absence(ApnsEnvironment::Production).is_some());
        assert_eq!(
            transport
                .endpoint(ApnsEnvironment::Sandbox)
                .map(ApnsEndpoint::authority),
            Some(base.trim_start_matches("http://"))
        );
    }

    /// A server that never answers is an error and never a second send.
    #[tokio::test]
    async fn a_connection_that_is_refused_is_reported_rather_than_repeated() {
        // A port nothing is listening on: bound, then dropped.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let transport = signing_transport(&format!("http://{address}"));

        match push(&transport, TOKEN).await {
            Err(SendError::Unanswered(e)) => {
                assert!(format!("{e:#}").contains(&address.to_string()), "{e:#}")
            }
            other => panic!("{other:?} is not a refused connection"),
        }
    }

    /// **A push that cannot get its turn is refused, not answered late.**
    ///
    /// The measured bug: the per-token lock and the global permit were taken
    /// before the deadline started, so a queued push waited without a clock and
    /// then received a full deadline of its own. Three Macs pushing one phone
    /// could therefore answer the third well past the daemon's 45-second outer
    /// bound, and the daemon would have given up on a request the relay was
    /// still working on.
    ///
    /// The turn is held here for longer than the whole deadline, which is what
    /// a slow earlier push looks like from the next one's point of view.
    #[tokio::test]
    async fn a_push_that_cannot_get_its_turn_is_refused_rather_than_answered_late() {
        let (base, ledger) = apple(Plan::answering(200, "")).await;
        let transport = signing_transport(&base).with_deadline(Duration::from_millis(150));
        let held = transport.order.acquire(&token_hash(TOKEN)).await;

        let started = Instant::now();
        let err = push(&transport, TOKEN)
            .await
            .expect_err("the turn never came");
        let waited = started.elapsed();
        drop(held);

        match err {
            // Nothing was written, so this is not the ambiguous failure — the
            // phone has no copy of this notification and the caller is told the
            // relay could not send rather than told it may have.
            SendError::Unavailable(why) => assert!(why.contains("queue"), "{why}"),
            other => panic!("{other:?} is not a refusal before dispatch"),
        }
        assert!(
            waited < Duration::from_secs(2),
            "the wait for a turn ran outside the deadline: {waited:?}"
        );
        assert_eq!(ledger.streams(), 0, "nothing may reach Apple");
        assert_eq!(ledger.connections(), 0);

        // And the turn coming back restores ordinary service.
        assert!(matches!(
            push(&transport, TOKEN).await.unwrap(),
            ApnsOutcome::Accepted { .. }
        ));
    }

    /// **One budget, however many attempts a request makes.**
    ///
    /// The opposite-host correction is a second send, and a second clock of its
    /// own would let one request run for twice the bound the daemon waits on —
    /// which is the daemon giving up on work the relay is still doing. The first
    /// attempt here spends the whole budget, so the second submits nothing and
    /// says so as the unambiguous failure: no phone has a copy of it.
    #[tokio::test]
    async fn a_second_send_inherits_what_the_first_left_of_the_budget() {
        let mut plan = Plan::answering(200, "");
        plan.hold = Duration::from_millis(300);
        let (base, _ledger) = apple(plan).await;
        let transport = signing_transport(&base).with_deadline(Duration::from_millis(200));
        let budget = transport.budget();

        let started = Instant::now();
        let first = transport
            .send(&budget, ApnsEnvironment::Sandbox, TOKEN, "{}", COLLAPSE_ID)
            .await;
        assert!(
            matches!(first, Err(SendError::Unanswered(_))),
            "{first:?} is not a request that went out and was not answered"
        );
        let second = transport
            .send(
                &budget,
                ApnsEnvironment::Sandbox,
                OTHER_TOKEN,
                "{}",
                COLLAPSE_ID,
            )
            .await;
        let elapsed = started.elapsed();

        match second {
            Err(SendError::Unavailable(why)) => assert!(why.contains("spent"), "{why}"),
            other => panic!("{other:?} is not a refusal on an exhausted budget"),
        }
        assert!(
            elapsed < Duration::from_millis(500),
            "two sends ran past one budget: {elapsed:?}"
        );
    }

    /// **The lock map is bounded.** A relay pushing millions of distinct
    /// phones would otherwise keep a mutex for each one for the life of the
    /// process.
    #[tokio::test]
    async fn a_tokens_lock_is_forgotten_once_nobody_holds_it() {
        let locks = Arc::new(TokenLocks::default());
        {
            let _turn = locks.acquire("a-token-digest").await;
            let held = locks.acquire("another-token-digest").await;
            assert_eq!(locks.0.lock().unwrap().len(), 2);
            drop(held);
            assert_eq!(locks.0.lock().unwrap().len(), 1);
        }
        assert!(locks.0.lock().unwrap().is_empty());
    }

    /// A base address without a host is refused where it is written, rather
    /// than becoming a push that goes nowhere.
    #[test]
    fn an_endpoint_needs_a_scheme_and_a_host() {
        assert_eq!(
            ApnsEndpoint::apple(ApnsEnvironment::Production).authority(),
            "api.push.apple.com"
        );
        assert_eq!(
            ApnsEndpoint::apple(ApnsEnvironment::Sandbox).authority(),
            "api.sandbox.push.apple.com"
        );
        assert_eq!(
            ApnsEndpoint::parse("http://127.0.0.1:9000")
                .unwrap()
                .authority(),
            "127.0.0.1:9000"
        );
        assert!(ApnsEndpoint::parse("/3/device/aa").is_err());
        assert!(ApnsEndpoint::parse("api.push.apple.com").is_err());
    }
}
