// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Guest resource records (`validate_resource`, `cluster_registry.py`
//! 1559–1780) and the cross-record invariants
//! (`validate_registry_invariants`, 1783–1895).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::Ipv4Addr;

use bmac_core::{GuestName, HostSlot, Vmid};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::check::{
    GUID_RE, MAC_RE, PURPOSE_RE, RESOURCE_RE, exact_keys, fail, int, is_uuid, node, node_list,
    normalize_domain, py_str, repr, safe_id, snapshot_name, str_of, timestamp, typed,
};
use crate::policy::{Policy, Spec, validate_spec};
use crate::state::{Kind, ResourceState};
use crate::{MAX_ROUTES, Result, SCHEMA_VERSION};

pub const RESOURCE_KEYS: &[&str] = &[
    "schema_version",
    "record_type",
    "id",
    "allocation_id",
    "request_fingerprint",
    "kind",
    "name",
    "index",
    "source",
    "vmid",
    "ip",
    "mac",
    "state",
    "purpose",
    "domains",
    "placement",
    "initial_node",
    "owner_node",
    "routes_enabled",
    "spec",
    "proxmox",
    "created_at",
    "updated_at",
    "revision",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Purpose {
    pub slug: String,
    pub source_resource: Option<GuestName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Domains {
    pub primary: String,
    pub aliases: Vec<String>,
    pub staging_base: String,
}

/// A ZFS GUID, which v2 accepts as text or a number and keeps as text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Guid(pub String);

impl<'de> Deserialize<'de> for Guid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Guid, D::Error> {
        Ok(Guid(py_str(&Value::deserialize(deserializer)?)))
    }
}

/// The snapshot a guest depends on. Production records carry only the first
/// three fields; staging records carry all of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub name: String,
    pub owner_node: HostSlot,
    pub guids: BTreeMap<HostSlot, Guid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_resource: Option<GuestName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_volume_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_volume_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_guids: Option<BTreeMap<String, BTreeMap<HostSlot, Guid>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependent_resources: Option<Vec<GuestName>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refcount: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxmoxMeta {
    pub ha_nodes: Vec<HostSlot>,
    pub replication_targets: Vec<HostSlot>,
    pub volume_id: Option<String>,
    pub snapshot: Option<Snapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    pub schema_version: u64,
    pub record_type: String,
    pub id: String,
    pub allocation_id: Option<String>,
    pub request_fingerprint: String,
    pub kind: Kind,
    pub name: GuestName,
    pub index: u32,
    pub source: Option<GuestName>,
    pub vmid: Vmid,
    pub ip: Ipv4Addr,
    pub mac: String,
    pub state: ResourceState,
    pub purpose: Purpose,
    pub domains: Domains,
    pub placement: Vec<HostSlot>,
    pub initial_node: HostSlot,
    pub owner_node: Option<HostSlot>,
    pub routes_enabled: bool,
    pub spec: Spec,
    pub proxmox: ProxmoxMeta,
    pub created_at: String,
    pub updated_at: String,
    pub revision: u64,
}

impl Resource {
    /// `resource_sort_key`: production first, then by number.
    pub fn sort_key(&self) -> (u8, u32, String) {
        let (rank, number) = match self.name {
            GuestName::Production { number } => (0, number),
            GuestName::Staging { number, .. } => (1, number),
        };
        (rank, number, self.name.to_string())
    }
}

fn normalize_domains(values: &Value) -> Result<Vec<String>> {
    let Some(values) = values.as_array() else {
        return fail("resource aliases are not normalized and unique");
    };
    let mut result: Vec<String> = Vec::new();
    for value in values {
        let Some(text) = value.as_str() else {
            return fail("resource aliases are not normalized and unique");
        };
        let domain = normalize_domain(text)?;
        if result.contains(&domain) {
            return fail(format!("duplicate domain in request: {domain}"));
        }
        result.push(domain);
    }
    Ok(result)
}

fn allocation_id(value: &Value) -> Result<()> {
    match value {
        Value::Null => Ok(()),
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() || text.len() > 128 || !crate::check::SAFE_ID_RE.is_match(text) {
                return fail("allocation id contains unsafe characters");
            }
            Ok(())
        }
        _ => fail("allocation id contains unsafe characters"),
    }
}

