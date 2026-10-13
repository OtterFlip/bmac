// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The setup job's private network phase (`DEPLOYMENT.md`, "Host setup
//! job"; v2's `configure_private_network`): find the private NIC by its MAC,
//! wait for its 10 Gb/s link, put it in the private bridge with the host's
//! private VLAN and replication addresses, and have the operator confirm the
//! VLAN from the provider's side.

use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bmac_core::HostSlot;
use bmac_core::job::Severity;
use bmac_exec::{Command, CommandRunner};
use bmac_inventory::Root;
use bmac_inventory::nics::{
    LinkControl, LinkVerdict, physical_nics, single_with_carrier, wait_for_links,
};
use bmac_inventory::root::attr;
use bmac_proto::api::ApiError;
use bmac_proto::inventory::LinkWait;
use bmac_settings::{BridgeAddresses, Values, bridge_addresses};
use serde_json::{Value, json};

use super::{Stop, Workflow};

pub const IP: &str = "/usr/sbin/ip";
pub const IFRELOAD: &str = "/usr/sbin/ifreload";
const INTERFACES: &str = "/etc/network/interfaces";
const SOURCE_LINE: &str = "source /etc/network/interfaces.d/*";
pub const BRIDGE_FILE: &str = "/etc/network/interfaces.d/bmac-private.cfg";
pub const DEFAULT_BRIDGE: &str = "vmbr-private";
/// The private NIC's required speed, Mb/s.
pub const PRIVATE_SPEED: u32 = 10_000;
/// The private gateway's default route loses to the public one's (metric 0).
pub const GATEWAY_METRIC: u32 = 1000;
/// Looked up to prove which default route wins; nothing is sent to it.
const PROBE: &str = "1.1.1.1";
pub const CONFIRM_TEXT: &str = "PRIVATE VLAN VERIFIED";

/// What the phase sets up, from the host's and the cluster's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPlan {
    pub bridge: String,
    pub addresses: BridgeAddresses,
    pub public_mac: String,
    pub public_ip: Ipv4Addr,
    /// A MAC, or `auto`.
    pub private_mac: String,
}

impl NetworkPlan {
    pub fn from_settings(
        host: HostSlot,
        values: &Values,
        cluster: &Values,
    ) -> Result<NetworkPlan, Stop> {
        let addresses = bridge_addresses(host, values, cluster).map_err(settings_error)?;
        let get = |key: &str| {
            values
                .get(key)
                .cloned()
                .ok_or_else(|| Stop::failed(format!("{host} has no {key} setting")))
        };
        let public_ip = get("PROXMOX_IP")?
            .parse()
            .map_err(|_| Stop::failed("PROXMOX_IP is not an IPv4 address"))?;
        let bridge = cluster
            .get("PROXMOX_PRIVATE_BRIDGE")
            .map_or(DEFAULT_BRIDGE, String::as_str)
            .to_string();
        Ok(NetworkPlan {
            bridge,
            addresses,
            public_mac: get("PROXMOX_PUBLIC_MAC")?,
            public_ip,
            private_mac: get("PROXMOX_SECONDARY_MAC")?,
        })
    }
}

pub struct Network<'a> {
    pub flow: &'a Workflow,
    pub runner: &'a dyn CommandRunner,
    /// `/` on a host; `/etc` and `/sys` are read under it.
    pub root: PathBuf,
    pub plan: &'a NetworkPlan,
    pub link_wait: LinkWait,
    pub links: Arc<dyn LinkControl + Send + Sync>,
}

fn fail(what: &str, e: impl std::fmt::Display) -> Stop {
    Stop::Failed {
        message: format!("{what} failed"),
        details: Some(e.to_string()),
    }
}

fn settings_error(e: ApiError) -> Stop {
    let details: Vec<String> = e
        .field_errors
        .iter()
        .map(|(key, message)| format!("{key}: {message}"))
        .collect();
    Stop::Failed {
        message: format!(
            "This host's settings can't set up the private network: {}",
            e.message
        ),
        details: (!details.is_empty()).then(|| details.join("\n")),
    }
}

/// One IPv4 address on an interface, from `ip -j -4 address show`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub interface: String,
    pub ip: Ipv4Addr,
    pub prefix: u8,
}

