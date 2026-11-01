//! **CodeConnect Mail: messages between hosted agents.**
//!
//! An agent names another by its address, `<run uid>@<computer>`: the run of the
//! session it means and no other, and the computer that runs it, by its Tailscale
//! name ([`protocol::tailnet`]). It sends with the `codeconnect` tool's
//! `send_message` and reads its own with `read_messages`. A session's
//! `cc-N@<computer>` is its label, for reading only: another run, later, can carry
//! the same one. Only a run whose supervisor speaks [`MAIL_MINOR`] or later was
//! launched able to read mail; any other is neither listed nor sent to. The receiving computer's daemon keeps each
//! message (`mail` in [`crate::store`]) until that session reads it, the session
//! ends, or a day passes; the sending computer's daemon keeps what its agent sent
//! (`mail_sent`), written before the message goes anywhere, and what that agent has
//! been told of it.
//!
//! **Where a message stands** ([`MailState`]): `sent` once the sender's daemon has
//! it, `waiting` once the receiver's keeps it, `read` once the receiving session's
//! own `read_messages` answer reached it, and otherwise `refused`, `expired` or
//! `ended`. A sender reads the changes in its next `read_messages`, each once.
//!
//! **Waking the agent.** The daemon never hands an agent a message: it wakes the
//! agent with one fixed line ([`nudge`]), which carries nothing of any message,
//! and the agent calls `read_messages`. Claude is woken by its mail waiter
//! ([`crate::claude_waiter`]), only between its turns; Codex through its own queue
//! ([`crate::codex_wake`]). The wake rule ([`Daemon::wake_if_due`]) decides, from
//! the mailbox, whenever mail is taken, a turn ends, a waiter registers or a Codex
//! link adopts its thread, and when the daemon starts: wake iff unread mail waits
//! and no wake is outstanding. After [`WAKE_MISSES`] turn ends that leave woken mail
//! unread, wakes stop until the person's next prompt or new mail.
//!
//! **Who sent it** is never taken from the agent or the message: here it is the run
//! the tool was started for, and from another computer it is a session of the
//! computer the connection came from, by that computer's own account.
//!
//! **Across computers.** A message to `cc-N@<another computer>` is carried by this
//! daemon to that computer's, over its WebSocket listener on the tailnet, as one
//! [`protocol::peer`] request, under the id it was given here; until that computer
//! takes it, it is offered again every sweep with the same id, and the receiver
//! keeps one copy. The receiver tells the sender's daemon each change in where the
//! message stands. Only the same owner's untagged computers are served, read from
//! the receiver's own `tailscale status`, never from the request.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};

use protocol::agent::AgentKind;
use protocol::ipc::{AgentEntry, MailState, MailStatus, MAIL_TEXT_BYTES};
use protocol::peer::{PeerReply, PeerRequest};
use protocol::tailnet::{Place, Refusal, Tailnet};

use crate::state::Daemon;
use crate::store::{MailIntake, MailLimits, MailOffer, MailTaken, SentMail};

/// The bounds on mail: an unread message is kept a day, one agent may leave
/// another thirty messages an hour, and a settled message is dropped after a week.
pub(crate) const LIMITS: MailLimits = MailLimits {
    unread_ms: 24 * 60 * 60 * 1000,
    per_pair_per_hour: 30,
    settled_ms: 7 * 24 * 60 * 60 * 1000,
};

/// The protocol minor whose supervisors launch their agents with the
/// `codeconnect` tool and its mail.
const MAIL_MINOR: u32 = 27;

/// How often unread mail is expired, messages no other computer has taken yet are
/// offered again, and senders on other computers are told what changed.
const SWEEP_EVERY: Duration = Duration::from_secs(30);

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

/// The sending session, as the receiving computer names it.
struct Sender {
    /// Its computer's tailnet name, or empty for this computer.
    computer: String,
    uid: String,
    /// `<run uid>@<computer>`.
    address: String,
    /// `cc-N@<computer>`.
    label: String,
    agent: String,
    project: String,
}

