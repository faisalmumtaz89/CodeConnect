//! One client connection.
//!
//! The handshake is `ccd`'s, refusal for refusal: the version is checked before
//! the credential, a bad credential gets one opaque `unauthorized` and the
//! socket closes, and a client that never says hello is dropped on the same
//! ten-second deadline. Those exact shapes are the contract the phone maps to a
//! terminal `.failed` — anything else and it retries a refusal for ever behind a
//! spinner instead of telling the reviewer what is wrong.
//!
//! Replay is `ccd`'s too. The broadcast receiver is created before any backlog
//! read, so an event appended during a replay either was already in the log when
//! it was read or arrives on the broadcast afterwards; a per-run watermark drops
//! what the replay already carried. A client that falls off the ring is sent an
//! explicit `resync` marker rather than being quietly skipped.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use protocol::event::{Event, EventKind, Source};
use protocol::ws::{
    AnswerPath, Capabilities, ClientMessage, ServerMessage, MAX_CLIENT_MESSAGE_BYTES,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

use crate::fleet::Fleet;
use crate::script::SAMPLE_DIFF;

/// `ccd::ws_server::HANDSHAKE_TIMEOUT`. A client that has not authenticated
/// within it is closed. Also the bound on reading the request head that decides
/// whether a connection is a health check, for the same reason it bounds the
/// rest: an opened socket that says nothing must not hold a task for ever.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// `ccd::ws_server::KEEPALIVE`.
const KEEPALIVE: Duration = Duration::from_secs(30);
/// `ccd::ws_server::WRITE_DEADLINE`. A peer that cannot take a frame in this
/// window has stopped reading, and the connection is closed rather than left
/// wedged inside a send that can never be cancelled.
const WRITE_DEADLINE: Duration = Duration::from_secs(20);
/// The name this server reports itself under in `hello_ack`.
const DEVICE_NAME: &str = "Review demo";

pub async fn handle<S>(fleet: Arc<Fleet>, stream: S, token: Arc<String>) -> Result<()>
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
    let mut events = fleet.subscribe_events();
    let mut watermarks: HashMap<String, u64> = HashMap::new();
    let mut backlog = Backlog::default();
    let mut history = History::default();
    let mut authed = false;
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
                        ClientMessage::Hello { protocol_version, token: given, .. } => {
                            // Checked before the credential, exactly as `ccd`
                            // does it: a peer on another major has a different
                            // idea of what these messages mean, and refusing
                            // first means it never reaches the token comparison
                            // and so cannot use this to probe credentials.
                            if *protocol_version != protocol::PROTOCOL_VERSION {
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
                            // One opaque message for every failure — a missing
                            // token, a wrong one, or a pairing code this server
                            // has no way to honour — so a peer that cannot
                            // authenticate learns nothing about which.
                            if given.as_deref() != Some(token.as_str()) {
                                send(&mut sink, &ServerMessage::Error {
                                    code: "unauthorized".into(),
                                    message: "invalid credentials".into(),
                                }).await?;
                                return Ok(());
                            }
                            authed = true;
                            send(&mut sink, &hello_ack()).await?;
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

                handle_message(&fleet, &mut sink, &mut watermarks, &mut backlog, &mut history, parsed)
                    .await?;
            }

            // One step of a replay, when one is in progress. Ready rather than
            // pending, so it competes for each pass with every other arm instead
            // of taking or ceding priority outright.
            _ = std::future::ready(()), if backlog.in_progress() => {
                backlog.step(&fleet, &mut sink, &mut watermarks).await?;
                tokio::task::yield_now().await;
            }

            // One step of an earlier page, competing for each pass the same way.
            _ = std::future::ready(()), if history.in_progress() => {
                history.step(&fleet, &mut sink).await?;
                tokio::task::yield_now().await;
            }

            live = events.recv(), if authed => {
                match live {
                    Ok(event) => {
                        let Some(&watermark) = watermarks.get(&event.session_uid) else { continue };
                        match live_delivery(event.seq, watermark) {
                            Live::Deliver => {
                                let (uid, seq) = (event.session_uid.clone(), event.seq);
                                send(&mut sink, &ServerMessage::Event { event }).await?;
                                watermarks.insert(uid, seq);
                            }
                            // A replay already carried it. Normal, and the only
                            // reason the watermark exists.
                            Live::AlreadySent => {}
                            // Never skipped to: the log has it, so the replay
                            // reads it from there. Announced only when no replay
                            // for that run is already pending, which would
                            // otherwise report one hole twice.
                            Live::Gap => {
                                if !backlog.replaying(&event.session_uid) {
                                    send(&mut sink, &ServerMessage::Event {
                                        event: gap_marker(&event, watermark),
                                    }).await?;
                                    backlog.begin(event.session_uid.clone());
                                }
                            }
                        }
                    }
                    // Falling off the ring is a fact about *this* connection that
                    // happened, so the marker is unconditional: there is nothing
                    // to infer and nothing to be wrong about.
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        for uid in watermarks.keys().cloned().collect::<Vec<String>>() {
                            let name = fleet.name_of(&uid);
                            send(&mut sink, &ServerMessage::Event {
                                event: resync_marker(&uid, &name, skipped),
                            }).await?;
                            backlog.begin(uid);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }

            _ = keepalive.tick() => {
                if authed {
                    write_bounded(&mut sink, Message::Ping(Vec::new()))
                        .await
                        .context("keepalive failed")?;
                }
            }

            () = &mut auth_deadline, if !authed => return Ok(()),
        }
    }
}

async fn handle_message<S>(
    fleet: &Arc<Fleet>,
    sink: &mut S,
    watermarks: &mut HashMap<String, u64>,
    backlog: &mut Backlog,
    history: &mut History,
    message: ClientMessage,
) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    match message {
        // A second hello is harmless; treat it as a no-op rather than an error.
        ClientMessage::Hello { .. } => {}
        // The demo daemon hosts scripted Claude runs only; compose is a Codex operation
        // and is refused honestly rather than dropped, exactly as its sibling below is.
        ClientMessage::Compose {
            session_id,
            request_id,
            ..
        } => {
            send(
                sink,
                &ServerMessage::ComposeResult {
                    session_id,
                    request_id,
                    result: protocol::ws::ComposeResult::Rejected {
                        reason: "the demo daemon hosts Claude sessions only".into(),
                    },
                },
            )
            .await?;
        }
        // The demo daemon hosts scripted Claude runs only; interrupt is a Codex
        // operation and is refused honestly rather than dropped.
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
                        reason: "the demo daemon hosts Claude sessions only".into(),
                    },
                },
            )
            .await?
        }
        ClientMessage::Ping => send(sink, &ServerMessage::Pong).await?,
        ClientMessage::Sessions => {
            send(
                sink,
                &ServerMessage::Sessions {
                    sessions: fleet.sessions(),
                },
            )
            .await?
        }
        ClientMessage::Subscribe {
            session_id,
            after_seq,
        } => match fleet.resolve(&session_id) {
            Some(uid) => {
                watermarks.insert(uid.clone(), after_seq);
                backlog.begin(uid);
            }
            None => {
                send(
                    sink,
                    &ServerMessage::Error {
                        code: "unknown_session".into(),
                        message: format!("unknown session {session_id}"),
                    },
                )
                .await?
            }
        },
        // `ccd`'s semantics: the newest events below the mark, capped, then one
        // end — with the reason when the run is unknown.
        ClientMessage::History {
            session_id,
            before_seq,
            limit,
            request_id,
        } => history.ask(HistoryRequest::new(
            fleet.resolve(&session_id),
            session_id,
            before_seq,
            limit.min(protocol::ws::MAX_HISTORY_PAGE) as usize,
            request_id,
        )),
        ClientMessage::Unsubscribe { session_id } => {
            if let Some(uid) = fleet.resolve(&session_id) {
                watermarks.remove(&uid);
                backlog.drop_session(&uid);
            }
            // Also verbatim, in case the reference was already a uid for a run
            // this server has since forgotten.
            watermarks.remove(&session_id);
            backlog.drop_session(&session_id);
        }
        ClientMessage::Answer {
            request_id,
            payload_hash,
            decision,
            session_id,
        } => {
            let result = fleet.answer(
                &request_id,
                &payload_hash,
                decision,
                session_id.as_deref(),
                std::time::Instant::now(),
            );
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
            complete_native_confirmation: _,
        } => {
            let result = fleet.send_text(
                &session_id,
                text,
                request_id.as_deref(),
                payload_hash.as_deref(),
                submit,
            );
            // `session_id` echoed exactly as it arrived: the client keys its
            // waiter on the string it chose, uid or name.
            send(sink, &ServerMessage::SendTextResult { session_id, result }).await?;
        }
        ClientMessage::GetDiff { session_id } => match fleet.resolve(&session_id) {
            Some(_) => {
                send(
                    sink,
                    &ServerMessage::Diff {
                        session_id,
                        unified: SAMPLE_DIFF.to_string(),
                        truncated: false,
                        captured_at: protocol::time::now_rfc3339(),
                        note: None,
                    },
                )
                .await?
            }
            None => {
                send(
                    sink,
                    &ServerMessage::Error {
                        code: "unknown_session".into(),
                        message: format!("unknown session {session_id}"),
                    },
                )
                .await?
            }
        },

        // Everything below is a capability this server does not advertise. Each
        // one is still *answered*, in the shape the wire defines for it, because
        // every reply the client waits on is keyed by a correlation id and a
        // request that goes unanswered is twenty seconds of spinner rather than
        // a sentence.
        ClientMessage::Capture { session_id, .. } => {
            send(
                sink,
                &ServerMessage::CaptureResult {
                    session_id,
                    text: "This demonstration daemon runs no terminal, so there is no pane to \
                           capture."
                        .into(),
                },
            )
            .await?
        }
        ClientMessage::GetCommandCatalog { session_id } => {
            send(
                sink,
                &ServerMessage::CommandCatalog {
                    session_id,
                    result: protocol::ws::CommandCatalogResult::Unavailable {
                        reason: "this demonstration daemon has no Claude Code binary to read a \
                                 command list from"
                            .into(),
                    },
                },
            )
            .await?
        }
        ClientMessage::DeleteSession { session_uid } => {
            send(
                sink,
                &ServerMessage::DeleteSessionResult {
                    session_uid,
                    result: protocol::ws::DeleteSessionResult::Failed {
                        message: "this demonstration daemon does not remove runs".into(),
                    },
                },
            )
            .await?
        }
        // Exactly what `ccd` answers when it holds no APNs key.
        ClientMessage::TestPush { request_id } => {
            send(
                sink,
                &ServerMessage::TestPushResult {
                    request_id,
                    result: protocol::ws::TestPushResult::PushUnconfigured,
                },
            )
            .await?
        }
        // And exactly what `ccd` answers a connection with no device row.
        ClientMessage::RegisterPush { .. } => {
            send(
                sink,
                &ServerMessage::Error {
                    code: "no_device".into(),
                    message: "push registration needs a paired device; this connection is on the \
                              bootstrap token"
                        .into(),
                },
            )
            .await?
        }
        // A live terminal is shell-equivalent authority. There is no shell here
        // and the capability says so; the refusal is enforced again anyway,
        // because a client that ignored the capability must still not get one.
        ClientMessage::TerminalAttach { attachment_id, .. }
        | ClientMessage::TerminalInput { attachment_id, .. }
        | ClientMessage::TerminalResize { attachment_id, .. }
        | ClientMessage::TerminalCredit { attachment_id, .. }
        | ClientMessage::TerminalDetach { attachment_id } => {
            send(
                sink,
                &ServerMessage::TerminalClosed {
                    attachment_id,
                    code: protocol::ws::terminal_close::NOT_AUTHORISED.into(),
                    reason: "this demonstration daemon serves no terminal".into(),
                },
            )
            .await?
        }
    }
    Ok(())
}

