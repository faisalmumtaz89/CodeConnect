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
use protocol::secret::Redacted;
use protocol::ws::{
    AnswerPath, Capabilities, ClientMessage, ServerMessage, MAX_CLIENT_MESSAGE_BYTES,
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
/// The longest a single frame may take to reach the peer before the connection
/// is judged dead. Without it a peer that stops reading its socket wedges the
/// very `send` that a revocation, a teardown, or a keepalive would ride, and
/// the connection can never be closed — the terminal makes this acute, since a
/// live shell must be revocable promptly. A healthy peer drains in
/// milliseconds; this bounds the pathological case to a fixed, generous window.
///
/// `pub(crate)` because the terminal's own tolerance for a peer holding a write
/// is derived from it rather than restated: this deadline sitting *below* the
/// terminal's stall deadline is what let a peer finish every write just inside it
/// and never be charged, so [`crate::terminal::PEER_DRAIN_WINDOW`] is a fraction
/// of this one and asserts as much at compile time. Two files, one number.
pub(crate) const WRITE_DEADLINE: Duration = Duration::from_secs(20);

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

/// The longest the *open* may take before the attach is abandoned.
///
/// This is how long the phone waits for its `terminal_attached`, and how long
/// the frames behind that attach sit in the opening's queue — no longer how long
/// the connection goes deaf, which it used to be and which is what made twenty
/// seconds expensive. [`crate::terminal::TerminalHandle::open`] bounds every
/// step it takes (three deadlined tmux probes and two teardown graces, some eight
/// seconds in all, plus [`crate::terminal::SUPERSEDE_WAIT`] when it is taking a
/// session's terminal over), but those are its bounds, not this loop's, and a
/// saturated blocking pool can queue in front of them.
///
/// Twenty seconds is chosen against those with better than twice the margin —
/// and chosen, not proven sufficient, because sufficiency is not knowable here:
/// how long the open really takes depends on how deep the blocking queue in
/// front of it is, and no constant can know that. What it buys is not "no attach
/// that would have succeeded is cut short", which this cannot promise. It buys
/// two things it can: an attach cut short is *reported* as having run out of
/// time, so the phone is told rather than left waiting and may simply ask again;
/// and a wedged open cannot hold a terminal id unanswered indefinitely.
const ATTACH_DEADLINE: Duration = Duration::from_secs(20);

/// The attach deadline in force, so a test can drive the give-up path without
/// waiting the production twenty seconds. Zero (the default) means "use the
/// constant".
#[cfg(test)]
static TEST_ATTACH_DEADLINE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn attach_deadline() -> Duration {
    #[cfg(test)]
    {
        let ms = TEST_ATTACH_DEADLINE_MS.load(std::sync::atomic::Ordering::Relaxed);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    ATTACH_DEADLINE
}

/// Whether plaintext (`ws://`) is admissible on a listener, and whether it is
/// *private*. Decided once at bind time from what the listener's address **is**
/// — never guessed per connection from where a peer claims to be.
///
/// The daemon's credential is a bearer token and a connection carries the
/// event log, the approval path, and the terminal, so the wire must be
/// unreadable in transit. That holds three ways: TLS, loopback (the bytes
/// never leave the machine), or this node's own Tailscale address (WireGuard
/// encrypts the path before it touches a network). Any other address —
/// an explicit `ws_bind` onto a LAN, say — gets TLS or nothing.
///
/// **Admissible and private are two questions, and they came apart when
/// `ws_allow_plaintext` was added.** That key lets an operator serve plaintext
/// on a LAN they vouch for, which is a decision about their network and *not* a
/// claim that the bytes are unreadable on it. Collapsing the two — the opt-in
/// used to answer [`Self::TrustedPath`] — meant everything gated on privacy
/// silently included the one configuration where there is none. The terminal is
/// what made that matter: a live shell's keystrokes are not something an
/// operator opts into sending in the clear by asking their phone to reach the
/// daemon at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaintextTrust {
    /// Loopback or this node's own tailnet address: plaintext is already
    /// private in transit.
    TrustedPath,
    /// A LAN the operator vouched for with `ws_allow_plaintext`: plaintext is
    /// admitted, on their instruction, and is exactly as private as that
    /// network. Shell-equivalent authority is not offered over it.
    OperatorAllowed,
    /// Anything else: plaintext would cross an unknown network in the clear,
    /// credential and all.
    RequireTls,
}

impl PlaintextTrust {
    /// Is a *plaintext* connection on this listener unreadable in transit?
    ///
    /// The question `terminal_pty` is gated on, and the reason this is a method
    /// rather than an inline comparison: two places ask it — the capability the
    /// ack advertises and the attach handler that enforces it independently —
    /// and they must never be able to disagree.
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
                    if let Err(err) = accept(daemon, stream, token, tls, trust).await {
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
/// hook path and every supervisor link, lives on the same budget. Without the
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
        return handle_client(daemon, stream, token, false, trust).await;
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
        return handle_client(daemon, stream, token, true, trust).await;
    }

    if plaintext_refused {
        crate::log_warn!("ws: refused a plaintext connection (wss is required on this listener)");
        return Ok(());
    }
    handle_client(daemon, stream, token, false, trust).await
}

/// `0x16` is the TLS `handshake` content type. No HTTP request method begins
/// with that byte, so one byte separates the two protocols unambiguously.
fn is_tls_hello(first: u8) -> bool {
    first == 0x16
}

/// The tmux socket a connection's attach resolves against: the daemon's, always,
/// except that a test may point it at a throwaway fixture server.
///
/// The hook exists because there is no other way to drive this loop's *own*
/// terminal arms — the attach, the output arm and its delivery accounting, the
/// teardown — against a real carrier. Without it the socket is a constant, every
/// attach over a live connection resolves nothing, and the arm that forwards
/// pane bytes is reachable only by reading the code.
#[cfg(test)]
static TEST_TMUX_SOCKET: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn tmux_socket() -> String {
    #[cfg(test)]
    if let Some(socket) = TEST_TMUX_SOCKET
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
    {
        return socket;
    }
    protocol::TMUX_SOCKET_NAME.to_string()
}

async fn handle_client<S>(
    daemon: Arc<Daemon>,
    stream: S,
    token: Arc<String>,
    tls_active: bool,
    trust: PlaintextTrust,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Whether *this* connection's bytes are unreadable in transit: TLS, or a
    // listener address on which plaintext already is. A terminal is
    // shell-equivalent authority and is offered on nothing else — including the
    // `ws_allow_plaintext` LAN, where the operator vouched for their network
    // and not for a cleartext shell on it. Decided once, here, and asked twice
    // below (the capability, and the attach handler that enforces it whatever
    // the client believed the capability said).
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
    let mut watermarks: HashMap<String, u64> = HashMap::new();
    let mut authed = false;
    // Read once for the connection rather than per message: only an attach uses
    // it, and every terminal frame would otherwise pay for a string it does not
    // need.
    let tmux_socket = tmux_socket();
    // Which device this connection belongs to, if any.
    //
    // Load-bearing for revocation: `hello` happens once and a phone then holds
    // the socket open for hours. Without re-checking, `codeconnect revoke` would take
    // effect only on the *next* connection — leaving a revoked device able to
    // keep answering approvals and typing into the session's TTY, which is the
    // opposite of what the operator just asked for.
    let mut device_id: Option<String> = None;
    // The connection's live terminal, split into three locals on purpose: the
    // control/credit side and the two receivers. Each select arm borrows only
    // its own receiver; the incoming arm's body borrows only `terminal`, so no
    // two ever contend for the same borrow across an await.
    //
    // They are installed together by a successful attach and cleared together by
    // every path that ends one, with two deliberate exceptions: the ack arm
    // clears `terminal_acks` alone when the writer ends (the output arm carries
    // the teardown), and `finish_terminal_open` installs none of them when it
    // refuses. Neither leaves a chunk without a terminal to write it to, which is
    // the state the output arm still has to answer for rather than assume away.
    //
    // **Declared handle-first, and that is load-bearing.** Locals drop in reverse
    // declaration order, so `terminal` — and with it `TerminalHandle::drop`,
    // which closes the carrier — goes *before* the drain. That is what makes a
    // reader parked on credit woken by the close rather than by its hand-over
    // failing, on every `?` and every `return` in this function.
    let mut terminal_acks: Option<tokio::sync::mpsc::UnboundedReceiver<u32>> = None;
    let mut terminal_out: Option<crate::terminal::OutputDrain> = None;
    let mut terminal: Option<TerminalConn> = None;
    // An attach whose carrier is still being opened. Its own local and its own
    // arm, because opening one is seconds of resolving, spawning and waiting for
    // a tmux client to bind — seconds this loop must not spend standing still.
    let mut opening: Option<Opening> = None;
    // The backlog this connection is part-way through paging out, for the same
    // reason: a replay is however many events the store holds, and awaiting it
    // inside the arm that asked for it is the loop standing still for all of
    // them. Its own local so no other arm's borrow ever meets it.
    let mut backlog = Backlog::default();
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.tick().await; // the first tick completes immediately

    let auth_deadline = tokio::time::sleep(HANDSHAKE_TIMEOUT);
    tokio::pin!(auth_deadline);

    loop {
        tokio::select! {
            // Read unconditionally. An attach in flight orders the *terminal*
            // frames behind it and nothing else: they go into the opening's own
            // bounded queue and are applied, in arrival order, the instant the
            // terminal they name exists (see `Opening::deferred`). Everything
            // else — an approval answer, a ping, a subscribe — is handled as it
            // arrives.
            //
            // It used to be gated on `opening.is_none()`, which bought that same
            // ordering with a fixed read buffer and a TCP window instead of a
            // queue, and charged the whole connection for it: an approval the
            // user had visibly given sat unread for up to `ATTACH_DEADLINE`
            // (twenty seconds) and could miss its `respond_by` outright, and the
            // app's own pings were unanswerable for the same window on a
            // connection whose timeout is thirty seconds.
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
                            features,
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
                                    message: format!(
                                        "this daemon speaks protocol {}.{}; upgrade the client",
                                        protocol::PROTOCOL_VERSION,
                                        protocol::PROTOCOL_MINOR,
                                    ),
                                }).await?;
                                return Ok(());
                            }
                            let outcome = daemon
                                .authenticate(
                                    &token,
                                    given.as_deref(),
                                    pairing_code.as_deref(),
                                    client_name.as_deref(),
                                )
                                .await;
                            let ack = match outcome {
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
                            authed = true;
                            device_id = match &ack {
                                AuthOutcome::Device(device) => Some(device.device_id.clone()),
                                AuthOutcome::Paired { device_id, .. } => Some(device_id.clone()),
                                _ => None,
                            };
                            crate::log_info!(
                                "ws: client {:?} connected over {} as {}",
                                client_name.as_deref().unwrap_or("unnamed"),
                                if tls_active { "wss" } else { "ws" },
                                describe(&ack),
                            );
                            // **The advertised feature set is read off the wire and
                            // dropped.** `hello` still carries a wire-legal
                            // `features`, and no shipping client puts anything in
                            // it — the iOS encoder has no such key, and the only
                            // other production client sends `None`. Persisting it
                            // meant a write path, an epoch stamp and a fail-closed
                            // record of failed writes, all acting on an input the
                            // wire cannot produce; the whole of it is gone. Said at
                            // debug rather than in silence so a phone that DOES
                            // start advertising is visible in a log before anybody
                            // wonders why it changed nothing.
                            if let Some(device_id) = device_id.as_deref() {
                                if features.is_some() {
                                    crate::log_debug!(
                                        "push: ignoring an advertised feature set from \
                                         {device_id}; nothing writes device features in this \
                                         phase"
                                    );
                                }
                            }
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
                    // Everything terminal goes down before the courtesy frame,
                    // so a phone that has stopped reading its socket cannot keep
                    // a live terminal — or an attach still opening — alive by
                    // wedging this send.
                    abandon_terminal(&mut terminal, &mut opening);
                    send(&mut sink, &ServerMessage::Error {
                        code: "revoked".into(),
                        message: "this device has been revoked".into(),
                    }).await?;
                    return Ok(());
                }

                // Terminal messages carry a live stream, not a request/response,
                // so they are driven against this connection's terminal state
                // rather than the stateless `handle_message`. A terminal is
                // shell-equivalent authority: only a paired device may open one,
                // never the static bootstrap token (which has no `device_id`).
                if is_terminal_message(&parsed) {
                    handle_terminal(
                        &daemon,
                        &mut terminal,
                        &mut terminal_out,
                        &mut terminal_acks,
                        &mut opening,
                        &mut sink,
                        &tmux_socket,
                        device_id.as_deref(),
                        private_transport,
                        parsed,
                    )
                    .await?;
                    continue;
                }

                handle_message(&daemon, &mut sink, &mut watermarks, &mut backlog, device_id.as_deref(), parsed)
                    .await?;
            }

            // One step of a backlog replay, when one is in progress: the next
            // event, or the next page from the store.
            //
            // The future is *ready*, not pending, and that is a decision. A
            // `yield_now` here would be pending on its first poll and so give
            // every other arm strict priority — which under a saturated pane
            // starves the replay instead, because the output arm is then ready
            // almost continuously. Both `biased` orders are wrong for the same
            // pair of reasons. `select!` picks uniformly at random among the
            // branches that are ready, so an always-ready branch gets served
            // about every other pass against any one competitor, and a stall
            // deadline is a hundred passes wide either way.
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
                // current-thread executor away from the carrier's own tasks,
                // which is the very starvation this arm exists to prevent.
                tokio::task::yield_now().await;
            }

            // Pane output, flow-controlled by the phone's credit. The carrier
            // only ever produces bytes it already held credit for, so
            // forwarding here cannot exceed the window; each forwarded byte
            // settles its grant in the ledger. `None` means the stream ended —
            // the pane closed or the client exited — so the terminal is torn
            // down (carrier first, courtesy frame second).
            chunk = next_terminal_output(&mut terminal_out) => {
                match chunk {
                    Some(chunk) => match terminal.as_mut() {
                        Some(conn) => write_terminal_chunk(&mut sink, conn, chunk).await?,
                        // A chunk with no terminal to write it to. The three
                        // locals move together, so there is no such state to
                        // reach — but the arm has to say what it does with a
                        // chunk it cannot write, and dropping one silently would
                        // settle bytes the phone never saw. Loud in a test build,
                        // logged in a release one, never quiet.
                        None => {
                            debug_assert!(
                                false,
                                "a pane chunk arrived with no terminal to write it to"
                            );
                            crate::log_warn!(
                                "ws: discarding {} pane bytes with no terminal attached",
                                chunk.bytes().len()
                            );
                        }
                    },
                    None => {
                        if let Some(conn) = terminal.take() {
                            let attachment_id = conn.attachment_id.clone();
                            // The carrier says why it ended — a session exit, an
                            // identity change, a stalled consumer — so the phone
                            // is told the truth rather than a blanket "ended".
                            // Both halves come from the carrier, because two of
                            // its endings share one wire code and only it knows
                            // which happened, so a sentence looked up from the
                            // code here could only ever be one of the two.
                            let cause = conn.handle.close_cause();
                            // Carrier down before the drain is dropped, so the
                            // handle is always closed first and a reader parked
                            // on credit is woken by that close rather than by its
                            // hand-over failing.
                            conn.teardown();
                            terminal_out = None;
                            terminal_acks = None;
                            close_terminal(&mut sink, &attachment_id, cause.code, cause.reason).await?;
                        }
                    }
                }
            }

            // Input the writer has handed to the tmux client, turned back into
            // replenished input credit for the phone. Bounded by the ledger, so
            // the sum cannot exceed the window and needs no ceiling check here.
            ack = next_input_ack(&mut terminal_acks) => {
                match ack {
                    Some(delivered) => {
                        if let Some(conn) = terminal.as_mut() {
                            conn.input_credit = conn.input_credit.saturating_add(delivered);
                            let attachment_id = conn.attachment_id.clone();
                            send(&mut sink, &ServerMessage::TerminalCredit {
                                attachment_id,
                                bytes: delivered,
                            }).await?;
                        }
                    }
                    // The writer ended: clear the receiver so this arm parks
                    // instead of returning `None` on every poll. The output
                    // arm carries the actual teardown.
                    None => terminal_acks = None,
                }
            }

            // An attach that has finished opening: this connection's terminal
            // now, or the reason there is none. The take is unconditional on
            // purpose — the arm above polls the spawned open by reference, and a
            // handle left in place after it yielded would be polled again on the
            // next pass, which panics.
            opened = next_terminal_open(&mut opening) => {
                if let Some(pending) = opening.take() {
                    let deferred = finish_terminal_open(
                        &daemon,
                        pending,
                        opened,
                        &mut terminal,
                        &mut terminal_out,
                        &mut terminal_acks,
                        &mut sink,
                        device_id.as_deref(),
                    )
                    .await?;
                    // The frames that arrived behind the attach, in the order
                    // they arrived, now that the terminal they name exists.
                    // Handled exactly as if they had been read here — including
                    // one that closes the terminal, after which the rest are
                    // answered `no such terminal`, which is what a phone that
                    // kept typing into a stream the daemon ended is owed.
                    for message in deferred {
                        handle_terminal(
                            &daemon,
                            &mut terminal,
                            &mut terminal_out,
                            &mut terminal_acks,
                            &mut opening,
                            &mut sink,
                            &tmux_socket,
                            device_id.as_deref(),
                            private_transport,
                            message,
                        )
                        .await?;
                    }
                }
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
                            // Carrier and attach down first, then a best-effort
                            // courtesy so the phone can say why it was
                            // disconnected. Neither is given a `terminal_closed`
                            // of its own: the whole connection is ending and says
                            // so, which is what a live terminal has always got
                            // here.
                            abandon_terminal(&mut terminal, &mut opening);
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

async fn handle_message<S>(
    daemon: &Arc<Daemon>,
    sink: &mut S,
    watermarks: &mut HashMap<String, u64>,
    // Where a subscribe asks for its backlog, rather than paging it out here:
    // this function is called from the arm that reads client messages, and a
    // replay awaited on that stack is the whole connection standing still for
    // however long the log is (see `Backlog`).
    backlog: &mut Backlog,
    // The authenticated device, when there is one. A client on the static
    // bootstrap token has no device row, so it has nowhere to register a push
    // token — see `RegisterPush`.
    device_id: Option<&str>,
    message: ClientMessage,
) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    match message {
        // A second hello does not re-authenticate, and there is nothing else for
        // it to do: the `features` it may carry is ignored on exactly the same
        // terms as the first hello's, because nothing in this phase writes a
        // device feature set.
        ClientMessage::Hello { features, .. } => {
            if let (Some(device_id), Some(_)) = (device_id, features) {
                crate::log_debug!(
                    "push: ignoring an advertised feature set from {device_id}; nothing \
                     writes device features in this phase"
                );
            }
        }
        ClientMessage::Ping => send(sink, &ServerMessage::Pong).await?,
        ClientMessage::RegisterPush {
            token,
            environment,
            relay_credential,
            features,
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
            // **The advertised feature set is ignored, exactly as on `hello`.** A
            // registration used to write it in the token's own statement, under
            // this run's epoch, with a fail-closed record of the writes that did
            // not land. Nothing on the wire can fill this field — no shipping
            // client encodes it — so all of that was a write path with no input,
            // and it is gone rather than kept working. The registration itself is
            // unaffected: the token, its environment and its credential are what a
            // phone actually sends, and they still commit as one tuple.
            if features.is_some() {
                crate::log_debug!(
                    "push: ignoring an advertised feature set from {device_id}; nothing \
                     writes device features in this phase"
                );
            }
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
        ClientMessage::DeleteSession { session_uid } => {
            // **By uid, never by name.** `resolve` accepts a tmux name and maps it
            // to the newest run under it, which is right for subscribing and wrong
            // for deleting: a name is handed to the next run, so a phone holding a
            // stale "cc-1" could destroy a session it never saw.
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
                .answer(&request_id, &payload_hash, decision, session_id.as_deref())
                .await;
            send(sink, &ServerMessage::AnswerResult { request_id, result }).await?;
        }
        ClientMessage::SendText {
            session_id,
            text,
            request_id,
            payload_hash,
            // Decoded so a client below minor 3 still parses, then discarded:
            // the interlock is the server's to choose.
            require: _,
            submit,
            complete_native_confirmation,
        } => {
            let result = daemon
                .send_text(
                    &session_id,
                    text,
                    request_id.as_deref(),
                    payload_hash.as_deref(),
                    submit,
                    complete_native_confirmation,
                )
                .await;
            send(sink, &ServerMessage::SendTextResult { session_id, result }).await?;
        }
        ClientMessage::Capture { session_id, lines } => {
            match daemon.capture(&session_id, lines.unwrap_or(80)).await {
                Ok(text) => {
                    send(sink, &ServerMessage::CaptureResult { session_id, text }).await?;
                }
                Err(err) => {
                    send(
                        sink,
                        &ServerMessage::Error {
                            code: "capture_failed".into(),
                            message: format!("{err}"),
                        },
                    )
                    .await?;
                }
            }
        }
        ClientMessage::GetCommandCatalog { session_id } => {
            let result = daemon.command_catalog(&session_id).await;
            send(sink, &ServerMessage::CommandCatalog { session_id, result }).await?;
        }
        ClientMessage::GetDiff { session_id } => match daemon.diff(&session_id).await {
            Ok(diff) => {
                let (unified, truncated) = fit_diff_in_a_frame(diff.unified, diff.truncated);
                send(
                    sink,
                    &ServerMessage::Diff {
                        session_id,
                        unified,
                        truncated,
                        // Stamped when the tree was read, not when the phone
                        // renders it: a diff without its age is a fact without
                        // its freshness.
                        captured_at: protocol::time::now_rfc3339(),
                        note: diff.note,
                    },
                )
                .await?;
            }
            // Only an unknown session reaches here; everything git could not do
            // comes back as a `note` on a successful diff.
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

        // The live-terminal messages carry a stream, not a request, so they are
        // intercepted against per-connection state before `handle_message` is
        // ever reached (see `is_terminal_message` in `handle_client`). Listing
        // them keeps this match exhaustive — a new client variant still forces a
        // decision here — while making the routing invariant explicit.
        ClientMessage::TerminalAttach { .. }
        | ClientMessage::TerminalInput { .. }
        | ClientMessage::TerminalResize { .. }
        | ClientMessage::TerminalCredit { .. }
        | ClientMessage::TerminalDetach { .. } => {
            unreachable!("terminal messages are routed by handle_terminal, not handle_message")
        }
        // Interrupt is a Codex steering operation whose actuation lands in a
        // later phase. It is **refused honestly** here, not dropped: the client
        // that sent it gets a typed `Rejected` naming the reason, so a mutation
        // result is never a silence. A Claude client never sends one.
        ClientMessage::Interrupt {
            session_id,
            request_id,
            ..
        } => {
            send(
                sink,
                &ServerMessage::InterruptResult {
                    session_id,
                    request_id,
                    result: protocol::ws::InterruptResult::Rejected {
                        reason: "interrupt is not supported yet".into(),
                    },
                },
            )
            .await?;
        }
    }
    Ok(())
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
fn fit_diff_in_a_frame(unified: String, truncated: bool) -> (String, bool) {
    // Headroom for the rest of the message (session id, timestamp, note, keys).
    let ceiling = MAX_CLIENT_MESSAGE_BYTES.saturating_sub(4096);
    let encoded_len = |text: &str| serde_json::to_string(text).map(|s| s.len()).unwrap_or(0);
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

// ------------------------------------------------------------------ terminal
//
// A live terminal is a long-lived bidirectional stream, not a request/response.
// Its connection state lives in `handle_client` as four deliberately separate
// locals — the control/credit side (`TerminalConn`), the output receiver
// (`terminal_out`), the input-ack receiver (`terminal_acks`), and the attach
// still opening (`opening`) — so each select arm borrows only its own, and no
// two ever conflict at the same await.

use base64::Engine as _;

/// Whether a client message belongs to the terminal stream (driven against
/// per-connection state) rather than the stateless request path.
fn is_terminal_message(message: &ClientMessage) -> bool {
    matches!(
        message,
        ClientMessage::TerminalAttach { .. }
            | ClientMessage::TerminalInput { .. }
            | ClientMessage::TerminalResize { .. }
            | ClientMessage::TerminalCredit { .. }
            | ClientMessage::TerminalDetach { .. }
    )
}

/// The connection's half of one live terminal: the carrier handle (everything
/// on it is non-blocking) and the exact credit ledger.
struct TerminalConn {
    attachment_id: String,
    handle: crate::terminal::TerminalHandle,
    /// One permit per output byte the phone will accept. The carrier's reader
    /// spends these; `terminal_credit` from the phone replenishes them.
    output_credit: Arc<tokio::sync::Semaphore>,
    /// The authoritative outstanding output-credit count: granted by the phone,
    /// not yet spent on a forwarded byte. The ceiling is enforced against this —
    /// never against the semaphore's `available_permits`, which under-reports
    /// while the reader has an acquire pending and would let a hostile grant
    /// overshoot the documented maximum.
    outstanding: u32,
    /// Input bytes the phone may still send before the daemon replenishes. It
    /// falls as input is accepted and rises only once the writer has delivered
    /// those bytes to the pane, so a phone that honours it can never queue more
    /// than one window of unwritten input in the daemon.
    input_credit: u32,
}

impl TerminalConn {
    /// Tear the carrier down; synchronous, never waits on anything. Called
    /// before any close frame is sent, so a phone that has stopped reading can
    /// slow the courtesy message but never the cleanup.
    fn teardown(self) {
        self.handle.close();
    }
}

/// Give up this connection's terminal *and* whatever attach was still opening.
/// Synchronous; waits on nothing.
///
/// One function because the two must always go together, and a connection that
/// tore down only the terminal is how they came apart: a revoked device whose
/// attach was still in flight kept an open running behind the close it was
/// sent — free to take the daemon-wide lease, supersede another connection's
/// terminal and spawn tmux for a device that had just been cut off. An
/// `Opening` aborts its open only when it is *dropped*, so it is dropped here
/// rather than parked in a local across whatever the caller writes next.
fn abandon_terminal(terminal: &mut Option<TerminalConn>, opening: &mut Option<Opening>) {
    if let Some(conn) = terminal.take() {
        conn.teardown();
    }
    drop(opening.take());
}

/// An attach whose carrier is still being opened, and everything the finished
/// open needs to become a live terminal.
///
/// Opening one resolves the session, spawns a tmux client, and waits for that
/// client to announce its attach and report its pane — seconds, in the worst
/// case. Parked here rather than awaited in the incoming-message arm, those
/// seconds stay the connection's to spend on everything else: the event log,
/// input acks, a previous terminal's output, revocation, keepalive.
struct Opening {
    attachment_id: String,
    /// The uid asked for and the geometry asked at, for the audit line the
    /// attach writes once it lands.
    session_uid: String,
    cols: u16,
    rows: u16,
    task: OpenTask,
    /// The semaphore the carrier spends output credit from, and the grant it
    /// was opened with: the open is handed the one, and the ledger needs the
    /// other.
    output_credit: Arc<tokio::sync::Semaphore>,
    granted: u32,
    /// The carrier's two streams, parked until there is a terminal to hang them
    /// on. The carrier can already be painting into `out` while the open
    /// finishes; a queue nobody is draining yet simply holds its reader, which
    /// costs the phone nothing (see `terminal::forward`).
    out: crate::terminal::OutputDrain,
    acks: tokio::sync::mpsc::UnboundedReceiver<u32>,
    /// Terminal frames that arrived while this open ran, in arrival order.
    ///
    /// They cannot be applied yet — there is no terminal for them to name — and
    /// they must not be dropped or answered `no such terminal`, because a phone
    /// that types the instant it sends `terminal_attach` is doing nothing wrong.
    /// So they wait here, and the open's own resolution drains them. Bounded by
    /// [`MAX_DEFERRED_TERMINAL_FRAMES`]; a queue that overflows fails the attach
    /// it belongs to and nothing else.
    ///
    /// Carried by the `Opening` rather than by the connection so that every path
    /// that ends an open — a refusal, a supersede, a revocation, a dropped
    /// connection — discards it by construction: the ids it names are dead the
    /// moment the attach is, and answering them afterwards would tell the phone
    /// about an attachment it has already been given a close for.
    deferred: VecDeque<ClientMessage>,
    /// Payload bytes the queue is holding, against
    /// [`MAX_DEFERRED_TERMINAL_BYTES`]. A count of frames is not a bound on
    /// memory; see that constant.
    deferred_bytes: usize,
    /// Decoded input bytes parked so far, against
    /// [`protocol::ws::TERMINAL_INITIAL_INPUT_CREDIT`] — the window the phone
    /// will be granted when this attach lands, and the only input it may have
    /// in flight until then.
    deferred_input: u32,
}

/// How many payload bytes may wait behind an attach that is still opening.
///
/// [`MAX_DEFERRED_TERMINAL_FRAMES`] bounds the queue's *length*, which is not a
/// bound on what it costs: frames are parked as deserialized `ClientMessage`s
/// and one may carry close to [`protocol::ws::MAX_CLIENT_MESSAGE_BYTES`], so
/// 128 of them is ~128 MiB parked in a single connection's opening, multiplied
/// by however many connections a paired client cares to open.
///
/// What a client honouring the protocol can legitimately park is far smaller
/// and exactly knowable: no input credit is returned until the terminal exists,
/// so at most `TERMINAL_INITIAL_INPUT_CREDIT` (32 KiB) of decoded input, which
/// base64 carries in ~44 KiB, plus one attachment id (≤64 bytes) per frame.
/// 64 KiB covers that with room to spare and covers nothing else — including a
/// `terminal_resize` whose only large field is an id no attachment could have.
const MAX_DEFERRED_TERMINAL_BYTES: usize = 64 * 1024;

/// The bytes a parked frame keeps alive: its payload and the id it names.
///
/// Approximate on purpose for the frames that *are* parked — the fixed-size
/// fields and the `VecDeque` slot are not counted, because what makes this a
/// resource bound rather than a frame count is the variable-length parts, and
/// they dominate by orders of magnitude.
///
/// Everything else is costed at more than [`MAX_DEFERRED_TERMINAL_BYTES`] will
/// ever admit, which is the half of this that has to hold in the future rather
/// than today: a frame this function does not know how to size is refused by
/// [`Opening::defer`] instead of being stored for nothing. A `0` here — which is
/// what the catch-all used to return — would make a new payload-bearing variant
/// routed to the queue free of charge, and the ceiling above it a bound on
/// nothing.
fn deferred_cost(message: &ClientMessage) -> usize {
    match message {
        ClientMessage::TerminalInput {
            attachment_id,
            data,
        } => attachment_id.len() + data.len(),
        ClientMessage::TerminalResize { attachment_id, .. }
        | ClientMessage::TerminalCredit { attachment_id, .. }
        | ClientMessage::TerminalDetach { attachment_id } => attachment_id.len(),
        // The terminal variant that is never parked: an attach supersedes the
        // open rather than queueing behind it (`handle_terminal` routes it out
        // before `defer` is reached), so naming it here is documentation, and
        // costing it unadmittable is what happens if that routing ever changes
        // without this being revisited.
        ClientMessage::TerminalAttach { .. } => MAX_DEFERRED_TERMINAL_BYTES + 1,
        // And nothing else reaches the queue at all: `is_terminal_message`
        // decides what `handle_terminal` — the only caller of `defer` — is given.
        // Unadmittable rather than free, so the failure mode of a routing change
        // or a new variant is one refused attach, never an unbounded queue.
        _ => MAX_DEFERRED_TERMINAL_BYTES + 1,
    }
}

impl Opening {
    /// Park a frame behind this open, or say why it cannot be parked.
    ///
    /// A frame is judged **on arrival**, not when the queue drains. The queue
    /// used to admit whatever deserialized and leave base64 decoding, the chunk
    /// bound and the credit ledger to the drain — so a frame that could only
    /// ever end in `protocol_error` still occupied the daemon for the length of
    /// the open. Refusing it here is what makes the bounds above bounds on what
    /// the protocol permits rather than on what a client chooses to send.
    ///
    /// The `Err` is the reason for a `protocol_error` on the attach's own id;
    /// the attach pays and the connection does not, exactly as the length bound
    /// has always worked.
    fn defer(&mut self, message: ClientMessage) -> std::result::Result<(), &'static str> {
        use protocol::ws::{MAX_TERMINAL_CHUNK_BYTES, TERMINAL_INITIAL_INPUT_CREDIT};

        if let ClientMessage::TerminalInput { data, .. } = &message {
            let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(data) else {
                return Err("malformed terminal input arrived while the attach was opening");
            };
            if decoded.len() > MAX_TERMINAL_CHUNK_BYTES {
                return Err("an oversized terminal frame arrived while the attach was opening");
            }
            // The window is not open yet, so this is all the input the phone
            // may have in flight; a frame past it is the same over-credit
            // violation the live terminal answers, caught before it is stored.
            let Some(spent) = self
                .deferred_input
                .checked_add(decoded.len() as u32)
                .filter(|spent| *spent <= TERMINAL_INITIAL_INPUT_CREDIT)
            else {
                return Err(
                    "terminal input beyond the credit window arrived while the attach was opening",
                );
            };
            self.deferred_input = spent;
        }
        if self.deferred.len() >= MAX_DEFERRED_TERMINAL_FRAMES {
            return Err("too many terminal frames arrived while the attach was opening");
        }
        let Some(held) = self
            .deferred_bytes
            .checked_add(deferred_cost(&message))
            .filter(|held| *held <= MAX_DEFERRED_TERMINAL_BYTES)
        else {
            return Err("too many terminal bytes arrived while the attach was opening");
        };
        self.deferred_bytes = held;
        self.deferred.push_back(message);
        Ok(())
    }
}

/// How many terminal frames may wait behind an attach that is still opening.
///
/// A bound, not a budget. What a legitimate client can put here is fixed by
/// something else entirely: input is credit-bounded to
/// `TERMINAL_INITIAL_INPUT_CREDIT` (32 KiB) of *unacknowledged* bytes and no
/// credit is returned until the terminal exists, so a phone honouring the
/// protocol cannot queue 128 input frames unless it is sending them 256 bytes
/// at a time — and a resize storm coalesces at the carrier, not here. So this
/// catches a client that is not honouring the protocol, and for that the answer
/// is to fail its attach (`protocol_error`) rather than to grow, or to take the
/// whole connection down over frames the rest of it never saw.
///
/// A count alone is not a resource bound — see [`MAX_DEFERRED_TERMINAL_BYTES`],
/// which is the other half of it.
const MAX_DEFERRED_TERMINAL_FRAMES: usize = 128;

/// A spawned [`crate::terminal::TerminalHandle::open`], aborted if it is dropped
/// rather than joined.
///
/// Dropping is how every path that ends a connection mid-attach reaches it — a
/// revocation, a read error, a peer going away — and abandoning an attach has to
/// be as non-blocking as the rest of the teardown.
///
/// What the abort buys is promptness, not safety — and promptness in tokio's
/// sense, which is weaker than "instantly". `abort` is cooperative: it stops a
/// task that is parked, and a task that is mid-poll on another worker runs to
/// its next yield point first. An open interrupted that way can still acquire
/// the session's lease, and can still supersede the incumbent that held it, on
/// its way there. That is *tolerable* rather than merely tolerated, and for the
/// reason below: whatever the aborted open managed to build is then dropped.
///
/// Both outcomes converge on the same teardown: an aborted open drops its
/// future, and a completed one whose handle is never taken drops its output, and
/// either way the `TerminalHandle` is dropped, which closes the carrier — that
/// close is what wakes the reaper holding the child and the leases, which is the
/// *only* thing that frees them.
/// (`kill_on_drop` on the child is a backstop for runtime shutdown, not the
/// mechanism here: the reaper owns the child, and abort does not touch it.) So
/// dropping this is always correct; it just returns the session's one-terminal
/// lease in a reap's time rather than at the end of an open nobody awaits.
struct OpenTask(
    tokio::task::JoinHandle<Result<crate::terminal::TerminalHandle, crate::terminal::OpenError>>,
);

impl Drop for OpenTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The attach arm's future: the carrier of the attach in flight, or never (when
/// there is none). At most one attach is ever in flight — a second
/// `terminal_attach` supersedes the first rather than joining it — so this fires
/// once per attach.
async fn next_terminal_open(
    opening: &mut Option<Opening>,
) -> Result<crate::terminal::TerminalHandle, crate::terminal::OpenError> {
    match opening {
        Some(pending) => (&mut pending.task.0).await.unwrap_or_else(|join| {
            // Only a panic inside the attach reaches here: an abort happens
            // when the `Opening` is dropped, which this borrow rules out.
            // Reported as the attach failing, so the phone is told rather than
            // left waiting on an acknowledgement that is never coming.
            Err(crate::terminal::OpenError::Unavailable(format!(
                "the attach task failed: {join}"
            )))
        }),
        None => std::future::pending().await,
    }
}

/// Turn a finished attach into this connection's terminal, or tell the phone why
/// there is none.
///
/// The revocation re-check lives here because this is where the window it closes
/// ends: the open awaited tmux, and a device revoked while it ran must not
/// receive its first pane byte through that window.
///
/// Returns the terminal frames that arrived while the open ran, for the caller
/// to apply now that there is a terminal to apply them to. Empty on every
/// refusal: those frames name an attachment the phone has just been told is
/// closed.
#[allow(clippy::too_many_arguments)]
async fn finish_terminal_open<S>(
    daemon: &Arc<Daemon>,
    pending: Opening,
    opened: Result<crate::terminal::TerminalHandle, crate::terminal::OpenError>,
    terminal: &mut Option<TerminalConn>,
    terminal_out: &mut Option<crate::terminal::OutputDrain>,
    terminal_acks: &mut Option<tokio::sync::mpsc::UnboundedReceiver<u32>>,
    sink: &mut S,
    device_id: Option<&str>,
) -> Result<VecDeque<ClientMessage>>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    use protocol::ws::terminal_close as tc;
    use protocol::ws::{
        MAX_TERMINAL_CHUNK_BYTES, TERMINAL_INITIAL_INPUT_CREDIT, TERMINAL_MAX_OUTSTANDING_CREDIT,
    };

    // The task goes with the rest: it has already produced `opened`, so the
    // abort its drop fires is a no-op, and naming it here would only be to
    // ignore it.
    let Opening {
        attachment_id,
        session_uid,
        cols,
        rows,
        output_credit,
        granted,
        out,
        acks,
        deferred,
        ..
    } = pending;

    let handle = match opened {
        Err(err) => {
            close_terminal(sink, &attachment_id, err.close_code(), &err.reason()).await?;
            return Ok(VecDeque::new());
        }
        Ok(handle) => handle,
    };
    if revoked(daemon, device_id).await {
        handle.close();
        close_terminal(
            sink,
            &attachment_id,
            tc::NOT_AUTHORISED,
            "this device has been revoked",
        )
        .await?;
        return Ok(VecDeque::new());
    }
    // A terminal is shell-equivalent authority; record *which device* attached,
    // and to which tmux session and disposable client, so the grant leaves an
    // audit trail. Without the device id, concurrent phones on one Mac are
    // indistinguishable in the log. (`None` is unreachable — the attach arm
    // refuses a connection with no paired device — but a log line is not the
    // place to assert it.)
    crate::log_info!(
        "ws: device {} attached a terminal to session {} (uid {}, client pid {}) at {}x{}",
        device_id.unwrap_or("?"),
        handle.session_id(),
        session_uid,
        handle.client_pid(),
        cols,
        rows,
    );
    *terminal = Some(TerminalConn {
        attachment_id: attachment_id.clone(),
        handle,
        output_credit,
        outstanding: granted,
        input_credit: TERMINAL_INITIAL_INPUT_CREDIT,
    });
    *terminal_out = Some(out);
    *terminal_acks = Some(acks);
    send(
        sink,
        &ServerMessage::TerminalAttached {
            attachment_id,
            input_credit: TERMINAL_INITIAL_INPUT_CREDIT,
            // Advertised rather than left for the client to hard-code: a phone
            // enforcing its own copy of these dies mid-session the day either
            // one moves. See `ServerMessage::TerminalAttached`.
            max_chunk_bytes: MAX_TERMINAL_CHUNK_BYTES as u32,
            max_outstanding_credit: TERMINAL_MAX_OUTSTANDING_CREDIT,
        },
    )
    .await?;
    Ok(deferred)
}

/// Encode a `terminal_closed` and send it. The write is bounded like every
/// other, and its result propagates: a courtesy frame that cannot be delivered
/// means the peer has stopped reading, which is connection-fatal — the caller
/// closing one attachment must not be left running against a peer that will
/// never hear anything again.
async fn close_terminal<S>(
    sink: &mut S,
    attachment_id: &str,
    code: &str,
    reason: &str,
) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    send(
        sink,
        &ServerMessage::TerminalClosed {
            attachment_id: attachment_id.to_string(),
            code: code.to_string(),
            reason: reason.to_string(),
        },
    )
    .await
}

/// Handle one terminal client message. Mutates the connection's terminal state
/// and may write to the sink. Any protocol violation closes only that
/// attachment; the connection survives.
#[allow(clippy::too_many_arguments)]
async fn handle_terminal<S>(
    daemon: &Arc<Daemon>,
    terminal: &mut Option<TerminalConn>,
    terminal_out: &mut Option<crate::terminal::OutputDrain>,
    terminal_acks: &mut Option<tokio::sync::mpsc::UnboundedReceiver<u32>>,
    // Where an attach in flight is parked. `Some` means the connection has no
    // terminal *yet*: every frame but another attach waits in that opening's
    // queue until it does.
    opening: &mut Option<Opening>,
    sink: &mut S,
    tmux_socket: &str,
    device_id: Option<&str>,
    // Whether this connection's bytes are unreadable in transit. A terminal is
    // shell-equivalent authority and needs one; see `handle_client`.
    private_transport: bool,
    message: ClientMessage,
) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    use protocol::ws::terminal_close as tc;
    use protocol::ws::{
        MAX_ATTACHMENT_ID_BYTES, MAX_TERMINAL_CHUNK_BYTES, TERMINAL_MAX_COLS,
        TERMINAL_MAX_OUTSTANDING_CREDIT, TERMINAL_MAX_ROWS, TERMINAL_MIN_COLS, TERMINAL_MIN_ROWS,
    };

    // Close the current terminal for a violation the phone caused: carrier
    // first (synchronous — a stalled phone can slow the frame, never the
    // cleanup), then clear the receivers, then the frame.
    macro_rules! fail_current {
        ($id:expr, $code:expr, $reason:expr) => {{
            if let Some(conn) = terminal.take() {
                conn.teardown();
            }
            *terminal_out = None;
            *terminal_acks = None;
            close_terminal(sink, $id, $code, $reason).await?;
            return Ok(());
        }};
    }

    // Behind an attach that is still opening, every frame but another attach
    // waits its turn. It cannot be applied — the terminal it names does not
    // exist yet — and it must not be refused, because sending input the instant
    // the attach goes out is a phone doing exactly what the protocol allows.
    if let Some(pending) = opening.as_mut() {
        if !matches!(message, ClientMessage::TerminalAttach { .. }) {
            if let Err(why) = pending.defer(message) {
                // The attach pays, and only the attach: the connection carries
                // an event log and an approval path that these frames have
                // nothing to do with. Dropping the `Opening` aborts the open and
                // discards the queue with it — before the write below, which a
                // stalled phone can hold for the whole deadline.
                let refused = pending.attachment_id.clone();
                *opening = None;
                close_terminal(sink, &refused, tc::PROTOCOL_ERROR, why).await?;
            }
            return Ok(());
        }
    }

    match message {
        ClientMessage::TerminalAttach {
            attachment_id,
            session_uid,
            cols,
            rows,
            output_credit,
        } => {
            // An attach that re-uses the id the connection is already using is
            // handled *before* the refusals below, and it is the one case that
            // is: a phone that attaches under an id it already holds has
            // plainly abandoned what that id named, and `terminal_closed` is
            // documented as terminal for an id. So the collision is itself the
            // supersede — the old carrier goes down here, silently, and the one
            // frame the phone eventually gets for this id is either the
            // `terminal_attached` below or a refusal, never a close that some
            // later frame contradicts by continuing to stream.
            //
            // The price is that a *malformed* attach re-using a live id does
            // cost that terminal. That is the right way round: the alternative
            // is a refusal naming an id that is still streaming, which iOS
            // treats as terminal and clears — orphaning a running tmux client
            // and its lease behind a UI that has forgotten them.
            //
            // Only what the colliding id names is touched. A terminal and an
            // attach in flight are mutually exclusive today — an attach
            // installs an `Opening`, and finishing it installs the terminal —
            // but saying it per id means a change to that can never turn one
            // supersede into an attachment abandoned without an answer.
            if let Some(conn) = terminal.take_if(|conn| conn.attachment_id == attachment_id) {
                conn.teardown();
                *terminal_out = None;
                *terminal_acks = None;
            }
            // Dropped rather than cleared: dropping the `Opening` is what
            // aborts the open behind it.
            drop(opening.take_if(|pending| pending.attachment_id == attachment_id));

            // Every refusal below is decided before anything the frame did
            // *not* name is torn down — with the collision above as the stated
            // exception, and the only one. A malformed attach must not cost the
            // phone a terminal it did not just abandon: the frame proves the
            // sender is confused, and answering "no" while its working shell
            // keeps running is strictly better than trading a live terminal for
            // a protocol error. So a non-colliding attach leaves the live
            // terminal alone whatever these checks decide, while an attach
            // re-using the connection's own id has already superseded what that
            // id named — deliberately, and before this block, because the
            // supersede is the collision and not the frame's validity. Nothing
            // in this block reads the connection's terminal state, so the order
            // among the checks is free; it only has to sit after the collision
            // and before any other teardown.
            if attachment_id.is_empty() || attachment_id.len() > MAX_ATTACHMENT_ID_BYTES {
                close_terminal(
                    sink,
                    &attachment_id,
                    tc::PROTOCOL_ERROR,
                    "bad attachment id",
                )
                .await?;
                return Ok(());
            }
            // Shell-equivalent authority: only a paired device, never the
            // static bootstrap token. Enforced here independently of the
            // advertised capability, so a client that ignores the capability
            // still cannot open one.
            if device_id.is_none() {
                close_terminal(
                    sink,
                    &attachment_id,
                    tc::NOT_AUTHORISED,
                    "this connection may not open a terminal",
                )
                .await?;
                return Ok(());
            }
            // And only over a transport nothing on the path can read. Enforced
            // here as well as in the advertised capability, for the same reason
            // the device check is: a client that ignores what it was told must
            // still not get a cleartext shell.
            if !private_transport {
                close_terminal(
                    sink,
                    &attachment_id,
                    tc::NOT_AUTHORISED,
                    "a terminal needs an encrypted connection",
                )
                .await?;
                return Ok(());
            }
            if !(TERMINAL_MIN_COLS..=TERMINAL_MAX_COLS).contains(&cols)
                || !(TERMINAL_MIN_ROWS..=TERMINAL_MAX_ROWS).contains(&rows)
            {
                close_terminal(sink, &attachment_id, tc::PROTOCOL_ERROR, "bad geometry").await?;
                return Ok(());
            }
            if output_credit == 0 || output_credit > TERMINAL_MAX_OUTSTANDING_CREDIT {
                close_terminal(
                    sink,
                    &attachment_id,
                    tc::PROTOCOL_ERROR,
                    "bad output credit",
                )
                .await?;
                return Ok(());
            }
            // A uid that is not a well-formed stamp can never name a session,
            // so it is refused here rather than by the resolver — which sits
            // *behind* the supersede and the lease, and so answered
            // `session_not_hosted` only after an attach carrying
            // `session_uid: ""` had already destroyed a working terminal. Same
            // code and same words the resolver would have used, so the wire is
            // unchanged: from the phone's side this is an unknown session.
            if !protocol::uid::is_well_formed(&session_uid) {
                close_terminal(
                    sink,
                    &attachment_id,
                    tc::SESSION_NOT_HOSTED,
                    &crate::terminal::OpenError::NotHosted.reason(),
                )
                .await?;
                return Ok(());
            }

            // The attach is good, so it may now take the connection's terminal
            // over rather than be punished for asking. A phone sending one is a
            // phone whose view of its own terminal has diverged from the
            // daemon's — a reconnect, a tab reopened, a detach whose write was
            // lost — and refusing it used to leave the *new* id unanswered for
            // ever while the old stream ran on.
            //
            // The old id is closed as `superseded` (so `terminal_closed` stays
            // truly terminal for it, and a same-id re-attach cannot leave the
            // previous terminal streaming behind a close the phone already saw).
            // The session's lease is released by the reaper a moment later, so
            // the open below meets its own dying predecessor and supersedes it
            // there too — one mechanism, not two.
            if let Some(conn) = terminal.take() {
                let existing = conn.attachment_id.clone();
                conn.teardown();
                *terminal_out = None;
                *terminal_acks = None;
                close_terminal(
                    sink,
                    &existing,
                    tc::SUPERSEDED,
                    "a newer attach took this session's terminal over",
                )
                .await?;
            }
            // An attach that arrives while another is still opening supersedes
            // it on the same reasoning, and the id it displaces is answered
            // rather than abandoned. Dropping the `Opening` aborts the open and
            // takes the queue behind it with it.
            if let Some(displaced) = opening.take() {
                let displaced_id = displaced.attachment_id.clone();
                // Dropped *before* the courtesy write, never held across it: a
                // stalled phone can hold that write for the whole 20s deadline,
                // and an `Opening` alive through it is an open still running —
                // free to take the daemon-wide lease, supersede another
                // connection's terminal and spawn tmux — behind a connection
                // that has already decided it has no attach in flight.
                drop(displaced);
                close_terminal(
                    sink,
                    &displaced_id,
                    tc::SUPERSEDED,
                    "a newer attach took this session's terminal over",
                )
                .await?;
            }

            let credit = Arc::new(tokio::sync::Semaphore::new(output_credit as usize));
            let (chunks, out) = crate::terminal::output_channel(4);
            let (acks_tx, acks) = tokio::sync::mpsc::unbounded_channel();
            // Spawned, never awaited here. The open resolves the session,
            // spawns a tmux client and waits for it to announce and bind:
            // seconds, in the worst case, and awaited on this line they would be
            // seconds in which the connection polls nothing else — no event, no
            // input ack, no pane byte, no revocation. An approval card raised
            // meanwhile would sit unsent, and a chatty fleet would lag the
            // client off the broadcast ring. The select loop takes the finished
            // attach from `opening` instead, and holds the terminal frames
            // behind this one in that opening's own queue, so they keep their
            // order and none is applied before the terminal it names exists.
            let attach = {
                let daemon = Arc::clone(daemon);
                let socket = tmux_socket.to_string();
                let session_uid = session_uid.clone();
                let credit = Arc::clone(&credit);
                tokio::spawn(async move {
                    let opened = tokio::time::timeout(
                        attach_deadline(),
                        crate::terminal::TerminalHandle::open(
                            &daemon.terminal_leases,
                            &socket,
                            &session_uid,
                            cols,
                            rows,
                            credit,
                            chunks,
                            acks_tx,
                        ),
                    )
                    .await;
                    // Dropping the open on expiry tears down whatever it had
                    // built, by the same route abandoning it does.
                    opened.unwrap_or_else(|_| {
                        Err(crate::terminal::OpenError::Unavailable(
                            "the attach did not complete in time".to_string(),
                        ))
                    })
                })
            };
            *opening = Some(Opening {
                attachment_id,
                session_uid,
                cols,
                rows,
                task: OpenTask(attach),
                output_credit: credit,
                granted: output_credit,
                out,
                acks,
                deferred: VecDeque::new(),
                deferred_bytes: 0,
                deferred_input: 0,
            });
        }

        ClientMessage::TerminalInput {
            attachment_id,
            data,
        } => {
            // The target is checked first, so any failure below tears down the
            // terminal the phone actually named — not a ghost id while the real
            // terminal keeps running behind a `terminal_closed` the phone saw.
            if !terminal
                .as_ref()
                .is_some_and(|conn| conn.attachment_id == attachment_id)
            {
                close_terminal(sink, &attachment_id, tc::PROTOCOL_ERROR, "no such terminal")
                    .await?;
                return Ok(());
            }
            // One decode that also bounds the chunk. A malformed or oversized
            // frame is a protocol violation that closes the terminal.
            let bytes = match base64::engine::general_purpose::STANDARD.decode(&data) {
                Ok(bytes) if bytes.len() <= MAX_TERMINAL_CHUNK_BYTES => bytes,
                _ => fail_current!(&attachment_id, tc::PROTOCOL_ERROR, "malformed input"),
            };
            // An empty frame carries nothing to type: dropped outright, so it
            // spends no credit, queues nothing, and produces no ack.
            if bytes.is_empty() {
                return Ok(());
            }
            // Spend input credit: a frame larger than the window the phone was
            // granted is an over-credit protocol violation. The credit is
            // replenished only once the writer has delivered these bytes to the
            // pane (the `terminal_acks` arm), so the ledger reflects unwritten
            // input and a phone that honours it never queues more than a window.
            let conn = terminal.as_mut().unwrap();
            let spend = bytes.len() as u32;
            if spend > conn.input_credit {
                fail_current!(&attachment_id, tc::PROTOCOL_ERROR, "input over credit");
            }
            conn.input_credit -= spend;
            // Hand off without awaiting, so a wedged pane cannot stop this loop
            // from seeing revocation or disconnect.
            if conn.handle.input(bytes).is_err() {
                fail_current!(&attachment_id, tc::SESSION_EXITED, "the session ended");
            }
        }

        ClientMessage::TerminalResize {
            attachment_id,
            cols,
            rows,
        } => {
            // A resize that cannot be honoured is ignored, not fatal: the
            // current size still holds and the terminal keeps working.
            if (TERMINAL_MIN_COLS..=TERMINAL_MAX_COLS).contains(&cols)
                && (TERMINAL_MIN_ROWS..=TERMINAL_MAX_ROWS).contains(&rows)
            {
                if let Some(conn) = terminal
                    .as_ref()
                    .filter(|conn| conn.attachment_id == attachment_id)
                {
                    conn.handle.resize(cols, rows);
                }
            }
        }

        ClientMessage::TerminalCredit {
            attachment_id,
            bytes,
        } => {
            let Some(conn) = terminal
                .as_mut()
                .filter(|conn| conn.attachment_id == attachment_id)
            else {
                close_terminal(sink, &attachment_id, tc::PROTOCOL_ERROR, "no such terminal")
                    .await?;
                return Ok(());
            };
            // Enforced against the exact ledger; the sum is computed wide so
            // a grant near u32::MAX cannot wrap its way past the ceiling.
            if u64::from(conn.outstanding) + u64::from(bytes)
                > u64::from(TERMINAL_MAX_OUTSTANDING_CREDIT)
            {
                fail_current!(&attachment_id, tc::PROTOCOL_ERROR, "output credit overflow");
            }
            conn.outstanding += bytes;
            conn.output_credit.add_permits(bytes as usize);
        }

        ClientMessage::TerminalDetach { attachment_id } => {
            if let Some(conn) = terminal.take_if(|conn| conn.attachment_id == attachment_id) {
                conn.teardown();
                *terminal_out = None;
                *terminal_acks = None;
                close_terminal(sink, &attachment_id, tc::DETACHED, "detached").await?;
            }
        }

        // handle_terminal is only reached for the terminal variants.
        _ => unreachable!("handle_terminal received a non-terminal message"),
    }
    Ok(())
}

