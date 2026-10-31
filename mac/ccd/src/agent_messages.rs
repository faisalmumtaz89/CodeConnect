//! **Messages between hosted agents.**
//!
//! An agent names another as `cc-N@<computer>`, the computer being its Tailscale
//! name ([`protocol::tailnet`]). The daemon of the computer the receiving agent runs
//! on keeps the message (`agent_messages` in [`crate::store`]) until that agent is
//! free, then gives it to the agent as a turn of its own: never into a turn that is
//! running, and never into the person's draft.
//!
//! A message is handed out at most once. Its row is marked `handing_out` and
//! committed before the agent is given it, and a daemon that stops in between finds
//! the row at start-up and records the outcome as unknown rather than giving it
//! again — unless the store can show nothing reached the agent (see
//! [`crate::store::Store::recover_agent_messages`]), when it waits again.
//!
//! **How each agent is given one.** Codex: as a turn started through the app-server
//! link, once its thread has been idle for a second. Claude, busy: at its turn's
//! end, all held at once, through its `Stop` hook ([`Daemon::agent_messages_at_turn_end`]).
//! Claude, idle: typed into its message box and submitted, only while its
//! transcript shows no turn open
//! ([`ClaudeTurn`]) and, read by its supervisor immediately before the keys, no
//! turn is running on screen and the box is empty
//! ([`protocol::ipc::composer_is_free`]).
//!
//! **Across computers.** A message to `cc-N@<another computer>` is carried by this
//! daemon to that computer's, over its WebSocket listener on the tailnet, as one
//! [`protocol::peer`] request; that daemon takes it as its own intake does
//! ([`Daemon::take_agent_message`]) and answers with the outcome. Only the same
//! owner's untagged computers are served, read from the receiver's own `tailscale
//! status`, never from the request.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};

use protocol::agent::AgentKind;
use protocol::event::{Event, EventKind};
use protocol::ipc::{AgentEntry, AgentMessageOutcome};
use protocol::peer::{PeerReply, PeerRequest};
use protocol::tailnet::{Place, Refusal, Tailnet};

use crate::state::Daemon;
use crate::store::AgentMessageFiling;

/// Why a message waits: its agent has a turn running, or one ended under a second
/// ago (see [`crate::codex_link::agent_message_hold`]).
pub(crate) const HELD_WORKING: &str =
    "the agent is working; it gets the message when its turn ends";
/// Why a message waits: the person stopped the agent's last turn.
pub(crate) const HELD_AFTER_INTERRUPT: &str =
    "the person stopped the agent's last turn; it gets the message after their next turn ends";
/// Why a message waits: this daemon's link to the agent's session is not ready to
/// start a turn (still connecting, reconnecting, or not yet watching the thread).
const HELD_NOT_READY: &str =
    "the agent's session is not ready to take a message yet; it gets it when it is";
/// Why a message waits: Claude's screen shows a turn running, or its message box
/// holds a draft, a menu or a view.
pub(crate) const HELD_COMPOSER_BUSY: &str =
    "the agent's message box is in use or a turn is running; it gets the message when the \
     box is free and empty";

/// The same words from the same agent inside this window are a repeat.
const REPEAT_WINDOW_MS: i64 = 10 * 60 * 1000;

/// How often a run's held messages are offered again.
const RETRY_EVERY: Duration = Duration::from_secs(1);

/// How long `tailscale status` gets to answer.
const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(3);

/// How long reaching another computer's daemon may take before nothing has been
/// sent: a computer that is up answers a connection in milliseconds.
const PEER_CONNECT_WAIT: Duration = Duration::from_secs(3);

/// How long another computer's daemon gets to say what became of a message:
/// longer than the receiving agent's own hand-out, a supervisor round trip
/// included, so its answer is what the sender is told.
const PEER_MESSAGE_WAIT: Duration = Duration::from_secs(20);

/// How long another computer gets to list its agents before `list_agents` goes on
/// without it.
const PEER_LIST_WAIT: Duration = Duration::from_secs(3);