/// What this server can actually do, reported honestly.
///
/// `tls` and `tls_active` are false because they are facts about *this* socket,
/// which is plain `ws` — the deployment terminates TLS at its edge and speaks
/// cleartext to this process over loopback. Claiming otherwise is not a
/// cosmetic lie: the phone reconnects over `wss://` the moment it sees `tls`
/// true on a plaintext link, and would flap for ever.
fn capabilities() -> Capabilities {
    Capabilities {
        // An answer reaches the script the moment it arrives; nothing can drop
        // it in between.
        can_approve_reliably: true,
        fail_mode: "fail_open".into(),
        answer_path: AnswerPath::HookReturn,
        hold_secs: 0,
        send_text: true,
        // No pane, no shell, no Claude Code binary, no APNs key, and nothing
        // destructive: an action this server cannot perform is not offered.
        capture: false,
        delete_session: false,
        test_push: false,
        push: false,
        push_relay: false,
        tls: false,
        tls_active: false,
        diff: true,
        risk_class: true,
        session_uid: true,
        send_text_idempotent: true,
        // True and provable: a card is answerable exactly while this server is
        // holding it, and resolving it takes it out of that set.
        prompt_identity: true,
        command_catalog: false,
        slash_composer_recovery: false,
        terminal_pty: false,
        // No Codex link and no turn to stop: an action this server cannot perform
        // is not offered.
        codex_interrupt: false,
        codex_compose: false,
        // Omitted while it would only name the Claude floor — same as ccd, so the
        // demo does not make a phone render a diagnostic capability row that
        // means nothing yet. An empty list is skipped on the wire.
        supported_agents: Vec::new(),
        question_card: false,
        // Its cards are answered by this server and nothing else: there is no
        // screen to type into.
        hook_only_approvals: true,
        history_pages: true,
    }
}

