// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! What job admission needs to know about the cluster: quorum, which hosts
//! are online, and where guests run. Read from pmxcfs: `/etc/pve/.members`
//! for membership, the guest's config under `/etc/pve/nodes/<host>/` for its
//! owner, and the registry for its placement.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::path::PathBuf;

use bmac_core::{GuestName, HostSlot};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Membership {
    pub quorate: bool,
    /// Online cluster members with a BMAC host name.
    pub online: BTreeSet<HostSlot>,
    /// Every cluster member with a BMAC host name, online or not, and its
    /// cluster address when known.
    pub members: BTreeMap<HostSlot, Option<IpAddr>>,
}

impl Membership {
    /// The cluster duty host: the lowest-numbered online host in the
    /// quorate partition. A partition without quorum has none.
    pub fn duty(&self) -> Option<HostSlot> {
        self.online.first().copied().filter(|_| self.quorate)
    }
}

/// Whether `host` is the duty host now, from a fresh read of membership.
/// Duties check this right before each action rather than trusting an
/// earlier answer, as v2's ingress coordinator does
/// (`sync_haproxy_routes.sh` 1152).
pub fn holds_duty(cluster: &dyn ClusterView, host: HostSlot) -> Result<(), String> {
    let membership = cluster.membership()?;
    match membership.duty() {
        Some(duty) if duty == host => Ok(()),
        Some(duty) => Err(format!("{duty} is the duty host, not {host}")),
        None => Err("the cluster has no quorum, so no host has cluster duties".into()),
    }
}

pub trait ClusterView: Send + Sync {
    fn membership(&self) -> Result<Membership, String>;
    /// The host whose node directory holds the guest's config, or `None`
    /// when no host has it.
    fn guest_owner(&self, guest: GuestName) -> Result<Option<HostSlot>, String>;
    /// The production guest's placement hosts, from the registry.
    fn placement(&self, guest: GuestName) -> Result<Vec<HostSlot>, String>;
}

/// Parses pmxcfs's `.members`. A host that isn't in a cluster lists only
/// itself and has no `cluster` object; it counts as quorate.
pub fn parse_members(text: &str) -> Result<Membership, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!(".members: {e}"))?;
    let Some(cluster) = value.get("cluster") else {
        let online: BTreeSet<HostSlot> = value["nodename"]
            .as_str()
            .and_then(|name| name.parse().ok())
            .into_iter()
            .collect();
        return Ok(Membership {
            quorate: true,
            members: online.iter().map(|h| (*h, None)).collect(),
            online,
        });
    };
    let quorate = cluster["quorate"].as_u64() == Some(1);
    let mut online = BTreeSet::new();
    let mut members = BTreeMap::new();
    if let Some(nodes) = value["nodelist"].as_object() {
        for (name, node) in nodes {
            let Ok(slot) = name.parse::<HostSlot>() else {
                continue;
            };
            members.insert(slot, node["ip"].as_str().and_then(|ip| ip.parse().ok()));
            if node["online"].as_u64() == Some(1) {
                online.insert(slot);
            }
        }
    }
    Ok(Membership {
        quorate,
        online,
        members,
    })
}

/// The cluster as pmxcfs shows it.
pub struct PveCluster {
    root: PathBuf,
    registry: bmac_registry::Registry,
}

impl PveCluster {
    pub fn new(root: impl Into<PathBuf>, registry: bmac_registry::Registry) -> PveCluster {
        PveCluster {
            root: root.into(),
            registry,
        }
    }

    pub fn system() -> PveCluster {
        PveCluster::new("/etc/pve", bmac_registry::Registry::system())
    }
}

impl ClusterView for PveCluster {
    fn membership(&self) -> Result<Membership, String> {
        let path = self.root.join(".members");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        parse_members(&text)
    }

    fn guest_owner(&self, guest: GuestName) -> Result<Option<HostSlot>, String> {
        let resource = self
            .registry
            .resource(guest)
            .map_err(|e| format!("{guest}: {e}"))?;
        let config = format!("{}.conf", resource.vmid);
        for host in HostSlot::all() {
            let path = self
                .root
                .join("nodes")
                .join(host.to_string())
                .join("qemu-server")
                .join(&config);
            if path.symlink_metadata().is_ok_and(|m| m.is_file()) {
                return Ok(Some(host));
            }
        }
        Ok(None)
    }

    fn placement(&self, guest: GuestName) -> Result<Vec<HostSlot>, String> {
        let resource = self
            .registry
            .resource(guest)
            .map_err(|e| format!("{guest}: {e}"))?;
        Ok(resource.placement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_give_quorum_and_online_hosts() {
        let text = r#"{"nodename": "mox2", "version": 9,
            "cluster": {"name": "c", "version": 4, "nodes": 4, "quorate": 1},
            "nodelist": {
                "mox1": {"id": 1, "online": 1, "ip": "10.0.0.1"},
                "mox2": {"id": 2, "online": 1, "ip": "10.0.0.2"},
                "mox3": {"id": 3, "online": 0, "ip": "10.0.0.3"},
                "other": {"id": 4, "online": 1, "ip": "10.0.0.4"}
            }}"#;
        let members = parse_members(text).unwrap();
        assert!(members.quorate);
        let online: Vec<String> = members.online.iter().map(|h| h.to_string()).collect();
        assert_eq!(online, ["mox1", "mox2"]);
        assert_eq!(members.duty().unwrap().to_string(), "mox1");
        assert_eq!(
            members.members.len(),
            3,
            "offline members count, other names don't"
        );
        assert_eq!(
            members.members[&HostSlot::new(3).unwrap()],
            Some("10.0.0.3".parse().unwrap())
        );

        let lost = text.replace(r#""quorate": 1"#, r#""quorate": 0"#);
        assert!(!parse_members(&lost).unwrap().quorate);
    }

    struct Fixed(Membership);

    impl ClusterView for Fixed {
        fn membership(&self) -> Result<Membership, String> {
            Ok(self.0.clone())
        }
        fn guest_owner(&self, _guest: GuestName) -> Result<Option<HostSlot>, String> {
            Ok(None)
        }
        fn placement(&self, _guest: GuestName) -> Result<Vec<HostSlot>, String> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn the_duty_moves_to_the_next_host_when_the_lowest_goes_offline() {
        let slot = |n| HostSlot::new(n).unwrap();
        let mut membership = Membership {
            quorate: true,
            online: [1, 2, 3].map(slot).into_iter().collect(),
            members: [1, 2, 3].map(|n| (slot(n), None)).into_iter().collect(),
        };
        assert_eq!(membership.duty(), Some(slot(1)));
        assert!(holds_duty(&Fixed(membership.clone()), slot(1)).is_ok());
        let refused = holds_duty(&Fixed(membership.clone()), slot(2)).unwrap_err();
        assert_eq!(refused, "mox1 is the duty host, not mox2");

        membership.online.remove(&slot(1));
        assert_eq!(membership.duty(), Some(slot(2)));
        assert!(holds_duty(&Fixed(membership.clone()), slot(2)).is_ok());
        assert!(holds_duty(&Fixed(membership.clone()), slot(1)).is_err());

        membership.quorate = false;
        assert_eq!(
            membership.duty(),
            None,
            "a partition without quorum has none"
        );
        assert!(holds_duty(&Fixed(membership), slot(2)).is_err());
    }

    #[test]
    fn a_lone_host_is_quorate() {
        let members = parse_members(r#"{"nodename": "mox1", "version": 0}"#).unwrap();
        assert!(members.quorate);
        assert_eq!(members.online.len(), 1);
        assert!(parse_members("not json").is_err());
    }
}
