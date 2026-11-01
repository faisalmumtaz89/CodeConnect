//! The computers an agent can message, read from `tailscale status --json`.
//!
//! A computer's agents take messages from the same Tailscale user's other
//! devices and from no one else. Both ends must be untagged: a tagged device is
//! owned by its tags rather than by a person, and every tagged device carries
//! the same tagged-devices user id, so comparing user ids alone would let any
//! tagged device message any other.
//!
//! A computer is named by the first label of its MagicDNS name (`studio` for
//! `studio.tail0000.ts.net.`), which is what an agent writes after the `@` in
//! `cc-2@studio`.

use std::collections::HashMap;
use std::net::IpAddr;

use serde::Deserialize;

/// One reading of the tailnet: this computer and its peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tailnet {
    me: Node,
    /// Sorted by name, so a listing reads the same on every call.
    peers: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    ips: Vec<IpAddr>,
    online: bool,
    user: i64,
    tagged: bool,
}

/// Why a computer may not message this one, or be messaged from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No device on the tailnet has this address or name.
    Unknown,
    /// Another Tailscale user's device, shared into this tailnet.
    OtherUser,
    /// The other device is tagged.
    Tagged,
    /// This computer is tagged, so it takes part in no messages.
    ThisComputerTagged,
    /// The device is on the tailnet but not connected now.
    Offline,
}

/// Where `cc-N@<computer>` lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place<'a> {
    /// This computer: the message is handed to its own daemon, with no dial.
    Here,
    /// Another computer, reached on the tailnet address its daemon listens on.
    Peer { name: &'a str, ip: IpAddr },
}

impl Tailnet {
    /// `None` unless the output is the status of a running backend.
    pub fn parse(status_json: &str) -> Option<Tailnet> {
        let status: Status = serde_json::from_str(status_json).ok()?;
        if status.backend_state != "Running" {
            return None;
        }
        let mut peers: Vec<Node> = status
            .peer
            .unwrap_or_default()
            .into_values()
            .map(Node::from)
            .collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        Some(Tailnet {
            me: Node::from(status.me),
            peers,
        })
    }

    /// This computer's name.
    pub fn name(&self) -> &str {
        &self.me.name
    }

    /// The computer a connection from `ip` came from, if it may message this
    /// one. An address no peer holds is refused, which covers loopback and so
    /// every connection a userspace-mode tailscaled hands on.
    pub fn sender(&self, ip: IpAddr) -> Result<&Node, Refusal> {
        let node = self
            .peers
            .iter()
            .find(|node| node.ips.contains(&ip))
            .ok_or(Refusal::Unknown)?;
        self.trust(node)?;
        Ok(node)
    }

    /// Where the computer named `computer` is, if this one may message it now. A
    /// tagged computer takes part in no messages, its own sessions' included.
    pub fn locate(&self, computer: &str) -> Result<Place<'_>, Refusal> {
        if self.me.tagged {
            return Err(Refusal::ThisComputerTagged);
        }
        if self.me.name.eq_ignore_ascii_case(computer) {
            return Ok(Place::Here);
        }
        let node = self
            .peers
            .iter()
            .find(|node| node.name.eq_ignore_ascii_case(computer))
            .ok_or(Refusal::Unknown)?;
        self.trust(node)?;
        if !node.online {
            return Err(Refusal::Offline);
        }
        node.place().ok_or(Refusal::Unknown)
    }

    /// Every other computer this one may message now, by name.
    pub fn reachable_peers(&self) -> impl Iterator<Item = Place<'_>> {
        self.peers
            .iter()
            .filter(|node| node.online && self.trust(node).is_ok())
            .filter_map(Node::place)
    }

    fn trust(&self, node: &Node) -> Result<(), Refusal> {
        if self.me.tagged {
            return Err(Refusal::ThisComputerTagged);
        }
        if node.tagged {
            return Err(Refusal::Tagged);
        }
        if node.user != self.me.user {
            return Err(Refusal::OtherUser);
        }
        Ok(())
    }
}

impl Node {
    /// The IPv4 address, because that is the one a daemon binds.
    fn place(&self) -> Option<Place<'_>> {
        let ip = self.ips.iter().copied().find(IpAddr::is_ipv4)?;
        Some(Place::Peer {
            name: &self.name,
            ip,
        })
    }
}

impl From<NodeStatus> for Node {
    fn from(status: NodeStatus) -> Node {
        Node {
            name: status
                .dns_name
                .split('.')
                .next()
                .unwrap_or_default()
                .to_string(),
            ips: status.tailscale_ips.unwrap_or_default(),
            online: status.online,
            user: status.user_id,
            tagged: status.tags.is_some_and(|tags| !tags.is_empty()),
        }
    }
}

/// The fields read from `tailscale status --json`. Tailscale writes an empty
/// list or map as `null` and leaves `Tags` out when there are none.
#[derive(Deserialize)]
struct Status {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "Self")]
    me: NodeStatus,
    #[serde(rename = "Peer")]
    peer: Option<HashMap<String, NodeStatus>>,
}

#[derive(Deserialize)]
struct NodeStatus {
    #[serde(rename = "DNSName")]
    dns_name: String,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<IpAddr>>,
    #[serde(rename = "UserID")]
    user_id: i64,
    #[serde(rename = "Tags", default)]
    tags: Option<Vec<String>>,
    #[serde(rename = "Online", default)]
    online: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real six-node tailnet (Tailscale 1.104.2): one user, nothing tagged or
    /// shared in, two peers offline. Names and addresses replaced.
    const STATUS: &str = include_str!("../../../fixtures/tailscale/status-six-nodes.json");

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn recorded() -> Tailnet {
        Tailnet::parse(STATUS).unwrap()
    }

