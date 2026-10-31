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
//! This build delivers to Codex sessions on this computer.

use std::sync::Arc;
use std::time::Duration;

use protocol::agent::AgentKind;
use protocol::ipc::{AgentEntry, AgentMessageOutcome};
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

/// The same words from the same agent inside this window are a repeat.
const REPEAT_WINDOW_MS: i64 = 10 * 60 * 1000;

/// How often a run's held messages are offered again.
const RETRY_EVERY: Duration = Duration::from_secs(1);

/// How long `tailscale status` gets to answer.
const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(3);

impl Daemon {
    /// **Take one agent's message**, and say what became of it.
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
        if text.is_empty() {
            return refuse("the message is empty".into());
        }
        if text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return refuse(
                "the message holds a control character; only newlines and tabs may be sent".into(),
            );
        }
        let Some(tailnet) = tailnet else {
            return refuse(
                "this computer is not connected to Tailscale, so it has no name to send from"
                    .into(),
            );
        };
        match tailnet.locate(computer) {
            Ok(Place::Here) => {}
            Ok(Place::Peer { .. }) => {
                return refuse("messages to other computers are not built in this build".into())
            }
            Err(refusal) => return refuse(refusal_sentence(refusal, computer)),
        }

        let sender = match self.db.get_session(from_uid.to_string()).await {
            Ok(Some(row)) => row,
            Ok(None) => return refuse("the sending session is not known to CodeConnect".into()),
            Err(err) => {
                crate::log_error!("agent message from {from_uid}: session lookup failed: {err:#}");
                return refuse("CodeConnect could not read its sessions".into());
            }
        };
        let target = match self.attached_run(name).await {
            Ok(Some(row)) => row,
            Ok(None) => return refuse(format!("{to} is not a running session")),
            Err(err) => {
                crate::log_error!("agent message to {name}: session lookup failed: {err:#}");
                return refuse("CodeConnect could not read its sessions".into());
            }
        };
        // As `list_agents` leaves the asker out: a message to oneself would come back
        // as a turn whose header asks for a reply to oneself.
        if target.session_uid == from_uid {
            return refuse(format!("{to} is this session's own address"));
        }
        if target.agent != AgentKind::Codex {
            return refuse(format!(
                "{to} runs {}, and delivery to it is not built in this build",
                agent_name(&target.agent)
            ));
        }

        let from = format!("{}@{}", sender.session_id, tailnet.name());
        let message = format!(
            "[CodeConnect] Message from {from} ({} · project {}). Reply with send_message to {from}.\n\n{text}",
            agent_name(&sender.agent),
            crate::project_label::project_label(&sender.cwd),
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
                from,
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
                return refuse(format!("{to} is not a running session"))
            }
            Err(err) => {
                crate::log_error!("agent message to {to}: could not keep it: {err:#}");
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

    /// **The agents `from_uid` can message**: the other running sessions on this
    /// computer, one per name, by name, after the asker's own address. `Err` says
    /// why there is no list.
    pub async fn list_agents(&self, from_uid: &str) -> Result<(String, Vec<AgentEntry>), String> {
        let tailnet = read_tailnet().await;
        self.list_agents_on(tailnet.as_ref(), from_uid).await
    }

    /// [`Daemon::list_agents`] against a given reading of the tailnet.
    pub(crate) async fn list_agents_on(
        &self,
        tailnet: Option<&Tailnet>,
        from_uid: &str,
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
        let runs = self.attached_runs().await.map_err(unreadable)?;
        let mut newest: std::collections::BTreeMap<String, crate::store::SessionRow> =
            std::collections::BTreeMap::new();
        for row in runs.into_iter().filter(|row| row.session_uid != from_uid) {
            match newest.get(&row.session_id) {
                Some(kept) if kept.session_uid >= row.session_uid => {}
                _ => {
                    newest.insert(row.session_id.clone(), row);
                }
            }
        }
        let agents = newest
            .into_values()
            .map(|row| AgentEntry {
                address: format!("{}@{}", row.session_id, tailnet.name()),
                agent: agent_name(&row.agent).to_string(),
                project: crate::project_label::project_label(&row.cwd),
            })
            .collect();
        Ok((you, agents))
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
    async fn hand_out_agent_messages(&self, session_uid: &str) -> Option<String> {
        let gate = {
            let mut gates = self.agent_message_gates.lock().await;
            gates.retain(|_, gate| Arc::strong_count(gate) > 1);
            Arc::clone(gates.entry(session_uid.to_string()).or_default())
        };
        let _one_pass = gate.lock().await;

        let held = match self.db.held_agent_messages(session_uid.to_string()).await {
            Ok(held) => held,
            Err(err) => {
                crate::log_error!("agent messages for {session_uid}: could not read them: {err:#}");
                return Some(HELD_WORKING.to_string());
            }
        };
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
            let hash = protocol::hash::compose_hash(session_uid, &row.message);
            let result = self
                .compose_as(
                    session_uid,
                    &format!("{}{}", crate::store::AGENT_MESSAGE_REQUEST_PREFIX, row.id),
                    row.message,
                    &hash,
                    true,
                )
                .await;
            let (status, waits) = match result {
                protocol::ws::ComposeResult::Started { .. }
                | protocol::ws::ComposeResult::Steered { .. }
                | protocol::ws::ComposeResult::Duplicate { .. } => ("delivered", None),
                protocol::ws::ComposeResult::Indeterminate { reason } => {
                    crate::log_warn!(
                        "agent message {} to {session_uid}: outcome unknown ({reason})",
                        row.id
                    );
                    ("unknown", None)
                }
                // The link's own sentences about its state are written for a phone
                // ("nothing was sent"); the sender is told the message waits.
                protocol::ws::ComposeResult::Rejected { reason } => {
                    let why = if reason == HELD_WORKING || reason == HELD_AFTER_INTERRUPT {
                        reason
                    } else {
                        crate::log_info!(
                            "agent message {} to {session_uid} waits: {reason}",
                            row.id
                        );
                        HELD_NOT_READY.to_string()
                    };
                    ("held", Some(why))
                }
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
}

/// This computer's tailnet, or `None` when Tailscale is missing, down or slow.
async fn read_tailnet() -> Option<Tailnet> {
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
