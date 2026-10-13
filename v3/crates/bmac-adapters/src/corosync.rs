// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Quorum and corosync links: `pvecm status`, `corosync-quorumtool -s`,
//! `corosync-cfgtool -n`, and the QDevice entry in `corosync.conf`.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use bmac_exec::{Command, CommandRunner};
use regex::Regex;
use serde::Serialize;

use crate::{AdapterError, programs, run_text};

static LOCAL_NODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Local node ID (\d+)").unwrap());
static PEER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^nodeid:?\s*(\d+)\s*:?\s*(reachable|unreachable)?").unwrap());
static LINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^LINK:?\s*(\d+)\b(.*)$").unwrap());
static DEVICE_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*device\s*\{").unwrap());
static DEVICE_HOST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*host:\s*(\S+)").unwrap());

/// One row of the membership table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberRow {
    /// Zero for the QDevice's own row.
    pub nodeid: u32,
    pub votes: u32,
    /// The Qdevice column (`A,V,NMW`), when the table has one.
    pub qdevice: Option<String>,
    /// The node's name or address, or `Qdevice`.
    pub name: String,
}

/// The votequorum report printed by `pvecm status` and
/// `corosync-quorumtool -s`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuorumStatus {
    /// `Key: value` lines, value trimmed.
    pub values: BTreeMap<String, String>,
    pub members: Vec<MemberRow>,
}

impl QuorumStatus {
    pub fn parse(text: &str) -> QuorumStatus {
        let mut values = BTreeMap::new();
        let mut members = Vec::new();
        let mut qdevice_column = false;
        for line in text.lines() {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.first() == Some(&"Nodeid") {
                qdevice_column = tokens.contains(&"Qdevice");
                continue;
            }
            if let Some(hex) = tokens.first().and_then(|t| t.strip_prefix("0x"))
                && let (Ok(nodeid), Some(Ok(votes))) = (
                    u32::from_str_radix(hex, 16),
                    tokens.get(1).map(|v| v.parse()),
                )
            {
                let rest = &tokens[2..];
                let (qdevice, name) = if qdevice_column && nodeid != 0 && rest.len() >= 2 {
                    (Some(rest[0].to_string()), rest[1..].join(" "))
                } else {
                    (None, rest.join(" "))
                };
                members.push(MemberRow {
                    nodeid,
                    votes,
                    qdevice,
                    name,
                });
                continue;
            }
            if let Some((key, value)) = line.split_once(':')
                && !key.starts_with(char::is_whitespace)
                && !key.is_empty()
            {
                values.insert(key.to_string(), value.trim().to_string());
            }
        }
        QuorumStatus { values, members }
    }

    /// The last word of a `Key: value` line, as v2's `pvecm_status_value`
    /// reads it.
    pub fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key)?.split_whitespace().last()
    }

    fn number(&self, key: &str) -> Option<u32> {
        self.value(key)?.parse().ok()
    }

    pub fn expected_votes(&self) -> Option<u32> {
        self.number("Expected votes")
    }

    pub fn total_votes(&self) -> Option<u32> {
        self.number("Total votes")
    }

    pub fn quorate(&self) -> bool {
        self.values.get("Quorate").map(String::as_str) == Some("Yes")
    }

    /// Whether `Flags:` lists `Qdevice`: the QDevice is alive and voting.
    pub fn has_qdevice_flag(&self) -> bool {
        self.values
            .get("Flags")
            .is_some_and(|flags| flags.split_whitespace().any(|f| f == "Qdevice"))
    }

    /// Every one of `node_count` members votes, the QDevice votes once, and
    /// the expected and total votes count both (v2 `qd_status_is_healthy`,
    /// `scripts/lib/qdevice.sh` 73–95).
    pub fn qdevice_healthy(&self, node_count: u32) -> bool {
        let with_qdevice = node_count + 1;
        self.expected_votes() == Some(with_qdevice)
            && self.total_votes() == Some(with_qdevice)
            && self.quorate()
            && self.has_qdevice_flag()
            && self
                .members
                .iter()
                .filter(|m| {
                    m.nodeid != 0
                        && m.votes == 1
                        && m.qdevice.as_deref().is_some_and(|q| q.starts_with("A,V,"))
                })
                .count()
                == node_count as usize
            && self
                .members
                .iter()
                .filter(|m| m.nodeid == 0 && m.votes == 1 && m.name == "Qdevice")
                .count()
                == 1
    }

    /// The votes count only the members and no QDevice is voting (v2
    /// `qd_status_absent_is_healthy`).
    pub fn qdevice_absent_healthy(&self, node_count: u32) -> bool {
        self.expected_votes() == Some(node_count)
            && self.total_votes() == Some(node_count)
            && self.quorate()
            && !self.has_qdevice_flag()
    }
}