/// Write one pane chunk to the phone, then settle it.
///
/// A function rather than four lines inside the select arm, because the ordering
/// is the whole of finding #3 and inline it was executed by no test at all: the
/// chunk counts against the carrier's stall deadline until it is dropped, and it
/// is dropped only once the write has returned. Taking a chunk off the queue is
/// not delivering it — the queue reads empty behind a write that is still in
/// flight, and a carrier that settled at the dequeue would read that as the phone
/// withholding credit and close a healthy one (`terminal::forward`).
///
/// The grant is settled against the ledger first, as it always was: that is the
/// phone's window reopening, which is a different question from delivery.
///
/// **One frame per chunk, and the margin for that is forty-fold.**
/// A piece is never larger than `MAX_TERMINAL_CHUNK_BYTES` (16 KiB) — the
/// carrier splits at exactly that, which is what makes the bound the protocol
/// documents as receiver-enforceable true — and base64 expands it by four
/// thirds to ~22 KiB, against a `MAX_CLIENT_MESSAGE_BYTES` of 1 MiB. Those two
/// are the pair that is load-bearing against each other, and nothing else here
/// is: the credit ceiling governs how much may be outstanding, never how much
/// rides one frame. Raise the chunk bound past 768 KiB and a single pane chunk
/// stops fitting — at which point [`send`] substitutes an oversized
/// placeholder, which for a `terminal_output` is an `error` frame carrying none
/// of the bytes, and the ledger has already been settled for them above.
async fn write_terminal_chunk<S>(
    sink: &mut S,
    conn: &mut TerminalConn,
    chunk: crate::terminal::OutputChunk,
) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    conn.outstanding = conn.outstanding.saturating_sub(chunk.bytes().len() as u32);
    let written = send(
        sink,
        &ServerMessage::TerminalOutput {
            attachment_id: conn.attachment_id.clone(),
            data: base64::engine::general_purpose::STANDARD.encode(chunk.bytes()),
        },
    )
    .await;
    // Here and not before, with the write behind it — including on the failure
    // path, where the write is equally over.
    drop(chunk);
    written
}

