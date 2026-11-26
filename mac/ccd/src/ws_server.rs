//! Tailnet WebSocket server.
//!
//! **Gap-free replay is the load-bearing property.** The broadcast receiver is
//! created when the connection opens, *before* any backlog read. So for any
//! event E appended during a replay: either E was already in the store when we
//! read it (sent from the backlog), or it arrives on the broadcast afterwards.
//! A per-session watermark drops anything at or below what we already sent, so
//! the client sees every seq exactly once, in order, with no gap.
//!
//! When a client falls off the broadcast ring entirely we do not skip: we emit
//! an explicit `Resync` marker and re-read from the store. A gap the client
//! cannot detect is the one failure that makes an event log worthless.
//!
//! Transport is `wss://` when `tailscale cert` gave us a certificate and
//! `ws://` otherwise, **on the same port**. The first byte of a connection
//! decides which (see [`is_tls_hello`]): a TLS record always starts `0x16`, and
//! an HTTP request never does. That avoids a flag day between a daemon and a
//! phone app that ship independently, and `tls_required` is the switch for
//! turning plaintext off once every client has moved.
//!
//! Tailnet membership is not authentication, so the bearer token is checked on
//! every connection regardless of scheme.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use protocol::event::{Event, EventKind, Source};
use protocol::peer::PeerRequest;
use protocol::secret::Redacted;
use protocol::ws::{
    AnsweredOn, Capabilities, ClientMessage, ServerMessage, MAX_CLIENT_MESSAGE_BYTES,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

use crate::state::{AuthOutcome, Daemon};

/// Backlog page size. Bounds memory when a phone reconnects after a long trip.
const REPLAY_PAGE: u32 = 500;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_secs(30);
/// How often a paired device's `peers` frame is read again; it is sent only when
/// it changed.
const PEERS_REFRESH: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(60)
};
/// The longest a single frame may take to reach the peer before the connection
/// is judged dead. Without it a peer that stops reading its socket wedges the
/// very `send` that a revocation, a teardown, or a keepalive would ride, and
/// the connection can never be closed. A healthy peer drains in milliseconds;
/// this bounds the pathological case to a fixed, generous window.
const WRITE_DEADLINE: Duration = Duration::from_secs(20);

/// The write deadline in force, so a test can drive the give-up path without
/// waiting the production twenty seconds. Zero (the default) means "use the
/// constant".
#[cfg(test)]
static TEST_WRITE_DEADLINE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn write_deadline() -> Duration {
    #[cfg(test)]
    {
        let ms = TEST_WRITE_DEADLINE_MS.load(std::sync::atomic::Ordering::Relaxed);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    WRITE_DEADLINE
}

/// Whether plaintext (`ws://`) is admissible on a listener, and whether it is
/// *private*. Decided once at bind time from what the listener's address **is**
/// — never guessed per connection from where a peer claims to be.
///
/// The daemon's credential is a bearer token and a connection carries the
/// event log and the approval path, so the wire must be unreadable in
/// transit. That holds three ways: TLS, loopback (the bytes
/// never leave the machine), or this node's own Tailscale address (WireGuard
/// encrypts the path before it touches a network). Any other address —
/// an explicit `ws_bind` onto a LAN, say — gets TLS or nothing.
///
/// **Admissible and private are two questions, and they came apart when
/// `ws_allow_plaintext` was added.** That key lets an operator serve plaintext
/// on a LAN they vouch for, which is a decision about their network and *not* a
/// claim that the bytes are unreadable on it. Collapsing the two — the opt-in
/// used to answer [`Self::TrustedPath`] — meant every answer about privacy
/// silently included the one configuration where there is none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaintextTrust {
    /// Loopback or this node's own tailnet address: plaintext is already
    /// private in transit.
    TrustedPath,
    /// A LAN the operator vouched for with `ws_allow_plaintext`: plaintext is
    /// admitted, on their instruction, and is exactly as private as that
    /// network.
    OperatorAllowed,
    /// Anything else: plaintext would cross an unknown network in the clear,
    /// credential and all.
    RequireTls,
}

impl PlaintextTrust {
    /// Is a *plaintext* connection on this listener unreadable in transit?
    ///
    /// What the `private_transport` capability says of a connection without TLS.
    pub fn is_private(self) -> bool {
        matches!(self, PlaintextTrust::TrustedPath)
    }
}

/// Classify one listener address against this node's tailnet addresses.
pub fn plaintext_trust(bind: IpAddr, tailnet: &[IpAddr]) -> PlaintextTrust {
    if bind.is_loopback() || tailnet.contains(&bind) {
        PlaintextTrust::TrustedPath
    } else {
        PlaintextTrust::RequireTls
    }
}

pub async fn serve(
    daemon: Arc<Daemon>,
    addr: SocketAddr,
    token: Arc<String>,
    tls: Option<TlsAcceptor>,
    trust: PlaintextTrust,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let plaintext_refused = daemon.config.tls_required || trust == PlaintextTrust::RequireTls;
    match (&tls, plaintext_refused) {
        (Some(_), true) => crate::log_info!("ws listening on wss://{addr} (plaintext refused)"),
        (Some(_), false) => crate::log_info!("ws listening on wss://{addr} (ws:// also accepted)"),
        (None, false) => crate::log_info!("ws listening on ws://{addr}"),
        // Said once, loudly, at startup. Otherwise the only evidence is a
        // per-connection debug line, and the operator sees a phone that cannot
        // connect with nothing in the log at the level they are running.
        (None, true) => crate::log_error!(
            "ws listening on {addr} but REFUSING EVERY CONNECTION: plaintext is not private \
             on this address and no certificate is available. Provide a certificate, or bind \
             loopback or this node's tailnet address."
        ),
    }

    accept_loop(daemon, listener, token, tls, trust).await
}

/// The accept loop, separated from the bind so a test can hand it a listener on
/// an ephemeral port and speak the real protocol to it.
async fn accept_loop(
    daemon: Arc<Daemon>,
    listener: TcpListener,
    token: Arc<String>,
    tls: Option<TlsAcceptor>,
    trust: PlaintextTrust,
) -> Result<()> {
    let limiter = Arc::new(ConnectionLimiter::new(
        daemon.config.ws_max_connections,
        daemon.config.ws_max_per_peer,
    ));

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                // Admission happens *before* the spawn, not inside it. A task
                // that spawns and then waits for a permit has already cost the
                // file descriptor and the task, which is the whole resource the
                // cap exists to protect.
                let Some(permit) = limiter.admit(peer.ip()) else {
                    crate::log_warn!(
                        "ws: refusing {peer}; at the connection limit ({} global, {} per peer)",
                        daemon.config.ws_max_connections,
                        daemon.config.ws_max_per_peer,
                    );
                    // Dropped, which sends RST/FIN immediately. A refused peer
                    // learns nothing it did not already know, and holding the
                    // socket open to say so would defeat the cap.
                    drop(stream);
                    continue;
                };
                let daemon = Arc::clone(&daemon);
                let token = Arc::clone(&token);
                let tls = tls.clone();
                tokio::spawn(async move {
                    // Released when this task ends, whatever ended it.
                    let _permit = permit;
                    if let Err(err) = accept(daemon, stream, peer.ip(), token, tls, trust).await {
                        crate::log_debug!("ws {peer} ended: {err:#}");
                    }
                });
            }
            Err(err) => {
                crate::log_error!("ws accept failed: {err}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// A global connection cap with a per-peer share.
///
/// Both halves are needed and neither is sufficient. Without the global cap,
/// anything that can reach the tailnet port holds the daemon's entire
/// file-descriptor budget open — and the *local* IPC socket, which carries the
/// CLI, lives on the same budget. Without the
/// per-peer share, one misbehaving client reaches the global cap by itself and
/// every other device is refused.
///
/// A plain `Semaphore` would give the global half only, so this is a counter
/// map behind one short synchronous lock: admission is a few integer
/// comparisons on a map with as many entries as there are live peers.
struct ConnectionLimiter {
    global: usize,
    per_peer: usize,
    live: std::sync::Mutex<LiveConnections>,
}

#[derive(Default)]
struct LiveConnections {
    total: usize,
    by_peer: HashMap<std::net::IpAddr, usize>,
}

/// Holds one slot. Releasing on `Drop` is what makes the accounting correct
/// under every exit path — a returned error, a panic in the connection task, a
/// cancelled future — rather than only the one the author remembered.
struct ConnectionPermit {
    limiter: Arc<ConnectionLimiter>,
    peer: std::net::IpAddr,
}

impl ConnectionLimiter {
    fn new(global: usize, per_peer: usize) -> ConnectionLimiter {
        ConnectionLimiter {
            global,
            per_peer,
            live: std::sync::Mutex::new(LiveConnections::default()),
        }
    }

    fn admit(self: &Arc<Self>, peer: std::net::IpAddr) -> Option<ConnectionPermit> {
        let mut live = self.live.lock().unwrap_or_else(|p| p.into_inner());
        if live.total >= self.global {
            return None;
        }
        let count = live.by_peer.entry(peer).or_insert(0);
        if *count >= self.per_peer {
            // Leaves a zero entry behind when this was the peer's first
            // attempt; `release` prunes it, and a zero costs one map slot until
            // then.
            return None;
        }
        *count += 1;
        live.total += 1;
        Some(ConnectionPermit {
            limiter: Arc::clone(self),
            peer,
        })
    }

    fn release(&self, peer: std::net::IpAddr) {
        let mut live = self.live.lock().unwrap_or_else(|p| p.into_inner());
        live.total = live.total.saturating_sub(1);
        if let Some(count) = live.by_peer.get_mut(&peer) {
            *count = count.saturating_sub(1);
            // Pruned rather than left at zero: the map is keyed by remote
            // address, and a long-running daemon would otherwise accumulate one
            // entry per address that ever connected.
            if *count == 0 {
                live.by_peer.remove(&peer);
            }
        }
    }

    #[cfg(test)]
    fn live_total(&self) -> usize {
        self.live.lock().unwrap_or_else(|p| p.into_inner()).total
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.limiter.release(self.peer);
    }
}

/// Decide the scheme from the first byte, then hand off to the same client loop.
async fn accept(
    daemon: Arc<Daemon>,
    stream: TcpStream,
    peer: IpAddr,
    token: Arc<String>,
    tls: Option<TlsAcceptor>,
    trust: PlaintextTrust,
) -> Result<()> {
    // Nagle costs latency on the small JSON frames this protocol is made of.
    let _ = stream.set_nodelay(true);

    // One rule for every plaintext path below: the operator's `tls_required`,
    // or a listener address on which plaintext would cross a network in the
    // clear. Enforced before the WebSocket handshake, so a refused connection
    // never reaches the credential exchange at all.
    let plaintext_refused = daemon.config.tls_required || trust == PlaintextTrust::RequireTls;

    let Some(acceptor) = tls else {
        if plaintext_refused {
            // Refusing is the honest outcome: silently serving plaintext would
            // contradict what the operator asked for, what the listener's
            // address can keep private, and what `capabilities.tls` reports.
            anyhow::bail!("plaintext is refused on this listener and no certificate is available");
        }
        return handle_client(daemon, stream, peer, token, false, trust).await;
    };

    // `peek` reads without consuming, so the byte is still there for the TLS
    // handshake. A stream that closes before its first byte is just a probe.
    let mut first = [0u8; 1];
    let peeked = tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.peek(&mut first))
        .await
        .context("timed out waiting for the first byte")?
        .context("peeking the first byte")?;
    if peeked == 0 {
        return Ok(());
    }

    if is_tls_hello(first[0]) {
        let stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream))
            .await
            .context("tls handshake timed out")?
            .context("tls handshake failed")?;
        return handle_client(daemon, stream, peer, token, true, trust).await;
    }

    if plaintext_refused {
        crate::log_warn!("ws: refused a plaintext connection (wss is required on this listener)");
        return Ok(());
    }
    handle_client(daemon, stream, peer, token, false, trust).await
}

/// `0x16` is the TLS `handshake` content type. No HTTP request method begins
/// with that byte, so one byte separates the two protocols unambiguously.
fn is_tls_hello(first: u8) -> bool {
    first == 0x16
}