fn purpose_slug(value: &Value) -> Result<()> {
    let purpose = value
        .as_str()
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    if purpose.len() > 63 || !PURPOSE_RE.is_match(&purpose) {
        return fail(
            "purpose must be a lowercase slug of at most 63 characters (letters, digits, and internal hyphens)",
        );
    }
    Ok(())
}

pub(crate) fn vmid(value: &Value, policy: &Policy) -> Result<()> {
    let Some(vmid) = int(value).filter(|v| (100..=999_999_999).contains(v)) else {
        return fail("VMID must be between 100 and 999999999");
    };
    let reserved = policy.network.haproxy_vmid_range;
    if (i128::from(reserved.start)..=i128::from(reserved.end)).contains(&vmid) {
        return fail(format!(
            "VMID {vmid} is reserved for fixed HAProxy LXCs ({}-{})",
            reserved.start, reserved.end
        ));
    }
    Ok(())
}

pub fn validate_resource(resource: &Value, policy: &Policy) -> Result<Resource> {
    if !resource.is_object() {
        return fail("resource record must be a JSON object");
    }
    exact_keys(resource, RESOURCE_KEYS, "resource record")?;
    if resource["schema_version"] != SCHEMA_VERSION || resource["record_type"] != "resource" {
        return fail("unsupported resource schema");
    }
    if !is_uuid(&resource["id"]) {
        return fail("resource id is not a UUID");
    }
    allocation_id(&resource["allocation_id"])?;
    if !resource["request_fingerprint"]
        .as_str()
        .is_some_and(|f| crate::check::SHA256_RE.is_match(f))
    {
        return fail("resource request fingerprint is invalid");
    }
    let kind = match resource["kind"].as_str() {
        Some("production") => Kind::Production,
        Some("staging") => Kind::Staging,
        _ => return fail("resource kind must be production or staging"),
    };
    let index = int(&resource["index"]).filter(|n| *n > 0);
    let Some(index) = index else {
        return fail("resource index must be a positive integer");
    };
    let name = str_of(&resource["name"]);
    let Some(captures) = RESOURCE_RE.captures(name) else {
        return fail(format!(
            "invalid resource name: {}",
            repr(&resource["name"])
        ));
    };
    vmid(&resource["vmid"], policy)?;
    let Some(ip) = resource["ip"]
        .as_str()
        .and_then(|ip| ip.parse::<Ipv4Addr>().ok())
    else {
        return fail("resource IP is invalid");
    };
    let Some(octet) = policy.network.offset(ip) else {
        return fail("resource IP is outside policy network");
    };
    if !resource["mac"].as_str().is_some_and(|m| MAC_RE.is_match(m)) {
        return fail("resource MAC is invalid or not normalized");
    }
    let Some(state) = resource["state"]
        .as_str()
        .and_then(ResourceState::from_text)
    else {
        return fail(format!(
            "unknown resource state: {}",
            repr(&resource["state"])
        ));
    };
    if !int(&resource["revision"]).is_some_and(|r| r >= 1) {
        return fail("resource revision must be positive");
    }
    timestamp(&resource["created_at"])?;
    timestamp(&resource["updated_at"])?;

    let purpose = &resource["purpose"];
    exact_keys(purpose, &["slug", "source_resource"], "resource purpose")?;
    purpose_slug(&purpose["slug"])?;

    let domains = &resource["domains"];
    exact_keys(
        domains,
        &["primary", "aliases", "staging_base"],
        "resource domains",
    )?;
    let primary = normalize_domain(str_of(&domains["primary"]))?;
    if domains["primary"] != primary.as_str() {
        return fail("resource primary domain is not normalized");
    }
    let aliases = normalize_domains(&domains["aliases"])?;
    if domains["aliases"] != Value::from(aliases.clone()) || aliases.contains(&primary) {
        return fail("resource aliases are not normalized and unique");
    }
    let staging_base = normalize_domain(str_of(&domains["staging_base"]))?;
    if domains["staging_base"] != staging_base.as_str() {
        return fail("resource staging base is not normalized");
    }
    if staging_base == primary || aliases.contains(&staging_base) {
        return fail("resource staging base must differ from routed domains");
    }

    let placement = node_list(&resource["placement"], policy, false, "resource placement")?;
    let member = |value: &Value| {
        value
            .as_str()
            .and_then(|n| n.parse::<HostSlot>().ok())
            .filter(|s| placement.contains(s))
    };
    if member(&resource["initial_node"]).is_none() {
        return fail("resource initial node must belong to placement");
    }
    if !resource["owner_node"].is_null() && member(&resource["owner_node"]).is_none() {
        return fail("resource owner node must belong to placement");
    }
    let Some(routes_enabled) = resource["routes_enabled"].as_bool() else {
        return fail("routes_enabled must be boolean");
    };
    if routes_enabled && !state.routable() {
        return fail("routes may only be enabled for stopped, ready, or active guests");
    }
    validate_spec(&resource["spec"])?;

    let proxmox = &resource["proxmox"];
    exact_keys(
        proxmox,
        &["ha_nodes", "replication_targets", "volume_id", "snapshot"],
        "resource proxmox metadata",
    )?;
    let what = "Proxmox node metadata";
    let ha_nodes = node_list(&proxmox["ha_nodes"], policy, true, what)?;
    let replication_targets = node_list(&proxmox["replication_targets"], policy, true, what)?;
    if ha_nodes
        .iter()
        .chain(&replication_targets)
        .any(|n| !placement.contains(n))
    {
        return fail("Proxmox HA/replication metadata exceeds placement");
    }
    if !proxmox["volume_id"].is_null() {
        safe_id(&proxmox["volume_id"], "volume id")?;
    }
    let snapshot = &proxmox["snapshot"];
    if !snapshot.is_null() {
        validate_snapshot(snapshot, resource, kind, &placement, policy)?;
    }

    if kind == Kind::Production {
        let Some(number) = captures.name("production") else {
            return fail("production resource has staging identity");
        };
        if !resource["source"].is_null() {
            return fail("production resource has staging identity");
        }
        if number.as_str().parse::<i128>().ok() != Some(index) {
            return fail("production resource index does not match its name");
        }
        let range = policy.network.production_range;
        let expected = i128::from(range.start) + index - 1;
        if expected > i128::from(range.end) || i128::from(octet) != expected {
            return fail("production resource name/IP formula is invalid");
        }
        if !purpose["source_resource"].is_null() {
            return fail("production purpose may not have a source resource");
        }
    } else {
        let (Some(stage), Some(source)) = (captures.name("stage"), captures.name("source")) else {
            return fail("staging source/name relationship is invalid");
        };
        if resource["source"] != source.as_str() {
            return fail("staging source/name relationship is invalid");
        }
        if stage.as_str().parse::<i128>().ok() != Some(index) {
            return fail("staging index does not match its name");
        }
        if !policy.network.staging_range.contains(octet) {
            return fail("staging IP is outside its configured range");
        }
        if purpose["source_resource"] != resource["source"] {
            return fail("staging purpose source does not match source resource");
        }
        if placement.len() != 1 {
            return fail("staging placement must contain exactly one node");
        }
        if !ha_nodes.is_empty() {
            return fail("staging resources may not have HA nodes");
        }
    }
    typed(resource, &format!("resource {name}"))
}

