//! A minimal phone, for the scenarios that have to arrive over the wire.
//!
//! Plaintext `ws://` on the daemon's own endpoint. The listener accepts both
//! schemes on one port (it peeks the first byte), so this exercises every line
//! of the real server except the TLS handshake — and bringing a certificate
//! verifier into the harness would test rustls rather than CodeConnect.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use protocol::event::Event;
use protocol::ws::{AnswerDecision, AnswerResult, ClientMessage, SendTextResult, ServerMessage};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::WebSocketStream;

pub struct Phone {
    socket: WebSocketStream<TcpStream>,
}

impl Phone {
    /// The TCP connection is made here and handed to `client_async` rather than
    /// letting `connect_async` do both.
    ///
    /// That is a deliberate dependency decision: `connect_async` lives behind
    /// tokio-tungstenite's `connect` feature, and cargo unifies features across
    /// a workspace — so enabling it for this harness would silently add code to
    /// the shipped daemon. A harness must not change what it is testing.
    pub async fn connect(host: &str, port: u16, token: &str) -> Result<Phone> {
        let url = format!("ws://{host}:{port}/");
        let stream = TcpStream::connect((host, port))
            .await
            .with_context(|| format!("connecting to {host}:{port}"))?;
        let _ = stream.set_nodelay(true);
        let (socket, _) = tokio_tungstenite::client_async(&url, stream)
            .await
            .with_context(|| format!("websocket handshake with {url}"))?;
        let mut phone = Phone { socket };
        phone
            .send(&ClientMessage::Hello {
                protocol_version: protocol::PROTOCOL_VERSION,
                token: Some(token.to_string()),
                pairing_code: None,
                ssh_pubkey: None,
                client_id: None,
                client_name: Some("ccsoak".into()),
            })
            .await?;
        match phone.next_message(Duration::from_secs(10)).await? {
            ServerMessage::HelloAck { protocol_minor, .. } => {
                if protocol_minor < protocol::PROTOCOL_MINOR {
                    bail!(
                        "the daemon reports protocol minor {protocol_minor}; this harness was \
                         built against {}. Is an older ccd still running? \
                         `codeconnect daemon restart` picks up new binaries.",
                        protocol::PROTOCOL_MINOR
                    );
                }
            }
            other => bail!("expected hello_ack, got {other:?}"),
        }
        Ok(phone)
    }

    pub async fn send(&mut self, message: &ClientMessage) -> Result<()> {
        let text = serde_json::to_string(message)?;
        self.socket
            .send(Message::Text(text))
            .await
            .context("websocket write failed")
    }

    /// The next decoded server message, or an error on timeout.
    pub async fn next_message(&mut self, timeout: Duration) -> Result<ServerMessage> {
        loop {
            let frame = tokio::time::timeout(timeout, self.socket.next())
                .await
                .map_err(|_| anyhow!("timed out waiting for the daemon"))?
                .ok_or_else(|| anyhow!("the daemon closed the connection"))??;
            match frame {
                Message::Text(text) => {
                    return serde_json::from_str(&text)
                        .with_context(|| format!("undecodable server message: {text}"))
                }
                Message::Binary(bytes) => {
                    return serde_json::from_slice(&bytes).context("undecodable binary message")
                }
                Message::Close(_) => bail!("the daemon closed the connection"),
                _ => continue,
            }
        }
    }

    /// Subscribe and drain the replay, stopping when nothing new arrives.
    ///
    /// `idle` rather than a count: the run may be live, so "the replay is over"
    /// can only be observed as a pause, and a fixed count would either truncate
    /// a long backlog or hang on a quiet session.
    pub async fn subscribe_and_drain(
        &mut self,
        session_ref: &str,
        after_seq: u64,
        idle: Duration,
    ) -> Result<Vec<Event>> {
        self.send(&ClientMessage::Subscribe {
            session_id: session_ref.to_string(),
            after_seq,
        })
        .await?;

        let mut events = Vec::new();
        loop {
            match self.next_message(idle).await {
                Ok(ServerMessage::Event { event }) => events.push(event),
                Ok(ServerMessage::Error { code, message }) => {
                    bail!("subscribe failed: {code}: {message}")
                }
                Ok(_) => continue,
                // A quiet moment is the end of the replay, not a failure.
                Err(_) => return Ok(events),
            }
        }
    }

    pub async fn sessions(&mut self) -> Result<Vec<protocol::event::SessionSummary>> {
        self.send(&ClientMessage::Sessions).await?;
        loop {
            match self.next_message(Duration::from_secs(10)).await? {
                ServerMessage::Sessions { sessions } => return Ok(sessions),
                _ => continue,
            }
        }
    }

