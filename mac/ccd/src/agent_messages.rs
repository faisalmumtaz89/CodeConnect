//! **Messages between hosted agents.**
//!
//! An agent names another as `cc-N@<computer>`, the computer being its Tailscale
//! name ([`protocol::tailnet`]). The daemon of the computer the receiving agent runs
//! on keeps the message (`agent_messages` in [`crate::store`]) for that agent.
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
use protocol::ipc::{AgentEntry, AgentMessageOutcome};
use protocol::peer::{PeerReply, PeerRequest};
use protocol::tailnet::{Place, Refusal, Tailnet};

use crate::state::Daemon;
use crate::store::AgentMessageFiling;

/// What a kept message is told.
const HELD: &str = "kept for the agent";

/// How long `tailscale status` gets to answer.
const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(3);

/// How long reaching another computer's daemon may take before nothing has been
/// sent: a computer that is up answers a connection in milliseconds.
const PEER_CONNECT_WAIT: Duration = Duration::from_secs(3);

/// How long another computer's daemon gets to say what became of a message.
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

    /// **Keep one message for an agent on this computer**: the same for a sender
    /// here and for one on another computer (`from_uid` is the sender's run when it
    /// is here).
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

        let filing = self
            .db
            .file_agent_message(
                target.session_uid.clone(),
                from.to_string(),
                message,
                protocol::time::now_rfc3339(),
            )
            .await;
        match filing {
            Ok(AgentMessageFiling::Held) => AgentMessageOutcome::Held {
                reason: HELD.to_string(),
            },
            Ok(AgentMessageFiling::NoSession) => refuse(format!("{name} is not a running session")),
            Err(err) => {
                crate::log_error!("agent message to {name}: could not keep it: {err:#}");
                refuse("CodeConnect could not keep the message".into())
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
            outcome: AgentMessageOutcome::Held {
                reason: HELD.to_string(),
            },
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