impl Daemon {
    /// **Send one agent's message**, and say where it stands: taken by a session on
    /// this computer, or carried to the computer the address names.
    pub async fn send_agent_message(
        self: &Arc<Self>,
        from_uid: &str,
        to: &str,
        text: &str,
        reply_to: Option<&str>,
    ) -> MailStatus {
        let tailnet = read_tailnet().await;
        self.send_agent_message_on(tailnet.as_ref(), from_uid, to, text, reply_to)
            .await
    }

    /// [`Daemon::send_agent_message`] against a given reading of the tailnet.
    pub(crate) async fn send_agent_message_on(
        self: &Arc<Self>,
        tailnet: Option<&Tailnet>,
        from_uid: &str,
        to: &str,
        text: &str,
        reply_to: Option<&str>,
    ) -> MailStatus {
        let message_id = match protocol::uid::new() {
            Ok(id) => id,
            Err(err) => {
                crate::log_error!("agent message from {from_uid}: no id: {err:#}");
                return refused(String::new(), "CodeConnect could not name the message");
            }
        };
        let refuse = |reason: &str| refused(message_id.clone(), reason);

        let Some((_, computer)) = to
            .rsplit_once('@')
            .filter(|(run, c)| protocol::uid::is_well_formed(run) && !c.is_empty())
        else {
            return refuse(
                "address an agent by its address, <run id>@computer, as list_agents or a \
                 message you read gives it",
            );
        };
        if let Err(reason) = sendable(text) {
            return refuse(&reason);
        }
        let Some(tailnet) = tailnet else {
            return refuse(
                "this computer is not connected to Tailscale, so it has no name to send from",
            );
        };
        let place = match tailnet.locate(computer) {
            Ok(place) => place,
            Err(refusal) => return refuse(&refusal_sentence(refusal, computer)),
        };
        let sender = match self.db.get_session(from_uid.to_string()).await {
            Ok(Some(row)) => row,
            Ok(None) => return refuse("the sending session is not known to CodeConnect"),
            Err(err) => {
                crate::log_error!("agent message from {from_uid}: session lookup failed: {err:#}");
                return refuse("CodeConnect could not read its sessions");
            }
        };
        // A reply goes in the conversation of the message it answers, which must be
        // one this session was sent.
        let context_id = match reply_to {
            None => protocol::uid::new().ok(),
            Some(original) => match self
                .db
                .mail_context(from_uid.to_string(), original.to_string())
                .await
            {
                Ok(Some(context)) => Some(context),
                Ok(None) => {
                    return refuse(&format!(
                        "reply_to {original} is not a message you were sent"
                    ))
                }
                Err(err) => {
                    crate::log_error!(
                        "agent message from {from_uid}: reply lookup failed: {err:#}"
                    );
                    return refuse("CodeConnect could not read its mail");
                }
            },
        };
        let Some(context_id) = context_id else {
            return refuse("CodeConnect could not name the conversation");
        };
        let recipient_computer = match &place {
            Place::Here => String::new(),
            Place::Peer { name, .. } => name.to_string(),
        };
        let sent = SentMail {
            message_id: message_id.clone(),
            context_id,
            in_reply_to: reply_to.map(str::to_string),
            session_uid: from_uid.to_string(),
            recipient: to.to_string(),
            recipient_computer,
            text: text.to_string(),
        };
        if let Err(err) = self
            .db
            .file_sent_mail(sent.clone(), protocol::time::now_unix_ms())
            .await
        {
            crate::log_error!("agent message from {from_uid}: could not keep it: {err:#}");
            return refuse("CodeConnect could not keep the message");
        }

        let (state, reason) = self.dispatch(tailnet, &sent, &sender).await;
        if let Err(err) = self
            .db
            .note_sent_mail(message_id.clone(), state, reason.clone(), true)
            .await
        {
            crate::log_error!("agent message {message_id}: could not record {state:?}: {err:#}");
        }
        MailStatus {
            message_id,
            state,
            reason,
        }
    }

