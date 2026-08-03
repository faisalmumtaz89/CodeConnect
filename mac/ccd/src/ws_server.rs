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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use protocol::event::{Event, EventKind, Source};
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

pub async fn serve(
    daemon: Arc<Daemon>,
    addr: SocketAddr,
    token: Arc<String>,
    tls: Option<TlsAcceptor>,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    match (&tls, daemon.config.tls_required) {
        (Some(_), true) => crate::log_info!("ws listening on wss://{addr} (plaintext refused)"),
        (Some(_), false) => crate::log_info!("ws listening on wss://{addr} (ws:// also accepted)"),
        (None, false) => crate::log_info!("ws listening on ws://{addr}"),
        // Said once, loudly, at startup. Otherwise the only evidence is a
        // per-connection debug line, and the operator sees a phone that cannot
        // connect with nothing in the log at the level they are running.
        (None, true) => crate::log_error!(
            "ws listening on {addr} but REFUSING EVERY CONNECTION: tls_required is set and \
             no certificate is available. Fix the certificate or set tls_required=false."
        ),
    }

    accept_loop(daemon, listener, token, tls).await
}

/// The accept loop, separated from the bind so a test can hand it a listener on
/// an ephemeral port and speak the real protocol to it.
async fn accept_loop(
    daemon: Arc<Daemon>,
    listener: TcpListener,
    token: Arc<String>,
    tls: Option<TlsAcceptor>,
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
                    if let Err(err) = accept(daemon, stream, token, tls).await {
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
) -> Result<()> {
    // Nagle costs latency on the small JSON frames this protocol is made of.
    let _ = stream.set_nodelay(true);

    let Some(acceptor) = tls else {
        if daemon.config.tls_required {
            // Configuration says TLS only, but we hold no certificate. Refusing
            // is the honest outcome: silently serving plaintext would contradict
            // what the operator asked for and what `capabilities.tls` reports.
            anyhow::bail!("tls_required is set but no certificate is available");
        }
        return handle_client(daemon, stream, token, false).await;
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
        return handle_client(daemon, stream, token, true).await;
    }

    if daemon.config.tls_required {
        crate::log_warn!("ws: refused a plaintext connection (tls_required is set)");
        return Ok(());
    }
    handle_client(daemon, stream, token, false).await
}

/// `0x16` is the TLS `handshake` content type. No HTTP request method begins
/// with that byte, so one byte separates the two protocols unambiguously.
fn is_tls_hello(first: u8) -> bool {
    first == 0x16
}

async fn handle_client<S>(
    daemon: Arc<Daemon>,
    stream: S,
    token: Arc<String>,
    tls_active: bool,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
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
    // Which device this connection belongs to, if any.
    //
    // Load-bearing for revocation: `hello` happens once and a phone then holds
    // the socket open for hours. Without re-checking, `codeconnect revoke` would take
    // effect only on the *next* connection — leaving a revoked device able to
    // keep answering approvals and typing into the session's TTY, which is the
    // opposite of what the operator just asked for.
    let mut device_id: Option<String> = None;
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.tick().await; // the first tick completes immediately

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
                            ssh_pubkey,
                            client_name,
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
                                    ssh_pubkey.as_deref(),
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
                            send(&mut sink, &hello_ack(&daemon, ack, tls_active)).await?;
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

                handle_message(&daemon, &mut sink, &mut watermarks, device_id.as_deref(), parsed)
                    .await?;
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
                                watermarks.insert(session_uid, seq);
                            }
                            Live::AlreadySent => {}
                            Live::Gap => {
                                crate::log_warn!(
                                    "ws: {} jumped from seq {watermark} to {}; resyncing from the log",
                                    event.session_uid, event.seq
                                );
                                let session_uid = event.session_uid.clone();
                                send(&mut sink, &ServerMessage::Event {
                                    event: gap_marker(&event, watermark),
                                }).await?;
                                replay(&daemon, &mut sink, &mut watermarks, &session_uid).await?;
                            }
                        }
                    }
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
                            replay(&daemon, &mut sink, &mut watermarks, &session_uid).await?;
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
                            // Best-effort courtesy so the phone can say why it
                            // was disconnected rather than showing a network
                            // error. The close happens whether or not it lands.
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
                    sink.send(Message::Ping(Vec::new())).await.context("keepalive failed")?;
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
        // A second hello is harmless; treat it as a no-op rather than an error.
        ClientMessage::Hello { .. } => {}
        ClientMessage::Ping => send(sink, &ServerMessage::Pong).await?,
        ClientMessage::RegisterPush { token, environment } => {
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
            match daemon.register_push(device_id, &token, &environment).await {
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
                replay(daemon, sink, watermarks, &row.session_uid).await?;
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
            let result = if !daemon.push.is_live() {
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
        } => {
            let result = daemon
                .send_text(
                    &session_id,
                    text,
                    request_id.as_deref(),
                    payload_hash.as_deref(),
                    submit,
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

/// Build the ack, attaching the new credentials when this hello was a pairing.
fn hello_ack(daemon: &Arc<Daemon>, outcome: AuthOutcome, tls_active: bool) -> ServerMessage {
    let (device_token, device_id, device_name, ssh_key_installed) = match outcome {
        AuthOutcome::Paired {
            device_id,
            device_name,
            token,
            ssh_key_installed,
        } => (
            Some(token),
            Some(device_id),
            Some(device_name),
            Some(ssh_key_installed),
        ),
        // An already-paired device is told what it is known as, so a rename on
        // the Mac shows up on the phone without a re-pair.
        AuthOutcome::Device(device) => (
            None,
            Some(device.device_id.clone()),
            Some(device.name.clone()),
            Some(device.ssh_key_installed),
        ),
        _ => (None, None, None, None),
    };
    ServerMessage::HelloAck {
        protocol_version: protocol::PROTOCOL_VERSION,
        protocol_minor: protocol::PROTOCOL_MINOR,
        server_time: protocol::time::now_rfc3339(),
        capabilities: capabilities(daemon, tls_active),
        device_token,
        device_id,
        device_name,
        ssh_key_installed,
    }
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

/// Page the backlog out of the store, advancing the watermark as we go.
async fn replay<S>(
    daemon: &Arc<Daemon>,
    sink: &mut S,
    watermarks: &mut HashMap<String, u64>,
    session_uid: &str,
) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    loop {
        let from = watermarks.get(session_uid).copied().unwrap_or(0);
        let page = daemon
            .db
            .events_after(session_uid.to_string(), from, REPLAY_PAGE)
            .await?;
        if page.is_empty() {
            return Ok(());
        }
        for event in page {
            let seq = event.seq;
            send(sink, &ServerMessage::Event { event }).await?;
            watermarks.insert(session_uid.to_string(), seq);
        }
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

fn capabilities(daemon: &Arc<Daemon>, tls_active: bool) -> Capabilities {
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
        // able to tell those apart without version arithmetic.
        test_push: daemon.push.is_live(),
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
        push: daemon.push.is_live(),
        // What the listener holds, not what this connection used.
        tls: daemon.endpoint.tls,
        tls_active,
        diff: crate::git::git_bin(daemon.config.git_bin.as_deref()).is_some(),
        risk_class: true,
        session_uid: true,
        send_text_idempotent: true,
        prompt_identity: true,
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
    sink.send(Message::Text(text))
        .await
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
        let (daemon, device_id) = daemon_with_a_device();
        let mut cancellations = daemon.revocations_tx.subscribe();

        let outcome = daemon.revoke(&device_id, false).await.unwrap();
        assert!(outcome.token_revoked);
        assert_eq!(
            cancellations.try_recv().unwrap(),
            device_id,
            "the revoked device id must be published for open sockets to match on"
        );
    }

    #[tokio::test]
    async fn an_ssh_only_revoke_publishes_nothing() {
        // `codeconnect ssh-revoke` deliberately keeps the phone paired. Closing its
        // sockets would be a different operation from the one asked for.
        let (daemon, device_id) = daemon_with_a_device();
        let mut cancellations = daemon.revocations_tx.subscribe();
        let outcome = daemon.revoke(&device_id, true).await.unwrap();
        assert!(!outcome.token_revoked);
        assert!(outcome.device.is_active());
        assert!(matches!(
            cancellations.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    fn ip(last: u8) -> std::net::IpAddr {
        std::net::IpAddr::from([100, 64, 0, last])
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
    }

    async fn live_server(config: protocol::config::Config) -> (LiveServer, String) {
        let (daemon, device_id) = daemon_with_a_device_and(config);
        // Bound here rather than inside `serve` so the test learns the port.
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let token = Arc::new("static-token-for-tests".to_string());
        {
            let daemon = Arc::clone(&daemon);
            let token = Arc::clone(&token);
            tokio::spawn(async move { accept_loop(daemon, listener, token, None).await });
        }
        (
            LiveServer {
                addr,
                daemon,
                token,
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
    }

    /// The next text frame, decoded. `None` at close.
    async fn next_json(
        socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    ) -> Option<serde_json::Value> {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("the server must answer within 5s")
            {
                Some(Ok(Message::Text(text))) => return Some(serde_json::from_str(&text).unwrap()),
                Some(Ok(Message::Close(_))) | None => return None,
                Some(Ok(_)) => continue,
                Some(Err(_)) => return None,
            }
        }
    }

    fn daemon_with_a_device_and(config: protocol::config::Config) -> (Arc<Daemon>, String) {
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

        server.daemon.revoke(&device_id, false).await.unwrap();

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
}
