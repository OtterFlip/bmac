// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `tailscale status --json` and `tailscale ip -4`.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};

use bmac_exec::{Command, CommandRunner};
use serde::{Deserialize, Serialize};

use crate::{AdapterError, programs, run_json, run_text};

/// One node of the tailnet as `tailscale status --json` describes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Node {
    #[serde(default)]
    pub host_name: String,
    /// The MagicDNS name with a trailing dot, such as
    /// `bmaclab-mox1.example.ts.net.`.
    #[serde(default, rename = "DNSName")]
    pub dns_name: String,
    #[serde(default, rename = "TailscaleIPs")]
    pub tailscale_ips: Vec<IpAddr>,
    #[serde(default)]
    pub online: bool,
    #[serde(default, rename = "OS")]
    pub os: String,
}

impl Node {
    /// The MagicDNS name's first label: the node's name on the tailnet.
    pub fn short_name(&self) -> &str {
        self.dns_name.split('.').next().unwrap_or_default()
    }

    pub fn ipv4(&self) -> Option<Ipv4Addr> {
        self.tailscale_ips.iter().find_map(|ip| match ip {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Tailnet {
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "MagicDNSSuffix")]
    pub magic_dns_suffix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Status {
    /// `Running` when connected; `NeedsLogin`, `Stopped`, ... otherwise.
    pub backend_state: String,
    #[serde(rename = "Self")]
    pub self_node: Option<Node>,
    /// Peers keyed by node key.
    #[serde(default, deserialize_with = "peers")]
    pub peer: BTreeMap<String, Node>,
    #[serde(default)]
    pub current_tailnet: Option<Tailnet>,
}

fn peers<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, Node>, D::Error> {
    Ok(Option::<BTreeMap<String, Node>>::deserialize(deserializer)?.unwrap_or_default())
}

impl Status {
    pub fn running(&self) -> bool {
        self.backend_state == "Running"
    }

    /// Peers whose tailnet name is `name`.
    pub fn peers_named<'s>(&'s self, name: &'s str) -> impl Iterator<Item = &'s Node> + 's {
        self.peer
            .values()
            .filter(move |node| node.short_name().eq_ignore_ascii_case(name))
    }
}

/// Parses `tailscale ip -4`: one address per line.
pub fn parse_ipv4(text: &str) -> Result<Vec<Ipv4Addr>, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.parse()
                .map_err(|_| format!("{line:?} is not an IPv4 address"))
        })
        .collect()
}

/// The `tailscale` command.
pub struct Tailscale<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> Tailscale<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Tailscale<'a> {
        Tailscale { runner }
    }

    pub async fn status(&self) -> Result<Status, AdapterError> {
        run_json(
            self.runner,
            Command::new(programs::TAILSCALE).args(["status", "--json"]),
        )
        .await
    }

    pub async fn ipv4(&self) -> Result<Vec<Ipv4Addr>, AdapterError> {
        let command = Command::new(programs::TAILSCALE).args(["ip", "-4"]);
        let text = run_text(self.runner, command.clone()).await?;
        parse_ipv4(&text).map_err(|e| AdapterError::parse(&command, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
      "Version": "1.90.0",
      "BackendState": "Running",
      "TailscaleIPs": ["100.64.0.5"],
      "Self": {"ID": "n1", "HostName": "bmaclab-mox1", "DNSName": "bmaclab-mox1.example.ts.net.",
               "OS": "linux", "TailscaleIPs": ["100.64.0.5", "fd7a:115c:a1e0::5"], "Online": true},
      "Peer": {
        "nodekey:aa": {"HostName": "bmaclab-qdevice", "DNSName": "bmaclab-qdevice.example.ts.net.",
                       "OS": "linux", "TailscaleIPs": ["100.64.0.9"], "Online": true, "Relay": "sea"},
        "nodekey:bb": {"HostName": "laptop", "DNSName": "laptop.example.ts.net.",
                       "TailscaleIPs": ["fd7a:115c:a1e0::7"], "Online": false}
      },
      "CurrentTailnet": {"Name": "example.org", "MagicDNSSuffix": "example.ts.net", "MagicDNSEnabled": true}
    }"#;

    #[test]
    fn status_reads_self_peers_and_tailnet() {
        let status: Status = serde_json::from_str(STATUS).unwrap();
        assert!(status.running());
        let me = status.self_node.as_ref().unwrap();
        assert_eq!(me.short_name(), "bmaclab-mox1");
        assert_eq!(me.ipv4(), Some(Ipv4Addr::new(100, 64, 0, 5)));
        let qdevice: Vec<_> = status.peers_named("bmaclab-qdevice").collect();
        assert_eq!(qdevice.len(), 1);
        assert_eq!(qdevice[0].ipv4(), Some(Ipv4Addr::new(100, 64, 0, 9)));
        assert_eq!(status.peers_named("laptop").next().unwrap().ipv4(), None);
        assert_eq!(
            status.current_tailnet.unwrap().magic_dns_suffix,
            "example.ts.net"
        );
    }

    #[test]
    fn logged_out_status_has_no_peers() {
        let status: Status =
            serde_json::from_str(r#"{"BackendState":"NeedsLogin","Self":null,"Peer":null}"#)
                .unwrap();
        assert!(!status.running());
        assert!(status.peer.is_empty());
    }

    #[test]
    fn ipv4_lines() {
        assert_eq!(
            parse_ipv4("100.64.0.5\n").unwrap(),
            [Ipv4Addr::new(100, 64, 0, 5)]
        );
        assert!(parse_ipv4("fd7a::1\n").is_err());
    }
}