    /// **Hand one message from the outbox to where it is going**: to the receiving
    /// run when it is on this computer, and otherwise offered to the computer it is
    /// for. The one way a message leaves the outbox, at its send and at every sweep
    /// after, so one written down before the daemon stopped still goes; the receiver
    /// keeps one copy however many times it is handed over.
    async fn dispatch(
        self: &Arc<Self>,
        tailnet: &Tailnet,
        mail: &SentMail,
        sender: &crate::store::SessionRow,
    ) -> (MailState, Option<String>) {
        let Some(run) = mail.recipient.rsplit_once('@').map(|(run, _)| run) else {
            return (MailState::Refused, Some("the address names no run".into()));
        };
        if !mail.recipient_computer.is_empty() {
            return match tailnet.locate(&mail.recipient_computer) {
                Ok(Place::Peer { name: peer, ip }) => self.carry(mail, sender, run, peer, ip).await,
                // Not reachable this time: still sent, and offered again at the next sweep.
                Ok(Place::Here) | Err(_) => (MailState::Sent, None),
            };
        }
        let from = Sender {
            computer: String::new(),
            uid: mail.session_uid.clone(),
            address: format!("{}@{}", mail.session_uid, tailnet.name()),
            label: format!("{}@{}", sender.session_id, tailnet.name()),
            agent: agent_name(&sender.agent).to_string(),
            project: crate::project_label::project_label(&sender.cwd),
        };
        let taken = self
            .take(
                &from,
                run,
                &mail.message_id,
                &mail.context_id,
                mail.in_reply_to.as_deref(),
                &mail.text,
            )
            .await;
        if taken.0 == MailState::Waiting {
            waiter_registered(self, run);
        }
        taken
    }

    /// **Take one message for the run `run` on this computer**: the same for a
    /// sender here and for one on another computer. Only that run, running now and
    /// launched with mail, takes it; never another run under the same label.
    async fn take(
        &self,
        from: &Sender,
        run: &str,
        message_id: &str,
        context_id: &str,
        in_reply_to: Option<&str>,
        text: &str,
    ) -> (MailState, Option<String>) {
        let refuse = |reason: String| (MailState::Refused, Some(reason));
        if let Err(reason) = sendable(text) {
            return refuse(reason);
        }
        let runs = match self.db.mail_runs().await {
            Ok(runs) => runs,
            Err(err) => {
                crate::log_error!("agent message to {run}: session lookup failed: {err:#}");
                return refuse("CodeConnect could not read its sessions".into());
            }
        };
        let Some((target, minor)) = runs.into_iter().find(|(row, _)| row.session_uid == run) else {
            return refuse(format!(
                "{run} is not a running session: it has ended, or never ran here"
            ));
        };
        let label = display(&target.session_id);
        // As `list_agents` leaves the asker out.
        if from.computer.is_empty() && target.session_uid == from.uid {
            return refuse("that is this session's own address".into());
        }
        if minor < MAIL_MINOR {
            return refuse(format!(
                "{label} was started before this computer's CodeConnect could give it messages; \
                 it can be messaged once it is restarted"
            ));
        }
        if let AgentKind::Unsupported(agent) = &target.agent {
            return refuse(format!(
                "{label} runs {}, which takes no messages",
                display(agent)
            ));
        }
        let intake = MailIntake {
            message_id: message_id.to_string(),
            context_id: context_id.to_string(),
            in_reply_to: in_reply_to.map(str::to_string),
            session_uid: target.session_uid,
            sender_computer: from.computer.clone(),
            sender_uid: from.uid.clone(),
            sender: from.address.clone(),
            sender_label: from.label.clone(),
            sender_agent: from.agent.clone(),
            sender_project: from.project.clone(),
            text: text.to_string(),
        };
        match self
            .db
            .take_mail(intake, protocol::time::now_unix_ms(), LIMITS)
            .await
        {
            Ok(MailTaken::Kept) => (MailState::Waiting, None),
            Ok(MailTaken::Known(state)) => (state, None),
            Ok(MailTaken::RateLimited) => refuse(format!(
                "rate limit: {} has left {label} {} messages in the last hour, the most \
                 CodeConnect takes from one agent for another; send again later",
                display(&from.label),
                LIMITS.per_pair_per_hour
            )),
            Ok(MailTaken::NoSession) => refuse(format!("{label} is not a running session")),
            Err(err) => {
                crate::log_error!("agent message to {run}: could not keep it: {err:#}");
                refuse("CodeConnect could not keep the message".into())
            }
        }
    }

