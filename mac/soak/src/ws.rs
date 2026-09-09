//! A minimal phone, for the scenarios that have to arrive over the wire.
//!
//! Plaintext `ws://` on the daemon's own endpoint. The listener accepts both
//! schemes on one port (it peeks the first byte), so this exercises every line
//! of the real server except the TLS handshake — and bringing a certificate
//! verifier into the harness would test rustls rather than CodeConnect.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use futures_util::{SinkExt, Stream, StreamExt};
use protocol::event::Event;
use protocol::ws::{
    AnswerDecision, AnswerResult, ClientMessage, SendTextResult, ServerMessage,
    MAX_TERMINAL_CHUNK_BYTES, TERMINAL_INITIAL_INPUT_CREDIT, TERMINAL_MAX_OUTSTANDING_CREDIT,
};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::WebSocketStream;

/// How a connection names itself. The two are not interchangeable: the static
/// token is the bootstrap credential and never carries terminal authority,
/// while a pairing code is single-use and *buys* the per-device token that
/// does.
pub enum Credential<'a> {
    Token(&'a str),
    PairingCode(&'a str),
}

/// What the daemon said when it admitted this connection.
///
/// Kept rather than discarded because the terminal is **connection**-scoped:
/// `terminal_pty` is not a build fact, and the device token exists on the wire
/// exactly once — in the ack to the pairing hello that bought it.
#[derive(Debug, Clone, Default)]
pub struct Greeting {
    pub terminal_pty: bool,
    pub device_token: Option<String>,
    pub device_name: Option<String>,
}

pub struct Phone {
    socket: WebSocketStream<TcpStream>,
    greeting: Greeting,
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
        Phone::hello(host, port, Credential::Token(token), "ccsoak").await
    }

    /// Redeem a pairing code, exactly as a phone scanning the QR does, and come
    /// back holding the device token the ack carried.
    ///
    /// This is the only way to reach the terminal: it is shell-equivalent
    /// authority, so the daemon offers it to a paired per-device credential and
    /// never to the static bootstrap token every other scenario uses.
    pub async fn pair(host: &str, port: u16, code: &str, name: &str) -> Result<Phone> {
        Phone::hello(host, port, Credential::PairingCode(code), name).await
    }

    async fn hello(
        host: &str,
        port: u16,
        credential: Credential<'_>,
        client_name: &str,
    ) -> Result<Phone> {
        let url = format!("ws://{host}:{port}/");
        let stream = TcpStream::connect((host, port))
            .await
            .with_context(|| format!("connecting to {host}:{port}"))?;
        let _ = stream.set_nodelay(true);
        let (socket, _) = tokio_tungstenite::client_async(&url, stream)
            .await
            .with_context(|| format!("websocket handshake with {url}"))?;
        let mut phone = Phone {
            socket,
            greeting: Greeting::default(),
        };
        let (token, pairing_code) = match credential {
            Credential::Token(token) => (Some(token.to_string()), None),
            Credential::PairingCode(code) => (None, Some(code.to_string())),
        };
        phone
            .send(&ClientMessage::Hello {
                protocol_version: protocol::PROTOCOL_VERSION,
                token,
                pairing_code,
                client_id: None,
                client_name: Some(client_name.into()),
                features: None,
            })
            .await?;
        match phone.next_message(Duration::from_secs(10)).await? {
            ServerMessage::HelloAck {
                protocol_minor,
                capabilities,
                device_token,
                device_name,
                ..
            } => {
                if protocol_minor < protocol::PROTOCOL_MINOR {
                    bail!(
                        "the daemon reports protocol minor {protocol_minor}; this harness was \
                         built against {}. Is an older ccd still running? \
                         `codeconnect daemon restart` picks up new binaries.",
                        protocol::PROTOCOL_MINOR
                    );
                }
                phone.greeting = Greeting {
                    terminal_pty: capabilities.terminal_pty,
                    device_token,
                    device_name,
                };
            }
            ServerMessage::Error { code, message } => bail!("hello refused: {code}: {message}"),
            other => bail!("expected hello_ack, got {other:?}"),
        }
        Ok(phone)
    }

    pub fn greeting(&self) -> &Greeting {
        &self.greeting
    }

    pub async fn send(&mut self, message: &ClientMessage) -> Result<()> {
        let text = serde_json::to_string(message)?;
        self.socket
            .send(Message::Text(text))
            .await
            .context("websocket write failed")
    }

    /// The next decoded server message, or an error once `timeout` is spent.
    ///
    /// `timeout` is the budget for the whole call, which is what every caller
    /// already reads it as.
    pub async fn next_message(&mut self, timeout: Duration) -> Result<ServerMessage> {
        next_decoded(&mut self.socket, timeout).await
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
            // The soak drives raw sends, not a sheet that disclosed anything.
            complete_native_confirmation: false,
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

/// The next decoded server message off `stream`, within one deadline.
///
/// Free and generic over the stream rather than a method on [`Phone`], because
/// the deadline is the whole of the property and none of it needs a socket: as
/// a method it could only ever be exercised against a live daemon, which is to
/// say never under `cargo test`, and the case it gets wrong is precisely the
/// one a live run cannot show you — a wait that simply never ends.
async fn next_decoded<S>(stream: &mut S, timeout: Duration) -> Result<ServerMessage>
where
    S: Stream<Item = std::result::Result<Message, WsError>> + Unpin,
{
    // One deadline for the whole call rather than a fresh one per frame. The
    // daemon pings every 30 seconds and a ping is skipped below, so a per-frame
    // timeout is renewed by exactly the traffic it exists to outlast: the
    // starvation scenario asks for 45 seconds *because* it is timing a 30
    // second close, and under a renewing timeout it would wait for ever and
    // never fail the daemon that failed to close it.
    tokio::time::timeout(timeout, async {
        loop {
            let frame = stream
                .next()
                .await
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
    })
    .await
    .map_err(|_| anyhow!("timed out after {timeout:?} waiting for the daemon"))?
}

// ------------------------------------------------------------------ terminal

/// The phone's half of one live terminal.
///
/// It **owns** the connection for as long as the attachment lasts, because
/// every terminal message is scoped to one attachment and one socket, and it
/// hands the connection back when the attachment ends — a closed terminal costs
/// the stream and never the connection, which is the property three of the
/// scenarios exist to check.
pub struct Terminal {
    phone: Phone,
    ledger: Ledger,
    /// Decoded pane bytes received, and the chunks they arrived in.
    pub received: usize,
    pub chunks: usize,
}

/// One attachment's two credit windows, and every rule about them.
///
/// Split out of [`Terminal`] because the flow-control contract this gauntlet
/// exists to police is entirely arithmetic and needs no connection to check.
/// Folded in beside the socket it would be unreachable from a test, and an
/// un-tested ledger is one that silently absorbs the very violation it is
/// watching for — which is exactly what `saturating_sub` used to do here.
struct Ledger {
    /// The attachment every frame must name.
    ///
    /// Checked rather than assumed, because a terminal shares its connection
    /// with the whole protocol: credit belonging to another attachment, taken
    /// in here, inflates this window until the harness types past the daemon's
    /// limit and is answered with `protocol_error` — the harness manufacturing
    /// the violation it then reports.
    attachment_id: String,
    /// Output credit granted to the daemon and not yet spent on a chunk.
    ///
    /// Invariant: an *upper bound* on the daemon's own outstanding credit. A
    /// grant is counted here the moment it is queued, before the daemon has it;
    /// a chunk is discounted only once it has arrived, after the daemon spent
    /// it. Both errors run the same way, so this number can never be below what
    /// the daemon believes it holds — and therefore a chunk larger than it is
    /// not a race but the daemon streaming past a window it was never given,
    /// which is the single violation this whole harness exists to catch.
    granted: u32,
    /// Bytes this end may still type before the daemon replenishes.
    ///
    /// Invariant: never above [`TERMINAL_INITIAL_INPUT_CREDIT`], the one window
    /// a phone ever has. The daemon returns credit only for input it has
    /// already delivered out of that window, so the sum cannot legitimately
    /// exceed it; a frame that would is a daemon bug, and clamping it here
    /// would hand the harness a window the daemon closes the terminal over the
    /// moment it is spent.
    input_credit: u32,
}

impl Ledger {
    /// Refuse a frame addressed to some other attachment.
    fn owns(&self, attachment_id: &str, frame: &str) -> Result<()> {
        if attachment_id != self.attachment_id {
            bail!(
                "{frame} for attachment {attachment_id}, which is not this terminal ({})",
                self.attachment_id
            );
        }
        Ok(())
    }

    /// Spend the output window on a chunk that has arrived. Exact: see the
    /// upper-bound invariant on `granted` for why an overshoot cannot be a race.
    ///
    /// The chunk ceiling is enforced here too, because it is documented as
    /// receiver-enforced and this harness is the only receiver in the workspace
    /// that can say so. It was not, and the daemon was sending frames sixteen
    /// times the bound: a real client that implemented the documented check
    /// would have closed a healthy terminal on its first busy screen, and
    /// nothing here would have caught it.
    fn spend_output(&mut self, bytes: usize) -> Result<()> {
        if bytes > MAX_TERMINAL_CHUNK_BYTES {
            bail!(
                "the daemon sent a {bytes} byte terminal_output, past the \
                 {MAX_TERMINAL_CHUNK_BYTES} one frame may carry"
            );
        }
        let spent = bytes as u64;
        let outstanding = u64::from(self.granted);
        if spent > outstanding {
            bail!(
                "the daemon sent {spent} bytes of output against {outstanding} bytes of \
                 outstanding credit, overshooting its window by {}",
                spent - outstanding
            );
        }
        self.granted -= spent as u32;
        Ok(())
    }

    /// Take back the credit the daemon returned for input it has delivered.
    fn return_input(&mut self, bytes: u32) -> Result<()> {
        let total = u64::from(self.input_credit) + u64::from(bytes);
        if total > u64::from(TERMINAL_INITIAL_INPUT_CREDIT) {
            bail!(
                "the daemon returned {bytes} bytes of input credit on top of {}, for {total} \
                 against the {TERMINAL_INITIAL_INPUT_CREDIT} byte window a phone ever has",
                self.input_credit
            );
        }
        self.input_credit = total as u32;
        Ok(())
    }

    /// Widen the output window, before the grant goes on the wire.
    fn grant(&mut self, bytes: u32) -> Result<()> {
        let total = u64::from(self.granted) + u64::from(bytes);
        if total > u64::from(TERMINAL_MAX_OUTSTANDING_CREDIT) {
            bail!(
                "granting {bytes} would leave {total} bytes outstanding, past the \
                 {TERMINAL_MAX_OUTSTANDING_CREDIT} the daemon closes on"
            );
        }
        self.granted = total as u32;
        Ok(())
    }

    /// Spend the input window on bytes about to be typed.
    fn spend_input(&mut self, bytes: u32) -> Result<()> {
        if bytes > self.input_credit {
            bail!(
                "typing {bytes} bytes with {} of input credit would be an over-send",
                self.input_credit
            );
        }
        self.input_credit -= bytes;
        Ok(())
    }

    /// The ledger's answer to one decoded server message: what the caller
    /// should report, or the contract violation that ends the run.
    fn record(&mut self, message: ServerMessage) -> Result<TerminalEvent> {
        match message {
            ServerMessage::TerminalOutput {
                attachment_id,
                data,
            } => {
                self.owns(&attachment_id, "output")?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&data)
                    .context("undecodable terminal_output")?;
                self.spend_output(bytes.len())?;
                Ok(TerminalEvent::Output(bytes))
            }
            ServerMessage::TerminalCredit {
                attachment_id,
                bytes,
            } => {
                self.owns(&attachment_id, "input credit")?;
                self.return_input(bytes)?;
                Ok(TerminalEvent::Credit)
            }
            // The one terminal frame whose id is *not* judged here. A close
            // corrupts no ledger — it ends the attachment — and its id is
            // evidence: `terminal_duplicate` proves the daemon closes the
            // terminal that exists rather than the ghost id a second attach
            // named, and it can only prove that if the id reaches it. Refusing
            // a foreign close here would turn "the daemon closed the wrong
            // attachment" from a named finding into a generic harness error.
            // Every consumer therefore owns the check; `detach` below does it.
            ServerMessage::TerminalClosed {
                attachment_id,
                code,
                reason,
            } => Ok(TerminalEvent::Closed {
                attachment_id,
                code,
                reason,
            }),
            _ => Ok(TerminalEvent::Other),
        }
    }
}

/// What arrived on a terminal's connection.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    Output(Vec<u8>),
    /// The daemon handed typed bytes to the pane and returned that much input
    /// credit. The ledger is already updated; the number is on the frame, and
    /// no scenario asserts against it separately.
    Credit,
    Closed {
        attachment_id: String,
        code: String,
        reason: String,
    },
    /// Anything else the socket carried — an event, a pong. A terminal shares
    /// its connection with the rest of the protocol.
    Other,
}

/// The answer to an attach: a live terminal, or the close the daemon sent
/// instead. Both give the connection back, since a refused attach leaves it
/// perfectly usable and the scenarios go on to use it.
pub enum Attachment {
    Live(Box<Terminal>),
    Refused {
        /// Boxed only because a connection is a large thing to carry inside an
        /// enum whose other arm is a pointer.
        phone: Box<Phone>,
        code: String,
        reason: String,
    },
}

impl Terminal {
    /// Ask for a terminal on `session_uid` and wait for the verdict.
    ///
    /// `output_credit` is the window the daemon may stream into before the
    /// first replenishment; it is the whole of the starvation scenario's
    /// instrument, so it is a parameter rather than a constant.
    pub async fn attach(
        mut phone: Phone,
        attachment_id: &str,
        session_uid: &str,
        cols: u16,
        rows: u16,
        output_credit: u32,
    ) -> Result<Attachment> {
        phone
            .send(&ClientMessage::TerminalAttach {
                attachment_id: attachment_id.to_string(),
                session_uid: session_uid.to_string(),
                cols,
                rows,
                output_credit,
            })
            .await?;
        loop {
            match phone.next_message(Duration::from_secs(20)).await? {
                ServerMessage::TerminalAttached {
                    attachment_id: attached,
                    input_credit,
                    max_chunk_bytes,
                    max_outstanding_credit,
                } => {
                    // The verdict has to be about the attachment that was
                    // asked for. An ack naming another id would open a
                    // `Terminal` whose every later frame is addressed
                    // elsewhere, and the whole run would then read as a stream
                    // of id mismatches rather than the one fact that a daemon
                    // acknowledged something nobody requested.
                    if attached != attachment_id {
                        bail!(
                            "terminal_attached named attachment {attached}, not the \
                             {attachment_id} that was asked for"
                        );
                    }
                    // The window the daemon opens is the window the protocol
                    // fixes. A larger one is a daemon bug that the harness
                    // would otherwise spend, and be closed for spending.
                    if input_credit > TERMINAL_INITIAL_INPUT_CREDIT {
                        bail!(
                            "the daemon granted {input_credit} bytes of input credit, past the \
                             {TERMINAL_INITIAL_INPUT_CREDIT} byte window the protocol fixes"
                        );
                    }
                    // The ceilings the daemon advertises are the ones this
                    // harness enforces below, and it says so here rather than
                    // silently taking its own copy as the truth. A daemon whose
                    // numbers differ is one whose terminals a phone built
                    // against these constants would lose mid-session, and the
                    // whole reason they are on the wire is that nobody notices
                    // that until it ships.
                    if max_chunk_bytes as usize != MAX_TERMINAL_CHUNK_BYTES
                        || max_outstanding_credit != TERMINAL_MAX_OUTSTANDING_CREDIT
                    {
                        bail!(
                            "the daemon advertises a {max_chunk_bytes}-byte chunk ceiling and \
                             {max_outstanding_credit} bytes of outstanding credit; this harness \
                             enforces {MAX_TERMINAL_CHUNK_BYTES} and \
                             {TERMINAL_MAX_OUTSTANDING_CREDIT}"
                        );
                    }
                    return Ok(Attachment::Live(Box::new(Terminal {
                        phone,
                        ledger: Ledger {
                            attachment_id: attachment_id.to_string(),
                            granted: output_credit,
                            input_credit,
                        },
                        received: 0,
                        chunks: 0,
                    })));
                }
                ServerMessage::TerminalClosed {
                    attachment_id: refused,
                    code,
                    reason,
                } => {
                    // A refusal names the id it refused: every close on this
                    // path is built from the `terminal_attach` frame just sent.
                    // The one daemon close that names a *different* attachment
                    // is the answer to a second attach on a connection that
                    // already holds a terminal — which is a harness bug here,
                    // since `Terminal::attach` consumes the connection, and the
                    // scenarios must be told rather than handed a refusal for
                    // somebody else's stream.
                    if refused != attachment_id {
                        bail!(
                            "the attach refusal named attachment {refused} as {code} ({reason}), \
                             not the {attachment_id} that was asked for"
                        );
                    }
                    return Ok(Attachment::Refused {
                        phone: Box::new(phone),
                        code,
                        reason,
                    });
                }
                ServerMessage::Error { code, message } => {
                    bail!("terminal_attach failed: {code}: {message}")
                }
                _ => continue,
            }
        }
    }

    pub fn id(&self) -> &str {
        &self.ledger.attachment_id
    }

    /// The connection, once the attachment is over.
    pub fn into_phone(self) -> Phone {
        self.phone
    }

    /// The next terminal message, with the ledgers kept in step.
    pub async fn next(&mut self, timeout: Duration) -> Result<TerminalEvent> {
        let event = self
            .ledger
            .record(self.phone.next_message(timeout).await?)?;
        // Counted only once the ledger has accepted the chunk, so a report
        // never quotes bytes that were an over-send rather than a delivery.
        if let TerminalEvent::Output(bytes) = &event {
            self.received += bytes.len();
            self.chunks += 1;
        }
        Ok(event)
    }

    /// Return `bytes` of output credit. Refuses locally rather than letting the
    /// daemon answer a ceiling breach with `protocol_error`.
    pub async fn grant(&mut self, bytes: u32) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        self.ledger.grant(bytes)?;
        let attachment_id = self.ledger.attachment_id.clone();
        self.phone
            .send(&ClientMessage::TerminalCredit {
                attachment_id,
                bytes,
            })
            .await
    }

    /// Type into the pane, spending input credit exactly.
    pub async fn type_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_TERMINAL_CHUNK_BYTES {
            bail!("{} bytes is more than one frame may carry", bytes.len());
        }
        self.ledger.spend_input(bytes.len() as u32)?;
        let attachment_id = self.ledger.attachment_id.clone();
        self.phone
            .send(&ClientMessage::TerminalInput {
                attachment_id,
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
            .await
    }

    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let attachment_id = self.ledger.attachment_id.clone();
        self.phone
            .send(&ClientMessage::TerminalResize {
                attachment_id,
                cols,
                rows,
            })
            .await
    }

    /// A second `terminal_attach` down a connection that already has one — the
    /// frame a well-behaved phone never sends, and the one the daemon answers
    /// by closing the terminal that actually exists.
    pub async fn attach_again(&mut self, attachment_id: &str, session_uid: &str) -> Result<()> {
        self.phone
            .send(&ClientMessage::TerminalAttach {
                attachment_id: attachment_id.to_string(),
                session_uid: session_uid.to_string(),
                cols: 80,
                rows: 24,
                output_credit: protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
            })
            .await
    }

    /// Read until `needle` appears in the stream, replenishing credit for every
    /// chunk as it arrives. Returns everything seen up to and including it.
    pub async fn read_until(&mut self, needle: &[u8], deadline: Duration) -> Result<Vec<u8>> {
        let start = Instant::now();
        let mut seen: Vec<u8> = Vec::new();
        loop {
            let left = deadline.checked_sub(start.elapsed()).ok_or_else(|| {
                anyhow!(
                    "{:?} never arrived in {deadline:?} ({} bytes seen)",
                    String::from_utf8_lossy(needle),
                    seen.len()
                )
            })?;
            match self.next(left).await {
                Ok(TerminalEvent::Output(bytes)) => {
                    let spent = bytes.len() as u32;
                    seen.extend_from_slice(&bytes);
                    self.grant(spent).await?;
                    if contains(&seen, needle) {
                        return Ok(seen);
                    }
                }
                Ok(TerminalEvent::Closed { code, reason, .. }) => bail!(
                    "the terminal closed as {code} ({reason}) while waiting for {:?}",
                    String::from_utf8_lossy(needle)
                ),
                Ok(_) => continue,
                Err(err) => bail!("waiting for {:?}: {err:#}", String::from_utf8_lossy(needle)),
            }
        }
    }

    /// Detach and consume the acknowledging close, giving the connection back.
    pub async fn detach(mut self) -> Result<(Phone, String)> {
        let attachment_id = self.ledger.attachment_id.clone();
        self.phone
            .send(&ClientMessage::TerminalDetach {
                attachment_id: attachment_id.clone(),
            })
            .await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| anyhow!("no terminal_closed after detaching {attachment_id}"))?;
            match self.next(left).await? {
                TerminalEvent::Closed {
                    attachment_id: closed,
                    code,
                    ..
                } => {
                    // `next` carries a close's id rather than judging it, so
                    // the check belongs at each consumer — and here it decides
                    // the answer: a close naming another attachment read as
                    // this one's acknowledgement would report `detached` for a
                    // terminal that is still running.
                    self.ledger.owns(&closed, "a close")?;
                    return Ok((self.phone, code));
                }
                // Output already in flight when the detach was sent. It spends
                // credit that is never replenished, which is correct: the
                // attachment is over.
                _ => continue,
            }
        }
    }
}