async fn handle_client<S>(
    daemon: Arc<Daemon>,
    stream: S,
    // Where the connection came from: the one thing another computer's daemon is
    // known by (`protocol::peer`).
    peer: IpAddr,
    token: Arc<String>,
    tls_active: bool,
    trust: PlaintextTrust,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Whether *this* connection's bytes are unreadable in transit: TLS, or a
    // listener address on which plaintext already is. Not the
    // `ws_allow_plaintext` LAN, where the operator vouched for their network
    // and not for the bytes on it. The ack reports it as `private_transport`.
    let private_transport = tls_active || trust.is_private();
    let config = WebSocketConfig {
        max_message_size: Some(MAX_CLIENT_MESSAGE_BYTES),
        max_frame_size: Some(MAX_CLIENT_MESSAGE_BYTES),
        ..Default::default()
    };
    let ws = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        tokio_tungstenite::accept_async_with_config(stream, Some(config)),
    )
    .await
    .context("websocket handshake timed out")?
    .context("websocket handshake failed")?;

    let (mut sink, mut source) = ws.split();
    // Subscribed before any backlog read — see the module comment.
    let mut events = daemon.events_tx.subscribe();
    // Subscribed *before* the hello that learns this connection's device id, so
    // a revocation racing the handshake cannot slip between the two: the
    // message is already queued on this receiver when the id arrives, and the
    // authentication path re-checks `revoked` itself.
    let mut revocations = daemon.revocations_tx.subscribe();
    // The accounts listing, pushed whenever it changes (minor 48), once this connection has itself sent an accounts
    // request: an app too old to read the frame never asks, and an iPhone's warns of every frame it can't read.
    // Subscribed at that first request, so nothing older than its answer follows it.
    let mut accounts_pushed: Option<broadcast::Receiver<ServerMessage>> = None;
    let mut watermarks: HashMap<String, u64> = HashMap::new();
    let mut authed = false;
    // Which device this connection belongs to, if any.
    //
    // Load-bearing for revocation: `hello` happens once and a phone then holds
    // the socket open for hours. Without re-checking, `codeconnect revoke` would take
    // effect only on the *next* connection — leaving a revoked device able to
    // keep answering approvals and typing into the session's TTY, which is the
    // opposite of what the operator just asked for.
    let mut device_id: Option<String> = None;
    // The owner of the uploads sent on this connection: what no message attached goes when it
    // closes, however it closes.
    let connection = UploadOwner::new(&daemon);
    // The Tailscale device this connection's peer is, when it is known: Tailscale's word for the address it came
    // from, else the node its device was last seen from. It is left out of the `peers` frame this connection gets.
    let mut viewer_node: Option<String> = None;
    // Which kind of device it is, as its hello named it: what its answers are
    // recorded as coming from.
    let mut answered_on = AnsweredOn::Iphone;
    // The backlog this connection is part-way through paging out. A replay is
    // however many events the store holds, and awaiting it inside the arm that
    // asked for it is the loop standing still for all of them. Its own local so
    // no other arm's borrow ever meets it.
    let mut backlog = Backlog::default();
    // Earlier pages the phone asked for, stepped the same way and for the same
    // reason. Its own queue rather than the backlog's: a page moves no
    // watermark and is not a delivery the push gate may count.
    let mut history = History::default();
    // Answers worked out off this loop (`list_projects`, `start_session`): they
    // take seconds of shells and git, which the loop must not stand still for.
    let (replies_tx, mut replies) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.tick().await; // the first tick completes immediately
                            // The `peers` frame this connection last sent, and the re-reads that keep it
                            // current: each one runs off this loop, because `tailscale status` can take
                            // seconds to give up, and comes back on `peers_read`.
    let mut peers_sent: Option<String> = None;
    let mut peers_refresh = tokio::time::interval(PEERS_REFRESH);
    peers_refresh.tick().await;
    let (peers_reader, mut peers_read) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
    // This connection's view of a run's shell (`crate::terminal`), one at a time, and the open in flight, by its
    // attachment id: an open waits on the run's gate and starts a shell, so it finishes off this loop and comes back
    // on `terminal_opened`. One finishing for an id no longer asked for is dropped, letting its shell go.
    let mut terminal: Option<crate::terminal::Attachment> = None;
    let mut terminal_opening: Option<String> = None;
    let (terminal_open_done, mut terminal_opened) = tokio::sync::mpsc::unbounded_channel::<(
        String,
        std::result::Result<crate::terminal::Opened, &'static str>,
    )>();

    let auth_deadline = tokio::time::sleep(HANDSHAKE_TIMEOUT);
    tokio::pin!(auth_deadline);

    loop {
        tokio::select! {
            incoming = source.next() => {
                let Some(message) = incoming else { return Ok(()) };
                let message = message.context("websocket read failed")?;
                let text = match message {
                    Message::Text(text) => text,
                    Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Message::Close(_) => return Ok(()),
                    // tungstenite answers Ping frames itself.
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                };

                let parsed: ClientMessage = match serde_json::from_str(&text) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        // Not a phone message: before a phone's hello it may be
                        // another computer's daemon asking one thing, answered once.
                        if !authed {
                            if let Ok(request) = serde_json::from_str::<PeerRequest>(&text) {
                                let reply = daemon.serve_peer(peer, request).await;
                                sink.send(Message::Text(serde_json::to_string(&reply)?)).await?;
                                let _ = sink.close().await;
                                return Ok(());
                            }
                        }
                        send(&mut sink, &ServerMessage::Error {
                            code: "bad_request".into(),
                            message: format!("undecodable message: {err}"),
                        }).await?;
                        continue;
                    }
                };

                if !authed {
                    match &parsed {
                        ClientMessage::Hello {
                            protocol_version,
                            token: given,
                            pairing_code,
                            client_name,
                            device_name,
                            ..
                        } => {
                            // Checked *before* the credential, and refused
                            // rather than warned about. The major version is
                            // the "can we talk at all" question: a client on a
                            // different major has, by the definition of the
                            // number, a different idea of what these messages
                            // mean — and the previous behaviour was to log a
                            // warning and then grant full access to the event
                            // log and the approval path anyway. Doing it first
                            // also means an incompatible peer never reaches the
                            // token comparison, so it cannot be used to probe
                            // credentials.
                            if *protocol_version != protocol::PROTOCOL_VERSION {
                                crate::log_warn!(
                                    "ws: refusing a client speaking protocol {protocol_version}; \
                                     we speak {}",
                                    protocol::PROTOCOL_VERSION
                                );
                                send(&mut sink, &ServerMessage::Error {
                                    code: "protocol_mismatch".into(),
                                    message: protocol::PROTOCOL_MISMATCH.into(),
                                }).await?;
                                return Ok(());
                            }
                            // Read once for both: whether Tailscale vouches for
                            // this address, and the `peers` frame a paired device
                            // is sent after its ack.
                            let tailnet = daemon.read_tailnet().await;
                            let vouched = match &tailnet {
                                Some(tailnet) => tailnet.sender(peer).map(|node| node.id.clone()),
                                None => Err(protocol::tailnet::Refusal::Unknown),
                            };
                            let outcome = daemon
                                .authenticate(
                                    &token,
                                    given.as_deref(),
                                    pairing_code.as_deref(),
                                    // The name a new device is listed under: what it calls itself, else
                                    // the app's name, as before minor 44.
                                    device_name.as_deref().or(client_name.as_deref()),
                                    vouched.as_deref().map_err(|refusal| *refusal),
                                )
                                .await;
                            let ack = match outcome {
                                AuthOutcome::Revoked => {
                                    crate::log_info!(
                                        "ws: a revoked device's token tried to connect"
                                    );
                                    // Only a holder of that pairing's token reaches this. It
                                    // is told "revoked", not "unauthorized", so it waits and
                                    // reconnects when the computer allows it again instead of
                                    // being sent to pair with a new code. A stranger, lacking
                                    // the token, still learns nothing.
                                    send(&mut sink, &ServerMessage::Error {
                                        code: "revoked".into(),
                                        message: "this device has been revoked".into(),
                                    }).await?;
                                    return Ok(());
                                }
                                AuthOutcome::Rejected(reason) => {
                                    crate::log_warn!("ws: rejected a client ({reason})");
                                    // One opaque message for every failure: a
                                    // peer that cannot authenticate learns
                                    // nothing about *which* credential was wrong.
                                    send(&mut sink, &ServerMessage::Error {
                                        code: "unauthorized".into(),
                                        message: "invalid credentials".into(),
                                    }).await?;
                                    return Ok(());
                                }
                                outcome => outcome,
                            };
                            // A paired device that now names itself differently is listed under its
                            // new name; one that names nothing keeps the name it has.
                            if let (AuthOutcome::Device(device), Some(name)) =
                                (&ack, device_name.as_deref())
                            {
                                if let Err(err) = daemon
                                    .db
                                    .rename_device(device.device_id.clone(), name.to_string())
                                    .await
                                {
                                    crate::log_error!(
                                        "could not rename device {}: {err:#}",
                                        device.device_id
                                    );
                                }
                            }
                            authed = true;
                            viewer_node = vouched.as_ref().ok().cloned().or_else(|| match &ack {
                                AuthOutcome::Device(device) => device.tailnet_node.clone(),
                                _ => None,
                            });
                            device_id = match &ack {
                                AuthOutcome::Device(device) => Some(device.device_id.clone()),
                                AuthOutcome::Paired { device_id, .. } => Some(device_id.clone()),
                                _ => None,
                            };
                            answered_on = AnsweredOn::from_client_name(client_name.as_deref());
                            crate::log_info!(
                                "ws: client {:?} connected over {} as {}",
                                client_name.as_deref().unwrap_or("unnamed"),
                                if tls_active { "wss" } else { "ws" },
                                describe(&ack),
                            );
                            match hello_ack(&daemon, ack, tls_active, private_transport).await {
                                Ok(ack) => send(&mut sink, &ack).await?,
                                // The ack could not be built truthfully — the
                                // one read it makes failed, and a `None` in its
                                // place would tell the phone something false
                                // about its own registration. Close honestly
                                // rather than hand it that; the phone reconnects
                                // and a transient fault is gone by the retry.
                                Err(err) => {
                                    crate::log_error!("ws: could not build hello_ack: {err:#}");
                                    send(
                                        &mut sink,
                                        &ServerMessage::Error {
                                            code: "handshake_failed".into(),
                                            message: "the daemon could not read the state this \
                                                      handshake must report; reconnect to retry"
                                                .into(),
                                        },
                                    )
                                    .await?;
                                    return Ok(());
                                }
                            }
                            if device_id.is_some() {
                                let frame = peers_frame(&daemon, tailnet.as_ref(), peer, viewer_node.as_deref());
                                peers_sent = Some(serde_json::to_string(&frame)?);
                                send(&mut sink, &frame).await?;
                            }
                            // The ack carried what a re-read would, so the first
                            // re-read is a whole period after it.
                            peers_refresh.reset();
                        }
                        _ => {
                            send(&mut sink, &ServerMessage::Error {
                                code: "unauthorized".into(),
                                message: "hello must come first".into(),
                            }).await?;
                            return Ok(());
                        }
                    }
                    continue;
                }

                // Checked before *every* message, not only the mutating ones:
                // a revoked device must not be able to keep reading the event
                // log either. One indexed lookup against a table with a handful
                // of rows, on a connection that is already doing JSON.
                if revoked(&daemon, device_id.as_deref()).await {
                    crate::log_warn!(
                        "ws: closing a connection for revoked device {:?}",
                        device_id.as_deref().unwrap_or("?")
                    );
                    send(&mut sink, &ServerMessage::Error {
                        code: "revoked".into(),
                        message: "this device has been revoked".into(),
                    }).await?;
                    return Ok(());
                }

                // Off the loop: the sample waits half a second for the CPU, and
                // nothing else on this connection may wait behind it.
                if matches!(parsed, ClientMessage::GetMachineStats) {
                    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                    let replies = replies_tx.clone();
                    tokio::spawn(async move {
                        let stats = crate::machine::stats(home.as_deref()).await;
                        let _ = replies.send(ServerMessage::MachineStats {
                            cpu: stats.cpu,
                            memory_used: stats.memory_used,
                            memory_total: stats.memory_total,
                            disk_used: stats.disk_used,
                            disk_total: stats.disk_total,
                            captured_at: protocol::time::now_rfc3339(),
                        });
                    });
                    continue;
                }

                // A run's shell: a paired device, and only over a transport nothing on the path can read, since what
                // it carries is a shell's keystrokes and output.
                if let Some(attachment_id) = terminal_frame(&parsed) {
                    let Some(device) = device_id.as_deref().filter(|_| private_transport) else {
                        send(&mut sink, &terminal_closed(attachment_id, protocol::ws::terminal_closed::NOT_AUTHORISED)).await?;
                        continue;
                    };
                    if let Some(reason) = handle_terminal(
                        &daemon,
                        device,
                        parsed,
                        &mut terminal,
                        &mut terminal_opening,
                        &terminal_open_done,
                    ) {
                        send(&mut sink, &reason).await?;
                    }
                    continue;
                }

                // Off the loop, for a paired device: git in a large repository takes
                // seconds, and an approval on this connection must not wait behind it.
                if let ClientMessage::GetRunGit { session_id } = parsed {
                    if device_id.is_none() {
                        send(&mut sink, &ServerMessage::Error {
                            code: "not_authorised".into(),
                            message: "only a paired device may read a run's repository".into(),
                        }).await?;
                        continue;
                    }
                    let daemon = daemon.clone();
                    let replies = replies_tx.clone();
                    tokio::spawn(async move {
                        // Whatever git, or the registry, could not say is the answer's note,
                        // so the reply always names the run it answers.
                        let (git, note) = match daemon.run_git(&session_id).await {
                            Ok(Ok(git)) => (Some(git), None),
                            Ok(Err(note)) => (None, Some(note)),
                            Err(err) => (None, Some(format!("{err}"))),
                        };
                        let _ = replies.send(ServerMessage::RunGit { session_id, git, note });
                    });
                    continue;
                }

                // Off the loop: a run's diff waits its turn at the folder's git gate, behind a
                // commit whose hooks may take minutes.
                if let ClientMessage::GetDiff { session_id } = parsed {
                    let daemon = daemon.clone();
                    let replies = replies_tx.clone();
                    tokio::spawn(async move {
                        let _ = replies.send(diff_reply(&daemon, session_id).await);
                    });
                    continue;
                }

                // An action on a run's repository: off the loop and for a paired device, as
                // the read is; a commit's hooks may take minutes.
                if let ClientMessage::GitAction { request_id, session_uid, action } = parsed {
                    if device_id.is_none() {
                        send(&mut sink, &ServerMessage::Error {
                            code: "not_authorised".into(),
                            message: "only a paired device may act on a run's repository".into(),
                        }).await?;
                        continue;
                    }
                    let daemon = daemon.clone();
                    let replies = replies_tx.clone();
                    tokio::spawn(async move {
                        let result = daemon.git_action(&session_uid, &request_id, &action).await;
                        let _ = replies.send(ServerMessage::GitActionResult {
                            request_id,
                            session_uid,
                            result,
                        });
                    });
                    continue;
                }

                // Starting, the folder and project frames that choose where, and the
                // folders' answers to "trust this folder?": any paired device, on any
                // transport the listener admits, with the authority that saying and
                // answering already carry there.
                if matches!(
                    parsed,
                    ClientMessage::ListProjects
                        | ClientMessage::GetProjectGit { .. }
                        | ClientMessage::ListFolder { .. }
                        | ClientMessage::CreateFolder { .. }
                        | ClientMessage::StartSession { .. }
                        | ClientMessage::ListFolderTrust { .. }
                        | ClientMessage::SetFolderTrust { .. }
                        | ClientMessage::ForgetFolderTrust { .. }
                        | ClientMessage::ListAccounts { .. }
                        | ClientMessage::AddAccount { .. }
                        | ClientMessage::ActivateAccount { .. }
                        | ClientMessage::SignInAccount { .. }
                        | ClientMessage::SignInAgent { .. }
                        | ClientMessage::SignOutAgent { .. }
                        | ClientMessage::RemoveAccount { .. }
                ) {
                    if device_id.is_none() {
                        send(&mut sink, &ServerMessage::Error {
                            code: "not_authorised".into(),
                            message: "only a paired device may start sessions".into(),
                        }).await?;
                        continue;
                    }
                    if accounts_pushed.is_none() && matches!(
                        parsed,
                        ClientMessage::ListAccounts { .. }
                            | ClientMessage::AddAccount { .. }
                            | ClientMessage::ActivateAccount { .. }
                            | ClientMessage::SignInAccount { .. }
                            | ClientMessage::SignInAgent { .. }
                            | ClientMessage::SignOutAgent { .. }
                            | ClientMessage::RemoveAccount { .. }
                    ) {
                        accounts_pushed = Some(daemon.accounts_tx.subscribe());
                    }
                    let daemon = daemon.clone();
                    let replies = replies_tx.clone();
                    let connection = connection.id;
                    tokio::spawn(async move {
                        match parsed {
                            ClientMessage::ListProjects => {
                                crate::start_session::list_projects(&daemon, &replies).await
                            }
                            parsed => {
                                let _ = replies
                                    .send(crate::start_session::answer(&daemon, connection, parsed).await);
                            }
                        }
                    });
                    continue;
                }

                handle_message(
                    &daemon,
                    &mut sink,
                    &mut watermarks,
                    &mut backlog,
                    &mut history,
                    device_id.as_deref(),
                    answered_on,
                    connection.id,
                    parsed,
                )
                .await?;
            }

            // One step of a backlog replay, when one is in progress: the next
            // event, or the next page from the store.
            //
            // The future is *ready*, not pending, and that is a decision. A
            // `yield_now` here would be pending on its first poll and so give
            // every other arm strict priority, which starves the replay whenever
            // another arm is ready almost continuously. Both `biased` orders are
            // wrong for the same pair of reasons. `select!` picks uniformly at
            // random among the branches that are ready, so an always-ready branch
            // gets served about every other pass against any one competitor.
            //
            // Every await is in the body, never in the future above, so a step
            // that has begun always finishes. That is what keeps the store read
            // strictly ordered against everything the loop has already
            // processed, which is the whole of the argument that a live event
            // dropped in favour of a replay cannot be lost (see `Backlog`).
            _ = std::future::ready(()), if backlog.in_progress() => {
                backlog.step(&daemon, &mut sink, &mut watermarks, device_id.as_deref()).await?;
                // Hand the runtime back before the next pass. Most steps park on
                // a socket write or a store read and yield of their own accord,
                // but one that skips an event a live delivery already carried
                // parks on nothing — and a run of those would hold a
                // current-thread executor away from every other task on it,
                // which is the very starvation this arm exists to prevent.
                tokio::task::yield_now().await;
            }

            // One step of an earlier page: a store read, one event, or the end.
            // Ready rather than pending for the replay arm's reason, so a page
            // competes for each pass with live delivery and the replay instead
            // of holding them off for its whole length.
            _ = std::future::ready(()), if history.in_progress() => {
                history.step(&daemon, &mut sink).await?;
                tokio::task::yield_now().await;
            }

            broadcast = events.recv() => {
                match broadcast {
                    Ok(event) => {
                        if !authed { continue; }
                        let Some(watermark) = watermarks.get(&event.session_uid).copied() else {
                            continue;
                        };
                        match live_delivery(event.seq, watermark) {
                            Live::Deliver => {
                                let session_uid = event.session_uid.clone();
                                let seq = event.seq;
                                send(&mut sink, &ServerMessage::Event { event }).await?;
                                if let Some(device) = device_id.as_deref() {
                                    daemon.push_gate.note_delivered(device, &session_uid, seq);
                                }
                                watermarks.insert(session_uid, seq);
                            }
                            Live::AlreadySent => {}
                            // Announced only when no replay for that session is
                            // already pending, and the event itself is not sent
                            // either way: the store already has it (`Daemon::ingest`
                            // appends inside the publish gate and only then
                            // broadcasts) and the replay reads it from there.
                            //
                            // `gap_marker`'s own doc says what it means — the
                            // *daemon* published out of order, as opposed to a slow
                            // client — and while that session's backlog is still
                            // being paged out, a seq above the successor carries no
                            // information about publish order at all: it is the
                            // replay not having reached it yet. Emitting the marker
                            // there would assert something unknown, and would
                            // announce one hole twice when the marker behind it was
                            // a real gap.
                            //
                            // The cost is honest and worth stating: an ordinary
                            // `subscribe` also leaves a replay pending with no
                            // marker behind it, so this can suppress a *first*
                            // diagnostic and not only a duplicate. What it cannot do
                            // is hide the bug the marker exists to catch, because
                            // that shows up in the steady state — which is every
                            // moment no replay is pending.
                            Live::Gap => {
                                if !backlog.replaying(&event.session_uid) {
                                    crate::log_warn!(
                                        "ws: {} jumped from seq {watermark} to {}; resyncing from the log",
                                        event.session_uid, event.seq
                                    );
                                    send(&mut sink, &ServerMessage::Event {
                                        event: gap_marker(&event, watermark),
                                    }).await?;
                                    backlog.begin(event.session_uid.clone());
                                }
                            }
                        }
                    }
                    // The marker is unconditional here, unlike the gap's: falling
                    // off the broadcast ring is a fact about *this connection*
                    // that happened, whatever else is in flight, so there is
                    // nothing to infer and nothing to be wrong about.
                    //
                    // The loop stays inline while the paging it asks for does
                    // not, and what bounds it is that it is one indexed
                    // `get_session` and one marker frame per *subscribed*
                    // session — a handful, on a connection that follows the
                    // sessions a phone is looking at. The paging behind it is the
                    // unbounded part, and that is exactly what is handed off.
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        crate::log_warn!("ws: client lagged {skipped} events; resyncing from the log");
                        let sessions: Vec<String> = watermarks.keys().cloned().collect();
                        for session_uid in sessions {
                            let name = daemon
                                .db
                                .get_session(session_uid.clone())
                                .await
                                .ok()
                                .flatten()
                                .map(|row| row.session_id)
                                .unwrap_or_default();
                            send(&mut sink, &ServerMessage::Event {
                                event: resync_marker(&session_uid, &name, skipped),
                            }).await?;
                            backlog.begin(session_uid);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }

            // The push half of revocation. Without it, `codeconnect revoke` reached an
            // idle phone only at the next keepalive — up to 30 seconds during
            // which a device the operator had just cut off was still receiving
            // the live event log.
            withdrawn = revocations.recv() => {
                match withdrawn {
                    Ok(revoked_id) => {
                        if device_id.as_deref() == Some(revoked_id.as_str()) {
                            crate::log_info!("ws: closing {revoked_id}'s connection; it was just revoked");
                            // A best-effort courtesy, so the phone can say why it
                            // was disconnected.
                            let _ = send(&mut sink, &ServerMessage::Error {
                                code: "revoked".into(),
                                message: "this device has been revoked".into(),
                            }).await;
                            return Ok(());
                        }
                    }
                    // Lagged off a 64-slot ring means many revocations landed
                    // at once; re-checking the store is exact and costs one
                    // indexed lookup.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if authed && revoked(&daemon, device_id.as_deref()).await {
                            return Ok(());
                        }
                    }
                    // Unreachable while this task holds the `Arc<Daemon>` that
                    // owns the sender. Returning rather than ignoring is what
                    // keeps it unreachable *safely*: an ignored `Closed` would
                    // be ready on every poll and spin this select loop hot.
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }

            pushed = async {
                match accounts_pushed.as_mut() {
                    Some(listing) => listing.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match pushed {
                    Ok(listing) => send(&mut sink, &listing).await?,
                    // Those missed are older than the ones still queued, which follow.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }

            Some(reply) = replies.recv() => send(&mut sink, &reply).await?,

            Some((attachment_id, opened)) = terminal_opened.recv() => {
                if terminal_opening.as_deref() != Some(attachment_id.as_str()) {
                    continue;
                }
                terminal_opening = None;
                match opened {
                    Ok(opened) => {
                        send(&mut sink, &ServerMessage::TerminalOpened {
                            attachment_id,
                            input_credit: protocol::ws::TERMINAL_INPUT_CREDIT,
                            max_chunk_bytes: protocol::ws::MAX_TERMINAL_CHUNK_BYTES,
                            max_outstanding_credit: protocol::ws::TERMINAL_MAX_OUTSTANDING_CREDIT,
                            folder: opened.folder,
                            replay_end: opened.replay_end,
                        }).await?;
                        terminal = Some(opened.attachment);
                    }
                    Err(reason) => send(&mut sink, &terminal_closed(&attachment_id, reason)).await?,
                }
            }

            // One step of the shell this connection views: one bounded chunk, a credit, or its close.
            () = async {
                match terminal.as_mut() {
                    Some(attachment) => attachment.ready().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(attachment) = terminal.as_mut() else { continue };
                match attachment.next() {
                    Some(crate::terminal::Step::Output { data, reset, offset }) => {
                        use base64::Engine as _;
                        send(&mut sink, &ServerMessage::TerminalOutput {
                            attachment_id: attachment.id.clone(),
                            data: base64::engine::general_purpose::STANDARD.encode(data),
                            reset,
                            offset,
                        }).await?;
                    }
                    Some(crate::terminal::Step::InputCredit(bytes)) => {
                        send(&mut sink, &ServerMessage::TerminalCredit {
                            attachment_id: attachment.id.clone(),
                            bytes,
                        }).await?;
                    }
                    Some(crate::terminal::Step::Closed(reason)) => {
                        let closed = terminal_closed(&attachment.id, reason);
                        terminal = None;
                        send(&mut sink, &closed).await?;
                    }
                    None => {}
                }
            }

            _ = peers_refresh.tick(), if device_id.is_some() => {
                let daemon = Arc::clone(&daemon);
                let peers_reader = peers_reader.clone();
                let viewer_node = viewer_node.clone();
                tokio::spawn(async move {
                    let tailnet = daemon.read_tailnet().await;
                    let _ = peers_reader.send(peers_frame(&daemon, tailnet.as_ref(), peer, viewer_node.as_deref()));
                });
            }

            Some(frame) = peers_read.recv() => {
                let json = serde_json::to_string(&frame)?;
                if peers_sent.as_deref() != Some(json.as_str()) {
                    send(&mut sink, &frame).await?;
                    peers_sent = Some(json);
                }
            }

            _ = keepalive.tick() => {
                if authed {
                    // An idle connection sends nothing, so the per-message check
                    // above would never fire on it. Revocation has to reach a
                    // phone that is merely listening, not only one that is
                    // asking for something.
                    if revoked(&daemon, device_id.as_deref()).await {
                        crate::log_warn!(
                            "ws: closing an idle connection for revoked device {:?}",
                            device_id.as_deref().unwrap_or("?")
                        );
                        return Ok(());
                    }
                    // Bounded like every other write: a peer that cannot even
                    // accept a ping in the deadline is gone, and saying so
                    // closes the connection instead of blocking here forever.
                    write_bounded(&mut sink, Message::Ping(Vec::new())).await.context("keepalive failed")?;
                }
            }

            () = &mut auth_deadline, if !authed => {
                crate::log_warn!("ws: client never authenticated; closing");
                return Ok(());
            }
        }
    }
}

/// What to do with a live event, given what this connection has already had.
#[derive(Debug, PartialEq, Eq)]
enum Live {
    Deliver,
    /// Below the watermark: a replay already carried it. Normal, and the only
    /// reason the watermark exists.
    AlreadySent,
    /// Above the successor: something is missing. Never skipped to.
    Gap,
}

/// Live delivery is *successor only*.
///
/// `seq > watermark` looks equivalent and is not. Accepting 2 when 1 has not
/// been sent advances the watermark past 1, and 1 is then discarded by that same
/// test when it arrives — silently, on this connection, for the rest of its
/// life. A client cannot detect a gap it was never told about, and an event log
/// whose replay can quietly omit a fact is not a source of truth.
fn live_delivery(seq: u64, watermark: u64) -> Live {
    match seq.cmp(&(watermark + 1)) {
        std::cmp::Ordering::Equal => Live::Deliver,
        std::cmp::Ordering::Less => Live::AlreadySent,
        std::cmp::Ordering::Greater => Live::Gap,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_message<S>(
    daemon: &Arc<Daemon>,
    sink: &mut S,
    watermarks: &mut HashMap<String, u64>,
    // Where a subscribe asks for its backlog, rather than paging it out here:
    // this function is called from the arm that reads client messages, and a
    // replay awaited on that stack is the whole connection standing still for
    // however long the log is (see `Backlog`).
    backlog: &mut Backlog,
    // Where a `history` request is queued, for the same reason.
    history: &mut History,
    // The authenticated device, when there is one. A client on the static
    // bootstrap token has no device row, so it has nowhere to register a push
    // token — see `RegisterPush`.
    device_id: Option<&str>,
    // What its answers are recorded as coming from.
    answered_on: AnsweredOn,
    // Whose uploads (`crate::uploads`) a message here may attach.
    connection: u64,
    message: ClientMessage,
) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    match message {
        // A second hello does not re-authenticate, and there is nothing else for
        // it to do.
        ClientMessage::Hello { .. } => {}
        // A shell's frames are this connection's stream, served in `handle_client` (`handle_terminal`).
        ClientMessage::TerminalOpen { .. }
        | ClientMessage::TerminalInput { .. }
        | ClientMessage::TerminalResize { .. }
        | ClientMessage::TerminalCredit { .. }
        | ClientMessage::TerminalDetach { .. } => {
            unreachable!("a terminal frame is served by handle_terminal")
        }
        ClientMessage::Ping => {
            send(sink, &ServerMessage::Pong).await?;
            // Each ping is the device reaching this computer. The apps send one every ten
            // seconds, so the Mac's list of devices reads a connected one as seen seconds
            // ago, not as "Connected". Spawned, so the answer above never waits for the
            // store.
            if let Some(device_id) = device_id {
                let daemon = Arc::clone(daemon);
                let device_id = device_id.to_string();
                tokio::spawn(async move {
                    let _ = daemon
                        .db
                        .touch_device(device_id, protocol::time::now_rfc3339())
                        .await;
                });
            }
        }
        // Answered off the loop by `handle_client`, which never passes them here.
        ClientMessage::GetDiff { .. }
        | ClientMessage::GetRunGit { .. }
        | ClientMessage::GitAction { .. }
        | ClientMessage::ListProjects
        | ClientMessage::GetProjectGit { .. }
        | ClientMessage::ListFolder { .. }
        | ClientMessage::CreateFolder { .. }
        | ClientMessage::StartSession { .. }
        | ClientMessage::ListFolderTrust { .. }
        | ClientMessage::SetFolderTrust { .. }
        | ClientMessage::ForgetFolderTrust { .. }
        | ClientMessage::ListAccounts { .. }
        | ClientMessage::AddAccount { .. }
        | ClientMessage::ActivateAccount { .. }
        | ClientMessage::SignInAccount { .. }
        | ClientMessage::SignInAgent { .. }
        | ClientMessage::SignOutAgent { .. }
        | ClientMessage::RemoveAccount { .. }
        | ClientMessage::GetMachineStats => {}
        ClientMessage::RegisterPush {
            token,
            environment,
            relay_credential,
        } => {
            // Normalised on the way in, so the column only ever holds one of
            // two spellings and a later reader cannot be surprised by "prod".
            let environment = crate::apns_sender::ApnsEnvironment::parse(
                environment.as_deref().unwrap_or_default(),
            )
            .as_str()
            .to_string();
            let Some(device_id) = device_id else {
                // Refused, and said out loud. A push token belongs to a device
                // row so that revoking the device stops its notifications; the
                // static token has no row, so accepting this would create a
                // notification stream nothing could ever switch off.
                send(
                    sink,
                    &ServerMessage::Error {
                        code: "no_device".into(),
                        message: "push registration needs a paired device; \
                              this connection is on the bootstrap token"
                            .into(),
                    },
                )
                .await?;
                return Ok(());
            };
            // **The mode decides what a registration must carry**, and a
            // registration that cannot be acted on is refused rather than
            // stored: a row the daemon will never send to is a phone waiting
            // for a notification nobody is going to try to deliver.
            let (token, credential) =
                match validated_registration(daemon.push.mode(), &token, relay_credential.as_ref())
                {
                    Ok(accepted) => accepted,
                    Err(refusal) => {
                        crate::log_warn!("push: refusing registration from {device_id}: {refusal}");
                        send(
                            sink,
                            &ServerMessage::Error {
                                code: "push_registration_failed".into(),
                                message: refusal,
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                };
            let registered = daemon
                .register_push(device_id, &token, &environment, credential.as_ref())
                .await;
            match registered {
                Ok(()) => crate::log_info!(
                    "push: device {device_id} registered for {environment} notifications"
                ),
                Err(err) => {
                    crate::log_error!("push: could not register {device_id}: {err:#}");
                    send(
                        sink,
                        &ServerMessage::Error {
                            code: "push_registration_failed".into(),
                            message: format!("could not store the push token: {err}"),
                        },
                    )
                    .await?;
                }
            }
        }
        // A failed read is reported as a failure. `unwrap_or_default` turned a
        // database error into an empty fleet — a phone showing "no sessions"
        // when the truth is "we could not look" is the daemon claiming
        // something it does not know.
        ClientMessage::Sessions => match daemon.sessions().await {
            Ok(sessions) => send(sink, &ServerMessage::Sessions { sessions }).await?,
            Err(err) => {
                crate::log_error!("could not list sessions: {err:#}");
                send(
                    sink,
                    &ServerMessage::Error {
                        code: "sessions_unavailable".into(),
                        message: format!("could not read the session list: {err}"),
                    },
                )
                .await?;
            }
        },
        // Watermarks are keyed by `session_uid`, so a client that subscribed to
        // `cc-1` is following *that run* and does not silently start receiving a
        // later session's events when the name is reused.
        ClientMessage::Subscribe {
            session_id,
            after_seq,
        } => match daemon.resolve(&session_id).await {
            Ok(row) => {
                watermarks.insert(row.session_uid.clone(), after_seq);
                backlog.begin(row.session_uid);
            }
            Err(err) => {
                send(
                    sink,
                    &ServerMessage::Error {
                        code: "unknown_session".into(),
                        message: format!("{err}"),
                    },
                )
                .await?;
            }
        },
        ClientMessage::History {
            session_id,
            before_seq,
            limit,
            request_id,
        } => {
            let session_uid = daemon
                .resolve(&session_id)
                .await
                .map(|row| row.session_uid)
                .map_err(|err| format!("{err}"));
            history.ask(HistoryRequest::new(
                session_id,
                session_uid,
                before_seq,
                limit.min(protocol::ws::MAX_HISTORY_PAGE),
                request_id,
            ));
        }
        ClientMessage::StopRun {
            session_id,
            request_id,
        } => {
            let result = daemon.stop_run(&session_id).await;
            send(
                sink,
                &ServerMessage::StopRunResult {
                    session_id,
                    request_id,
                    result,
                },
            )
            .await?;
        }
        ClientMessage::SetModel {
            session_id,
            request_id,
            model,
            effort,
        } => {
            let result = daemon
                .set_setting(
                    &session_id,
                    crate::claude_host::Setting::Model { model, effort },
                )
                .await;
            send(
                sink,
                &ServerMessage::SettingResult {
                    session_id,
                    request_id,
                    result,
                },
            )
            .await?;
        }
        ClientMessage::SetMode {
            session_id,
            request_id,
            mode,
            confirmed,
        } => {
            let result = daemon
                .set_setting(
                    &session_id,
                    crate::claude_host::Setting::Mode { mode, confirmed },
                )
                .await;
            send(
                sink,
                &ServerMessage::SettingResult {
                    session_id,
                    request_id,
                    result,
                },
            )
            .await?;
        }
        ClientMessage::DeleteSession { session_uid } => {
            // **By uid, never by name.** `resolve` accepts a name and maps it to the
            // newest run under it, which is right for subscribing and wrong for
            // deleting.
            let result = match daemon.delete_exited_session(&session_uid).await {
                Ok(outcome) => outcome,
                // Answered as a delete result, not as a bare `error` frame. An
                // `error` names no session, so a phone waiting on this one cannot
                // tell the failure is its own and sits on a spinner until its
                // timeout.
                Err(err) => {
                    crate::log_error!("delete_session {session_uid}: {err:#}");
                    protocol::ws::DeleteSessionResult::Failed {
                        message: "the daemon could not remove that session".into(),
                    }
                }
            };
            // Only when the row is really gone. The watermark is what feeds this
            // socket's replay, and dropping it for a session the daemon just
            // refused to delete stops that session's events reaching a phone that
            // still believes it is subscribed — silent until the next reconnect.
            // Matched positively so a later outcome has to opt in to this.
            if matches!(
                result,
                protocol::ws::DeleteSessionResult::Deleted { .. }
                    | protocol::ws::DeleteSessionResult::NotFound
            ) {
                watermarks.remove(&session_uid);
                // And any replay still queued for it. `Backlog::step` refuses to
                // page to a missing watermark anyway, so this is the eager half:
                // without it a delete followed by a fresh subscribe before the
                // next step would hand the new subscription a page read against
                // the old one.
                backlog.drop_session(&session_uid);
                // And any page of it still being sent, which ends with the reason.
                history.drop_session(&session_uid);
            }
            send(
                sink,
                &ServerMessage::DeleteSessionResult {
                    session_uid,
                    result,
                },
            )
            .await?;
        }
        ClientMessage::TestPush { request_id } => {
            // Every branch answers the request it was asked — same contract as
            // `delete_session`: a phone holding a spinner can only be released
            // by a result carrying its own correlation id.
            // **Configured, not reachable.** A relay that is down at this
            // instant is still the delivery path this daemon has, and answering
            // `push_unconfigured` would tell the phone to stop offering the
            // button that is the only way to find out when it comes back. The
            // send below reports the real failure.
            let result = if !daemon.push.mode().is_configured() {
                protocol::ws::TestPushResult::PushUnconfigured
            } else if let Some(device) = device_id {
                match daemon.test_push_gate(device).await {
                    Some(retry_after_secs) => {
                        protocol::ws::TestPushResult::RateLimited { retry_after_secs }
                    }
                    None => match daemon.push.send_test(device).await {
                        Ok(crate::apns::TestDelivery::Accepted { apns_id }) => {
                            protocol::ws::TestPushResult::Accepted { apns_id }
                        }
                        Ok(crate::apns::TestDelivery::Unconfigured) => {
                            protocol::ws::TestPushResult::PushUnconfigured
                        }
                        Ok(crate::apns::TestDelivery::NoToken) => {
                            protocol::ws::TestPushResult::NoRegisteredToken
                        }
                        // **Not a dead token.** The registration is intact and
                        // the repair is a fresh bearer, so the phone must not
                        // be told to go and ask Apple for another token.
                        Ok(crate::apns::TestDelivery::CredentialInvalid) => {
                            protocol::ws::TestPushResult::CredentialInvalid
                        }
                        // The relay's own budget, reported with the wait it
                        // named — distinct from the daemon's 30-second floor
                        // above, which is this Mac refusing its own user.
                        Ok(crate::apns::TestDelivery::RateLimited { retry_after_secs }) => {
                            protocol::ws::TestPushResult::RateLimited { retry_after_secs }
                        }
                        Ok(crate::apns::TestDelivery::Failed(reason)) => {
                            protocol::ws::TestPushResult::Failed { reason }
                        }
                        // The sender dropped its half without answering — a bug
                        // worth a log line, reported as a failure rather than a
                        // hang.
                        Err(_) => protocol::ws::TestPushResult::Failed {
                            reason: "the push sender did not report an outcome".into(),
                        },
                    },
                }
            } else {
                // The static bootstrap token has no device row: nothing to
                // send to, and by design nothing it may exercise.
                protocol::ws::TestPushResult::NotPairedDevice
            };
            send(sink, &ServerMessage::TestPushResult { request_id, result }).await?;
        }
        ClientMessage::Unsubscribe { session_id } => {
            // Resolution can fail here — the run may have been forgotten — and
            // an unsubscribe that cannot name anything has nothing to undo, so
            // the id is also removed verbatim in case it was already a uid.
            if let Ok(row) = daemon.resolve(&session_id).await {
                watermarks.remove(&row.session_uid);
            }
            watermarks.remove(&session_id);
        }
        ClientMessage::Answer {
            request_id,
            payload_hash,
            decision,
            session_id,
        } => {
            let result = daemon
                .answer(
                    &request_id,
                    &payload_hash,
                    decision,
                    session_id.as_deref(),
                    answered_on,
                )
                .await;
            send(sink, &ServerMessage::AnswerResult { request_id, result }).await?;
        }
        ClientMessage::SendText {
            session_id,
            text,
            request_id,
            payload_hash,
            submit,
            attachments,
        } => {
            let result = daemon
                .send_text(
                    &session_id,
                    text,
                    request_id.as_deref(),
                    payload_hash.as_deref(),
                    submit,
                    connection,
                    &attachments,
                )
                .await;
            send(sink, &ServerMessage::SendTextResult { session_id, result }).await?;
        }
        ClientMessage::GetCommandCatalog { session_id } => {
            let result = daemon.command_catalog(&session_id).await;
            send(sink, &ServerMessage::CommandCatalog { session_id, result }).await?;
        }

        // Stop the turn a Codex session, or a Claude run CodeConnect started, is
        // running. Every refusal is a typed `Rejected` naming the reason, so a
        // mutation result is never a silence, and any other Claude session is
        // refused by the agent branch inside rather than
        // by this arm — the daemon is the one place that knows which agent a run
        // is hosting.
        ClientMessage::Interrupt {
            session_id,
            request_id,
            turn_id,
            payload_hash,
        } => {
            let result = daemon
                .interrupt(&session_id, &request_id, &turn_id, &payload_hash)
                .await;
            send(
                sink,
                &ServerMessage::InterruptResult {
                    session_id,
                    request_id,
                    result,
                },
            )
            .await?;
        }
        ClientMessage::PutFile {
            session_id,
            upload_id,
            name,
            size,
            offset,
            data,
        } => {
            // A file for a run names its session; one for a start names none, and is held
            // pending, this connection's alone, until a start takes it.
            let session_uid = match (device_id, session_id) {
                (None, _) => Err("only a paired device may send files".to_string()),
                (Some(_), None) => Ok(crate::uploads::PENDING.to_string()),
                (Some(_), Some(session_id)) => daemon
                    .resolve(&session_id)
                    .await
                    .map(|row| row.session_uid)
                    .map_err(|err| format!("{err}")),
            };
            let result = match session_uid {
                Ok(session_uid) => put_file(
                    daemon,
                    connection,
                    &session_uid,
                    &upload_id,
                    &name,
                    size,
                    offset,
                    &data,
                ),
                Err(reason) => protocol::ws::PutFileResult::Refused { reason },
            };
            send(sink, &ServerMessage::PutFileResult { upload_id, result }).await?;
        }
        // Say something to a Codex session. Every refusal is a typed `Rejected` naming
        // the reason, so a mutation result is never a silence, and a Claude session is
        // refused by the agent branch inside — the daemon is the one place that knows
        // which agent a run is hosting, and `send_text` is where a Claude session's free
        // text goes.
        ClientMessage::Compose {
            session_id,
            request_id,
            text,
            payload_hash,
            attachments,
        } => {
            let result = daemon
                .compose(
                    &session_id,
                    &request_id,
                    text,
                    &payload_hash,
                    connection,
                    &attachments,
                )
                .await;
            send(
                sink,
                &ServerMessage::ComposeResult {
                    session_id,
                    request_id,
                    result,
                },
            )
            .await?;
        }
    }
    Ok(())
}

/// One `put_file` chunk, from a paired device only, as every frame that changes this computer's
/// files is: written, or refused with its reason.
#[allow(clippy::too_many_arguments)]
fn put_file(
    daemon: &Daemon,
    connection: u64,
    session_uid: &str,
    upload_id: &str,
    name: &str,
    size: u64,
    offset: u64,
    data: &str,
) -> protocol::ws::PutFileResult {
    use base64::Engine as _;
    use protocol::ws::PutFileResult;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return PutFileResult::Refused {
            reason: "a chunk's data is base64".into(),
        };
    };
    match daemon.uploads.put(
        &crate::uploads::root(),
        connection,
        session_uid,
        upload_id,
        name,
        size,
        offset,
        &bytes,
    ) {
        Ok(received) => PutFileResult::Received { received },
        Err(reason) => PutFileResult::Refused { reason },
    }
}

/// A connection's hold on the uploads sent on it ([`crate::uploads`]): those no message
/// attached are removed when it is dropped, at the connection's end.
struct UploadOwner {
    id: u64,
    daemon: Arc<Daemon>,
}

impl UploadOwner {
    fn new(daemon: &Arc<Daemon>) -> UploadOwner {
        // From 1: 0 is no connection, which attaches nothing.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        UploadOwner {
            id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            daemon: Arc::clone(daemon),
        }
    }
}

impl Drop for UploadOwner {
    fn drop(&mut self) {
        self.daemon
            .uploads
            .close_connection(&crate::uploads::root(), self.id);
    }
}

/// Keep a diff inside one WebSocket frame *after* JSON escaping.
///
/// The 512KB cap (`protocol::ws::MAX_DIFF_BYTES`) is on the diff text. JSON
/// escaping can nearly double that — a diff is mostly newlines, and every `\n`
/// becomes two bytes — so a capped diff can still encode past the 1MB frame
/// limit. `URLSessionWebSocketTask` defaults to a 1MB maximum message and
/// *fails the connection* rather than the message, which would turn a large
/// diff into a disconnect loop. Shrinking further and keeping `truncated: true`
/// is the honest trade:
/// the phone already knows not to trust an incomplete diff.
/// `reserved` is what the message's `files` take once encoded.
fn fit_diff_in_a_frame(unified: String, truncated: bool, reserved: usize) -> (String, bool) {
    // Headroom for the rest of the message (session id, timestamp, note, keys).
    let ceiling = MAX_CLIENT_MESSAGE_BYTES
        .saturating_sub(4096)
        .saturating_sub(reserved);
    if encoded_len(&unified) <= ceiling {
        return (unified, truncated);
    }

    // Halve until it fits. At most a handful of rounds from any real diff, and
    // each round is a byte-count, not a re-encode of the whole message.
    let mut text = unified;
    while encoded_len(&text) > ceiling && !text.is_empty() {
        let mut end = text.len() / 2;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text.push_str("\n… diff truncated by CodeConnect to fit one message\n");
    (text, true)
}

/// The most a diff's `files` take of its frame once encoded; the rest is the diff's.
const FILES_FRAME_BUDGET: usize = 256 * 1024;

/// Keep a diff's `files` inside [`FILES_FRAME_BUDGET`] once encoded: the longest run of
/// files from the start that fits, saying so when any are left out. A thousand deep paths
/// would otherwise crowd the diff out of its frame.
fn fit_files_in_a_frame(
    files: Option<Vec<protocol::ws::DiffFile>>,
    truncated: bool,
) -> (Option<Vec<protocol::ws::DiffFile>>, bool) {
    let Some(mut files) = files else {
        return (None, truncated);
    };
    // `[` and `]`, then each file and the comma before every one but the first.
    let mut used = 2;
    let fits = files
        .iter()
        .take_while(|file| {
            used += encoded_len(file) + usize::from(used > 2);
            used <= FILES_FRAME_BUDGET
        })
        .count();
    let cut = truncated || fits < files.len();
    files.truncate(fits);
    (Some(files), cut)
}

fn encoded_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_string(value)
        .map(|text| text.len())
        .unwrap_or(0)
}

/// Build the ack, attaching the new credentials when this hello was a pairing.
///
/// **Fallible on purpose.** The one read it makes — the device's authoritative
/// push environment — is a fact the phone persists over its own cached copy, so
/// there is no safe way to guess it: a read that fails cannot be reported as
/// `None`, because `None` is the positive claim "no token is registered" and a
/// phone that believed it would discard a live registration. When the read
/// cannot be substantiated the whole ack fails and the caller closes the
/// connection honestly; the phone reconnects, and a transient database fault is
/// gone by the retry. A freshly paired device is the one case that never reads:
/// its row was just created with no token, so `None` there is the truth and not
/// a collapsed error.
async fn hello_ack(
    daemon: &Arc<Daemon>,
    outcome: AuthOutcome,
    tls_active: bool,
    private_transport: bool,
) -> Result<ServerMessage> {
    let (device_token, device_id, device_name) = match outcome {
        AuthOutcome::Paired {
            device_id,
            device_name,
            token,
        } => (Some(token), Some(device_id), Some(device_name)),
        // An already-paired device is told what it is known as, so a rename on
        // the Mac shows up on the phone without a re-pair.
        AuthOutcome::Device(device) => (
            None,
            Some(device.device_id.clone()),
            Some(device.name.clone()),
        ),
        _ => (None, None, None),
    };
    // **What the daemon holds, not what the phone last said.** A relay binding
    // is the authority for a token's APNs environment and corrects the daemon
    // on an accepted send; a phone that went on resending the value it first
    // cached would undo that correction on every handshake. Absent when this
    // device has no token registered, and absent for a connection with no
    // device row at all — there is nothing to report about nobody.
    let push_environment = match (device_id.as_deref(), device_token.is_some()) {
        // A freshly paired device: its row was created moments ago with no
        // token, so absence is the truth and no read is needed. Reading here
        // would also mean a database fault could sink a handshake that has
        // already minted a token the phone has not yet received.
        (Some(_), true) => None,
        // A reconnecting device: absence is a claim about its row that must be
        // read, never guessed. A failed read fails the ack — see the note on
        // this function — rather than fabricating "no token".
        (Some(device), false) => daemon
            .db
            .push_environment_for(device.to_string())
            .await
            .with_context(|| {
                format!("reading the push environment for {device} to build hello_ack")
            })?,
        // The static bootstrap connection has no device row to report on.
        (None, _) => None,
    };
    Ok(ServerMessage::HelloAck {
        protocol_version: protocol::PROTOCOL_VERSION,
        protocol_minor: protocol::PROTOCOL_MINOR,
        server_time: protocol::time::now_rfc3339(),
        capabilities: capabilities(daemon, tls_active, private_transport, device_id.is_some()),
        device_token,
        device_id,
        device_name,
        push_environment,
        machine_name: crate::machine::computer_name(),
        machine_os: Some(std::env::consts::OS.to_string()),
        machine_id: daemon.machine_id.get().cloned(),
    })
}

/// Has this connection's device been revoked since it said hello?
///
/// `None` is the static token, which has no revocation record — deleting
/// `~/.codeconnect/token` is how that one is retired, and existing connections
/// are expected to survive it.
///
/// **A failed lookup is treated as revoked.** It used to be treated as active,
/// on the reasoning that dropping every live connection over a SQLite hiccup
/// turns a transient error into a fleet-wide outage. That gets the direction of
/// failure exactly backwards: this check is an authorisation decision, and an
/// authorisation decision that cannot be made must not be resolved in the
/// caller's favour. The cost of being wrong is asymmetric — a false close costs
/// a reconnect, a false open leaves a revoked phone reading the event log and
/// answering approvals — and the reconnect itself has to authenticate against
/// the same database, so a genuinely broken store denies access either way
/// rather than quietly grandfathering whoever was already inside.
/// The `peers` frame for one reading of this computer's tailnet, told to the
/// connection from `viewer`, whose own Tailscale device is `viewer_node` when known.
fn peers_frame(
    daemon: &Daemon,
    tailnet: Option<&protocol::tailnet::Tailnet>,
    viewer: IpAddr,
    viewer_node: Option<&str>,
) -> ServerMessage {
    protocol::tailnet::peers_frame(
        tailnet,
        daemon.config.tailscale_bin().is_some(),
        daemon.config.ws_port,
        viewer,
        viewer_node,
    )
}

async fn revoked(daemon: &Arc<Daemon>, device_id: Option<&str>) -> bool {
    let Some(device_id) = device_id else {
        return false;
    };
    match daemon.db.device_is_active(device_id.to_string()).await {
        Ok(active) => !active,
        Err(err) => {
            crate::log_error!(
                "could not check revocation for {device_id} ({err:#}); closing the connection \
                 rather than assuming it is still authorised"
            );
            true
        }
    }
}

/// How a connection authenticated, for the log. Never includes a credential.
fn describe(outcome: &AuthOutcome) -> String {
    match outcome {
        AuthOutcome::Static => "the static token".to_string(),
        AuthOutcome::Device(device) => format!("device {} ({})", device.device_id, device.name),
        AuthOutcome::Paired {
            device_id,
            device_name,
            ..
        } => {
            format!("newly paired device {device_id} ({device_name})")
        }
        AuthOutcome::Rejected(reason) => format!("rejected: {reason}"),
        AuthOutcome::Revoked => "a revoked device's token".to_string(),
    }
}

/// A backlog replay the connection is part-way through.
///
/// Replay used to run to completion inside the arm that asked for it, and the
/// loop polled nothing else while it did. So the paging is a state the loop
/// steps through instead: one unit of work per pass,
/// competing for each pass with every other arm rather than shutting them all
/// out, and no bound at all on how long a replay may take, because a replay no
/// longer holds anything up.
///
/// **Nothing is lost by stepping, and that is the load-bearing part.**
///
/// - Live delivery is unchanged: [`live_delivery`] still delivers only
///   `seq == watermark + 1`, so a live event arriving during a replay is
///   delivered only when it is exactly the next one — which is in order — and the
///   replay then skips it from its page because the watermark has moved past it.
/// - An event dropped in favour of a replay cannot be lost. `Daemon::ingest`
///   appends to the store and only then broadcasts, both inside the session's
///   publish gate. A replay ends only on a page read that came back empty, and
///   every page read is issued from the arm *body* — that is, strictly after
///   every broadcast the loop has already handled. So the read that ends a replay
///   is issued after any event that replay dropped, and sees it.
/// - **This is why the page read must not be the arm's future.** As the future it
///   would be cancellable: a broadcast could be handled, and its event dropped,
///   while a read issued *before* the drop was still in flight — and if that read
///   came back empty the replay would end having never looked again.
///
/// What changes for the client is that the connection now reads the next client
/// message while a replay is pending, where it used to finish the backlog first.
/// Event frames keep their order against each other; a reply to a message sent
/// behind a `subscribe` can now arrive between two replayed events.
#[derive(Default)]
struct Backlog {
    /// Sessions still to be paged out, the one being paged now at the front.
    /// A session already queued is never queued twice: a replay reads from the
    /// watermark, so the one already asked for carries everything a second
    /// would have.
    sessions: std::collections::VecDeque<String>,
    /// Events read from the store for the front session and not yet sent.
    page: std::collections::VecDeque<Event>,
}

impl Backlog {
    /// Ask for `session_uid` to be paged out.
    ///
    /// Re-asking for the session already at the front **discards the buffered
    /// page**, because that page was read from the watermark this connection had
    /// at the time and the caller has just moved it. A client that subscribed
    /// from seq 900 and then re-subscribed from seq 100 would otherwise be
    /// handed a page holding only events above 900 — every one of which passes
    /// the watermark test — and 101..900 would be skipped in silence. Dropping
    /// the page costs one store read and cannot skip anything, because the next
    /// read is issued from the watermark now in force.
    ///
    /// A session queued behind the front has no page of its own yet, so there is
    /// nothing there to be stale.
    fn begin(&mut self, session_uid: String) {
        if self.sessions.front() == Some(&session_uid) {
            self.page.clear();
        } else if !self.replaying(&session_uid) {
            self.sessions.push_back(session_uid);
        }
    }

    /// Whether this session's backlog is queued or being paged out now.
    fn replaying(&self, session_uid: &str) -> bool {
        self.sessions.iter().any(|queued| queued == session_uid)
    }

    /// Whether there is any paging left to do — the condition on the arm that
    /// does it, so a connection with no backlog polls nothing extra.
    fn in_progress(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Abandon this session's replay, for a caller that has just taken the
    /// subscription away.
    ///
    /// The step below refuses to replay to a missing watermark anyway, so this is
    /// the eager half rather than the safe one. It is still needed: a delete
    /// followed by a fresh subscribe before the next step would leave the new
    /// subscription holding a page read against the old one.
    fn drop_session(&mut self, session_uid: &str) {
        if self
            .sessions
            .front()
            .is_some_and(|front| front == session_uid)
        {
            self.page.clear();
        }
        self.sessions.retain(|queued| queued != session_uid);
    }

    /// Do exactly one unit of work: send one event, or read one page, or finish
    /// one session. Never more, because everything else this connection owes is
    /// waiting on the pass that follows.
    async fn step<S>(
        &mut self,
        daemon: &Arc<Daemon>,
        sink: &mut S,
        watermarks: &mut HashMap<String, u64>,
        // The authenticated device, when there is one, so each successful send
        // can be recorded against it — the push gate's seen-filter is only as
        // true as this bookkeeping.
        device_id: Option<&str>,
    ) -> Result<()>
    where
        S: SinkExt<Message> + Unpin,
        <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
    {
        let Some(session_uid) = self.sessions.front().cloned() else {
            return Ok(());
        };
        let Some(watermark) = watermarks.get(&session_uid).copied() else {
            // No watermark is no subscription: `delete_session` removes it, and
            // replaying to a subscription that no longer exists is what the old
            // inline `unwrap_or(0)` did here — which restarted that session from
            // seq 0 and sent the phone the whole log it had just unsubscribed
            // from.
            self.page.clear();
            self.sessions.pop_front();
            return Ok(());
        };
        if let Some(event) = self.page.pop_front() {
            // `seq > watermark`, exactly as the inline replay behaved, and
            // deliberately not the `seq == watermark + 1` the live path uses. A
            // hole in the *store* would leave such a page's head permanently
            // unsendable: the watermark would never move, the same page would be
            // re-read for ever, and the connection would livelock. A store gap is
            // passed through instead, visible to the client in the seq it
            // carries, exactly as before.
            if event.seq > watermark {
                let seq = event.seq;
                send(sink, &ServerMessage::Event { event }).await?;
                watermarks.insert(session_uid.clone(), seq);
                // After the successful send, never before: "written to the
                // socket" is the only delivery this side can attest.
                if let Some(device) = device_id {
                    daemon.push_gate.note_delivered(device, &session_uid, seq);
                }
            }
            // Anything at or below the watermark the live arm has already sent,
            // which is the whole of why a live event during a replay is not a
            // duplicate.
            return Ok(());
        }
        // The page is spent. Read the next one from the watermark now in force,
        // so events the live arm delivered meanwhile are not re-read.
        let page = daemon
            .db
            .events_after(session_uid.clone(), watermark, REPLAY_PAGE)
            .await?;
        if page.is_empty() {
            self.sessions.pop_front();
        } else {
            self.page = page.into();
        }
        Ok(())
    }
}

/// Why a page ended early because its run is gone.
const SESSION_REMOVED: &str = "the session was removed while this page was being sent";

/// The earlier pages a connection owes, served one unit of work per pass.
///
/// Stepped for [`Backlog`]'s reason — a page is up to [`protocol::ws::MAX_HISTORY_PAGE`]
/// frames, and sending them inside the arm that asked would stop live delivery
/// and the replay for all of them — and kept apart from it because
/// a page is not a delivery in order: it moves no watermark, and it never tells
/// the push gate a device has seen anything, since events below a phone's window
/// say nothing about whether it has seen the newest ones.
///
/// **A second request for a run whose page is still being sent is queued behind
/// it**, not merged and not refused: requests are served in arrival order, and
/// each gets its own events and its own end. A phone sends one at a time, so this
/// is the simplest rule that still answers every request it could send.
#[derive(Default)]
struct History {
    /// Requests in arrival order, the one being served at the front.
    requests: VecDeque<HistoryRequest>,
}

/// One `history` request and how far it has got.
struct HistoryRequest {
    /// As the client sent it, so the end names the run the way it asked.
    session_id: String,
    /// The run the reference resolved to when the request arrived.
    session_uid: String,
    before_seq: u64,
    /// Already capped at [`protocol::ws::MAX_HISTORY_PAGE`].
    limit: u32,
    request_id: String,
    /// The page read from the store, oldest first; `None` until it is read.
    page: Option<VecDeque<Event>>,
    /// The lowest seq sent so far.
    first_seq: u64,
    /// Why the page ends early. Set when the run is unknown, when the store read
    /// fails, or when the run is removed while its page is being sent.
    error: Option<String>,
}

impl HistoryRequest {
    fn new(
        session_id: String,
        session_uid: std::result::Result<String, String>,
        before_seq: u64,
        limit: u32,
        request_id: String,
    ) -> HistoryRequest {
        let (session_uid, error) = match session_uid {
            Ok(uid) => (uid, None),
            Err(error) => (String::new(), Some(error)),
        };
        HistoryRequest {
            session_id,
            session_uid,
            before_seq,
            limit,
            request_id,
            page: None,
            first_seq: before_seq,
            error,
        }
    }
}

impl History {
    fn ask(&mut self, request: HistoryRequest) {
        self.requests.push_back(request);
    }

    fn in_progress(&self) -> bool {
        !self.requests.is_empty()
    }

    /// End every page of this run still owed, for a caller that has just removed
    /// it. Each still gets its end, carrying the reason, on its next step.
    fn drop_session(&mut self, session_uid: &str) {
        for request in &mut self.requests {
            if request.session_uid == session_uid && request.error.is_none() {
                request.page = None;
                request.error = Some(SESSION_REMOVED.into());
            }
        }
    }

    /// Do exactly one unit of work: read one page, send one event, or send one
    /// end.
    async fn step<S>(&mut self, daemon: &Arc<Daemon>, sink: &mut S) -> Result<()>
    where
        S: SinkExt<Message> + Unpin,
        <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
    {
        let Some(request) = self.requests.front_mut() else {
            return Ok(());
        };
        if request.error.is_none() {
            if let Some(event) = request.page.as_mut().and_then(VecDeque::pop_front) {
                request.first_seq = request.first_seq.min(event.seq);
                return send(sink, &ServerMessage::Event { event }).await;
            }
            // Asked before the page is read and again before its end is sent: a
            // run removed elsewhere — by another connection, or at the Mac — ends
            // its page with the reason, rather than as an empty or short page that
            // reads as the head of the log.
            request.error = match daemon.db.get_session(request.session_uid.clone()).await {
                Ok(Some(_)) => None,
                Ok(None) => Some(SESSION_REMOVED.into()),
                Err(err) => Some(format!("could not read the session: {err}")),
            };
            if request.error.is_none() && request.page.is_none() {
                match daemon
                    .db
                    .events_before(
                        request.session_uid.clone(),
                        request.before_seq,
                        request.limit,
                    )
                    .await
                {
                    Ok(page) => request.page = Some(page.into()),
                    Err(err) => {
                        crate::log_error!(
                            "could not read a history page of {}: {err:#}",
                            request.session_uid
                        );
                        request.error = Some(format!("could not read the log: {err}"));
                    }
                }
                return Ok(());
            }
        }
        let Some(request) = self.requests.pop_front() else {
            return Ok(());
        };
        send(
            sink,
            &ServerMessage::HistoryEnd {
                session_id: request.session_id,
                request_id: request.request_id,
                before_seq: request.before_seq,
                first_seq: request.first_seq,
                error: request.error,
            },
        )
        .await
    }
}

fn resync_marker(session_uid: &str, session_id: &str, skipped: u64) -> Event {
    marker(
        session_uid,
        session_id,
        serde_json::json!({
            "skipped": skipped,
            "reason": "client fell behind the live stream; the log is being replayed",
        }),
    )
}

/// The marker for an out-of-order arrival, as opposed to a slow client.
///
/// A distinct payload because the two are genuinely different problems: falling
/// behind the ring is a client that could not keep up, while a gap here is the
/// *daemon* publishing out of order. Collapsing them would hide a server bug
/// inside a client-shaped message.
fn gap_marker(event: &Event, watermark: u64) -> Event {
    marker(
        &event.session_uid,
        &event.session_id,
        serde_json::json!({
            "skipped": event.seq.saturating_sub(watermark + 1),
            "expected_seq": watermark + 1,
            "received_seq": event.seq,
            "reason": "an event arrived out of order; the log is being replayed from the last \
                       sequence this connection actually received",
        }),
    )
}

fn marker(session_uid: &str, session_id: &str, payload: serde_json::Value) -> Event {
    Event {
        // Never a logged fact: 0 is outside the seq space, so a client cannot
        // mistake a marker for a numbered event or advance a watermark past it.
        seq: 0,
        session_uid: session_uid.to_string(),
        session_id: session_id.to_string(),
        ts: protocol::time::now_rfc3339(),
        kind: EventKind::Resync,
        payload,
        source: Source::Daemon,
        source_event_id: None,
        turn_id: None,
        item_id: None,
    }
}

/// What this daemon will store for a registration, or the reason it will not.
///
/// `Ok` carries the whole of what belongs in the row: the token as it will be
/// stored — normalised here, so no caller can file a spelling the relay would
/// meter as a second binding — and the credential to keep beside it.
///
/// **The mode is the rule.** Relay mode has no way to send without a bearer, so
/// a credential-less registration is refused rather than filed: storing it would
/// leave a phone believing notifications were coming and a daemon unable to
/// send one, and nothing on either side would say so until somebody pressed the
/// test button. Direct mode is the mirror image — it needs no third party, and
/// a credential offered to it is discarded rather than written down, because a
/// bearer this Mac will never present is a secret kept for no reason. Off
/// refuses both: a registration that cannot be acted on is not a registration.
fn validated_registration(
    mode: crate::apns::PushMode,
    token: &str,
    relay_credential: Option<&Redacted>,
) -> Result<(String, Option<Redacted>), String> {
    let token = crate::apns::normalize_device_token(token).map_err(|err| format!("{err}"))?;
    match mode {
        crate::apns::PushMode::Off => {
            Err("push is not configured on this computer, so there is nowhere to register".into())
        }
        crate::apns::PushMode::Direct => Ok((token, None)),
        crate::apns::PushMode::Relay => {
            let credential = relay_credential.ok_or_else(|| {
                "this daemon sends through the push relay and needs a relay credential \
                 alongside the token"
                    .to_string()
            })?;
            // Validated on its exposed bytes and re-wrapped, so the raw bearer
            // lives only inside `checked_credential` and never as a bare
            // `String` a later edit could log. The check normalises nothing —
            // the relay minted it — so the value stored is the value sent.
            crate::apns::checked_credential(credential.expose()).map_err(|err| format!("{err}"))?;
            Ok((token, Some(credential.clone())))
        }
    }
}

pub(crate) fn capabilities(
    daemon: &Arc<Daemon>,
    tls_active: bool,
    private_transport: bool,
    paired: bool,
) -> Capabilities {
    // **One transport, named once.** `push` and `push_relay` are one-hot and
    // both are read off the mode rather than off a predicate: a phone has to
    // choose between registering a bare token and enrolling for a credential,
    // and two flags derived independently could tell it to do both or neither.
    let mode = daemon.push.mode();
    Capabilities {
        // True: an answer reaches the agent through its run's host, and one
        // that cannot be handed over is refused rather than dropped.
        can_approve_reliably: true,
        // Minor 7. A phone talking to an older daemon finds no such key in the
        // ack and reads that as false, so the swipe is absent rather than
        // present and broken. That default is the *phone's* — it is Swift and
        // never runs serde; `Capabilities.advertises` is where it lives. The
        // `#[serde(default)]` on this field is for Rust decoders only.
        delete_session: true,
        // Same liveness as `push`, but its own flag: a minor-6 daemon can send
        // pushes without understanding the test request, and the phone must be
        // able to tell those apart without version arithmetic. True in relay
        // mode as well, because a test is exactly how a relay path is proven.
        test_push: mode.is_configured(),
        send_text: true,
        // **Direct only, and false in relay mode on purpose.** A client that
        // predates `push_relay` reads this flag alone; a true here would send
        // it to ask for notification permission and register a token with no
        // credential attached, and every relayed send would be refused.
        push: mode == crate::apns::PushMode::Direct,
        push_relay: mode == crate::apns::PushMode::Relay,
        // What the listener holds, not what this connection used.
        tls: daemon.endpoint.tls,
        tls_active,
        diff: crate::git::git_bin(daemon.config.git_bin.as_deref()).is_some(),
        risk_class: true,
        session_uid: true,
        send_text_idempotent: true,
        prompt_identity: true,
        command_catalog: true,
        // A fact about this connection's wire, the same for every credential:
        // the caller decides it from TLS and the listener's address.
        private_transport,
        // **Derived from the agent list rather than written true**, so the flag
        // cannot outlive the ability it advertises: a stop is something this
        // daemon can perform only for a Codex session, and a build that stopped
        // hosting Codex would go on claiming a Stop button the phone could tap
        // for nothing.
        //
        // **And that is the whole of what it says.** This set is composed once per
        // connection and has no session in hand, so the flag is true on a Claude-only
        // fleet and true for a Codex run that cannot be stopped — see
        // `Capabilities::codex_interrupt`, which names what a client must add to it to
        // know whether one particular session can be stopped right now.
        codex_interrupt: daemon
            .supported_agents()
            .contains(&protocol::agent::AgentKind::Codex),
        // Composed from the same fact and carrying the same caveat: it says this build
        // understands the message, never that a particular session can be composed to.
        // See `Capabilities::codex_compose`.
        codex_compose: daemon
            .supported_agents()
            .contains(&protocol::agent::AgentKind::Codex),
        // **Omitted while the phone can do nothing with it.** This used to read
        // "omitted while it would only say Claude", and the set is no longer only
        // Claude: `supported_agents()` admits Codex now that a coordinator can
        // register one. The field stays empty anyway, because the reason to send it
        // was never the daemon's side — it is a diagnostic row a shipped phone
        // would render, and no shipped phone can act on "codex" yet. Left empty
        // here it is skipped on the wire, keeping the ack byte-identical to minor
        // 14; the daemon still knows its real set through `supported_agents()` for
        // the IPC negotiation, which is the consumer that actually gates on it.
        // This is populated for the phone with the client work that can render what
        // naming an agent opens onto.
        supported_agents: Vec::new(),
        question_card: true,
        hook_only_approvals: true,
        history_pages: true,
        // Starting and browsing are offered to every paired device the start
        // handler serves.
        start_session: paired,
        start_worktree: paired,
        browse_folders: paired,
        machine_stats: true,
        stop_run: true,
        // Stop reaches a Claude run CodeConnect started through its agent host; any other
        // Claude session is refused by the agent branch inside.
        claude_interrupt: true,
        // Each agent's accounts: kept and chosen by a paired device, as starting is.
        accounts: paired,
        // Files for a run's agent: sent by a paired device, as starting is.
        uploads: paired,
        // A run's shell: a paired device, over a transport nothing on the path can read.
        run_terminal: paired && private_transport,
        // A run's repository for its Changes panel: read for a paired device, as its diff's
        // actions will be.
        changes: paired,
        // The actions on a run's repository a paired device may send, one by one.
        git_actions: if paired {
            vec!["commit".to_string()]
        } else {
            Vec::new()
        },
    }
}

/// The answer to a `get_diff`: the run's diff fitted to one frame. Everything git could
/// not do, and an unknown run, is the diff's `note`, so the reply always names the run it
/// answers.
async fn diff_reply(daemon: &Arc<Daemon>, session_id: String) -> ServerMessage {
    let diff = daemon
        .diff(&session_id)
        .await
        .unwrap_or_else(|err| crate::git::Diff {
            unified: String::new(),
            truncated: false,
            note: Some(format!("{err}")),
            files: None,
            files_truncated: false,
        });
    let (files, files_truncated) = fit_files_in_a_frame(diff.files, diff.files_truncated);
    let (unified, truncated) =
        fit_diff_in_a_frame(diff.unified, diff.truncated, encoded_len(&files));
    ServerMessage::Diff {
        session_id,
        unified,
        truncated,
        // Stamped when the tree was read, not when the phone renders it: a diff without its
        // age is a fact without its freshness.
        captured_at: protocol::time::now_rfc3339(),
        note: diff.note,
        files,
        files_truncated,
    }
}

/// The attachment a terminal frame names, `None` for any other frame.
fn terminal_frame(message: &ClientMessage) -> Option<&str> {
    match message {
        ClientMessage::TerminalOpen { attachment_id, .. }
        | ClientMessage::TerminalInput { attachment_id, .. }
        | ClientMessage::TerminalResize { attachment_id, .. }
        | ClientMessage::TerminalCredit { attachment_id, .. }
        | ClientMessage::TerminalDetach { attachment_id } => Some(attachment_id),
        _ => None,
    }
}

fn terminal_closed(attachment_id: &str, reason: &str) -> ServerMessage {
    ServerMessage::TerminalClosed {
        attachment_id: attachment_id.to_string(),
        reason: reason.to_string(),
    }
}

/// One terminal frame from a paired device on a private transport, against this connection's attachment and the open
/// in flight. What it answers at once, if anything (a refusal or a close); everything else comes on the loop's
/// terminal arms. A frame for an attachment already gone is dropped: the client sent it before it learnt.
fn handle_terminal(
    daemon: &Arc<Daemon>,
    device_id: &str,
    message: ClientMessage,
    terminal: &mut Option<crate::terminal::Attachment>,
    opening: &mut Option<String>,
    opened: &tokio::sync::mpsc::UnboundedSender<(
        String,
        std::result::Result<crate::terminal::Opened, &'static str>,
    )>,
) -> Option<ServerMessage> {
    use base64::Engine as _;
    use protocol::ws::terminal_closed::PROTOCOL_ERROR;
    // The attachment this connection holds, if `attachment_id` names it.
    fn held<'a>(
        terminal: &'a mut Option<crate::terminal::Attachment>,
        attachment_id: &str,
    ) -> Option<&'a mut crate::terminal::Attachment> {
        terminal
            .as_mut()
            .filter(|attachment| attachment.id == attachment_id)
    }
    // A frame that broke the rules ends the attachment it names.
    let broken = |terminal: &mut Option<crate::terminal::Attachment>, attachment_id: &str| {
        *terminal = None;
        Some(terminal_closed(attachment_id, PROTOCOL_ERROR))
    };
    match message {
        ClientMessage::TerminalOpen {
            attachment_id,
            session_id,
            cols,
            rows,
            output_credit,
            resume_after,
            takeover,
        } => {
            if attachment_id.is_empty()
                || attachment_id.len() > protocol::ws::MAX_ATTACHMENT_ID_BYTES
            {
                return Some(terminal_closed(&attachment_id, PROTOCOL_ERROR));
            }
            // One terminal per connection: this open replaces the one before, which lets its shell go first.
            *terminal = None;
            *opening = Some(attachment_id.clone());
            let daemon = Arc::clone(daemon);
            let opened = opened.clone();
            let device_id = device_id.to_string();
            tokio::spawn(async move {
                let request = crate::terminal::OpenRequest {
                    attachment_id: attachment_id.clone(),
                    session_uid: session_id,
                    cols,
                    rows,
                    output_credit,
                    resume_after,
                    takeover,
                    device_id,
                };
                let result = crate::terminal::open(&daemon, request).await;
                let _ = opened.send((attachment_id, result));
            });
            None
        }
        ClientMessage::TerminalInput {
            attachment_id,
            data,
        } => {
            let attachment = held(terminal, &attachment_id)?;
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
                return broken(terminal, &attachment_id);
            };
            if attachment.input(bytes).is_err() {
                return broken(terminal, &attachment_id);
            }
            None
        }
        ClientMessage::TerminalResize {
            attachment_id,
            cols,
            rows,
        } => {
            let attachment = held(terminal, &attachment_id)?;
            if !protocol::ws::TERMINAL_COLS.contains(&cols)
                || !protocol::ws::TERMINAL_ROWS.contains(&rows)
            {
                return broken(terminal, &attachment_id);
            }
            attachment.resize(cols, rows);
            None
        }
        ClientMessage::TerminalCredit {
            attachment_id,
            bytes,
        } => {
            let attachment = held(terminal, &attachment_id)?;
            if attachment.credit(bytes).is_err() {
                return broken(terminal, &attachment_id);
            }
            None
        }
        ClientMessage::TerminalDetach { attachment_id } => {
            if held(terminal, &attachment_id).is_some() {
                *terminal = None;
            }
            if opening.as_deref() == Some(attachment_id.as_str()) {
                *opening = None;
            }
            None
        }
        _ => None,
    }
}

/// Serialise and send, never exceeding the frame the client agreed to read.
///
/// Incoming messages are capped at [`MAX_CLIENT_MESSAGE_BYTES`] by the
/// WebSocket config; outgoing ones were not capped at all. That asymmetry is
/// not cosmetic: `URLSessionWebSocketTask` — what the iOS client uses — defaults
/// to a 1MB maximum message and **fails the connection** rather than the
/// message when one arrives over it. A single oversized event therefore does
/// not lose one fact, it disconnects the phone, and the phone reconnects,
/// replays, and hits the same event again. That is a disconnect loop in which
/// the client can never get past the event, and it is reachable from an event
/// payload larger than the diff path's own limit.
///
/// So an oversized frame is replaced by a same-shaped message the client can
/// read, saying what happened. The fact stays in the log with its seq, the
/// watermark still advances, and the phone can fetch the detail another way —
/// which is strictly better than a connection that cannot make progress.
async fn send<S>(sink: &mut S, message: &ServerMessage) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    let text = serde_json::to_string(message)?;
    let text = if text.len() > MAX_CLIENT_MESSAGE_BYTES {
        crate::log_warn!(
            "ws: an outbound message of {} bytes exceeds the {MAX_CLIENT_MESSAGE_BYTES}-byte \
             frame limit; sending a placeholder instead of failing the connection",
            text.len()
        );
        oversized_placeholder(message, text.len())?
    } else {
        text
    };
    write_bounded(sink, Message::Text(text)).await
}

/// Send one frame under [`WRITE_DEADLINE`]. A write that does not complete in
/// that window means the peer has stopped reading; the connection is closed
/// rather than left wedged inside an un-cancellable `send`, which is what keeps
/// revocation and teardown promptly reachable.
async fn write_bounded<S>(sink: &mut S, message: Message) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    tokio::time::timeout(write_deadline(), sink.send(message))
        .await
        .context("websocket write stalled past the deadline; peer not reading")?
        .context("websocket write failed")
}