    /// **Offer one message to the computer it is for**, once, and say where it
    /// stands by that computer's answer. One it could not be offered to is still
    /// `sent`, and the next sweep offers it again under the same id.
    async fn carry(
        &self,
        mail: &SentMail,
        sender: &crate::store::SessionRow,
        run: &str,
        peer: &str,
        ip: IpAddr,
    ) -> (MailState, Option<String>) {
        let request = PeerRequest::PeerMail {
            message_id: mail.message_id.clone(),
            context_id: mail.context_id.clone(),
            in_reply_to: mail.in_reply_to.clone(),
            from_uid: mail.session_uid.clone(),
            from: sender.session_id.clone(),
            agent: agent_name(&sender.agent).to_string(),
            project: crate::project_label::project_label(&sender.cwd),
            to: run.to_string(),
            text: mail.text.clone(),
        };
        let not_yet = |why: String| {
            (
                MailState::Sent,
                Some(format!(
                    "{peer} has not taken it yet ({why}); CodeConnect offers it again until it does \
                     or a day passes"
                )),
            )
        };
        match dial_peer(
            SocketAddr::new(ip, self.config.ws_port),
            &request,
            PEER_MESSAGE_WAIT,
        )
        .await
        {
            Ok(PeerReply::PeerMailResult { state, reason }) => (state, reason),
            Ok(PeerReply::PeerRefused { reason }) => (
                MailState::Refused,
                Some(format!("{peer} refused it: {reason}")),
            ),
            Ok(_) => not_yet("it answered with something other than this message's state".into()),
            Err(Dial::Older) => (
                MailState::Refused,
                Some(format!(
                    "{peer} runs an older CodeConnect, which takes no messages from agents"
                )),
            ),
            Err(Dial::Unreachable(why)) | Err(Dial::NoAnswer(why)) => not_yet(why),
        }
    }

    /// **Offer `from_uid` its unread messages** and the changes in where its own
    /// sent messages stand, under a new offer id. Nothing is read until
    /// [`Daemon::read_offered_mail`] names the offer.
    pub async fn offer_agent_messages(
        &self,
        from_uid: &str,
    ) -> Result<(String, MailOffer), String> {
        let offer = protocol::uid::new().map_err(|err| {
            crate::log_error!("read_messages for {from_uid}: no offer id: {err:#}");
            "CodeConnect could not read its mail".to_string()
        })?;
        let batch = self
            .db
            .offer_mail(
                from_uid.to_string(),
                offer.clone(),
                protocol::time::now_unix_ms(),
            )
            .await
            .map_err(|err| {
                crate::log_error!("read_messages for {from_uid}: {err:#}");
                "CodeConnect could not read its mail".to_string()
            })?;
        Ok((offer, batch))
    }

    /// The offer reached its agent: its messages are read, and senders on other
    /// computers are told.
    pub async fn read_offered_mail(self: &Arc<Self>, offer: &str) {
        match self.db.read_mail(offer.to_string()).await {
            Ok(0) => {}
            Ok(_) => self.tell_senders_soon(),
            Err(err) => crate::log_error!("mail offer {offer}: could not mark it read: {err:#}"),
        }
    }

    /// The offer did not reach its agent: everything in it is unread again.
    pub async fn release_offered_mail(&self, offer: &str) {
        if let Err(err) = self.db.release_mail(offer.to_string()).await {
            crate::log_error!("mail offer {offer}: could not release it: {err:#}");
        }
    }

