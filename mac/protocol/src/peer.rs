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

use crate::ipc::{AgentEntry, AgentMessageOutcome};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerRequest {
    /// The agents running on the receiving computer.
    PeerListAgents,
    /// A message from one of the asking computer's agents to one of the
    /// receiver's. The sender's computer is the receiver's own reading of where
    /// the connection came from; `from`, `agent` and `project` say which of its
    /// agents is writing.
    PeerAgentMessage {
        /// The sending session's name, `cc-N`.
        from: String,
        /// `Claude` or `Codex`.
        agent: String,
        /// The sending session's project.
        project: String,
        /// The receiving session's name, `cc-N`.
        to: String,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerReply {
    PeerAgents {
        agents: Vec<AgentEntry>,
    },
    PeerAgentMessageResult {
        outcome: AgentMessageOutcome,
    },
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
        let request = serde_json::to_string(&PeerRequest::PeerAgentMessage {
            from: "cc-2".into(),
            agent: "Codex".into(),
            project: "api".into(),
            to: "cc-1".into(),
            text: "hi".into(),
        })
        .unwrap();
        assert!(
            request.contains(r#""type":"peer_agent_message""#),
            "{request}"
        );
        assert!(serde_json::from_str::<crate::ws::ClientMessage>(&request).is_err());
        assert!(
            serde_json::from_str::<crate::ws::ClientMessage>(r#"{"type":"peer_list_agents"}"#)
                .is_err()
        );
        let hello = r#"{"type":"hello","protocol_version":1,"token":"t"}"#;
        assert!(serde_json::from_str::<crate::ws::ClientMessage>(hello).is_ok());
        assert!(serde_json::from_str::<PeerRequest>(hello).is_err());
        assert_eq!(
            serde_json::from_str::<PeerRequest>(&request).unwrap(),
            PeerRequest::PeerAgentMessage {
                from: "cc-2".into(),
                agent: "Codex".into(),
                project: "api".into(),
                to: "cc-1".into(),
                text: "hi".into(),
            }
        );
    }
}