fn hello_ack() -> ServerMessage {
    ServerMessage::HelloAck {
        protocol_version: protocol::PROTOCOL_VERSION,
        protocol_minor: protocol::PROTOCOL_MINOR,
        server_time: protocol::time::now_rfc3339(),
        capabilities: capabilities(),
        // No pairing and no device row: this server has one static credential
        // and mints nothing. The name is what the phone lists this link under.
        device_token: None,
        device_id: None,
        device_name: Some(DEVICE_NAME.into()),
        // No push of any kind, so there is no token and no environment to have
        // an opinion about.
        push_environment: None,
    }
}

/// What to do with a live event, given what this connection has already had.
#[derive(Debug, PartialEq, Eq)]
enum Live {
    Deliver,
    AlreadySent,
    Gap,
}

/// Live delivery is *successor only*.
///
/// `seq > watermark` looks equivalent and is not: accepting 2 when 1 has not
/// been sent advances the watermark past 1, and 1 is then discarded by that same
/// test when it arrives — silently, for the rest of the connection's life.
fn live_delivery(seq: u64, watermark: u64) -> Live {
    match seq.cmp(&(watermark + 1)) {
        std::cmp::Ordering::Equal => Live::Deliver,
        std::cmp::Ordering::Less => Live::AlreadySent,
        std::cmp::Ordering::Greater => Live::Gap,
    }
}