    pub async fn capture(&mut self, session_ref: &str, lines: u32) -> Result<String> {
        self.send(&ClientMessage::Capture {
            session_id: session_ref.to_string(),
            lines: Some(lines),
        })
        .await?;
        loop {
            match self.next_message(Duration::from_secs(15)).await? {
                ServerMessage::CaptureResult { text, .. } => return Ok(text),
                ServerMessage::Error { code, message } => {
                    bail!("capture failed: {code}: {message}")
                }
                _ => continue,
            }
        }
    }

    /// A takeover carrying a mutation identity, so a retry replays rather than
    /// types a second time. `request_id` of `None` is the pre-minor-3 shape.
    pub async fn send_text(
        &mut self,
        session_ref: &str,
        text: &str,
        request_id: Option<&str>,
    ) -> Result<SendTextResult> {
        self.send(&ClientMessage::SendText {
            session_id: session_ref.to_string(),
            text: text.to_string(),
            request_id: request_id.map(str::to_string),
            payload_hash: request_id
                .map(|_| protocol::hash::send_text_hash(session_ref, text, true)),
            require: None,
            submit: true,
        })
        .await?;
        loop {
            match self.next_message(Duration::from_secs(20)).await? {
                ServerMessage::SendTextResult { result, .. } => return Ok(result),
                ServerMessage::Error { code, message } => {
                    bail!("send_text failed: {code}: {message}")
                }
                _ => continue,
            }
        }
    }

    pub async fn answer(
        &mut self,
        request_id: &str,
        payload_hash: &str,
        decision: AnswerDecision,
        session_ref: &str,
    ) -> Result<AnswerResult> {
        self.send(&ClientMessage::Answer {
            request_id: request_id.to_string(),
            payload_hash: payload_hash.to_string(),
            decision,
            session_id: Some(session_ref.to_string()),
        })
        .await?;
        loop {
            match self.next_message(Duration::from_secs(30)).await? {
                ServerMessage::AnswerResult { result, .. } => return Ok(result),
                ServerMessage::Error { code, message } => bail!("answer failed: {code}: {message}"),
                _ => continue,
            }
        }
    }
}

/// Is a replay contiguous, strictly increasing and free of repeats?
///
/// Returns the complaint rather than a bool: a gauntlet report that says
/// "failed" without saying where is a report nobody can act on.
pub fn check_replay(events: &[Event], after_seq: u64, session_uid: &str) -> Result<()> {
    let mut expected = after_seq + 1;
    let mut seen = std::collections::HashSet::new();
    for event in events {
        // A resync marker carries seq 0 by design and is not part of the run.
        if event.kind == protocol::event::EventKind::Resync {
            bail!("the daemon emitted a resync marker during a plain replay");
        }
        if event.session_uid != session_uid {
            bail!(
                "replay for {session_uid} contained an event from {}",
                event.session_uid
            );
        }
        if event.seq != expected {
            bail!(
                "replay after_seq={after_seq} jumped from {} to {} (expected {expected})",
                expected.saturating_sub(1),
                event.seq
            );
        }
        if !seen.insert(event.seq) {
            bail!("replay repeated seq {}", event.seq);
        }
        expected += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::event::{EventKind, Source};

    fn event(seq: u64, uid: &str) -> Event {
        Event {
            seq,
            session_uid: uid.into(),
            session_id: "cc-1".into(),
            ts: "t".into(),
            kind: EventKind::ToolCall,
            payload: serde_json::Value::Null,
            source: Source::Hook,
            source_event_id: None,
            turn_id: None,
            item_id: None,
        }
    }

    #[test]
    fn a_contiguous_replay_passes() {
        let events: Vec<Event> = (4..=9).map(|seq| event(seq, "u")).collect();
        check_replay(&events, 3, "u").unwrap();
        check_replay(&[], 99, "u").unwrap();
    }

    #[test]
    fn a_gap_a_repeat_and_a_stranger_are_all_caught() {
        let gap = vec![event(4, "u"), event(6, "u")];
        assert!(check_replay(&gap, 3, "u").is_err());

        let repeat = vec![event(4, "u"), event(4, "u")];
        assert!(check_replay(&repeat, 3, "u").is_err());

        let wrong_start = vec![event(5, "u")];
        assert!(check_replay(&wrong_start, 3, "u").is_err());

        let stranger = vec![event(4, "other")];
        assert!(check_replay(&stranger, 3, "u").is_err());
    }

    #[test]
    fn a_resync_marker_is_a_failure_not_an_event() {
        let mut marker = event(0, "u");
        marker.kind = EventKind::Resync;
        assert!(check_replay(&[marker], 0, "u").is_err());
    }
}