/// The stand-in for a message too large to send.
///
/// An `Event` keeps its identity — seq, session, kind, timestamps — and loses
/// only its payload, so the client's watermark advances exactly as it would
/// have and the timeline shows a fact that happened rather than a hole. Anything
/// else becomes an `Error`, because the alternative is silence.
fn oversized_placeholder(message: &ServerMessage, bytes: usize) -> Result<String> {
    let replacement = match message {
        ServerMessage::Event { event } => ServerMessage::Event {
            event: Event {
                // The keys ingest writes for an oversized payload, which are the
                // ones the phone reads.
                payload: serde_json::json!({
                    "_codeconnect_truncated": true,
                    "_original_bytes": bytes,
                    "reason": "this event was larger than one WebSocket frame; its payload was \
                               dropped so the connection could continue",
                }),
                ..event.clone()
            },
        },
        _ => ServerMessage::Error {
            code: "message_too_large".into(),
            message: format!(
                "a {bytes}-byte reply exceeded the {MAX_CLIENT_MESSAGE_BYTES}-byte frame limit \
                 and was not sent"
            ),
        },
    };
    let text = serde_json::to_string(&replacement)?;
    // A placeholder that is itself too large would reintroduce the bug it
    // exists to remove. Only reachable if an event's *identity* fields are
    // enormous, which nothing here generates — but "nothing generates it" is
    // not a bound.
    if text.len() > MAX_CLIENT_MESSAGE_BYTES {
        return Ok(serde_json::to_string(&ServerMessage::Error {
            code: "message_too_large".into(),
            message: "a reply exceeded the frame limit and was not sent".into(),
        })?);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A redirected `$HOME` for the length of one test, for the same reason
    /// `state.rs`'s revoke tests take one: `Daemon::revoke` sweeps
    /// `$HOME/.ssh/authorized_keys` unconditionally, so a test that revokes
    /// under the developer's own `$HOME` points a function whose job is
    /// deleting lines from `authorized_keys` at the developer's
    /// `authorized_keys`. It is also what keeps the sweep's own seams honest:
    /// `FakeHome` holds a process-wide lock so that only one test at a time is
    /// inside a sweep, and a test that skips it is a second sweep running
    /// through the hook another test armed — which is a flake in that test and
    /// nothing at all in this one, so it has to be taken here rather than left
    /// to whoever debugs it later.
    ///
    /// Named for the test, so a leftover directory in `$TMPDIR` says who left it.
    fn redirected_home(tag: &str) -> crate::legacy_credentials::test_support::FakeHome {
        crate::legacy_credentials::test_support::FakeHome::new(tag)
    }

    /// **Give the daemon a row for a run, hosted by Claude.**
    ///
    /// The agent is a fact about the run, and the run is a row: a daemon that has
    /// never heard of a uid cannot say what is running under it, and refuses
    /// rather than guessing.
    fn hosting_a_claude_run(daemon: &Arc<Daemon>, uid: &str) {
        let now = protocol::time::now_rfc3339();
        let row = crate::store::SessionRow {
            session_uid: uid.to_string(),
            session_id: "cc-term".into(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: protocol::event::Lifecycle::Live,
            created_at: now.clone(),
            updated_at: now,
            agent: protocol::agent::AgentKind::Claude,
            codex_thread_id: None,
        };
        assert_eq!(daemon.store.begin_run(&row, uid, None, None).unwrap(), None);
    }

    /// A daemon with a private store holding exactly one paired device.
    fn daemon_with_a_device() -> (Arc<Daemon>, String) {
        let store = Arc::new(crate::store::Store::open_temp("ccd-ws"));
        let now = protocol::time::now_rfc3339();
        let device_id = "abcd1234".to_string();
        store
            .insert_device(&device_id, "iPhone", "not-a-real-hash", &now, None)
            .unwrap();

        let daemon = Daemon::new(
            protocol::config::Config::default(),
            store,
            Arc::new(crate::apns::LoggingPushSender::new()),
            crate::state::Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
        );
        (daemon, device_id)
    }

    #[tokio::test]
    async fn a_database_error_closes_the_connection_instead_of_admitting_it() {
        // The defect: `device_is_active` returning `Err` was read as "still
        // active", so a phone that was already connected kept reading the event
        // log and answering approvals for as long as the store stayed broken.
        // An authorisation question that cannot be answered must not be
        // resolved in the caller's favour — the cost is asymmetric, a false
        // close is a reconnect and a false open is a revoked device inside.
        let (daemon, device_id) = daemon_with_a_device();
        assert!(
            !revoked(&daemon, Some(&device_id)).await,
            "a live device must not be treated as revoked"
        );

        daemon.store.break_device_lookups_for_tests();
        assert!(
            daemon.store.device_is_active(&device_id).is_err(),
            "the fixture must actually break the lookup"
        );
        assert!(
            revoked(&daemon, Some(&device_id)).await,
            "an unanswerable revocation check must fail closed"
        );
    }

    #[tokio::test]
    async fn the_static_token_is_unaffected_by_a_database_error() {
        // `None` is the static bootstrap token, which has no revocation record
        // at all: deleting `~/.codeconnect/token` is how that one is retired,
        // and SECURITY.md says open connections deliberately survive it.
        // Failing closed on device lookups must not change that.
        let (daemon, _) = daemon_with_a_device();
        daemon.store.break_device_lookups_for_tests();
        assert!(!revoked(&daemon, None).await);
    }

    #[tokio::test]
    async fn revoking_publishes_a_cancellation_the_same_instant() {
        // Revocation used to reach an idle socket only at its next keepalive,
        // up to 30 seconds later. `codeconnect revoke` does not mean "in a while".
        let _home = redirected_home("ws-revoke-cancels");
        let (daemon, device_id) = daemon_with_a_device();
        let mut cancellations = daemon.revocations_tx.subscribe();

        let outcome = daemon.revoke(&device_id).await.unwrap();
        assert!(outcome.token_revoked);
        assert_eq!(
            cancellations.try_recv().unwrap(),
            device_id,
            "the revoked device id must be published for open sockets to match on"
        );
    }

    fn ip(last: u8) -> std::net::IpAddr {
        std::net::IpAddr::from([100, 64, 0, last])
    }

    #[test]
    fn plaintext_is_trusted_only_on_loopback_or_this_nodes_tailnet_address() {
        let tailnet: Vec<IpAddr> = vec![
            "100.64.12.34".parse().unwrap(),
            "fd7a:115c:a1e0::1".parse().unwrap(),
        ];
        // Loopback never leaves the machine.
        assert_eq!(
            plaintext_trust("127.0.0.1".parse().unwrap(), &tailnet),
            PlaintextTrust::TrustedPath
        );
        // This node's own tailnet addresses: WireGuard carries the bytes.
        assert_eq!(
            plaintext_trust("100.64.12.34".parse().unwrap(), &tailnet),
            PlaintextTrust::TrustedPath
        );
        assert_eq!(
            plaintext_trust("fd7a:115c:a1e0::1".parse().unwrap(), &tailnet),
            PlaintextTrust::TrustedPath
        );
        // A LAN address, an unspecified bind, and a CGNAT-shaped address that
        // is NOT this node's: all TLS-only. The last one is the trap — being
        // in 100.64/10 proves nothing about who carries the packets.
        assert_eq!(
            plaintext_trust("192.168.1.20".parse().unwrap(), &tailnet),
            PlaintextTrust::RequireTls
        );
        assert_eq!(
            plaintext_trust("0.0.0.0".parse().unwrap(), &tailnet),
            PlaintextTrust::RequireTls
        );
        assert_eq!(
            plaintext_trust("100.64.99.99".parse().unwrap(), &tailnet),
            PlaintextTrust::RequireTls
        );
        // No tailscale at all: loopback stays usable, nothing else does.
        assert_eq!(
            plaintext_trust("127.0.0.1".parse().unwrap(), &[]),
            PlaintextTrust::TrustedPath
        );
        assert_eq!(
            plaintext_trust("100.64.12.34".parse().unwrap(), &[]),
            PlaintextTrust::RequireTls
        );
    }

    /// The admission gate, live: a listener that requires TLS drops a
    /// plaintext connection before the WebSocket handshake — the credential
    /// exchange is never reached.
    #[tokio::test]
    async fn a_tls_only_listener_refuses_plaintext_before_the_handshake() {
        let (daemon, _device) = daemon_with_a_device();
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let token = Arc::new("static-token-for-tests".to_string());
        // Ended with the test rather than forgotten: a spawned accept loop holds
        // its listener's descriptor for the rest of the process.
        let accepting = tokio::spawn(async move {
            accept_loop(daemon, listener, token, None, PlaintextTrust::RequireTls).await
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let url = format!("ws://{addr}/");
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::client_async(&url, stream),
        )
        .await
        .expect("the refusal is immediate, not a hang");
        assert!(
            refused.is_err(),
            "a plaintext upgrade on a TLS-only listener must fail"
        );
        accepting.abort();
    }

    // --------------------------------------------------- live socket harness

    /// A real listener on loopback, speaking the real protocol.
    ///
    /// The three properties below — a refused major, a cap that actually
    /// refuses, a revocation that closes an idle socket — are all decided in
    /// `handle_client`'s select loop, and a unit test of the predicate under
    /// each one would prove the predicate rather than the behaviour. This costs
    /// one ephemeral port.
    struct LiveServer {
        addr: SocketAddr,
        daemon: Arc<Daemon>,
        token: Arc<String>,
        /// The same database the daemon writes, so a test can read what a
        /// registration actually stored rather than what it hoped it did.
        store: Arc<crate::store::Store>,
        /// The accept loop, ended when the test that started it is over.
        ///
        /// It used to be spawned and forgotten, which leaks the listener's
        /// descriptor for the rest of the process — permanently, since nothing
        /// closes it.
        accepting: tokio::task::JoinHandle<Result<()>>,
    }

    impl Drop for LiveServer {
        fn drop(&mut self) {
            self.accepting.abort();
        }
    }

    async fn live_server(config: protocol::config::Config) -> (LiveServer, String) {
        live_server_pushing(config, Arc::new(crate::apns::LoggingPushSender::new())).await
    }

    /// The same listener, over a daemon whose push is whatever the test needs it
    /// to be — the mode is what every capability and registration rule below
    /// turns on.
    async fn live_server_pushing(
        config: protocol::config::Config,
        push: Arc<dyn crate::apns::PushSender>,
    ) -> (LiveServer, String) {
        live_server_on(config, push, PlaintextTrust::TrustedPath).await
    }

    /// The same listener, classified as `trust` says, as `serve` classifies a real one.
    async fn live_server_on(
        config: protocol::config::Config,
        push: Arc<dyn crate::apns::PushSender>,
        trust: PlaintextTrust,
    ) -> (LiveServer, String) {
        let (daemon, device_id, store, _path) = daemon_with_a_device_pushing(config, push);
        // Bound here rather than inside `serve` so the test learns the port.
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let token = Arc::new("static-token-for-tests".to_string());
        let accepting = {
            let daemon = Arc::clone(&daemon);
            let token = Arc::clone(&token);
            tokio::spawn(async move { accept_loop(daemon, listener, token, None, trust).await })
        };
        (
            LiveServer {
                addr,
                daemon,
                token,
                store,
                accepting,
            },
            device_id,
        )
    }

    impl LiveServer {
        /// Connect, `hello`, and return the socket plus the first reply.
        async fn hello(
            &self,
            protocol_version: u32,
            device_token: Option<&str>,
        ) -> (
            tokio_tungstenite::WebSocketStream<TcpStream>,
            serde_json::Value,
        ) {
            let stream = TcpStream::connect(self.addr).await.unwrap();
            let url = format!("ws://{}/", self.addr);
            let (mut socket, _) = tokio_tungstenite::client_async(&url, stream).await.unwrap();
            let hello = serde_json::json!({
                "type": "hello",
                "protocol_version": protocol_version,
                "token": device_token.unwrap_or(self.token.as_str()),
                "client_name": "test",
            });
            socket.send(Message::Text(hello.to_string())).await.unwrap();
            let reply = next_json(&mut socket).await.expect("a reply to hello");
            (socket, reply)
        }

        /// The same handshake, carrying the `features` key no build reads any more.
        ///
        /// A separate method rather than a parameter on [`LiveServer::hello`]: every
        /// other test here sends what every phone in the field sends, and threading
        /// a `None` through all of them would make the ordinary case read like the
        /// exception.
        async fn hello_advertising(
            &self,
            device_token: &str,
            features: serde_json::Value,
        ) -> (
            tokio_tungstenite::WebSocketStream<TcpStream>,
            serde_json::Value,
        ) {
            let stream = TcpStream::connect(self.addr).await.unwrap();
            let url = format!("ws://{}/", self.addr);
            let (mut socket, _) = tokio_tungstenite::client_async(&url, stream).await.unwrap();
            socket
                .send(Message::Text(
                    hello_frame(device_token, features).to_string(),
                ))
                .await
                .unwrap();
            let reply = next_json(&mut socket).await.expect("a reply to hello");
            (socket, reply)
        }
    }

    /// A `hello` at the current protocol, on the device token these tests pair with,
    /// carrying the `features` key no build reads any more. Used for both the
    /// handshake hello and the second one sent down an already-authenticated socket
    /// — the two are different handlers, and the test below holds both.
    fn hello_frame(token: &str, features: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "hello",
            "protocol_version": protocol::PROTOCOL_VERSION,
            "token": token,
            "client_name": "test",
            "features": features,
        })
    }

    /// How a read of the next text frame ended.
    ///
    /// Three outcomes and not two, because silence and closure are different
    /// facts and two tests here turn on the difference: they assert that a
    /// refused client's socket is *closed* rather than merely quiet, which is a
    /// claim a reader that collapsed the two could not make.
    enum Frame {
        Json(serde_json::Value),
        /// The peer closed, or the stream failed.
        Closed,
        /// Nothing arrived in the time allowed.
        Silent,
    }

    /// Read one text frame, waiting no longer than `within`.
    ///
    /// The deadline is absolute rather than per poll, so a keepalive ping — which
    /// is skipped rather than returned — cannot extend the wait a caller asked to
    /// be bounded. A `peers` frame is skipped the same way: every paired device is
    /// sent one after its ack, unasked, and the tests of it read it with
    /// [`next_peers`].
    async fn next_frame(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        within: Duration,
    ) -> Frame {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, socket.next()).await {
                Err(_) => return Frame::Silent,
                Ok(Some(Ok(Message::Text(text)))) => {
                    let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if frame["type"] == "peers" {
                        continue;
                    }
                    return Frame::Json(frame);
                }
                Ok(Some(Ok(Message::Close(_))) | None) => return Frame::Closed,
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) => return Frame::Closed,
            }
        }
    }

    /// The next text frame, decoded. `None` at close.
    ///
    /// Silence is a panic, not a `None`, and that is what almost every test here
    /// wants: a server that stops answering is a failure, and one that says so
    /// beats a suite that waits. Collapsing it into `None` would also quietly
    /// weaken the two tests that read `None` as "the connection was closed".
    async fn next_json(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    ) -> Option<serde_json::Value> {
        match next_frame(socket, Duration::from_secs(5)).await {
            Frame::Json(value) => Some(value),
            Frame::Closed => None,
            Frame::Silent => panic!("the server must answer within 5s"),
        }
    }

    /// The next text frame, decoded, or `None` for silence as well as closure.
    ///
    /// For the tests that are waiting on a *state* rather than on a frame, and so
    /// have to be able to look, find nothing, and act again. Sharing
    /// [`next_frame`] with [`next_json`] is what keeps the two policies one
    /// reader: the difference between them is the answer to silence and nothing
    /// else.
    async fn next_json_within(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        within: Duration,
    ) -> Option<serde_json::Value> {
        match next_frame(socket, within).await {
            Frame::Json(value) => Some(value),
            Frame::Closed | Frame::Silent => None,
        }
    }

    /// A live Codex run begun as the engine begins one and taking mail, on a daemon
    /// a test drives; no link is attached, so a message to it waits.
    async fn codex_run(daemon: &Arc<Daemon>, uid: &str, name: &str) {
        let now = protocol::time::now_rfc3339();
        let row = crate::store::SessionRow {
            session_uid: uid.to_string(),
            session_id: name.into(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: protocol::event::Lifecycle::Live,
            created_at: now.clone(),
            updated_at: now,
            agent: protocol::agent::AgentKind::Codex,
            codex_thread_id: None,
        };
        assert_eq!(daemon.store.begin_run(&row, uid, None, None).unwrap(), None);
        daemon.mail_run_live(uid).await;
    }

    /// The next text frame, `peers` included, or `None` for silence or closure.
    async fn next_raw(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        within: Duration,
    ) -> Option<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => return serde_json::from_str(&text).ok(),
                Ok(Some(Ok(Message::Close(_))) | Some(Err(_)) | None) | Err(_) => return None,
                Ok(Some(Ok(_))) => continue,
            }
        }
    }

    /// The next `peers` frame within `within`, past any other.
    async fn next_peers(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        within: Duration,
    ) -> Option<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let frame = next_raw(socket, left).await?;
            if frame["type"] == "peers" {
                return Some(frame);
            }
        }
    }

    /// The recorded tailnet with `edit` applied, in which "laptop" is this test's
    /// loopback, so a connection from here comes from the owner's laptop.
    fn tailnet_from_the_laptop(
        edit: impl FnOnce(&mut serde_json::Value),
    ) -> protocol::tailnet::Tailnet {
        let mut status: serde_json::Value = serde_json::from_str(
            &include_str!("../../../fixtures/tailscale/status-six-nodes.json")
                .replace("100.64.0.6", "127.0.0.1"),
        )
        .unwrap();
        edit(&mut status);
        protocol::tailnet::Tailnet::parse(&status.to_string()).unwrap()
    }

    fn set_tailnet(server: &LiveServer, tailnet: Option<protocol::tailnet::Tailnet>) {
        *server
            .daemon
            .test_tailnet
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = tailnet;
    }

    /// A hello with neither a token nor a code, from this test's loopback.
    async fn hello_with_no_credential(
        server: &LiveServer,
    ) -> (
        tokio_tungstenite::WebSocketStream<TcpStream>,
        serde_json::Value,
    ) {
        let stream = TcpStream::connect(server.addr).await.unwrap();
        let url = format!("ws://{}/", server.addr);
        let (mut socket, _) = tokio_tungstenite::client_async(&url, stream).await.unwrap();
        let hello = serde_json::json!({
            "type": "hello",
            "protocol_version": protocol::PROTOCOL_VERSION,
            "client_name": "iPad",
        });
        socket.send(Message::Text(hello.to_string())).await.unwrap();
        let reply = next_raw(&mut socket, Duration::from_secs(5))
            .await
            .expect("a reply to hello");
        (socket, reply)
    }

    /// One hello, sent as `hello`, from this test's loopback; the daemon's first reply.
    async fn hello_raw(server: &LiveServer, hello: serde_json::Value) -> serde_json::Value {
        let stream = TcpStream::connect(server.addr).await.unwrap();
        let url = format!("ws://{}/", server.addr);
        let (mut socket, _) = tokio_tungstenite::client_async(&url, stream).await.unwrap();
        socket.send(Message::Text(hello.to_string())).await.unwrap();
        next_raw(&mut socket, Duration::from_secs(5))
            .await
            .expect("a reply to hello")
    }

    async fn device_name(server: &LiveServer, device_id: &str) -> String {
        let devices = server.daemon.list_devices().await.unwrap();
        devices
            .into_iter()
            .find(|d| d.device_id == device_id)
            .expect("the device is listed")
            .name
    }

    /// **A device is listed under what it calls itself (minor 44), over the real listener.** A new device takes
    /// its `device_name`; a paired one is renamed on its next hello when the name differs; a hello naming nothing
    /// keeps the name the device has, as every client before minor 44 does. Must-fail: drop `device_name` from
    /// the name `authenticate` is given, or the rename after a token hello, and the device reads "iPad" or the
    /// old name.
    #[tokio::test]
    async fn a_device_is_listed_under_what_it_calls_itself_and_renamed_on_its_next_hello() {
        let (server, _device) = live_server(protocol::config::Config::default()).await;
        set_tailnet(&server, Some(tailnet_from_the_laptop(|_| {})));
        let hello = |extra: serde_json::Value| {
            let mut hello = serde_json::json!({
                "type": "hello",
                "protocol_version": protocol::PROTOCOL_VERSION,
                "client_name": "CodeConnect Mac",
            });
            hello
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            hello
        };

        let ack = hello_raw(
            &server,
            hello(serde_json::json!({"device_name": "Jordan’s MacBook Air"})),
        )
        .await;
        assert_eq!(ack["type"], "hello_ack", "{ack}");
        assert_eq!(ack["device_name"], "Jordan’s MacBook Air", "{ack}");
        let id = ack["device_id"].as_str().unwrap().to_string();
        let token = ack["device_token"].as_str().unwrap().to_string();

        let again = hello_raw(
            &server,
            hello(serde_json::json!({"token": token, "device_name": "Air"})),
        )
        .await;
        assert_eq!(again["type"], "hello_ack", "{again}");
        assert_eq!(
            device_name(&server, &id).await,
            "Air",
            "renamed on its next hello"
        );

        let older = hello_raw(&server, hello(serde_json::json!({"token": token}))).await;
        assert_eq!(older["type"], "hello_ack", "{older}");
        assert_eq!(
            device_name(&server, &id).await,
            "Air",
            "a hello naming nothing keeps the name"
        );
    }

    /// **A device linked by an address that is not its tailnet address is not told about itself**, over the real
    /// listener: its token carries the Tailscale device it was seen from, and that node is left out of its `peers`
    /// frame. Here the device comes from loopback and the tailnet puts the laptop elsewhere, as a Mac paired by its
    /// LAN address would be. Must-fail: pass `None` for the viewer's node, and the laptop is listed to itself.
    #[tokio::test]
    async fn a_device_linked_by_a_lan_address_is_not_listed_to_itself() {
        // The device `live_server` pairs, by its token (its id is what `live_server` returns).
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let device = "device-token-for-tests";
        let mut status: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/tailscale/status-six-nodes.json"
        ))
        .unwrap();
        for node in status["Peer"].as_object_mut().unwrap().values_mut() {
            if node["HostName"] == "linux-box" {
                node["Online"] = true.into();
            }
        }
        let laptop = status["Peer"]
            .as_object()
            .unwrap()
            .values()
            .find(|node| node["HostName"] == "laptop")
            .and_then(|node| node["ID"].as_str())
            .expect("the laptop's node id")
            .to_string();
        set_tailnet(
            &server,
            Some(protocol::tailnet::Tailnet::parse(&status.to_string()).unwrap()),
        );
        server
            .daemon
            .db
            .stamp_tailnet_node(protocol::hash::sha256_hex(device.as_bytes()), laptop)
            .await
            .unwrap();

        let stream = TcpStream::connect(server.addr).await.unwrap();
        let url = format!("ws://{}/", server.addr);
        let (mut socket, _) = tokio_tungstenite::client_async(&url, stream).await.unwrap();
        let hello = serde_json::json!({
            "type": "hello", "protocol_version": protocol::PROTOCOL_VERSION, "token": device, "client_name": "CodeConnect Mac",
        });
        socket.send(Message::Text(hello.to_string())).await.unwrap();
        let peers = next_peers(&mut socket, Duration::from_secs(5))
            .await
            .expect("a peers frame after the ack");
        let names: Vec<&str> = peers["computers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"laptop"), "listed to itself: {names:?}");
        assert!(
            names.contains(&"linux-box"),
            "the other computer is still listed: {names:?}"
        );
    }

    /// **The owner's own Tailscale device signs in with no code, over the real
    /// listener, and is told the owner's other computers:** the frame
    /// `fixtures/wire/peers.json` records, right after its ack, and again when the
    /// tailnet changes, and not when it does not.
    #[tokio::test]
    async fn the_owners_tailscale_device_signs_in_with_no_code_and_learns_the_other_computers() {
        let (server, _device) = live_server(protocol::config::Config::default()).await;
        set_tailnet(&server, Some(tailnet_from_the_laptop(|_| {})));

        let (mut socket, ack) = hello_with_no_credential(&server).await;
        assert_eq!(ack["type"], "hello_ack", "{ack}");
        assert_eq!(
            ack["device_token"].as_str().map(str::len),
            Some(64),
            "{ack}"
        );
        // The recorded frame, less this test's own computer: a computer is never
        // told about itself.
        let mut expected: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/wire/peers.json")).unwrap();
        let expected = {
            let running = &mut expected["running"];
            running["computers"] = serde_json::json!([]);
            running.clone()
        };
        // Right after the ack, not at the first re-read: the ack and this frame go
        // out back to back, and the first re-read is a whole `PEERS_REFRESH` away.
        assert_eq!(
            next_raw(&mut socket, PEERS_REFRESH / 2).await,
            Some(expected),
            "the frame right after the ack"
        );

        set_tailnet(
            &server,
            Some(tailnet_from_the_laptop(|status| {
                for node in status["Peer"].as_object_mut().unwrap().values_mut() {
                    if node["HostName"] == "linux-box" {
                        node["Online"] = true.into();
                    }
                }
            })),
        );
        let changed = next_peers(&mut socket, Duration::from_secs(5))
            .await
            .expect("a changed tailnet is sent");
        let names: Vec<&str> = changed["computers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|computer| computer["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["linux-box"]);
        assert_eq!(
            next_peers(&mut socket, PEERS_REFRESH * 4).await,
            None,
            "an unchanged tailnet is not sent again"
        );
    }

    /// **A computer removed from this one is no longer told about**: the next
    /// re-read of the tailnet drops it from an open connection's `peers`.
    #[tokio::test]
    async fn a_removed_computer_drops_out_of_the_peers_a_viewer_is_told() {
        let (server, _device) = live_server(protocol::config::Config::default()).await;
        set_tailnet(
            &server,
            Some(tailnet_from_the_laptop(|status| {
                for node in status["Peer"].as_object_mut().unwrap().values_mut() {
                    if node["HostName"] == "linux-box" {
                        node["Online"] = true.into();
                    }
                }
            })),
        );
        let (mut socket, ack) = hello_with_no_credential(&server).await;
        assert_eq!(ack["type"], "hello_ack", "{ack}");
        let names = |frame: serde_json::Value| -> Vec<String> {
            frame["computers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|computer| computer["name"].as_str().unwrap().to_string())
                .collect()
        };
        let first = next_peers(&mut socket, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(names(first), ["linux-box"]);

        // linux-box signed in once through Tailscale, and is removed.
        server
            .daemon
            .store
            .insert_device(
                "lb",
                "linux-box",
                "hash-lb",
                "2026-11-08T00:00:00Z",
                Some("n0000000003CNTRL"),
            )
            .unwrap();
        server.daemon.revoke("lb").await.unwrap();
        let after = next_peers(&mut socket, Duration::from_secs(5))
            .await
            .expect("the change is sent");
        assert!(names(after).is_empty());
    }

    /// **A hello with no credential is refused unless Tailscale vouches for the
    /// address it came from:** with no tailnet, from another user's device, from a
    /// tagged one. A code still pairs from there, and nothing else was added.
    #[tokio::test]
    async fn a_hello_with_no_credential_is_refused_unless_tailscale_vouches_for_its_address() {
        let (server, _device) = live_server(protocol::config::Config::default()).await;
        let laptop = |field: &'static str, value: serde_json::Value| {
            tailnet_from_the_laptop(move |status| {
                for node in status["Peer"].as_object_mut().unwrap().values_mut() {
                    if node["HostName"] == "laptop" {
                        node[field] = value.clone();
                    }
                }
            })
        };
        for (case, tailnet) in [
            ("no tailnet", None),
            ("another user's device", Some(laptop("UserID", 9999.into()))),
            (
                "a tagged device",
                Some(laptop("Tags", serde_json::json!(["tag:server"]))),
            ),
        ] {
            set_tailnet(&server, tailnet);
            let (_socket, reply) = hello_with_no_credential(&server).await;
            assert_eq!(reply["type"], "error", "{case}: {reply}");
            assert_eq!(reply["code"], "unauthorized", "{case}: {reply}");
        }
        assert_eq!(server.daemon.list_devices().await.unwrap().len(), 1);

        let (code, _) = server.daemon.create_pairing(300).await.unwrap();
        let stream = TcpStream::connect(server.addr).await.unwrap();
        let url = format!("ws://{}/", server.addr);
        let (mut socket, _) = tokio_tungstenite::client_async(&url, stream).await.unwrap();
        let hello = serde_json::json!({
            "type": "hello",
            "protocol_version": protocol::PROTOCOL_VERSION,
            "pairing_code": code,
            "client_name": "iPad",
        });
        socket.send(Message::Text(hello.to_string())).await.unwrap();
        let ack = next_json(&mut socket).await.unwrap();
        assert_eq!(ack["type"], "hello_ack", "a code still pairs: {ack}");
        assert_eq!(server.daemon.list_devices().await.unwrap().len(), 2);
    }

    /// **Block → Allow, proven at the wire.** A blocked pairing's own token is answered
    /// `revoked` — not `unauthorized` — so the phone keeps it and waits; an unknown token
    /// stays `unauthorized`, so `revoked` is reachable only by a token that matches a
    /// blocked pairing; and once the pairing is unblocked the same token is admitted
    /// again, with no new code. This is the daemon half of the contract the app's
    /// reconnect rests on, read end to end from the handshake frame rather than from a
    /// fixture the app wrote itself: it fails if `TokenAuth::Revoked` is mapped to
    /// `Rejected` in `state.rs`, or if `ws_server` sends `unauthorized` for
    /// `AuthOutcome::Revoked`.
    #[tokio::test]
    async fn a_blocked_pairings_token_is_answered_revoked_and_readmitted_once_unblocked() {
        let (server, device_id) = live_server(protocol::config::Config::default()).await;
        let token = "device-token-for-tests";

        // Live: the held token admits.
        let (_s, ack) = server.hello(protocol::PROTOCOL_VERSION, Some(token)).await;
        assert_eq!(
            ack["type"], "hello_ack",
            "the held token admits while live: {ack}"
        );

        // Blocked: the same token is answered `revoked`, not `unauthorized`.
        server
            .store
            .revoke_device(&device_id, &protocol::time::now_rfc3339())
            .unwrap();
        let (_s, blocked) = server.hello(protocol::PROTOCOL_VERSION, Some(token)).await;
        assert_eq!(
            blocked["type"], "error",
            "a blocked token is refused: {blocked}"
        );
        assert_eq!(
            blocked["code"], "revoked",
            "a blocked pairing's own token is answered `revoked`: {blocked}"
        );

        // A stranger's token stays `unauthorized`: `revoked` is not a free oracle.
        let (_s, stranger) = server
            .hello(protocol::PROTOCOL_VERSION, Some("no-such-token"))
            .await;
        assert_eq!(
            stranger["type"], "error",
            "an unknown token is refused: {stranger}"
        );
        assert_eq!(
            stranger["code"], "unauthorized",
            "an unknown token is `unauthorized`, not `revoked`: {stranger}"
        );

        // Allowed again: the same held token is admitted, no new code.
        server.store.unblock_device(&device_id).unwrap();
        let (_s, readmitted) = server.hello(protocol::PROTOCOL_VERSION, Some(token)).await;
        assert_eq!(
            readmitted["type"], "hello_ack",
            "the unblocked token is admitted again: {readmitted}"
        );
    }

    /// **One computer's agent messages another computer's, over the real
    /// listener.** The sending daemon dials the receiver's port at the address its
    /// tailnet gives the receiving computer; the receiver serves the connection
    /// because its own tailnet says that address is one of the owner's computers,
    /// and heads the message with the name it knows that computer by. A peer
    /// request on a phone's connection after its hello is a bad request, as before.
    #[tokio::test]
    async fn an_agent_messages_an_agent_on_another_computer() {
        // Both daemons run here: "laptop" is this test's loopback.
        let tailnet = protocol::tailnet::Tailnet::parse(
            &include_str!("../../../fixtures/tailscale/status-six-nodes.json")
                .replace("100.64.0.6", "127.0.0.1"),
        )
        .unwrap();
        let (receiver, _device) = live_server(protocol::config::Config::default()).await;
        *receiver
            .daemon
            .test_tailnet
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(tailnet.clone());
        let target = protocol::uid::new().unwrap();
        codex_run(&receiver.daemon, &target, "cc-1").await;
        let (sender, _, _, _) = daemon_with_a_device_pushing(
            protocol::config::Config {
                ws_port: receiver.addr.port(),
                ..protocol::config::Config::default()
            },
            Arc::new(crate::apns::LoggingPushSender::new()),
        );
        let from = protocol::uid::new().unwrap();
        codex_run(&sender, &from, "cc-2").await;

        let status = sender
            .send_agent_message_on(
                Some(&tailnet),
                &from,
                &format!("{target}@laptop"),
                "please review",
                None,
            )
            .await;
        assert_eq!(
            status.state,
            protocol::ipc::MailState::Waiting,
            "{status:?}"
        );
        let refused = sender
            .send_agent_message_on(
                Some(&tailnet),
                &from,
                &format!("{}@laptop", protocol::uid::new().unwrap()),
                "hi",
                None,
            )
            .await;
        assert!(
            refused.state == protocol::ipc::MailState::Refused
                && refused
                    .reason
                    .as_deref()
                    .is_some_and(|r| r.contains("not a running session")),
            "{refused:?}"
        );

        // A message the receiver never answered is still `sent`, and the sweep
        // offers it again under its own id; the receiver keeps one copy.
        let retried = crate::store::SentMail {
            message_id: protocol::uid::new().unwrap(),
            context_id: protocol::uid::new().unwrap(),
            in_reply_to: None,
            session_uid: from.clone(),
            recipient: format!("{target}@laptop"),
            recipient_computer: "laptop".into(),
            text: "are you there?".into(),
        };
        sender
            .store
            .file_sent_mail(&retried, protocol::time::now_unix_ms())
            .unwrap();
        for _ in 0..2 {
            sender.sweep_mail_on(Some(&tailnet)).await;
        }
        let (_, batch) = receiver.daemon.offer_agent_messages(&target).await.unwrap();
        assert_eq!(
            batch
                .messages
                .iter()
                .map(|m| (m.message_id.as_str(), m.label.as_str(), m.text.as_str()))
                .collect::<Vec<_>>(),
            [
                (status.message_id.as_str(), "cc-2@laptop", "please review"),
                (retried.message_id.as_str(), "cc-2@laptop", "are you there?"),
            ]
        );
        assert!(
            batch
                .messages
                .iter()
                .all(|m| m.from == format!("{from}@laptop")),
            "the sender is a run of the computer the connection came from"
        );
        let (_, mine) = sender.offer_agent_messages(&from).await.unwrap();
        assert_eq!(
            mine.updates
                .iter()
                .map(|u| (u.message_id.as_str(), u.state))
                .collect::<Vec<_>>(),
            [(
                retried.message_id.as_str(),
                protocol::ipc::MailState::Waiting
            )],
            "the sender is told the retry was taken"
        );

        let (mut phone, ack) = receiver.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(ack["type"], "hello_ack", "{ack}");
        phone
            .send(Message::Text(r#"{"type":"peer_list_agents"}"#.into()))
            .await
            .unwrap();
        let answer = next_json(&mut phone).await.unwrap();
        assert_eq!(answer["code"], "bad_request", "{answer}");
    }

    /// **A computer whose agents could not be listed is named** (B-18): the Mac
    /// is up on the tailnet but nothing answers on its port, and the listing says so
    /// rather than leaving it out. The Windows PC and the iPhone, online too, run no
    /// CodeConnect: they are not asked, nor named (round 2, R2-3). All three are on
    /// loopback, so whatever is dialled is dialled here.
    #[tokio::test]
    async fn a_computer_that_does_not_answer_a_listing_is_named() {
        let status = ["100.64.0.2", "100.64.0.4", "100.64.0.6"].iter().fold(
            include_str!("../../../fixtures/tailscale/status-six-nodes.json").to_string(),
            |status, ip| status.replace(ip, "127.0.0.1"),
        );
        let tailnet = protocol::tailnet::Tailnet::parse(&status).unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (daemon, _, _, _) = daemon_with_a_device_pushing(
            protocol::config::Config {
                ws_port: closed,
                ..protocol::config::Config::default()
            },
            Arc::new(crate::apns::LoggingPushSender::new()),
        );
        let (agents, unreachable) = daemon.ask_peers(&tailnet).await;
        assert!(agents.is_empty());
        assert_eq!(unreachable.len(), 1, "{unreachable:?}");
        assert!(unreachable[0].starts_with("laptop ("), "{unreachable:?}");
    }

    fn daemon_with_a_device_pushing(
        config: protocol::config::Config,
        push: Arc<dyn crate::apns::PushSender>,
    ) -> (
        Arc<Daemon>,
        String,
        Arc<crate::store::Store>,
        crate::store::TempDb,
    ) {
        let store = Arc::new(crate::store::Store::open_temp("ccd-ws-live"));
        let path = store.temp_db();
        let now = protocol::time::now_rfc3339();
        let device_id = "abcd1234".to_string();
        let device_token = "device-token-for-tests";
        store
            .insert_device(
                &device_id,
                "iPhone",
                &protocol::hash::sha256_hex(device_token.as_bytes()),
                &now,
                None,
            )
            .unwrap();
        let daemon = Daemon::new(
            config,
            Arc::clone(&store),
            push,
            crate::state::Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
        );
        (daemon, device_id, store, path)
    }

    // ------------------------------------------- the push compatibility matrix

    /// A sender that is nothing but a mode. Every rule below is decided by the
    /// mode alone, so a fake that has one is a complete stand-in — and one that
    /// could also deliver would let a test pass for the wrong reason.
    struct ModeSender(crate::apns::PushMode);

    impl crate::apns::PushSender for ModeSender {
        fn send(&self, _hint: &crate::apns::PushHint, _excluded: &[String]) {}
        fn mode(&self) -> crate::apns::PushMode {
            self.0
        }
    }

    async fn server_in(mode: crate::apns::PushMode) -> (LiveServer, String) {
        live_server_pushing(
            protocol::config::Config::default(),
            Arc::new(ModeSender(mode)),
        )
        .await
    }

    /// Send one `register_push` and return whatever came back, or `None` when
    /// the daemon accepted it silently — registration is idempotent and
    /// deliberately unacknowledged, so silence is the success case.
    async fn register(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        document: serde_json::Value,
    ) -> Option<serde_json::Value> {
        socket
            .send(Message::Text(document.to_string()))
            .await
            .unwrap();
        next_json_within(socket, Duration::from_millis(400)).await
    }

    fn registration(token: &str, credential: Option<&str>) -> serde_json::Value {
        let mut document = serde_json::json!({
            "type": "register_push",
            "token": token,
            "environment": "production",
        });
        if let Some(credential) = credential {
            document["relay_credential"] = credential.into();
        }
        document
    }

    const A_TOKEN: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";

    /// **The capability table, one-hot in every mode.**
    ///
    /// `push` and `push_relay` answer different questions — "register a bare
    /// token" and "enrol for a credential first" — and a phone that saw both
    /// would do one of them wrongly. The row that carries the compatibility
    /// story is relay's `push = false`: a client that predates minor 14 reads
    /// only that flag, and reading `true` would send it to register a token
    /// with no bearer, producing a device that never rings and says nothing.
    #[tokio::test]
    async fn the_push_capabilities_are_one_hot_and_honest_in_every_mode() {
        for (mode, push, relay, test) in [
            (crate::apns::PushMode::Direct, true, false, true),
            (crate::apns::PushMode::Relay, false, true, true),
            (crate::apns::PushMode::Off, false, false, false),
        ] {
            let (server, _device) = server_in(mode).await;
            let (_socket, ack) = server.hello(protocol::PROTOCOL_VERSION, None).await;
            assert_eq!(ack["type"], "hello_ack", "{mode:?}");
            assert_eq!(ack["protocol_minor"], protocol::PROTOCOL_MINOR, "{mode:?}");
            let capabilities = &ack["capabilities"];
            assert_eq!(capabilities["push"], push, "push in {mode:?}");
            assert_eq!(capabilities["push_relay"], relay, "push_relay in {mode:?}");
            assert_eq!(capabilities["test_push"], test, "test_push in {mode:?}");
            assert!(
                !(capabilities["push"].as_bool().unwrap()
                    && capabilities["push_relay"].as_bool().unwrap()),
                "the two flags are never both true"
            );
        }
    }

    /// A relay daemon has no way to send without a bearer, so a registration
    /// that carries none is refused with a reason rather than filed — the row
    /// would be a phone waiting for a notification nobody will attempt.
    #[tokio::test]
    async fn a_relay_daemon_refuses_a_registration_that_carries_no_credential() {
        let (server, _device) = server_in(crate::apns::PushMode::Relay).await;
        let (mut socket, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        let refusal = register(&mut socket, registration(A_TOKEN, None))
            .await
            .expect("a refusal the phone can act on, not silence");
        assert_eq!(refusal["type"], "error");
        assert_eq!(refusal["code"], "push_registration_failed");
        assert!(
            refusal["message"]
                .as_str()
                .unwrap()
                .contains("relay credential"),
            "the message must name what is missing: {refusal}"
        );
        assert!(
            server.store.push_targets().unwrap().is_empty(),
            "nothing may be stored for a registration that was refused"
        );
    }

    /// The same daemon accepts the whole tuple, and stores it whole.
    #[tokio::test]
    async fn a_relay_daemon_stores_the_tuple_a_current_app_sends() {
        let (server, device) = server_in(crate::apns::PushMode::Relay).await;
        let (mut socket, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert!(
            register(&mut socket, registration(A_TOKEN, Some("a-relay-bearer")))
                .await
                .is_none(),
            "an accepted registration is answered with silence, by design"
        );
        let stored = server.store.push_targets().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].device_id, device);
        assert_eq!(stored[0].token, A_TOKEN);
        assert_eq!(stored[0].environment, "production");
        assert_eq!(
            stored[0].credential.as_ref().map(|c| c.expose()),
            Some("a-relay-bearer")
        );
    }

    /// **A frame still carrying the `features` list minor 15 defined is handled
    /// as if it did not.**
    ///
    /// No shipping phone ever sent the key, and this build no longer names it, so
    /// serde ignores it as an unknown field. Asserted against the live handlers
    /// rather than only at the decode, because the handshake `hello`, a second
    /// `hello` down an authenticated socket and `register_push` are three
    /// different arms, and a refusal at any of them would cut a phone off.
    ///
    /// **Mutation:** add `deny_unknown_fields` to `ClientMessage` and the
    /// handshake is refused.
    #[tokio::test]
    async fn a_frame_still_carrying_features_is_handled_as_if_it_did_not() {
        let advertising = serde_json::json!({"agents": ["claude", "codex"]});
        let (server, device) = server_in(crate::apns::PushMode::Direct).await;

        let (mut socket, ack) = server
            .hello_advertising("device-token-for-tests", advertising.clone())
            .await;
        assert_eq!(ack["type"], "hello_ack", "the handshake is accepted: {ack}");

        // A second hello, down the socket that is already authenticated, then a
        // ping: the pong is how the test knows the re-hello was consumed.
        socket
            .send(Message::Text(
                hello_frame("device-token-for-tests", advertising.clone()).to_string(),
            ))
            .await
            .unwrap();
        socket
            .send(Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .unwrap();
        assert_eq!(
            next_json(&mut socket).await.expect("the ping is answered")["type"],
            "pong",
            "the re-hello was consumed without an error"
        );

        let mut registering = registration(A_TOKEN, None);
        registering["features"] = advertising;
        assert!(
            register(&mut socket, registering).await.is_none(),
            "a frame carrying the key is a good registration, not an error"
        );
        let stored = server.store.push_targets().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].device_id, device);
        assert_eq!(stored[0].token, A_TOKEN);
    }

    /// The device's `last_seen_at`, as `codeconnect devices` and the Mac's Settings ›
    /// Devices read it.
    fn last_seen(store: &crate::store::Store, device: &str) -> Option<String> {
        store
            .list_devices()
            .unwrap()
            .into_iter()
            .find(|row| row.device_id == device)
            .and_then(|row| row.last_seen_at)
    }

    /// The device's `last_seen_at` once it is later than `after`, waiting up to two
    /// seconds; the last value read when it never is.
    async fn last_seen_after(
        store: &crate::store::Store,
        device: &str,
        after: &str,
    ) -> Option<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let seen = last_seen(store, device);
            if seen.as_deref().is_some_and(|seen| seen > after)
                || tokio::time::Instant::now() >= deadline
            {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// **A connected device is seen at each ping, not only at its hello.** The apps
    /// ping every ten seconds, and each ping is the device reaching this computer,
    /// so its `last_seen_at`, which Settings › Devices shows as "Last seen … ago",
    /// moves with them instead of growing from the hello for as long as the
    /// connection lasts.
    #[tokio::test]
    async fn a_connected_device_is_seen_at_each_ping_and_not_only_at_its_hello() {
        let (server, device) = live_server(protocol::config::Config::default()).await;
        let (mut socket, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["type"], "hello_ack", "{ack}");
        let at_hello = last_seen(&server.store, &device).expect("the hello stamps the device");

        tokio::time::sleep(Duration::from_millis(20)).await;
        socket
            .send(Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .unwrap();
        assert_eq!(
            next_json(&mut socket).await.expect("the ping is answered")["type"],
            "pong"
        );
        let seen = last_seen_after(&server.store, &device, &at_hello).await;
        assert!(
            seen.as_deref().is_some_and(|seen| seen > at_hello.as_str()),
            "a ping is the device reaching this computer: still {seen:?}, the hello's {at_hello}"
        );
    }

    /// **A ping is answered while the store's writer is held.** The ping's touch waits
    /// for the writer; its answer does not, because the Kit judges a link by its
    /// answers and drops one silent for thirty seconds. Held here from a plain thread,
    /// the writer delays only the touch, which lands once the writer is free.
    #[tokio::test]
    async fn a_ping_is_answered_while_the_stores_writer_is_held() {
        let (server, device) = live_server(protocol::config::Config::default()).await;
        let (mut socket, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["type"], "hello_ack", "{ack}");
        let at_hello = last_seen(&server.store, &device).expect("the hello stamps the device");

        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = {
            let store = Arc::clone(&server.store);
            std::thread::spawn(move || {
                let _writer = store.hold_writer();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv();
            })
        };
        held_rx.recv().unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        socket
            .send(Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .unwrap();
        let answer = next_json_within(&mut socket, Duration::from_secs(2)).await;
        release_tx.send(()).unwrap();
        holder.join().unwrap();

        assert_eq!(
            answer.as_ref().map(|reply| reply["type"].clone()),
            Some(serde_json::json!("pong")),
            "the ping's answer waited for the store's writer: {answer:?}"
        );
        let seen = last_seen_after(&server.store, &device, &at_hello).await;
        assert!(
            seen.as_deref().is_some_and(|seen| seen > at_hello.as_str()),
            "the touch lands once the writer is free: still {seen:?}, the hello's {at_hello}"
        );
    }

    /// **A direct daemon accepts the registration an older phone sends** — that
    /// is the whole of the "old app, new daemon" row — and does not write down a
    /// bearer it will never present, even when one is offered.
    #[tokio::test]
    async fn a_direct_daemon_takes_the_token_alone_and_keeps_no_bearer() {
        let (server, _device) = server_in(crate::apns::PushMode::Direct).await;
        let (mut socket, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert!(register(&mut socket, registration(A_TOKEN, None))
            .await
            .is_none());
        let stored = server.store.push_targets().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].credential, None);

        assert!(
            register(&mut socket, registration(A_TOKEN, Some("offered-anyway")))
                .await
                .is_none()
        );
        let stored = server.store.push_targets().unwrap();
        assert_eq!(
            stored[0].credential, None,
            "a bearer this Mac will never present is a secret kept for no reason"
        );
    }

    /// Off refuses both shapes. A registration that cannot be acted on is not a
    /// registration, and storing one would leave the phone believing otherwise.
    #[tokio::test]
    async fn a_daemon_with_push_off_refuses_a_registration_rather_than_filing_one() {
        let (server, _device) = server_in(crate::apns::PushMode::Off).await;
        let (mut socket, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        for credential in [None, Some("a-relay-bearer")] {
            let refusal = register(&mut socket, registration(A_TOKEN, credential))
                .await
                .expect("a refusal, not silence");
            assert_eq!(refusal["code"], "push_registration_failed");
        }
        assert!(server.store.push_targets().unwrap().is_empty());
    }

    /// **The bootstrap token has no device row, in any mode.** A push token
    /// belongs to a row so that revoking the device stops its notifications;
    /// accepting one here would create a stream nothing could switch off.
    #[tokio::test]
    async fn a_bootstrap_connection_can_never_register_a_push_token() {
        for mode in [
            crate::apns::PushMode::Direct,
            crate::apns::PushMode::Relay,
            crate::apns::PushMode::Off,
        ] {
            let (server, _device) = server_in(mode).await;
            // No device token: the harness falls back to the static one.
            let (mut socket, ack) = server.hello(protocol::PROTOCOL_VERSION, None).await;
            assert!(
                ack["device_id"].is_null(),
                "a static connection has no device row: {ack}"
            );
            let refusal = register(&mut socket, registration(A_TOKEN, Some("a-relay-bearer")))
                .await
                .expect("a refusal, not silence");
            assert_eq!(refusal["code"], "no_device", "{mode:?}");
            assert!(server.store.push_targets().unwrap().is_empty());
        }
    }

    /// A token that is not a token is refused before it is stored, so the relay
    /// meters one binding per phone rather than one per spelling — and so a row
    /// can never hold something no sender could address.
    #[tokio::test]
    async fn a_malformed_token_is_refused_rather_than_stored() {
        let (server, _device) = server_in(crate::apns::PushMode::Relay).await;
        let (mut socket, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        for bad in [
            "",
            "aabb",
            "not-hex-at-all-not-hex-at-all-not-hex",
            &"a".repeat(600),
        ] {
            let refusal = register(&mut socket, registration(bad, Some("bearer")))
                .await
                .expect("a refusal, not silence");
            assert_eq!(refusal["code"], "push_registration_failed", "{bad:?}");
        }
        assert!(server.store.push_targets().unwrap().is_empty());
    }

    /// **The handshake reports the environment the daemon holds**, which is how
    /// a relay-side correction reaches a phone that would otherwise go on
    /// resending the value it first cached. Absent before there is a token to
    /// have an environment for.
    #[tokio::test]
    async fn the_handshake_carries_the_environment_the_daemon_holds() {
        let (server, _device) = server_in(crate::apns::PushMode::Relay).await;
        let (mut socket, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert!(
            ack.get("push_environment").is_none(),
            "a daemon holding no token for this device claims no environment: {ack}"
        );

        assert!(
            register(&mut socket, registration(A_TOKEN, Some("a-relay-bearer")))
                .await
                .is_none()
        );

        let (_socket, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["push_environment"], "production");

        // The relay corrects the binding; the next handshake carries the
        // correction rather than the value the phone first sent. The credential
        // is the one this device registered with, so the CAS matches the row.
        server
            .store
            .set_push_environment(&device_of(&ack), A_TOKEN, Some("a-relay-bearer"), "sandbox")
            .unwrap();
        let (_socket, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(
            ack["push_environment"], "sandbox",
            "the daemon reports what it holds, not what it was told"
        );
    }

    fn device_of(ack: &serde_json::Value) -> String {
        ack["device_id"]
            .as_str()
            .expect("a paired ack names the device")
            .to_string()
    }

    /// **A read the handshake cannot make is a failed handshake, never a false
    /// `None`.** Absence of `push_environment` is the positive claim "no token
    /// registered", which a phone persists over its own cached tuple — so a
    /// database fault that was reported as absence would make a phone with a
    /// live registration discard it. When the read cannot be substantiated the
    /// ack fails and the connection closes; the phone reconnects.
    ///
    /// The reproduction drops the one column the read needs, which the auth
    /// path does not touch — so authentication still succeeds and the failure is
    /// isolated to exactly the read under test. A reconnect must error rather
    /// than answer with an ack whose `push_environment` is a fabricated `None`,
    /// and a fresh pair must still ack `None` without reading.
    #[tokio::test]
    async fn a_failed_environment_read_fails_the_handshake_rather_than_faking_absence() {
        let (daemon, device_id, store, path) = daemon_with_a_device_pushing(
            protocol::config::Config::default(),
            Arc::new(ModeSender(crate::apns::PushMode::Relay)),
        );
        // A live relay registration for the reconnecting device.
        store
            .set_push_token(&device_id, A_TOKEN, "production", Some("a-relay-bearer"))
            .unwrap();

        // Break the one read `hello_ack` makes, and nothing the auth path uses:
        // `device_by_token_hash` selects no push column, so a reconnect still
        // authenticates and the failure lands on `push_environment_for` alone.
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute("ALTER TABLE devices DROP COLUMN push_environment", [])
            .expect("SQLite drops the column the read needs");
        drop(raw);

        let reconnect = || {
            AuthOutcome::Device(Box::new(crate::store::DeviceRow {
                device_id: device_id.clone(),
                name: "iPhone".into(),
                created_at: protocol::time::now_rfc3339(),
                last_seen_at: None,
                revoked_at: None,
                tailnet_node: None,
                paired_before_nodes: false,
            }))
        };
        let result = hello_ack(&daemon, reconnect(), true, true).await;
        assert!(
            result.is_err(),
            "a read it could not make must fail the ack, not fabricate absence: {result:?}"
        );

        // And a *fresh pair* never makes the read at all — its row is new and
        // has no token — so it still succeeds with a truthful `None` even while
        // the column is gone. This is what proves the fix distinguishes "known
        // absent" from "could not read" rather than blanket-failing.
        let fresh = AuthOutcome::Paired {
            device_id: "newly-paired".into(),
            device_name: "iPad".into(),
            token: "a-fresh-device-token".into(),
        };
        match hello_ack(&daemon, fresh, true, true).await {
            Ok(ServerMessage::HelloAck {
                push_environment, ..
            }) => assert_eq!(
                push_environment, None,
                "a device just created has no token, so absence here is the truth"
            ),
            other => panic!("a fresh pair reads nothing and must still ack: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_client_on_a_different_protocol_major_is_refused_not_warned_about() {
        // The defect: a major mismatch logged a warning and then handed the
        // client the full event log and the approval path. The major version is
        // the "can we talk at all" question — a peer on a different one has, by
        // the definition of the number, a different idea of what these messages
        // mean.
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let (mut socket, reply) = server.hello(protocol::PROTOCOL_VERSION + 1, None).await;
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["code"], "protocol_mismatch");
        assert!(
            next_json(&mut socket).await.is_none(),
            "the connection must be closed, not merely complained at"
        );
    }

    #[tokio::test]
    async fn a_client_on_the_current_major_still_gets_in() {
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let (_socket, reply) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(reply["type"], "hello_ack");
        assert_eq!(reply["protocol_version"], protocol::PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn the_engine_speaks_major_two_and_refuses_major_one_in_plain_words() {
        // Literal numbers on purpose: this pins the break itself, which the
        // relative tests above cannot see.
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let (_socket, reply) = server.hello(2, None).await;
        assert_eq!(reply["type"], "hello_ack");
        assert_eq!(reply["protocol_version"], 2);
        let (_socket, reply) = server.hello(1, None).await;
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["code"], "protocol_mismatch");
        assert_eq!(
            reply["message"],
            "This computer runs a different version of CodeConnect. Update CodeConnect on both, then try again."
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn the_ack_names_this_machine_as_its_owner_named_it() {
        // `scutil` is the owner's own view of the name, read independently of
        // the call under test.
        let scutil = std::process::Command::new("/usr/sbin/scutil")
            .args(["--get", "ComputerName"])
            .output()
            .unwrap();
        let computer_name = String::from_utf8(scutil.stdout).unwrap();
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let (_socket, reply) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(reply["type"], "hello_ack");
        assert_eq!(reply["machine_name"], computer_name.trim_end_matches('\n'));
        assert_eq!(reply["machine_os"], "macos");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_ack_names_this_linux_machine_by_its_hostname() {
        // `uname -n` is the system's own view of the name, read independently
        // of the call under test.
        let uname = std::process::Command::new("uname")
            .arg("-n")
            .output()
            .unwrap();
        let hostname = String::from_utf8(uname.stdout).unwrap();
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let (_socket, reply) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(reply["type"], "hello_ack");
        assert_eq!(reply["machine_name"], hostname.trim_end_matches('\n'));
        assert_eq!(reply["machine_os"], "linux");
    }

    #[tokio::test]
    async fn every_ack_says_which_daemon_this_is() {
        let (server, _) = live_server(protocol::config::Config::default()).await;
        server
            .daemon
            .machine_id
            .set("0123456789abcdef0123456789abcdef".into())
            .unwrap();
        for _ in 0..2 {
            let (_socket, reply) = server.hello(protocol::PROTOCOL_VERSION, None).await;
            assert_eq!(
                reply["machine_id"], "0123456789abcdef0123456789abcdef",
                "{reply}"
            );
        }
    }

    #[tokio::test]
    async fn the_ack_says_this_daemon_answers_claudes_questions() {
        // A phone offers to answer a question only on this word, and a phone
        // that does not see it shows the question read-only.
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let (_socket, reply) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(reply["capabilities"]["question_card"], true, "{reply}");
    }

    #[tokio::test]
    async fn the_ack_says_this_daemon_answers_claude_cards_only_through_their_hooks() {
        // A phone offers to answer a Claude approval only on this word, and a
        // phone that does not see it shows the approval read-only: an older
        // daemon types answers into whatever prompt is on screen.
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let (_socket, reply) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(
            reply["capabilities"]["hook_only_approvals"], true,
            "{reply}"
        );
    }

    #[tokio::test]
    async fn the_listener_refuses_connections_past_the_cap() {
        let config = protocol::config::Config {
            ws_max_connections: 2,
            ws_max_per_peer: 2,
            ..Default::default()
        };
        let (server, _) = live_server(config).await;
        let held: Vec<_> = futures_util::future::join_all(
            (0..2).map(|_| server.hello(protocol::PROTOCOL_VERSION, None)),
        )
        .await;
        for (_, reply) in &held {
            assert_eq!(reply["type"], "hello_ack");
        }

        // The third is dropped at accept, before a handshake is attempted.
        let stream = TcpStream::connect(server.addr).await.unwrap();
        let url = format!("ws://{}/", server.addr);
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::client_async(&url, stream),
        )
        .await
        .expect("the refusal must be prompt, not a hang");
        assert!(
            refused.is_err(),
            "a connection past the cap must not complete a handshake"
        );

        // And the cap is a ceiling, not a latch.
        drop(held);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (_socket, reply) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(reply["type"], "hello_ack", "room must come back");
    }

    #[tokio::test]
    async fn revoking_closes_an_idle_socket_immediately() {
        let _home = redirected_home("ws-revoke-idle-socket");
        // Before the cancellation broadcast, revocation reached a socket only
        // when that socket next did something — a message, or the 30-second
        // keepalive. A phone sitting on a subscription kept receiving the event
        // log for up to half a minute after `codeconnect revoke`.
        let (server, device_id) = live_server(protocol::config::Config::default()).await;
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");
        assert_eq!(reply["device_id"], device_id);

        server.daemon.revoke(&device_id).await.unwrap();

        // Well inside the 30s keepalive, and the socket sends nothing that
        // would trigger the per-message check. Requiring the explicit `revoked`
        // frame rather than merely a closed socket is what stops this passing
        // for an unrelated disconnect.
        let told = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(message) = next_json(&mut socket).await {
                if message["code"] == "revoked" {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(
            told,
            "an idle revoked connection must be told and closed without waiting for a keepalive"
        );
        assert!(
            next_json(&mut socket).await.is_none(),
            "and the socket must then close"
        );
    }

    #[test]
    fn the_accept_loop_stops_admitting_at_the_global_cap() {
        // The defect: `listener.accept()` spawned unconditionally, so anything
        // that could reach the tailnet port held file descriptors without a
        // ceiling — and the *local* IPC socket, which carries the CLI, lives on
        // the same budget.
        let limiter = Arc::new(ConnectionLimiter::new(3, 3));
        let permits: Vec<_> = (0..3)
            .map(|i| limiter.admit(ip(i)).expect("under the cap"))
            .collect();
        assert_eq!(limiter.live_total(), 3);
        assert!(
            limiter.admit(ip(9)).is_none(),
            "the fourth connection must be refused"
        );

        // And the cap has to be a *ceiling*, not a one-way latch: closing a
        // connection has to make room for the next one.
        drop(permits);
        assert_eq!(limiter.live_total(), 0);
        assert!(limiter.admit(ip(9)).is_some());
    }

    #[test]
    fn one_peer_cannot_consume_everyone_elses_share() {
        // The global cap alone is not enough. Without a per-peer share, one
        // misbehaving client reaches the global limit by itself and every other
        // device — including the phone the operator is holding — is refused.
        let limiter = Arc::new(ConnectionLimiter::new(10, 2));
        let hog: Vec<_> = (0..2)
            .map(|_| limiter.admit(ip(1)).expect("within its share"))
            .collect();
        assert!(
            limiter.admit(ip(1)).is_none(),
            "a third from the same peer must be refused"
        );
        assert!(
            limiter.admit(ip(2)).is_some(),
            "a different peer must still get in"
        );
        drop(hog);
        assert!(limiter.admit(ip(1)).is_some(), "and its share comes back");
    }

    #[test]
    fn released_peers_are_pruned_rather_than_accumulated() {
        // A long-running daemon would otherwise keep one map entry per address
        // that ever connected — a slow leak keyed by something a peer chooses.
        let limiter = Arc::new(ConnectionLimiter::new(64, 8));
        for i in 0..50 {
            drop(limiter.admit(ip(i)).unwrap());
        }
        let live = limiter.live.lock().unwrap();
        assert_eq!(live.total, 0);
        assert!(
            live.by_peer.is_empty(),
            "{} peer entries survived their connections",
            live.by_peer.len()
        );
    }

    #[test]
    fn an_oversized_event_becomes_a_placeholder_that_keeps_its_sequence() {
        // Incoming messages are capped at the frame limit; outgoing ones were
        // not capped at all. `URLSessionWebSocketTask` fails the *connection*
        // rather than the message when one arrives over its 1MB default, so a
        // single oversized event disconnected the phone — which then
        // reconnected, replayed, and hit the same event again. A loop the
        // client could never get past.
        let mut event = marker(
            "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "cc-1",
            serde_json::Value::Null,
        );
        event.seq = 77;
        event.kind = EventKind::ToolCall;
        event.payload = serde_json::json!({ "blob": "x".repeat(2 * MAX_CLIENT_MESSAGE_BYTES) });
        let message = ServerMessage::Event { event };
        let raw = serde_json::to_string(&message).unwrap();
        assert!(raw.len() > MAX_CLIENT_MESSAGE_BYTES, "fixture must be huge");

        let sent = oversized_placeholder(&message, raw.len()).unwrap();
        assert!(
            sent.len() <= MAX_CLIENT_MESSAGE_BYTES,
            "the placeholder is {} bytes",
            sent.len()
        );
        let decoded: serde_json::Value = serde_json::from_str(&sent).unwrap();
        // The identity survives, so the client's watermark still advances and
        // the timeline shows a fact rather than a hole.
        assert_eq!(decoded["event"]["seq"], 77);
        assert_eq!(decoded["event"]["kind"], "tool_call");
        assert_eq!(
            decoded["event"]["session_uid"],
            "01K1B3XQ8ZC0DE5FGH7JKMNPQR"
        );
        // The spellings ingest writes and the phone reads
        // (`Event.isTruncated` reads `_codeconnect_truncated`), so the row says
        // its output was too large rather than drawing an empty payload.
        assert_eq!(decoded["event"]["payload"]["_codeconnect_truncated"], true);
        assert_eq!(decoded["event"]["payload"]["_original_bytes"], raw.len());
    }

    #[test]
    fn an_oversized_reply_that_is_not_an_event_says_so() {
        let message = ServerMessage::Error {
            code: "example".into(),
            message: "y".repeat(2 * MAX_CLIENT_MESSAGE_BYTES),
        };
        let raw = serde_json::to_string(&message).unwrap();
        let sent = oversized_placeholder(&message, raw.len()).unwrap();
        assert!(sent.len() <= MAX_CLIENT_MESSAGE_BYTES);
        let decoded: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(decoded["code"], "message_too_large");
    }

    #[test]
    fn resync_marker_is_recognisable_and_carries_no_seq() {
        let marker = resync_marker("01K1B3XQ8ZC0DE5FGH7JKMNPQR", "cc-1", 42);
        assert_eq!(marker.kind, EventKind::Resync);
        assert_eq!(marker.seq, 0, "a marker must not look like a logged fact");
        assert_eq!(marker.payload["skipped"], 42);
        // It has to name the run it interrupts, or a client following two
        // sessions cannot tell which stream just lost its place.
        assert_eq!(marker.session_uid, "01K1B3XQ8ZC0DE5FGH7JKMNPQR");
        assert_eq!(marker.session_id, "cc-1");
    }

    #[test]
    fn an_out_of_order_event_is_a_gap_and_never_a_skip() {
        // The rule this reduces to. Two tasks commit seq 1 and 2 and
        // publish them in the wrong order; the socket used to accept 2 (because
        // `2 > 0`), move its watermark to 2, and then *discard* 1 by the same
        // test — losing a committed fact on that connection with nothing said.
        assert_eq!(live_delivery(1, 0), Live::Deliver);
        assert_eq!(
            live_delivery(2, 0),
            Live::Gap,
            "seq 2 with nothing delivered is a hole, not the next event"
        );
        // Having taken the gap path and replayed, the late 1 is below the
        // watermark and is correctly a no-op rather than a second delivery.
        assert_eq!(live_delivery(1, 2), Live::AlreadySent);
        assert_eq!(live_delivery(2, 2), Live::AlreadySent);
        assert_eq!(live_delivery(3, 2), Live::Deliver);
        // A client that subscribed from a watermark ahead of the log is never
        // told about events it claims to have; that is a drop, not a resync
        // loop.
        assert_eq!(live_delivery(5, 1_000), Live::AlreadySent);
    }

    #[test]
    fn a_gap_marker_says_what_was_expected_and_what_arrived() {
        let mut event = resync_marker("01K1B3XQ8ZC0DE5FGH7JKMNPQR", "cc-1", 0);
        event.seq = 9;
        let marker = gap_marker(&event, 4);
        assert_eq!(marker.kind, EventKind::Resync);
        assert_eq!(marker.seq, 0, "a marker must not look like a logged fact");
        assert_eq!(marker.payload["expected_seq"], 5);
        assert_eq!(marker.payload["received_seq"], 9);
        assert_eq!(marker.payload["skipped"], 4);
        assert_eq!(marker.session_uid, "01K1B3XQ8ZC0DE5FGH7JKMNPQR");
    }

    #[test]
    fn an_escape_heavy_diff_still_fits_one_frame() {
        // The pathological shape: a diff that is almost entirely newlines, so
        // JSON escaping doubles it. At the 512KB content cap that
        // encodes to ~1MB — past the frame limit — and must be shrunk.
        let nasty = "\n".repeat(protocol::ws::MAX_DIFF_BYTES);
        let (text, truncated) = fit_diff_in_a_frame(nasty, true, 0);
        let encoded = serde_json::to_string(&text).unwrap().len();
        assert!(
            encoded <= MAX_CLIENT_MESSAGE_BYTES,
            "{encoded} bytes still exceeds the frame limit"
        );
        assert!(truncated);
        assert!(text.contains("truncated"));
    }

    #[test]
    fn an_ordinary_diff_is_passed_through_untouched() {
        let diff = "diff --git a/x b/x\n+one\n-two\n".to_string();
        let (text, truncated) = fit_diff_in_a_frame(diff.clone(), false, 0);
        assert_eq!(text, diff, "a small diff must not be rewritten");
        assert!(!truncated);
        // An already-truncated small diff keeps saying so.
        assert!(fit_diff_in_a_frame(diff, true, 0).1);
    }

    /// **A diff with the most files and the largest diff still fits one frame**: a
    /// thousand deep, escape-heavy paths are cut to the files' budget and say so, and
    /// the diff fits beside them, so the whole message stays under the frame limit.
    #[test]
    fn a_diff_with_many_deep_paths_still_fits_one_frame() {
        let deep = |n: usize| protocol::ws::DiffFile {
            path: format!("{}/{n}", "\\\"/".repeat(400)),
            old_path: Some("\n".repeat(400)),
            status: protocol::ws::DiffFileStatus::Renamed,
            added: Some(u64::MAX),
            removed: Some(u64::MAX),
            binary: false,
        };
        let many: Vec<_> = (0..protocol::ws::MAX_DIFF_FILES).map(deep).collect();
        let (files, cut) = fit_files_in_a_frame(Some(many.clone()), false);
        assert!(cut, "a list past its budget says it was cut");
        assert!(encoded_len(&files) <= FILES_FRAME_BUDGET);
        let kept = files.as_ref().unwrap().len();
        assert!(
            encoded_len(&many[..=kept]) > FILES_FRAME_BUDGET,
            "the longest prefix that fits is kept, not fewer: {kept}"
        );
        let (unified, _) = fit_diff_in_a_frame(
            "\n".repeat(protocol::ws::MAX_DIFF_BYTES),
            true,
            encoded_len(&files),
        );
        let message = ServerMessage::Diff {
            session_id: "s".repeat(64),
            unified,
            truncated: true,
            captured_at: protocol::time::now_rfc3339(),
            note: Some("n".repeat(200)),
            files,
            files_truncated: cut,
        };
        let encoded = serde_json::to_string(&message).unwrap().len();
        assert!(encoded <= MAX_CLIENT_MESSAGE_BYTES, "{encoded} bytes");

        let few = vec![deep(1)];
        assert_eq!(
            fit_files_in_a_frame(Some(few.clone()), false),
            (Some(few), false),
            "a list inside its budget passes untouched"
        );
        assert_eq!(fit_files_in_a_frame(None, false), (None, false));
    }

    #[test]
    fn an_empty_diff_survives_the_fitting() {
        let (text, truncated) = fit_diff_in_a_frame(String::new(), false, 0);
        assert_eq!(text, "");
        assert!(!truncated);
    }

    #[test]
    fn multibyte_content_is_never_split_mid_character() {
        // Emoji are 4 bytes and escape to 12; the shrink loop must still land
        // on a character boundary or `String::truncate` panics.
        let nasty = "🔥".repeat(protocol::ws::MAX_DIFF_BYTES / 4);
        let (text, _) = fit_diff_in_a_frame(nasty, true, 0);
        assert!(text.is_char_boundary(0));
        assert!(
            serde_json::to_string(&text).unwrap().len() <= MAX_CLIENT_MESSAGE_BYTES,
            "still too large"
        );
    }

    #[test]
    fn only_a_tls_record_looks_like_tls() {
        // 0x16 is the TLS handshake content type.
        assert!(is_tls_hello(0x16));
        // Every HTTP method a WebSocket upgrade could start with.
        for method in ["GET ", "POST", "HEAD", "OPTI", "PUT ", "CONN"] {
            assert!(
                !is_tls_hello(method.as_bytes()[0]),
                "{method} must not be read as TLS"
            );
        }
        assert!(!is_tls_hello(b'\n'));
        assert!(!is_tls_hello(0x00));
    }

    // --------------------------------------------------------------- backlog

    /// Put one session and `events` facts under it into the store, and answer
    /// with the uid.
    ///
    /// One transaction for the lot, because the replay test below needs tens of
    /// thousands of them and a commit each would cost more than the replay it is
    /// timing. The batch drops the facts of a session it cannot find, so the run's
    /// row goes in first, as the engine writes one, and a full count proves the
    /// seeding worked.
    fn seed_backlog(daemon: &Arc<Daemon>, events: usize) -> String {
        use protocol::event::{Lifecycle, PendingEvent};

        let uid = protocol::uid::new().expect("mint a uid");
        let now = protocol::time::now_rfc3339();
        let row = crate::store::SessionRow {
            session_uid: uid.clone(),
            session_id: "cc-backlog".into(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: Lifecycle::Live,
            created_at: now.clone(),
            updated_at: now.clone(),
            agent: protocol::agent::AgentKind::Claude,
            codex_thread_id: None,
        };
        assert_eq!(
            daemon.store.begin_run(&row, &uid, None, None).unwrap(),
            None
        );
        let pendings: Vec<PendingEvent> = (0..events)
            .map(|n| PendingEvent {
                session_uid: uid.clone(),
                session_id: "cc-backlog".into(),
                ts: now.clone(),
                kind: EventKind::ToolCall,
                payload: serde_json::json!({ "n": n }),
                source: Source::Daemon,
                source_event_id: None,
                turn_id: None,
                item_id: None,
            })
            .collect();
        let written = daemon
            .store
            .append_batch(&pendings)
            .expect("the backlog is written");
        assert_eq!(written.len(), events, "every seeded event landed");
        uid
    }

    /// The seq of every `event` frame the sink has collected, in order.
    fn replayed_seqs(sink: &CollectSink) -> Vec<u64> {
        sink.0
            .iter()
            .filter_map(|message| match message {
                ServerMessage::Event { event } => Some(event.seq),
                _ => None,
            })
            .collect()
    }

    /// A replay is a state the loop steps through: one unit of work per step,
    /// and an empty page is what ends it.
    ///
    /// The unit here is deliberately "one event *or* one page read", not "one
    /// event", and both are asserted. A step that read a page and then sent its
    /// first event would still look like one event per step from the outside, and
    /// would put an unbounded store read in front of every other arm on the pass
    /// that started a replay — which is a smaller version of the defect the whole
    /// mechanism exists to remove. Counting the frames after the *first* step,
    /// where there are none, is what catches that.
    ///
    /// Driven at [`Backlog`] rather than over a socket because what is under test
    /// is the arithmetic of stepping: a live connection would have to be raced to
    /// observe a single step, and the race would be what the test proved.
    #[tokio::test]
    async fn a_replay_steps_one_event_at_a_time_and_ends_on_an_empty_page() {
        let (daemon, _device) = daemon_with_a_device();
        let uid = seed_backlog(&daemon, 3);
        let mut sink = CollectSink::default();
        let mut watermarks: HashMap<String, u64> = HashMap::new();
        watermarks.insert(uid.clone(), 0);
        let mut backlog = Backlog::default();
        backlog.begin(uid.clone());
        assert!(backlog.in_progress(), "the replay is queued");

        // The first step reads the page and sends nothing.
        backlog
            .step(&daemon, &mut sink, &mut watermarks, None)
            .await
            .unwrap();
        assert!(
            replayed_seqs(&sink).is_empty(),
            "the store read and the send must be separate steps, got {:?}",
            replayed_seqs(&sink)
        );

        // Then one event per step, and the watermark moves with it.
        for seq in 1..=3u64 {
            backlog
                .step(&daemon, &mut sink, &mut watermarks, None)
                .await
                .unwrap();
            assert_eq!(
                replayed_seqs(&sink),
                (1..=seq).collect::<Vec<_>>(),
                "a step sent more than the one event it owed"
            );
            assert_eq!(watermarks[&uid], seq, "the watermark follows the send");
        }

        // The page is spent: one more step reads, comes back empty, and the
        // session is done.
        assert!(backlog.in_progress(), "the session is not finished yet");
        backlog
            .step(&daemon, &mut sink, &mut watermarks, None)
            .await
            .unwrap();
        assert!(!backlog.in_progress(), "an empty page ends that session");
        assert_eq!(
            replayed_seqs(&sink),
            vec![1, 2, 3],
            "and nothing was sent twice"
        );

        // And a step with nothing queued is a no-op rather than a panic: the arm
        // is guarded by `in_progress`, but the guard and the step are two
        // separate lines and only one of them is under this test.
        backlog
            .step(&daemon, &mut sink, &mut watermarks, None)
            .await
            .unwrap();
        assert_eq!(replayed_seqs(&sink), vec![1, 2, 3]);
    }

    /// An event the live arm carried while a replay was pending is skipped by the
    /// replay rather than sent a second time.
    ///
    /// This is what makes stepping safe at all. The live arm delivers only the
    /// exact successor, so an event it delivers mid-replay is in order — and the
    /// watermark it moves is what the replay's page is filtered against on every
    /// subsequent step. Without the filter the phone would see the same seq
    /// twice, which for an event log is a fact that appears to have happened
    /// twice.
    ///
    /// The watermark is moved *between* steps with the page already in hand,
    /// which is precisely the window stepping opens and the old inline replay did
    /// not have.
    #[tokio::test]
    async fn a_replayed_event_the_live_arm_already_carried_is_skipped_not_doubled() {
        let (daemon, _device) = daemon_with_a_device();
        let uid = seed_backlog(&daemon, 3);
        let mut sink = CollectSink::default();
        let mut watermarks: HashMap<String, u64> = HashMap::new();
        watermarks.insert(uid.clone(), 0);
        let mut backlog = Backlog::default();
        backlog.begin(uid.clone());

        // The page is read while the watermark is still 0, so it holds 1, 2, 3.
        backlog
            .step(&daemon, &mut sink, &mut watermarks, None)
            .await
            .unwrap();
        // The live arm then delivers 1 and 2 on this connection.
        watermarks.insert(uid.clone(), 2);

        // Two steps to walk past them, sending nothing.
        for _ in 0..2 {
            backlog
                .step(&daemon, &mut sink, &mut watermarks, None)
                .await
                .unwrap();
            assert!(
                replayed_seqs(&sink).is_empty(),
                "an event the live arm already sent was replayed on top of it, got {:?}",
                replayed_seqs(&sink)
            );
        }

        // And 3, which the live arm did not carry, is still sent.
        backlog
            .step(&daemon, &mut sink, &mut watermarks, None)
            .await
            .unwrap();
        assert_eq!(replayed_seqs(&sink), vec![3], "the tail must still arrive");
    }

    /// Asking again for the session already being paged out throws away the page
    /// read against the watermark that has just moved.
    ///
    /// A phone may subscribe from seq 900, then re-subscribe from 100 — a fresh
    /// install replaying a session it has lost, say. The second `subscribe` moves
    /// the watermark backwards, and a page buffered from the first holds only
    /// events above 900: every one of them passes `seq > watermark`, so 101..900
    /// would be skipped in silence and the phone would be told nothing. Discarding
    /// costs one store read, which is the cheapest possible way to be right.
    ///
    /// Shaped as two `begin`s around one page read, because that is the only
    /// order in which the stale page exists: a `begin` before the read has
    /// nothing to discard, and one after the page is spent discards nothing.
    #[tokio::test]
    async fn re_subscribing_from_further_back_discards_the_page_it_had_read() {
        let (daemon, _device) = daemon_with_a_device();
        let uid = seed_backlog(&daemon, 4);
        let mut sink = CollectSink::default();
        let mut watermarks: HashMap<String, u64> = HashMap::new();
        // Subscribed from the tail: the page read below holds nothing but 4.
        watermarks.insert(uid.clone(), 3);
        let mut backlog = Backlog::default();
        backlog.begin(uid.clone());
        backlog
            .step(&daemon, &mut sink, &mut watermarks, None)
            .await
            .unwrap();

        // Re-subscribed from the start, exactly as `handle_message` does it.
        watermarks.insert(uid.clone(), 0);
        backlog.begin(uid.clone());

        // Stepped to exhaustion. Bounded, so a replay that never ends fails here
        // rather than hanging the suite.
        for _ in 0..64 {
            if !backlog.in_progress() {
                break;
            }
            backlog
                .step(&daemon, &mut sink, &mut watermarks, None)
                .await
                .unwrap();
        }
        assert!(!backlog.in_progress(), "the replay must finish");
        assert_eq!(
            replayed_seqs(&sink),
            vec![1, 2, 3, 4],
            "a page read against the old watermark skipped everything below it"
        );
    }

    /// Deleting a session drops the replay it had queued, rather than leaving one
    /// to be handed to whatever subscribes next.
    ///
    /// `Backlog::step` refuses to page to a watermark that is not there, which
    /// covers the case where nothing re-subscribes. It does not cover the one
    /// where something does: a `subscribe` landing before the next step would
    /// find the session still queued, `begin` would treat it as already asked
    /// for, and the new subscription would inherit a page read against the run
    /// that was deleted.
    #[tokio::test]
    async fn a_deleted_subscription_drops_the_replay_it_had_queued() {
        let (daemon, _device) = daemon_with_a_device();
        let uid = seed_backlog(&daemon, 3);
        let mut sink = CollectSink::default();
        let mut watermarks: HashMap<String, u64> = HashMap::new();
        watermarks.insert(uid.clone(), 0);
        let mut backlog = Backlog::default();
        backlog.begin(uid.clone());
        backlog
            .step(&daemon, &mut sink, &mut watermarks, None)
            .await
            .unwrap();

        backlog.drop_session(&uid);
        assert!(
            !backlog.in_progress() && !backlog.replaying(&uid),
            "the deleted session's replay must be gone, not merely unreachable"
        );

        // And a fresh subscription gets a replay of its own, from its own
        // watermark, rather than the page the deleted one had in hand.
        watermarks.insert(uid.clone(), 0);
        backlog.begin(uid.clone());
        for _ in 0..64 {
            if !backlog.in_progress() {
                break;
            }
            backlog
                .step(&daemon, &mut sink, &mut watermarks, None)
                .await
                .unwrap();
        }
        assert_eq!(
            replayed_seqs(&sink),
            vec![1, 2, 3],
            "the new subscription must get the whole log once"
        );
    }

    /// A gap is announced once, and never for a hole the replay it already
    /// started is on its way to fill.
    ///
    /// `gap_marker` says a specific thing — the *daemon* published out of order,
    /// as opposed to a client that fell behind — and while a session's backlog is
    /// still being paged out, a seq above the successor says nothing about
    /// publish order at all: it is the replay not having reached it yet. Sending
    /// the marker there would assert something unknown, and would announce one
    /// hole twice.
    ///
    /// Both halves are needed, and the second is what stops the first from being
    /// satisfied by a daemon that never announces anything. **They need opposite
    /// treatment, because only one of the two states is observable from the
    /// client.**
    ///
    /// "A replay is running" is observable, and cheaply: a replayed event has
    /// arrived and 19,999 have not, so publishing on the first one lands inside
    /// the replay with most of a second of margin. No barrier is needed for that
    /// direction — the margin *is* the barrier.
    ///
    /// "A replay has ended" is not observable at all. Receiving the last backlog
    /// event is not it: [`Backlog::step`] ends a replay on the empty page read
    /// that comes *after* the last event, a store round trip later, and a publish
    /// landing in that window is correctly suppressed. There is no frame that
    /// says the replay is over. So the second half waits for the *state* by
    /// asking again — publish, look briefly, publish again — rather than for a
    /// moment by timing, and a single publish there is exactly the flake it
    /// replaces: the test failed precisely when the rule worked.
    ///
    /// The retry is bounded overall, so a suppression that never lifts fails here
    /// on its own assertion instead of retrying for ever.
    #[tokio::test]
    async fn a_gap_is_not_announced_while_that_sessions_replay_is_pending() {
        // Tens of thousands of frames go over a real socket below, and
        // `a_write_the_peer_never_takes_is_given_up_on` shortens the write
        // deadline process-wide to 200ms. A test that writes this many frames
        // must not run beside it.
        let _serial = write_deadline_guard().await;
        /// Long enough that a publish sent after the first replayed event is
        /// still comfortably inside the replay: measured above at some 30µs an
        /// event, so this is most of a second of paging.
        const PENDING_REPLAY_EVENTS: usize = 20_000;
        /// Far above anything the store holds, so `live_delivery` can only read
        /// it as a gap.
        const FAR_AHEAD: u64 = 9_000_000;

        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let uid = seed_backlog(&server.daemon, PENDING_REPLAY_EVENTS);
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");

        let out_of_order = Event {
            seq: FAR_AHEAD,
            session_uid: uid.clone(),
            session_id: "cc-backlog".into(),
            ts: protocol::time::now_rfc3339(),
            kind: EventKind::ToolCall,
            payload: serde_json::Value::Null,
            source: Source::Daemon,
            source_event_id: None,
            turn_id: None,
            item_id: None,
        };

        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "subscribe",
                    "session_id": uid,
                    "after_seq": 0,
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let is_marker = |message: &serde_json::Value| {
            message["type"] == "event" && message["event"]["kind"] == "resync"
        };
        let mut published = false;
        let mut markers_during = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while tokio::time::Instant::now() < deadline {
            let Some(message) = next_json(&mut socket).await else {
                break;
            };
            if is_marker(&message) {
                markers_during += 1;
                continue;
            }
            let Some(seq) = message["event"]["seq"].as_u64() else {
                continue;
            };
            // The first replayed event proves the replay is running; the publish
            // then lands with thousands still to go.
            if !published {
                published = true;
                server.daemon.events_tx.send(out_of_order.clone()).unwrap();
            }
            if seq as usize == PENDING_REPLAY_EVENTS {
                break;
            }
        }
        assert!(published, "the replay must have sent something");
        assert_eq!(
            markers_during, 0,
            "a hole the pending replay was already on its way to fill was \
             announced as the daemon publishing out of order"
        );

        // And once no replay is pending the same publish is announced, so the
        // silence above is the rule and not a marker path that never fires.
        // Asked again rather than asked once, because nothing on the wire says
        // when the replay ended: a publish that lands in the window between the
        // last replayed event and the empty page read that follows it is
        // suppressed, correctly, and a test that published only there would sit
        // out its read waiting for a marker its own subject had prevented.
        let mut announced = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline && !announced {
            server
                .daemon
                .events_tx
                .send(out_of_order.clone())
                .expect("the connection is still reading the broadcast");
            while let Some(message) =
                next_json_within(&mut socket, Duration::from_millis(200)).await
            {
                if is_marker(&message) {
                    announced = true;
                    break;
                }
            }
        }
        assert!(
            announced,
            "a gap with no replay pending must still be announced"
        );
    }

    // --------------------------------------------------------------- history

    /// Ask for one earlier page over a live socket, and collect what it sends:
    /// the seq of every event frame up to the page's `history_end`, and that end.
    async fn history_page(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        session_id: &str,
        before_seq: u64,
        limit: u32,
        request_id: &str,
    ) -> (Vec<u64>, serde_json::Value) {
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "history",
                    "session_id": session_id,
                    "before_seq": before_seq,
                    "limit": limit,
                    "request_id": request_id,
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let mut seqs = Vec::new();
        loop {
            let message = next_json(socket).await.expect("the connection stays open");
            match message["type"].as_str() {
                Some("event") => seqs.push(message["event"]["seq"].as_u64().unwrap()),
                Some("history_end") => return (seqs, message),
                _ => panic!("a history page sends events and then its end, got {message}"),
            }
        }
    }

    /// **An earlier page is the newest `limit` events below `before_seq`, oldest
    /// first, and then exactly one `history_end`** — at every edge a phone can
    /// reach: the cap, the head of the log, past the tail, an empty run, and a
    /// run the daemon does not know.
    #[tokio::test]
    async fn a_history_page_is_the_newest_events_below_the_mark_then_its_end() {
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let uid = seed_backlog(&server.daemon, 600);
        let empty = seed_backlog(&server.daemon, 0);
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");
        assert_eq!(reply["capabilities"]["history_pages"], true);

        // A limit above 500 is served as 500.
        let (seqs, end) = history_page(&mut socket, &uid, 601, 1000, "h1").await;
        assert_eq!(seqs, (101..=600).collect::<Vec<u64>>());
        assert_eq!(end["session_id"], uid.as_str());
        assert_eq!(end["request_id"], "h1");
        assert_eq!(end["before_seq"], 601);
        assert_eq!(end["first_seq"], 101);
        assert!(end.get("error").is_none(), "{end}");

        // The next page down, as a phone pages.
        let (seqs, end) = history_page(&mut socket, &uid, 101, 500, "h2").await;
        assert_eq!(seqs, (1..=100).collect::<Vec<u64>>());
        assert_eq!(end["first_seq"], 1);

        // Nothing is below seq 1, and nothing is below 0.
        for before in [1u64, 0] {
            let (seqs, end) = history_page(&mut socket, &uid, before, 500, "h3").await;
            assert!(seqs.is_empty(), "{seqs:?}");
            assert_eq!(end["first_seq"], before);
            assert!(end.get("error").is_none(), "{end}");
        }

        // Past the tail is the tail, however far past.
        for before in [700u64, u64::MAX] {
            let (seqs, end) = history_page(&mut socket, &uid, before, 3, "h4").await;
            assert_eq!(seqs, vec![598, 599, 600]);
            assert_eq!(end["first_seq"], 598);
            assert_eq!(end["before_seq"], before);
        }

        // A run with no events ends at once, at the mark it was asked about.
        let (seqs, end) = history_page(&mut socket, &empty, 10, 500, "h5").await;
        assert!(seqs.is_empty());
        assert_eq!(end["first_seq"], 10);
        assert!(end.get("error").is_none(), "{end}");

        // A run nobody knows is still answered, with the reason.
        let (seqs, end) =
            history_page(&mut socket, "01K1B3XQ8ZC0DE5FGH7JKMNPQZ", 10, 500, "h6").await;
        assert!(seqs.is_empty());
        assert_eq!(end["request_id"], "h6");
        assert_eq!(end["first_seq"], 10);
        assert!(
            end["error"].as_str().is_some_and(|e| !e.is_empty()),
            "{end}"
        );
    }

    /// A live event for a subscribed run, as `Daemon::ingest` would broadcast it.
    fn live_event(uid: &str, seq: u64) -> Event {
        Event {
            seq,
            session_uid: uid.to_string(),
            session_id: "cc-backlog".into(),
            ts: protocol::time::now_rfc3339(),
            kind: EventKind::ToolCall,
            payload: serde_json::json!({ "live": seq }),
            source: Source::Daemon,
            source_event_id: None,
            turn_id: None,
            item_id: None,
        }
    }

    /// **A page does not hold live delivery back for its whole length.** A live
    /// event published while a 500-event page is being sent reaches the phone
    /// before that page's end — so it was interleaved with the page rather than
    /// queued behind it.
    #[tokio::test]
    async fn live_events_keep_flowing_while_a_page_is_served() {
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let uid = seed_backlog(&server.daemon, 1000);
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");
        // Subscribed at the tail, so nothing is replayed and the next live seq is
        // 1001.
        socket
            .send(Message::Text(
                serde_json::json!({"type": "subscribe", "session_id": uid, "after_seq": 1000})
                    .to_string(),
            ))
            .await
            .unwrap();
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "history", "session_id": uid, "before_seq": 1001,
                    "limit": 500, "request_id": "live",
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let mut page_before_live = None;
        let mut page = Vec::new();
        loop {
            let message = next_json(&mut socket)
                .await
                .expect("the connection stays open");
            match message["type"].as_str() {
                Some("event") => {
                    let seq = message["event"]["seq"].as_u64().unwrap();
                    if seq == 1001 {
                        page_before_live = Some(page.len());
                        continue;
                    }
                    // The first page frame proves the page is being served; the
                    // live event is published with most of it still to go.
                    if page.is_empty() {
                        server
                            .daemon
                            .events_tx
                            .send(live_event(&uid, 1001))
                            .unwrap();
                    }
                    page.push(seq);
                }
                Some("history_end") => break,
                _ => panic!("unexpected frame {message}"),
            }
        }
        assert_eq!(page, (501..=1000).collect::<Vec<u64>>());
        let sent_first = page_before_live
            .expect("the live event must arrive before the page's end, not after it");
        assert!(
            sent_first < 500,
            "the live event waited for the whole page: {sent_first} page events went first"
        );
    }

    /// **A page moves no watermark and tells the push gate nothing.** A device
    /// handed a whole page is still not counted as having seen any of it, and a
    /// subscription's watermark stays where `subscribe` and the replay put it: the
    /// next live seq is delivered as the successor rather than announced as a gap.
    #[tokio::test]
    async fn a_page_moves_no_watermark_and_records_no_delivery() {
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let uid = seed_backlog(&server.daemon, 600);
        let (mut socket, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;

        // Unsubscribed: a page of 101..600 to this device, and the gate still
        // rings it for every one of them.
        let (seqs, _) = history_page(&mut socket, &uid, 601, 500, "unsubscribed").await;
        assert_eq!(seqs, (101..=600).collect::<Vec<u64>>());
        let ticket = server.daemon.push_gate.admit_turn_end(&uid);
        let excluded = server
            .daemon
            .push_gate
            .exclusions_if_valid(&uid, ticket, false, 101)
            .expect("the ticket is current");
        assert!(
            excluded.is_empty(),
            "a page was counted as delivery: {excluded:?}"
        );

        // Subscribed at 300: the replay sends 301..600 and moves the watermark.
        socket
            .send(Message::Text(
                serde_json::json!({"type": "subscribe", "session_id": uid, "after_seq": 300})
                    .to_string(),
            ))
            .await
            .unwrap();
        let mut replayed = Vec::new();
        while replayed.last() != Some(&600) {
            let message = next_json(&mut socket).await.unwrap();
            replayed.push(message["event"]["seq"].as_u64().unwrap());
        }
        assert_eq!(replayed, (301..=600).collect::<Vec<u64>>());

        // A page below it, and then the next live seq is still the successor.
        let (seqs, _) = history_page(&mut socket, &uid, 301, 500, "below").await;
        assert_eq!(seqs, (1..=300).collect::<Vec<u64>>());
        server.daemon.events_tx.send(live_event(&uid, 601)).unwrap();
        let message = next_json(&mut socket).await.unwrap();
        assert_eq!(
            message["event"]["seq"], 601,
            "a page moved the watermark, so the next live event was not the successor: {message}"
        );
    }

    /// **Removing a run while its page is being sent ends the page with the
    /// reason**, rather than leaving the phone waiting on an end that never
    /// comes or reading a short page as complete.
    #[tokio::test]
    async fn a_page_cut_short_by_removing_its_session_ends_with_an_error() {
        let (daemon, _device) = daemon_with_a_device();
        let uid = seed_backlog(&daemon, 10);
        daemon
            .store
            .set_lifecycle(&uid, protocol::event::Lifecycle::Exited)
            .unwrap();

        let mut sink = CollectSink::default();
        let mut watermarks: HashMap<String, u64> = HashMap::new();
        let mut backlog = Backlog::default();
        let mut history = History::default();
        let ask = |request_id: &str| ClientMessage::History {
            session_id: uid.clone(),
            before_seq: 11,
            limit: 500,
            request_id: request_id.into(),
        };
        for request_id in ["cut", "queued"] {
            handle_message(
                &daemon,
                &mut sink,
                &mut watermarks,
                &mut backlog,
                &mut history,
                None,
                AnsweredOn::Iphone,
                1,
                ask(request_id),
            )
            .await
            .unwrap();
        }
        // The page is read, and its first two events are sent.
        for _ in 0..3 {
            history.step(&daemon, &mut sink).await.unwrap();
        }
        handle_message(
            &daemon,
            &mut sink,
            &mut watermarks,
            &mut backlog,
            &mut history,
            None,
            AnsweredOn::Iphone,
            1,
            ClientMessage::DeleteSession {
                session_uid: uid.clone(),
            },
        )
        .await
        .unwrap();
        for _ in 0..8 {
            if !history.in_progress() {
                break;
            }
            history.step(&daemon, &mut sink).await.unwrap();
        }
        assert!(!history.in_progress(), "both requests must be answered");

        let frames: Vec<serde_json::Value> = sink
            .0
            .iter()
            .map(|message| serde_json::to_value(message).unwrap())
            .collect();
        assert_eq!(frames[0]["event"]["seq"], 1);
        assert_eq!(frames[1]["event"]["seq"], 2);
        assert_eq!(frames[2]["type"], "delete_session_result");
        assert_eq!(frames[2]["result"]["status"], "deleted", "{}", frames[2]);
        let ends: Vec<&serde_json::Value> = frames
            .iter()
            .filter(|frame| frame["type"] == "history_end")
            .collect();
        assert_eq!(ends.len(), 2, "{frames:?}");
        assert_eq!(ends[0]["request_id"], "cut");
        assert_eq!(ends[0]["first_seq"], 1);
        assert!(ends[0]["error"].is_string(), "{}", ends[0]);
        assert_eq!(ends[1]["request_id"], "queued");
        assert!(ends[1]["error"].is_string(), "{}", ends[1]);
        assert_eq!(frames.len(), 5, "nothing else was sent: {frames:?}");
    }

    /// **A page of a run another connection removes ends with the reason**,
    /// whether the removal lands before the page is read — where an empty page
    /// would otherwise read as the head of the log — or while it is being sent.
    #[tokio::test]
    async fn a_page_of_a_session_removed_elsewhere_ends_with_an_error() {
        let (daemon, _device) = daemon_with_a_device();
        let mut sink = CollectSink::default();
        let mut watermarks: HashMap<String, u64> = HashMap::new();
        let mut backlog = Backlog::default();
        let mut history = History::default();
        // The connection that removes it.
        let mut other_sink = CollectSink::default();
        let mut other_watermarks: HashMap<String, u64> = HashMap::new();
        let mut other_backlog = Backlog::default();
        let mut other_history = History::default();

        for (removed_after_steps, request_id) in [(0usize, "before-read"), (2, "mid-page")] {
            let uid = seed_backlog(&daemon, 10);
            daemon
                .store
                .set_lifecycle(&uid, protocol::event::Lifecycle::Exited)
                .unwrap();
            handle_message(
                &daemon,
                &mut sink,
                &mut watermarks,
                &mut backlog,
                &mut history,
                None,
                AnsweredOn::Iphone,
                1,
                ClientMessage::History {
                    session_id: uid.clone(),
                    before_seq: 11,
                    limit: 500,
                    request_id: request_id.into(),
                },
            )
            .await
            .unwrap();
            for _ in 0..removed_after_steps {
                history.step(&daemon, &mut sink).await.unwrap();
            }
            handle_message(
                &daemon,
                &mut other_sink,
                &mut other_watermarks,
                &mut other_backlog,
                &mut other_history,
                None,
                AnsweredOn::Iphone,
                1,
                ClientMessage::DeleteSession {
                    session_uid: uid.clone(),
                },
            )
            .await
            .unwrap();
            let deleted = serde_json::to_value(other_sink.last()).unwrap();
            assert_eq!(
                deleted["result"]["status"], "deleted",
                "the premise: {deleted}"
            );
            for _ in 0..16 {
                if !history.in_progress() {
                    break;
                }
                history.step(&daemon, &mut sink).await.unwrap();
            }
            assert!(!history.in_progress());
            let end = serde_json::to_value(sink.last()).unwrap();
            assert_eq!(end["type"], "history_end", "{end}");
            assert_eq!(end["request_id"], request_id);
            assert!(end["error"].is_string(), "{request_id}: {end}");
        }
    }

    // ----------------------------------------------------------------- sinks

    /// A sink that decodes and records every server message, so a test can drive
    /// a path exactly as `handle_client` does and read back what the phone would
    /// see.
    #[derive(Default)]
    struct CollectSink(Vec<ServerMessage>);

    impl CollectSink {
        fn last(&self) -> &ServerMessage {
            self.0.last().expect("at least one message was sent")
        }
    }

    impl futures_util::Sink<Message> for CollectSink {
        type Error = std::convert::Infallible;
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(
            mut self: std::pin::Pin<&mut Self>,
            item: Message,
        ) -> Result<(), Self::Error> {
            if let Message::Text(text) = item {
                self.0
                    .push(serde_json::from_str(&text).expect("server messages are valid json"));
            }
            Ok(())
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Shortens the write deadline for one test and restores the production
    /// value on drop, so a test that panics part-way cannot leave every write
    /// behind it running against a 200ms bound.
    ///
    /// **The restore is not enough on its own, because the override is
    /// process-wide** — and neither is the setter taking a guard. A process-wide
    /// override is exclusive only when the test that *sets* it and every test
    /// that could *observe* it both take the same guard; a guard one side holds
    /// alone excludes nothing.
    ///
    /// Here the setter is `a_write_the_peer_never_takes_is_given_up_on`, and the
    /// observer is the test whose own writes must be allowed to take longer than
    /// 200ms: `a_gap_is_not_announced_while_that_sessions_replay_is_pending`,
    /// which pushes tens of thousands of frames over a real socket. Both take
    /// [`write_deadline_guard`].
    struct WriteDeadline;

    impl WriteDeadline {
        fn shortened_to(deadline: Duration) -> WriteDeadline {
            TEST_WRITE_DEADLINE_MS.store(
                deadline.as_millis() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            WriteDeadline
        }
    }

    impl Drop for WriteDeadline {
        fn drop(&mut self) {
            TEST_WRITE_DEADLINE_MS.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Serialises the test that shortens the write deadline with the test whose
    /// writes it would cut short — see [`WriteDeadline`]. A `tokio::sync::Mutex`
    /// because the guard is held across the tests' awaits; it does not poison,
    /// so a panicking test still frees the next.
    async fn write_deadline_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    /// A sink whose flush never completes, so a frame written to it is in flight
    /// and never taken — what a peer that stops reading its socket looks like
    /// from the writer's side.
    struct StuckSink;

    impl futures_util::Sink<Message> for StuckSink {
        type Error = std::convert::Infallible;
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(self: std::pin::Pin<&mut Self>, _: Message) -> Result<(), Self::Error> {
            Ok(())
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Pending
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A frame the peer never takes is given up on rather than waited on for
    /// ever.
    ///
    /// Every write in this file rides [`write_bounded`], and the deadline is the
    /// only reason a peer that stops reading its socket cannot wedge the very
    /// send that a revocation or a teardown would ride. Nothing else here tests
    /// it: a real peer with a full kernel send buffer is not something a unit
    /// test can arrange, so this substitutes a sink that simply never completes
    /// its flush — the same shape from the writer's side.
    ///
    /// The deadline is shortened so the test does not wait the production twenty
    /// seconds, and the whole call is wrapped in a longer deadline of its own:
    /// without that, removing the bound under test would hang the suite instead
    /// of failing this assertion.
    #[tokio::test]
    async fn a_write_the_peer_never_takes_is_given_up_on() {
        // The shortened deadline is process-wide, so this takes the write-deadline
        // guard: without it the 200ms below reaches whatever else is running, and
        // `a_gap_is_not_announced_while_that_sessions_replay_is_pending` would
        // fail its own writes for a deadline it never asked for.
        let _serial = write_deadline_guard().await;
        let deadline = Duration::from_millis(200);
        let _shortened = WriteDeadline::shortened_to(deadline);
        let mut sink = StuckSink;
        let outcome = tokio::time::timeout(
            deadline * 10,
            write_bounded(&mut sink, Message::Text("frame".into())),
        )
        .await;
        let Ok(result) = outcome else {
            panic!("the write was never given up on — it is not bounded by WRITE_DEADLINE");
        };
        let err = result.expect_err("a peer that never takes the frame must fail the write");
        assert!(
            format!("{err:#}").contains("peer not reading"),
            "the failure must name what it means, got {err:#}"
        );
    }

    /// **A paired device gets the project list; the bootstrap token gets a
    /// refusal.** A folder that is gone is listed as gone, and a repository's
    /// branch rides along. The list's agent check asks the test's own claude.
    #[tokio::test]
    async fn list_projects_answers_a_paired_device_with_its_folders() {
        let _env = crate::ProcessEnvRead::take();
        let root = std::env::temp_dir().join(format!(
            "ccd-projects-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let (claude, _) = crate::start_session::test_claude(&root.join("claude-rig"));
        let (server, _device) = live_server(protocol::config::Config {
            claude_bin: Some(claude.display().to_string()),
            codex_bin: Some(
                crate::start_session::test_codex(&claude)
                    .display()
                    .to_string(),
            ),
            ..Default::default()
        })
        .await;
        let repo = root.join("app");
        std::fs::create_dir_all(&repo).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q", "-b", "feature/x"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(init.success());
        let gone = root.join("gone");
        for (cwd, at) in [
            (&gone, "2026-11-01T00:00:00.000Z"),
            (&repo, "2026-11-02T00:00:00.000Z"),
        ] {
            let uid = protocol::uid::new().unwrap();
            let row = crate::store::SessionRow {
                session_uid: uid.clone(),
                session_id: "cc-1".into(),
                cwd: cwd.display().to_string(),
                claude_session_id: None,
                transcript_path: None,
                lifecycle: protocol::event::Lifecycle::Exited,
                created_at: at.into(),
                updated_at: at.into(),
                agent: protocol::agent::AgentKind::Claude,
                codex_thread_id: None,
            };
            assert_eq!(
                server.store.begin_run(&row, &uid, None, None).unwrap(),
                None
            );
        }
        let list = Message::Text(r#"{"type":"list_projects"}"#.into());

        let (mut bootstrap, ack) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(ack["capabilities"]["start_session"], false, "{ack}");
        bootstrap.send(list.clone()).await.unwrap();
        let refused = next_json(&mut bootstrap).await.unwrap();
        assert_eq!(refused["code"], "not_authorised", "{refused}");

        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["capabilities"]["start_session"], true, "{ack}");
        phone.send(list).await.unwrap();
        let reply = match next_frame(&mut phone, Duration::from_secs(30)).await {
            Frame::Json(reply) => reply,
            _ => panic!("no projects reply within 30 s"),
        };
        assert_eq!(reply["type"], "projects", "{reply}");
        let projects = reply["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 2, "{reply}");
        assert_eq!(projects[0]["path"], repo.display().to_string());
        assert_eq!(projects[0]["name"], "app");
        assert_eq!(projects[0]["exists"], true);
        assert_eq!(projects[0]["git_branch"], "feature/x");
        assert_eq!(projects[1]["name"], "gone");
        assert_eq!(projects[1]["exists"], false);
        assert!(projects[1].get("git_branch").is_none(), "{reply}");
        let agents: Vec<&str> = reply["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|agent| agent["agent"].as_str().unwrap())
            .collect();
        assert_eq!(agents, ["claude", "codex", "antigravity"]);
        assert_eq!(
            reply["agents"][0]["available"], true,
            "the stub claude was asked: {reply}"
        );
        assert_eq!(reply["agents"][0]["signed_in"], true, "{reply}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// **A machine-stats sample holds nothing up**: it is advertised, its answer
    /// carries every value, and a `sessions` asked right behind it on the same
    /// connection is answered first, well inside the sample's half second.
    #[tokio::test]
    async fn machine_stats_is_answered_without_holding_up_the_connection() {
        let _env = crate::ProcessEnvRead::take();
        let (server, _device) = live_server(protocol::config::Config::default()).await;
        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["capabilities"]["machine_stats"], true, "{ack}");

        let asked = std::time::Instant::now();
        for request in [ClientMessage::GetMachineStats, ClientMessage::Sessions] {
            phone
                .send(Message::Text(serde_json::to_string(&request).unwrap()))
                .await
                .unwrap();
        }
        let first = next_json(&mut phone).await.unwrap();
        let sessions_after = asked.elapsed();
        assert_eq!(first["type"], "sessions", "{first}");
        assert!(
            sessions_after < crate::machine::CPU_WINDOW / 2,
            "sessions waited {sessions_after:?} behind the sample"
        );

        let stats = next_json(&mut phone).await.unwrap();
        assert_eq!(stats["type"], "machine_stats", "{stats}");
        assert!(asked.elapsed() >= crate::machine::CPU_WINDOW, "{stats}");
        for key in [
            "cpu",
            "memory_used",
            "memory_total",
            "disk_used",
            "disk_total",
        ] {
            assert!(stats[key].is_number(), "{key}: {stats}");
        }
    }

    /// **Browsing and making folders answer only a paired device**, as starting
    /// does: the bootstrap token is refused, and a paired phone gets its listing and
    /// its new folder.
    #[tokio::test]
    async fn list_folder_and_create_folder_answer_only_a_paired_device() {
        let (server, _device) = live_server(protocol::config::Config::default()).await;
        let root = std::env::temp_dir().join(format!(
            "ccd-browse-ws-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(root.join("app")).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.display().to_string();
        let list = Message::Text(
            serde_json::to_string(&ClientMessage::ListFolder {
                request_id: "b1".into(),
                path: Some(path.clone()),
                show_hidden: false,
            })
            .unwrap(),
        );
        let create = Message::Text(
            serde_json::to_string(&ClientMessage::CreateFolder {
                request_id: "c1".into(),
                parent: path.clone(),
                name: "made".into(),
            })
            .unwrap(),
        );

        let (mut bootstrap, ack) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(ack["capabilities"]["browse_folders"], false, "{ack}");
        for request in [list.clone(), create.clone()] {
            bootstrap.send(request).await.unwrap();
            let refused = next_json(&mut bootstrap).await.unwrap();
            assert_eq!(refused["code"], "not_authorised", "{refused}");
        }
        assert!(
            !root.join("made").exists(),
            "a refused create makes nothing"
        );

        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["capabilities"]["browse_folders"], true, "{ack}");
        phone.send(list).await.unwrap();
        assert_eq!(
            next_json(&mut phone).await.unwrap(),
            serde_json::json!({"type": "folder_listing", "request_id": "b1", "path": path,
                "folders": ["app"], "truncated": false})
        );
        phone.send(create).await.unwrap();
        assert_eq!(
            next_json(&mut phone).await.unwrap(),
            serde_json::json!({"type": "create_folder_result", "request_id": "c1",
                "result": {"status": "created"}})
        );
        assert!(root.join("made").is_dir());

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The session a fixture's start outcome names, in `cwd`. Filed long ago, so the
    /// project list's order and dates are the fixture's own.
    fn register_stub_session(store: &crate::store::Store, cwd: String) {
        let row = crate::store::SessionRow {
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            session_id: "cc-9".into(),
            cwd,
            claude_session_id: None,
            transcript_path: None,
            lifecycle: protocol::event::Lifecycle::Exited,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
            agent: protocol::agent::AgentKind::Claude,
            codex_thread_id: None,
        };
        assert_eq!(
            store.begin_run(&row, &row.session_uid, None, None).unwrap(),
            None
        );
    }

    /// **The start-session wire a minor-28 daemon sent, still in what this build
    /// sends.** The fixture is what minor 28 emitted, kept as it was: the phone's
    /// tests read its copy under `ios/` as an older Mac. This build may add a
    /// field a minor-28 phone ignores, and nothing else, so every value the
    /// fixture holds must be in this build's wire unchanged. Capabilities removed in X12
    /// leave the record with their removal; any other difference is still a failure.
    /// Its start outcomes are set, not started ([`derived_minor_28_wire`]): this build
    /// checks their shape, not
    /// what a start answers today.
    #[tokio::test]
    async fn the_minor_28_wire_fixture_is_still_in_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/claude/minor-28-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let emitted = derived_minor_28_wire().await;
        if let Some(changed) = first_difference(&committed, &emitted, "$") {
            panic!(
                "a minor-28 phone would read this build differently at {changed}; only a \
                 field it ignores may be added"
            );
        }
    }

    /// Where `wire` stops holding `fixture`: an object may carry fields the
    /// fixture lacks, and every other value must be the same.
    fn first_difference(
        fixture: &serde_json::Value,
        wire: &serde_json::Value,
        at: &str,
    ) -> Option<String> {
        match (fixture, wire) {
            (serde_json::Value::Object(fixture), serde_json::Value::Object(wire)) => {
                fixture.iter().find_map(|(key, value)| match wire.get(key) {
                    Some(sent) => first_difference(value, sent, &format!("{at}.{key}")),
                    None => Some(format!("{at}.{key} (missing)")),
                })
            }
            (serde_json::Value::Array(fixture), serde_json::Value::Array(wire))
                if fixture.len() == wire.len() =>
            {
                fixture
                    .iter()
                    .zip(wire)
                    .enumerate()
                    .find_map(|(index, (value, sent))| {
                        first_difference(value, sent, &format!("{at}[{index}]"))
                    })
            }
            _ => (fixture != wire).then(|| format!("{at}: {fixture} became {wire}")),
        }
    }

    /// **The branch and worktree wire a minor-29 daemon sends, emitted by this
    /// build**, but for its start outcomes, which are set, not started
    /// ([`derived_minor_29_wire`]). The phone's tests read the copy under `ios/`
    /// (byte-identical, by `fixtures/check-duplicated-fixtures.sh`).
    #[tokio::test]
    async fn the_minor_29_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/claude/minor-29-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_minor_29_wire().await,
            committed,
            "the committed minor-29 wire contract no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_minor_29_wire_fixture, then copy it over its copy under ios/ \
             (fixtures/check-duplicated-fixtures.sh names it)"
        );
    }

    /// Rewrite `fixtures/claude/minor-29-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_minor_29_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_minor_29_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/minor-29-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// **The folder wire a minor-30 daemon sends, emitted by this build**, but for
    /// its start outcomes, which are set, not started ([`derived_minor_30_wire`]). The
    /// phone's tests read the copy under `ios/` (byte-identical, by
    /// `fixtures/check-duplicated-fixtures.sh`).
    #[tokio::test]
    async fn the_minor_30_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/claude/minor-30-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_minor_30_wire().await,
            committed,
            "the committed minor-30 wire contract no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_minor_30_wire_fixture, then copy it over its copy under ios/ \
             (fixtures/check-duplicated-fixtures.sh names it)"
        );
    }

    /// Rewrite `fixtures/claude/minor-30-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_minor_30_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_minor_30_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/minor-30-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// **The session-list fields and machine stats a minor-31 daemon sends,
    /// emitted by this build.** The Mac app's tests read the copy under `ios/`
    /// (byte-identical, by `fixtures/check-duplicated-fixtures.sh`).
    #[tokio::test]
    async fn the_minor_31_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/claude/minor-31-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_minor_31_wire().await,
            committed,
            "the committed minor-31 wire contract no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_minor_31_wire_fixture, then copy it over its copy under ios/ \
             (fixtures/check-duplicated-fixtures.sh names it)"
        );
    }

    /// Rewrite `fixtures/claude/minor-31-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_minor_31_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_minor_31_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/minor-31-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// **Ending a run, as a minor-35 daemon sends it, emitted by this build.** The
    /// phone's tests read the copy under `ios/` (byte-identical, by
    /// `fixtures/check-duplicated-fixtures.sh`).
    #[tokio::test]
    async fn the_minor_35_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/claude/minor-35-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_minor_35_wire().await,
            committed,
            "the committed minor-35 wire contract no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_minor_35_wire_fixture, then copy it over its copy under ios/ \
             (fixtures/check-duplicated-fixtures.sh names it)"
        );
    }

    /// Rewrite `fixtures/claude/minor-35-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_minor_35_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_minor_35_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/minor-35-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// The capabilities a paired phone is offered over a live socket, with `stop_run`;
    /// the `stop_run` request and its three answers, the refusal as the daemon words an
    /// unknown run; and a session list holding a run that is `stopping`.
    async fn derived_minor_35_wire() -> serde_json::Value {
        let (server, _device) = live_server(protocol::config::Config::default()).await;
        let (_phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
        let answer = |result| {
            serde_json::to_value(ServerMessage::StopRunResult {
                session_id: uid.into(),
                request_id: "stop-1".into(),
                result,
            })
            .unwrap()
        };
        let stopping = protocol::event::SessionSummary {
            session_uid: uid.into(),
            session_id: uid.into(),
            cwd: "/tmp/ccd-minor-35/app".into(),
            lifecycle: protocol::event::Lifecycle::Stopping,
            link: protocol::event::Link::Attached,
            claude_session_id: None,
            transcript_path: None,
            last_seq: 12,
            created_at: "2026-11-14T07:00:00.000Z".into(),
            updated_at: "2026-11-14T08:00:00.000Z".into(),
            blocked_on: Vec::new(),
            project_label: crate::project_label::project_label("/tmp/ccd-minor-35/app"),
            agent: protocol::agent::AgentKind::Claude,
            codex_thread_id: None,
            codex_link: protocol::event::CodexLink::None,
            pending: Vec::new(),
            model: None,
            permission_mode: None,
            effort: None,
            menus: None,
            model_label: None,
            git_branch: None,
            diff_stat: None,
            relaunchable: false,
            switching: None,
            account: None,
        };
        serde_json::json!({
            "hello_ack_capabilities": ack["capabilities"],
            "stop_run_request": serde_json::to_value(ClientMessage::StopRun {
                session_id: uid.into(),
                request_id: "stop-1".into(),
            })
            .unwrap(),
            "stop_run_stopping": answer(protocol::ws::StopRunResult::Stopping),
            "stop_run_already_ended": answer(protocol::ws::StopRunResult::AlreadyEnded),
            "stop_run_refused": answer(server.daemon.stop_run(uid).await),
            "sessions": serde_json::to_value(ServerMessage::Sessions {
                sessions: vec![stopping],
            })
            .unwrap(),
        })
    }

    /// The capabilities a paired phone is offered over a live socket, the
    /// `get_machine_stats` request, and the two answers it can get, whole and
    /// with the disk unread; and a session list with and without a branch and
    /// line counts. The answers are this build's encoding of fixed values, so the
    /// fixture is the same on every machine: how they are read is covered by the
    /// tests of `machine` and of the turn-end refresh.
    async fn derived_minor_31_wire() -> serde_json::Value {
        let (server, _device) = live_server(protocol::config::Config::default()).await;
        let (_phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        let stats = |disk: Option<(u64, u64)>| ServerMessage::MachineStats {
            cpu: Some(0.38),
            memory_used: Some(76_235_669_504),
            memory_total: Some(206_158_430_208),
            disk_used: disk.map(|(used, _)| used),
            disk_total: disk.map(|(_, total)| total),
            captured_at: "2026-11-07T04:00:00.000Z".into(),
        };
        let summary = |uid: &str, name: &str, cwd: &str, tree: Option<(&str, u64, u64)>| {
            protocol::event::SessionSummary {
                session_uid: uid.into(),
                session_id: name.into(),
                cwd: cwd.into(),
                lifecycle: protocol::event::Lifecycle::Live,
                link: protocol::event::Link::Attached,
                claude_session_id: None,
                transcript_path: None,
                last_seq: 12,
                created_at: "2026-11-07T03:00:00.000Z".into(),
                updated_at: "2026-11-07T04:00:00.000Z".into(),
                blocked_on: Vec::new(),
                project_label: crate::project_label::project_label(cwd),
                agent: protocol::agent::AgentKind::Claude,
                codex_thread_id: None,
                codex_link: protocol::event::CodexLink::None,
                pending: Vec::new(),
                model: None,
                permission_mode: None,
                effort: None,
                menus: None,
                model_label: None,
                git_branch: tree.map(|(branch, _, _)| branch.to_string()),
                diff_stat: tree
                    .map(|(_, added, removed)| protocol::event::DiffStat { added, removed }),

                relaunchable: false,
                switching: None,
                account: None,
            }
        };
        serde_json::json!({
            "hello_ack_capabilities": ack["capabilities"],
            "get_machine_stats_request": serde_json::to_value(ClientMessage::GetMachineStats)
                .unwrap(),
            "machine_stats": serde_json::to_value(stats(Some((1_400_000_000_000, 4_000_000_000_000))))
                .unwrap(),
            "machine_stats_partial": serde_json::to_value(stats(None)).unwrap(),
            "sessions": serde_json::to_value(ServerMessage::Sessions {
                sessions: vec![
                    summary(
                        "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
                        "cc-1",
                        "/tmp/ccd-minor-31/app",
                        Some(("feature/fleet", 120, 7)),
                    ),
                    summary("01K1B3XQ8ZC0DE5FGH7JKMNPQS", "cc-2", "/tmp/ccd-minor-31/notes", None),
                ],
            })
            .unwrap(),
        })
    }

    /// A folder's folders, a new folder, a start in a folder no session has run in,
    /// and that folder's repository, as a paired phone receives them over a live
    /// socket; and each refusal of a relative path. The start's outcomes are set, not
    /// started: a start runs an agent, and these are the frames the apps decode, as
    /// the launcher route answered them.
    ///
    /// Made afresh under `/tmp/ccd-minor-30`, written as that path wherever it
    /// really is (`/private/tmp/…` on a Mac), so the fixture is the same on every
    /// machine.
    async fn derived_minor_30_wire() -> serde_json::Value {
        use protocol::agent::AgentKind;

        let root = std::path::Path::new("/tmp/ccd-minor-30");
        let _ = std::fs::remove_dir_all(root);
        let src = root.join("src");
        for folder in ["app", ".config", "semi;"] {
            std::fs::create_dir_all(src.join(folder)).unwrap();
        }
        std::fs::write(src.join("README"), "").unwrap();
        let app = src.join("app");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&app)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .env("GIT_AUTHOR_DATE", "2026-11-04T09:00:00Z")
                .env("GIT_COMMITTER_DATE", "2026-11-04T09:00:00Z")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "first"]);
        let canonical = root.canonicalize().unwrap().display().to_string();
        let at = |folder: &std::path::Path| {
            folder
                .display()
                .to_string()
                .replacen("/tmp/ccd-minor-30", &canonical, 1)
        };

        let (server, _device) = live_server(protocol::config::Config::default()).await;
        register_stub_session(&server.store, at(&app));
        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        async fn ask(
            phone: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
            message: ClientMessage,
        ) -> (serde_json::Value, serde_json::Value) {
            let request = serde_json::to_value(&message).unwrap();
            phone
                .send(Message::Text(request.to_string()))
                .await
                .unwrap();
            (request, next_json(phone).await.unwrap())
        }
        let list = |path: Option<String>, show_hidden: bool| ClientMessage::ListFolder {
            request_id: "b1".into(),
            path,
            show_hidden,
        };
        let (list_folder_request, folder_listing) =
            ask(&mut phone, list(Some(at(&src)), false)).await;
        let (list_folder_hidden_request, folder_listing_hidden) =
            ask(&mut phone, list(Some(at(&src)), true)).await;
        let (_, folder_listing_unreadable) =
            ask(&mut phone, list(Some(at(&root.join("gone"))), false)).await;
        let (_, folder_listing_relative) = ask(&mut phone, list(Some("src".into()), false)).await;
        let create = |name: &str| ClientMessage::CreateFolder {
            request_id: "c1".into(),
            parent: at(&src),
            name: name.into(),
        };
        let (create_folder_request, create_folder_created) =
            ask(&mut phone, create("new-app")).await;
        let (_, create_folder_failed_exists) = ask(&mut phone, create("new-app")).await;
        let (_, create_folder_failed_not_one_name) = ask(&mut phone, create("a/b")).await;
        let (get_project_git_unlisted_request, project_git_unlisted) =
            ask(&mut phone, ClientMessage::GetProjectGit { path: at(&app) }).await;
        let (_, get_project_git_relative_refused) = ask(
            &mut phone,
            ClientMessage::GetProjectGit {
                path: "src/app".into(),
            },
        )
        .await;

        let reply = |result: protocol::ws::StartSessionResult| {
            serde_json::to_value(ServerMessage::StartSessionResult {
                request_id: "r1".into(),
                result,
            })
            .unwrap()
        };
        let wire = serde_json::json!({
            "hello_ack_capabilities": ack["capabilities"],
            "list_folder_home_request": serde_json::to_value(list(None, false)).unwrap(),
            "list_folder_request": list_folder_request,
            "folder_listing": folder_listing,
            "list_folder_hidden_request": list_folder_hidden_request,
            "folder_listing_hidden": folder_listing_hidden,
            "folder_listing_unreadable": folder_listing_unreadable,
            "folder_listing_relative": folder_listing_relative,
            "create_folder_request": create_folder_request,
            "create_folder_created": create_folder_created,
            "create_folder_failed_exists": create_folder_failed_exists,
            "create_folder_failed_not_one_name": create_folder_failed_not_one_name,
            "start_session_unlisted_request": serde_json::to_value(ClientMessage::StartSession {
                request_id: "r1".into(),
                path: at(&src.join("new-app")),
                agent: AgentKind::Claude,
                prompt: None,
                worktree: None,
                model: None,
                trust: None,
                attachments: Vec::new(),
            })
            .unwrap(),
            "start_session_unlisted_started": reply(protocol::ws::StartSessionResult::Started {
                session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
                cwd: Some(at(&src.join("new-app"))),
            }),
            "start_session_relative_failed": reply(protocol::ws::StartSessionResult::Failed {
                reason: protocol::ws::StartFailure::LaunchFailed,
                detail: "\"src/new-app\" is not an absolute path".into(),
            }),
            "get_project_git_unlisted_request": get_project_git_unlisted_request,
            "project_git_unlisted": project_git_unlisted,
            "get_project_git_relative_refused": get_project_git_relative_refused,
        });
        let _ = std::fs::remove_dir_all(root);
        serde_json::from_str(&wire.to_string().replace(&canonical, "/tmp/ccd-minor-30")).unwrap()
    }

    /// A repository's worktrees and branches as a paired phone receives them over
    /// a live socket, and the `start_session_result` of a start in each kind of
    /// worktree. The outcomes are set, not started: a start runs an agent, and these
    /// are the frames the apps decode, as the launcher route answered them.
    ///
    /// The repository is made afresh under `/tmp/ccd-minor-29`, and the folder
    /// git reports (`/private/tmp/…` on a Mac) is written as that path, so the
    /// fixture is the same on every machine. Commit dates are fixed, so the
    /// branches come in the same order.
    async fn derived_minor_29_wire() -> serde_json::Value {
        use protocol::agent::AgentKind;
        use protocol::ws::{StartSessionResult, StartWorktree};

        let root = std::path::Path::new("/tmp/ccd-minor-29");
        let _ = std::fs::remove_dir_all(root);
        let (app, notes) = (root.join("app"), root.join("notes"));
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&notes).unwrap();
        let git = |dir: &std::path::Path, date: &str, args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        let commit = |dir: &std::path::Path, date: &str| {
            git(dir, date, &["commit", "-q", "--allow-empty", "-m", date]);
        };
        git(&app, "2026-11-01T09:00:00Z", &["init", "-q", "-b", "main"]);
        commit(&app, "2026-11-01T09:00:00Z");
        for (branch, date) in [
            ("bugfix-12", "2026-11-02T09:00:00Z"),
            ("feature/search", "2026-11-03T09:00:00Z"),
        ] {
            git(&app, date, &["switch", "-q", "-c", branch]);
            commit(&app, date);
        }
        git(&app, "2026-11-01T09:00:00Z", &["switch", "-q", "main"]);
        let review = root.join("app-review");
        let gone = root.join("app-spike");
        for (folder, branch) in [(&review, "review/ui"), (&gone, "old-spike")] {
            let folder = folder.display().to_string();
            git(
                &app,
                "2026-11-01T09:00:00Z",
                &["worktree", "add", "-q", "-b", branch, &folder],
            );
        }
        std::fs::remove_dir_all(&gone).unwrap();
        std::fs::write(app.join("notes.txt"), "uncommitted\n").unwrap();

        let canonical = root.canonicalize().unwrap().display().to_string();
        let at = |folder: &std::path::Path| {
            folder
                .display()
                .to_string()
                .replacen("/tmp/ccd-minor-29", &canonical, 1)
        };
        let (server, _device) = live_server(protocol::config::Config::default()).await;
        for cwd in [at(&app), at(&notes)] {
            let row = crate::store::SessionRow {
                session_uid: protocol::uid::new().unwrap(),
                session_id: "cc-1".into(),
                cwd,
                claude_session_id: None,
                transcript_path: None,
                lifecycle: protocol::event::Lifecycle::Exited,
                created_at: "2026-11-04T09:00:00.000Z".into(),
                updated_at: "2026-11-04T09:00:00.000Z".into(),
                agent: AgentKind::Claude,
                codex_thread_id: None,
            };
            assert_eq!(
                server
                    .store
                    .begin_run(&row, &row.session_uid, None, None)
                    .unwrap(),
                None
            );
        }

        register_stub_session(&server.store, at(&app));
        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        let get_project_git =
            |path: String| serde_json::to_value(ClientMessage::GetProjectGit { path }).unwrap();
        let mut answers = Vec::new();
        for path in [at(&app), at(&notes)] {
            phone
                .send(Message::Text(get_project_git(path).to_string()))
                .await
                .unwrap();
            answers.push(next_json(&mut phone).await.unwrap());
        }
        let [project_git, project_git_not_a_repository] =
            <[serde_json::Value; 2]>::try_from(answers).unwrap();
        let get_project_git_request = get_project_git(at(&app));

        let reply = |request_id: &str, result: StartSessionResult| {
            serde_json::to_value(ServerMessage::StartSessionResult {
                request_id: request_id.into(),
                result,
            })
            .unwrap()
        };
        let started = |cwd: Option<String>| StartSessionResult::Started {
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            cwd,
        };
        let refused = |detail: String| StartSessionResult::Failed {
            reason: protocol::ws::StartFailure::LaunchFailed,
            detail,
        };
        let worktrees = app.join(".claude/worktrees");
        let request = |agent: AgentKind, worktree: StartWorktree| {
            serde_json::to_value(ClientMessage::StartSession {
                request_id: "r1".into(),
                path: at(&app),
                agent,
                prompt: None,
                worktree: Some(worktree),
                model: None,
                trust: None,
                attachments: Vec::new(),
            })
            .unwrap()
        };
        let existing = || StartWorktree::Existing { path: at(&review) };
        let branch = || StartWorktree::Branch {
            branch: "feature/search".into(),
        };
        let new = || StartWorktree::New {
            name: "fix-login".into(),
        };

        let wire = serde_json::json!({
            "hello_ack_capabilities": ack["capabilities"],
            "get_project_git_request": get_project_git_request,
            "project_git": project_git,
            "project_git_not_a_repository": project_git_not_a_repository,
            "start_session_existing_request": request(AgentKind::Claude, existing()),
            "start_session_branch_request": request(AgentKind::Codex, branch()),
            "start_session_new_request": request(AgentKind::Claude, new()),
            "start_existing_started": reply("r1", started(Some(at(&review)))),
            "start_branch_started": reply(
                "r2",
                started(Some(at(&worktrees.join("feature+search"))))
            ),
            "start_new_claude_started": reply("r3", started(None)),
            "start_new_codex_started": reply("r4", started(Some(at(&worktrees.join("fix-login"))))),
            "start_new_codex_failed_exists": reply(
                "r5",
                refused("fatal: a branch named 'fix-login' already exists".into())
            ),
            "start_new_failed_invalid_name": reply(
                "r6",
                refused(
                    "\"fix login;\" is not a valid worktree name: use only letters, digits, \
                     dots, underscores and dashes"
                        .into()
                )
            ),
            "start_branch_failed_checked_out": reply(
                "r7",
                refused(format!("old-spike is already checked out in {}", at(&gone)))
            ),
        });
        let _ = std::fs::remove_dir_all(root);
        serde_json::from_str(&wire.to_string().replace(&canonical, "/tmp/ccd-minor-29")).unwrap()
    }

    /// The handshake and project list as a paired phone receives them over a
    /// live socket, and each start outcome as a `start_session_result`. The outcomes
    /// are set, not started: a start runs an agent, and these are the frames the
    /// apps decode, as the launcher route answered them.
    ///
    /// Fixed paths under `/tmp/ccd-minor-28`, so the fixture is the same on every
    /// machine. Which agents can start is set rather than checked: a check asks
    /// this machine's agents, and the fixture would say what it found. The check
    /// the list starts for the next list asks the fixture's own claude.
    async fn derived_minor_28_wire() -> serde_json::Value {
        use protocol::agent::AgentKind;
        use protocol::ws::{AgentAvailability, StartFailure, StartSessionResult};

        let _env = crate::ProcessEnvRead::take();

        let root = std::path::Path::new("/tmp/ccd-minor-28");
        let (app, notes, gone) = (root.join("app"), root.join("notes"), root.join("gone"));
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&notes).unwrap();
        assert!(!gone.exists(), "{} must not exist", gone.display());
        let init = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&app)
            .status()
            .unwrap();
        assert!(init.success());
        let (claude, _) = crate::start_session::test_claude(&root.join("claude-rig"));

        let (server, _device) = live_server(protocol::config::Config {
            claude_bin: Some(claude.display().to_string()),
            codex_bin: Some(
                crate::start_session::test_codex(&claude)
                    .display()
                    .to_string(),
            ),
            ..Default::default()
        })
        .await;
        for (cwd, at, agent) in [
            (&app, "2026-11-04T09:00:00.000Z", AgentKind::Codex),
            (&notes, "2026-11-03T18:30:00.000Z", AgentKind::Claude),
            (&gone, "2026-10-01T08:00:00.000Z", AgentKind::Claude),
        ] {
            let row = crate::store::SessionRow {
                session_uid: protocol::uid::new().unwrap(),
                session_id: "cc-1".into(),
                cwd: cwd.display().to_string(),
                claude_session_id: None,
                transcript_path: None,
                lifecycle: protocol::event::Lifecycle::Exited,
                created_at: at.into(),
                updated_at: at.into(),
                agent,
                codex_thread_id: None,
            };
            assert_eq!(
                server
                    .store
                    .begin_run(&row, &row.session_uid, None, None)
                    .unwrap(),
                None
            );
        }
        // The accounts the check was made on, one per agent the phone can start.
        server.daemon.agents.set(
            vec![None, None, None],
            vec![
                AgentAvailability {
                    agent: AgentKind::Claude,
                    available: true,
                    detail: None,
                    models: Vec::new(),
                    signed_in: None,
                },
                AgentAvailability {
                    agent: AgentKind::Codex,
                    available: false,
                    detail: Some("could not find the codex binary: install Codex".into()),
                    models: Vec::new(),
                    signed_in: None,
                },
            ],
        );

        register_stub_session(&server.store, app.display().to_string());
        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        let list_projects_request = serde_json::to_value(ClientMessage::ListProjects).unwrap();
        phone
            .send(Message::Text(list_projects_request.to_string()))
            .await
            .unwrap();
        let projects = next_json(&mut phone).await.unwrap();

        let reply = |request_id: &str, result: StartSessionResult| {
            serde_json::to_value(ServerMessage::StartSessionResult {
                request_id: request_id.into(),
                result,
            })
            .unwrap()
        };
        let started = StartSessionResult::Started {
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            cwd: None,
        };
        let failed = |reason: StartFailure, detail: &str| StartSessionResult::Failed {
            reason,
            detail: detail.into(),
        };
        let starts = crate::start_session::Starts::default();
        let first = starts.once("r-repeat", async { started.clone() }).await;
        let again = starts
            .once("r-repeat", async {
                failed(StartFailure::LaunchFailed, "a second start ran")
            })
            .await;

        serde_json::json!({
            "hello_ack_capabilities": ack["capabilities"],
            "list_projects_request": list_projects_request,
            "start_session_request": serde_json::to_value(ClientMessage::StartSession {
                request_id: "r1".into(),
                path: app.display().to_string(),
                agent: AgentKind::Claude,
                prompt: Some("fix the \"login\" test; then run it\nand commit;".into()),
                worktree: None,
                model: None,
                trust: None,
                attachments: Vec::new(),
            })
            .unwrap(),
            "projects": projects,
            "start_started": reply("r1", started),
            "start_failed_folder_missing": reply(
                "r2",
                failed(
                    StartFailure::FolderMissing,
                    &format!("{} no longer exists", gone.display())
                )
            ),
            "start_failed_agent_missing": reply(
                "r3",
                failed(
                    StartFailure::AgentMissing,
                    "could not find the codex binary: install Codex"
                )
            ),
            "start_failed_launch_failed": reply(
                "r4",
                failed(StartFailure::LaunchFailed, "creating tmux session cc-3")
            ),
            "start_repeat": [reply("r-repeat", first), reply("r-repeat", again)],
        })
    }

    /// The `private_transport` capability says whether this connection's bytes
    /// are unreadable in transit, over every transport the daemon serves.
    ///
    /// It is a fact about the wire, so a paired device and the bootstrap token
    /// are told the same. The `ws_allow_plaintext` rows are the ones that matter:
    /// the listener admits the connection on the operator's word, and that word
    /// is about their network, not about the bytes on it.
    #[tokio::test]
    async fn private_transport_is_true_only_where_the_bytes_cannot_be_read_in_transit() {
        let (daemon, _device) = daemon_with_a_device();
        let paired = || {
            AuthOutcome::Device(Box::new(crate::store::DeviceRow {
                device_id: "dev-1".into(),
                name: "iPhone".into(),
                created_at: protocol::time::now_rfc3339(),
                last_seen_at: None,
                revoked_at: None,
                tailnet_node: None,
                paired_before_nodes: false,
            }))
        };
        let private = |tls_active: bool, trust: PlaintextTrust, outcome: AuthOutcome| {
            let daemon = Arc::clone(&daemon);
            async move {
                match hello_ack(
                    &daemon,
                    outcome,
                    tls_active,
                    tls_active || trust.is_private(),
                )
                .await
                .expect("this device reads cleanly, so the ack builds")
                {
                    ServerMessage::HelloAck { capabilities, .. } => capabilities.private_transport,
                    other => panic!("hello_ack must answer an ack, got {other:?}"),
                }
            }
        };

        assert!(
            private(false, PlaintextTrust::TrustedPath, paired()).await,
            "loopback and this Mac's tailnet address are private already"
        );
        assert!(
            private(true, PlaintextTrust::RequireTls, paired()).await,
            "and so is wss, on any listener"
        );
        assert!(
            !private(false, PlaintextTrust::OperatorAllowed, paired()).await,
            "ws_allow_plaintext admits the connection; it does not make the LAN private"
        );
        assert!(
            private(true, PlaintextTrust::TrustedPath, AuthOutcome::Static).await,
            "the bootstrap token is told the same fact about the same wire"
        );
        assert!(
            !private(false, PlaintextTrust::OperatorAllowed, AuthOutcome::Static).await,
            "and the operator's LAN is not private for it either"
        );
    }

    /// **215 G1 at the wire: a `set_mode` frame that does not say the person confirmed Bypass
    /// permissions is refused.** The frame as an app without the confirmation sends it, with no
    /// `confirmed`, to a Claude run this daemon knows, is answered with the confirmation
    /// refusal: ccd checks the mode before it looks for the run's host.
    #[tokio::test]
    async fn a_set_mode_frame_without_the_confirmation_is_refused() {
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
        hosting_a_claude_run(&server.daemon, uid);
        let (mut phone, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        let frame = serde_json::json!({"type": "set_mode", "session_id": uid,
            "request_id": "mode-1", "mode": "bypassPermissions"});
        phone.send(Message::Text(frame.to_string())).await.unwrap();
        let answer = loop {
            let reply = next_json(&mut phone).await.expect("an answer to set_mode");
            if reply["type"] == "setting_result" {
                break reply;
            }
        };
        assert_eq!(answer["request_id"], "mode-1", "{answer}");
        assert_eq!(
            answer["result"]["reason"],
            "Turning on Bypass permissions needs your confirmation in the app; nothing was changed.",
            "{answer}"
        );
    }

    /// On the LAN the operator switched on with `ws_allow_plaintext`, a
    /// paired device may start sessions and browse folders, as it may read, answer and
    /// say there. The bootstrap token still may not, and the ack does not call the LAN
    /// private. The list's agent check asks the test's own claude.
    #[tokio::test]
    async fn a_paired_device_on_the_operator_allowed_lan_may_start_and_browse() {
        let _env = crate::ProcessEnvRead::take();
        let root = std::env::temp_dir().join(format!(
            "ccd-lan-start-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let (claude, _) = crate::start_session::test_claude(&root);
        let (server, _) = live_server_on(
            protocol::config::Config {
                claude_bin: Some(claude.display().to_string()),
                codex_bin: Some(
                    crate::start_session::test_codex(&claude)
                        .display()
                        .to_string(),
                ),
                ..Default::default()
            },
            Arc::new(crate::apns::LoggingPushSender::new()),
            PlaintextTrust::OperatorAllowed,
        )
        .await;
        let list = Message::Text(r#"{"type":"list_projects"}"#.into());

        let (mut bootstrap, ack) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(ack["capabilities"]["start_session"], false, "{ack}");
        bootstrap.send(list.clone()).await.unwrap();
        let refused = next_json(&mut bootstrap).await.unwrap();
        assert_eq!(refused["code"], "not_authorised", "{refused}");

        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["capabilities"]["start_session"], true, "{ack}");
        assert_eq!(ack["capabilities"]["browse_folders"], true, "{ack}");
        assert_eq!(ack["capabilities"]["private_transport"], false, "{ack}");
        phone.send(list).await.unwrap();
        let reply = match next_frame(&mut phone, Duration::from_secs(30)).await {
            Frame::Json(reply) => reply,
            _ => panic!("no projects reply within 30 s"),
        };
        assert_eq!(reply["type"], "projects", "{reply}");
        assert_eq!(reply["agents"][0]["agent"], "claude", "{reply}");
        assert_eq!(
            reply["agents"][0]["available"], true,
            "the stub claude was asked: {reply}"
        );
        assert_eq!(reply["agents"][0]["signed_in"], true, "{reply}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// **A run's shell is offered, and served, only to a paired device on a private transport**: never to the
    /// bootstrap token, never over the plaintext LAN the operator allowed, and an open the gate lets through still
    /// names a run the daemon has, or is closed `not_live`.
    #[tokio::test]
    async fn a_runs_shell_is_only_for_a_paired_device_on_a_private_transport() {
        let open = Message::Text(
            r#"{"type":"terminal_open","attachment_id":"t1","session_id":"01NOSUCHRUN","cols":80,"rows":24,"output_credit":65536,"takeover":true}"#.into(),
        );
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let (mut bootstrap, ack) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(ack["capabilities"]["run_terminal"], false, "{ack}");
        bootstrap.send(open.clone()).await.unwrap();
        let refused = next_json(&mut bootstrap).await.unwrap();
        assert_eq!(refused["type"], "terminal_closed", "{refused}");
        assert_eq!(refused["reason"], "not_authorised", "{refused}");
        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["capabilities"]["run_terminal"], true, "{ack}");
        phone.send(open.clone()).await.unwrap();
        let unknown = next_json(&mut phone).await.unwrap();
        assert_eq!(unknown["type"], "terminal_closed", "{unknown}");
        assert_eq!(unknown["reason"], "not_live", "{unknown}");

        let (lan, _) = live_server_on(
            protocol::config::Config::default(),
            Arc::new(crate::apns::LoggingPushSender::new()),
            PlaintextTrust::OperatorAllowed,
        )
        .await;
        let (mut phone, ack) = lan
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["capabilities"]["run_terminal"], false, "{ack}");
        phone.send(open).await.unwrap();
        let refused = next_json(&mut phone).await.unwrap();
        assert_eq!(refused["reason"], "not_authorised", "{refused}");
    }

    /// A stub claude for a list test, signed out, and a live server whose check asks
    /// it; the folder to remove after.
    async fn list_rig(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, LiveServer) {
        let root = std::env::temp_dir().join(format!(
            "ccd-{tag}-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let (claude, _) = crate::start_session::test_claude(&root);
        let (server, _) = live_server(protocol::config::Config {
            claude_bin: Some(claude.display().to_string()),
            codex_bin: Some(
                crate::start_session::test_codex(&claude)
                    .display()
                    .to_string(),
            ),
            ..Default::default()
        })
        .await;
        let rig = (root, claude, server);
        say_signed_in(&rig.1, false);
        rig
    }

    /// What the stub claude's `auth status` says: signed in, or signed out as
    /// 2.1.268 says it (exit 1).
    fn say_signed_in(claude: &std::path::Path, signed_in: bool) {
        let file = |what: &str| format!("{}.{what}", claude.display());
        std::fs::write(
            file("auth"),
            format!("{{\n  \"loggedIn\": {signed_in}\n}}\n"),
        )
        .unwrap();
        std::fs::write(file("auth_exit"), if signed_in { "0" } else { "1" }).unwrap();
    }

    /// What a `projects` frame says of Claude Code's sign-in.
    fn claude_signed_in(frame: &ServerMessage) -> Option<bool> {
        match frame {
            ServerMessage::Projects { agents, .. } => agents
                .iter()
                .find(|agent| agent.agent == protocol::agent::AgentKind::Claude)
                .and_then(|agent| agent.signed_in),
            other => panic!("not a projects frame: {other:?}"),
        }
    }

    /// **A list answers again when the check it started finds the agents changed**
    /// (B17). The first list waits for its check and is answered once. After the
    /// stub claude signs in, the next list is answered from the last check (signed
    /// out) at once, while its own check is still held, and then with what that
    /// check found, the same folder in both. With nothing changed, a list is
    /// answered once.
    #[tokio::test]
    async fn a_list_answers_again_when_its_check_finds_a_change() {
        let _env = crate::ProcessEnvRead::take();
        let (root, claude, server) = list_rig("list-again").await;
        let app = root.join("app");
        std::fs::create_dir_all(&app).unwrap();
        register_stub_session(&server.store, app.display().to_string());
        let folders = |frame: &ServerMessage| match frame {
            ServerMessage::Projects { projects, .. } => projects
                .iter()
                .map(|project| project.path.clone())
                .collect::<Vec<_>>(),
            other => panic!("not a projects frame: {other:?}"),
        };
        let (replies, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let drain = |sent: &mut tokio::sync::mpsc::UnboundedReceiver<ServerMessage>| {
            let mut said = Vec::new();
            while let Ok(frame) = sent.try_recv() {
                assert_eq!(
                    folders(&frame),
                    [app.display().to_string()],
                    "every frame's folder"
                );
                said.push(claude_signed_in(&frame));
            }
            said
        };
        crate::start_session::list_projects(&server.daemon, &replies).await;
        assert_eq!(
            drain(&mut sent),
            [Some(false)],
            "the first list waits for its check"
        );

        say_signed_in(&claude, true);
        let hold = format!("{}.auth_hold", claude.display());
        std::fs::write(&hold, "").unwrap();
        let listing = tokio::spawn({
            let (daemon, replies) = (Arc::clone(&server.daemon), replies.clone());
            async move { crate::start_session::list_projects(&daemon, &replies).await }
        });
        let answer = tokio::time::timeout(Duration::from_secs(10), sent.recv())
            .await
            .expect("answered while its check is held")
            .unwrap();
        assert_eq!(
            claude_signed_in(&answer),
            Some(false),
            "the last check's answer"
        );
        assert_eq!(folders(&answer), [app.display().to_string()]);
        assert!(
            sent.try_recv().is_err(),
            "nothing more while the check is held"
        );
        std::fs::remove_file(&hold).unwrap();
        listing.await.unwrap();
        assert_eq!(drain(&mut sent), [Some(true)], "then what the check found");

        crate::start_session::list_projects(&server.daemon, &replies).await;
        assert_eq!(
            drain(&mut sent),
            [Some(true)],
            "nothing changed: one answer"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// **Over the wire, the second answer reaches the device that listed**: it
    /// lists (signed out), the stub claude signs in, and the next list gets the
    /// last check's answer and then the new one.
    #[tokio::test]
    async fn the_device_that_listed_hears_what_its_check_found() {
        let _env = crate::ProcessEnvRead::take();
        let (root, claude, server) = list_rig("list-again-wire").await;
        let (mut phone, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        async fn next_projects(
            socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        ) -> Option<bool> {
            loop {
                match next_frame(socket, Duration::from_secs(30)).await {
                    Frame::Json(frame) if frame["type"] == "projects" => {
                        let frame: ServerMessage = serde_json::from_value(frame).unwrap();
                        return claude_signed_in(&frame);
                    }
                    Frame::Json(_) => continue,
                    Frame::Closed | Frame::Silent => panic!("no projects frame within 30 s"),
                }
            }
        }
        let list = || Message::Text(r#"{"type":"list_projects"}"#.into());
        phone.send(list()).await.unwrap();
        assert_eq!(next_projects(&mut phone).await, Some(false));
        say_signed_in(&claude, true);
        phone.send(list()).await.unwrap();
        assert_eq!(
            next_projects(&mut phone).await,
            Some(false),
            "the last check's answer"
        );
        assert_eq!(
            next_projects(&mut phone).await,
            Some(true),
            "then what its check found"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// One list from a paired device on a fresh live server whose check asks a stub
    /// claude, and the lines [`crate::start_session::claude_env_log`] gained meanwhile:
    /// what the stub's runs saw of `HOME`, `PATH` and `XPC_SERVICE_NAME`.
    async fn canary_of_one_list(tag: &str) -> Vec<String> {
        let log = crate::start_session::claude_env_log();
        let from = std::fs::metadata(&log).map_or(0, |meta| meta.len() as usize);
        let root = std::env::temp_dir().join(format!(
            "ccd-{tag}-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let (claude, _) = crate::start_session::test_claude(&root);
        let (server, _) = live_server(protocol::config::Config {
            claude_bin: Some(claude.display().to_string()),
            codex_bin: Some(
                crate::start_session::test_codex(&claude)
                    .display()
                    .to_string(),
            ),
            ..Default::default()
        })
        .await;
        let (mut phone, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        phone
            .send(Message::Text(r#"{"type":"list_projects"}"#.into()))
            .await
            .unwrap();
        match next_frame(&mut phone, Duration::from_secs(30)).await {
            Frame::Json(reply) => assert_eq!(reply["type"], "projects", "{reply}"),
            _ => panic!("no projects reply within 30 s"),
        }
        std::fs::remove_dir_all(&root).unwrap();
        let said = std::fs::read_to_string(&log).unwrap_or_default();
        said.get(from..)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// **A list never runs in a `HOME` another test set**: the check that fails when
    /// a writer no longer takes the process-environment lock. A thread installs a fake
    /// `HOME`, says so, and lets it go by itself 3 s later; only then does this test
    /// list. With the lock, the list waits for the writer, so the stub claude never
    /// sees the fake `HOME`. Without it, the probe runs in it. The writer never waits
    /// for the reader, so this cannot deadlock. Ignored: it holds a writer for 3 s, so
    /// it is run alone.
    #[tokio::test]
    #[ignore = "holds the process-environment lock's writer for 3 s: run alone"]
    async fn a_list_never_runs_in_a_home_another_test_set() {
        let (placed, in_place) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let home = crate::legacy_credentials::test_support::FakeHome::new("mf1");
            placed.send(()).unwrap();
            std::thread::sleep(Duration::from_secs(3));
            drop(home);
        });
        in_place.recv().unwrap();
        let _env = crate::ProcessEnvRead::take();
        let lines = canary_of_one_list("mf1").await;
        writer.join().unwrap();
        assert!(!lines.is_empty(), "the stub claude was run");
        assert!(
            lines.iter().all(|line| !line.contains("/ccd-home-")),
            "a probe ran in the fake HOME: {lines:?}"
        );
    }

    /// **A list never takes the service branch for a label another test set**: the
    /// same shape as `a_list_never_runs_in_a_home_another_test_set`, with
    /// [`crate::LaunchdLabelEnv`] setting this build's own launchd label, which sends
    /// `phone_environment` to launchd's session environment (`XPC_SERVICE_NAME=0`).
    /// Run where the process itself has none of 0, as under `env -i`. Ignored: it
    /// holds a writer for 3 s, so it is run alone.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "holds the process-environment lock's writer for 3 s: run alone"]
    async fn a_list_never_takes_the_service_branch_for_a_label_another_test_set() {
        let (placed, in_place) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let label = crate::LaunchdLabelEnv::take();
            label.set(Some(protocol::launchd_label()));
            placed.send(()).unwrap();
            std::thread::sleep(Duration::from_secs(3));
            drop(label);
        });
        in_place.recv().unwrap();
        let _env = crate::ProcessEnvRead::take();
        assert_ne!(
            std::env::var_os("XPC_SERVICE_NAME").as_deref(),
            Some(std::ffi::OsStr::new("0")),
            "this process's own XPC_SERVICE_NAME is 0: run it under env -i"
        );
        let lines = canary_of_one_list("mf2").await;
        writer.join().unwrap();
        assert!(!lines.is_empty(), "the stub claude was run");
        assert!(
            lines
                .iter()
                .all(|line| !line.ends_with("XPC_SERVICE_NAME=0")),
            "a probe took the service branch: {lines:?}"
        );
    }

    /// A run may start without a first message: a start with none, an empty one or one of
    /// only white space goes on to the ordinary checks, as a start with one does.
    #[tokio::test]
    async fn a_start_without_a_first_message_goes_on_to_the_ordinary_checks() {
        let _env = crate::ProcessEnvRead::take();
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let (mut phone, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        let start = |request_id: &str, prompt: Option<&str>| {
            Message::Text(
                serde_json::to_string(&ClientMessage::StartSession {
                    request_id: request_id.into(),
                    path: "/nonexistent-codeconnect-start-folder".into(),
                    agent: protocol::agent::AgentKind::Claude,
                    prompt: prompt.map(str::to_string),
                    worktree: None,
                    model: None,
                    trust: None,
                    attachments: Vec::new(),
                })
                .unwrap(),
            )
        };
        let result = |reply: serde_json::Value| reply["result"].clone();
        for (id, prompt) in [
            ("r-none", None),
            ("r-empty", Some("")),
            ("r-blank", Some(" \n\t")),
            ("r-said", Some("fix the tests")),
        ] {
            phone.send(start(id, prompt)).await.unwrap();
            let reply = loop {
                let message = next_json(&mut phone).await.expect("a start_session_result");
                if message["type"] == "start_session_result" {
                    break message;
                }
            };
            assert_eq!(reply["request_id"], id, "{reply}");
            assert_eq!(result(reply.clone())["reason"], "folder_missing", "{reply}");
        }
    }

    /// The folders' answers to "trust this folder?" are listed, changed and
    /// forgotten by a paired device, each request answered with the whole list.
    /// The bootstrap token is refused all three.
    #[tokio::test]
    async fn folder_answers_are_listed_changed_and_forgotten_by_a_paired_device() {
        use protocol::host::Trust;
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let frame =
            |message: ClientMessage| Message::Text(serde_json::to_string(&message).unwrap());
        let set = |request_id: &str, path: &str, trust: Trust| {
            frame(ClientMessage::SetFolderTrust {
                request_id: request_id.into(),
                path: path.into(),
                trust,
            })
        };
        let forget = |request_id: &str, path: &str| {
            frame(ClientMessage::ForgetFolderTrust {
                request_id: request_id.into(),
                path: path.into(),
            })
        };
        let list = |request_id: &str| {
            frame(ClientMessage::ListFolderTrust {
                request_id: request_id.into(),
            })
        };

        let (mut bootstrap, _) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        for refused in [
            list("b1"),
            set("b2", "/srv/a", Trust::Trusted),
            forget("b3", "/srv/a"),
        ] {
            bootstrap.send(refused).await.unwrap();
            let reply = next_json(&mut bootstrap).await.unwrap();
            assert_eq!(reply["code"], "not_authorised", "{reply}");
        }

        let (mut phone, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        /// Sends `message` and returns its `folder_trust` reply's folders as
        /// sorted `[path, trust]` pairs.
        async fn ask(
            phone: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
            message: Message,
            request_id: &str,
        ) -> serde_json::Value {
            phone.send(message).await.unwrap();
            let reply = loop {
                let reply = next_json(phone).await.expect("a folder_trust reply");
                if reply["type"] == "folder_trust" {
                    break reply;
                }
            };
            assert_eq!(reply["request_id"], request_id, "{reply}");
            let mut folders: Vec<(String, String)> = reply["folders"]
                .as_array()
                .unwrap()
                .iter()
                .map(|folder| {
                    (
                        folder["path"].as_str().unwrap().to_string(),
                        folder["trust"].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            folders.sort();
            serde_json::json!(folders)
        }
        assert_eq!(
            ask(&mut phone, list("t1"), "t1").await,
            serde_json::json!([])
        );
        assert_eq!(
            ask(&mut phone, set("t2", "/srv/a", Trust::Trusted), "t2").await,
            serde_json::json!([["/srv/a", "trusted"]])
        );
        assert_eq!(
            ask(&mut phone, set("t3", "/srv/b", Trust::NotTrusted), "t3").await,
            serde_json::json!([["/srv/a", "trusted"], ["/srv/b", "not_trusted"]])
        );
        assert_eq!(
            ask(&mut phone, set("t4", "/srv/a", Trust::NotTrusted), "t4").await,
            serde_json::json!([["/srv/a", "not_trusted"], ["/srv/b", "not_trusted"]])
        );
        assert_eq!(
            ask(&mut phone, forget("t5", "/srv/a"), "t5").await,
            serde_json::json!([["/srv/b", "not_trusted"]])
        );
        assert_eq!(
            ask(&mut phone, list("t6"), "t6").await,
            serde_json::json!([["/srv/b", "not_trusted"]])
        );
    }

    /// **Every accounts call answers at once, and what the agents say follows by push** (0e's rule, 2026-11-23): an
    /// add answers within a bound while its agent takes 3 s to say whether it is signed in, which then arrives pushed;
    /// a sign-in answers at once and, at the login's end, its sign-in and its own facts are pushed with no list; and a
    /// list right after a fresh read answers from what is held and starts no agent.
    #[tokio::test]
    async fn an_accounts_call_answers_at_once_and_what_its_agent_says_follows_by_push() {
        let _env = crate::ProcessEnvRead::take();
        use protocol::agent::AgentKind;
        let agents =
            std::env::temp_dir().join(format!("ccw-push.{}", &protocol::uid::new().unwrap()[20..]));
        std::fs::create_dir_all(&agents).unwrap();
        let codex = crate::accounts::tests::stand_in_codex(&agents);
        std::fs::write(agents.join("slowstatus"), "").unwrap();
        let claude = agents.join("signed-out");
        std::fs::write(&claude, "#!/bin/sh\nexit 1\n").unwrap();
        std::process::Command::new("chmod")
            .arg("755")
            .arg(&claude)
            .status()
            .unwrap();
        let (server, _) = live_server(protocol::config::Config {
            codex_bin: Some(codex.display().to_string()),
            claude_bin: Some(claude.display().to_string()),
            ..protocol::config::Config::default()
        })
        .await;
        let (mut phone, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        let send = |message: ClientMessage| Message::Text(serde_json::to_string(&message).unwrap());
        // The next frame of `type` naming `request` (a pushed listing names none) that `wanted` accepts, within 20 s.
        async fn until(
            phone: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
            request: Option<&str>,
            wanted: impl Fn(&serde_json::Value) -> bool,
        ) -> serde_json::Value {
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_secs(20) {
                if let Frame::Json(frame) = next_frame(phone, Duration::from_secs(1)).await {
                    if frame["type"] == "accounts"
                        && frame["request_id"].as_str() == request
                        && wanted(&frame)
                    {
                        return frame;
                    }
                }
            }
            panic!("no such frame within 20 s");
        }
        let runs = || std::fs::read_to_string(agents.join("runs")).unwrap_or_default();

        // Added: answered at once, though its agent takes 3 s to say whether it is signed in; that follows by push.
        let started = std::time::Instant::now();
        phone
            .send(send(ClientMessage::AddAccount {
                request_id: "p1".into(),
                agent: AgentKind::Codex,
                label: "B".into(),
            }))
            .await
            .unwrap();
        let added = until(&mut phone, Some("p1"), |_| true).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "answered in {:?}",
            started.elapsed()
        );
        assert_eq!(
            added["accounts"][0].get("signed_in"),
            None,
            "not read yet: {added}"
        );
        let b = added["accounts"][0]["id"].as_str().unwrap().to_string();
        until(&mut phone, None, |frame| {
            frame["accounts"][0]["signed_in"] == false
        })
        .await;

        // Signed in: answered at once; at the login's end its sign-in and its own facts are pushed, with no list.
        phone
            .send(send(ClientMessage::SignInAccount {
                request_id: "p2".into(),
                account_id: b.clone(),
            }))
            .await
            .unwrap();
        let signing = until(&mut phone, Some("p2"), |_| true).await;
        assert_eq!(signing["accounts"][0]["signing_in"], true, "{signing}");
        let read = until(&mut phone, None, |frame| {
            frame["accounts"][0]["signed_in"] == true
                && frame["accounts"][0]["facts"]["email"].is_string()
        })
        .await;
        assert_eq!(
            read["accounts"][0]["facts"]["email"],
            format!("{b}@example.test"),
            "{read}"
        );

        // Listed right after: answered from what is held, and no agent started.
        let before = runs();
        phone
            .send(send(ClientMessage::ListAccounts {
                request_id: "p3".into(),
            }))
            .await
            .unwrap();
        let listed = until(&mut phone, Some("p3"), |_| true).await;
        assert_eq!(listed["accounts"][0]["signed_in"], true, "{listed}");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            runs(),
            before,
            "a list right after a fresh read started an agent"
        );

        // Removed: answered once its own logout is done, not after the other accounts' sign-ins are asked again.
        phone
            .send(send(ClientMessage::AddAccount {
                request_id: "p4".into(),
                agent: AgentKind::Codex,
                label: "X".into(),
            }))
            .await
            .unwrap();
        let added = until(&mut phone, Some("p4"), |_| true).await;
        let x = added["accounts"][1]["id"].as_str().unwrap().to_string();
        let started = std::time::Instant::now();
        phone
            .send(send(ClientMessage::RemoveAccount {
                request_id: "p5".into(),
                account_id: x,
            }))
            .await
            .unwrap();
        let removed = until(&mut phone, Some("p5"), |_| true).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "removed in {:?}",
            started.elapsed()
        );
        assert_eq!(
            removed["accounts"].as_array().unwrap().len(),
            1,
            "{removed}"
        );
        let _ = std::fs::remove_dir_all(&agents);
    }

    /// **The frames the accounts page reads, in their real order** (recorded for the apps): each add's answer with its
    /// sign-in and facts not read yet, each sign-in's answer, then the pushes that fill them in: a Codex account with
    /// one usage window, a Claude Code account whose sign-in has no plan limits, and one with two windows.
    #[tokio::test]
    async fn the_accounts_pages_frames_arrive_answer_then_push() {
        let _env = crate::ProcessEnvRead::take();
        use protocol::agent::AgentKind;
        let agents =
            std::env::temp_dir().join(format!("ccw-page.{}", &protocol::uid::new().unwrap()[20..]));
        std::fs::create_dir_all(&agents).unwrap();
        let codex = crate::accounts::tests::stand_in_codex(&agents);
        let claude = crate::accounts::tests::stand_in_claude(&agents);
        // The host's usage launch: no plan limits for an account named in `nolimits-<id>`, else both windows.
        let host = agents.join("cc-agent-host");
        std::fs::write(
            &host,
            "#!/bin/sh\nif [ -e \"$(dirname \"$0\")/nolimits-$(basename \"$CLAUDE_CONFIG_DIR\")\" ]; then \
             echo '{\"available\":false,\"plan\":null,\"windows\":[]}'; else \
             echo '{\"available\":true,\"plan\":\"max\",\"windows\":[{\"name\":\"five_hour\",\"utilization\":12,\"resets_at\":1795900000},{\"name\":\"seven_day\",\"utilization\":42,\"resets_at\":1796400000}]}'; fi\n",
        )
        .unwrap();
        std::process::Command::new("chmod")
            .arg("755")
            .arg(&host)
            .status()
            .unwrap();
        let (server, _) = live_server(protocol::config::Config {
            codex_bin: Some(codex.display().to_string()),
            claude_bin: Some(claude.display().to_string()),
            agent_host_bin: Some(host.display().to_string()),
            ..protocol::config::Config::default()
        })
        .await;
        let (mut phone, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        let send = |message: ClientMessage| Message::Text(serde_json::to_string(&message).unwrap());
        let mut frames: Vec<serde_json::Value> = Vec::new();
        // Every accounts frame in the order it arrives, until one `done` accepts, within 30 s.
        async fn read_until(
            phone: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
            frames: &mut Vec<serde_json::Value>,
            done: impl Fn(&serde_json::Value) -> bool,
        ) -> serde_json::Value {
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_secs(30) {
                if let Frame::Json(frame) = next_frame(phone, Duration::from_secs(1)).await {
                    if frame["type"] == "accounts" {
                        frames.push(frame.clone());
                        if done(&frame) {
                            return frame;
                        }
                    }
                }
            }
            panic!("not within 30 s: {frames:#?}");
        }
        let mut ids = Vec::new();
        for (n, (agent, label)) in [
            (AgentKind::Codex, "B"),
            (AgentKind::Claude, "C"),
            (AgentKind::Claude, "D"),
        ]
        .into_iter()
        .enumerate()
        {
            let request = format!("add-{n}");
            phone
                .send(send(ClientMessage::AddAccount {
                    request_id: request.clone(),
                    agent,
                    label: label.into(),
                }))
                .await
                .unwrap();
            let added = read_until(&mut phone, &mut frames, |frame| {
                frame["request_id"] == request.as_str()
            })
            .await;
            let row = added["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["label"] == label)
                .unwrap()
                .clone();
            assert_eq!(
                (row.get("signed_in"), row.get("facts")),
                (None, None),
                "not read yet: {row}"
            );
            ids.push(row["id"].as_str().unwrap().to_string());
        }
        std::fs::write(agents.join(format!("nolimits-{}", ids[1])), "").unwrap();
        let row = |frame: &serde_json::Value, id: &str| {
            frame["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == id)
                .cloned()
                .unwrap_or_default()
        };
        for (n, id) in ids.iter().enumerate() {
            let request = format!("sign-in-{n}");
            phone
                .send(send(ClientMessage::SignInAccount {
                    request_id: request.clone(),
                    account_id: id.clone(),
                }))
                .await
                .unwrap();
            read_until(&mut phone, &mut frames, |frame| {
                frame["request_id"] == request.as_str()
            })
            .await;
            // One sign-in at a time: a newer one replaces the one going, so each is let end first.
            read_until(&mut phone, &mut frames, |frame| {
                frame.get("request_id").is_none() && row(frame, id)["signed_in"] == true
            })
            .await;
        }
        let whole = |frame: &serde_json::Value| {
            frame.get("request_id").is_none()
                && ids.iter().all(|id| {
                    row(frame, id)["signed_in"] == true
                        && row(frame, id)["facts"]["email"].is_string()
                })
                && row(frame, &ids[2])["facts"]["windows"]
                    .as_array()
                    .is_some_and(|windows| windows.len() == 2)
                && row(frame, &ids[1])["facts"]["limits_available"] == false
        };
        // The last sign-in's push may already be the whole one.
        let filled = match frames.last().filter(|frame| whole(frame)) {
            Some(frame) => frame.clone(),
            None => read_until(&mut phone, &mut frames, whole).await,
        };
        assert_eq!(
            row(&filled, &ids[0])["facts"]["windows"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{filled}"
        );
        assert_eq!(
            row(&filled, &ids[1])["facts"].get("windows"),
            None,
            "no plan limits: no window: {filled}"
        );
        assert_eq!(
            row(&filled, &ids[2])["facts"]["limits_available"],
            true,
            "{filled}"
        );
        crate::switch::window::keep(&frames, "accounts-answer-then-push-wire.json");
        let _ = std::fs::remove_dir_all(&agents);
    }

    /// **A file sent from the start box is held pending, for a paired device only, until its
    /// connection closes** (minor 50): a `put_file` naming no session lands under `.pending`,
    /// the bootstrap token is refused, and closing the phone's connection removes what no start
    /// took.
    #[tokio::test]
    async fn a_start_box_file_is_held_pending_until_its_connection_closes() {
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let chunk = |id: &str| {
            Message::Text(
                serde_json::json!({"type": "put_file", "upload_id": id, "name": "a.txt",
                                   "size": 1, "offset": 0, "data": "YQ=="})
                .to_string(),
            )
        };
        let (mut bootstrap, ack) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(ack["capabilities"]["uploads"], false, "{ack}");
        bootstrap.send(chunk("pend-b")).await.unwrap();
        let reply = next_json(&mut bootstrap).await.unwrap();
        assert_eq!(reply["result"]["status"], "refused", "{reply}");

        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["capabilities"]["uploads"], true, "{ack}");
        phone.send(chunk("pend-p")).await.unwrap();
        let reply = next_json(&mut phone).await.unwrap();
        assert_eq!(
            reply["result"],
            serde_json::json!({"status": "received", "received": 1}),
            "{reply}"
        );
        let file = crate::uploads::root()
            .join(crate::uploads::PENDING)
            .join("pend-p")
            .join("a.txt");
        assert_eq!(std::fs::read(&file).unwrap(), b"a");
        drop(phone);
        let mut gone = false;
        for _ in 0..100 {
            if !file.exists() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "the closed connection's pending file was removed");
    }

    /// **A run's repository is read for a paired device only** (minor 58): advertised to it
    /// and not to the bootstrap token, which is refused; the paired device gets the run's
    /// branch, and a run outside a repository answers with a note.
    #[tokio::test]
    async fn a_runs_repository_is_read_for_a_paired_device_only() {
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let repo = std::env::temp_dir().join(format!(
            "ccd-rungit-ws-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&repo).unwrap();
        let git = crate::git::git_bin(None).expect("git must be installed");
        let status = std::process::Command::new(git)
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q", "-b", "trunk"])
            .status()
            .unwrap();
        assert!(status.success());
        register_stub_session(&server.store, repo.to_str().unwrap().into());
        let ask = Message::Text(
            serde_json::json!({"type": "get_run_git", "session_id": "cc-9"}).to_string(),
        );

        let (mut bootstrap, ack) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(ack["capabilities"]["changes"], false, "{ack}");
        bootstrap.send(ask.clone()).await.unwrap();
        let reply = next_json(&mut bootstrap).await.unwrap();
        assert_eq!(reply["type"], "error", "{reply}");
        assert_eq!(reply["code"], "not_authorised", "{reply}");

        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["capabilities"]["changes"], true, "{ack}");
        phone.send(ask.clone()).await.unwrap();
        let reply = next_json(&mut phone).await.unwrap();
        assert_eq!(reply["type"], "run_git", "{reply}");
        assert_eq!(reply["session_id"], "cc-9", "{reply}");
        assert_eq!(reply["git"]["branch"], "trunk", "{reply}");

        std::fs::remove_dir_all(repo.join(".git")).unwrap();
        phone.send(ask).await.unwrap();
        let reply = next_json(&mut phone).await.unwrap();
        assert_eq!(
            reply,
            serde_json::json!({"type": "run_git", "session_id": "cc-9", "note": "not a git repository"})
        );

        phone
            .send(Message::Text(
                serde_json::json!({"type": "get_run_git", "session_id": "no-such-run"}).to_string(),
            ))
            .await
            .unwrap();
        let reply = next_json(&mut phone).await.unwrap();
        assert_eq!(
            reply["type"], "run_git",
            "an unknown run is answered by name: {reply}"
        );
        assert_eq!(reply["session_id"], "no-such-run", "{reply}");
        assert!(reply["note"].is_string(), "{reply}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// **A slow repository read holds nothing else on the connection**: with git taking two
    /// seconds, a ping sent after `get_run_git` is answered first.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_slow_repository_read_does_not_hold_the_connection() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "ccd-rungit-slow-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let real = crate::git::git_bin(None).expect("git must be installed");
        let slow = dir.join("slow-git");
        std::fs::write(
            &slow,
            format!("#!/bin/sh\nsleep 2\nexec '{}' \"$@\"\n", real.display()),
        )
        .unwrap();
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = protocol::config::Config {
            git_bin: Some(slow.to_str().unwrap().into()),
            ..protocol::config::Config::default()
        };
        let (server, _) = live_server(config).await;
        register_stub_session(&server.store, dir.to_str().unwrap().into());
        let (mut phone, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        phone
            .send(Message::Text(
                serde_json::json!({"type": "get_run_git", "session_id": "cc-9"}).to_string(),
            ))
            .await
            .unwrap();
        phone
            .send(Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .unwrap();
        let first = next_json_within(&mut phone, Duration::from_secs(1))
            .await
            .expect("the ping is answered while git is still reading");
        assert_eq!(first["type"], "pong", "{first}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A commit is made for a paired device only** (minor 59): `git_actions` names `commit`
    /// to it and nothing to the bootstrap token, which is refused; the paired device's
    /// commit is answered with its sha.
    #[tokio::test]
    async fn a_commit_is_made_for_a_paired_device_only() {
        let _env = crate::ProcessEnvRead::take();
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let repo = std::env::temp_dir().join(format!(
            "ccd-commit-ws-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&repo).unwrap();
        let git = crate::git::git_bin(None).expect("git must be installed");
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "t"],
            &["config", "user.email", "t@example.com"],
            &["commit", "-q", "--allow-empty", "-m", "first"],
        ] {
            let status = std::process::Command::new(&git)
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        register_stub_session(&server.store, repo.to_str().unwrap().into());
        let ask = Message::Text(
            serde_json::json!({"type": "git_action", "request_id": "c-1",
                               "session_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
                               "action": {"kind": "commit", "message": "Add a"}})
            .to_string(),
        );

        let (mut bootstrap, ack) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert!(ack["capabilities"].get("git_actions").is_none(), "{ack}");
        bootstrap.send(ask.clone()).await.unwrap();
        let reply = next_json(&mut bootstrap).await.unwrap();
        assert_eq!(reply["code"], "not_authorised", "{reply}");

        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(
            ack["capabilities"]["git_actions"],
            serde_json::json!(["commit"]),
            "{ack}"
        );
        phone.send(ask).await.unwrap();
        let reply = next_json(&mut phone).await.unwrap();
        let head = std::process::Command::new(&git)
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            reply,
            serde_json::json!({"type": "git_action_result", "request_id": "c-1",
                               "session_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
                               "result": {"status": "done",
                                          "sha": String::from_utf8(head.stdout).unwrap().trim()}})
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// **A diff waits for a commit off the loop**: with a pre-commit hook taking two seconds,
    /// a `get_diff` sent during the commit holds nothing else on the connection.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_diff_behind_a_slow_commit_does_not_hold_the_connection() {
        let _env = crate::ProcessEnvRead::take();
        use std::os::unix::fs::PermissionsExt;
        let (server, _) = live_server(protocol::config::Config::default()).await;
        let repo = std::env::temp_dir().join(format!(
            "ccd-commit-diff-ws-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&repo).unwrap();
        let git = crate::git::git_bin(None).expect("git must be installed");
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "t"],
            &["config", "user.email", "t@example.com"],
            &["commit", "-q", "--allow-empty", "-m", "first"],
        ] {
            let status = std::process::Command::new(&git)
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let hook = repo.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nsleep 2\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        register_stub_session(&server.store, repo.to_str().unwrap().into());
        let (mut phone, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        for frame in [
            serde_json::json!({"type": "git_action", "request_id": "c-slow",
                               "session_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
                               "action": {"kind": "commit", "message": "slow"}}),
            serde_json::json!({"type": "get_diff", "session_id": "cc-9"}),
            serde_json::json!({"type": "ping"}),
        ] {
            phone.send(Message::Text(frame.to_string())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let first = next_json_within(&mut phone, Duration::from_secs(1))
            .await
            .expect("the ping is answered while the commit's hook runs");
        assert_eq!(first["type"], "pong", "{first}");

        phone
            .send(Message::Text(
                serde_json::json!({"type": "get_diff", "session_id": "no-such-run"}).to_string(),
            ))
            .await
            .unwrap();
        let mut named = None;
        for _ in 0..4 {
            let reply = next_json_within(&mut phone, Duration::from_secs(5))
                .await
                .unwrap();
            if reply["session_id"] == "no-such-run" {
                named = Some(reply);
                break;
            }
        }
        let named = named.expect("an unknown run's diff is answered by name");
        assert_eq!(named["type"], "diff", "{named}");
        assert!(named["note"].is_string(), "{named}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// **Accounts are kept and chosen by a paired device only** (minor 48): advertised to it and not to the bootstrap
    /// token, which is refused all three asks. An account is added for its agent, made active, and set back to
    /// Default; another agent's id is refused and nothing changes.
    #[tokio::test]
    async fn accounts_are_kept_and_chosen_by_a_paired_device() {
        // Every listing asks the agent, from the session's environment: here a stand-in for each, signed out.
        let _env = crate::ProcessEnvRead::take();
        use protocol::agent::AgentKind;
        let agents = std::env::temp_dir().join(format!(
            "ccw-agents.{}",
            &protocol::uid::new().unwrap()[20..]
        ));
        std::fs::create_dir_all(&agents).unwrap();
        let stand_in = agents.join("signed-out");
        std::fs::write(&stand_in, "#!/bin/sh\nexit 1\n").unwrap();
        std::process::Command::new("chmod")
            .arg("755")
            .arg(&stand_in)
            .status()
            .unwrap();
        let (server, _) = live_server(protocol::config::Config {
            codex_bin: Some(stand_in.display().to_string()),
            claude_bin: Some(stand_in.display().to_string()),
            ..protocol::config::Config::default()
        })
        .await;
        let frame =
            |message: ClientMessage| Message::Text(serde_json::to_string(&message).unwrap());
        let list = |id: &str| {
            frame(ClientMessage::ListAccounts {
                request_id: id.into(),
            })
        };
        let add = |id: &str, agent: AgentKind, label: &str| {
            frame(ClientMessage::AddAccount {
                request_id: id.into(),
                agent,
                label: label.into(),
            })
        };
        let activate = |id: &str, agent: AgentKind, account: Option<&str>| {
            frame(ClientMessage::ActivateAccount {
                request_id: id.into(),
                agent,
                account_id: account.map(str::to_string),
            })
        };

        let (mut bootstrap, ack) = server.hello(protocol::PROTOCOL_VERSION, None).await;
        assert_eq!(ack["capabilities"]["accounts"], false, "{ack}");
        for refused in [
            list("b1"),
            add("b2", AgentKind::Codex, "Work"),
            activate("b3", AgentKind::Codex, None),
        ] {
            bootstrap.send(refused).await.unwrap();
            let reply = next_json(&mut bootstrap).await.unwrap();
            assert_eq!(reply["code"], "not_authorised", "{reply}");
        }

        let (mut phone, ack) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(ack["capabilities"]["accounts"], true, "{ack}");
        // Kept with a made-up computer name: the one this daemon reads is the computer's own.
        let mut kept = ack.clone();
        kept["machine_name"] = "Example Mac".into();
        crate::switch::window::keep(std::slice::from_ref(&kept), "minor-48-hello-wire.json");
        async fn ask(
            phone: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
            message: Message,
            request_id: &str,
        ) -> serde_json::Value {
            phone.send(message).await.unwrap();
            loop {
                let reply = next_json(phone).await.expect("a reply");
                // A pushed listing names no request; it is not the answer.
                if reply["type"] == "accounts" && reply.get("request_id").is_none() {
                    continue;
                }
                // Every answer to an accounts request names it, a refusal (`accounts_refused`) too.
                if reply["type"] == "accounts"
                    || reply["type"] == "accounts_refused"
                    || reply["type"] == "error"
                {
                    assert_eq!(reply["request_id"], request_id, "{reply}");
                    break reply;
                }
            }
        }
        // A change made by one paired device is pushed, naming no request, to each paired connection that has asked
        // about accounts itself, and to no other: one that never asked may be an app that can't read the frame.
        let (mut watcher, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        let (mut silent, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        ask(&mut watcher, list("w1"), "w1").await;
        let empty = ask(&mut phone, list("a1"), "a1").await;
        assert_eq!(empty["accounts"], serde_json::json!([]), "{empty}");
        assert_eq!(empty["active"], serde_json::json!([]), "{empty}");

        let added = ask(&mut phone, add("a2", AgentKind::Codex, "Work"), "a2").await;
        let pushed = loop {
            let frame =
                tokio::time::timeout(std::time::Duration::from_secs(10), next_json(&mut watcher))
                    .await
                    .expect("a push within 10 s")
                    .expect("a frame");
            if frame["type"] == "accounts" {
                break frame;
            }
        };
        assert_eq!(pushed.get("request_id"), None, "{pushed}");
        assert_eq!(pushed["accounts"][0]["label"], "Work", "{pushed}");
        while let Ok(frame) =
            tokio::time::timeout(std::time::Duration::from_secs(1), next_json(&mut silent)).await
        {
            let frame = frame.expect("a frame");
            assert_ne!(
                frame["type"], "accounts",
                "pushed to a connection that never asked: {frame}"
            );
        }
        let work = added["accounts"][0]["id"].as_str().unwrap().to_string();
        assert!(protocol::uid::is_well_formed(&work), "{added}");
        assert_eq!(added["accounts"][0]["agent"], "codex");
        assert_eq!(
            added["active"],
            serde_json::json!([]),
            "adding does not choose it: {added}"
        );

        let chosen = ask(
            &mut phone,
            activate("a3", AgentKind::Codex, Some(&work)),
            "a3",
        )
        .await;
        assert_eq!(
            chosen["active"],
            serde_json::json!([{"agent": "codex", "account_id": work}]),
            "{chosen}"
        );

        // Codex's account is not Claude's.
        let refused = ask(
            &mut phone,
            activate("a4", AgentKind::Claude, Some(&work)),
            "a4",
        )
        .await;
        assert_eq!(
            (refused["type"].as_str(), refused["code"].as_str()),
            (Some("accounts_refused"), Some("bad_request")),
            "{refused}"
        );
        let unchanged = ask(&mut phone, list("a5"), "a5").await;
        assert_eq!(unchanged["active"], chosen["active"], "{unchanged}");

        // One label per agent: the same label again is the account it made, not a twin; a name is 1 to 64 characters.
        let again = ask(&mut phone, add("a7", AgentKind::Codex, "  Work "), "a7").await;
        // The same account, its row as held now (its sign-in read since the add).
        assert_eq!(again["accounts"].as_array().unwrap().len(), 1, "{again}");
        assert_eq!(
            again["accounts"][0]["id"], added["accounts"][0]["id"],
            "{again}"
        );
        for label in ["   ", &"x".repeat(65)] {
            let refused = ask(&mut phone, add("a8", AgentKind::Codex, label), "a8").await;
            assert_eq!(
                (refused["type"].as_str(), refused["code"].as_str()),
                (Some("accounts_refused"), Some("bad_request")),
                "{refused}"
            );
        }
        // An agent with no accounts here is refused, Default included.
        let other = AgentKind::from_str_lossy("gemini");
        let refused = ask(&mut phone, activate("a9", other, None), "a9").await;
        assert_eq!(
            (refused["type"].as_str(), refused["code"].as_str()),
            (Some("accounts_refused"), Some("bad_request")),
            "{refused}"
        );

        let default = ask(&mut phone, activate("a6", AgentKind::Codex, None), "a6").await;
        assert_eq!(default["active"], serde_json::json!([]), "{default}");
        assert_eq!(
            default["accounts"].as_array().unwrap().len(),
            1,
            "Default keeps the account: {default}"
        );
    }
}