impl Daemon {
    /// **Take one agent's message**, and say what became of it: given to an agent
    /// on this computer, or carried to the computer the address names.
    pub async fn send_agent_message(
        self: &Arc<Self>,
        from_uid: &str,
        to: &str,
        text: &str,
    ) -> AgentMessageOutcome {
        let tailnet = read_tailnet().await;
        self.send_agent_message_on(tailnet.as_ref(), from_uid, to, text)
            .await
    }

    /// [`Daemon::send_agent_message`] against a given reading of the tailnet.
    pub(crate) async fn send_agent_message_on(
        self: &Arc<Self>,
        tailnet: Option<&Tailnet>,
        from_uid: &str,
        to: &str,
        text: &str,
    ) -> AgentMessageOutcome {
        let refuse = |reason: String| AgentMessageOutcome::Refused { reason };

        let Some((name, computer)) = to
            .rsplit_once('@')
            .filter(|(n, c)| !n.is_empty() && !c.is_empty())
        else {
            return refuse("address an agent as cc-N@computer, for example cc-2@studio".into());
        };
        if let Err(reason) = sendable(text) {
            return refuse(reason.into());
        }
        let Some(tailnet) = tailnet else {
            return refuse(
                "this computer is not connected to Tailscale, so it has no name to send from"
                    .into(),
            );
        };
        let place = match tailnet.locate(computer) {
            Ok(place) => place,
            Err(refusal) => return refuse(refusal_sentence(refusal, computer)),
        };
        let sender = match self.db.get_session(from_uid.to_string()).await {
            Ok(Some(row)) => row,
            Ok(None) => return refuse("the sending session is not known to CodeConnect".into()),
            Err(err) => {
                crate::log_error!("agent message from {from_uid}: session lookup failed: {err:#}");
                return refuse("CodeConnect could not read its sessions".into());
            }
        };
        let agent = agent_name(&sender.agent).to_string();
        let project = crate::project_label::project_label(&sender.cwd);

        let Place::Peer { name: peer, ip } = place else {
            let from = format!("{}@{}", sender.session_id, tailnet.name());
            return self
                .take_agent_message(&from, &agent, &project, name, Some(from_uid), text)
                .await;
        };
        let request = PeerRequest::PeerAgentMessage {
            from: sender.session_id,
            agent,
            project,
            to: name.to_string(),
            text: text.to_string(),
        };
        match dial_peer(
            SocketAddr::new(ip, self.config.ws_port),
            &request,
            PEER_MESSAGE_WAIT,
        )
        .await
        {
            Ok(PeerReply::PeerAgentMessageResult { outcome }) => outcome,
            Ok(PeerReply::PeerRefused { reason }) => refuse(format!("{peer} refused it: {reason}")),
            Ok(PeerReply::PeerAgents { .. }) => AgentMessageOutcome::Indeterminate {
                reason: format!("{peer} answered with something other than this message's outcome"),
            },
            Err(Dial::Unreachable(why)) => refuse(format!(
                "{peer} could not be reached ({why}); nothing was sent"
            )),
            Err(Dial::Older) => refuse(format!(
                "{peer} runs an older CodeConnect, which takes no messages from agents"
            )),
            Err(Dial::NoAnswer(why)) => AgentMessageOutcome::Indeterminate {
                reason: format!(
                    "{peer} was sent the message but did not say what became of it ({why}); \
                     it is not sent again"
                ),
            },
        }
    }