fn validate_snapshot(
    snapshot: &Value,
    resource: &Value,
    kind: Kind,
    placement: &[HostSlot],
    policy: &Policy,
) -> Result<()> {
    if !snapshot.is_object() {
        return fail("snapshot metadata must be an object or null");
    }
    let mut keys = vec!["name", "owner_node", "guids"];
    if kind == Kind::Staging {
        keys.extend([
            "source_resource",
            "source_volume_id",
            "source_volume_ids",
            "volume_guids",
            "verified",
            "dependent_resources",
            "refcount",
        ]);
    }
    exact_keys(snapshot, &keys, "snapshot metadata")?;
    let Some(name) = snapshot["name"].as_str() else {
        return fail("snapshot name is unsafe or uses Proxmox's reserved __replicate_ prefix");
    };
    snapshot_name(name)?;
    let owner = node(&snapshot["owner_node"], policy)?;
    if kind == Kind::Production && !placement.contains(&owner) {
        return fail("snapshot owner node must belong to placement");
    }
    let Some(guids) = snapshot["guids"].as_object() else {
        return fail("snapshot GUIDs must be an object");
    };
    for (slot, guid) in guids {
        let slot = node(&Value::from(slot.as_str()), policy)?;
        if (kind == Kind::Production && !placement.contains(&slot))
            || !GUID_RE.is_match(&py_str(guid))
        {
            return fail("snapshot GUID metadata is invalid");
        }
    }
    if kind == Kind::Production {
        return Ok(());
    }
    if snapshot["source_resource"] != resource["source"] {
        return fail("staging snapshot source must match the resource source");
    }
    safe_id(&snapshot["source_volume_id"], "snapshot source volume id")?;
    let volume_ids = snapshot["source_volume_ids"].as_array();
    let unique = volume_ids.is_some_and(|ids| {
        let set: BTreeSet<String> = ids.iter().map(Value::to_string).collect();
        !ids.is_empty() && set.len() == ids.len()
    });
    let Some(volume_ids) = volume_ids.filter(|_| unique) else {
        return fail("staging snapshot source volumes must be a non-empty unique array");
    };
    for id in volume_ids {
        safe_id(id, "snapshot source volume id")?;
    }
    if !volume_ids.contains(&snapshot["source_volume_id"]) {
        return fail("staging root source volume must be among snapshotted volumes");
    }
    let Some(verified) = snapshot["verified"].as_bool() else {
        return fail("staging snapshot verified flag must be boolean");
    };
    let Some(volume_guids) = snapshot["volume_guids"].as_object() else {
        return fail("staging snapshot volume GUIDs must be an object");
    };
    if verified {
        let listed: BTreeSet<&str> = volume_ids.iter().filter_map(Value::as_str).collect();
        if volume_guids
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            != listed
        {
            return fail("verified staging snapshot GUIDs must cover every source volume");
        }
        for (volume, node_guids) in volume_guids {
            let Some(node_guids) = node_guids.as_object().filter(|m| !m.is_empty()) else {
                return fail(format!(
                    "snapshot GUIDs for {volume} must be a non-empty object"
                ));
            };
            let mut distinct = BTreeSet::new();
            for (slot, guid) in node_guids {
                node(&Value::from(slot.as_str()), policy)?;
                if !GUID_RE.is_match(&py_str(guid)) {
                    return fail(format!("snapshot GUID metadata is invalid for {volume}"));
                }
                distinct.insert(guid.to_string());
            }
            if distinct.len() != 1 {
                return fail(format!("snapshot GUID differs between nodes for {volume}"));
            }
        }
        if Some(&snapshot["guids"]) != volume_guids.get(str_of(&snapshot["source_volume_id"])) {
            return fail("root snapshot GUIDs must match the root volume GUID map");
        }
    } else if !guids.is_empty() || !volume_guids.is_empty() {
        return fail("unverified staging snapshot intent may not claim GUIDs");
    }
    if snapshot["dependent_resources"] != Value::from(vec![resource["name"].clone()]) {
        return fail("a unique staging snapshot must name its one dependent resource");
    }
    if int(&snapshot["refcount"]) != Some(1) {
        return fail("a unique staging snapshot refcount must be exactly one");
    }
    Ok(())
}