/// One peer as `corosync-cfgtool -n` shows it from this node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerLinks {
    pub reachable: Option<bool>,
    /// Link number to whether it is connected.
    pub links: BTreeMap<u32, bool>,
}

impl PeerLinks {
    /// Unreachable, or every listed link down.
    pub fn down(&self) -> bool {
        self.reachable == Some(false)
            || (!self.links.is_empty() && !self.links.values().any(|up| *up))
    }
}

/// Parses `corosync-cfgtool -n`: every peer except this node, keyed by
/// node ID (v2 `show_cluster_health.sh`, `parse_links`).
pub fn parse_links(text: &str) -> BTreeMap<u32, PeerLinks> {
    let mut local = None;
    let mut peers: BTreeMap<u32, PeerLinks> = BTreeMap::new();
    let mut current = None;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(captures) = LOCAL_NODE_RE.captures(line) {
            local = captures[1].parse::<u32>().ok();
            continue;
        }
        if let Some(captures) = PEER_RE.captures(line)
            && let Ok(nodeid) = captures[1].parse::<u32>()
        {
            current = Some(nodeid);
            peers.insert(
                nodeid,
                PeerLinks {
                    reachable: captures.get(2).map(|m| m.as_str() == "reachable"),
                    links: BTreeMap::new(),
                },
            );
            continue;
        }
        if let Some(captures) = LINK_RE.captures(line)
            && let Some(peer) = current.and_then(|id| peers.get_mut(&id))
            && let Ok(link) = captures[1].parse::<u32>()
        {
            let rest = &captures[2];
            peer.links.insert(
                link,
                !rest.contains("disabled")
                    && !rest.contains("disconnected")
                    && rest.contains("connected"),
            );
        }
    }
    if let Some(local) = local {
        peers.remove(&local);
    }
    peers
}

/// The QDevice entry of `/etc/pve/corosync.conf`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QdeviceEntry {
    pub registered: bool,
    /// The `host:` of the `device` block.
    pub address: Option<String>,
}

pub fn parse_qdevice_entry(corosync_conf: &str) -> QdeviceEntry {
    match DEVICE_BLOCK_RE.find(corosync_conf) {
        None => QdeviceEntry {
            registered: false,
            address: None,
        },
        Some(block) => QdeviceEntry {
            registered: true,
            address: DEVICE_HOST_RE
                .captures(&corosync_conf[block.end()..])
                .map(|captures| captures[1].to_string()),
        },
    }
}