    /// **Keep one message for an agent on this computer** and hand it out if the
    /// agent is free: the same for a sender here and for one on another computer
    /// (`from_uid` is the sender's run when it is here).
    async fn take_agent_message(
        self: &Arc<Self>,
        from: &str,
        agent: &str,
        project: &str,
        name: &str,
        from_uid: Option<&str>,
        text: &str,
    ) -> AgentMessageOutcome {
        let refuse = |reason: String| AgentMessageOutcome::Refused { reason };
        if let Err(reason) = sendable(text) {
            return refuse(reason.into());
        }
        let target = match self.attached_run(name).await {
            Ok(Some(row)) => row,
            Ok(None) => return refuse(format!("{name} is not a running session")),
            Err(err) => {
                crate::log_error!("agent message to {name}: session lookup failed: {err:#}");
                return refuse("CodeConnect could not read its sessions".into());
            }
        };
        // As `list_agents` leaves the asker out: a message to oneself would come back
        // as a turn whose header asks for a reply to oneself.
        if Some(target.session_uid.as_str()) == from_uid {
            return refuse(format!("{name} is this session's own address"));
        }
        if let AgentKind::Unsupported(agent) = &target.agent {
            return refuse(format!("{name} runs {agent}, which takes no messages"));
        }

        let message = format!(
            "[CodeConnect] Message from {from} ({agent} · project {project}). Reply with \
             send_message to {from}.\n\n{text}"
        );
        if message.len() > protocol::ws::MAX_COMPOSE_BYTES {
            return refuse(format!(
                "the message is too long ({} bytes with its header; at most {})",
                message.len(),
                protocol::ws::MAX_COMPOSE_BYTES
            ));
        }

        let now_ms = protocol::time::now_unix_ms();
        let filing = self
            .db
            .file_agent_message(
                target.session_uid.clone(),
                from.to_string(),
                protocol::hash::sha256_hex(text.as_bytes()),
                message,
                protocol::time::rfc3339_from_unix_ms(now_ms),
                protocol::time::rfc3339_from_unix_ms(now_ms - REPEAT_WINDOW_MS),
            )
            .await;
        let id = match filing {
            Ok(AgentMessageFiling::Held(id)) => id,
            Ok(AgentMessageFiling::Duplicate) => return AgentMessageOutcome::Duplicate,
            Ok(AgentMessageFiling::NoSession) => {
                return refuse(format!("{name} is not a running session"))
            }
            Err(err) => {
                crate::log_error!("agent message to {name}: could not keep it: {err:#}");
                return refuse("CodeConnect could not keep the message".into());
            }
        };

        let held = self.hand_out_agent_messages(&target.session_uid).await;
        match self.db.agent_message_status(id).await {
            Ok(Some(status)) if status == "delivered" => AgentMessageOutcome::Delivered,
            Ok(Some(status)) if status == "unknown" => AgentMessageOutcome::Indeterminate {
                reason: "it was handed to the agent and whether it arrived is not known".into(),
            },
            Ok(_) | Err(_) => {
                self.retry_agent_messages(&target.session_uid).await;
                AgentMessageOutcome::Held {
                    reason: held.unwrap_or_else(|| HELD_WORKING.to_string()),
                }
            }
        }
    }

    /// **Serve one request from another computer's daemon**, `source` being where
    /// the connection came from. Only the same owner's untagged computers are
    /// served, read from this computer's own `tailscale status`.
    pub(crate) async fn serve_peer(
        self: &Arc<Self>,
        source: IpAddr,
        request: PeerRequest,
    ) -> PeerReply {
        let tailnet = read_tailnet().await;
        self.serve_peer_on(tailnet.as_ref(), source, request).await
    }

    /// [`Daemon::serve_peer`] against a given reading of the tailnet.
    pub(crate) async fn serve_peer_on(
        self: &Arc<Self>,
        tailnet: Option<&Tailnet>,
        source: IpAddr,
        request: PeerRequest,
    ) -> PeerReply {
        let refuse = |reason: &str| PeerReply::PeerRefused {
            reason: reason.to_string(),
        };
        let Some(tailnet) = tailnet else {
            return refuse("the receiving computer is not connected to Tailscale");
        };
        let sender = match tailnet.sender(source) {
            Ok(node) => node.name.clone(),
            Err(refusal) => {
                crate::log_warn!("refused a peer request from {source}: {refusal:?}");
                return refuse(match refusal {
                    Refusal::OtherUser => "the sending computer belongs to another Tailscale user",
                    Refusal::Tagged => "the sending computer is a tagged device",
                    Refusal::ThisComputerTagged => "the receiving computer is a tagged device",
                    Refusal::Unknown | Refusal::Offline => {
                        "the request did not come from one of your computers on Tailscale"
                    }
                });
            }
        };
        match request {
            PeerRequest::PeerListAgents => match self.local_agents(tailnet.name(), None).await {
                Ok(agents) => PeerReply::PeerAgents { agents },
                Err(_) => refuse("the receiving computer could not read its sessions"),
            },
            PeerRequest::PeerAgentMessage {
                from,
                agent,
                project,
                to,
                text,
            } => {
                if [&from, &agent, &project].iter().any(|label| {
                    label.is_empty() || label.contains('@') || label.chars().any(char::is_control)
                }) {
                    return refuse(
                        "the sender's name, agent or project is not one this computer reads",
                    );
                }
                PeerReply::PeerAgentMessageResult {
                    outcome: self
                        .take_agent_message(
                            &format!("{from}@{sender}"),
                            &agent,
                            &project,
                            &to,
                            None,
                            &text,
                        )
                        .await,
                }
            }
        }
    }