/// The output select arm's future: the next pane chunk, or never (when no
/// terminal is attached). `None` means the stream ended — the pane closed.
///
/// The chunk that comes back counts against the carrier's stall deadline until
/// it is dropped, so the caller holds it across the write it does.
async fn next_terminal_output(
    terminal_out: &mut Option<crate::terminal::OutputDrain>,
) -> Option<crate::terminal::OutputChunk> {
    match terminal_out {
        Some(drain) => drain.recv().await,
        None => std::future::pending().await,
    }
}

/// The input-ack select arm's future: the next count of input bytes the writer
/// has handed to the tmux client, or never (when no terminal is attached). Each
/// count is turned back into replenished input credit for the phone. `None`
/// means the writer ended; the output arm handles the teardown.
async fn next_input_ack(
    terminal_acks: &mut Option<tokio::sync::mpsc::UnboundedReceiver<u32>>,
) -> Option<u32> {
    match terminal_acks {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
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
    // A live terminal is shell-equivalent authority, so it is offered only to a
    // paired per-device credential — never the static bootstrap token (the
    // `device_id` is `Some` for exactly the paired cases above) — and only over
    // a transport nothing on the path can read. The deleted SSH terminal was
    // encrypted and TOFU-pinned in every configuration; offering this one over
    // an operator's cleartext LAN would be a regression on that, whatever the
    // rest of the protocol is willing to cross it.
    let terminal_allowed = device_id.is_some() && private_transport;
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
        capabilities: capabilities(daemon, tls_active, terminal_allowed),
        device_token,
        device_id,
        device_name,
        push_environment,
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
    }
}

/// A backlog replay the connection is part-way through.
///
/// Replay used to run to completion inside the arm that asked for it, and the
/// loop polled nothing else while it did — including the arm that drains the
/// terminal's output. A backlog long enough to outlast the carrier's forgiveness
/// therefore closed a healthy phone's terminal for a stall the daemon itself had
/// caused, and lengthening the forgiveness only moved where the line was. So the
/// paging is a state the loop steps through instead: one unit of work per pass,
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
            Err("push is not configured on this Mac, so there is nowhere to register".into())
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