    /// A run ended: its unread messages are ended. A sender on another computer is
    /// told at the next sweep.
    pub(crate) async fn end_mail(&self, session_uid: &str) {
        if let Err(err) = self.db.end_mail(session_uid.to_string()).await {
            crate::log_error!("mail for {session_uid}: could not end it: {err:#}");
        }
    }

    /// Tell senders on other computers what changed, now rather than at the next sweep.
    fn tell_senders_soon(self: &Arc<Self>) {
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            if let Some(tailnet) = read_tailnet().await {
                daemon.tell_senders(&tailnet).await;
            }
        });
    }

    /// **One sweep**: settle what time and ended runs decide, offer again what no
    /// other computer has taken, and tell senders on other computers what changed.
    pub(crate) async fn sweep_mail_on(self: &Arc<Self>, tailnet: Option<&Tailnet>) {
        if let Err(err) = self
            .db
            .sweep_mail(protocol::time::now_unix_ms(), LIMITS)
            .await
        {
            crate::log_error!("mail sweep: {err:#}");
        }
        let Some(tailnet) = tailnet else {
            return;
        };
        match self.db.unsent_mail().await {
            Ok(unsent) => {
                for mail in unsent {
                    self.carry_again(tailnet, &mail).await;
                }
            }
            Err(err) => crate::log_error!("mail sweep: could not read the outbox: {err:#}"),
        }
        self.tell_senders(tailnet).await;
    }

    /// Hand one `sent` message over again, and keep where it stands now.
    async fn carry_again(self: &Arc<Self>, tailnet: &Tailnet, mail: &SentMail) {
        let sender = match self.db.get_session(mail.session_uid.clone()).await {
            Ok(Some(row)) => row,
            _ => return,
        };
        let (state, reason) = self.dispatch(tailnet, mail, &sender).await;
        if state != MailState::Sent {
            if let Err(err) = self
                .db
                .note_sent_mail(mail.message_id.clone(), state, reason, false)
                .await
            {
                crate::log_error!(
                    "agent message {}: could not record {state:?}: {err:#}",
                    mail.message_id
                );
            }
        }
    }

    /// Tell each sender on another computer where its messages here now stand.
    async fn tell_senders(&self, tailnet: &Tailnet) {
        let untold = match self.db.untold_mail().await {
            Ok(untold) => untold,
            Err(err) => {
                crate::log_error!("mail: could not read the untold changes: {err:#}");
                return;
            }
        };
        for change in untold {
            let Ok(Place::Peer { ip, .. }) = tailnet.locate(&change.sender_computer) else {
                continue;
            };
            let request = PeerRequest::PeerMailState {
                message_id: change.message_id.clone(),
                state: change.state,
            };
            let answer = dial_peer(
                SocketAddr::new(ip, self.config.ws_port),
                &request,
                PEER_MESSAGE_WAIT,
            )
            .await;
            if matches!(answer, Ok(PeerReply::PeerMailStateTaken)) {
                if let Err(err) = self.db.told_mail(change.message_id, change.state).await {
                    crate::log_error!("mail: could not record a told change: {err:#}");
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
        let computer = match tailnet.sender(source) {
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
            PeerRequest::PeerMail {
                message_id,
                context_id,
                in_reply_to,
                from_uid,
                from,
                agent,
                project,
                to,
                text,
            } => {
                // The ids are the sender's daemon's own; its labels are only shown.
                if ![&from_uid, &message_id, &context_id]
                    .iter()
                    .all(|id| protocol::uid::is_well_formed(id))
                {
                    return refuse("the message's ids are not ones this computer reads");
                }
                let sender = Sender {
                    address: format!("{from_uid}@{computer}"),
                    label: format!("{from}@{computer}"),
                    computer,
                    uid: from_uid,
                    agent,
                    project,
                };
                let (state, reason) = self
                    .take(
                        &sender,
                        &to,
                        &message_id,
                        &context_id,
                        in_reply_to.as_deref(),
                        &text,
                    )
                    .await;
                if state == MailState::Waiting {
                    waiter_registered(self, &to);
                }
                PeerReply::PeerMailResult { state, reason }
            }
            PeerRequest::PeerMailState { message_id, state } => {
                // Only the computer the message went to says where it stands.
                match self.db.sent_mail_computer(message_id.clone()).await {
                    Ok(Some(went_to)) if went_to == computer => {}
                    Ok(_) => return refuse("this computer sent you no such message"),
                    Err(_) => return refuse("the receiving computer could not read its mail"),
                }
                match self.db.note_sent_mail(message_id, state, None, false).await {
                    Ok(()) => PeerReply::PeerMailStateTaken,
                    Err(_) => refuse("the receiving computer could not record it"),
                }
            }
        }
    }

    /// **The agents `from_uid` can message**: the other running sessions on this
    /// computer and on the owner's other computers, one per name, after the
    /// asker's own address. `Err` says why there is no list.
    pub async fn list_agents(
        &self,
        from_uid: &str,
    ) -> Result<(AgentEntry, Vec<AgentEntry>, Vec<String>), String> {
        let tailnet = read_tailnet().await;
        let (peers, unreachable) = match &tailnet {
            Some(tailnet) => self.ask_peers(tailnet).await,
            None => (Vec::new(), Vec::new()),
        };
        let (you, agents) = self
            .list_agents_on(tailnet.as_ref(), from_uid, peers)
            .await?;
        Ok((you, agents, unreachable))
    }

    /// The agents on each of this computer's reachable peers, and the computers that
    /// did not list theirs, each with why.
    pub(crate) async fn ask_peers(&self, tailnet: &Tailnet) -> (Vec<AgentEntry>, Vec<String>) {
        let asks = tailnet.reachable_peers().filter_map(|place| match place {
            Place::Peer { name, ip } => Some(async move {
                let reply = dial_peer(
                    SocketAddr::new(ip, self.config.ws_port),
                    &PeerRequest::PeerListAgents,
                    PEER_LIST_WAIT,
                )
                .await;
                (name.to_string(), reply)
            }),
            Place::Here => None,
        });
        let (mut agents, mut unreachable) = (Vec::new(), Vec::new());
        for (name, reply) in futures_util::future::join_all(asks).await {
            match reply {
                Ok(PeerReply::PeerAgents { agents: theirs }) => agents.extend(theirs),
                Ok(PeerReply::PeerRefused { reason }) => {
                    unreachable.push(format!("{name} ({reason})"))
                }
                Ok(_) => unreachable.push(format!("{name} (an answer this build does not read)")),
                Err(Dial::Older) => unreachable.push(format!("{name} (an older CodeConnect)")),
                Err(Dial::Unreachable(why) | Dial::NoAnswer(why)) => {
                    unreachable.push(format!("{name} ({why})"))
                }
            }
        }
        (agents, unreachable)
    }

    /// [`Daemon::list_agents`] against a given reading of the tailnet, and the
    /// agents its other computers answered with.
    pub(crate) async fn list_agents_on(
        &self,
        tailnet: Option<&Tailnet>,
        from_uid: &str,
        peers: Vec<AgentEntry>,
    ) -> Result<(AgentEntry, Vec<AgentEntry>), String> {
        let Some(tailnet) = tailnet else {
            return Err(
                "this computer is not connected to Tailscale, so it has no name to send from"
                    .into(),
            );
        };
        // As for a send: a computer that takes part in no messages lists no one.
        if let Err(refusal) = tailnet.locate(tailnet.name()) {
            return Err(refusal_sentence(refusal, tailnet.name()));
        }
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
            Some(row) => entry(&row, tailnet.name()),
            None => return Err("the asking session is not known to CodeConnect".into()),
        };
        let mut agents = self
            .local_agents(tailnet.name(), Some(from_uid))
            .await
            .map_err(unreadable)?;
        agents.extend(peers);
        Ok((you, agents))
    }

    /// This computer's runs that have not ended and were launched with mail, each
    /// run its own entry, leaving out `except_uid`.
    async fn local_agents(
        &self,
        this_computer: &str,
        except_uid: Option<&str>,
    ) -> anyhow::Result<Vec<AgentEntry>> {
        Ok(self
            .db
            .mail_runs()
            .await?
            .into_iter()
            .filter(|(row, minor)| {
                *minor >= MAIL_MINOR
                    && Some(row.session_uid.as_str()) != except_uid
                    && !matches!(row.agent, AgentKind::Unsupported(_))
            })
            .map(|(row, _)| entry(&row, this_computer))
            .collect())
    }
}

/// **The line that wakes an agent for its mail**, by this computer's mail switch,
/// read afresh for every wake: on, to read and handle each message as its sender
/// asks; off, or never answered, to read and show them to the person.
pub(crate) fn nudge() -> &'static str {
    protocol::ipc::mail_nudge(protocol::ipc::mail_acts(&protocol::root_dir()))
}