    /// **The agents `from_uid` can message**: the other running sessions on this
    /// computer and on the owner's other computers, one per name, after the
    /// asker's own address. `Err` says why there is no list.
    pub async fn list_agents(&self, from_uid: &str) -> Result<(String, Vec<AgentEntry>), String> {
        let tailnet = read_tailnet().await;
        let mut peers = Vec::new();
        if let Some(tailnet) = &tailnet {
            let asks = tailnet.reachable_peers().filter_map(|place| match place {
                Place::Peer { ip, .. } => Some(dial_peer(
                    SocketAddr::new(ip, self.config.ws_port),
                    &PeerRequest::PeerListAgents,
                    PEER_LIST_WAIT,
                )),
                Place::Here => None,
            });
            for reply in futures_util::future::join_all(asks).await {
                if let Ok(PeerReply::PeerAgents { agents }) = reply {
                    peers.extend(agents);
                }
            }
        }
        self.list_agents_on(tailnet.as_ref(), from_uid, peers).await
    }

    /// [`Daemon::list_agents`] against a given reading of the tailnet, and the
    /// agents its other computers answered with.
    pub(crate) async fn list_agents_on(
        &self,
        tailnet: Option<&Tailnet>,
        from_uid: &str,
        peers: Vec<AgentEntry>,
    ) -> Result<(String, Vec<AgentEntry>), String> {
        let Some(tailnet) = tailnet else {
            return Err(
                "this computer is not connected to Tailscale, so it has no name to send from"
                    .into(),
            );
        };
        let unreadable = |err: anyhow::Error| {
            crate::log_error!("listing agents for {from_uid}: {err:#}");
            "CodeConnect could not read its sessions".to_string()
        };
        let you = match self
            .db
            .get_session(from_uid.to_string())
            .await
            .map_err(unreadable)?
        {
            Some(row) => format!("{}@{}", row.session_id, tailnet.name()),
            None => return Err("the asking session is not known to CodeConnect".into()),
        };
        let mut agents = self
            .local_agents(tailnet.name(), Some(from_uid))
            .await
            .map_err(unreadable)?;
        agents.extend(peers);
        Ok((you, agents))
    }

    /// This computer's running sessions, one per name (the newest), addressed with
    /// this computer's name, leaving out `except_uid`.
    async fn local_agents(
        &self,
        this_computer: &str,
        except_uid: Option<&str>,
    ) -> anyhow::Result<Vec<AgentEntry>> {
        let runs = self.attached_runs().await?;
        let mut newest: std::collections::BTreeMap<String, crate::store::SessionRow> =
            std::collections::BTreeMap::new();
        for row in runs
            .into_iter()
            .filter(|row| Some(row.session_uid.as_str()) != except_uid)
        {
            match newest.get(&row.session_id) {
                Some(kept) if kept.session_uid >= row.session_uid => {}
                _ => {
                    newest.insert(row.session_id.clone(), row);
                }
            }
        }
        Ok(newest
            .into_values()
            .map(|row| AgentEntry {
                address: format!("{}@{this_computer}", row.session_id),
                agent: agent_name(&row.agent).to_string(),
                project: crate::project_label::project_label(&row.cwd),
            })
            .collect())
    }

