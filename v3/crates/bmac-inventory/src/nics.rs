// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Physical NICs and the link-wait rules (`DEPLOYMENT.md`, "NicLink-wait
//! rules"): bring each NIC up, poll carrier `attempts` times
//! `interval_seconds` apart, read the speed only once carrier is up, and
//! tell "no carrier" from "wrong speed".

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use bmac_proto::inventory::{InventoryNic, LinkWait, NicLink};

use crate::root::{Root, attr, link_name, names};

/// Brings a NIC administratively up; a NIC that is down reports no carrier.
pub trait LinkControl {
    fn set_up(&self, nic: &str) -> Result<(), String>;
}

/// `ip link set dev <nic> up`.
pub struct IpLink;

impl LinkControl for IpLink {
    fn set_up(&self, nic: &str) -> Result<(), String> {
        let output = Command::new("ip")
            .args(["link", "set", "dev", nic, "up"])
            .output()
            .map_err(|e| format!("couldn't run ip: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }
}

/// For reading links once, without bringing anything up.
pub struct LeaveAsIs;

impl LinkControl for LeaveAsIs {
    fn set_up(&self, _nic: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Physical Ethernet NICs: a backing device, ARP type 1 (Ethernet), and not
/// wireless. Bridges, VLANs, bonds, and loopback have no backing device.
pub fn physical_nics(root: &Root) -> Vec<String> {
    let net = root.sys.join("class/net");
    names(&net)
        .into_iter()
        .filter(|name| {
            let dir = net.join(name);
            dir.join("device").exists()
                && attr(&dir.join("type")).as_deref() == Some("1")
                && !dir.join("wireless").exists()
                && !dir.join("phy80211").exists()
        })
        .collect()
}

fn carrier(root: &Root, nic: &str) -> bool {
    // Reading carrier on a NIC that is down fails with EINVAL.
    attr(&root.sys.join("class/net").join(nic).join("carrier")).as_deref() == Some("1")
}

fn up(root: &Root, nic: &str, waited_seconds: u32) -> NicLink {
    let dir = root.sys.join("class/net").join(nic);
    NicLink::Up {
        // Unknown reads as -1 or fails while the link negotiates.
        speed_mbps: attr(&dir.join("speed"))
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|s| *s > 0)
            .and_then(|s| u32::try_from(s).ok()),
        duplex: attr(&dir.join("duplex")).filter(|d| d != "unknown"),
        waited_seconds,
    }
}

/// Waits for carrier on every NIC at once, so the whole wait is at most
/// `attempts` reads however many NICs there are. `sleep` is called between
/// reads.
pub fn wait_for_links(
    root: &Root,
    nics: &[String],
    wait: &LinkWait,
    control: &dyn LinkControl,
    sleep: &mut dyn FnMut(Duration),
) -> BTreeMap<String, NicLink> {
    let mut links = BTreeMap::new();
    let mut pending = Vec::new();
    for nic in nics {
        match control.set_up(nic) {
            Ok(()) => pending.push(nic.clone()),
            Err(message) => {
                links.insert(nic.clone(), NicLink::Error { message });
            }
        }
    }
    let attempts = wait.attempts.max(1);
    for attempt in 1..=attempts {
        let waited = (attempt - 1) * wait.interval_seconds;
        pending.retain(|nic| {
            if carrier(root, nic) {
                links.insert(nic.clone(), up(root, nic, waited));
                false
            } else {
                true
            }
        });
        if pending.is_empty() {
            break;
        }
        if attempt < attempts {
            sleep(Duration::from_secs(u64::from(wait.interval_seconds)));
        }
    }
    let waited_seconds = (attempts - 1) * wait.interval_seconds;
    for nic in pending {
        links.insert(nic, NicLink::NoCarrier { waited_seconds });
    }
    links
}

/// Every physical NIC with its link after the wait.
pub fn read_nics(
    root: &Root,
    wait: &LinkWait,
    control: &dyn LinkControl,
    sleep: &mut dyn FnMut(Duration),
) -> Vec<InventoryNic> {
    let names = physical_nics(root);
    let mut links = wait_for_links(root, &names, wait, control, sleep);
    names
        .into_iter()
        .map(|name| {
            let dir = root.sys.join("class/net").join(&name);
            InventoryNic {
                mac: attr(&dir.join("address"))
                    .unwrap_or_default()
                    .to_ascii_lowercase(),
                driver: link_name(&dir.join("device/driver")),
                bus: link_name(&dir.join("device/subsystem")),
                link: links
                    .remove(&name)
                    .unwrap_or(NicLink::NoCarrier { waited_seconds: 0 }),
                name,
            }
        })
        .collect()
}

/// What a link means for a NIC expected at a given speed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkVerdict {
    Ok,
    NoCarrier {
        waited_seconds: u32,
    },
    WrongSpeed {
        expected_mbps: u32,
        actual_mbps: Option<u32>,
    },
    Error {
        message: String,
    },
}

impl LinkVerdict {
    pub fn of(link: &NicLink, expected_mbps: Option<u32>) -> LinkVerdict {
        match link {
            NicLink::NoCarrier { waited_seconds } => LinkVerdict::NoCarrier {
                waited_seconds: *waited_seconds,
            },
            NicLink::Error { message } => LinkVerdict::Error {
                message: message.clone(),
            },
            NicLink::Up { speed_mbps, .. } => match expected_mbps {
                Some(expected) if *speed_mbps != Some(expected) => LinkVerdict::WrongSpeed {
                    expected_mbps: expected,
                    actual_mbps: *speed_mbps,
                },
                _ => LinkVerdict::Ok,
            },
        }
    }

    /// One sentence for the operator.
    pub fn describe(&self, nic: &str) -> String {
        match self {
            LinkVerdict::Ok => format!("{nic} has a link."),
            LinkVerdict::NoCarrier { waited_seconds } => {
                format!(
                    "{nic} has no carrier after about {waited_seconds} seconds. Check the cable and the switch port."
                )
            }
            LinkVerdict::WrongSpeed {
                expected_mbps,
                actual_mbps: Some(actual),
            } => {
                format!("{nic} has a link at {actual} Mb/s, not the expected {expected_mbps} Mb/s.")
            }
            LinkVerdict::WrongSpeed { expected_mbps, .. } => {
                format!("{nic} has a link but reports no speed; {expected_mbps} Mb/s was expected.")
            }
            LinkVerdict::Error { message } => format!("{nic} couldn't be brought up: {message}"),
        }
    }
}

/// Automatic detection of a NIC whose MAC isn't configured: after the wait,
/// exactly one candidate may have carrier (v2's rule).
pub fn single_with_carrier(links: &BTreeMap<String, NicLink>) -> Result<String, Vec<String>> {
    let up: Vec<String> = links
        .iter()
        .filter(|(_, link)| matches!(link, NicLink::Up { .. }))
        .map(|(nic, _)| nic.clone())
        .collect();
    match up.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(up),
    }
}