/// A turn may end this many times with woken mail still unread before wakes stop
/// until a person's turn or new mail.
const WAKE_MISSES: u32 = 3;

/// **A turn of `run_uid` ended**: Claude's `Stop`, Codex's `turn/completed`. The
/// wake it answered is no longer outstanding, and the rule decides again.
pub(crate) fn turn_ended(daemon: &Arc<Daemon>, run_uid: &str) {
    let (daemon, run_uid) = (Arc::clone(daemon), run_uid.to_string());
    tokio::spawn(async move {
        if let Err(err) = daemon
            .db
            .mail_turn_ended(run_uid.clone(), protocol::time::now_unix_ms(), WAKE_MISSES)
            .await
        {
            crate::log_error!("mail for {run_uid}: could not record the turn's end: {err:#}");
        }
        daemon.wake_if_due(&run_uid).await;
    });
}

/// **Decide again whether to wake `run_uid`**: its Claude waiter registered, its
/// Codex link adopted a thread, or mail was taken for it.
pub(crate) fn waiter_registered(daemon: &Arc<Daemon>, run_uid: &str) {
    let (daemon, run_uid) = (Arc::clone(daemon), run_uid.to_string());
    tokio::spawn(async move { daemon.wake_if_due(&run_uid).await });
}

/// **A Claude waiter of `run_uid` registered.** One whose own start showed no turn
/// running (`idle_since_ms`: a `Stop`, or a session start that was not a compaction)
/// shows that a turn prompted before then has ended, its end seen or not; then the
/// rule decides again.
pub(crate) fn claude_waiter_registered(
    daemon: &Arc<Daemon>,
    run_uid: &str,
    idle_since_ms: Option<i64>,
) {
    let (daemon, run_uid) = (Arc::clone(daemon), run_uid.to_string());
    tokio::spawn(async move {
        if let Some(since_ms) = idle_since_ms {
            if let Err(err) = daemon.db.mail_idle_since(run_uid.clone(), since_ms).await {
                crate::log_error!("mail for {run_uid}: could not record the turn's end: {err:#}");
            }
        }
        daemon.wake_if_due(&run_uid).await;
    });
}

