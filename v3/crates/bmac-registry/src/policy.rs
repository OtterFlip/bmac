// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The registry policy (`validate_policy`, `cluster_registry.py` 1359–1508)
//! and resource specs (`validate_spec`, 1511–1553).

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use bmac_core::HostSlot;

use crate::check::{exact_keys, fail, int, is_uuid, py_str, safe_id, timestamp, typed};
use crate::state::{Kind, state_transitions_json};
use crate::{Result, SCHEMA_VERSION};

/// Addresses as offsets from the policy network's address; on a /24 they
/// are the last octets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffsetRange {
    pub start: u32,
    pub end: u32,
}

impl OffsetRange {
    /// How many addresses the range holds; a valid range holds at least one.
    pub fn size(self) -> u32 {
        self.end - self.start + 1
    }

    pub fn contains(self, offset: u32) -> bool {
        (self.start..=self.end).contains(&offset)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmidRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyNetwork {
    pub cidr: String,
    pub guest_gateway: String,
    pub mox_host_range: OffsetRange,
    pub haproxy_range: OffsetRange,
    pub production_range: OffsetRange,
    pub staging_range: OffsetRange,
    pub haproxy_vmid_range: VmidRange,
}

impl PolicyNetwork {
    /// The network's address and prefix length.
    pub fn network(&self) -> (Ipv4Addr, u8) {
        parse_network(&self.cidr).unwrap_or((Ipv4Addr::UNSPECIFIED, 32))
    }

    /// The network's address.
    pub fn base(&self) -> Ipv4Addr {
        self.network().0
    }

    /// `ip`'s offset from the network's address when it lies in the network.
    pub fn offset(&self, ip: Ipv4Addr) -> Option<u32> {
        let (base, len) = self.network();
        let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
        let ip = u32::from(ip);
        (ip & mask == u32::from(base)).then_some(ip - u32::from(base))
    }

    /// The address at `offset` from the network's address.
    pub fn address(&self, offset: u32) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.base()).saturating_add(offset))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyLimits {
    pub max_hosts: u8,
    pub max_production: u32,
    pub max_staging: u32,
    pub reservation_ttl_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyTags {
    pub production: String,
    pub staging: String,
    pub evictable: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskAllocation {
    Sparse,
    Reserved,
}

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;
pub const MAX_DISK_BYTES: u64 = 1024 * 1024 * GIB;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub cores: u32,
    pub memory_mb: u64,
    pub disk_gib: u64,
    pub disk_allocation: DiskAllocation,
    /// Minutes between replication runs; v2 stores it as text.
    #[serde(deserialize_with = "minutes", serialize_with = "minutes_text")]
    pub replication_interval: u32,
    /// The Proxmox storage of the root disk, which carries its block size.
    pub storage: String,
    /// Present only after an online production-disk growth; `disk_gib` stays
    /// the creation-time size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
}

impl Spec {
    pub fn effective_disk_bytes(&self) -> u64 {
        self.disk_bytes.unwrap_or(self.disk_gib * GIB)
    }
}

fn minutes<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<u32, D::Error> {
    let value = Value::deserialize(deserializer)?;
    py_str(&value).parse().map_err(serde::de::Error::custom)
}

fn minutes_text<S: serde::Serializer>(
    value: &u32,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDefaults {
    pub production: Spec,
    pub staging: Spec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema_version: u64,
    pub record_type: String,
    pub registry_id: String,
    pub network: PolicyNetwork,
    pub limits: PolicyLimits,
    pub tags: PolicyTags,
    pub defaults: PolicyDefaults,
    pub state_transitions: BTreeMap<Kind, BTreeMap<String, Vec<String>>>,
    pub created_at: String,
}

fn parse_network(cidr: &str) -> Option<(Ipv4Addr, u8)> {
    let (address, prefix) = cidr.split_once('/')?;
    let prefix: u8 = prefix.parse().ok()?;
    if prefix.to_string() != cidr.split_once('/')?.1 {
        return None;
    }
    let address: Ipv4Addr = address.parse().ok()?;
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    };
    (prefix <= 32 && u32::from(address) & !mask == 0).then_some((address, prefix))
}

fn parse_interface(text: &str) -> Option<(Ipv4Addr, u8)> {
    let (address, prefix) = match text.split_once('/') {
        Some((a, p)) => (a, p.parse::<u8>().ok().filter(|p| *p <= 32)?),
        None => (text, 32),
    };
    Some((address.parse().ok()?, prefix))
}

/// A Proxmox storage ID, as pvesm accepts it.
fn storage_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let (Some(first), Some(last)) = (bytes.first(), bytes.last()) else {
        return false;
    };
    (2..=64).contains(&bytes.len())
        && first.is_ascii_lowercase()
        && (last.is_ascii_lowercase() || last.is_ascii_digit())
        && bytes
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
}

pub fn validate_spec(spec: &Value) -> Result<()> {
    let Some(map) = spec.as_object() else {
        return fail("resource spec must be an object");
    };
    let mut keys = vec![
        "cores",
        "memory_mb",
        "disk_gib",
        "disk_allocation",
        "replication_interval",
        "storage",
    ];
    if map.contains_key("disk_bytes") {
        keys.push("disk_bytes");
    }
    exact_keys(spec, &keys, "resource spec")?;
    for field in ["cores", "memory_mb", "disk_gib"] {
        if !int(&spec[field]).is_some_and(|n| n > 0) {
            return fail(format!("resource spec {field} must be a positive integer"));
        }
    }
    if !matches!(
        spec["disk_allocation"].as_str(),
        Some("sparse" | "reserved")
    ) {
        return fail("disk allocation must be sparse or reserved");
    }
    let interval = py_str(&spec["replication_interval"]);
    if interval.is_empty()
        || interval.starts_with('0')
        || !interval.bytes().all(|b| b.is_ascii_digit())
    {
        return fail("replication interval must be a positive minute count");
    }
    if !spec["storage"].as_str().is_some_and(storage_id) {
        return fail("resource spec storage must be a Proxmox storage id");
    }
    if let Some(bytes) = map.get("disk_bytes") {
        let disk_gib = int(&spec["disk_gib"]).unwrap_or(0);
        let ok = bytes.as_u64().is_some_and(|b| {
            b % MIB == 0 && i128::from(b) > disk_gib * i128::from(GIB) && b <= MAX_DISK_BYTES
        });
        if !ok {
            return fail(
                "resource spec disk_bytes must be a whole-MiB byte count larger than disk_gib",
            );
        }
    }
    Ok(())
}

/// The prefix lengths a policy network may have.
const PREFIXES: std::ops::RangeInclusive<u8> = 16..=24;

/// The VMIDs of the hosts' fixed HAProxy LXCs, 9110 plus the slot, for
/// every slot a cluster can have.
pub const HAPROXY_VMIDS: VmidRange = VmidRange {
    start: 9111,
    end: 9110 + HostSlot::MAX as u32,
};

const RANGES: [&str; 4] = [
    "mox_host_range",
    "haproxy_range",
    "production_range",
    "staging_range",
];

pub fn validate_policy(policy: &Value) -> Result<Policy> {
    if !policy.is_object() {
        return fail("registry policy must be a JSON object");
    }
    exact_keys(
        policy,
        &[
            "schema_version",
            "record_type",
            "registry_id",
            "network",
            "limits",
            "tags",
            "defaults",
            "state_transitions",
            "created_at",
        ],
        "registry policy",
    )?;
    if policy["schema_version"] != SCHEMA_VERSION || policy["record_type"] != "policy" {
        return fail("unsupported registry policy schema");
    }
    if !is_uuid(&policy["registry_id"]) {
        return fail("policy registry_id is not a UUID");
    }
    timestamp(&policy["created_at"])?;

    let network = &policy["network"];
    if !network.is_object() {
        return fail("policy network must be an object");
    }
    exact_keys(
        network,
        &[
            "cidr",
            "guest_gateway",
            "mox_host_range",
            "haproxy_range",
            "production_range",
            "staging_range",
            "haproxy_vmid_range",
        ],
        "policy network",
    )?;
    let cidr = network["cidr"].as_str().and_then(parse_network);
    let gateway = network["guest_gateway"].as_str().and_then(parse_interface);
    let shared = || {
        fail(format!(
            "policy network and guest gateway must share an IPv4 network from /{} to /{}",
            PREFIXES.start(),
            PREFIXES.end()
        ))
    };
    let (Some((base, len)), Some((gateway_ip, gateway_len))) = (cidr, gateway) else {
        return shared();
    };
    let mask = u32::MAX << (32 - u32::from(len.max(1)));
    if !PREFIXES.contains(&len)
        || gateway_len != len
        || u32::from(gateway_ip) & mask != u32::from(base)
    {
        return shared();
    }
    let last = !mask - 1;
    if network["haproxy_vmid_range"] != serde_json::json!(HAPROXY_VMIDS) {
        return fail(format!(
            "policy haproxy_vmid_range must be {}-{}",
            HAPROXY_VMIDS.start, HAPROXY_VMIDS.end
        ));
    }
    let mut ranges = Vec::new();
    for name in RANGES {
        let value = &network[name];
        let ok = value
            .as_object()
            .is_some_and(|m| m.len() == 2 && m.contains_key("start") && m.contains_key("end"))
            && matches!((int(&value["start"]), int(&value["end"])), (Some(s), Some(e)) if 1 <= s && s <= e && e <= i128::from(last));
        if !ok {
            return fail(format!("policy {name} is not a usable host-offset range"));
        }
        ranges.push((
            name,
            int(&value["start"]).unwrap_or(0),
            int(&value["end"]).unwrap_or(0),
        ));
    }
    for (index, (name, start, end)) in ranges.iter().enumerate() {
        for (other, other_start, other_end) in &ranges[index + 1..] {
            if start <= other_end && other_start <= end {
                return fail(format!("policy {name} and {other} overlap"));
            }
        }
    }
    let gateway_octet = i128::from(u32::from(gateway_ip) - u32::from(base));
    if !(1..=i128::from(last)).contains(&gateway_octet)
        || ranges
            .iter()
            .any(|(_, s, e)| (*s..=*e).contains(&gateway_octet))
    {
        return fail("policy guest gateway is invalid or overlaps an allocation range");
    }
    let size = |name: &str| {
        ranges
            .iter()
            .find(|r| r.0 == name)
            .map(|r| r.2 - r.1 + 1)
            .unwrap_or(0)
    };

    let limits = &policy["limits"];
    exact_keys(
        limits,
        &[
            "max_hosts",
            "max_production",
            "max_staging",
            "reservation_ttl_seconds",
        ],
        "policy limits",
    )?;
    let max_hosts =
        int(&limits["max_hosts"]).filter(|n| (1..=i128::from(HostSlot::MAX)).contains(n));
    let Some(max_hosts) = max_hosts else {
        return fail(format!(
            "policy max_hosts must be between 1 and {}",
            HostSlot::MAX
        ));
    };

    if size("mox_host_range") < max_hosts || size("haproxy_range") < max_hosts {
        return fail("policy host allocation ranges are too small");
    }
    if int(&limits["max_production"]) != Some(size("production_range"))
        || int(&limits["max_staging"]) != Some(size("staging_range"))
    {
        return fail("policy allocation limits do not match IP ranges");
    }
    if !int(&limits["reservation_ttl_seconds"]).is_some_and(|n| n >= 60) {
        return fail("policy reservation TTL is invalid");
    }

    let tags = &policy["tags"];
    exact_keys(tags, &["production", "staging", "evictable"], "policy tags")?;
    let mut seen = Vec::new();
    for (name, value) in tags.as_object().into_iter().flatten() {
        safe_id(value, &format!("{name} tag"))?;
        if !seen.contains(&value) {
            seen.push(value);
        }
    }
    if seen.len() != 3 {
        return fail("policy tags must be distinct");
    }

    let defaults = &policy["defaults"];
    exact_keys(defaults, &["production", "staging"], "policy defaults")?;
    for kind in ["production", "staging"] {
        let spec = &defaults[kind];
        exact_keys(
            spec,
            &[
                "cores",
                "memory_mb",
                "disk_gib",
                "disk_allocation",
                "replication_interval",
                "storage",
            ],
            &format!("policy {kind} defaults"),
        )?;
        validate_spec(spec)?;
    }
    if policy["state_transitions"] != state_transitions_json() {
        return fail("policy state transitions differ from this schema version");
    }
    typed(policy, "registry policy")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn networks_parse_strictly() {
        assert_eq!(
            parse_network("10.1.2.0/24"),
            Some((Ipv4Addr::new(10, 1, 2, 0), 24))
        );
        assert_eq!(parse_network("10.1.2.5/24"), None, "host bits set");
        assert_eq!(parse_network("10.1.2.0/024"), None);
        assert_eq!(
            parse_interface("10.1.2.1/24"),
            Some((Ipv4Addr::new(10, 1, 2, 1), 24))
        );
        assert_eq!(
            parse_interface("10.1.2.1"),
            Some((Ipv4Addr::new(10, 1, 2, 1), 32))
        );
    }
}