    /// Offer a run's held messages again every second until none is left or its
    /// supervisor is gone. A supervisor that registers again starts this anew.
    pub async fn retry_agent_messages(self: &Arc<Self>, session_uid: &str) {
        if !self
            .agent_message_retries
            .lock()
            .await
            .insert(session_uid.to_string())
        {
            return;
        }
        let daemon = Arc::clone(self);
        let session_uid = session_uid.to_string();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RETRY_EVERY).await;
                let attached = daemon.is_attached(&session_uid).await;
                if attached {
                    daemon.hand_out_agent_messages(&session_uid).await;
                }
                // Decided under the set's lock: a sender that finds this run in the set
                // leaves its message to this loop, so the loop may only leave once it has
                // seen that nothing is held.
                let mut retries = daemon.agent_message_retries.lock().await;
                let waiting = attached
                    && daemon
                        .db
                        .held_agent_messages(session_uid.clone())
                        .await
                        .map_or(true, |held| !held.is_empty());
                if !waiting {
                    retries.remove(&session_uid);
                    return;
                }
            }
        });
    }

    /// **Hand out a run's oldest held message**, if its agent is free.
    ///
    /// One per pass: a message that starts a turn makes the agent busy, and the next
    /// waits for that turn's end like any other. Returns why the messages left wait,
    /// or `None` when none is left.
    pub(crate) async fn hand_out_agent_messages(&self, session_uid: &str) -> Option<String> {
        let gate = self.agent_message_gate(session_uid).await;
        let _one_pass = gate.lock().await;

        let held = match self.db.held_agent_messages(session_uid.to_string()).await {
            Ok(held) => held,
            Err(err) => {
                crate::log_error!("agent messages for {session_uid}: could not read them: {err:#}");
                return Some(HELD_WORKING.to_string());
            }
        };
        if held.is_empty() {
            return None;
        }
        let claude = match self.db.get_session(session_uid.to_string()).await {
            Ok(Some(row)) => row.agent == AgentKind::Claude,
            Ok(None) => return Some(HELD_NOT_READY.to_string()),
            Err(err) => {
                crate::log_error!(
                    "agent messages for {session_uid}: session lookup failed: {err:#}"
                );
                return Some(HELD_NOT_READY.to_string());
            }
        };
        if claude {
            match self.db.claude_turn(session_uid.to_string()).await {
                Ok(ClaudeTurn::Free) => {}
                Ok(ClaudeTurn::Running) => return Some(HELD_WORKING.to_string()),
                Ok(ClaudeTurn::Interrupted) => return Some(HELD_AFTER_INTERRUPT.to_string()),
                Err(err) => {
                    crate::log_error!("agent messages for {session_uid}: turn unreadable: {err:#}");
                    return Some(HELD_WORKING.to_string());
                }
            }
        }
        let count = held.len();
        for (index, row) in held.into_iter().enumerate() {
            match self.db.claim_agent_message(row.id).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(err) => {
                    crate::log_error!("agent message {}: could not mark it: {err:#}", row.id);
                    return Some(HELD_WORKING.to_string());
                }
            }
            let (status, waits) = if claude {
                self.type_agent_message_into(session_uid, row.id, row.message)
                    .await
            } else {
                self.compose_agent_message(session_uid, row.id, row.message)
                    .await
            };
            if let Err(err) = self
                .db
                .settle_agent_message(row.id, status, protocol::time::now_rfc3339())
                .await
            {
                crate::log_error!(
                    "agent message {}: could not record {status}: {err:#}",
                    row.id
                );
            }
            if waits.is_some() {
                return waits;
            }
            if status == "delivered" {
                return (index + 1 < count).then(|| HELD_WORKING.to_string());
            }
        }
        None
    }

    /// **Hand a busy Claude its held messages at its turn's end**, oldest first, as
    /// one text for its `Stop` hook to print: Claude takes them up after its own
    /// answer, in a continuation of that turn. Each is claimed before it is handed
    /// over and recorded delivered with it, so it goes at most once. A hook Claude
    /// cancels after this (its own timeout) loses them, and this daemon cannot
    /// tell.
    pub(crate) async fn agent_messages_at_turn_end(&self, session_uid: &str) -> Option<String> {
        let gate = self.agent_message_gate(session_uid).await;
        let _one_pass = gate.lock().await;
        let held = match self.db.held_agent_messages(session_uid.to_string()).await {
            Ok(held) => held,
            Err(err) => {
                crate::log_error!("agent messages for {session_uid}: could not read them: {err:#}");
                return None;
            }
        };
        let mut handed = Vec::new();
        for row in held {
            match self.db.claim_agent_message(row.id).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(err) => {
                    crate::log_error!("agent message {}: could not mark it: {err:#}", row.id);
                    break;
                }
            }
            if let Err(err) = self
                .db
                .settle_agent_message(row.id, "delivered", protocol::time::now_rfc3339())
                .await
            {
                crate::log_error!(
                    "agent message {}: could not record delivered: {err:#}",
                    row.id
                );
            }
            handed.push(row.message);
        }
        (!handed.is_empty()).then(|| handed.join("\n\n"))
    }

    /// The lock one run's hand-outs take, so a turn's end and the retry loop never
    /// hand out the same run's messages at once.
    async fn agent_message_gate(&self, session_uid: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.agent_message_gates.lock().await;
        gates.retain(|_, gate| Arc::strong_count(gate) > 1);
        Arc::clone(gates.entry(session_uid.to_string()).or_default())
    }

    /// Give a Claude run one message by typing it into its composer, and say what
    /// became of it: its new status, and why it waits if it does.
    async fn type_agent_message_into(
        &self,
        session_uid: &str,
        id: i64,
        message: String,
    ) -> (&'static str, Option<String>) {
        match self.type_agent_message(session_uid, message).await {
            protocol::ws::SendTextResult::Sent { .. } => ("delivered", None),
            protocol::ws::SendTextResult::Refused { reason } => {
                crate::log_info!("agent message {id} to {session_uid} waits: {reason}");
                let why = if reason == crate::state::SUPERVISOR_PREDATES_AGENT_MESSAGES {
                    reason
                } else {
                    HELD_COMPOSER_BUSY.to_string()
                };
                ("held", Some(why))
            }
            other => {
                crate::log_warn!(
                    "agent message {id} to {session_uid}: outcome unknown ({other:?})"
                );
                ("unknown", None)
            }
        }
    }

    /// Give a Codex run one message as a turn of its own, and say what became of it:
    /// its new status, and why it waits if it does.
    async fn compose_agent_message(
        &self,
        session_uid: &str,
        id: i64,
        message: String,
    ) -> (&'static str, Option<String>) {
        let hash = protocol::hash::compose_hash(session_uid, &message);
        let result = self
            .compose_as(
                session_uid,
                &format!("{}{id}", crate::store::AGENT_MESSAGE_REQUEST_PREFIX),
                message,
                &hash,
                true,
            )
            .await;
        match result {
            protocol::ws::ComposeResult::Started { .. }
            | protocol::ws::ComposeResult::Steered { .. }
            | protocol::ws::ComposeResult::Duplicate { .. } => ("delivered", None),
            protocol::ws::ComposeResult::Indeterminate { reason } => {
                crate::log_warn!("agent message {id} to {session_uid}: outcome unknown ({reason})");
                ("unknown", None)
            }
            // The link's own sentences about its state are written for a phone
            // ("nothing was sent"); the sender is told the message waits.
            protocol::ws::ComposeResult::Rejected { reason } => {
                let why = if reason == HELD_WORKING || reason == HELD_AFTER_INTERRUPT {
                    reason
                } else {
                    crate::log_info!("agent message {id} to {session_uid} waits: {reason}");
                    HELD_NOT_READY.to_string()
                };
                ("held", Some(why))
            }
        }
    }
}