/// Does `haystack` contain `needle`? A pane's bytes are a stream, not a string:
/// the marker may straddle two chunks, so the search is over the accumulation.
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Is a replay contiguous, strictly increasing and free of repeats?
///
/// Returns the complaint rather than a bool: a gauntlet report that says
/// "failed" without saying where is a report nobody can act on.
pub fn check_replay(events: &[Event], after_seq: u64, session_uid: &str) -> Result<()> {
    // Zipped rather than a hand-rolled counter: the expected sequence *is* the
    // index offset from the watermark.
    let mut seen = std::collections::HashSet::new();
    for (expected, event) in (after_seq + 1..).zip(events.iter()) {
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::event::{EventKind, Source};
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};

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
    fn a_marker_is_found_across_the_chunks_it_arrived_in() {
        // Pane bytes are a stream: tmux decides where one `%output` line ends,
        // so a marker matched only within a single chunk would pass or fail on
        // the server's chunking rather than on the round trip.
        let stream = b"\x1b[0m\x1b[2J\x1b[Hsh-3.2$ echo SOAK''-M-7\r\nSOAK-M".to_vec();
        assert!(!contains(&stream, b"SOAK-M-7"));
        let mut whole = stream;
        whole.extend_from_slice(b"-7\r\n");
        assert!(contains(&whole, b"SOAK-M-7"));
        // And the command that printed it is not itself the answer: the shell
        // strips the empty quotes, so the echo can never match the marker.
        assert!(!contains(b"echo SOAK''-M-7", b"SOAK-M-7"));
        assert!(!contains(b"", b"SOAK-M-7"));
    }

    #[test]
    fn a_resync_marker_is_a_failure_not_an_event() {
        let mut marker = event(0, "u");
        marker.kind = EventKind::Resync;
        assert!(check_replay(&[marker], 0, "u").is_err());
    }

    // ---------------------------------------------------------------- ledger

    /// A ledger for `att-1`, with the two windows wherever a case needs them.
    fn ledger(granted: u32, input_credit: u32) -> Ledger {
        Ledger {
            attachment_id: "att-1".into(),
            granted,
            input_credit,
        }
    }

    fn output(attachment_id: &str, bytes: &[u8]) -> ServerMessage {
        ServerMessage::TerminalOutput {
            attachment_id: attachment_id.into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    fn credit(attachment_id: &str, bytes: u32) -> ServerMessage {
        ServerMessage::TerminalCredit {
            attachment_id: attachment_id.into(),
            bytes,
        }
    }

    fn closed(attachment_id: &str) -> ServerMessage {
        ServerMessage::TerminalClosed {
            attachment_id: attachment_id.into(),
            code: protocol::ws::terminal_close::DETACHED.into(),
            reason: "the tab was closed".into(),
        }
    }

    /// Output is spent against the grant exactly, and an overshoot is the
    /// failure rather than a clamp.
    ///
    /// `saturating_sub` let a daemon streaming past the window it was granted
    /// land this ledger on 0 and the scenario report a pass — hiding the single
    /// violation the whole gauntlet exists to catch. The boundary matters as
    /// much as the breach: a chunk that fills the window to the byte is a
    /// daemon obeying the contract, and failing that would make every honest
    /// run a red one.
    #[test]
    fn output_is_spent_against_the_grant_exactly_and_an_overshoot_is_the_failure() {
        let mut exact = ledger(8, 0);
        exact.record(output("att-1", b"exactly8")).unwrap();
        assert_eq!(exact.granted, 0, "a chunk that fills the window is legal");

        let err = exact.record(output("att-1", b"x")).unwrap_err().to_string();
        assert!(
            err.contains("sent 1 bytes of output against 0 bytes"),
            "{err}"
        );

        let mut overshot = ledger(8, 0);
        let err = overshot
            .record(output("att-1", b"eleven byte"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("sent 11 bytes of output against 8 bytes") && err.contains("by 3"),
            "{err}"
        );
        assert_eq!(overshot.granted, 8, "a refused chunk spends nothing");
    }

    /// A frame addressed to another attachment is refused, whatever it carries.
    ///
    /// A terminal shares its connection with the rest of the protocol, so a
    /// stranger's frame can land on it. Absorbed, a stranger's credit inflates
    /// this window; the harness then types past the daemon's limit, is closed
    /// for `protocol_error`, and reports a violation it manufactured itself.
    #[test]
    fn a_frame_naming_another_attachment_is_refused_whatever_it_carries() {
        let mut led = ledger(64, 0);

        assert!(led.record(output("att-2", b"hi")).is_err());
        assert_eq!(led.granted, 64, "a stranger's output spends nothing");

        assert!(led.record(credit("att-2", 512)).is_err());
        assert_eq!(led.input_credit, 0, "a stranger's credit lands nowhere");

        // The same guard `detach` puts on the close it is waiting for, so a
        // foreign close is never read as this attachment's acknowledgement.
        assert!(led.owns("att-2", "a close").is_err());
        led.owns("att-1", "a close").unwrap();
    }

    /// Input credit returns to the window and never past it.
    ///
    /// ccd hands credit back only for input it has already delivered out of one
    /// [`TERMINAL_INITIAL_INPUT_CREDIT`] window, so the sum cannot legitimately
    /// exceed it. `saturating_add` hid an over-grant and left the harness
    /// holding credit that the daemon closes the terminal for spending.
    #[test]
    fn input_credit_returns_to_the_window_and_never_past_it() {
        let mut led = ledger(0, TERMINAL_INITIAL_INPUT_CREDIT);
        led.spend_input(100).unwrap();
        led.record(credit("att-1", 100)).unwrap();
        assert_eq!(
            led.input_credit, TERMINAL_INITIAL_INPUT_CREDIT,
            "exactly what was spent comes back"
        );

        let err = led.record(credit("att-1", 1)).unwrap_err().to_string();
        assert!(
            err.contains(&TERMINAL_INITIAL_INPUT_CREDIT.to_string()),
            "{err}"
        );
        assert_eq!(
            led.input_credit, TERMINAL_INITIAL_INPUT_CREDIT,
            "a refused grant lands nowhere"
        );

        // Widened before it is compared, so no grant can wrap u32 and come back
        // down under the ceiling.
        assert!(led.record(credit("att-1", u32::MAX)).is_err());
        assert_eq!(led.input_credit, TERMINAL_INITIAL_INPUT_CREDIT);
    }

    /// A close is carried to the caller with the id it named, deliberately
    /// unjudged — this is the one terminal frame the ledger does not police.
    ///
    /// `terminal_duplicate` proves ccd answers a second attach by closing the
    /// terminal that *exists* rather than the ghost id the frame named, and it
    /// can only prove that from the id on the event. Refusing a foreign close
    /// here would turn "the daemon closed the wrong attachment" from a named
    /// finding into a generic harness error, so the check lives at each
    /// consumer instead.
    #[test]
    fn a_close_is_carried_to_the_caller_with_the_id_it_named() {
        let mut led = ledger(0, 0);
        match led.record(closed("att-2")).unwrap() {
            TerminalEvent::Closed { attachment_id, .. } => assert_eq!(attachment_id, "att-2"),
            other => panic!("a close became {other:?}"),
        }
    }

    /// Both local windows refuse before the daemon has to.
    ///
    /// A harness that walked into `protocol_error` on its own account would be
    /// measuring its own bug and reporting it as the daemon's.
    #[test]
    fn the_local_windows_refuse_before_the_daemon_has_to() {
        let mut led = ledger(TERMINAL_MAX_OUTSTANDING_CREDIT - 1, 4);
        led.grant(1).unwrap();
        assert!(
            led.grant(1).is_err(),
            "one byte past the ceiling ccd closes on"
        );
        assert_eq!(led.granted, TERMINAL_MAX_OUTSTANDING_CREDIT);

        led.spend_input(4).unwrap();
        assert!(led.spend_input(1).is_err(), "typing on an empty window");
        assert_eq!(led.input_credit, 0);
    }

    // -------------------------------------------------------- read deadline

    /// A socket that hands over `pings` frames the decoder skips, on a fixed
    /// cadence, then `payload` if there is one, then pings for ever.
    struct Cadence {
        every: Duration,
        sleep: Pin<Box<tokio::time::Sleep>>,
        pings: usize,
        payload: Option<Message>,
    }

    impl Cadence {
        fn new(every: Duration, pings: usize, payload: Option<Message>) -> Cadence {
            Cadence {
                every,
                sleep: Box::pin(tokio::time::sleep(every)),
                pings,
                payload,
            }
        }
    }

    impl Stream for Cadence {
        type Item = std::result::Result<Message, WsError>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            if this.sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            let next = tokio::time::Instant::now() + this.every;
            this.sleep.as_mut().reset(next);
            if this.pings > 0 {
                this.pings -= 1;
                return Poll::Ready(Some(Ok(Message::Ping(Vec::new()))));
            }
            match this.payload.take() {
                Some(message) => Poll::Ready(Some(Ok(message))),
                None => Poll::Ready(Some(Ok(Message::Ping(Vec::new())))),
            }
        }
    }

    /// The read budget is absolute: traffic the decoder skips does not renew it.
    ///
    /// ccd pings every 30 seconds and a ping is skipped below, so a per-frame
    /// timeout was renewed by exactly the traffic it had to outlast. The
    /// starvation scenario asks for 45 seconds *because* it is timing a 30
    /// second close; under the old shape it waited for ever and never failed
    /// the daemon that failed to close it. Measured in real time rather than
    /// under `tokio::time::pause`, which needs a tokio feature this workspace
    /// does not enable and a harness must not add one the daemon then inherits.
    #[tokio::test]
    async fn a_read_budget_is_absolute_and_a_ping_a_frame_does_not_renew_it() {
        let mut socket = Cadence::new(Duration::from_millis(25), usize::MAX, None);
        let budget = Duration::from_millis(200);
        let started = Instant::now();
        // Outer bound so a regression fails in seconds rather than hanging the
        // suite for ever, which is precisely what the old shape did.
        let outcome =
            tokio::time::timeout(Duration::from_secs(10), next_decoded(&mut socket, budget))
                .await
                .expect("the pings renewed the budget and the read never ended");
        let waited = started.elapsed();

        let err = outcome
            .expect_err("a socket that only pings has nothing to decode")
            .to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(
            waited >= budget,
            "it gave up after {waited:?}, short of its {budget:?} budget"
        );
        assert!(
            waited < Duration::from_secs(2),
            "it waited {waited:?} on a {budget:?} budget"
        );
    }

    /// The skipped frames are skipped, not fatal: a message behind them still
    /// arrives. Without this the deadline test above would pass on a decoder
    /// that had simply stopped reading.
    #[tokio::test]
    async fn a_message_behind_the_pings_still_arrives_inside_the_budget() {
        let pong = serde_json::to_string(&ServerMessage::Pong).unwrap();
        let mut socket = Cadence::new(Duration::from_millis(10), 3, Some(Message::Text(pong)));
        let message = next_decoded(&mut socket, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(matches!(message, ServerMessage::Pong), "got {message:?}");
    }
}
