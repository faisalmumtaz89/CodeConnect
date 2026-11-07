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

use crate::ws::{PeerComputer, ServerMessage, TailnetState};

/// One reading of the tailnet: this computer and its peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tailnet {
    me: Node,
    /// This computer's user's login, from the status's `User` map.
    login: Option<String>,
    /// Sorted by name, so a listing reads the same on every call.
    peers: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    /// The whole MagicDNS name without its trailing dot; empty when it has none.
    dns_name: String,
    ips: Vec<IpAddr>,
    online: bool,
    user: i64,
    tagged: bool,
    /// The operating system Tailscale reports (`macOS`, `linux`, `windows`, `iOS`, …).
    os: String,
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
    /// The device runs an operating system CodeConnect does not run on: a phone, a
    /// tablet, a Windows PC.
    NoCodeConnect,
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
        let login = status
            .user
            .and_then(|mut users| users.remove(&status.me.user_id.to_string()))
            .and_then(|user| user.login_name);
        Some(Tailnet {
            me: Node::from(status.me),
            login,
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
        if !node.runs_codeconnect() {
            return Err(Refusal::NoCodeConnect);
        }
        if !node.online {
            return Err(Refusal::Offline);
        }
        node.place().ok_or(Refusal::Unknown)
    }

    /// Every other computer this one may message now, by name.
    pub fn reachable_peers(&self) -> impl Iterator<Item = Place<'_>> {
        self.peers
            .iter()
            .filter(|node| self.reachable(node))
            .filter_map(Node::place)
    }

    fn reachable(&self, node: &Node) -> bool {
        node.online && node.runs_codeconnect() && self.trust(node).is_ok()
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

/// The [`ServerMessage::Peers`] frame for one reading of the tailnet: `None` when
/// Tailscale is not running, or not installed, which `installed` tells apart. The
/// computers are those [`Tailnet::reachable_peers`] names, and `port` is this
/// daemon's, which Mail also takes as every peer's.
///
/// Names come from `DNSName`, never `HostName`: an iPhone reports its `HostName`
/// as `localhost`.
pub fn peers_frame(tailnet: Option<&Tailnet>, installed: bool, port: u16) -> ServerMessage {
    let Some(tailnet) = tailnet else {
        return ServerMessage::Peers {
            tailnet: if installed {
                TailnetState::NotRunning
            } else {
                TailnetState::NotInstalled
            },
            login: None,
            tagged: false,
            computers: Vec::new(),
        };
    };
    let computers = tailnet
        .peers
        .iter()
        .filter(|node| tailnet.reachable(node))
        .filter_map(|node| {
            let ipv4 = node.ips.iter().copied().find(IpAddr::is_ipv4)?;
            Some(PeerComputer {
                name: node.name.clone(),
                host: if node.dns_name.is_empty() {
                    ipv4.to_string()
                } else {
                    node.dns_name.clone()
                },
                port,
            })
        })
        .collect();
    ServerMessage::Peers {
        tailnet: TailnetState::Running,
        login: tailnet.login.clone(),
        tagged: tailnet.me.tagged,
        computers,
    }
}

impl Node {
    /// Whether CodeConnect runs on this device's operating system.
    fn runs_codeconnect(&self) -> bool {
        matches!(self.os.as_str(), "macOS" | "linux")
    }

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
            dns_name: status.dns_name.trim_end_matches('.').to_string(),
            ips: status.tailscale_ips.unwrap_or_default(),
            online: status.online,
            user: status.user_id,
            tagged: status.tags.is_some_and(|tags| !tags.is_empty()),
            os: status.os,
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
    /// Keyed by the decimal user id.
    #[serde(rename = "User", default)]
    user: Option<HashMap<String, UserStatus>>,
}

#[derive(Deserialize)]
struct UserStatus {
    #[serde(rename = "LoginName", default)]
    login_name: Option<String>,
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
    #[serde(rename = "OS", default)]
    os: String,
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

    /// A device CodeConnect does not run on (the iPhone, the Windows PC) is never
    /// dialled, online or not.
    #[test]
    fn a_name_is_located_here_on_a_peer_offline_or_nowhere() {
        let tailnet = recorded();
        assert_eq!(tailnet.locate("studio"), Ok(Place::Here));
        assert_eq!(tailnet.locate("Studio"), Ok(Place::Here));
        assert_eq!(
            tailnet.locate("laptop"),
            Ok(Place::Peer {
                name: "laptop",
                ip: ip("100.64.0.6")
            })
        );
        for device in ["phone", "tablet", "desktop"] {
            assert_eq!(
                tailnet.locate(device),
                Err(Refusal::NoCodeConnect),
                "{device}"
            );
        }
        assert_eq!(tailnet.locate("linux-box"), Err(Refusal::Offline));
        assert_eq!(tailnet.locate("nowhere"), Err(Refusal::Unknown));
        assert_eq!(tailnet.locate(""), Err(Refusal::Unknown));
    }

    /// Reachable: online, trusted, and running an OS CodeConnect runs on (round 2,
    /// R2-3). The online Windows PC and iPhone are never contacted.
    #[test]
    fn only_online_trusted_computers_codeconnect_runs_on_are_reachable() {
        let tailnet = edited(|s| {
            peer(s, "linux-box")["Online"] = true.into();
            peer(s, "tablet")["Online"] = true.into();
        });
        let reachable: Vec<Place> = tailnet.reachable_peers().collect();
        assert_eq!(
            reachable,
            [
                Place::Peer {
                    name: "laptop",
                    ip: ip("100.64.0.6")
                },
                Place::Peer {
                    name: "linux-box",
                    ip: ip("100.64.0.3")
                },
            ]
        );
        let tailnet = edited(|s| peer(s, "laptop")["UserID"] = 9999.into());
        assert_eq!(tailnet.reachable_peers().count(), 0);
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

    fn computers(frame: ServerMessage) -> Vec<PeerComputer> {
        match frame {
            ServerMessage::Peers { computers, .. } => computers,
            other => panic!("not a peers frame: {other:?}"),
        }
    }

    /// The same computers Mail may reach, named by their whole MagicDNS name, or
    /// by their IPv4 address when they have none.
    #[test]
    fn the_peers_frame_lists_the_computers_mail_may_reach() {
        let tailnet = edited(|s| peer(s, "linux-box")["Online"] = true.into());
        let listed = computers(peers_frame(Some(&tailnet), true, 8787));
        assert_eq!(
            listed,
            [
                PeerComputer {
                    name: "laptop".into(),
                    host: "laptop.tail0000.ts.net".into(),
                    port: 8787
                },
                PeerComputer {
                    name: "linux-box".into(),
                    host: "linux-box.tail0000.ts.net".into(),
                    port: 8787
                },
            ]
        );
        let reachable: Vec<&str> = tailnet
            .reachable_peers()
            .map(|place| match place {
                Place::Peer { name, .. } => name,
                Place::Here => "here",
            })
            .collect();
        assert_eq!(reachable, ["laptop", "linux-box"]);

        let tailnet = edited(|s| peer(s, "laptop")["DNSName"] = "".into());
        assert_eq!(
            computers(peers_frame(Some(&tailnet), true, 8787))[0].host,
            "100.64.0.6"
        );
    }

    #[test]
    fn a_tagged_computer_lists_no_one_and_says_it_is_tagged() {
        let tailnet = edited(|s| s["Self"]["Tags"] = serde_json::json!(["tag:server"]));
        let ServerMessage::Peers {
            tagged, computers, ..
        } = peers_frame(Some(&tailnet), true, 8787)
        else {
            panic!("not a peers frame");
        };
        assert!(tagged);
        assert!(computers.is_empty());
    }

    #[test]
    fn a_status_without_this_users_login_still_reads() {
        let tailnet = edited(|s| s["User"] = serde_json::Value::Null);
        let ServerMessage::Peers { login, .. } = peers_frame(Some(&tailnet), true, 8787) else {
            panic!("not a peers frame");
        };
        assert_eq!(login, None);
    }

    /// The frame for the recorded tailnet, for one that is not running, and for a
    /// computer without Tailscale.
    fn derived_peers_wire() -> serde_json::Value {
        serde_json::json!({
            "running": peers_frame(Some(&recorded()), true, 8787),
            "not_running": peers_frame(None, true, 8787),
            "not_installed": peers_frame(None, false, 8787),
        })
    }

    /// **The `peers` frames, emitted by this build**, compared as JSON values. The
    /// file is the fixture for the apps' decoders of this frame; the copy under
    /// `ios/` is byte-identical, by `fixtures/check-duplicated-fixtures.sh`.
    #[test]
    fn the_peers_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/wire/peers.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_peers_wire(),
            committed,
            "the committed peers frames no longer match what this build emits. Regenerate \
             them with cargo test -p protocol -- --ignored regenerate_the_peers_wire_fixture, \
             then copy the file over its copy under ios/"
        );
    }

    /// Rewrite `fixtures/wire/peers.json` with what this build emits.
    #[test]
    #[ignore = "generator, not a gate"]
    fn regenerate_the_peers_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_peers_wire()).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/wire/peers.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }
}