/// Whether a message's text may be sent at all: not empty, and no control
/// character but newlines and tabs.
fn sendable(text: &str) -> Result<(), &'static str> {
    if text.is_empty() {
        return Err("the message is empty");
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("the message holds a control character; only newlines and tabs may be sent");
    }
    Ok(())
}

/// Why another computer's daemon gave no answer to read.
#[derive(Debug)]
enum Dial {
    /// No connection was made, so nothing was sent.
    Unreachable(String),
    /// It answered the request as an undecodable message: a daemon from before
    /// peers existed.
    Older,
    /// The request was sent and no answer came: whether it acted is not known.
    NoAnswer(String),
}

/// **Ask another computer's daemon one thing**: connect to its WebSocket
/// listener, send the request as the first frame, read one reply.
async fn dial_peer(
    addr: SocketAddr,
    request: &PeerRequest,
    wait: Duration,
) -> Result<PeerReply, Dial> {
    let stream =
        match tokio::time::timeout(PEER_CONNECT_WAIT, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(err)) => return Err(Dial::Unreachable(err.to_string())),
            Err(_) => return Err(Dial::Unreachable("no answer to the connection".into())),
        };
    let url = format!("ws://{addr}/");
    let mut socket = match tokio::time::timeout(
        PEER_CONNECT_WAIT,
        tokio_tungstenite::client_async(&url, stream),
    )
    .await
    {
        Ok(Ok((socket, _))) => socket,
        Ok(Err(err)) => return Err(Dial::Unreachable(err.to_string())),
        Err(_) => return Err(Dial::Unreachable("no WebSocket answer".into())),
    };
    let frame = serde_json::to_string(request).map_err(|err| Dial::Unreachable(err.to_string()))?;
    if let Err(err) = socket
        .send(tokio_tungstenite::tungstenite::Message::Text(frame))
        .await
    {
        return Err(Dial::NoAnswer(err.to_string()));
    }
    let answer = tokio::time::timeout(wait, async {
        while let Some(message) = socket.next().await {
            match message {
                Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => return Ok(text),
                Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            }
        }
        Err("the connection closed without an answer".to_string())
    })
    .await;
    let _ = socket.close(None).await;
    let text = match answer {
        Ok(Ok(text)) => text,
        Ok(Err(why)) => return Err(Dial::NoAnswer(why)),
        Err(_) => return Err(Dial::NoAnswer(format!("no answer in {} s", wait.as_secs()))),
    };
    if let Ok(reply) = serde_json::from_str::<PeerReply>(&text) {
        return Ok(reply);
    }
    match serde_json::from_str::<protocol::ws::ServerMessage>(&text) {
        Ok(protocol::ws::ServerMessage::Error { code, .. }) if code == "bad_request" => {
            Err(Dial::Older)
        }
        _ => Err(Dial::NoAnswer("an answer this build does not read".into())),
    }
}