/// **A prompt started a turn of `run_uid`**: the fixed nudge when it is a person's
/// turn and unread mail waits, for that turn to read it.
pub(crate) async fn prompt_submitted(daemon: &Arc<Daemon>, run_uid: &str) -> Option<&'static str> {
    match daemon
        .db
        .mail_prompt(run_uid.to_string(), protocol::time::now_unix_ms())
        .await
    {
        Ok(true) => Some(nudge()),
        Ok(false) => None,
        Err(err) => {
            crate::log_error!("mail for {run_uid}: could not record the prompt: {err:#}");
            None
        }
    }
}

impl Daemon {
    /// **The wake rule.** Wake `run_uid` when unread mail waits, no wake is
    /// outstanding, wakes have not stopped, and, for Claude, no turn of it runs:
    /// decided in one store transaction, then issued. A wake that cannot be issued
    /// is released, and the next trigger decides again; nothing else ever reaches
    /// the agent. The wake carries nothing of any message.
    pub(crate) async fn wake_if_due(self: &Arc<Self>, run_uid: &str) {
        let claude = match self.db.get_session(run_uid.to_string()).await {
            Ok(Some(row)) => match row.agent {
                AgentKind::Claude => true,
                AgentKind::Codex => false,
                AgentKind::Unsupported(_) => return,
            },
            Ok(None) => return,
            Err(err) => {
                crate::log_error!("mail for {run_uid}: session lookup failed: {err:#}");
                return;
            }
        };
        match self
            .db
            .claim_mail_wake(run_uid.to_string(), protocol::time::now_unix_ms(), claude)
            .await
        {
            Ok(true) => {}
            Ok(false) => return,
            Err(err) => {
                crate::log_error!("mail for {run_uid}: wake decision failed: {err:#}");
                return;
            }
        }
        let issued = if claude {
            crate::claude_waiter::wake_claude(self, run_uid)
        } else {
            match crate::codex_wake::wake_codex(self, run_uid).await {
                Ok(()) => true,
                Err(err) => {
                    crate::log_warn!("mail for {run_uid}: Codex was not woken: {err:#}");
                    false
                }
            }
        };
        if !issued {
            if let Err(err) = self.db.release_mail_wake(run_uid.to_string()).await {
                crate::log_error!("mail for {run_uid}: could not release the wake: {err:#}");
            }
        }
    }