pub fn parse_addresses(json: &str) -> Result<Vec<Address>, String> {
    let rows: Vec<Value> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let mut found = Vec::new();
    for row in rows {
        let Some(interface) = row["ifname"].as_str() else {
            continue;
        };
        for info in row["addr_info"].as_array().into_iter().flatten() {
            if info["family"] != "inet" {
                continue;
            }
            let (Some(ip), Some(prefix)) = (
                info["local"].as_str().and_then(|s| s.parse().ok()),
                info["prefixlen"]
                    .as_u64()
                    .and_then(|p| u8::try_from(p).ok()),
            ) else {
                continue;
            };
            found.push(Address {
                interface: interface.to_string(),
                ip,
                prefix,
            });
        }
    }
    Ok(found)
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    }
}

fn in_network(ip: Ipv4Addr, network: (Ipv4Addr, u8)) -> bool {
    u32::from(ip) & mask(network.1) == u32::from(network.0) & mask(network.1)
}

/// The problems with the addresses once the bridge is up: it holds exactly
/// the two it was given, and nothing else on the host is in the replication
/// network (v2's check).
pub fn address_problems(
    addresses: &[Address],
    bridge: &str,
    expected: &BridgeAddresses,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (what, (ip, prefix)) in [
        ("private VLAN", expected.private),
        ("replication", expected.replication),
    ] {
        let here = addresses
            .iter()
            .any(|a| a.interface == bridge && a.ip == ip && a.prefix == prefix);
        if !here {
            problems.push(format!(
                "{bridge} doesn't have its {what} address {ip}/{prefix}."
            ));
        }
    }
    let replication: Vec<&Address> = addresses
        .iter()
        .filter(|a| in_network(a.ip, expected.replication))
        .collect();
    for other in replication {
        if other.ip != expected.replication.0 || other.interface != bridge {
            problems.push(format!(
                "{} has {}/{}, which is in the replication network {}/{} too.",
                other.interface,
                other.ip,
                other.prefix,
                expected.replication.0,
                expected.replication.1
            ));
        }
    }
    problems
}

/// One route from `ip -j route ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub gateway: Option<Ipv4Addr>,
    pub device: String,
    pub metric: u32,
}

pub fn parse_routes(json: &str) -> Result<Vec<Route>, String> {
    let rows: Vec<Value> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(Route {
                gateway: row["gateway"].as_str().and_then(|g| g.parse().ok()),
                device: row["dev"].as_str()?.to_string(),
                metric: row["metric"]
                    .as_u64()
                    .and_then(|m| u32::try_from(m).ok())
                    .unwrap_or(0),
            })
        })
        .collect())
}

/// The bridge's stanza in `interfaces(5)` form.
pub fn bridge_stanza(bridge: &str, nic: &str, addresses: &BridgeAddresses) -> String {
    let (private, private_prefix) = addresses.private;
    let (replication, replication_prefix) = addresses.replication;
    let mut text = format!(
        "# Written by BMAC's host setup job: the private VLAN bridge.\n\
         auto {bridge}\n\
         iface {bridge} inet static\n\
         \taddress {private}/{private_prefix}\n\
         \taddress {replication}/{replication_prefix}\n\
         \tbridge-ports {nic}\n\
         \tbridge-stp off\n\
         \tbridge-fd 0\n"
    );
    if let (true, Some(gateway)) = (addresses.gateway_route, addresses.gateway) {
        let route = format!("default via {gateway} dev {bridge} metric {GATEWAY_METRIC}");
        text.push_str(&format!(
            "\tpost-up {IP} route replace {route}\n\
             \tpre-down {IP} route del {route} || true\n"
        ));
    }
    text
}

/// Whether `interfaces` reads the files in `interfaces.d`.
pub fn sources_directory(interfaces: &str) -> bool {
    interfaces.lines().any(|line| {
        let words: Vec<&str> = line.split_whitespace().collect();
        matches!(
            words.as_slice(),
            ["source", glob] if *glob == "/etc/network/interfaces.d/*"
                || *glob == "interfaces.d/*"
        )
    })
}