/// The reading [`read_tailnet`] gives instead of `tailscale status`, for a test
/// that has to be a computer of its own.
#[cfg(test)]
pub(crate) static TEST_TAILNET: std::sync::Mutex<Option<Tailnet>> = std::sync::Mutex::new(None);

/// This computer's tailnet, or `None` when Tailscale is missing, down or slow.
async fn read_tailnet() -> Option<Tailnet> {
    #[cfg(test)]
    if let Some(tailnet) = TEST_TAILNET
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
    {
        return Some(tailnet);
    }
    let bin = protocol::pairing::tailscale_bin()?;
    let run = tokio::process::Command::new(bin)
        .args(["status", "--json"])
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(TAILSCALE_TIMEOUT, run)
        .await
        .ok()?
        .ok()?;
    Tailnet::parse(&String::from_utf8_lossy(&output.stdout))
}

fn refusal_sentence(refusal: Refusal, computer: &str) -> String {
    match refusal {
        Refusal::Unknown => format!("{computer} is not a computer on your Tailscale network"),
        Refusal::OtherUser => format!("{computer} belongs to another Tailscale user"),
        Refusal::Tagged => format!("{computer} is a tagged device, which takes no messages"),
        Refusal::ThisComputerTagged => {
            "this computer is a tagged device, which takes part in no messages".into()
        }
        Refusal::Offline => format!("{computer} is offline"),
    }
}

