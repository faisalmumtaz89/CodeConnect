//! **What one computer's daemon asks another's**, for agents messaging each other
//! across computers (minor 27).
//!
//! A peer connects to the other daemon's WebSocket listener, sends one request,
//! reads one reply, and closes. The receiver tries a frame as one of these only
//! when it is not a phone message and no phone hello has been accepted on the
//! connection, so nothing a phone sends or reads changes. An older daemon answers the
//! frame as it answers any undecodable message, `bad_request`, which the sender
//! reads as "that computer runs an older CodeConnect".
//!
//! Who is asking is never taken from the request: the receiver looks the
//! connection's source address up in its own `tailscale status` and serves only
//! the same owner's untagged computers ([`crate::tailnet::Tailnet::sender`]).

use serde::{Deserialize, Serialize};

use crate::ipc::{AgentEntry, MailState};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerRequest {
    /// The agents running on the receiving computer.
    PeerListAgents,
    /// A message from one of the asking computer's agents to one of the
    /// receiver's. The sender's computer is the receiver's own reading of where
    /// the connection came from; `from_uid`, `from`, `agent` and `project` say which
    /// of its agents is writing. Sent again with the same `message_id` until the
    /// receiver answers, which keeps one copy whatever the number of tries.
    PeerMail {
        message_id: String,
        context_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_reply_to: Option<String>,
        /// The sending run.
        from_uid: String,
        /// The sending session's name, `cc-N`, for its label.
        from: String,
        /// `Claude` or `Codex`.
        agent: String,
        /// The sending session's project.
        project: String,
        /// The receiving run.
        to: String,
        text: String,
    },
    /// Where a message the asking computer holds for one of its sessions now
    /// stands, for the receiver, which sent it. Taken only from the computer the
    /// message was sent to.
    PeerMailState {
        message_id: String,
        state: MailState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerReply {
    PeerAgents {
        agents: Vec<AgentEntry>,
    },
    /// Where a [`PeerRequest::PeerMail`] stands now on the receiving computer.
    PeerMailResult {
        state: MailState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A [`PeerRequest::PeerMailState`] was taken.
    PeerMailStateTaken,
    /// The receiver does not serve this connection: why, in a sentence for the
    /// sending agent.
    PeerRefused {
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer request is never a phone message, and a phone's hello is never a peer
    /// request: the receiver tries one only after the other has failed, and each
    /// must fail on the other's frames.
    #[test]
    fn peer_requests_and_phone_messages_never_decode_as_each_other() {
        let mail = PeerRequest::PeerMail {
            message_id: "m1".into(),
            context_id: "c1".into(),
            in_reply_to: None,
            from_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            from: "cc-2".into(),
            agent: "Codex".into(),
            project: "api".into(),
            to: "cc-1".into(),
            text: "hi".into(),
        };
        let request = serde_json::to_string(&mail).unwrap();
        assert!(request.contains(r#""type":"peer_mail""#), "{request}");
        assert!(serde_json::from_str::<crate::ws::ClientMessage>(&request).is_err());
        assert!(
            serde_json::from_str::<crate::ws::ClientMessage>(r#"{"type":"peer_list_agents"}"#)
                .is_err()
        );
        let hello = r#"{"type":"hello","protocol_version":1,"token":"t"}"#;
        assert!(serde_json::from_str::<crate::ws::ClientMessage>(hello).is_ok());
        assert!(serde_json::from_str::<PeerRequest>(hello).is_err());
        assert_eq!(serde_json::from_str::<PeerRequest>(&request).unwrap(), mail);
        let state = r#"{"type":"peer_mail_state","message_id":"m1","state":"read"}"#;
        assert!(serde_json::from_str::<crate::ws::ClientMessage>(state).is_err());
        assert_eq!(
            serde_json::from_str::<PeerRequest>(state).unwrap(),
            PeerRequest::PeerMailState {
                message_id: "m1".into(),
                state: MailState::Read,
            }
        );
    }
}