fn capabilities(daemon: &Arc<Daemon>, tls_active: bool, terminal_allowed: bool) -> Capabilities {
    // **One transport, named once.** `push` and `push_relay` are one-hot and
    // both are read off the mode rather than off a predicate: a phone has to
    // choose between registering a bare token and enrolling for a credential,
    // and two flags derived independently could tell it to do both or neither.
    let mode = daemon.push.mode();
    Capabilities {
        // True: an answer reaches the agent either as a hook return or as
        // keystrokes into the live prompt, and a refused injection is reported
        // rather than silently dropped.
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
        // The hook emits nothing when we are unreachable, so a dead daemon is
        // indistinguishable from no daemon.
        fail_mode: "fail_open".into(),
        answer_path: if daemon.config.hold_ms > 0 {
            AnswerPath::HookReturn
        } else {
            AnswerPath::SendKeys
        },
        hold_secs: daemon.config.hold_ms / 1000,
        send_text: true,
        capture: true,
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
        slash_composer_recovery: true,
        // A live terminal is shell-equivalent authority. Offered to a paired
        // per-device credential and never the static bootstrap token — the
        // caller decides `terminal_allowed`. The `TerminalAttach` handler
        // enforces the same rule independently, so a client that ignores this
        // capability still cannot open one.
        terminal_pty: terminal_allowed,
        // **Omitted while it would only say "Claude".** Advertising the legacy
        // floor to the phone adds a diagnostic row that means nothing yet and
        // that a shipped phone would render. It is left empty here (and so
        // skipped on the wire, keeping the ack byte-identical to minor 14); the
        // daemon still knows its real set through `supported_agents()` for the
        // IPC negotiation, and this field is populated for the phone in Phase 2
        // when it names an agent the daemon can actually drive.
        supported_agents: Vec::new(),
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
                payload: serde_json::json!({
                    "codeconnect_truncated": true,
                    "original_bytes": bytes,
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

    /// A daemon with a private store holding exactly one paired device.
    fn daemon_with_a_device() -> (Arc<Daemon>, String) {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ccd-ws-{}-{}-{}.db",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(crate::store::Store::open(&path).unwrap());
        let now = protocol::time::now_rfc3339();
        let device_id = "abcd1234".to_string();
        store
            .insert_device(&device_id, "iPhone", "not-a-real-hash", &now)
            .unwrap();

        let (transcript_tx, transcript_rx) = tokio::sync::mpsc::unbounded_channel();
        Box::leak(Box::new(transcript_rx));
        let daemon = Daemon::new(
            protocol::config::Config::default(),
            store,
            Arc::new(crate::apns::LoggingPushSender::new()),
            crate::state::Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
            transcript_tx,
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
        // its listener's descriptor for the rest of the process, which the
        // carrier-leak test in `terminal` counts and cannot tell from a leak of
        // its own.
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
        /// The file that database lives in, so a test can reach it with a
        /// connection of its own — see [`LiveServer::refuse_push_token_writes`].
        path: std::path::PathBuf,
        /// The accept loop, ended when the test that started it is over.
        ///
        /// It used to be spawned and forgotten, which leaks the listener's
        /// descriptor for the rest of the process — permanently, since nothing
        /// closes it. That is invisible until something counts descriptors, and
        /// `terminal`'s carrier-leak test does: a few of these landing inside
        /// its measurement window is an absolute count that never comes back
        /// down, and no amount of waiting fixes it because nothing here was
        /// ever going to release them.
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
        let (daemon, device_id, store, path) = daemon_with_a_device_pushing(config, push);
        // Bound here rather than inside `serve` so the test learns the port.
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let token = Arc::new("static-token-for-tests".to_string());
        let accepting = {
            let daemon = Arc::clone(&daemon);
            let token = Arc::clone(&token);
            tokio::spawn(async move {
                accept_loop(daemon, listener, token, None, PlaintextTrust::TrustedPath).await
            })
        };
        (
            LiveServer {
                addr,
                daemon,
                token,
                store,
                path,
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

        /// The same handshake, carrying an advertised feature set.
        ///
        /// A separate method rather than a parameter on [`LiveServer::hello`]: every
        /// other test here is about a phone that advertises nothing, which is every
        /// phone in the field, and threading a `None` through all of them would make
        /// the ordinary case read like the exception.
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

        /// Make the next `register_push` fail **in the store**, leaving every
        /// other query answering normally.
        ///
        /// The arm under test is the one after a registration has been accepted
        /// as well-formed and the write itself did not land, and nothing the
        /// daemon does can produce that on demand — so it is manufactured, for
        /// the same reason and on the same terms as
        /// [`crate::store::Store::break_device_lookups_for_tests`]. That one
        /// drops the table, which is too big a hammer here: the assertion this
        /// enables is a *read* of the same table afterwards, so the breakage has
        /// to be narrow enough to leave the read working.
        ///
        /// `BEFORE UPDATE OF push_token` is exactly that narrow. It aborts the
        /// one statement `Store::set_push_token` uses to claim the token — which
        /// reaches the daemon as the same `Err` a corrupt page or a full disk
        /// would — and touches nothing else: `devices.features` is written by a
        /// different statement and read by a different one again, so a clearing
        /// write on the failure path would still land, which is the whole point.
        fn refuse_push_token_writes(&self) {
            rusqlite::Connection::open(&self.path)
                .expect("test fixture")
                .execute_batch(
                    "CREATE TRIGGER refuse_push_token_writes
                       BEFORE UPDATE OF push_token ON devices
                     BEGIN
                       SELECT RAISE(ABORT, 'the devices table would not take this write');
                     END;",
                )
                .expect("test fixture");
        }
    }

    /// A `hello` at the current protocol, on the device token these tests pair with,
    /// carrying an advertised feature set. Used for both the handshake hello and the
    /// second one sent down an already-authenticated socket — the two are different
    /// handlers, and the whole point of the test below is that they behave alike.
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
    /// be bounded.
    async fn next_frame(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        within: Duration,
    ) -> Frame {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, socket.next()).await {
                Err(_) => return Frame::Silent,
                Ok(Some(Ok(Message::Text(text)))) => {
                    return Frame::Json(serde_json::from_str(&text).unwrap())
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

    fn daemon_with_a_device_pushing(
        config: protocol::config::Config,
        push: Arc<dyn crate::apns::PushSender>,
    ) -> (
        Arc<Daemon>,
        String,
        Arc<crate::store::Store>,
        std::path::PathBuf,
    ) {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ccd-ws-live-{}-{}-{}.db",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(crate::store::Store::open(&path).unwrap());
        let now = protocol::time::now_rfc3339();
        let device_id = "abcd1234".to_string();
        let device_token = "device-token-for-tests";
        store
            .insert_device(
                &device_id,
                "iPhone",
                &protocol::hash::sha256_hex(device_token.as_bytes()),
                &now,
            )
            .unwrap();
        let (transcript_tx, transcript_rx) = tokio::sync::mpsc::unbounded_channel();
        Box::leak(Box::new(transcript_rx));
        let daemon = Daemon::new(
            config,
            Arc::clone(&store),
            push,
            crate::state::Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
            transcript_tx,
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
            server
                .store
                .push_targets(crate::state::feature_epoch())
                .unwrap()
                .is_empty(),
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
        let stored = server
            .store
            .push_targets(crate::state::feature_epoch())
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].device_id, device);
        assert_eq!(stored[0].token, A_TOKEN);
        assert_eq!(stored[0].environment, "production");
        assert_eq!(
            stored[0].credential.as_ref().map(|c| c.expose()),
            Some("a-relay-bearer")
        );
    }

    /// **An advertised feature set is wire-legal, accepted, and ignored.**
    ///
    /// `features` is still a field on `register_push`, and the daemon used to store
    /// it beside the token under this run's epoch. No shipping client can put
    /// anything in it, so that write was machinery for an input the wire cannot
    /// produce and it is gone — but the field itself stays legal, because refusing
    /// a frame for carrying it would break the phone that eventually sends one.
    ///
    /// Both halves are the same rule stated twice: the registration succeeds, and
    /// the row it wrote is at the Claude floor — the same row a phone that named
    /// nothing leaves, because the column is never written at all.
    ///
    /// **Mutation:** make the handler refuse or persist an advertised set and one
    /// of the two assertions fails.
    #[tokio::test]
    async fn an_advertised_feature_set_is_accepted_and_changes_nothing() {
        let codex = protocol::agent::AgentKind::Codex;
        let (server, _device) = server_in(crate::apns::PushMode::Direct).await;
        let (mut socket, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;

        let mut advertising = registration(A_TOKEN, None);
        advertising["features"] = serde_json::json!({"agents": ["claude", "codex"]});
        assert!(
            register(&mut socket, advertising).await.is_none(),
            "a frame carrying a feature set is a good registration, not an error"
        );
        let stored = server
            .store
            .push_targets(crate::state::feature_epoch())
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].token, A_TOKEN);
        assert!(
            !stored[0].features.supports(&codex),
            "nothing wrote the advertisement down, so the row is the floor a device \
             that said nothing would have"
        );
        assert!(
            stored[0]
                .features
                .supports(&protocol::agent::AgentKind::Claude),
            "and the floor is Claude, not silence: an empty column is the row every \
             phone predating the field has"
        );
    }

    /// The device's stored feature column, read twice: once under this run's epoch
    /// and once under another's.
    ///
    /// Two reads because they pin different bytes. The first decodes the JSON, so it
    /// pins what the device said; the second is `Unconfirmable` **only while a set is
    /// stored at all**, so it pins that the column is non-`NULL` and that the epoch
    /// stamp beside it is this run's. A write that cleared either would move one of
    /// them, and `NULL` — the shape a clearing write leaves — moves both to the
    /// Claude floor at once.
    fn stored_features(
        server: &LiveServer,
    ) -> (crate::store::DeviceFeatures, crate::store::DeviceFeatures) {
        let mine = server
            .store
            .push_targets(crate::state::feature_epoch())
            .unwrap();
        let foreign = server.store.push_targets("some-other-daemon-run").unwrap();
        assert_eq!(mine.len(), 1, "one registered device");
        assert_eq!(foreign.len(), 1);
        (mine[0].features.clone(), foreign[0].features.clone())
    }

    /// **AN INERT FIELD LEAVES A PRE-EXISTING SET EXACTLY AS IT FOUND IT** (round-9 F7).
    ///
    /// The sibling test above starts from a fresh `NULL` row, and `NULL` is what a
    /// clearing write produces — so "nothing was written" and "the column was wiped"
    /// are the same observation there, and restoring `set_device_features(.., None,
    /// ..)` to either handler passes it unchanged while erasing whatever the device
    /// had said and handing a `[Codex]`-only phone the Claude doorbells its own claim
    /// excluded. That is not a hypothetical shape: it is precisely the state
    /// [`crate::store::DeviceFeatures::Unconfirmable`] exists for, and the read side
    /// is already built to tell it from the floor.
    ///
    /// So this preseeds a real set and asserts it still reads back as the same set,
    /// under the same epoch stamp, after every path that carries a `features` field.
    /// Both reads decode rather than compare raw column bytes — that is what the
    /// push projection itself does, so it is the granularity the behaviour lives at
    /// — and between them they pin the column as populated, as this run's, and as
    /// saying Codex, which is every property a clearing write would move:
    ///
    ///   * the **handshake** `hello`, which is a different handler from
    ///   * a **second** `hello` down an already-authenticated socket;
    ///   * an **accepted** `register_push`;
    ///   * a **refused** one — a relay daemon's registration with no credential, so
    ///     the frame is rejected after the feature field has been read;
    ///   * and one whose **store write failed** — accepted as well-formed, then not
    ///     written. That is a *different arm* from the refusal: the refusal returns
    ///     during credential validation and never reaches `register_push` at all, so
    ///     a leg that only exercises it leaves the failure arm below unwitnessed and
    ///     a clearing write there survives every other assertion here.
    ///
    /// **Mutation:** call `set_device_features(device_id, None, feature_epoch())` from
    /// any one of those five sites and its assertion fails: the confirmed `[Codex]`
    /// set becomes the empty legacy set, which is the Claude floor under both reads.
    #[tokio::test]
    async fn an_advertised_feature_set_leaves_a_stored_one_untouched_on_every_path() {
        let codex = protocol::agent::AgentKind::Codex;
        let claude = protocol::agent::AgentKind::Claude;
        let advertising = serde_json::json!({"agents": ["claude", "codex"]});
        let (server, device) = server_in(crate::apns::PushMode::Relay).await;

        // A registered phone whose column holds a confirmed `[Codex]` set. Written
        // through the store rather than the wire because nothing on the wire can
        // write it — that is the whole subject of this test — and `push_targets`
        // only reports rows that carry a token, so the registration comes first.
        let (mut socket, _) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert!(
            register(&mut socket, registration(A_TOKEN, Some("a-relay-bearer")))
                .await
                .is_none(),
            "the premise: this device has a push token, or it is in no fan-out at all"
        );
        server
            .store
            .set_device_features(
                &device,
                Some(r#"{"agents":["codex"]}"#),
                crate::state::feature_epoch(),
            )
            .unwrap();

        let seeded = stored_features(&server);
        assert_eq!(
            seeded.0,
            crate::store::DeviceFeatures::Advertised(protocol::ws::ClientFeatures {
                agents: vec![codex.clone()]
            }),
            "the premise: this row says Codex and only Codex"
        );
        assert_eq!(
            seeded.1,
            crate::store::DeviceFeatures::Unconfirmable,
            "the premise: a set really is stored — an empty column reads as the Claude \
             floor under every epoch, and could not tell a clearing write apart"
        );
        assert!(
            !seeded.0.supports(&claude),
            "the premise that gives the mutation teeth: this phone has said it cannot \
             render a Claude alert, so a write that broadened it back to the floor \
             would start sending it ones"
        );

        // The handshake hello, advertising something else entirely.
        let (mut socket, _) = server
            .hello_advertising("device-token-for-tests", advertising.clone())
            .await;
        assert_eq!(
            stored_features(&server),
            seeded,
            "the first hello reads the field and drops it; it does not get to rewrite \
             what this device is already on record as saying"
        );

        // A second hello, down the socket that is already authenticated. Different
        // handler, same rule — and the phone is answered rather than dropped, which
        // is how the test knows the frame was processed at all.
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
            "the re-hello was consumed before the ping, so what follows is a claim \
             about a frame the daemon really handled"
        );
        assert_eq!(
            stored_features(&server),
            seeded,
            "a second hello does not re-authenticate and does not re-advertise either"
        );

        // An ACCEPTED registration carrying a feature set.
        let mut accepted = registration(A_TOKEN, Some("a-relay-bearer"));
        accepted["features"] = advertising.clone();
        assert!(
            register(&mut socket, accepted).await.is_none(),
            "the premise: this registration was accepted"
        );
        assert_eq!(
            stored_features(&server),
            seeded,
            "the token, its environment and its credential commit as one tuple; the \
             feature set is not part of it and does not travel with it"
        );

        // And a REFUSED one — a relay daemon with no credential to present. The
        // field is read before the refusal, so this is the path where a write would
        // be worst: a frame that mutated nothing else still erasing this column.
        let mut refused = registration(A_TOKEN, None);
        refused["features"] = advertising.clone();
        let error = register(&mut socket, refused)
            .await
            .expect("a refusal the phone can act on");
        assert_eq!(
            error["code"], "push_registration_failed",
            "the premise: this registration was refused, not accepted"
        );
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("relay credential"),
            "and refused *during validation* — the code is shared with the storage \
             failure below, so the message is what says which arm ran: {error}"
        );
        assert_eq!(
            stored_features(&server),
            seeded,
            "a refused frame mutates nothing, and this column is nothing's most \
             valuable case: it is the only record of what this phone can open"
        );

        // And one that was accepted and then FAILED TO STORE. The leg above
        // returns while validating the credential and never calls
        // `Daemon::register_push`; this one gets past validation with the bearer
        // the accepted leg used, and dies in the write. It is its own arm, with
        // its own handling, and nothing above reaches it.
        server.refuse_push_token_writes();
        let mut unstorable = registration(A_TOKEN, Some("a-relay-bearer"));
        unstorable["features"] = advertising;
        let error = register(&mut socket, unstorable)
            .await
            .expect("a failed store is reported, not swallowed");
        assert_eq!(
            error["code"], "push_registration_failed",
            "the premise: this registration failed: {error}"
        );
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .starts_with("could not store the push token"),
            "the premise that makes this leg distinct: validation passed and the \
             *write* is what failed, so this is the post-validation arm: {error}"
        );
        assert_eq!(
            stored_features(&server),
            seeded,
            "a registration whose write failed has even less licence to rewrite this \
             column than one that was refused outright: the daemon just demonstrated \
             it could not write, and the only column it would still reach is the one \
             holding what this phone said it can open"
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
        let stored = server
            .store
            .push_targets(crate::state::feature_epoch())
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].credential, None);

        assert!(
            register(&mut socket, registration(A_TOKEN, Some("offered-anyway")))
                .await
                .is_none()
        );
        let stored = server
            .store
            .push_targets(crate::state::feature_epoch())
            .unwrap();
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
        assert!(server
            .store
            .push_targets(crate::state::feature_epoch())
            .unwrap()
            .is_empty());
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
            assert!(server
                .store
                .push_targets(crate::state::feature_epoch())
                .unwrap()
                .is_empty());
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
        assert!(server
            .store
            .push_targets(crate::state::feature_epoch())
            .unwrap()
            .is_empty());
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
        // ceiling — and the *local* IPC socket, which carries the hook path and
        // every supervisor link, lives on the same budget.
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
        assert_eq!(decoded["event"]["payload"]["codeconnect_truncated"], true);
        assert_eq!(decoded["event"]["payload"]["original_bytes"], raw.len());
    }

    #[test]
    fn an_oversized_reply_that_is_not_an_event_says_so() {
        let message = ServerMessage::CaptureResult {
            session_id: "cc-1".into(),
            text: "y".repeat(2 * MAX_CLIENT_MESSAGE_BYTES),
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
        let (text, truncated) = fit_diff_in_a_frame(nasty, true);
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
        let (text, truncated) = fit_diff_in_a_frame(diff.clone(), false);
        assert_eq!(text, diff, "a small diff must not be rewritten");
        assert!(!truncated);
        // An already-truncated small diff keeps saying so.
        assert!(fit_diff_in_a_frame(diff, true).1);
    }

    #[test]
    fn an_empty_diff_survives_the_fitting() {
        let (text, truncated) = fit_diff_in_a_frame(String::new(), false);
        assert_eq!(text, "");
        assert!(!truncated);
    }

    #[test]
    fn multibyte_content_is_never_split_mid_character() {
        // Emoji are 4 bytes and escape to 12; the shrink loop must still land
        // on a character boundary or `String::truncate` panics.
        let nasty = "🔥".repeat(protocol::ws::MAX_DIFF_BYTES / 4);
        let (text, _) = fit_diff_in_a_frame(nasty, true);
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
    /// timing. `append_batch_with_cursor` refuses a session it cannot find, so the
    /// row goes in first and a zero count still proves the seeding worked.
    fn seed_backlog(daemon: &Arc<Daemon>, events: usize) -> String {
        use protocol::event::{Lifecycle, PendingEvent};

        let uid = protocol::uid::new().expect("mint a uid");
        let now = protocol::time::now_rfc3339();
        let written = daemon
            .store
            .upsert_session(&crate::store::SessionRow {
                session_uid: uid.clone(),
                session_id: "cc-backlog".into(),
                tmux_session: "cc-backlog".into(),
                tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
                cwd: "/tmp".into(),
                claude_session_id: None,
                transcript_path: None,
                lifecycle: Lifecycle::Live,
                created_at: now.clone(),
                updated_at: now.clone(),
                agent: protocol::agent::AgentKind::Claude,
                codex_thread_id: None,
                codex_socket: None,
            })
            .expect("the session row is written");
        // A freshly minted uid cannot be tombstoned, but the upsert says so
        // rather than assuming it: a batch append against a session that is not
        // there writes nothing at all and would leave the count below to explain
        // it.
        assert!(matches!(written, crate::store::SessionUpsert::Present));
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
        let cursor = crate::store::TailCursor {
            path: "/dev/null".into(),
            dev: 0,
            ino: 0,
            offset: 0,
            last_line_start: 0,
            last_line_sha: String::new(),
        };
        let written = daemon
            .store
            .append_batch_with_cursor(&uid, &pendings, &cursor)
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
        let _serial = crate::terminal::fixture_test_guard().await;
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

    /// How many facts the replay test seeds.
    ///
    /// Chosen by measurement, not by arithmetic: what matters is that paging them
    /// out takes longer in wall clock than a whole terminal attachment's peer
    /// budget, and how long one event takes to reach a phone is a property of
    /// this machine's socket and JSON rather than of anything stated here.
    /// Measured at 3.9 seconds against a budget of 2 — near twice over, which is
    /// the margin a faster machine is bought with. The test asserts the wall time
    /// it actually got and says to raise this if the margin ever runs out,
    /// because a replay that finished quickly would prove nothing at all.
    ///
    /// The two are not the same currency and the comparison does not claim they
    /// are. The budget is *charged* time, and a replay holds no terminal write,
    /// so it is charged nothing however long it runs. Outlasting the budget in
    /// wall clock is a proxy for "the backlog is long" — long enough that the
    /// per-call ceiling this replaces would have closed the phone.
    const REPLAY_TEST_EVENTS: usize = 120_000;

    /// A long replay does not cost a healthy phone its terminal.
    ///
    /// This is the whole of finding #3, end to end and against nothing stubbed:
    /// a real socket, a real tmux pane, a real store. The replay used to run to
    /// completion inside the arm that asked for it, so while it ran the loop
    /// polled nothing else — including the arm that drains the carrier's output.
    /// The carrier's reader then parked with its queue full, and the per-call
    /// ceiling turned that into `slow_consumer` on a phone that had done nothing
    /// but credit every byte it was sent.
    ///
    /// Four things are asserted and each rules out a different way of passing
    /// vacuously:
    ///
    /// - The replay really did outlast, in wall clock, the whole of an
    ///   attachment's peer budget. A backlog that pages out inside it proves
    ///   nothing, because the old code survived those too; if this ever fails,
    ///   the backlog has to grow. It stands in for length and is not a charge the
    ///   replay could have spent — see [`REPLAY_TEST_EVENTS`].
    /// - A pane chunk reached the phone *between* the first replayed event and
    ///   the last, which is the loop having kept draining rather than having
    ///   drained before and after.
    /// - No `terminal_closed` arrived, which is the defect itself.
    /// - The whole backlog arrived, in seq order with no gap — because a replay
    ///   that yields to other arms must not lose its place, and a test that only
    ///   checked the terminal survived would pass on a replay that silently
    ///   dropped half the log.
    ///
    /// The pane ticks rather than being quiet, so there is something for the
    /// output arm to carry throughout; the stall deadline is shortened so the
    /// budget the replay has to outlast is seconds rather than minutes.
    #[tokio::test]
    async fn a_long_replay_never_closes_a_healthy_phones_terminal() {
        // The shortened stall deadline is process-wide, and so is the fixture
        // socket every attach resolves against.
        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start_running(
            "while :; do echo TICK; sleep 0.05; done",
        ) else {
            eprintln!("skipped: no tmux");
            return;
        };
        let stall = Duration::from_millis(250);
        let _deadline = crate::terminal::StallDeadline::shortened_to(stall);
        let charged_budget = stall * crate::terminal::PEER_STALL_BUDGET_DEADLINES;
        let _socket = FixtureSocket::set(&fx.sock);
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let backlog_uid = seed_backlog(&server.daemon, REPLAY_TEST_EVENTS);
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");

        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "terminal_attach",
                    "attachment_id": "att-replay",
                    "session_uid": fx.uid,
                    "cols": 80,
                    "rows": 24,
                    "output_credit": protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                })
                .to_string(),
            ))
            .await
            .unwrap();

        // An honest phone: every pane chunk is credited the moment it arrives, so
        // nothing that happens below can be the phone withholding credit.
        async fn credit(socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>, bytes: usize) {
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "type": "terminal_credit",
                        "attachment_id": "att-replay",
                        "bytes": bytes,
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
        }

        // Streaming before the subscribe, so the pause the replay causes is a
        // pause in something that was already moving.
        let mut attached = false;
        let mut streaming = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline && !(attached && streaming) {
            let Some(message) = next_json(&mut socket).await else {
                break;
            };
            match message["type"].as_str() {
                Some("terminal_attached") => attached = true,
                Some("terminal_output") => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(message["data"].as_str().expect("output carries data"))
                        .expect("output is base64");
                    credit(&mut socket, bytes.len()).await;
                    streaming = true;
                }
                Some("terminal_closed") => {
                    panic!("the terminal closed before the replay: {message}")
                }
                _ => {}
            }
        }
        assert!(
            attached && streaming,
            "the terminal must be streaming first"
        );

        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "subscribe",
                    "session_id": backlog_uid,
                    "after_seq": 0,
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let mut seqs: Vec<u64> = Vec::with_capacity(REPLAY_TEST_EVENTS);
        let mut first_event_at: Option<std::time::Instant> = None;
        let mut last_event_at = std::time::Instant::now();
        let mut chunks_during_replay = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while tokio::time::Instant::now() < deadline {
            let Some(message) = next_json(&mut socket).await else {
                break;
            };
            match message["type"].as_str() {
                Some("event") => {
                    first_event_at.get_or_insert_with(std::time::Instant::now);
                    last_event_at = std::time::Instant::now();
                    seqs.push(
                        message["event"]["seq"]
                            .as_u64()
                            .expect("an event carries a seq"),
                    );
                    if seqs.len() == REPLAY_TEST_EVENTS {
                        break;
                    }
                }
                Some("terminal_output") => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(message["data"].as_str().expect("output carries data"))
                        .expect("output is base64");
                    credit(&mut socket, bytes.len()).await;
                    if first_event_at.is_some() {
                        chunks_during_replay += 1;
                    }
                }
                Some("terminal_closed") => panic!(
                    "the phone credited every byte it was sent and still lost its \
                     terminal to the daemon's own replay: {message}"
                ),
                _ => {}
            }
        }

        let replay_took = last_event_at
            .saturating_duration_since(first_event_at.expect("the replay sent something"));
        assert!(
            replay_took >= charged_budget,
            "the replay took {replay_took:?} of wall clock, inside the \
             attachment's charged budget of {charged_budget:?} — the replay holds \
             no terminal write and so spends none of that budget, which makes \
             this only a proxy for a backlog long enough to matter, and it is not \
             one yet: raise REPLAY_TEST_EVENTS"
        );
        assert!(
            chunks_during_replay > 0,
            "no pane output reached the phone between the first replayed event \
             and the last, so the loop was not draining the carrier while it \
             replayed"
        );
        assert_eq!(
            seqs.len(),
            REPLAY_TEST_EVENTS,
            "the replay lost its place: {} of {REPLAY_TEST_EVENTS} events arrived",
            seqs.len()
        );
        assert!(
            seqs.iter().copied().eq(1..=REPLAY_TEST_EVENTS as u64),
            "the backlog must arrive in seq order with no gap"
        );
        assert!(fx.alive(), "the session outlives the connection");
    }

    // -------------------------------------------------------------- terminal

    /// A sink that decodes and records every server message, so a test can drive
    /// the terminal path exactly as `handle_client` does and read back what the
    /// phone would see.
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
    /// observers are every test whose own writes must be allowed to take longer
    /// than 200ms: `a_pane_chunk_stays_charged_until_its_write_returns`, which
    /// parks a frame at a gated sink and asserts the write succeeds, and the two
    /// that push tens of thousands of frames over a real socket,
    /// `a_long_replay_never_closes_a_healthy_phones_terminal` and
    /// `a_gap_is_not_announced_while_that_sessions_replay_is_pending`. All four
    /// take `crate::terminal::fixture_test_guard`, the suite-wide serial guard
    /// this file already uses for the tmux tests.
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

    /// A sink whose flush parks until a test lets it through, so a test can act
    /// while a frame is genuinely in flight — the window the whole delivery
    /// accounting turns on, and the one a sink that completes immediately can
    /// never open.
    #[derive(Clone, Default)]
    struct GatedSink(Arc<GateState>);

    #[derive(Default)]
    struct GateState {
        /// Set once the flush has been polled, so a test knows the write is
        /// under way rather than hoping it is.
        reached: std::sync::atomic::AtomicBool,
        open: std::sync::atomic::AtomicBool,
        waker: std::sync::Mutex<Option<std::task::Waker>>,
    }

    impl GatedSink {
        /// Wait until a frame is actually in flight. Bounded, so a test that
        /// never writes fails its own assertion rather than hanging.
        async fn writing(&self) {
            for _ in 0..200 {
                if self.0.reached.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("no frame ever reached the sink");
        }

        fn release(&self) {
            self.0.open.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(waker) = self.0.waker.lock().unwrap().take() {
                waker.wake();
            }
        }
    }

    impl futures_util::Sink<Message> for GatedSink {
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
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            self.0
                .reached
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if self.0.open.load(std::sync::atomic::Ordering::SeqCst) {
                return std::task::Poll::Ready(Ok(()));
            }
            *self.0.waker.lock().unwrap() = Some(cx.waker().clone());
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
        // The shortened deadline is process-wide, so this takes the suite-wide
        // serial guard: without it the 200ms below reaches whatever else is
        // running, and `a_pane_chunk_stays_charged_until_its_write_returns` would
        // fail its own write for a deadline it never asked for.
        let _serial = crate::terminal::fixture_test_guard().await;
        let deadline = Duration::from_millis(200);
        let _shortened = WriteDeadline::shortened_to(deadline);
        let mut sink = GatedSink::default();
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

    /// A pane chunk stays charged against the carrier's stall deadline for the
    /// whole of its write, and is settled only once that write returns.
    ///
    /// This is the production drop point, and it is the entire point of the
    /// delivery accounting: the queue reads empty the moment a chunk is dequeued,
    /// so a carrier that settled there would see nothing outstanding while a
    /// frame was still in flight, expire its credit deadline, and close a phone
    /// that had been given nothing as a slow consumer. The gated sink is what
    /// makes that window real rather than instantaneous — against an ordinary
    /// sink the write completes inside the same poll and the ordering is
    /// unobservable, which is exactly why this went uncovered.
    #[tokio::test]
    async fn a_pane_chunk_stays_charged_until_its_write_returns() {
        use protocol::ws::TERMINAL_INITIAL_INPUT_CREDIT;

        // The other side of `WriteDeadline`'s bargain: this test parks a write at
        // a gated sink and asserts it succeeds, so it is the one a leaked 200ms
        // override would break. Taking the same guard is what keeps the two
        // apart.
        let _serial = crate::terminal::fixture_test_guard().await;

        let (tx, mut rx) = crate::terminal::output_channel(4);
        assert!(
            tx.hand_over_for_test(b"pane bytes".to_vec()).await,
            "the queue takes the chunk"
        );
        assert_eq!(rx.undelivered_count(), 1, "queued output is owed");

        let chunk = rx.recv().await.expect("the chunk the carrier handed over");
        assert_eq!(
            rx.undelivered_count(),
            1,
            "taking a chunk off the queue is not delivering it"
        );

        let mut conn = TerminalConn {
            attachment_id: "att-1".into(),
            handle: crate::terminal::TerminalHandle::inert_stub(),
            output_credit: Arc::new(tokio::sync::Semaphore::new(0)),
            outstanding: 64,
            input_credit: TERMINAL_INITIAL_INPUT_CREDIT,
        };
        let sink = GatedSink::default();
        let writing = {
            let mut sink = sink.clone();
            tokio::spawn(async move {
                let written = write_terminal_chunk(&mut sink, &mut conn, chunk).await;
                (written.is_ok(), conn.outstanding)
            })
        };

        // Mid-write: the frame is in the sink and has not been flushed.
        sink.writing().await;
        assert_eq!(
            rx.undelivered_count(),
            1,
            "a chunk whose write has not returned must still be owed"
        );

        sink.release();
        let (ok, outstanding) = writing.await.expect("the write task finishes");
        assert!(ok, "the write succeeds");
        assert_eq!(
            rx.undelivered_count(),
            0,
            "and is settled once the write returns"
        );
        // The credit ledger is settled by the same call, and by the byte count.
        assert_eq!(outstanding, 64 - "pane bytes".len() as u32);
    }

    /// Shortens the attach deadline for one test and restores the production
    /// value on drop, so a test that panics part-way cannot leave every attach
    /// behind it running against a fraction of a second.
    ///
    /// **The restore is not enough on its own**, and neither is the setter
    /// taking a guard — the whole rule is [`WriteDeadline`]'s: a process-wide
    /// override is exclusive only when the test that sets it and every test that
    /// could observe it both take the same guard.
    ///
    /// The setter is `an_attach_that_does_not_finish_in_time_is_abandoned_and_reported`.
    /// The observers are every test that drives a real
    /// `TerminalHandle::open`: the tmux-fixture ones, which take
    /// `crate::terminal::fixture_test_guard` for the fixture's own sake, and
    /// `a_failed_attach_is_reported_with_its_own_close_code`, which has no
    /// fixture and takes it purely for this. A test refused before anything is
    /// spawned never reaches the deadline and needs no guard —
    /// `a_duplicate_attach_closes_the_existing_terminal` and
    /// `the_wiring_refuses_bad_terminal_requests` both assert
    /// `conn.opening.is_none()`, which is what makes that checkable rather than
    /// assumed.
    struct AttachDeadline;

    impl AttachDeadline {
        fn shortened_to(deadline: Duration) -> AttachDeadline {
            TEST_ATTACH_DEADLINE_MS.store(
                deadline.as_millis() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            AttachDeadline
        }
    }

    impl Drop for AttachDeadline {
        fn drop(&mut self) {
            TEST_ATTACH_DEADLINE_MS.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// An attach that never finishes is given up on, and the phone is told which
    /// failure that was.
    ///
    /// A `TerminalHandle::open` that never returns is an attachment id the phone
    /// is never told about, a queue of frames behind it that is never drained,
    /// and a session lease held by nothing — for ever. `ATTACH_DEADLINE` is the
    /// only thing standing between those states and nothing here witnessed it.
    ///
    /// Driven by holding the open at its own first instruction, which is a wedge
    /// no timing can produce on demand, and against a socket no tmux server is
    /// on — so an attach that got past the hold would fail for a *different*
    /// reason, which is what makes the reason assertion below load-bearing rather
    /// than decorative. Removing the timeout leaves it sitting at the gate until
    /// the gate's own bound releases it, and it then fails as
    /// `session_not_hosted`.
    #[tokio::test]
    async fn an_attach_that_does_not_finish_in_time_is_abandoned_and_reported() {
        // Both the deadline override and the open hold are process-wide.
        let _serial = crate::terminal::fixture_test_guard().await;
        let _deadline = AttachDeadline::shortened_to(Duration::from_millis(300));
        let held = crate::terminal::OpenHold::close();
        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals::default();
        let mut sink = CollectSink::default();

        attach(
            &daemon,
            &mut conn,
            &mut sink,
            "cc-no-server-here",
            Some(&device),
            attach_msg(
                "att-1",
                &some_uid(),
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        drop(held);

        let ServerMessage::TerminalClosed {
            attachment_id,
            code,
            reason,
        } = sink.last()
        else {
            panic!(
                "the abandoned attach must be answered, got {:?}",
                sink.last()
            );
        };
        assert_eq!(attachment_id, "att-1");
        assert_eq!(code, protocol::ws::terminal_close::TMUX_UNAVAILABLE);
        assert!(
            reason.contains("did not complete in time"),
            "the phone must be told the attach ran out of time rather than that \
             the session was not there, got {code} ({reason})"
        );
        assert!(
            conn.terminal.is_none() && conn.opening.is_none(),
            "nothing is installed, and nothing is left opening"
        );
    }

    /// Dropping an [`OpenTask`] aborts the task that was opening the attach.
    ///
    /// The previous round judged this untestable, and that judgement was about
    /// the *carrier*: an aborted open and a completed-but-dropped one converge on
    /// the same teardown, so no fact about the tmux client can tell them apart.
    /// The *task* is a different subject and is witnessed directly — a future
    /// holding a value whose `Drop` sets a flag, parked on `pending()` for ever.
    /// A task that is never aborted never drops that future, so the flag never
    /// fires and this fails on its own assertion rather than on a timeout of
    /// something else.
    ///
    /// What it proves is that the drop aborts a task that had not finished, and
    /// that the abort releases what that task was holding.
    ///
    /// What it does **not** prove, and what no test here should be read as
    /// claiming: that the abort stops the open between any two particular
    /// instructions. The task witnessed here is parked on `pending()`, so the
    /// abort finds it at a yield point and takes it there. `abort` is
    /// cooperative — an open being polled on another worker runs to *its* next
    /// yield point, and lease acquisition and the supersede that goes with it
    /// can happen before that. The guarantee is the one asserted below: the task
    /// is aborted and its resources are dropped. Why that is enough, rather than
    /// why it is prompt, is the convergence argument on [`OpenTask`] itself, and
    /// it is an argument rather than a test for the reason above.
    #[tokio::test]
    async fn dropping_an_attach_in_flight_aborts_the_task_opening_it() {
        use std::sync::atomic::{AtomicBool, Ordering};

        /// Sets the flag when the future holding it is dropped, which for a
        /// parked task happens only on abort.
        struct Witness(Arc<AtomicBool>);

        impl Drop for Witness {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let task = {
            let witness = Witness(Arc::clone(&dropped));
            tokio::spawn(async move {
                let _held = witness;
                std::future::pending::<()>().await;
                unreachable!("the parked future never completes on its own")
            })
        };
        let opening = OpenTask(task);
        // Given a poll before the drop, so what the drop ends is a live task
        // parked at a yield point rather than one that had already finished.
        tokio::task::yield_now().await;
        assert!(
            !dropped.load(Ordering::SeqCst),
            "the future must still be alive, or the drop below proves nothing"
        );

        drop(opening);
        let mut aborted = false;
        for _ in 0..200 {
            if dropped.load(Ordering::SeqCst) {
                aborted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            aborted,
            "dropping an attach in flight left its task parked, so a connection \
             that goes away mid-attach leaks the open until tmux answers"
        );
    }

    /// [`abandon_terminal`] gives up the attach that was still opening, not only
    /// the terminal — and gives it up by *dropping* it, which is what aborts the
    /// open.
    ///
    /// The two used to be separate lines in two arms, and one arm had only one
    /// of them: the per-message revocation check tore down the terminal and left
    /// the attach opening behind the close it sent. Witnessed the way
    /// `dropping_an_attach_in_flight_aborts_the_task_opening_it` witnesses it —
    /// a parked task holding a value whose `Drop` sets a flag — so this fails on
    /// its own assertion if the `Opening` is merely cleared from the connection
    /// while its task runs on.
    #[tokio::test]
    async fn abandoning_a_terminal_abandons_the_attach_still_opening_with_it() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Witness(Arc<AtomicBool>);

        impl Drop for Witness {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let task = {
            let witness = Witness(Arc::clone(&dropped));
            tokio::spawn(async move {
                let _held = witness;
                std::future::pending::<()>().await;
                unreachable!("the parked future never completes on its own")
            })
        };
        let mut terminal: Option<TerminalConn> = None;
        let mut opening = Some(Opening {
            attachment_id: "att-1".into(),
            session_uid: some_uid(),
            cols: 80,
            rows: 24,
            task: OpenTask(task),
            output_credit: Arc::new(tokio::sync::Semaphore::new(0)),
            granted: 0,
            out: crate::terminal::output_channel(4).1,
            acks: tokio::sync::mpsc::unbounded_channel().1,
            deferred: VecDeque::new(),
            deferred_bytes: 0,
            deferred_input: 0,
        });
        // Given a poll before the abandonment, so what it ends is a live task
        // parked at a yield point. As above, that is what makes the abort
        // observable here — not a claim that an open mid-poll would stop where
        // this one does.
        tokio::task::yield_now().await;
        assert!(
            !dropped.load(Ordering::SeqCst),
            "the open must still be running, or abandoning it proves nothing"
        );

        abandon_terminal(&mut terminal, &mut opening);
        assert!(opening.is_none(), "the connection has no attach in flight");
        let mut aborted = false;
        for _ in 0..200 {
            if dropped.load(Ordering::SeqCst) {
                aborted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            aborted,
            "the attach was cleared from the connection but its open kept \
             running — free to take the session's lease and spawn tmux for a \
             device that has just been cut off"
        );
    }

    /// A displaced attach is dropped *before* the courtesy write that answers
    /// it, never held across it.
    ///
    /// The write is bounded, not instant: a phone that has stopped reading its
    /// socket holds it for the whole write deadline. An `Opening` alive through
    /// that window is an open still running behind a connection that has already
    /// decided it has none — and this open would take the session's daemon-wide
    /// lease (which `TerminalHandle::open` does *before* it resolves or spawns),
    /// supersede whatever held it, and start a tmux client for an attach the
    /// phone has been told is superseded.
    ///
    /// So: hold the first attach at its first instruction, displace it, stall
    /// the courtesy write, and let the gate through. The lease staying free is
    /// the assertion — it is the first thing the surviving open would take.
    #[tokio::test]
    async fn a_displaced_attach_is_aborted_before_the_frame_that_answers_it() {
        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals::default();
        let sink = GatedSink::default();

        let held = crate::terminal::OpenHold::close();
        handle_terminal(
            &daemon,
            &mut conn.terminal,
            &mut conn.out,
            &mut conn.acks,
            &mut conn.opening,
            &mut sink.clone(),
            &fx.sock,
            Some(&device),
            true,
            attach_msg(
                "att-1",
                &fx.uid,
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await
        .expect("a gated sink takes the attach, which writes nothing");
        held.reached().await;

        // The displacing attach runs on its own task, so the test can act while
        // its `terminal_closed` for `att-1` is genuinely in flight. It names a
        // different session, so nothing but the displaced open can touch
        // `fx.uid`'s lease.
        let displacing = {
            let daemon = Arc::clone(&daemon);
            let device = device.clone();
            let socket = fx.sock.clone();
            let mut sink = sink.clone();
            tokio::spawn(async move {
                handle_terminal(
                    &daemon,
                    &mut conn.terminal,
                    &mut conn.out,
                    &mut conn.acks,
                    &mut conn.opening,
                    &mut sink,
                    &socket,
                    Some(&device),
                    true,
                    attach_msg(
                        "att-2",
                        &some_uid(),
                        protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                    ),
                )
                .await
                .expect("a gated sink cannot fail");
                conn
            })
        };
        sink.writing().await;
        // Everything the displaced open needs is now available to it: the gate
        // is open and the fixture is a real session it would attach to.
        drop(held);

        let mut leaked = false;
        for _ in 0..200 {
            if daemon.terminal_leases.held(&fx.uid) {
                leaked = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !leaked,
            "an attach answered as superseded went on to take its session's \
             lease and spawn a tmux client, behind a connection that believes \
             it has no attach in flight"
        );

        sink.release();
        let mut conn = displacing.await.expect("the displacing attach finishes");
        assert!(
            conn.opening
                .as_ref()
                .is_some_and(|pending| pending.attachment_id == "att-2"),
            "and the newer attach is the one in flight"
        );
        abandon_terminal(&mut conn.terminal, &mut conn.opening);
    }

    /// Points every attach in the process at a fixture's tmux socket, and puts
    /// the daemon's own back on drop — so a panicking test cannot leave the next
    /// one attaching to a socket that is not there.
    struct FixtureSocket;

    impl FixtureSocket {
        fn set(socket: &str) -> FixtureSocket {
            *TEST_TMUX_SOCKET.lock().unwrap_or_else(|p| p.into_inner()) = Some(socket.to_string());
            FixtureSocket
        }
    }

    impl Drop for FixtureSocket {
        fn drop(&mut self) {
            *TEST_TMUX_SOCKET.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
    }

    /// The connection's own output arm carries pane bytes to the phone.
    ///
    /// Every other terminal test here drives `handle_terminal` and the receivers
    /// directly, which is enough to prove the ledgers and nothing at all about
    /// the select loop: measured, an unconditional log placed as the output arm's
    /// first statement was never once reached by the suite, so the arm that
    /// carries every pane byte — and with it the delivery accounting finding #3
    /// turns on — was reachable only by reading it. This drives the real loop
    /// over a real socket against a real tmux session, and the marker it waits
    /// for can arrive no other way.
    #[tokio::test]
    async fn the_connections_own_loop_carries_pane_output_to_the_phone() {
        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let _socket = FixtureSocket::set(&fx.sock);
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");

        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "terminal_attach",
                    "attachment_id": "att-loop",
                    "session_uid": fx.uid,
                    "cols": 80,
                    "rows": 24,
                    "output_credit": protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                })
                .to_string(),
            ))
            .await
            .unwrap();

        // Type a marker and read frames until the pane's echo of it comes back
        // as `terminal_output`, crediting as a phone does. Only the output arm
        // sends that frame, so its arrival is the arm having run.
        let mut typed = false;
        let mut carried = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < deadline {
            let Some(message) = next_json(&mut socket).await else {
                break;
            };
            match message["type"].as_str() {
                Some("terminal_attached") => {
                    assert_eq!(message["attachment_id"], "att-loop");
                    socket
                        .send(Message::Text(
                            serde_json::json!({
                                "type": "terminal_input",
                                "attachment_id": "att-loop",
                                "data": base64::engine::general_purpose::STANDARD
                                    .encode(b"echo LOOP_MARKER\r"),
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                    typed = true;
                }
                Some("terminal_output") => {
                    let data = message["data"].as_str().expect("output carries data");
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .expect("output is base64");
                    // Credit it back, exactly as the phone does, so the carrier
                    // keeps streaming rather than parking on an empty grant.
                    socket
                        .send(Message::Text(
                            serde_json::json!({
                                "type": "terminal_credit",
                                "attachment_id": "att-loop",
                                "bytes": bytes.len(),
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                    if String::from_utf8_lossy(&bytes).contains("LOOP_MARKER") {
                        carried = true;
                        break;
                    }
                }
                Some("terminal_closed") => {
                    panic!("the terminal closed instead of streaming: {message}")
                }
                _ => {}
            }
        }
        assert!(typed, "the attach was acknowledged over the wire");
        assert!(
            carried,
            "the connection's output arm never carried the pane's bytes to the phone"
        );
        assert!(fx.alive(), "the session outlives the connection");
    }

    /// A carrier that ends on its own tells the phone *which* ending it was, in
    /// the carrier's own words.
    ///
    /// The output arm's `None` branch — the one that runs when the carrier's own
    /// stream closes, as opposed to the phone detaching or an attach being
    /// refused — is the only path that puts a [`crate::terminal::CloseCause`] on
    /// the wire, and it was reachable by reading the code and no other way.
    ///
    /// What rides it matters more than the branch does. Two endings share the
    /// `slow_consumer` code, and the *sentence* is the only thing that tells them
    /// apart for a user: the app shows the daemon's `reason` verbatim whenever
    /// there is one and reaches its own per-code sentence only when that is empty
    /// (`TerminalCarrier.describe(code:reason:)`). An untested path carrying the
    /// sole user-visible discriminator is worth more than a test of the branch.
    ///
    /// So this drives the ending the distinction is about rather than the
    /// cheapest one that reaches the branch. The attach is given a grant smaller
    /// than the screen it paints, so the reader spends the whole of it on a
    /// prefix and is left waiting on credit; the phone reads its socket
    /// throughout — which is what completes the write and leaves nothing owed,
    /// so the wait is genuinely the phone's — and simply never credits. That is
    /// `slow_consumer` meaning the one thing it may honestly mean.
    ///
    /// Three weaker shapes would each miss it. Asserting only the code passes for
    /// either stall, which is the entire ambiguity. Asserting only that some
    /// `terminal_closed` arrived passes for a detach, a refused attach or a
    /// session that exited — none of which reach this branch. And the sentence is
    /// compared against the constant that mints it rather than a copy: a
    /// transcribed literal goes on passing while the two drift apart.
    #[tokio::test]
    async fn a_carrier_that_stalls_tells_the_phone_which_stall_it_was() {
        // The stall deadline is process-wide, and so is the fixture socket every
        // attach resolves against.
        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let stall = Duration::from_millis(1000);
        let _deadline = crate::terminal::StallDeadline::shortened_to(stall);
        let _socket = FixtureSocket::set(&fx.sock);
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");

        // Eight bytes against the screen the attach paints: the grant is spent on
        // a prefix of the very first chunk, and the reader waits on credit for
        // the rest of it. A phone-sized grant would leave it waiting on the pane
        // instead, and the carrier would simply sit there.
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "terminal_attach",
                    "attachment_id": "att-stall",
                    "session_uid": fx.uid,
                    "cols": 80,
                    "rows": 24,
                    "output_credit": 8,
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let mut streamed = 0usize;
        let mut closed = None;
        let deadline = tokio::time::Instant::now() + stall * 20;
        while tokio::time::Instant::now() < deadline {
            let Some(message) = next_json(&mut socket).await else {
                break;
            };
            match message["type"].as_str() {
                // Read and never credited. Reading is what completes the write,
                // so the bytes are the phone's to answer for; not crediting is
                // the whole stimulus.
                Some("terminal_output") => streamed += 1,
                Some("terminal_closed") => {
                    closed = Some(message);
                    break;
                }
                _ => {}
            }
        }

        let closed = closed.expect("a stalled carrier must tell the phone it closed");
        assert!(
            streamed > 0,
            "the grant must have been spent on output the phone actually \
             received, or the reader was never waiting on credit and this close \
             is some other close"
        );
        assert_eq!(closed["attachment_id"], "att-stall");
        assert_eq!(
            closed["code"],
            protocol::ws::terminal_close::SLOW_CONSUMER,
            "got {closed}"
        );
        assert_eq!(
            closed["reason"],
            crate::terminal::cause::CREDIT_STARVED.reason,
            "the phone must be told which of the two stalls this was, in the \
             carrier's own words — got {closed}"
        );
        assert!(fx.alive(), "the session outlives the stalled terminal");
    }

    /// Drive one terminal message against the connection-local state, exactly
    /// as `handle_client` does, over a private transport. `acks` is the
    /// input-ack receiver the connection keeps; the streaming helpers below
    /// pump it.
    async fn drive(
        daemon: &Arc<Daemon>,
        conn: &mut Locals,
        sink: &mut CollectSink,
        socket: &str,
        device_id: Option<&str>,
        message: ClientMessage,
    ) {
        drive_over(daemon, conn, sink, socket, device_id, true, message).await
    }

    /// The same, saying out loud whether this connection's bytes are private in
    /// transit — the one thing a terminal needs beyond a paired device.
    #[allow(clippy::too_many_arguments)]
    async fn drive_over(
        daemon: &Arc<Daemon>,
        conn: &mut Locals,
        sink: &mut CollectSink,
        socket: &str,
        device_id: Option<&str>,
        private_transport: bool,
        message: ClientMessage,
    ) {
        handle_terminal(
            daemon,
            &mut conn.terminal,
            &mut conn.out,
            &mut conn.acks,
            &mut conn.opening,
            sink,
            socket,
            device_id,
            private_transport,
            message,
        )
        .await
        .expect("a collect sink cannot fail");
    }

    /// Attach the way the connection does: drive the message, then take the
    /// finished open off the arm that `handle_client` selects on. An attach
    /// refused before it spawns leaves nothing to settle and returns here.
    async fn attach(
        daemon: &Arc<Daemon>,
        conn: &mut Locals,
        sink: &mut CollectSink,
        socket: &str,
        device_id: Option<&str>,
        message: ClientMessage,
    ) {
        drive(daemon, conn, sink, socket, device_id, message).await;
        settle(daemon, conn, sink, socket, device_id).await;
    }

    /// Take the finished open off the arm `handle_client` selects on, and drain
    /// the frames that waited behind it — the arm's whole body, so a test never
    /// reproduces half of it.
    async fn settle(
        daemon: &Arc<Daemon>,
        conn: &mut Locals,
        sink: &mut CollectSink,
        socket: &str,
        device_id: Option<&str>,
    ) {
        // Nothing was spawned, so there is nothing to settle — and waiting on
        // the arm would park for ever, exactly as it does in the loop.
        if conn.opening.is_none() {
            return;
        }
        let opened = next_terminal_open(&mut conn.opening).await;
        let Some(pending) = conn.opening.take() else {
            return;
        };
        let deferred = finish_terminal_open(
            daemon,
            pending,
            opened,
            &mut conn.terminal,
            &mut conn.out,
            &mut conn.acks,
            sink,
            device_id,
        )
        .await
        .expect("a collect sink cannot fail");
        for message in deferred {
            drive(daemon, conn, sink, socket, device_id, message).await;
        }
    }

    /// The four connection-local terminal slots, together so tests thread one
    /// value instead of four.
    #[derive(Default)]
    struct Locals {
        terminal: Option<TerminalConn>,
        out: Option<crate::terminal::OutputDrain>,
        acks: Option<tokio::sync::mpsc::UnboundedReceiver<u32>>,
        opening: Option<Opening>,
    }

    fn attach_msg(id: &str, uid: &str, credit: u32) -> ClientMessage {
        ClientMessage::TerminalAttach {
            attachment_id: id.into(),
            session_uid: uid.into(),
            cols: 80,
            rows: 24,
            output_credit: credit,
        }
    }

    fn input_msg(id: &str, bytes: &[u8]) -> ClientMessage {
        ClientMessage::TerminalInput {
            attachment_id: id.into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    /// A well-formed session uid for the refusal tests, which never reach a real
    /// session (they are refused before resolve, or resolve against a dead
    /// socket). A malformed uid would be refused for the wrong reason.
    fn some_uid() -> String {
        protocol::uid::new().unwrap()
    }

    /// The wired path against real tmux: attach streams the active pane under
    /// the output-credit protocol; input's byte credit is replenished only once
    /// the writer has handed it to the client (the ack arm); and the exact
    /// outstanding ledger closes an over-grant — including the case where the
    /// carrier has an acquire pending, which a permit-count check would wave
    /// through.
    #[tokio::test]
    async fn the_wired_terminal_streams_and_enforces_credit() {
        use protocol::ws::terminal_close as tc;

        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals::default();
        let mut sink = CollectSink::default();

        // Attach with a single byte of output credit: the first echo
        // immediately out-sizes the grant, so the carrier is left mid-chunk
        // with an acquire pending — the exact state the ledger must survive.
        attach(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            attach_msg("att-1", &fx.uid, 1),
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalAttached { attachment_id, .. } if attachment_id == "att-1"),
            "attach is acknowledged, got {:?}",
            sink.last()
        );
        assert!(conn.terminal.is_some() && conn.out.is_some() && conn.acks.is_some());
        assert!(
            conn.opening.is_none(),
            "the attach is settled, not in flight"
        );
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            input_msg("att-1", b"echo starve\r"),
        )
        .await;

        // The one granted output byte is unsettled (nothing forwarded), so a
        // maximal grant on top of it overflows the ceiling and closes the
        // terminal — exactly the grant a permit-count check would have waved
        // through while the carrier held a pending acquire.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            ClientMessage::TerminalCredit {
                attachment_id: "att-1".into(),
                bytes: protocol::ws::TERMINAL_MAX_OUTSTANDING_CREDIT,
            },
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalClosed { code, .. } if code == tc::PROTOCOL_ERROR),
            "an over-grant closes the terminal, got {:?}",
            sink.last()
        );
        assert!(conn.terminal.is_none() && conn.out.is_none() && conn.acks.is_none());
        assert!(fx.alive(), "the session outlives the closed terminal");

        // A fresh attach with a phone-sized window, at the first attempt. The
        // lease frees only once the previous client is reaped, and the attach
        // waits on that itself now rather than leaving the caller to retry — so
        // no loop here, which is the point: a phone has no loop either.
        let mut sink = CollectSink::default();
        attach(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            attach_msg(
                "att-2",
                &fx.uid,
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalAttached { .. }),
            "the leases were free again, got {:?}",
            sink.last()
        );

        // Typed input reaches the pane. Its input credit is replenished by the
        // exact byte count — but only through the ack arm, after the writer
        // delivered it, never on receipt.
        let keystrokes = b"echo WIRED_MARKER\r";
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            input_msg("att-2", keystrokes),
        )
        .await;
        let replenished = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            next_input_ack(&mut conn.acks),
        )
        .await
        .expect("the writer reports delivery")
        .expect("an ack arrives");
        assert_eq!(
            replenished,
            keystrokes.len() as u32,
            "credit returns exactly what was delivered"
        );
        if let Some(t) = conn.terminal.as_mut() {
            t.input_credit = t.input_credit.saturating_add(replenished);
        }

        let mut streamed = String::new();
        let mut saw = false;
        for _ in 0..100 {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                next_terminal_output(&mut conn.out),
            )
            .await
            {
                Ok(Some(chunk)) => {
                    let delivered = chunk.bytes().len() as u32;
                    let t = conn.terminal.as_mut().unwrap();
                    t.outstanding = t.outstanding.saturating_sub(delivered);
                    streamed.push_str(&String::from_utf8_lossy(chunk.bytes()));
                    // The write, then the credit — in that order, because the
                    // phone cannot credit bytes it has not been sent. Dropping
                    // the chunk here is this test standing in for the write.
                    drop(chunk);
                    drive(
                        &daemon,
                        &mut conn,
                        &mut sink,
                        &fx.sock,
                        Some(&device),
                        ClientMessage::TerminalCredit {
                            attachment_id: "att-2".into(),
                            bytes: delivered,
                        },
                    )
                    .await;
                    if streamed.contains("WIRED_MARKER") {
                        saw = true;
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {}
            }
        }
        assert!(saw, "the echoed marker streamed back over the wire");

        // Clean detach: carrier torn down, state cleared, session alive.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            ClientMessage::TerminalDetach {
                attachment_id: "att-2".into(),
            },
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalClosed { code, .. } if code == tc::DETACHED),
            "detach is acknowledged as such, got {:?}",
            sink.last()
        );
        assert!(conn.terminal.is_none() && conn.out.is_none() && conn.acks.is_none());
        assert!(fx.alive(), "detaching a viewer leaves the session running");
    }

    /// The connection goes on serving its other arms while an attach is still
    /// opening.
    ///
    /// `TerminalHandle::open` resolves the session, spawns a tmux client, and
    /// waits for that client to announce and report its pane — seconds, in the
    /// worst case. Awaited inside the incoming-message arm, those are seconds in
    /// which this connection polls nothing else: an approval card raised
    /// meanwhile goes unsent, a chatty fleet lags the client off the broadcast
    /// ring, and a revocation is not acted on until the attach returns. The last
    /// is what this proves, because it is the one an operator asked for out
    /// loud. The attach is held at its first instruction so the window belongs
    /// to the test rather than to a race with tmux, and the revocation that
    /// lands inside it must still be answered promptly.
    #[tokio::test]
    async fn a_revocation_is_answered_while_an_attach_is_still_opening() {
        let _home = redirected_home("ws-revoke-mid-attach");
        // Every attach in the process is held, so this takes the same serial
        // guard the tmux-fixture tests take.
        let _serial = crate::terminal::fixture_test_guard().await;
        let (server, device_id) = live_server(protocol::config::Config::default()).await;
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");

        let held = crate::terminal::OpenHold::close();
        let attach = serde_json::json!({
            "type": "terminal_attach",
            "attachment_id": "att-1",
            "session_uid": some_uid(),
            "cols": 80,
            "rows": 24,
            "output_credit": protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
        });
        socket
            .send(Message::Text(attach.to_string()))
            .await
            .unwrap();
        // The attach is provably inside the open before anything else happens,
        // so what follows is measured against a connection that is mid-attach —
        // not one that has already finished, or not yet started.
        held.reached().await;

        server.daemon.revoke(&device_id).await.unwrap();
        let told = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(message) = next_json(&mut socket).await {
                if message["code"] == "revoked" {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(told, "a revocation waited for an attach to finish opening");
        drop(held);
    }

    /// A frame sent behind an attach is applied to the terminal that attach
    /// creates, never to the absence of one.
    ///
    /// This is what `if opening.is_none()` on the incoming arm buys, and it is
    /// invisible to every other test here because none of them sends a second
    /// frame while an attach is in flight. Without the guard the loop reads the
    /// input the moment it lands — while `opening` is `Some` and `terminal` is
    /// still `None` — and answers a `terminal_closed`/"no such terminal" for a
    /// terminal that is seconds from existing. The phone did nothing wrong: it
    /// sent two frames in order down one socket.
    ///
    /// The attach is held at its first instruction so the window belongs to the
    /// test rather than to a race with tmux.
    #[tokio::test]
    async fn a_frame_behind_an_attach_waits_for_the_terminal_it_names() {
        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let _socket = FixtureSocket::set(&fx.sock);
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");

        let held = crate::terminal::OpenHold::close();
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "terminal_attach",
                    "attachment_id": "att-1",
                    "session_uid": fx.uid,
                    "cols": 80,
                    "rows": 24,
                    "output_credit": protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                })
                .to_string(),
            ))
            .await
            .unwrap();
        // Provably inside the open before the second frame is sent, so the
        // ordering below is the one the guard governs and not a race.
        held.reached().await;

        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "terminal_input",
                    "attachment_id": "att-1",
                    "data": base64::engine::general_purpose::STANDARD.encode(b"echo BEHIND\r"),
                })
                .to_string(),
            ))
            .await
            .unwrap();
        drop(held);

        // The attach is acknowledged and the input applied to it. A
        // `terminal_closed` at any point is the failure: it means the input was
        // read against a connection that had no terminal yet.
        let mut attached = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < deadline {
            let Some(message) = next_json(&mut socket).await else {
                break;
            };
            match message["type"].as_str() {
                Some("terminal_attached") => attached = true,
                Some("terminal_closed") => panic!(
                    "a frame sent behind the attach was answered before the terminal existed: {message}"
                ),
                // The echo of the input the pane received, which is the input
                // having been applied to the terminal the attach created.
                Some("terminal_output") => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(message["data"].as_str().expect("output carries data"))
                        .expect("output is base64");
                    if String::from_utf8_lossy(&bytes).contains("BEHIND") {
                        assert!(attached, "output arrived before the attach was acknowledged");
                        return;
                    }
                    socket
                        .send(Message::Text(
                            serde_json::json!({
                                "type": "terminal_credit",
                                "attachment_id": "att-1",
                                "bytes": bytes.len(),
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                }
                _ => {}
            }
        }
        panic!("the input sent behind the attach never reached the pane (attached={attached})");
    }

    /// A ping sent while an attach is opening is answered while it is still
    /// opening.
    ///
    /// The regression this exists for was not about pings. The incoming arm was
    /// gated on `opening.is_none()`, so for the whole of an attach — up to
    /// `ATTACH_DEADLINE`, twenty seconds — the connection read *nothing*: an
    /// approval answer the user had visibly given sat unread and could miss its
    /// `respond_by` outright. A ping is the cheapest message that proves the
    /// gate is gone, and it is not incidental either: the app's own liveness
    /// runs on it against a thirty-second timeout.
    #[tokio::test]
    async fn a_ping_is_answered_while_an_attach_is_still_opening() {
        // Every attach in the process is held, so this takes the same serial
        // guard the tmux-fixture tests take.
        let _serial = crate::terminal::fixture_test_guard().await;
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");

        let held = crate::terminal::OpenHold::close();
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "terminal_attach",
                    "attachment_id": "att-1",
                    "session_uid": some_uid(),
                    "cols": 80,
                    "rows": 24,
                    "output_credit": protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                })
                .to_string(),
            ))
            .await
            .unwrap();
        // Provably inside the open, so the ping below is sent to a connection
        // that is genuinely mid-attach rather than one already finished.
        held.reached().await;

        socket
            .send(Message::Text(
                serde_json::json!({ "type": "ping" }).to_string(),
            ))
            .await
            .unwrap();
        let answer = next_json_within(&mut socket, Duration::from_secs(5).min(attach_deadline()))
            .await
            .expect("the connection must answer while the attach is still opening");
        assert_eq!(
            answer["type"], "pong",
            "a message unrelated to the terminal must be handled during an \
             attach, got {answer}"
        );
        drop(held);
    }

    /// A second connection takes a session's terminal over: the incumbent's
    /// connection is told `superseded`, and the newcomer gets a live terminal.
    ///
    /// The lockout this replaces was the product's worst terminal bug. A lease
    /// held by a silently-dead connection — iOS backgrounds the app and the
    /// socket dies without a FIN — survives for minutes, and the reconnecting
    /// phone was refused its *own* session's terminal for that whole window with
    /// nothing but a Retry that kept failing. Proven over the real wire against
    /// real tmux, because the mechanism spans two connections, the lease, and
    /// the reaper.
    #[tokio::test]
    async fn a_second_connection_takes_the_sessions_terminal_over() {
        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let _socket = FixtureSocket::set(&fx.sock);
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;

        let (mut incumbent, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");
        attach_over(&mut incumbent, "att-a", &fx.uid).await;

        // A second connection, exactly as a reconnecting phone is: a new socket,
        // a new hello, and an attach on the session it already had one on.
        let (mut newcomer, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");
        newcomer
            .send(Message::Text(
                serde_json::json!({
                    "type": "terminal_attach",
                    "attachment_id": "att-b",
                    "session_uid": fx.uid,
                    "cols": 80,
                    "rows": 24,
                    "output_credit": protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                })
                .to_string(),
            ))
            .await
            .unwrap();

        // The incumbent is told which ending this was, on its own id. Not
        // `session_exited` and not a socket that merely goes quiet: the phone
        // showing that terminal has to be able to say why it stopped.
        let closed = read_terminal_close(&mut incumbent, "att-a").await;
        assert_eq!(
            closed["code"],
            protocol::ws::terminal_close::SUPERSEDED,
            "the displaced terminal must be told it was superseded, got {closed}"
        );

        // And the newcomer really has a terminal, inside the attach deadline.
        let attached = read_attach_verdict(&mut newcomer, "att-b").await;
        assert_eq!(
            attached["type"], "terminal_attached",
            "the takeover must produce a live terminal, got {attached}"
        );
        assert!(fx.alive(), "the session outlives the handover");
    }

    /// A takeover the incumbent never completes is refused as `session_busy` —
    /// a code of its own, and the incumbent's lease is left where it was.
    ///
    /// `attachment_limit` used to carry this, which told the phone "the Mac is
    /// full" for what is one stuck disposable client on one session. The two
    /// want different answers from a client: the cap is not worth retrying by
    /// itself, and this is.
    #[tokio::test]
    async fn a_takeover_that_times_out_is_refused_as_session_busy() {
        let _serial = crate::terminal::fixture_test_guard().await;
        let _wait = crate::terminal::SupersedeWait::shortened_to(Duration::from_millis(100));
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");

        // An incumbent nothing will release — the stuck child, without needing
        // one. Held for the whole test.
        let uid = some_uid();
        let held = server.daemon.terminal_leases.hold_for_test(&uid);

        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "terminal_attach",
                    "attachment_id": "att-1",
                    "session_uid": uid,
                    "cols": 80,
                    "rows": 24,
                    "output_credit": protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let closed = read_terminal_close(&mut socket, "att-1").await;
        assert_eq!(
            closed["code"],
            protocol::ws::terminal_close::SESSION_BUSY,
            "a takeover that could not complete is its own refusal, got {closed}"
        );
        assert!(
            server.daemon.terminal_leases.held(&uid),
            "and the incumbent's lease survives it — a lease released by a \
             failed takeover would let two clients overlap on one session"
        );
        drop(held);
    }

    /// The global cap still answers `attachment_limit`, and that code now means
    /// only the cap.
    #[tokio::test]
    async fn the_global_cap_is_the_only_attachment_limit_left() {
        let _serial = crate::terminal::fixture_test_guard().await;
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");

        // Every slot taken, by sessions unrelated to the one asked for below —
        // which is what makes this the cap and not the per-session lease.
        let held: Vec<_> = (0..crate::terminal::MAX_TERMINAL_ATTACHMENTS)
            .map(|_| server.daemon.terminal_leases.hold_for_test(&some_uid()))
            .collect();

        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "terminal_attach",
                    "attachment_id": "att-1",
                    "session_uid": some_uid(),
                    "cols": 80,
                    "rows": 24,
                    "output_credit": protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let closed = read_terminal_close(&mut socket, "att-1").await;
        assert_eq!(
            closed["code"],
            protocol::ws::terminal_close::ATTACHMENT_LIMIT,
            "the Mac being full is the one refusal this code still names, got {closed}"
        );
        drop(held);
    }

    /// No `terminal_output` frame carries more than `MAX_TERMINAL_CHUNK_BYTES`
    /// decoded bytes, whatever the phone has granted.
    ///
    /// The bound is documented as receiver-enforced, and the forwarder used to
    /// hand over as much as the current grant covered — so a phone granting the
    /// full ceiling was sent frames sixteen times the bound it was told to
    /// enforce, and a client that implemented the documented check would have
    /// killed a healthy terminal on its first busy screen.
    ///
    /// Driven by the **attach snapshot**, which is the case the finding named
    /// and the only one that can produce a chunk this large: live output arrives
    /// one control-mode `%output` line at a time, but the carrier synthesises
    /// the whole current screen as a *single* chunk on bind. So the pane here is
    /// a large one, filled — a hundred kilobytes of grid, handed to the
    /// forwarder in one piece, against the maximum grant a phone may open with.
    /// Before the cap that arrived as one ~100 KiB frame.
    #[tokio::test]
    async fn no_terminal_output_frame_exceeds_the_documented_chunk_bound() {
        use protocol::ws::MAX_TERMINAL_CHUNK_BYTES;

        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        // A pane big enough that its screen alone outruns the bound several
        // times over, and filled before anything attaches, so the snapshot is
        // the whole of it.
        let (cols, rows) = (500u16, 200u16);
        fx.tmux(&[
            "resize-window",
            "-t",
            "cc-term",
            "-x",
            &cols.to_string(),
            "-y",
            &rows.to_string(),
        ]);
        fx.tmux(&[
            "send-keys",
            "-t",
            "cc-term",
            &format!(
                "for i in $(seq 1 {}); do printf '%0{}d\\n' 0; done",
                rows + 20,
                cols - 1
            ),
            "Enter",
        ]);
        let painted = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < painted {
            let screen = fx.tmux(&["capture-pane", "-p", "-t", "cc-term"]);
            if screen.stdout.len() > MAX_TERMINAL_CHUNK_BYTES * 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let screen = fx.tmux(&["capture-pane", "-p", "-t", "cc-term"]);
        assert!(
            screen.stdout.len() > MAX_TERMINAL_CHUNK_BYTES * 4,
            "the fixture pane holds only {} bytes, so an attach snapshot of it \
             could not exceed the bound however the forwarder behaved",
            screen.stdout.len()
        );

        let _socket = FixtureSocket::set(&fx.sock);
        let (server, _device_id) = live_server(protocol::config::Config::default()).await;
        let (mut socket, reply) = server
            .hello(protocol::PROTOCOL_VERSION, Some("device-token-for-tests"))
            .await;
        assert_eq!(reply["type"], "hello_ack");

        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "terminal_attach",
                    "attachment_id": "att-1",
                    "session_uid": fx.uid,
                    "cols": cols,
                    "rows": rows,
                    // The largest window the protocol allows, which is the whole
                    // stimulus: what used to decide the frame size was the
                    // grant, so a phone-sized one would not discriminate.
                    "output_credit": protocol::ws::TERMINAL_MAX_OUTSTANDING_CREDIT,
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let mut attached = false;
        let mut streamed = 0usize;
        let mut largest = 0usize;
        let want = MAX_TERMINAL_CHUNK_BYTES * 4;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline && streamed < want {
            let Some(message) = next_json_within(&mut socket, Duration::from_secs(5)).await else {
                break;
            };
            match message["type"].as_str() {
                Some("terminal_attached") => attached = true,
                Some("terminal_output") => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(message["data"].as_str().expect("output carries data"))
                        .expect("output is base64");
                    largest = largest.max(bytes.len());
                    streamed += bytes.len();
                    assert!(
                        bytes.len() <= MAX_TERMINAL_CHUNK_BYTES,
                        "a terminal_output carried {} decoded bytes, past the \
                         {MAX_TERMINAL_CHUNK_BYTES} a receiver is told to enforce",
                        bytes.len()
                    );
                    socket
                        .send(Message::Text(
                            serde_json::json!({
                                "type": "terminal_credit",
                                "attachment_id": "att-1",
                                "bytes": bytes.len(),
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                }
                Some("terminal_closed") => panic!("the terminal closed: {message}"),
                _ => {}
            }
        }
        assert!(attached, "the attach must have been acknowledged");
        assert!(
            streamed >= want,
            "only {streamed} of {want} bytes streamed, so the frames asserted on \
             above are not the snapshot this is about"
        );
        // The bound was approached and not merely never exceeded: a forwarder
        // handing over a byte at a time would satisfy the assertion in the loop
        // and prove nothing about the cap.
        assert!(
            largest > MAX_TERMINAL_CHUNK_BYTES / 2,
            "the largest frame was {largest} bytes, so nothing here ever reached \
             the size the cap exists to split"
        );
    }

    /// Attach over a live socket and wait for the ack, crediting output as it
    /// arrives so the carrier is never left stalled behind this helper.
    async fn attach_over(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        attachment_id: &str,
        session_uid: &str,
    ) {
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "terminal_attach",
                    "attachment_id": attachment_id,
                    "session_uid": session_uid,
                    "cols": 80,
                    "rows": 24,
                    "output_credit": protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let verdict = read_attach_verdict(socket, attachment_id).await;
        assert_eq!(
            verdict["type"], "terminal_attached",
            "the attach must land, got {verdict}"
        );
    }

    /// Read until the attach for `attachment_id` is answered either way.
    async fn read_attach_verdict(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        attachment_id: &str,
    ) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + attach_deadline() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let Some(message) = next_json_within(socket, Duration::from_secs(5)).await else {
                continue;
            };
            match message["type"].as_str() {
                Some("terminal_attached" | "terminal_closed")
                    if message["attachment_id"] == attachment_id =>
                {
                    return message
                }
                _ => {}
            }
        }
        panic!("the attach on {attachment_id} was never answered");
    }

    /// Read until `attachment_id` is closed, granting credit for anything it
    /// streams on the way so the close is the carrier's and not a stall.
    async fn read_terminal_close(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        attachment_id: &str,
    ) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + attach_deadline() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let Some(message) = next_json_within(socket, Duration::from_secs(5)).await else {
                continue;
            };
            match message["type"].as_str() {
                Some("terminal_closed") if message["attachment_id"] == attachment_id => {
                    return message
                }
                Some("terminal_output") => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(message["data"].as_str().expect("output carries data"))
                        .expect("output is base64");
                    socket
                        .send(Message::Text(
                            serde_json::json!({
                                "type": "terminal_credit",
                                "attachment_id": message["attachment_id"],
                                "bytes": bytes.len(),
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                }
                _ => {}
            }
        }
        panic!("{attachment_id} was never closed");
    }

    /// A device revoked while its attach was opening never gets the terminal.
    ///
    /// The open awaits tmux, so a revocation can always land inside it — and by
    /// then the carrier is real: a client is attached to the session and its
    /// reader is holding the pane's first bytes. The re-check that closes that
    /// window is the last thing standing between a device the operator has just
    /// cut off and a live shell, so it is proven against a real carrier rather
    /// than a stub. The revocation is placed between the spawn and the settle,
    /// which is exactly where the window is.
    #[tokio::test]
    async fn a_device_revoked_while_its_attach_opened_is_refused_the_terminal() {
        let _home = redirected_home("ws-revoke-during-open");
        use protocol::ws::terminal_close as tc;

        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals::default();
        let mut sink = CollectSink::default();

        drive(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            attach_msg(
                "att-1",
                &fx.uid,
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        assert!(conn.opening.is_some(), "the attach is in flight");
        daemon.revoke(&device).await.unwrap();

        let opened = next_terminal_open(&mut conn.opening).await;
        assert!(opened.is_ok(), "the carrier itself opened; the refusal below must be the revocation and not a failed attach");
        let pending = conn.opening.take().expect("the attach is still parked");
        finish_terminal_open(
            &daemon,
            pending,
            opened,
            &mut conn.terminal,
            &mut conn.out,
            &mut conn.acks,
            &mut sink,
            Some(&device),
        )
        .await
        .expect("a collect sink cannot fail");

        assert!(
            matches!(sink.last(), ServerMessage::TerminalClosed { code, .. } if code == tc::NOT_AUTHORISED),
            "a revoked device is refused the terminal its attach had already opened, got {:?}",
            sink.last()
        );
        assert!(
            conn.terminal.is_none() && conn.out.is_none() && conn.acks.is_none(),
            "and no terminal is installed for it"
        );
        assert!(fx.alive(), "the session outlives the refused attach");
    }

    /// A second attach on one connection takes the terminal over: the existing
    /// one is closed as `superseded`, named by its *own* id, and the new attach
    /// is then processed like any other.
    ///
    /// It used to be a protocol violation that closed the old terminal and
    /// returned — leaving the new id neither attached nor refused, which the
    /// phone shows as "Opening a terminal…" for ever, because its carrier
    /// discards closes naming other ids and has no attach timeout of its own.
    /// So both halves are asserted: the close names `att-1`, *and* the open for
    /// `att-2` began.
    #[tokio::test]
    async fn a_second_attach_supersedes_the_terminal_this_connection_had() {
        use protocol::ws::{terminal_close as tc, TERMINAL_INITIAL_INPUT_CREDIT};

        // The second attach is now *spawned* rather than refused, so this takes
        // the serial guard every test that opens one takes: the open gate and
        // the fixture socket are process-wide.
        let _serial = crate::terminal::fixture_test_guard().await;
        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals {
            terminal: Some(TerminalConn {
                attachment_id: "att-1".into(),
                handle: crate::terminal::TerminalHandle::inert_stub(),
                output_credit: Arc::new(tokio::sync::Semaphore::new(0)),
                outstanding: 0,
                input_credit: TERMINAL_INITIAL_INPUT_CREDIT,
            }),
            out: Some(crate::terminal::output_channel(4).1),
            acks: Some(tokio::sync::mpsc::unbounded_channel().1),
            opening: None,
        };
        let mut sink = CollectSink::default();

        // Re-attach under a DIFFERENT id, which is the case that can tell the
        // two ids apart. Re-attaching under the same one satisfies the assertion
        // below whichever id the close names, so it proves nothing about the
        // rule it is written for.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            attach_msg(
                "att-2",
                &some_uid(),
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalClosed { attachment_id, code, .. }
                if attachment_id == "att-1" && code == tc::SUPERSEDED),
            "the close must name the terminal that actually exists (att-1) as \
             superseded, not the id the second attach asked for, got {:?}",
            sink.last()
        );
        assert!(conn.terminal.is_none() && conn.out.is_none() && conn.acks.is_none());
        assert!(
            conn.opening
                .as_ref()
                .is_some_and(|pending| pending.attachment_id == "att-2"),
            "the new attach must be under way — an id that is never answered is \
             the permanent spinner this replaced"
        );
    }

    /// A `terminal_attach` that arrives while another is still opening
    /// supersedes it: the in-flight id is answered, and the newer one takes
    /// over.
    ///
    /// The one path that can strand two ids at once. The displaced open is
    /// aborted by dropping its `Opening`, and if that dropped silently the phone
    /// would be left waiting on an acknowledgement for an attach the daemon has
    /// forgotten.
    #[tokio::test]
    async fn an_attach_arriving_mid_open_supersedes_the_one_in_flight() {
        use protocol::ws::terminal_close as tc;

        let _serial = crate::terminal::fixture_test_guard().await;
        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals::default();
        let mut sink = CollectSink::default();

        let held = crate::terminal::OpenHold::close();
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            attach_msg(
                "att-1",
                &some_uid(),
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        // Provably inside the open, so the second attach really does meet one in
        // flight rather than one that has already failed against a dead socket.
        held.reached().await;

        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            attach_msg(
                "att-2",
                &some_uid(),
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalClosed { attachment_id, code, .. }
                if attachment_id == "att-1" && code == tc::SUPERSEDED),
            "the displaced open must be answered on its own id, got {:?}",
            sink.last()
        );
        assert!(
            conn.opening
                .as_ref()
                .is_some_and(|pending| pending.attachment_id == "att-2"),
            "and the newer attach must be the one in flight"
        );
        drop(held);
    }

    /// A malformed attach is refused without costing the phone the terminal it
    /// already has.
    ///
    /// Takeover and validation are both wanted, and the order they run in is
    /// the whole of the difference: superseding first means a frame that never
    /// had a chance of opening anything — a bad geometry, a credit outside the
    /// window, an oversized id, a uid that is not a uid — still trades a working
    /// shell for a protocol error, which is a phone losing its terminal for
    /// asking badly rather than for asking again. Every refusal is therefore
    /// decided before anything is torn down, and this pins that: the old
    /// terminal is still streaming after the refusal, and the refusal names the
    /// new id.
    ///
    /// Every bad attach here names an id of its *own*. One that re-used the
    /// live id would be a re-attach, which is a different rule with a test of
    /// its own (`a_close_never_names_an_id_that_is_still_live`).
    #[tokio::test]
    async fn a_malformed_attach_is_refused_without_costing_the_live_terminal() {
        use protocol::ws::terminal_close as tc;

        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals::default();
        let mut sink = CollectSink::default();

        attach(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            attach_msg(
                "att-live",
                &fx.uid,
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalAttached { attachment_id, .. }
                if attachment_id == "att-live"),
            "the terminal under test must be live to begin with, got {:?}",
            sink.last()
        );

        // Every shape the handler refuses, each against the same live terminal,
        // and each with the code it is owed — a malformed uid is an unknown
        // session, not a protocol violation.
        for (label, want, bad) in [
            (
                "bad geometry",
                tc::PROTOCOL_ERROR,
                ClientMessage::TerminalAttach {
                    attachment_id: "att-bad".into(),
                    session_uid: fx.uid.clone(),
                    cols: 0,
                    rows: 24,
                    output_credit: protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                },
            ),
            (
                "bad output credit",
                tc::PROTOCOL_ERROR,
                ClientMessage::TerminalAttach {
                    attachment_id: "att-bad".into(),
                    session_uid: fx.uid.clone(),
                    cols: 80,
                    rows: 24,
                    output_credit: 0,
                },
            ),
            (
                "oversized attachment id",
                tc::PROTOCOL_ERROR,
                ClientMessage::TerminalAttach {
                    attachment_id: "x".repeat(protocol::ws::MAX_ATTACHMENT_ID_BYTES + 1),
                    session_uid: fx.uid.clone(),
                    cols: 80,
                    rows: 24,
                    output_credit: protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                },
            ),
            // The uid's shape used to be checked by the resolver alone, which
            // sits behind the supersede and the lease: an empty uid therefore
            // tore down the live terminal, spawned an attach, and only then
            // answered `session_not_hosted`. It belongs with the other
            // refusals, which is what this case pins.
            (
                "empty session uid",
                tc::SESSION_NOT_HOSTED,
                ClientMessage::TerminalAttach {
                    attachment_id: "att-bad".into(),
                    session_uid: String::new(),
                    cols: 80,
                    rows: 24,
                    output_credit: protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                },
            ),
            (
                "malformed session uid",
                tc::SESSION_NOT_HOSTED,
                ClientMessage::TerminalAttach {
                    attachment_id: "att-bad".into(),
                    session_uid: "not a uid".into(),
                    cols: 80,
                    rows: 24,
                    output_credit: protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                },
            ),
        ] {
            drive(&daemon, &mut conn, &mut sink, &fx.sock, Some(&device), bad).await;
            assert!(
                matches!(sink.last(), ServerMessage::TerminalClosed { attachment_id, code, .. }
                    if attachment_id != "att-live" && code == want),
                "{label} must be refused as {want} on its own id, got {:?}",
                sink.last()
            );
            assert!(
                conn.terminal
                    .as_ref()
                    .is_some_and(|live| live.attachment_id == "att-live"),
                "{label} must leave the live terminal attached"
            );
            assert!(
                conn.out.is_some() && conn.acks.is_some(),
                "{label} must leave the live terminal's streams intact"
            );
        }

        // And the terminal that survived all three is still a working one.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            input_msg("att-live", b"echo SURVIVED\r"),
        )
        .await;
        let acked = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            next_input_ack(&mut conn.acks),
        )
        .await
        .expect("the writer reports delivery")
        .expect("an ack arrives");
        assert_eq!(acked, b"echo SURVIVED\r".len() as u32);

        if let Some(conn) = conn.terminal.take() {
            conn.teardown();
        }
    }

    /// No frame ever tells the phone an id is closed while that id is live.
    ///
    /// `terminal_closed` is documented as terminal for an id: iOS clears the
    /// attachment it names and never listens for it again. Two paths used to
    /// break that. A valid re-attach under the *same* id answered `closed(X)`
    /// and then `attached(X)`, which contradicts the protocol's own statement
    /// outright. And a malformed attach re-using the live id was refused with
    /// `closed(X)` while X went on streaming — the phone drops it, and the tmux
    /// client and daemon-wide lease behind it are orphaned from the UI with no
    /// way back to them.
    ///
    /// One rule settles both: an attach whose id collides with the live (or
    /// opening) one *is* the supersede, so the id is torn down before the frame
    /// is judged. Then a valid re-attach needs no close at all, and a refusal
    /// names an id that really is dead.
    #[tokio::test]
    async fn a_close_never_names_an_id_that_is_still_live() {
        use protocol::ws::terminal_close as tc;

        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals::default();
        let mut sink = CollectSink::default();

        attach(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            attach_msg(
                "att-1",
                &fx.uid,
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalAttached { attachment_id, .. }
                if attachment_id == "att-1"),
            "the terminal under test must be live to begin with, got {:?}",
            sink.last()
        );

        // A valid re-attach under the same id: the phone gets one answer for
        // `att-1`, and it is an attach.
        sink.0.clear();
        attach(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            attach_msg(
                "att-1",
                &fx.uid,
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        assert!(
            !sink.0.iter().any(|message| matches!(
                message,
                ServerMessage::TerminalClosed { attachment_id, .. } if attachment_id == "att-1"
            )),
            "a re-attach under a live id must not close it — the phone would \
             clear the very attachment it just asked for, got {:?}",
            sink.0
        );
        assert!(
            matches!(sink.last(), ServerMessage::TerminalAttached { attachment_id, .. }
                if attachment_id == "att-1"),
            "and it must be attached, got {:?}",
            sink.last()
        );
        assert!(
            conn.terminal
                .as_ref()
                .is_some_and(|live| live.attachment_id == "att-1"),
            "the re-attach is this connection's terminal now"
        );

        // A malformed attach re-using the live id. It is refused — but the id
        // it names is genuinely gone by then, which is the invariant: nothing
        // is left to stream behind the close.
        sink.0.clear();
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            ClientMessage::TerminalAttach {
                attachment_id: "att-1".into(),
                session_uid: fx.uid.clone(),
                cols: 0,
                rows: 24,
                output_credit: protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            },
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalClosed { attachment_id, code, .. }
                if attachment_id == "att-1" && code == tc::PROTOCOL_ERROR),
            "the refusal names the id the phone re-used, got {:?}",
            sink.last()
        );
        assert!(
            conn.terminal.is_none() && conn.opening.is_none(),
            "and nothing named `att-1` survives the close it was given"
        );
        assert!(
            conn.out.is_none() && conn.acks.is_none(),
            "including its streams"
        );
        assert!(fx.alive(), "the tmux session outlives its terminals");

        // And the same rule for an id that is still *opening*, which has its own
        // branch: a re-attach under it is answered once, by the attach that wins.
        // A close here would be a `terminal_closed` for an id the phone is at
        // that moment waiting to be told it has.
        let held = crate::terminal::OpenHold::close();
        let mut conn = Locals::default();
        sink.0.clear();
        for _ in 0..2 {
            drive(
                &daemon,
                &mut conn,
                &mut sink,
                &fx.sock,
                Some(&device),
                attach_msg(
                    "att-2",
                    &fx.uid,
                    protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                ),
            )
            .await;
        }
        assert!(
            sink.0.is_empty(),
            "a re-attach under an id that is still opening must not close it, \
             got {:?}",
            sink.0
        );
        assert!(
            conn.opening
                .as_ref()
                .is_some_and(|pending| pending.attachment_id == "att-2"),
            "and the newer attach is the one in flight"
        );
        drop(held);
        abandon_terminal(&mut conn.terminal, &mut conn.opening);
    }

    /// A terminal frame that arrives while an attach is opening is applied to
    /// the terminal that attach creates, in the order it arrived — and the
    /// queue holding it is bounded, with the overflow costing that attach and
    /// nothing else.
    ///
    /// The connection reads *every* message during an open now, so these frames
    /// need somewhere to wait: they name a terminal that does not exist yet, and
    /// answering them `no such terminal` would punish a phone for typing the
    /// instant it asked for a terminal, which the protocol allows.
    #[tokio::test]
    async fn frames_behind_an_attach_are_queued_and_the_queue_is_bounded() {
        use protocol::ws::terminal_close as tc;

        let _serial = crate::terminal::fixture_test_guard().await;
        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals::default();
        let mut sink = CollectSink::default();

        let held = crate::terminal::OpenHold::close();
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            attach_msg(
                "att-1",
                &some_uid(),
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        held.reached().await;

        // Exactly the bound: none of these is answered, and none is dropped.
        for n in 0..MAX_DEFERRED_TERMINAL_FRAMES {
            drive(
                &daemon,
                &mut conn,
                &mut sink,
                "unused",
                Some(&device),
                input_msg("att-1", format!("{n}").as_bytes()),
            )
            .await;
        }
        assert_eq!(
            conn.opening.as_ref().map(|pending| pending.deferred.len()),
            Some(MAX_DEFERRED_TERMINAL_FRAMES),
            "every frame behind the attach waits for it, in arrival order"
        );
        assert!(
            sink.0.is_empty(),
            "and none of them is answered while it waits, got {:?}",
            sink.last()
        );

        // One past it. The attach is what pays — not the connection, whose
        // event log and approval path these frames have nothing to do with.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            input_msg("att-1", b"one too many"),
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalClosed { attachment_id, code, .. }
                if attachment_id == "att-1" && code == tc::PROTOCOL_ERROR),
            "the overflow fails the attach it belongs to, got {:?}",
            sink.last()
        );
        assert!(
            conn.opening.is_none(),
            "and the open is abandoned, taking its queue with it"
        );
        drop(held);
    }

    /// The queue behind an attach is bounded by what it *costs*, not only by
    /// how many frames it holds — and a frame that can only ever end in
    /// `protocol_error` is refused when it arrives rather than stored and
    /// judged later.
    ///
    /// The count bound above is not a resource bound: frames were parked as
    /// deserialized `ClientMessage`s *before* base64 decoding, the chunk check,
    /// the id check and credit accounting, and one client message may run to
    /// nearly `MAX_CLIENT_MESSAGE_BYTES`. So 128 of them is ~128 MiB parked in
    /// one connection's opening, on a connection that has done nothing but pair
    /// — and nothing stops a client opening more connections. The frames here
    /// are the realistic ones for that: a single near-maximal input frame, a
    /// run of maximal chunks past the credit window, and frames that carry
    /// their bytes somewhere other than the payload.
    #[tokio::test]
    async fn the_deferred_queue_is_bounded_by_bytes_and_not_only_by_frames() {
        use protocol::ws::{
            terminal_close as tc, MAX_TERMINAL_CHUNK_BYTES, TERMINAL_INITIAL_INPUT_CREDIT,
        };

        let _serial = crate::terminal::fixture_test_guard().await;
        let (daemon, device) = daemon_with_a_device();
        // Every open in this test parks at the gate, so `opening` stays `Some`
        // and the queue is the only thing under test.
        let held = crate::terminal::OpenHold::close();

        /// A connection with one attach in flight and an empty queue behind it.
        /// Each case starts from a fresh one, because a refusal discards the
        /// whole `Opening` and the queue with it.
        async fn parked(daemon: &Arc<Daemon>, device: &str) -> (Locals, CollectSink) {
            let mut conn = Locals::default();
            let mut sink = CollectSink::default();
            drive(
                daemon,
                &mut conn,
                &mut sink,
                "unused",
                Some(device),
                attach_msg(
                    "att-1",
                    &some_uid(),
                    protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
                ),
            )
            .await;
            assert!(
                conn.opening.is_some(),
                "the attach must be in flight for anything to queue behind it"
            );
            (conn, sink)
        }

        /// Reads the sink rather than `last()`, so a frame that was silently
        /// parked reports the assertion below instead of panicking on an empty
        /// sink — which is exactly what every failure here looks like.
        fn refused(sink: &CollectSink) -> bool {
            matches!(sink.0.last(), Some(ServerMessage::TerminalClosed { attachment_id, code, .. })
                if attachment_id == "att-1" && code == tc::PROTOCOL_ERROR)
        }

        // One frame, 700 KiB of payload — inside what a client message may
        // carry, and on its own more than five times the whole queue's budget.
        // Under a count bound alone this parked, and 127 more like it could
        // follow.
        let (mut conn, mut sink) = parked(&daemon, &device).await;
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            input_msg("att-1", &vec![b'x'; 700 * 1024]),
        )
        .await;
        assert!(
            refused(&sink),
            "a single oversized frame must be refused when it arrives, got {:?}",
            sink.0
        );
        assert!(
            conn.opening.is_none(),
            "and it costs the attach it belongs to, never the connection"
        );

        // One byte past what a `terminal_input` may carry: small enough that
        // neither the queue's byte budget nor the credit window would notice
        // it, so this is the chunk bound and nothing else. Parked, it would
        // have been refused at the drain instead — after occupying the daemon
        // for the length of the open.
        let (mut conn, mut sink) = parked(&daemon, &device).await;
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            input_msg("att-1", &vec![b'x'; MAX_TERMINAL_CHUNK_BYTES + 1]),
        )
        .await;
        assert!(
            refused(&sink),
            "a frame past the chunk bound must be refused when it arrives, got {:?}",
            sink.0
        );

        // Chunk-sized frames, which are individually legal. Two fill the input
        // window the attach will be granted; no credit comes back until the
        // terminal exists, so a third is over credit before it is stored.
        let (mut conn, mut sink) = parked(&daemon, &device).await;
        let window = TERMINAL_INITIAL_INPUT_CREDIT as usize / MAX_TERMINAL_CHUNK_BYTES;
        for _ in 0..window {
            drive(
                &daemon,
                &mut conn,
                &mut sink,
                "unused",
                Some(&device),
                input_msg("att-1", &vec![b'x'; MAX_TERMINAL_CHUNK_BYTES]),
            )
            .await;
        }
        assert_eq!(
            conn.opening.as_ref().map(|pending| pending.deferred.len()),
            Some(window),
            "a phone honouring the protocol fills the window and is not refused"
        );
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            input_msg("att-1", b"one byte past the window"),
        )
        .await;
        assert!(
            refused(&sink),
            "input past the credit window must be refused at {window} frames, \
             nowhere near the {MAX_DEFERRED_TERMINAL_FRAMES}-frame bound, got {:?}",
            sink.0
        );

        // And bytes that arrive somewhere other than the payload. A resize
        // carries no input at all, so the credit window never sees it; only a
        // bound on what the queue holds does. The id is where those bytes ride,
        // and nothing outside the attach arm judges an id's shape — which is
        // exactly why the queue has to be bounded by what it holds rather than
        // by what a well-formed frame would have held.
        let (mut conn, mut sink) = parked(&daemon, &device).await;
        let bloated = || ClientMessage::TerminalResize {
            attachment_id: "x".repeat(MAX_DEFERRED_TERMINAL_BYTES / 2),
            cols: 80,
            rows: 24,
        };
        for _ in 0..2 {
            drive(
                &daemon,
                &mut conn,
                &mut sink,
                "unused",
                Some(&device),
                bloated(),
            )
            .await;
        }
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            bloated(),
        )
        .await;
        assert!(
            refused(&sink),
            "frames whose bytes are not input must be bounded too, got {:?}",
            sink.0
        );
        assert!(conn.opening.is_none(), "and only the attach pays for them");
        drop(held);
    }

    /// A frame the queue cannot size is refused, never parked for free.
    ///
    /// `the_deferred_queue_is_bounded_by_bytes_and_not_only_by_frames` pins the
    /// half of the bound that holds today: the parked variants are costed by
    /// their bytes. This pins the half that has to hold tomorrow. The catch-all
    /// in [`deferred_cost`] used to answer `0`, so any frame it did not know how
    /// to size was admitted for nothing — and a byte ceiling that a frame can
    /// pass at zero cost is a bound on nothing.
    ///
    /// `Sessions` is the stand-in, and standing in is the whole of its job:
    /// `is_terminal_message` decides what `handle_terminal` is given, so nothing
    /// routes it here today. That is exactly the shape of the thing this
    /// guards against — a variant that is not routed to the queue *yet*.
    #[tokio::test]
    async fn a_frame_the_queue_cannot_size_is_refused_rather_than_parked_for_free() {
        fn opening() -> Opening {
            Opening {
                attachment_id: "att-1".into(),
                session_uid: some_uid(),
                cols: 80,
                rows: 24,
                // Never awaited: what is under test is the queue's arithmetic,
                // and the open behind it only has to exist.
                task: OpenTask(tokio::spawn(std::future::pending())),
                output_credit: Arc::new(tokio::sync::Semaphore::new(0)),
                granted: 0,
                out: crate::terminal::output_channel(4).1,
                acks: tokio::sync::mpsc::unbounded_channel().1,
                deferred: VecDeque::new(),
                deferred_bytes: 0,
                deferred_input: 0,
            }
        }

        // On an *empty* queue, which is what makes this a statement about the
        // frame's cost rather than about the ceiling: a frame costed honestly
        // at anything under 64 KiB would be admitted here.
        let mut pending = opening();
        assert!(
            pending.defer(ClientMessage::Sessions).is_err(),
            "a frame `deferred_cost` cannot size must fail the attach, not be \
             stored at zero cost against a ceiling it can never reach"
        );
        assert!(
            pending.deferred.is_empty() && pending.deferred_bytes == 0,
            "and the refusal must leave the queue exactly as it found it"
        );

        // The one terminal variant that is never parked, for the same reason:
        // it is routed out before `defer` today, and if that ever stops being
        // true it must cost more than the queue will admit rather than nothing.
        let mut pending = opening();
        assert!(
            pending
                .defer(ClientMessage::TerminalAttach {
                    attachment_id: "att-2".into(),
                    session_uid: some_uid(),
                    cols: 80,
                    rows: 24,
                    output_credit: 1,
                })
                .is_err(),
            "an attach that reached the queue must be refused rather than parked"
        );
    }

    /// A terminal is refused on a connection whose bytes are not private in
    /// transit, and the capability says so before the phone ever asks.
    ///
    /// The configuration this is about is real and supported: `ws_bind` onto a
    /// LAN with `ws_allow_plaintext`, which the daemon honours with a warning.
    /// Everything else the protocol carries crosses it on the operator's say-so;
    /// a live shell's keystrokes do not, and the deleted SSH terminal was
    /// encrypted in every configuration there was.
    #[tokio::test]
    async fn a_terminal_needs_an_encrypted_connection() {
        use protocol::ws::terminal_close as tc;

        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals::default();
        let mut sink = CollectSink::default();

        drive_over(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            false,
            attach_msg(
                "att-1",
                &some_uid(),
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalClosed { attachment_id, code, reason }
                if attachment_id == "att-1"
                    && code == tc::NOT_AUTHORISED
                    && reason.contains("encrypted")),
            "a cleartext connection is refused a terminal, got {:?}",
            sink.last()
        );
        assert!(
            conn.opening.is_none() && conn.terminal.is_none(),
            "and nothing was spawned for it"
        );
    }

    /// The `terminal_pty` capability answers the same question the attach
    /// handler does, over every transport the daemon serves.
    ///
    /// Advertising a terminal the attach then refuses would be this app's rule
    /// broken from the inside: an action it cannot perform is not offered. The
    /// `ws_allow_plaintext` row is the one that used to be wrong — the listener
    /// admitted the connection, so `terminal_allowed` said yes, and `tls_active`
    /// was never consulted at all.
    #[tokio::test]
    async fn the_terminal_capability_is_offered_only_over_a_private_transport() {
        let (daemon, _device) = daemon_with_a_device();
        let paired = || {
            AuthOutcome::Device(Box::new(crate::store::DeviceRow {
                device_id: "dev-1".into(),
                name: "iPhone".into(),
                created_at: protocol::time::now_rfc3339(),
                last_seen_at: None,
                revoked_at: None,
            }))
        };
        let offered = |tls_active: bool, trust: PlaintextTrust, outcome: AuthOutcome| {
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
                    ServerMessage::HelloAck { capabilities, .. } => capabilities.terminal_pty,
                    other => panic!("hello_ack must answer an ack, got {other:?}"),
                }
            }
        };

        assert!(
            offered(false, PlaintextTrust::TrustedPath, paired()).await,
            "loopback and this Mac's tailnet address are private already"
        );
        assert!(
            offered(true, PlaintextTrust::RequireTls, paired()).await,
            "and so is wss, on any listener"
        );
        assert!(
            !offered(false, PlaintextTrust::OperatorAllowed, paired()).await,
            "ws_allow_plaintext admits the connection; it does not make the LAN \
             private, and a terminal is shell-equivalent authority"
        );
        assert!(
            !offered(true, PlaintextTrust::TrustedPath, AuthOutcome::Static).await,
            "and the static bootstrap token never gets one, encrypted or not"
        );
    }

    /// A detach, a resize and an input naming *another* terminal leave the live
    /// one alone.
    ///
    /// Each of the three is filtered by attachment id, and each filter is one
    /// line that a test naming the happy path never touches: a detach that
    /// ignored the id would tear down the terminal the phone is still using on a
    /// frame meant for one it has already closed, and a resize that ignored it
    /// would reshape a pane on someone else's say-so.
    #[tokio::test]
    async fn terminal_frames_naming_another_attachment_leave_this_one_alone() {
        use protocol::ws::{terminal_close as tc, TERMINAL_INITIAL_INPUT_CREDIT};

        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals {
            terminal: Some(TerminalConn {
                attachment_id: "att-live".into(),
                handle: crate::terminal::TerminalHandle::inert_stub(),
                output_credit: Arc::new(tokio::sync::Semaphore::new(0)),
                outstanding: 0,
                input_credit: TERMINAL_INITIAL_INPUT_CREDIT,
            }),
            out: Some(crate::terminal::output_channel(4).1),
            acks: Some(tokio::sync::mpsc::unbounded_channel().1),
            opening: None,
        };
        let mut sink = CollectSink::default();

        // A detach for an id this connection does not have: silently ignored,
        // and the live terminal survives it.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            ClientMessage::TerminalDetach {
                attachment_id: "att-ghost".into(),
            },
        )
        .await;
        assert!(
            conn.terminal.is_some() && conn.out.is_some() && conn.acks.is_some(),
            "a detach naming another terminal tore this one down"
        );
        assert!(
            sink.0.is_empty(),
            "and it answered nothing, got {:?}",
            sink.0
        );

        // (Resize is the third of these filters, but on a stub it has nothing to
        // show for itself — `a_resize_applies_only_to_this_terminal_and_only_in_range`
        // drives it against a real pane, where the size is observable.)

        // Input is the one that answers, because a phone typing into a terminal
        // that is not there has to be told — and it must still not disturb the
        // one that is.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            input_msg("att-ghost", b"x"),
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalClosed { attachment_id, code, .. }
                if attachment_id == "att-ghost" && code == tc::PROTOCOL_ERROR),
            "got {:?}",
            sink.last()
        );
        assert!(
            conn.terminal.is_some() && conn.out.is_some() && conn.acks.is_some(),
            "the live terminal was torn down by a frame naming another"
        );

        // And the real detach still works, so none of the above is passing by
        // the whole path being broken.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            ClientMessage::TerminalDetach {
                attachment_id: "att-live".into(),
            },
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalClosed { attachment_id, code, .. }
                if attachment_id == "att-live" && code == tc::DETACHED),
            "got {:?}",
            sink.last()
        );
        assert!(conn.terminal.is_none() && conn.out.is_none() && conn.acks.is_none());
    }

    /// A resize is applied only for the terminal it names, and only when the
    /// geometry is one the wire admits.
    ///
    /// A resize that skipped the id check would let a frame for a terminal the
    /// phone has already closed reshape the one it is looking at; one that
    /// skipped the bounds would hand tmux a geometry the protocol says is out of
    /// range.
    ///
    /// The two refusals are read at the carrier's own boundary rather than off
    /// the pane, and that is deliberate: measured on tmux 3.7b, a
    /// `refresh-client` past the wire's range is clamped by the server, so the
    /// pane's size cannot tell a resize this connection refused from one tmux
    /// refused. What this file is answerable for is whether the resize was
    /// forwarded at all. The accepted case is still checked against the real
    /// pane, so the refusals are refusals and not a resize path that never
    /// worked.
    #[tokio::test]
    async fn a_resize_applies_only_to_this_terminal_and_only_in_range() {
        let _serial = crate::terminal::fixture_test_guard().await;
        let Some(fx) = crate::terminal::tests::Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals::default();
        let mut sink = CollectSink::default();

        attach(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            attach_msg(
                "att-1",
                &fx.uid,
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalAttached { .. }),
            "the attach is acknowledged, got {:?}",
            sink.last()
        );

        /// The window's geometry, as tmux reports it.
        fn geometry(fx: &crate::terminal::tests::Fixture) -> String {
            let out = fx.tmux(&[
                "list-windows",
                "-t",
                "=cc-term",
                "-F",
                "#{window_width}x#{window_height}",
            ]);
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        assert_eq!(geometry(&fx), "80x24", "the fixture starts at its own size");
        let asked_for = |conn: &Locals| {
            conn.terminal
                .as_ref()
                .expect("the terminal is still attached")
                .handle
                .desired_size()
        };
        assert_eq!(
            asked_for(&conn),
            (80, 24),
            "the attach asked for its own size"
        );

        // Another terminal's id: never forwarded.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            ClientMessage::TerminalResize {
                attachment_id: "att-ghost".into(),
                cols: 120,
                rows: 40,
            },
        )
        .await;
        assert_eq!(
            asked_for(&conn),
            (80, 24),
            "a resize naming another terminal was forwarded to this one"
        );

        // Past the wire's maximum columns: never forwarded either, and not fatal
        // — the terminal keeps working at the size it had.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            ClientMessage::TerminalResize {
                attachment_id: "att-1".into(),
                cols: protocol::ws::TERMINAL_MAX_COLS + 1,
                rows: 40,
            },
        )
        .await;
        assert_eq!(
            asked_for(&conn),
            (80, 24),
            "an out-of-range resize was forwarded to the carrier"
        );
        assert!(
            matches!(sink.last(), ServerMessage::TerminalAttached { .. }),
            "a resize that cannot be honoured is ignored, not fatal — got {:?}",
            sink.last()
        );

        // And the one that is this terminal's, and in range, is forwarded *and*
        // lands on the pane — so the two refusals above are refusals and not a
        // resize path that never worked.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            &fx.sock,
            Some(&device),
            ClientMessage::TerminalResize {
                attachment_id: "att-1".into(),
                cols: 120,
                rows: 40,
            },
        )
        .await;
        assert_eq!(asked_for(&conn), (120, 40));
        let mut landed = false;
        for _ in 0..30 {
            if geometry(&fx) == "120x40" {
                landed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            landed,
            "a well-formed resize for this terminal never reached the pane"
        );

        if let Some(live) = conn.terminal.take() {
            live.teardown();
        }
    }

    /// An attach that fails is reported with the failure's *own* close code, so
    /// the phone can tell a session that is not hosted from a tmux that is not
    /// answering — and can decide whether retrying is worth anything.
    ///
    /// Reached by attaching a well-formed uid against a socket no server is on,
    /// which is the one refusal that happens inside the open rather than in the
    /// wiring ahead of it.
    #[tokio::test]
    async fn a_failed_attach_is_reported_with_its_own_close_code() {
        // Held as an *observer* of two process-wide overrides rather than as a
        // setter of either: this drives a real `TerminalHandle::open`, so it is
        // cut short by a shortened `ATTACH_DEADLINE` and parked by a closed
        // `OPEN_GATE`, and either one turns the refusal it is about into
        // `tmux_unavailable`. A guard only excludes when both sides hold it.
        let _serial = crate::terminal::fixture_test_guard().await;
        // Only a present tmux can answer `session_not_hosted`: it takes the
        // running binary to ask the server-less socket and be told no session
        // is there. Absent the binary the open returns `tmux_unavailable`, a
        // true answer about that machine but not the distinction this proves, so
        // skip where the fixture tests skip rather than assert one this machine
        // cannot draw.
        if protocol::tmux::tmux_bin().is_none() {
            eprintln!("skipped: no tmux");
            return;
        }
        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals::default();
        let mut sink = CollectSink::default();

        attach(
            &daemon,
            &mut conn,
            &mut sink,
            "cc-no-server-here",
            Some(&device),
            attach_msg(
                "att-1",
                &some_uid(),
                protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
        )
        .await;

        let ServerMessage::TerminalClosed {
            attachment_id,
            code,
            reason,
        } = sink.last()
        else {
            panic!("the attach must be answered, got {:?}", sink.last());
        };
        assert_eq!(attachment_id, "att-1");
        assert_eq!(
            code,
            protocol::ws::terminal_close::SESSION_NOT_HOSTED,
            "a uid no live session carries is `session_not_hosted`, not a \
             blanket failure — got {code} ({reason})"
        );
        assert!(
            conn.terminal.is_none() && conn.out.is_none() && conn.acks.is_none(),
            "nothing is installed for a refused attach"
        );
    }

    /// Input beyond the granted window is an over-credit protocol violation
    /// that closes the terminal — proven without tmux via the closed stub, so
    /// the ledger, not any live pane, is what is under test. A one-byte frame
    /// stays under the chunk cap, so only the credit check can reject it.
    #[tokio::test]
    async fn input_beyond_the_credit_window_is_refused() {
        use protocol::ws::terminal_close as tc;

        let (daemon, device) = daemon_with_a_device();
        let mut conn = Locals {
            terminal: Some(TerminalConn {
                attachment_id: "att-1".into(),
                handle: crate::terminal::TerminalHandle::inert_stub(),
                output_credit: Arc::new(tokio::sync::Semaphore::new(0)),
                outstanding: 0,
                // A tiny window so a legal-sized frame can still overrun it.
                input_credit: 4,
            }),
            out: Some(crate::terminal::output_channel(4).1),
            acks: Some(tokio::sync::mpsc::unbounded_channel().1),
            opening: None,
        };
        let mut sink = CollectSink::default();

        // Four bytes fit the window and are accepted (no reply — credit returns
        // only through the ack arm, which the closed stub never drives).
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            input_msg("att-1", b"1234"),
        )
        .await;
        assert!(conn.terminal.is_some(), "an in-window frame is accepted");
        // The window is now spent; one more byte overruns it and closes.
        drive(
            &daemon,
            &mut conn,
            &mut sink,
            "unused",
            Some(&device),
            input_msg("att-1", b"5"),
        )
        .await;
        assert!(
            matches!(sink.last(), ServerMessage::TerminalClosed { code, .. } if code == tc::PROTOCOL_ERROR),
            "input over the window is refused, got {:?}",
            sink.last()
        );
        assert!(conn.terminal.is_none() && conn.out.is_none() && conn.acks.is_none());
    }

    /// The wiring refuses malformed or unauthorised terminal requests without
    /// ever spawning a client, so none of these needs tmux.
    #[tokio::test]
    async fn the_wiring_refuses_bad_terminal_requests() {
        use protocol::ws::terminal_close as tc;
        use protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT;

        let (daemon, device) = daemon_with_a_device();

        // Each case runs against fresh, empty state; the socket is unroutable,
        // so any request that reached the carrier would fail loudly instead of
        // being refused where the test claims.
        async fn refuse(
            daemon: &Arc<Daemon>,
            device_id: Option<&str>,
            message: ClientMessage,
            want: &str,
        ) {
            let mut conn = Locals::default();
            let mut sink = CollectSink::default();
            drive(
                daemon,
                &mut conn,
                &mut sink,
                "cc-does-not-exist",
                device_id,
                message,
            )
            .await;
            assert!(
                conn.terminal.is_none() && conn.opening.is_none(),
                "nothing may open, and nothing may be left opening"
            );
            assert!(
                matches!(sink.last(), ServerMessage::TerminalClosed { code, .. } if code == want),
                "expected {want}, got {:?}",
                sink.last()
            );
        }

        let uid = some_uid();
        // The static bootstrap token may never open a terminal —
        // shell-equivalent authority belongs only to a paired device.
        refuse(
            &daemon,
            None,
            attach_msg("a", &uid, TERMINAL_INITIAL_OUTPUT_CREDIT),
            tc::NOT_AUTHORISED,
        )
        .await;
        // Degenerate geometry.
        refuse(
            &daemon,
            Some(&device),
            ClientMessage::TerminalAttach {
                attachment_id: "a".into(),
                session_uid: uid.clone(),
                cols: 1,
                rows: 24,
                output_credit: TERMINAL_INITIAL_OUTPUT_CREDIT,
            },
            tc::PROTOCOL_ERROR,
        )
        .await;
        // Zero output credit would deadlock the stream before it began.
        refuse(
            &daemon,
            Some(&device),
            attach_msg("a", &uid, 0),
            tc::PROTOCOL_ERROR,
        )
        .await;
        // An empty attachment id, and one past the length bound. Both halves of
        // that check need a case: with only the empty one, the bound itself is
        // never exercised and an id of any size gets as far as spawning an
        // attach — which the `opening` assertion above is what catches.
        refuse(
            &daemon,
            Some(&device),
            attach_msg("", &uid, TERMINAL_INITIAL_OUTPUT_CREDIT),
            tc::PROTOCOL_ERROR,
        )
        .await;
        refuse(
            &daemon,
            Some(&device),
            attach_msg(
                &"x".repeat(protocol::ws::MAX_ATTACHMENT_ID_BYTES + 1),
                &uid,
                TERMINAL_INITIAL_OUTPUT_CREDIT,
            ),
            tc::PROTOCOL_ERROR,
        )
        .await;
        // Input naming a terminal that was never opened.
        refuse(
            &daemon,
            Some(&device),
            input_msg("ghost", b"x"),
            tc::PROTOCOL_ERROR,
        )
        .await;
    }
}