/// **Where a Claude run's turns stand**, by its transcript as the log holds it.
///
/// A turn opens on a line Claude Code hands to the agent as a prompt
/// ([`crate::state::delivers_prompt_line`]); `/compact` writes none (2.1.314,
/// measured). It closes at a turn end the tailer files: a `stop_hook_summary`, or
/// a `turn_duration` that ends its turn, which covers an API error. Claude's
/// interrupt marker, written on Esc and on "No" at a tool's prompt, leaves the run
/// interrupted until a later prompt's turn ends: Claude is asking the person what
/// to do instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeTurn {
    /// No turn open, and the last one was not stopped by the person.
    Free,
    /// A prompt was handed over and its turn has not ended.
    Running,
    /// The person stopped the last turn.
    Interrupted,
}

impl ClaudeTurn {
    /// What `event`, read newest first, settles, or `None` to read on. `ended` says
    /// whether a turn end newer than it has been read.
    pub(crate) fn settled_by(event: &Event, ended: &mut bool) -> Option<ClaudeTurn> {
        if event.kind == EventKind::TurnComplete {
            *ended = true;
            return None;
        }
        if is_interrupt_marker(event) {
            return Some(ClaudeTurn::Interrupted);
        }
        crate::state::delivers_prompt_line(&event.kind, &event.payload).then_some(if *ended {
            ClaudeTurn::Free
        } else {
            ClaudeTurn::Running
        })
    }
}

/// Claude Code's own record that the person stopped the turn: an unstamped,
/// non-meta user line whose content is exactly one text block reading
/// `[Request interrupted by user]` or `[Request interrupted by user for tool use]`.
/// The phone's `isClaudeInterrupt`, the same test.
fn is_interrupt_marker(event: &Event) -> bool {
    let line = &event.payload;
    if event.kind != EventKind::UserMessage || line["isMeta"] == true || !line["origin"].is_null() {
        return false;
    }
    let Some([block]) = line["message"]["content"].as_array().map(Vec::as_slice) else {
        return false;
    };
    block["type"] == "text"
        && matches!(
            block["text"].as_str(),
            Some("[Request interrupted by user]" | "[Request interrupted by user for tool use]")
        )
}

fn agent_name(agent: &AgentKind) -> &str {
    match agent {
        AgentKind::Claude => "Claude",
        AgentKind::Codex => "Codex",
        AgentKind::Unsupported(name) => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::Message;

    /// A listener on loopback that answers the first frame with `answer`, or never
    /// when it is `None`, and hands back its address.
    async fn fake_daemon(answer: Option<String>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = socket.next().await;
            match answer {
                Some(text) => {
                    let _ = socket.send(Message::Text(text)).await;
                }
                None => tokio::time::sleep(Duration::from_secs(5)).await,
            }
        });
        addr
    }

    /// **What another computer's answer means to the sender**: its reply; a daemon
    /// from before peers, which answers `bad_request`; silence after the request
    /// went out; and no connection at all, when nothing was sent.
    #[tokio::test]
    async fn a_peer_is_read_as_answered_older_silent_or_unreachable() {
        let wait = Duration::from_millis(500);
        let delivered = PeerReply::PeerAgentMessageResult {
            outcome: AgentMessageOutcome::Delivered,
        };
        let addr = fake_daemon(Some(serde_json::to_string(&delivered).unwrap())).await;
        assert_eq!(
            dial_peer(addr, &PeerRequest::PeerListAgents, wait)
                .await
                .unwrap(),
            delivered
        );

        let older = serde_json::to_string(&protocol::ws::ServerMessage::Error {
            code: "bad_request".into(),
            message: "undecodable message".into(),
        })
        .unwrap();
        let addr = fake_daemon(Some(older)).await;
        assert!(matches!(
            dial_peer(addr, &PeerRequest::PeerListAgents, wait).await,
            Err(Dial::Older)
        ));

        let addr = fake_daemon(None).await;
        assert!(matches!(
            dial_peer(addr, &PeerRequest::PeerListAgents, wait).await,
            Err(Dial::NoAnswer(_))
        ));

        let closed = {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            listener.local_addr().unwrap()
        };
        assert!(matches!(
            dial_peer(closed, &PeerRequest::PeerListAgents, wait).await,
            Err(Dial::Unreachable(_))
        ));
    }
}