/// Every `nodelist { node { ... } }` block of `/etc/pve/corosync.conf`, as
/// its keys and values.
pub fn nodelist(corosync_conf: &str) -> Vec<BTreeMap<String, String>> {
    let mut nodes = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut node: BTreeMap<String, String> = BTreeMap::new();
    for line in corosync_conf.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if let Some(block) = line.strip_suffix('{') {
            path.push(block.trim().to_string());
            if path == ["nodelist", "node"] {
                node.clear();
            }
        } else if line == "}" {
            if path == ["nodelist", "node"] {
                nodes.push(std::mem::take(&mut node));
            }
            path.pop();
        } else if path == ["nodelist", "node"]
            && let Some((key, value)) = line.split_once(':')
        {
            node.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    nodes
}

/// The `ringN_addr` values of node `name` in `/etc/pve/corosync.conf` that
/// are IP addresses, in ring order. Host names are skipped.
pub fn node_addresses(corosync_conf: &str, name: &str) -> Vec<std::net::IpAddr> {
    let Some(node) = nodelist(corosync_conf)
        .into_iter()
        .find(|n| n.get("name").map(String::as_str) == Some(name))
    else {
        return Vec::new();
    };
    let mut rings: Vec<(u32, std::net::IpAddr)> = node
        .iter()
        .filter_map(|(key, value)| {
            let ring = key
                .strip_prefix("ring")?
                .strip_suffix("_addr")?
                .parse()
                .ok()?;
            Some((ring, value.parse().ok()?))
        })
        .collect();
    rings.sort();
    rings.into_iter().map(|(_, ip)| ip).collect()
}

/// Node ID to node name, from `/etc/pve/corosync.conf`.
pub fn node_names(corosync_conf: &str) -> BTreeMap<u32, String> {
    nodelist(corosync_conf)
        .into_iter()
        .filter_map(|n| Some((n.get("nodeid")?.parse().ok()?, n.get("name")?.clone())))
        .collect()
}

/// `pvecm` and the corosync tools.
pub struct Corosync<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> Corosync<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Corosync<'a> {
        Corosync { runner }
    }

    pub async fn pvecm_status(&self) -> Result<QuorumStatus, AdapterError> {
        let text = run_text(self.runner, Command::new(programs::PVECM).arg("status")).await?;
        Ok(QuorumStatus::parse(&text))
    }

    /// `corosync-quorumtool -s` exits 2 when the node is not quorate; the
    /// report is still read.
    pub async fn quorum(&self) -> Result<QuorumStatus, AdapterError> {
        let text = run_text(
            self.runner,
            Command::new(programs::COROSYNC_QUORUMTOOL)
                .arg("-s")
                .expect_codes(&[0, 2]),
        )
        .await?;
        Ok(QuorumStatus::parse(&text))
    }

    pub async fn links(&self) -> Result<BTreeMap<u32, PeerLinks>, AdapterError> {
        let text = run_text(
            self.runner,
            Command::new(programs::COROSYNC_CFGTOOL).arg("-n"),
        )
        .await?;
        Ok(parse_links(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_addresses_come_from_the_named_node() {
        let conf = "logging {\n  debug: off\n}\n\
            nodelist {\n\
              node {\n    name: mox1\n    nodeid: 1\n    ring1_addr: 100.101.1.1\n    ring0_addr: 10.10.0.1\n  }\n\
              node {\n    name: mox2\n    nodeid: 2\n    ring0_addr: 10.10.0.2 # private\n  }\n\
              node {\n    name: mox3\n    ring0_addr: mox3.lan\n  }\n\
            }\n\
            quorum {\n  provider: corosync_votequorum\n  device {\n    name: mox1\n    host: 100.64.0.5\n  }\n}\n";
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        assert_eq!(
            node_names(conf),
            [(1, "mox1".to_string()), (2, "mox2".to_string())].into()
        );
        assert_eq!(
            node_addresses(conf, "mox1"),
            [ip("10.10.0.1"), ip("100.101.1.1")]
        );
        assert_eq!(node_addresses(conf, "mox2"), [ip("10.10.0.2")]);
        assert!(node_addresses(conf, "mox3").is_empty());
        assert!(node_addresses(conf, "mox4").is_empty());
        assert!(node_addresses("", "mox1").is_empty());
    }

    /// The `pvecm status` v2's health test fakes for an even cluster with a
    /// QDevice (`test_show_cluster_health.py`, `FAKE_HOST_COMMAND`).
    fn pvecm(names: &[&str], up: &[&str], registered: bool, voting: bool) -> String {
        let expected = names.len() + usize::from(registered);
        let total = up.len() + usize::from(voting);
        let mut text = String::from(
            "Cluster information\n-------------------\nName:             MyAppCloud\n\n",
        );
        text += "Quorum information\n------------------\n";
        text += &format!(
            "Quorate:          {}\n\n",
            if total * 2 > expected { "Yes" } else { "No" }
        );
        text += "Votequorum information\n----------------------\n";
        text += &format!("Expected votes:   {expected}\n");
        text += &format!("Total votes:      {total}\n");
        text += &format!(
            "Flags:            Quorate{}\n\n",
            if voting { " Qdevice" } else { "" }
        );
        text += "Membership information\n----------------------\n";
        text += "    Nodeid      Votes    Qdevice Name\n";
        for name in up {
            let index = names.iter().position(|n| n == name).unwrap() + 1;
            let flags = if voting { "A,V,NMW" } else { "NA,NV,NMW" };
            text += &format!("0x{index:08x}          1    {flags} {name}\n");
        }
        if registered {
            text += &format!(
                "0x00000000          {}            Qdevice\n",
                u8::from(voting)
            );
        }
        text
    }

    const FOUR: &[&str] = &["mox1", "mox2", "mox3", "mox4"];

    #[test]
    fn qdevice_health_needs_every_vote() {
        let healthy = QuorumStatus::parse(&pvecm(FOUR, FOUR, true, true));
        assert_eq!(healthy.expected_votes(), Some(5));
        assert_eq!(healthy.value("Name"), Some("MyAppCloud"));
        assert_eq!(healthy.members.len(), 5);
        assert_eq!(healthy.members[0].qdevice.as_deref(), Some("A,V,NMW"));
        assert_eq!(healthy.members[4].name, "Qdevice");
        assert!(healthy.qdevice_healthy(4));
        assert!(!healthy.qdevice_healthy(3));
        assert!(!healthy.qdevice_absent_healthy(4));

        let not_voting = QuorumStatus::parse(&pvecm(FOUR, FOUR, true, false));
        assert!(!not_voting.qdevice_healthy(4));
        let host_down = QuorumStatus::parse(&pvecm(FOUR, &FOUR[..3], true, true));
        assert!(!host_down.qdevice_healthy(4));

        let three = &FOUR[..3];
        let odd = QuorumStatus::parse(&pvecm(three, three, false, false));
        assert!(odd.qdevice_absent_healthy(3));
        assert!(
            odd.members
                .iter()
                .all(|m| m.qdevice.as_deref() == Some("NA,NV,NMW"))
        );
    }

    #[test]
    fn quorumtool_without_a_qdevice_column() {
        let text = "Quorum information\n------------------\nQuorate:          No\n\n\
            Membership information\n----------------------\n\
            \x20   Nodeid      Votes Name\n\
            0x00000001          1 10.0.0.1 (local)\n";
        let status = QuorumStatus::parse(text);
        assert!(!status.quorate());
        assert_eq!(
            status.members,
            [MemberRow {
                nodeid: 1,
                votes: 1,
                qdevice: None,
                name: "10.0.0.1 (local)".into()
            }]
        );
    }

    #[test]
    fn links_drop_the_local_node_and_read_each_link() {
        let text = "Local node ID 1, transport knet\n\
            nodeid: 2 reachable\n\
            \x20  LINK: 0 udp (10.0.0.1->10.0.0.2) enabled connected mtu: 1397\n\
            \x20  LINK: 1 udp (100.64.0.1->100.64.0.2) enabled disconnected mtu: 1397\n\n\
            nodeid: 3 unreachable\n\
            \x20  LINK: 0 udp (a->b) enabled disconnected mtu: 1397\n\
            \x20  LINK: 1 udp (a->b) disabled connected mtu: 1397\n\n\
            nodeid: 1 reachable\n";
        let peers = parse_links(text);
        assert_eq!(peers.keys().copied().collect::<Vec<_>>(), [2, 3]);
        assert_eq!(peers[&2].links, BTreeMap::from([(0, true), (1, false)]));
        assert!(!peers[&2].down());
        assert_eq!(peers[&3].reachable, Some(false));
        assert!(peers[&3].down());
    }

    #[test]
    fn qdevice_entry_reads_the_device_block_host() {
        let conf = "quorum {\n  provider: corosync_votequorum\n  device {\n    model: net\n    net {\n      algorithm: ffsplit\n      host: 100.64.0.9\n      tls: on\n    }\n    votes: 1\n  }\n}\n";
        assert_eq!(
            parse_qdevice_entry(conf),
            QdeviceEntry {
                registered: true,
                address: Some("100.64.0.9".into())
            }
        );
        assert!(!parse_qdevice_entry("quorum {\n}\n").registered);
    }
}
