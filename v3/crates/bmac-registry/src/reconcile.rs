// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The registry compared with live Proxmox state, read-only: the checks
//! v2's `cluster_registry.py reconcile --live` makes with what its live
//! observation collects (`collect_live_observation` 4071–4193: nodes, VMs,
//! and volumes; `reconcile_report` 4259–4692), with v2's codes and wording.
//! Nothing is ever changed here.

use std::collections::{BTreeMap, BTreeSet};

use bmac_core::Vmid;

use crate::policy::Policy;
use crate::resource::Resource;
use crate::state::{Kind, ResourceState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedVm {
    pub vmid: Vmid,
    pub name: String,
    pub node: String,
    pub status: String,
    pub mac: Option<String>,
    pub tags: BTreeSet<String>,
    pub volume_ids: BTreeSet<String>,
}

/// What `pvesh` shows: every node and whether it is online, and the
/// registered VMs that exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    pub nodes: BTreeMap<String, bool>,
    pub vms: BTreeMap<Vmid, ObservedVm>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub severity: Severity,
    pub code: &'static str,
    pub resource: String,
    pub message: String,
}

/// Every disagreement, errors first, then by code and resource.
pub fn live(
    resources: &[Resource],
    policy: &Policy,
    observed: &Observed,
    now_ms: i64,
) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut issue = |severity, code, resource: &Resource, message: String| {
        issues.push(Issue {
            severity,
            code,
            resource: resource.name.to_string(),
            message,
        })
    };
    let volumes: BTreeSet<&str> = observed
        .vms
        .values()
        .flat_map(|vm| vm.volume_ids.iter().map(String::as_str))
        .collect();
    for resource in resources {
        let placement: Vec<String> = resource.placement.iter().map(|h| h.to_string()).collect();
        let missing: Vec<&str> = placement
            .iter()
            .filter(|n| !observed.nodes.contains_key(*n))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            issue(
                Severity::Error,
                "placement-node-missing",
                resource,
                format!(
                    "placement nodes absent from Proxmox API: {}",
                    missing.join(", ")
                ),
            );
        }
        let offline: Vec<&str> = placement
            .iter()
            .filter(|n| observed.nodes.get(*n) == Some(&false))
            .map(String::as_str)
            .collect();
        if !offline.is_empty() {
            issue(
                Severity::Warning,
                "placement-node-offline",
                resource,
                format!("placement nodes reported offline: {}", offline.join(", ")),
            );
        }

        match observed.vms.get(&resource.vmid) {
            None => {
                let created = resource
                    .created_at
                    .parse::<jiff::Timestamp>()
                    .map(|t| t.as_millisecond())
                    .unwrap_or(now_ms);
                let age_seconds = (now_ms - created) / 1000;
                let reserving = matches!(
                    resource.state,
                    ResourceState::Reserved | ResourceState::Provisioning
                );
                if reserving && age_seconds > policy.limits.reservation_ttl_seconds as i64 {
                    issue(
                        Severity::Error,
                        "stale-reservation",
                        resource,
                        "reservation exceeded policy TTL and no VM exists".to_string(),
                    );
                } else if !reserving && resource.state != ResourceState::Failed {
                    issue(
                        Severity::Error,
                        "vm-missing",
                        resource,
                        format!(
                            "registered {} resource has no observed VM",
                            resource.state.as_str()
                        ),
                    );
                }
            }
            Some(vm) => {
                if vm.name != resource.name.to_string() {
                    issue(
                        Severity::Error,
                        "vm-name-mismatch",
                        resource,
                        "VM name differs from registry".to_string(),
                    );
                }
                if vm
                    .mac
                    .as_deref()
                    .is_some_and(|mac| !mac.eq_ignore_ascii_case(&resource.mac))
                {
                    issue(
                        Severity::Error,
                        "vm-mac-mismatch",
                        resource,
                        "VM MAC differs from registry".to_string(),
                    );
                }
                if !placement.contains(&vm.node) {
                    issue(
                        Severity::Error,
                        "vm-placement-mismatch",
                        resource,
                        format!("VM is on {}, outside placement", vm.node),
                    );
                }
                let expected: Vec<&String> = match resource.kind {
                    Kind::Production => vec![&policy.tags.production],
                    Kind::Staging => vec![&policy.tags.staging, &policy.tags.evictable],
                };
                let mut absent: Vec<&str> = expected
                    .into_iter()
                    .filter(|tag| !vm.tags.contains(*tag))
                    .map(String::as_str)
                    .collect();
                absent.sort();
                if !absent.is_empty() {
                    issue(
                        Severity::Error,
                        "vm-tag-mismatch",
                        resource,
                        format!("VM is missing required tags: {}", absent.join(", ")),
                    );
                }
                if resource.state == ResourceState::Active && vm.status != "running" {
                    issue(
                        Severity::Error,
                        "vm-status-mismatch",
                        resource,
                        "active registry resource is not running".to_string(),
                    );
                }
            }
        }

        if let Some(volume) = &resource.proxmox.volume_id
            && !volumes.contains(volume.as_str())
        {
            issue(
                Severity::Error,
                "volume-missing",
                resource,
                format!("volume {volume} is absent"),
            );
        }
    }
    issues
        .sort_by(|a, b| (a.severity, a.code, &a.resource).cmp(&(b.severity, b.code, &b.resource)));
    issues
}

/// The MAC of a QEMU `net0` value, lower-cased (v2 4148–4156).
pub fn net0_mac(net0: &str) -> Option<String> {
    net0.split(',').find_map(|part| {
        let (model, mac) = part.split_once('=')?;
        let known = matches!(model, "virtio" | "e1000" | "rtl8139" | "vmxnet3");
        let octets: Vec<&str> = mac.split(':').collect();
        let valid = octets.len() == 6
            && octets
                .iter()
                .all(|o| o.len() == 2 && o.chars().all(|c| c.is_ascii_hexdigit()));
        (known && valid).then(|| mac.to_ascii_lowercase())
    })
}

/// The volume IDs of a VM's disks in its QEMU configuration (v2 4158–4167).
pub fn disk_volumes(config: &serde_json::Map<String, serde_json::Value>) -> BTreeSet<String> {
    config
        .iter()
        .filter(|(key, _)| {
            let bus = key.trim_end_matches(|c: char| c.is_ascii_digit());
            bus.len() < key.len() && matches!(bus, "scsi" | "virtio" | "sata" | "ide")
        })
        .filter_map(|(_, value)| value.as_str())
        .map(|value| value.split(',').next().unwrap_or_default().to_string())
        .filter(|volume| !matches!(volume.as_str(), "none" | "cdrom" | "cloudinit"))
        .collect()
}

/// A VM's tags in its QEMU configuration (`;` or `,` separated).
pub fn config_tags(config: &serde_json::Map<String, serde_json::Value>) -> BTreeSet<String> {
    config
        .get("tags")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .split([';', ','])
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}