    /// Every run with unread mail, as the daemon starts.
    async fn wake_all_due(self: &Arc<Self>) {
        match self
            .db
            .runs_with_unread_mail(protocol::time::now_unix_ms())
            .await
        {
            Ok(runs) => {
                for run in runs {
                    self.wake_if_due(&run).await;
                }
            }
            Err(err) => {
                crate::log_error!("mail: could not read the runs with unread mail: {err:#}")
            }
        }
    }
}

/// A run as `list_agents` gives it.
fn entry(row: &crate::store::SessionRow, this_computer: &str) -> AgentEntry {
    AgentEntry {
        address: format!("{}@{this_computer}", row.session_uid),
        label: format!("{}@{this_computer}", row.session_id),
        agent: agent_name(&row.agent).to_string(),
        project: crate::project_label::project_label(&row.cwd),
    }
}

/// A label as it is shown in a sentence: any control character, a newline
/// included, is a space, so a label is never more than one line of what it is
/// set in.
fn display(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Whether a message's text may be sent at all: not empty, no longer than
/// [`MAIL_TEXT_BYTES`], and no control character but newlines and tabs.
fn sendable(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("the message is empty".into());
    }
    if text.len() > MAIL_TEXT_BYTES {
        return Err(format!(
            "the message is too long ({} bytes; at most {MAIL_TEXT_BYTES})",
            text.len()
        ));
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(
            "the message holds a control character; only newlines and tabs may be sent".into(),
        );
    }
    Ok(())
}

/// A refusal under `message_id`.
fn refused(message_id: String, reason: &str) -> MailStatus {
    MailStatus {
        message_id,
        state: MailState::Refused,
        reason: Some(reason.to_string()),
    }
}

/// **Wake every run with unread mail, then sweep the mail every [`SWEEP_EVERY`]**,
/// for as long as the daemon runs.
pub async fn run_mail_sweeps(daemon: Arc<Daemon>) {
    daemon.wake_all_due().await;
    let mut ticker = tokio::time::interval(SWEEP_EVERY);
    loop {
        ticker.tick().await;
        let tailnet = read_tailnet().await;
        daemon.sweep_mail_on(tailnet.as_ref()).await;
    }
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
        let delivered = PeerReply::PeerMailResult {
            state: MailState::Waiting,
            reason: None,
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