/// A replay this connection is part-way through.
///
/// Stepped rather than run to completion, so a long backlog does not stop the
/// connection answering a ping or an approval while it pages out.
#[derive(Default)]
struct Backlog {
    sessions: std::collections::VecDeque<String>,
    page: std::collections::VecDeque<Event>,
}

/// How many events one page read takes. `ccd::ws_server::REPLAY_PAGE`.
const REPLAY_PAGE: usize = 500;

impl Backlog {
    fn begin(&mut self, uid: String) {
        if self.sessions.front() == Some(&uid) {
            self.page.clear();
        } else if !self.replaying(&uid) {
            self.sessions.push_back(uid);
        }
    }

    fn replaying(&self, uid: &str) -> bool {
        self.sessions.iter().any(|queued| queued == uid)
    }

    fn in_progress(&self) -> bool {
        !self.sessions.is_empty()
    }

    fn drop_session(&mut self, uid: &str) {
        if self.sessions.front().is_some_and(|front| front == uid) {
            self.page.clear();
        }
        self.sessions.retain(|queued| queued != uid);
    }

    /// Exactly one unit of work: send one event, read one page, or finish one
    /// run. Never more, because everything else this connection owes is waiting
    /// on the pass that follows.
    async fn step<S>(
        &mut self,
        fleet: &Arc<Fleet>,
        sink: &mut S,
        watermarks: &mut HashMap<String, u64>,
    ) -> Result<()>
    where
        S: SinkExt<Message> + Unpin,
        <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
    {
        let Some(uid) = self.sessions.front().cloned() else {
            return Ok(());
        };
        let Some(watermark) = watermarks.get(&uid).copied() else {
            // No watermark is no subscription. Replaying to one that no longer
            // exists would send a whole log the client just unsubscribed from.
            self.page.clear();
            self.sessions.pop_front();
            return Ok(());
        };
        if let Some(event) = self.page.pop_front() {
            // `seq > watermark`, deliberately not the successor test the live
            // path uses: a hole in the log would leave a page's head permanently
            // unsendable and the connection would livelock re-reading it.
            if event.seq > watermark {
                let seq = event.seq;
                send(sink, &ServerMessage::Event { event }).await?;
                watermarks.insert(uid, seq);
            }
            return Ok(());
        }
        // The page is spent. Read the next from the watermark now in force, so
        // events the live arm delivered meanwhile are not re-read.
        let page = fleet.events_after(&uid, watermark, REPLAY_PAGE);
        if page.is_empty() {
            self.sessions.pop_front();
        } else {
            self.page = page.into();
        }
        Ok(())
    }
}

/// The earlier pages this connection owes, stepped like [`Backlog`]: one page
/// read, one event or one end per pass. Requests are answered in arrival order,
/// a second for the same run behind the first, as `ccd` does. A page moves no
/// watermark.
#[derive(Default)]
struct History {
    requests: std::collections::VecDeque<HistoryRequest>,
}

struct HistoryRequest {
    /// As the client sent it.
    session_id: String,
    /// The run it resolved to when it arrived.
    uid: String,
    before_seq: u64,
    limit: usize,
    request_id: String,
    /// The page, oldest first; `None` until it is read.
    page: Option<std::collections::VecDeque<Event>>,
    /// The lowest seq sent so far.
    first_seq: u64,
    /// Why the page ends early: the run is unknown, or gone from the fleet.
    error: Option<String>,
}

