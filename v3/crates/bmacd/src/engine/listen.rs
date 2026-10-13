// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Which addresses the Dashboard listens on: this host's Tailscale
//! addresses and its private (corosync ring) addresses. Anything outside
//! the private and Tailscale ranges is dropped, so a public address is
//! never offered even if it appears in a ring.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use bmac_adapters::corosync;
use bmac_adapters::tailscale::{Status, Tailscale};
use bmac_core::ports;
use bmac_exec::CommandRunner;

use crate::log;

pub const COROSYNC_CONF: &str = "/etc/pve/corosync.conf";

/// Whether an address may carry the Dashboard: RFC 1918, Tailscale's
/// 100.64.0.0/10, or IPv6 unique local (which includes Tailscale's
/// `fd7a:115c:a1e0::/48`).
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private() || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// The Dashboard's listen addresses from what Tailscale and corosync report.
pub fn dashboard_addresses(
    tailscale: Option<&Status>,
    corosync_conf: Option<&str>,
    node: &str,
) -> Vec<SocketAddr> {
    let mut ips: Vec<IpAddr> = Vec::new();
    if let Some(node) = tailscale
        .filter(|s| s.running())
        .and_then(|s| s.self_node.as_ref())
    {
        ips.extend(&node.tailscale_ips);
    }
    if let Some(conf) = corosync_conf {
        ips.extend(corosync::node_addresses(conf, node));
    }
    let mut addresses: Vec<SocketAddr> = Vec::new();
    for ip in ips {
        if !is_private(ip) {
            log::warning(&format!(
                "not listening on {ip}: it is not a private or Tailscale address"
            ));
            continue;
        }
        let address = SocketAddr::new(ip, ports::DASHBOARD);
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    addresses.sort();
    addresses
}

/// Reads the current addresses from the host.
/// This host's Proxmox node name: the short host name.
pub fn node_name() -> String {
    let node = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    node.split('.').next().unwrap_or_default().to_string()
}

pub async fn discover(runner: &dyn CommandRunner, corosync_conf: &Path) -> Vec<SocketAddr> {
    let status = Tailscale::new(runner).status().await.ok();
    let conf = std::fs::read_to_string(corosync_conf).ok();
    let short = node_name();
    dashboard_addresses(status.as_ref(), conf.as_deref(), &short)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: &str, ips: &[&str]) -> Status {
        serde_json::from_value(serde_json::json!({
            "BackendState": state,
            "Self": {"DNSName": "mox1.example.ts.net.", "TailscaleIPs": ips},
        }))
        .unwrap()
    }

    const CONF: &str = "nodelist {\n  node {\n    name: mox1\n    ring0_addr: 10.10.0.1\n  }\n  \
        node {\n    name: mox2\n    ring0_addr: 10.10.0.2\n  }\n}\n";

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn tailscale_and_private_ring_addresses() {
        let ts = status("Running", &["100.101.2.3", "fd7a:115c:a1e0::1"]);
        assert_eq!(
            dashboard_addresses(Some(&ts), Some(CONF), "mox1"),
            [
                addr("10.10.0.1:8443"),
                addr("100.101.2.3:8443"),
                addr("[fd7a:115c:a1e0::1]:8443")
            ]
        );
    }

    #[test]
    fn a_stopped_tailnet_or_missing_cluster_gives_fewer_addresses() {
        let stopped = status("Stopped", &["100.101.2.3"]);
        assert_eq!(
            dashboard_addresses(Some(&stopped), Some(CONF), "mox1"),
            [addr("10.10.0.1:8443")]
        );
        let running = status("Running", &["100.101.2.3"]);
        assert_eq!(
            dashboard_addresses(Some(&running), None, "mox1"),
            [addr("100.101.2.3:8443")]
        );
        assert!(dashboard_addresses(None, None, "mox1").is_empty());
    }

    #[test]
    fn public_and_wildcard_addresses_are_never_offered() {
        let conf = "nodelist {\n  node {\n    name: mox1\n    ring0_addr: 203.0.113.7\n    \
            ring1_addr: 0.0.0.0\n    ring2_addr: 192.168.5.1\n  }\n}\n";
        assert_eq!(
            dashboard_addresses(None, Some(conf), "mox1"),
            [addr("192.168.5.1:8443")]
        );
        let ts = status("Running", &["2001:db8::1", "127.0.0.1", "100.128.0.1"]);
        assert!(dashboard_addresses(Some(&ts), None, "mox1").is_empty());
    }

    #[test]
    fn private_ranges() {
        for ok in [
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.1",
            "192.168.0.1",
            "100.64.0.1",
            "100.127.255.254",
            "fd00::1",
        ] {
            assert!(is_private(ok.parse().unwrap()), "{ok}");
        }
        for no in [
            "8.8.8.8",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.0",
            "0.0.0.0",
            "::",
            "2001:db8::1",
            "fe80::1",
        ] {
            assert!(!is_private(no.parse().unwrap()), "{no}");
        }
    }
}