    /// The recorded status with `edit` applied to its JSON.
    fn edited(edit: impl FnOnce(&mut serde_json::Value)) -> Tailnet {
        let mut status: serde_json::Value = serde_json::from_str(STATUS).unwrap();
        edit(&mut status);
        Tailnet::parse(&status.to_string()).unwrap()
    }

    fn peer<'a>(status: &'a mut serde_json::Value, name: &str) -> &'a mut serde_json::Value {
        status["Peer"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .find(|node| node["HostName"] == name)
            .unwrap()
    }

    #[test]
    fn the_recorded_status_reads_as_this_computer_and_five_peers() {
        let tailnet = recorded();
        assert_eq!(tailnet.name(), "studio");
        let names: Vec<&str> = tailnet.peers.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["desktop", "laptop", "linux-box", "phone", "tablet"]);
    }

    #[test]
    fn a_same_user_untagged_peer_may_send_from_either_of_its_addresses() {
        let tailnet = recorded();
        assert_eq!(tailnet.sender(ip("100.64.0.2")).unwrap().name, "desktop");
        assert_eq!(
            tailnet.sender(ip("fd7a:115c:a1e0::2")).unwrap().name,
            "desktop"
        );
        // Being offline in the status does not refuse a connection that arrived.
        assert_eq!(tailnet.sender(ip("100.64.0.3")).unwrap().name, "linux-box");
    }

    #[test]
    fn an_address_no_peer_holds_is_refused() {
        let tailnet = recorded();
        for source in [
            "127.0.0.1",
            "::1",
            "100.64.0.1",
            "100.64.0.99",
            "192.168.1.2",
        ] {
            assert_eq!(
                tailnet.sender(ip(source)),
                Err(Refusal::Unknown),
                "{source}"
            );
        }
    }

    #[test]
    fn another_users_device_is_refused_both_ways() {
        let tailnet = edited(|s| peer(s, "desktop")["UserID"] = 9999.into());
        assert_eq!(tailnet.sender(ip("100.64.0.2")), Err(Refusal::OtherUser));
        assert_eq!(tailnet.locate("desktop"), Err(Refusal::OtherUser));
    }

    #[test]
    fn a_tagged_peer_is_refused_both_ways_even_with_the_same_user_id() {
        let tailnet = edited(|s| peer(s, "desktop")["Tags"] = serde_json::json!(["tag:server"]));
        assert_eq!(tailnet.sender(ip("100.64.0.2")), Err(Refusal::Tagged));
        assert_eq!(tailnet.locate("desktop"), Err(Refusal::Tagged));
    }

    #[test]
    fn a_tagged_computer_takes_part_in_no_messages() {
        let tailnet = edited(|s| s["Self"]["Tags"] = serde_json::json!(["tag:server"]));
        assert_eq!(
            tailnet.sender(ip("100.64.0.2")),
            Err(Refusal::ThisComputerTagged)
        );
        assert_eq!(tailnet.locate("desktop"), Err(Refusal::ThisComputerTagged));
        assert_eq!(
            tailnet.locate("studio"),
            Err(Refusal::ThisComputerTagged),
            "nor between its own sessions"
        );
        assert_eq!(tailnet.reachable_peers().count(), 0);
    }

    #[test]
    fn a_name_is_located_here_on_a_peer_offline_or_nowhere() {
        let tailnet = recorded();
        assert_eq!(tailnet.locate("studio"), Ok(Place::Here));
        assert_eq!(tailnet.locate("Studio"), Ok(Place::Here));
        assert_eq!(
            tailnet.locate("desktop"),
            Ok(Place::Peer {
                name: "desktop",
                ip: ip("100.64.0.2")
            })
        );
        assert_eq!(tailnet.locate("linux-box"), Err(Refusal::Offline));
        assert_eq!(tailnet.locate("nowhere"), Err(Refusal::Unknown));
        assert_eq!(tailnet.locate(""), Err(Refusal::Unknown));
    }

    #[test]
    fn only_online_trusted_peers_are_reachable() {
        let tailnet = edited(|s| peer(s, "laptop")["UserID"] = 9999.into());
        let reachable: Vec<Place> = tailnet.reachable_peers().collect();
        assert_eq!(
            reachable,
            [
                Place::Peer {
                    name: "desktop",
                    ip: ip("100.64.0.2")
                },
                Place::Peer {
                    name: "phone",
                    ip: ip("100.64.0.4")
                },
            ]
        );
    }

    #[test]
    fn a_stopped_backend_or_garbage_is_no_tailnet() {
        let mut status: serde_json::Value = serde_json::from_str(STATUS).unwrap();
        status["BackendState"] = "Stopped".into();
        assert_eq!(Tailnet::parse(&status.to_string()), None);
        assert_eq!(Tailnet::parse("tailscale: not running"), None);
    }

    #[test]
    fn a_tailnet_with_no_peers_still_names_this_computer() {
        let tailnet = edited(|s| s["Peer"] = serde_json::Value::Null);
        assert_eq!(tailnet.locate("studio"), Ok(Place::Here));
        assert_eq!(tailnet.locate("desktop"), Err(Refusal::Unknown));
    }
}