impl Network<'_> {
    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    fn net(&self, nic: &str) -> PathBuf {
        self.root.join("sys/class/net").join(nic)
    }

    fn mac(&self, nic: &str) -> Option<String> {
        attr(&self.net(nic).join("address")).map(|m| m.to_ascii_lowercase())
    }

    /// The bridge, bond, or other device a NIC is enslaved to.
    fn master(&self, nic: &str) -> Option<String> {
        std::fs::read_link(self.net(nic).join("master"))
            .ok()?
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }

    fn is_bridge(&self, nic: &str) -> bool {
        self.net(nic).join("bridge").is_dir()
    }

    fn by_mac(&self, nics: &[String], mac: &str, what: &str) -> Result<String, Stop> {
        let wanted = mac.to_ascii_lowercase();
        let found: Vec<&String> = nics
            .iter()
            .filter(|n| self.mac(n).as_deref() == Some(wanted.as_str()))
            .collect();
        match found.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(Stop::Failed {
                message: format!("No physical NIC on this host has the {what} MAC {mac}."),
                details: Some(
                    nics.iter()
                        .map(|n| format!("{n}: {}", self.mac(n).unwrap_or_default()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            }),
            several => Err(Stop::failed(format!(
                "{} physical NICs have the {what} MAC {mac}.",
                several.len()
            ))),
        }
    }

    async fn run(&self, command: Command) -> Result<String, Stop> {
        let shown = command.to_string();
        let output = self
            .runner
            .run(command)
            .await
            .map_err(|e| fail(&shown, e))?;
        Ok(output.stdout_text())
    }

    async fn addresses(&self) -> Result<Vec<Address>, Stop> {
        let json = self
            .run(Command::new(IP).args(["-j", "-4", "address", "show"]))
            .await?;
        parse_addresses(&json).map_err(|e| fail("reading ip's address list", e))
    }

    async fn routes(&self, args: &[&str]) -> Result<Vec<Route>, Stop> {
        let json = self
            .run(
                Command::new(IP)
                    .args(["-j", "route"])
                    .args(args.iter().copied()),
            )
            .await?;
        parse_routes(&json).map_err(|e| fail("reading ip's routes", e))
    }

    async fn wait(
        &self,
        nics: Vec<String>,
    ) -> Result<std::collections::BTreeMap<String, bmac_proto::inventory::NicLink>, Stop> {
        let root = Root::under(&self.root);
        let wait = self.link_wait;
        let control = Arc::clone(&self.links);
        tokio::task::spawn_blocking(move || {
            wait_for_links(
                &root,
                &nics,
                &wait,
                control.as_ref(),
                &mut std::thread::sleep,
            )
        })
        .await
        .map_err(|e| fail("waiting for the link", e))
    }

    /// The public NIC and the bridge on it that owns `PROXMOX_IP`.
    async fn public(&self, physical: &[String]) -> Result<(String, String), Stop> {
        let nic = self.by_mac(physical, &self.plan.public_mac, "public")?;
        let Some(bridge) = self.master(&nic).filter(|m| self.is_bridge(m)) else {
            return Err(Stop::failed(format!(
                "The public NIC {nic} isn't in a bridge; the Proxmox installer puts it in vmbr0."
            )));
        };
        let ip = self.plan.public_ip;
        let owns = self
            .addresses()
            .await?
            .iter()
            .any(|a| a.interface == bridge && a.ip == ip);
        if !owns {
            return Err(Stop::failed(format!(
                "{bridge}, the public NIC {nic}'s bridge, doesn't have PROXMOX_IP {ip}."
            )));
        }
        Ok((nic, bridge))
    }

    async fn private(
        &self,
        physical: &[String],
        public: &str,
        bridge: &str,
    ) -> Result<String, Stop> {
        let mac = self.plan.private_mac.as_str();
        let free = |n: &String| self.master(n).is_none_or(|m| m == bridge);
        if mac != "auto" {
            let nic = self.by_mac(physical, mac, "private")?;
            if nic == public {
                return Err(Stop::failed(format!(
                    "{nic} is both the public and the private NIC."
                )));
            }
            if !free(&nic) {
                return Err(Stop::failed(format!(
                    "The private NIC {nic} is already in {}.",
                    self.master(&nic).unwrap_or_default()
                )));
            }
            return Ok(nic);
        }
        let candidates: Vec<String> = physical
            .iter()
            .filter(|n| n.as_str() != public && free(n))
            .cloned()
            .collect();
        let links = self.wait(candidates.clone()).await?;
        single_with_carrier(&links).map_err(|up| Stop::Failed {
            message: if up.is_empty() {
                "PROXMOX_SECONDARY_MAC is auto, and no spare NIC has carrier.".into()
            } else {
                format!(
                    "PROXMOX_SECONDARY_MAC is auto, and {} spare NICs have carrier; set it to \
                     the private NIC's MAC.",
                    up.len()
                )
            },
            details: Some(format!("Spare NICs: {}", candidates.join(", "))),
        })
    }

    /// Writes `path` if its content differs; whether it did.
    fn write(&self, path: &str, text: &str) -> Result<bool, Stop> {
        let path = self.at(path);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(text) {
            return Ok(false);
        }
        let write = |path: &Path| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            super::write_atomic(path, text.as_bytes())?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
        };
        write(&path).map_err(|e| fail(&format!("writing {}", path.display()), e))?;
        Ok(true)
    }

    fn ensure_source_line(&self) -> Result<(), Stop> {
        let text = std::fs::read_to_string(self.at(INTERFACES))
            .map_err(|e| fail(&format!("reading {INTERFACES}"), e))?;
        if sources_directory(&text) {
            return Ok(());
        }
        let mut text = text;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("\n{SOURCE_LINE}\n"));
        self.write(INTERFACES, &text)?;
        self.flow
            .info(format!("{INTERFACES} now reads /etc/network/interfaces.d."));
        Ok(())
    }

    async fn check_routes(
        &self,
        bridge: &str,
        public_bridge: &str,
        addresses: &BridgeAddresses,
    ) -> Result<(), Stop> {
        let (true, Some(gateway)) = (addresses.gateway_route, addresses.gateway) else {
            return Ok(());
        };
        let defaults = self.routes(&["show", "default"]).await?;
        let ours = Route {
            gateway: Some(gateway),
            device: bridge.to_string(),
            metric: GATEWAY_METRIC,
        };
        if !defaults.contains(&ours) {
            return Err(Stop::Failed {
                message: format!(
                    "The second default route via {gateway} on {bridge} isn't in place."
                ),
                details: Some(format!("{defaults:?}")),
            });
        }
        let chosen = self.routes(&["get", PROBE]).await?;
        match chosen.first() {
            Some(route) if route.device == public_bridge => {
                self.flow.info(format!(
                    "The second default route via {gateway} is in place, behind the public one."
                ));
                Ok(())
            }
            other => Err(Stop::Failed {
                message: format!(
                    "Traffic to the internet would leave by {}, not the public bridge \
                     {public_bridge}.",
                    other.map_or("no route", |r| r.device.as_str())
                ),
                details: Some(format!("{chosen:?}")),
            }),
        }
    }

    pub async fn run_phase(&self) -> Result<Value, Stop> {
        let addresses = &self.plan.addresses;
        let bridge = self.plan.bridge.as_str();
        let physical = physical_nics(&Root::under(&self.root));
        let (public, public_bridge) = self.public(&physical).await?;
        if bridge == public_bridge {
            return Err(Stop::failed(format!(
                "PROXMOX_PRIVATE_BRIDGE is {bridge}, the public bridge."
            )));
        }
        let private = self.private(&physical, &public, bridge).await?;
        let links = self.wait(vec![private.clone()]).await?;
        let verdict = links
            .get(&private)
            .map_or(LinkVerdict::NoCarrier { waited_seconds: 0 }, |link| {
                LinkVerdict::of(link, Some(PRIVATE_SPEED))
            });
        if verdict != LinkVerdict::Ok {
            return Err(Stop::failed(verdict.describe(&private)));
        }
        self.flow.info(format!(
            "The private NIC {private} ({}) has a 10 Gb/s link.",
            self.mac(&private).unwrap_or_default()
        ));

        self.ensure_source_line()?;
        let stanza = bridge_stanza(bridge, &private, addresses);
        if self.write(BRIDGE_FILE, &stanza)? {
            self.flow.info(format!("Wrote {BRIDGE_FILE}."));
        }
        self.run(Command::new(IFRELOAD).arg("-a")).await?;
        let problems = address_problems(&self.addresses().await?, bridge, addresses);
        if !problems.is_empty() {
            return Err(Stop::Failed {
                message: format!("{bridge} didn't come up with its addresses."),
                details: Some(problems.join("\n")),
            });
        }
        self.check_routes(bridge, &public_bridge, addresses).await?;
        let (ip, prefix) = addresses.private;
        let confirmed = self
            .flow
            .confirm(
                "Check the private VLAN",
                &format!(
                    "{bridge} has {ip}/{prefix}. From the provider's console or another host \
                     on the VLAN, check that this address answers, and that the provider gives \
                     one shared Layer-2 VLAN rather than separate routed segments."
                ),
                Severity::Warning,
                Some(CONFIRM_TEXT),
                vec![
                    format!("Public NIC: {public} in {public_bridge}"),
                    format!("Private NIC: {private} in {bridge}"),
                ],
            )
            .await?;
        if !confirmed {
            return Err(Stop::failed(
                "The private VLAN wasn't confirmed. Check it and resume the job.",
            ));
        }
        Ok(json!({
            "public_nic": public,
            "public_bridge": public_bridge,
            "private_nic": private,
            "bridge": bridge,
        }))
    }
}

#[cfg(test)]
#[path = "network_tests.rs"]
mod tests;