impl HistoryRequest {
    fn new(
        uid: Option<String>,
        session_id: String,
        before_seq: u64,
        limit: usize,
        request_id: String,
    ) -> HistoryRequest {
        HistoryRequest {
            error: uid
                .is_none()
                .then(|| format!("unknown session {session_id}")),
            uid: uid.unwrap_or_default(),
            session_id,
            before_seq,
            limit,
            request_id,
            page: None,
            first_seq: before_seq,
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

    async fn step<S>(&mut self, fleet: &Arc<Fleet>, sink: &mut S) -> Result<()>
    where
        S: SinkExt<Message> + Unpin,
        <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
    {
        let Some(request) = self.requests.front_mut() else {
            return Ok(());
        };
        if request.error.is_none() {
            if let Some(event) = request.page.as_mut().and_then(|page| page.pop_front()) {
                request.first_seq = request.first_seq.min(event.seq);
                return send(sink, &ServerMessage::Event { event }).await;
            }
            // Asked before the page is read and again before its end is sent: a run
            // the fleet has dropped since ends its page with the reason, as `ccd`'s
            // removed run does.
            if fleet.resolve(&request.uid).is_none() {
                request.error = Some(format!(
                    "session {} is no longer in the fleet",
                    request.session_id
                ));
            } else if request.page.is_none() {
                request.page = Some(
                    fleet
                        .events_before(&request.uid, request.before_seq, request.limit)
                        .into(),
                );
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

async fn send<S>(sink: &mut S, message: &ServerMessage) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    let text = serde_json::to_string(message)?;
    write_bounded(sink, Message::Text(text)).await
}

async fn write_bounded<S>(sink: &mut S, message: Message) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    tokio::time::timeout(WRITE_DEADLINE, sink.send(message))
        .await
        .context("websocket write stalled past the deadline; peer not reading")?
        .context("websocket write failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::WebSocketStream;

    /// Long enough to be typed at the app's pairing field, which refuses
    /// anything shorter before it dials.
    const TOKEN: &str = "review-token-long-enough";

    type Client = WebSocketStream<TcpStream>;

    async fn start() -> (Arc<Fleet>, std::net::SocketAddr) {
        let fleet = Arc::new(Fleet::new(Instant::now()).expect("the shipped script must load"));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = Arc::clone(&fleet);
        tokio::spawn(async move {
            let _ = crate::serve(served, listener, Arc::new(TOKEN.to_string())).await;
        });
        (fleet, addr)
    }

    async fn connect(addr: std::net::SocketAddr) -> Client {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (ws, _) = tokio_tungstenite::client_async(format!("ws://{addr}/"), stream)
            .await
            .expect("the server must accept a websocket upgrade on any path");
        ws
    }

    async fn say(ws: &mut Client, message: serde_json::Value) {
        ws.send(Message::Text(message.to_string())).await.unwrap();
    }

    async fn hear(ws: &mut Client) -> serde_json::Value {
        loop {
            match ws.next().await.expect("the server closed early").unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    /// Did the server close, rather than leave the connection open?
    async fn closed(ws: &mut Client) -> bool {
        matches!(
            ws.next().await,
            None | Some(Err(_)) | Some(Ok(Message::Close(_)))
        )
    }

    async fn paired(addr: std::net::SocketAddr) -> Client {
        let mut ws = connect(addr).await;
        say(
            &mut ws,
            json!({
                "type": "hello",
                "protocol_version": protocol::PROTOCOL_VERSION,
                "token": TOKEN,
                "client_id": "test",
                "client_name": "CodeConnect iPhone",
            }),
        )
        .await;
        let ack = hear(&mut ws).await;
        assert_eq!(ack["type"], "hello_ack", "{ack}");
        ws
    }

    /// Step the script by hand. The clock is driven rather than read, so nothing
    /// advances behind an assertion.
    fn pump(fleet: &Fleet, clock: &mut Instant, steps: usize) {
        for _ in 0..steps {
            *clock += Duration::from_secs(120);
            fleet.tick(*clock);
        }
    }

    async fn seqs_of(ws: &mut Client, count: usize) -> Vec<u64> {
        let mut seqs = Vec::new();
        while seqs.len() < count {
            let frame = hear(ws).await;
            if frame["type"] == "event" {
                seqs.push(frame["event"]["seq"].as_u64().unwrap());
            }
        }
        seqs
    }

    #[tokio::test]
    async fn the_ack_reports_this_protocol_and_offers_nothing_it_cannot_do() {
        let (_fleet, addr) = start().await;
        let mut ws = connect(addr).await;
        say(
            &mut ws,
            json!({"type": "hello", "protocol_version": 1, "token": TOKEN}),
        )
        .await;
        let ack = hear(&mut ws).await;
        assert_eq!(ack["protocol_version"], protocol::PROTOCOL_VERSION);
        assert_eq!(ack["protocol_minor"], protocol::PROTOCOL_MINOR);
        assert_eq!(ack["device_name"], DEVICE_NAME);
        let capabilities = &ack["capabilities"];
        for offered in [
            "send_text",
            "diff",
            "risk_class",
            "session_uid",
            "prompt_identity",
        ] {
            assert_eq!(capabilities[offered], true, "{offered} must be offered");
        }
        // Nothing destructive, nothing that would need a machine behind it, and
        // — load-bearing — no TLS: the phone re-dials `wss://` the moment it sees
        // `tls` true on a plaintext link, and would flap for ever.
        for withheld in [
            "terminal_pty",
            "codex_interrupt",
            "push",
            "push_relay",
            "test_push",
            "delete_session",
            "capture",
            "command_catalog",
            "tls",
            "tls_active",
        ] {
            assert_eq!(capabilities[withheld], false, "{withheld} must be withheld");
        }
        // No device row is minted, so nothing rotates the phone's credential and
        // nothing enables a notification stream that could not be switched off.
        assert!(ack.get("device_token").is_none(), "{ack}");
        assert!(ack.get("device_id").is_none(), "{ack}");
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused_in_the_shape_the_phone_maps_to_a_terminal_failure() {
        let (_fleet, addr) = start().await;
        for credential in [
            json!({"type": "hello", "protocol_version": 1, "token": "not-the-review-token"}),
            // No credential at all.
            json!({"type": "hello", "protocol_version": 1}),
            // A pairing code, which this server has no way to honour. One opaque
            // answer for all three: a peer that cannot authenticate learns
            // nothing about which credential was wrong.
            json!({"type": "hello", "protocol_version": 1, "pairing_code": "ABCD2345"}),
        ] {
            let mut ws = connect(addr).await;
            say(&mut ws, credential.clone()).await;
            let frame = hear(&mut ws).await;
            assert_eq!(frame["type"], "error", "{credential}");
            assert_eq!(frame["code"], "unauthorized", "{credential}");
            // The message is not decoration: the client's `error` decoder
            // requires both keys, and a frame missing either is dropped — which
            // would leave a reviewer on a spinner instead of a reason.
            assert_eq!(frame["message"], "invalid credentials", "{credential}");
            assert!(closed(&mut ws).await, "the refusal must close the socket");
        }
    }

    #[tokio::test]
    async fn anything_before_hello_is_refused_the_same_way() {
        let (_fleet, addr) = start().await;
        let mut ws = connect(addr).await;
        say(&mut ws, json!({"type": "sessions"})).await;
        let frame = hear(&mut ws).await;
        assert_eq!(frame["code"], "unauthorized");
        assert_eq!(frame["message"], "hello must come first");
        assert!(closed(&mut ws).await);
    }

    #[tokio::test]
    async fn a_mismatched_major_is_refused_before_the_credential_is_ever_looked_at() {
        let (_fleet, addr) = start().await;
        let mut ws = connect(addr).await;
        // A wrong token *and* a wrong major. The version answer is the one that
        // comes back, which is what stops this being a way to probe credentials.
        say(
            &mut ws,
            json!({
                "type": "hello",
                "protocol_version": protocol::PROTOCOL_VERSION + 1,
                "token": "not-the-review-token",
            }),
        )
        .await;
        let frame = hear(&mut ws).await;
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "protocol_mismatch");
        assert_eq!(
            frame["message"],
            format!(
                "this daemon speaks protocol {}.{}; upgrade the client",
                protocol::PROTOCOL_VERSION,
                protocol::PROTOCOL_MINOR
            ),
            "the phone shows this sentence to the reviewer verbatim"
        );
        assert!(closed(&mut ws).await);
    }

    #[tokio::test]
    async fn a_client_that_never_says_hello_is_dropped_rather_than_held() {
        let (_fleet, addr) = start().await;
        let mut ws = connect(addr).await;
        // Nothing is sent. The deadline is ten seconds, so this proves the
        // connection is not held open indefinitely without also spending it.
        let held = tokio::time::timeout(Duration::from_millis(200), closed(&mut ws)).await;
        assert!(held.is_err(), "the deadline must not have passed yet");
    }

    #[tokio::test]
    async fn a_reconnect_with_after_seq_resumes_without_holes_or_duplicates() {
        let (fleet, addr) = start().await;
        // Build a log while nobody is watching.
        let mut clock = Instant::now();
        pump(&fleet, &mut clock, 4);
        let uid = fleet.resolve("cc-1").unwrap();
        let opening: Vec<u64> = fleet
            .events_after(&uid, 0, 1000)
            .iter()
            .map(|event| event.seq)
            .collect();
        assert!(opening.len() >= 4, "{opening:?}");

        // A cold phone takes the whole log.
        let mut first = paired(addr).await;
        say(
            &mut first,
            json!({"type": "subscribe", "session_id": uid, "after_seq": 0}),
        )
        .await;
        assert_eq!(seqs_of(&mut first, opening.len()).await, opening);
        let watermark = *opening.last().unwrap();
        drop(first);

        // The reviewer answers, and the turn continues while the phone is away.
        let card = fleet
            .events_after(&uid, 0, 1000)
            .into_iter()
            .rfind(|event| event.kind == EventKind::ApprovalRequest)
            .expect("cc-1 must be holding a card");
        let card = &card.payload["card"];
        let answered = fleet.answer(
            card["request_id"].as_str().unwrap(),
            card["payload_hash"].as_str().unwrap(),
            protocol::ws::AnswerDecision::Allow,
            Some(&uid),
            clock,
        );
        assert!(matches!(
            answered,
            protocol::ws::AnswerResult::Applied { .. }
        ));
        pump(&fleet, &mut clock, 4);

        let whole: Vec<u64> = fleet
            .events_after(&uid, 0, 1000)
            .iter()
            .map(|event| event.seq)
            .collect();
        assert!(whole.len() > opening.len(), "{whole:?}");

        // The reconnect picks up exactly where it left off.
        let expected: Vec<u64> = whole
            .iter()
            .copied()
            .filter(|seq| *seq > watermark)
            .collect();
        let mut second = paired(addr).await;
        say(
            &mut second,
            json!({"type": "subscribe", "session_id": uid, "after_seq": watermark}),
        )
        .await;
        let resumed = seqs_of(&mut second, expected.len()).await;
        assert_eq!(
            resumed, expected,
            "a resumed stream is the log above the watermark, in order and once each"
        );
        assert_eq!(
            resumed,
            (watermark + 1..=*whole.last().unwrap()).collect::<Vec<u64>>(),
            "and it has no holes in it"
        );
    }

    #[tokio::test]
    async fn a_subscribe_may_name_a_run_by_uid_or_by_name_and_an_unknown_one_says_so() {
        let (fleet, addr) = start().await;
        fleet.tick(Instant::now() + Duration::from_secs(120));
        let mut ws = paired(addr).await;

        say(&mut ws, json!({"type": "sessions"})).await;
        let fleet_frame = hear(&mut ws).await;
        assert_eq!(fleet_frame["type"], "sessions");
        assert_eq!(fleet_frame["sessions"].as_array().unwrap().len(), 5);

        // The phone subscribes with whatever the summary called the run, which
        // is the uid whenever one was minted.
        let uid = fleet_frame["sessions"][0]["session_uid"].as_str().unwrap();
        say(
            &mut ws,
            json!({"type": "subscribe", "session_id": uid, "after_seq": 0}),
        )
        .await;
        assert_eq!(hear(&mut ws).await["type"], "event");

        say(
            &mut ws,
            json!({"type": "subscribe", "session_id": "cc-nothing", "after_seq": 0}),
        )
        .await;
        let refusal = hear(&mut ws).await;
        assert_eq!(refusal["code"], "unknown_session");
        assert_eq!(refusal["message"], "unknown session cc-nothing");
    }

    #[tokio::test]
    async fn a_ping_is_answered_and_a_diff_is_served_for_any_run() {
        let (fleet, addr) = start().await;
        fleet.tick(Instant::now() + Duration::from_secs(120));
        let mut ws = paired(addr).await;

        say(&mut ws, json!({"type": "ping"})).await;
        assert_eq!(hear(&mut ws).await["type"], "pong");

        // Echoed byte for byte: the client keys its waiter on the string it
        // chose, so a reply naming the run differently never reaches it.
        say(&mut ws, json!({"type": "get_diff", "session_id": "cc-2"})).await;
        let diff = hear(&mut ws).await;
        assert_eq!(diff["type"], "diff");
        assert_eq!(diff["session_id"], "cc-2");
        assert_eq!(diff["truncated"], false);
        assert!(diff["unified"].as_str().unwrap().starts_with("diff --git "));

        say(
            &mut ws,
            json!({"type": "get_diff", "session_id": "cc-nothing"}),
        )
        .await;
        assert_eq!(hear(&mut ws).await["code"], "unknown_session");
    }

    #[tokio::test]
    async fn an_undecodable_frame_is_reported_without_dropping_the_connection() {
        let (_fleet, addr) = start().await;
        let mut ws = paired(addr).await;
        ws.send(Message::Text("{not json".into())).await.unwrap();
        let frame = hear(&mut ws).await;
        assert_eq!(frame["code"], "bad_request");
        assert!(frame["message"]
            .as_str()
            .unwrap()
            .starts_with("undecodable"));
        // Still usable: a malformed frame is a bug worth seeing, never a reason
        // to drop a working connection.
        say(&mut ws, json!({"type": "ping"})).await;
        assert_eq!(hear(&mut ws).await["type"], "pong");
    }

    #[tokio::test]
    async fn a_terminal_attach_is_refused_even_though_the_capability_already_said_no() {
        let (_fleet, addr) = start().await;
        let mut ws = paired(addr).await;
        say(
            &mut ws,
            json!({
                "type": "terminal_attach", "attachment_id": "att-1",
                "session_uid": "cc-1", "cols": 80, "rows": 24, "output_credit": 65536,
            }),
        )
        .await;
        let closed = hear(&mut ws).await;
        assert_eq!(closed["type"], "terminal_closed");
        assert_eq!(closed["attachment_id"], "att-1");
        assert_eq!(closed["code"], protocol::ws::terminal_close::NOT_AUTHORISED);
    }

    #[tokio::test]
    async fn a_plain_get_is_answered_two_hundred_so_a_health_check_passes() {
        let (_fleet, addr) = start().await;
        for path in ["/", "/healthz", "/anything/at/all"] {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(format!("GET {path} HTTP/1.1\r\nHost: demo\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8_lossy(&response);
            assert!(
                response.starts_with("HTTP/1.1 200 OK"),
                "{path}: {response}"
            );
        }
    }

    /// **The demo advertises earlier pages and serves them, and its fleet names
    /// each open card by reference and each run's facts**, read off its own log
    /// exactly as `ccd` reads them.
    #[tokio::test]
    async fn the_demo_serves_earlier_pages_and_its_summaries_carry_refs_and_facts() {
        let (fleet, addr) = start().await;
        let mut clock = Instant::now();
        pump(&fleet, &mut clock, 4);
        let uid = fleet.resolve("cc-1").unwrap();
        let log = fleet.events_after(&uid, 0, 1000);
        let tail = log.last().unwrap().seq;
        assert!(tail >= 4, "the premise: a log to page through");

        let mut ws = connect(addr).await;
        say(
            &mut ws,
            json!({"type": "hello", "protocol_version": 1, "token": TOKEN}),
        )
        .await;
        let ack = hear(&mut ws).await;
        assert_eq!(ack["capabilities"]["history_pages"], true, "{ack}");

        say(
            &mut ws,
            json!({"type": "history", "session_id": uid, "before_seq": tail,
                   "limit": 2, "request_id": "d1"}),
        )
        .await;
        let mut seqs = Vec::new();
        let end = loop {
            let frame = hear(&mut ws).await;
            match frame["type"].as_str() {
                Some("event") => seqs.push(frame["event"]["seq"].as_u64().unwrap()),
                Some("history_end") => break frame,
                _ => panic!("{frame}"),
            }
        };
        assert_eq!(seqs, vec![tail - 2, tail - 1]);
        assert_eq!(end["request_id"], "d1");
        assert_eq!(end["first_seq"], tail - 2);
        assert_eq!(end["before_seq"], tail);
        assert!(end.get("error").is_none(), "{end}");

        say(
            &mut ws,
            json!({"type": "history", "session_id": "cc-nothing", "before_seq": 5,
                   "limit": 500, "request_id": "d2"}),
        )
        .await;
        let end = hear(&mut ws).await;
        assert_eq!(end["type"], "history_end");
        assert_eq!(end["first_seq"], 5);
        assert!(end["error"].is_string(), "{end}");

        say(&mut ws, json!({"type": "sessions"})).await;
        let sessions = hear(&mut ws).await;
        let summary = sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|summary| summary["session_uid"] == uid.as_str())
            .unwrap()
            .clone();
        let card = log
            .iter()
            .rfind(|event| event.kind == EventKind::ApprovalRequest)
            .expect("cc-1 is holding a card");
        assert_eq!(summary["blocked_on"].as_array().unwrap().len(), 1);
        assert_eq!(
            summary["pending"],
            json!([{
                "request_id": card.payload["card"]["request_id"],
                "seq": card.seq,
                "hold": "held",
            }])
        );
        let start = log
            .iter()
            .find(|event| event.kind == EventKind::SessionStart)
            .unwrap();
        assert_eq!(summary["model"]["value"], start.payload["model"]);
        assert_eq!(summary["model"]["seq"], start.seq);
        assert_eq!(summary["model"]["source"], "session_start");
        assert_eq!(summary["model"]["at"], start.ts.as_str());
        assert_eq!(
            summary["permission_mode"],
            json!({"value": "default", "seq": card.seq})
        );
    }

    /// A sink that keeps every frame, so a test can step [`History`] directly.
    #[derive(Default)]
    struct Collect(Vec<Message>);

    impl futures_util::Sink<Message> for Collect {
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
            self.0.push(item);
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

    /// **A page of a run the fleet retires ends with the reason** — retired
    /// before the page is read, or while it is being sent.
    #[tokio::test]
    async fn a_page_of_a_run_retired_from_the_fleet_ends_with_an_error() {
        let fleet = Arc::new(Fleet::new(Instant::now()).expect("the shipped script must load"));
        let mut clock = Instant::now();
        for (steps_before, request_id) in [(0usize, "before-read"), (2, "mid-page")] {
            let uid = fleet.resolve("cc-5").unwrap();
            while fleet.events_after(&uid, 0, 10).len() < 5 {
                pump(&fleet, &mut clock, 1);
            }
            let mut history = History::default();
            history.ask(HistoryRequest::new(
                Some(uid.clone()),
                "cc-5".into(),
                5,
                500,
                request_id.into(),
            ));
            let mut sink = Collect::default();
            for _ in 0..steps_before {
                history.step(&fleet, &mut sink).await.unwrap();
            }
            // Played until the run is gone from the fleet: retired, then dropped
            // when its successor retires.
            while fleet.resolve(&uid).is_some() {
                pump(&fleet, &mut clock, 1);
            }
            for _ in 0..16 {
                if !history.in_progress() {
                    break;
                }
                history.step(&fleet, &mut sink).await.unwrap();
            }
            let Some(Message::Text(end)) = sink.0.last() else {
                panic!("the page sent nothing");
            };
            let end: serde_json::Value = serde_json::from_str(end).unwrap();
            assert_eq!(end["type"], "history_end", "{end}");
            assert_eq!(end["request_id"], request_id);
            assert!(end["error"].is_string(), "{request_id}: {end}");
        }
    }
}