/// The part of a staging snapshot record every sharing resource repeats.
fn shared_identity(snapshot: &Snapshot) -> Snapshot {
    Snapshot {
        dependent_resources: None,
        refcount: None,
        ..snapshot.clone()
    }
}

pub fn validate_registry_invariants(resources: &[Resource]) -> Result<()> {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut vmids = HashMap::new();
    let mut ips = HashMap::new();
    let mut macs: HashMap<&str, String> = HashMap::new();
    let mut domains: HashMap<&str, String> = HashMap::new();
    let mut allocations: HashMap<&str, String> = HashMap::new();
    let mut purposes: HashMap<&str, String> = HashMap::new();
    let mut by_name: HashMap<GuestName, &Resource> = HashMap::new();
    for resource in resources {
        let name = resource.name.to_string();
        by_name.insert(resource.name, resource);
        if let Some(prior) = names.insert(name.clone(), name.clone()) {
            return fail(format!("duplicate name '{name}': {prior} and {name}"));
        }
        if let Some(prior) = vmids.insert(resource.vmid, name.clone()) {
            return fail(format!(
                "duplicate vmid {}: {prior} and {name}",
                resource.vmid
            ));
        }
        if let Some(prior) = ips.insert(resource.ip, name.clone()) {
            return fail(format!(
                "duplicate ip '{}': {prior} and {name}",
                resource.ip
            ));
        }
        if let Some(prior) = macs.insert(&resource.mac, name.clone()) {
            return fail(format!(
                "duplicate mac '{}': {prior} and {name}",
                resource.mac
            ));
        }
        if let Some(id) = &resource.allocation_id
            && let Some(prior) = allocations.insert(id, name.clone())
        {
            return fail(format!(
                "duplicate allocation id '{id}': {prior} and {name}"
            ));
        }
        if resource.kind == Kind::Production
            && let Some(prior) = purposes.insert(&resource.purpose.slug, name.clone())
        {
            return fail(format!(
                "duplicate production purpose '{}': {prior} and {name}",
                resource.purpose.slug
            ));
        }
        for domain in std::iter::once(&resource.domains.primary).chain(&resource.domains.aliases) {
            if let Some(prior) = domains.insert(domain, name.clone()) {
                return fail(format!("duplicate domain '{domain}': {prior} and {name}"));
            }
        }
    }
    if domains.len() > MAX_ROUTES {
        return fail(format!(
            "registry route count exceeds renderer limit of {MAX_ROUTES}"
        ));
    }
    let mut snapshot_keys: HashMap<(GuestName, &str), GuestName> = HashMap::new();
    for resource in resources {
        if resource.kind != Kind::Staging {
            continue;
        }
        let source_name = resource.source.map(|s| s.to_string()).unwrap_or_default();
        let Some(source) = resource.source.and_then(|s| by_name.get(&s)) else {
            return fail(format!(
                "staging resource {} has missing source {source_name}",
                resource.name
            ));
        };
        if source.kind != Kind::Production {
            return fail(format!(
                "staging resource {} source is not production",
                resource.name
            ));
        }
        let Some(snapshot) = &resource.proxmox.snapshot else {
            continue;
        };
        if snapshot.source_resource != Some(source.name) {
            return fail("staging snapshot dependency source is inconsistent");
        }
        if snapshot.source_volume_id != source.proxmox.volume_id {
            return fail("staging snapshot dependency source volume differs from production");
        }
        if !source.placement.contains(&snapshot.owner_node) {
            return fail("staging snapshot owner is outside production placement");
        }
        for volume in snapshot.source_volume_ids.iter().flatten() {
            if volume.split(':').next().unwrap_or("").is_empty() {
                return fail("staging snapshot source volume lacks storage id");
            }
        }
        let placement: BTreeSet<HostSlot> = source.placement.iter().copied().collect();
        let verified = snapshot.verified == Some(true);
        if verified {
            if snapshot.guids.keys().copied().collect::<BTreeSet<_>>() != placement {
                return fail("staging snapshot GUIDs must cover every production placement node");
            }
            for (volume, node_guids) in snapshot.volume_guids.iter().flatten() {
                if node_guids.keys().copied().collect::<BTreeSet<_>>() != placement {
                    return fail(format!(
                        "snapshot GUIDs for {volume} must cover production placement"
                    ));
                }
            }
        } else if !matches!(
            resource.state,
            ResourceState::Snapshotting
                | ResourceState::Replicating
                | ResourceState::CleanupPending
                | ResourceState::Failed
        ) {
            return fail("unverified staging snapshot intent is invalid in the current state");
        }
        let key = (source.name, snapshot.name.as_str());
        if let Some(prior) = snapshot_keys.get(&key) {
            let prior_snapshot = by_name[prior]
                .proxmox
                .snapshot
                .as_ref()
                .expect("recorded with a snapshot");
            if !(verified && prior_snapshot.verified == Some(true)) {
                return fail(format!(
                    "staging snapshot '{}' is claimed by {prior} and {} before verification; \
                     only a verified snapshot may be shared",
                    snapshot.name, resource.name
                ));
            }
            if shared_identity(snapshot) != shared_identity(prior_snapshot) {
                return fail(format!(
                    "staging snapshot '{}' is shared by {prior} and {} with differing metadata",
                    snapshot.name, resource.name
                ));
            }
            continue;
        }
        snapshot_keys.insert(key, resource.name);
    }
    Ok(())
}
