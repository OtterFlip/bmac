// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Cluster status: what each host's collectors observe about that host
//! (`MONITORING.md`, "Collectors"), the conditions drawn from them
//! ("Conditions"), and the snapshots `GET /api/v1/cluster`, `/hosts`,
//! `/vms`, and `/storage` assemble from every host's newest observations.
//! The web app shows these as they are; it never works out cluster state
//! itself.
//!
//! Observations travel as [`crate::stream::StreamBody::Observation`], named
//! by one of the `*_COLLECTOR` constants, with the matching type as `data`.

use std::collections::BTreeMap;

use bmac_core::{GuestName, HostSlot, OperationId, Vmid};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const HOST_COLLECTOR: &str = "host";
pub const CLUSTER_COLLECTOR: &str = "cluster";
pub const GUESTS_COLLECTOR: &str = "guests";
pub const STORAGE_COLLECTOR: &str = "storage";
pub const UNITS_COLLECTOR: &str = "units";
pub const SMART_COLLECTOR: &str = "smart";
pub const TAILSCALE_COLLECTOR: &str = "tailscale";
pub const CLOCK_COLLECTOR: &str = "clock";
pub const CERTIFICATES_COLLECTOR: &str = "certificates";
pub const PMXCFS_COLLECTOR: &str = "pmxcfs";
pub const CLEANUP_COLLECTOR: &str = "cleanup";
pub const VRRP_COLLECTOR: &str = "vrrp";
pub const INGRESS_COLLECTOR: &str = "ingress";
pub const AGENTS_COLLECTOR: &str = "agents";
pub const QDEVICE_COLLECTOR: &str = "qdevice";
pub const REGISTRY_COLLECTOR: &str = "registry";
/// Not a collector: the host sending notifications reports on them.
pub const NOTIFY_COLLECTOR: &str = "notify";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ConditionSeverity {
    Info,
    Warning,
    Critical,
}

/// A suggested operation with prefilled values, or advice alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct NextStep {
    pub text: String,
    pub operation: Option<OperationId>,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

impl NextStep {
    pub fn run(text: impl Into<String>, operation: &str, args: &[(&str, &str)]) -> NextStep {
        NextStep {
            text: text.into(),
            operation: operation.parse().ok(),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
}

/// Something about one resource that someone may need to act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Condition {
    /// What kind, such as `zfs.pool.capacity`.
    pub id: String,
    /// What it is about, such as `mox2/rpool` or `prod3`.
    pub resource: String,
    pub severity: ConditionSeverity,
    pub message: String,
    /// When it was first seen in the observations it comes from.
    pub since_ms: i64,
    pub next_steps: Vec<NextStep>,
}

// ---------------------------------------------------------------------------
// Observations: each host about itself.

/// `host`: this host's load and versions, from `/proc` and `statvfs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct HostObservation {
    pub at_ms: i64,
    pub bmac_version: String,
    pub uptime_seconds: Option<u64>,
    pub cores: Option<u32>,
    /// Busy share of all cores since the previous observation, 0 to 1.
    pub cpu_fraction: Option<f64>,
    pub load1: Option<f64>,
    pub memory_used: Option<u64>,
    pub memory_total: Option<u64>,
    pub root_used: Option<u64>,
    pub root_total: Option<u64>,
}

/// `cluster`: quorum and the QDevice as this host sees them, from
/// `pvecm status` and `corosync.conf`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ClusterObservation {
    pub at_ms: i64,
    pub name: Option<String>,
    pub quorate: Option<bool>,
    pub expected_votes: Option<u32>,
    pub total_votes: Option<u32>,
    pub quorum: Option<u32>,
    /// `corosync.conf` has a QDevice `device` block.
    pub qdevice_registered: Option<bool>,
    /// The QDevice's address from that block.
    pub qdevice_address: Option<String>,
    /// `pvecm status` shows the QDevice alive and voting.
    pub qdevice_voting: Option<bool>,
    /// Every peer and its Corosync links, from `corosync-cfgtool -n`.
    #[serde(default)]
    pub peers: Vec<CorosyncPeer>,
    pub errors: Vec<String>,
}

/// One guest whose configuration is on this host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct GuestObservation {
    pub vmid: Vmid,
    pub name: Option<String>,
    /// `running`, `stopped`, and so on.
    pub status: Option<String>,
    pub cores: Option<u32>,
    pub memory_bytes: Option<u64>,
    pub disk_bytes: Option<u64>,
    pub uptime_seconds: Option<u64>,
    /// An HA resource exists for it.
    pub ha_configured: bool,
    /// The state HA was asked for, such as `started`.
    pub ha_requested: Option<String>,
    /// The state HA reports, such as `started` or `error`.
    pub ha_state: Option<String>,
}

/// A replication job whose source is this host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ReplicationObservation {
    /// `100-0`.
    pub id: String,
    pub guest: Vmid,
    pub target: String,
    pub schedule: Option<String>,
    pub last_sync_ms: Option<i64>,
    pub last_try_ms: Option<i64>,
    pub next_sync_ms: Option<i64>,
    pub duration_seconds: Option<f64>,
    pub fail_count: u32,
    pub error: Option<String>,
    pub disabled: bool,
}

/// `guests`: the guests on this host and the replication from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct GuestsObservation {
    pub at_ms: i64,
    pub guests: Vec<GuestObservation>,
    pub replication: Vec<ReplicationObservation>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Pool {
    pub name: String,
    pub health: Option<String>,
    pub size: Option<u64>,
    pub allocated: Option<u64>,
    pub free: Option<u64>,
    pub fragmentation_percent: Option<u64>,
    /// `free / size`.
    pub free_fraction: Option<f64>,
    /// The `scan:` line, while a scrub or resilver runs or after one.
    pub scan: Option<String>,
    /// The `remove:` line, while a vdev is evacuated.
    pub remove: Option<String>,
    pub errors: Option<String>,
}

/// A Proxmox storage as this host sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ProxmoxStorage {
    pub storage: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub active: bool,
    pub enabled: bool,
    pub total: Option<u64>,
    pub used: Option<u64>,
    pub avail: Option<u64>,
    pub content: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum VdevStatus {
    Online,
    Degraded,
    Faulted,
    Evacuating,
    Resilvering,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Encryption {
    Luks,
    None,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct VdevMember {
    pub path: String,
    pub state: Option<String>,
    pub disk: Option<String>,
    pub serial: Option<String>,
    pub model: Option<String>,
    pub size: Option<u64>,
    pub luks: bool,
    pub mapper: Option<String>,
    /// The member no longer resolves to an installed disk.
    pub missing: bool,
}

/// A top-level data vdev of an imported pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Vdev {
    pub pool: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub state: Option<String>,
    pub status: VdevStatus,
    pub encryption: Encryption,
    pub holds_esp: bool,
    pub size: Option<u64>,
    pub allocated: Option<u64>,
    pub free: Option<u64>,
    pub members: Vec<VdevMember>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum DiskStatus {
    Available,
    AwaitingFinalization,
    Evacuating,
    PendingReplacement,
    PendingAddition,
    InUse,
    NoSerial,
}

impl DiskStatus {
    pub fn label(self) -> &'static str {
        match self {
            DiskStatus::AwaitingFinalization => "Awaiting finalization",
            DiskStatus::Evacuating => "Evacuating",
            DiskStatus::PendingReplacement => "Replacement in progress",
            DiskStatus::PendingAddition => "New vdev in progress",
            DiskStatus::InUse => "In use",
            DiskStatus::NoSerial => "No serial",
            DiskStatus::Available => "Available",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum DiskContents {
    Blank,
    PartitionsOrSignatures,
    Mounted,
}

/// A physical disk in no pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Disk {
    pub disk: String,
    pub serial: Option<String>,
    pub model: Option<String>,
    pub size: Option<u64>,
    pub tran: Option<String>,
    pub contents: DiskContents,
    pub status: DiskStatus,
    pub label: String,
    pub detail: String,
    pub in_use_reasons: Vec<String>,
    /// Nothing holds the disk; it can be pulled physically.
    pub removable: bool,
}

/// `storage`: this host's pools, vdevs, free disks, and Proxmox storages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct StorageObservation {
    pub at_ms: i64,
    pub pools: Vec<Pool>,
    pub storages: Vec<ProxmoxStorage>,
    pub vdevs: Vec<Vdev>,
    pub disks: Vec<Disk>,
    /// Problems `zpool status` shows in a pool's members: states, error
    /// counts, data errors (v2 `show_cluster_health.sh` 692–743).
    #[serde(default)]
    pub findings: Vec<PoolFinding>,
    /// LUKS mappings that disagree with `/etc/crypttab` or rpool's members.
    #[serde(default)]
    pub luks_mismatches: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PoolFinding {
    pub pool: String,
    pub message: String,
}

/// A Corosync peer as this host's `corosync-cfgtool -n` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CorosyncPeer {
    pub node_id: u32,
    pub name: Option<HostSlot>,
    pub reachable: Option<bool>,
    /// Link number to whether it is connected.
    pub links: BTreeMap<u32, bool>,
}

/// `units`: the units every host must run, and any failed unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct UnitsObservation {
    pub at_ms: i64,
    /// Unit name to `systemctl is-active` state.
    pub required: BTreeMap<String, String>,
    pub failed: Vec<String>,
    pub errors: Vec<String>,
}

/// One disk's SMART state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SmartDisk {
    pub disk: String,
    pub serial: Option<String>,
    pub model: Option<String>,
    /// `passed`, `failing`, `unavailable`, `timed_out`, or `not_installed`.
    pub health: String,
    pub verdict: Option<String>,
    pub reallocated_sectors: Option<u64>,
    pub pending_sectors: Option<u64>,
    pub media_errors: Option<u64>,
}

/// `smart`: every physical disk's SMART health and wear counters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SmartObservation {
    pub at_ms: i64,
    pub disks: Vec<SmartDisk>,
    pub errors: Vec<String>,
}

/// `tailscale`: this host's Tailscale client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TailscaleObservation {
    pub at_ms: i64,
    /// `Running` when connected; `NeedsLogin`, `Stopped`, ... otherwise.
    pub backend_state: Option<String>,
    pub name: Option<String>,
    pub ipv4: Option<String>,
    pub errors: Vec<String>,
}

/// `clock`: time synchronization from `chronyc tracking`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ClockObservation {
    pub at_ms: i64,
    pub synchronized: Option<bool>,
    pub leap_status: Option<String>,
    pub offset_seconds: Option<f64>,
    pub reference: Option<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CertificateExpiry {
    /// `dashboard` or `proxmox`.
    pub name: String,
    pub path: String,
    pub not_after_ms: Option<i64>,
    pub error: Option<String>,
}

/// `certificates`: when this host's certificates expire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct CertificatesObservation {
    pub at_ms: i64,
    pub certificates: Vec<CertificateExpiry>,
}

/// `qdevice`: the registered QDevice as this host reaches it, without SSH
/// (v2 `show_cluster_health.sh` 556–583).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct QdeviceObservation {
    pub at_ms: i64,
    /// The QDevice's name on the tailnet, from the cluster's settings.
    pub name: Option<String>,
    /// The address in `corosync.conf`.
    pub registered_address: Option<String>,
    /// The address the tailnet gives that name.
    pub tailnet_address: Option<String>,
    /// Port 5403 at the registered address accepted a connection.
    pub qnetd_reachable: Option<bool>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RegistryIssue {
    /// `error` or `warning`.
    pub severity: String,
    /// v2's code, such as `vm-missing`.
    pub code: String,
    pub resource: String,
    pub message: String,
}

/// `registry`: where the registry disagrees with live Proxmox state. Only
/// the duty host makes this observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct RegistryObservation {
    pub at_ms: i64,
    pub issues: Vec<RegistryIssue>,
    pub errors: Vec<String>,
}

pub const LOGS_COLLECTOR: &str = "logs";

/// A peer's copy of this host's current stream that is falling behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PeerLag {
    pub peer: HostSlot,
    pub behind_records: u64,
    /// How old the oldest record the peer lacks is.
    pub behind_ms: i64,
}

/// `logs`: this host's record store and how far its peers have copied it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LogObservation {
    pub at_ms: i64,
    pub used_bytes: u64,
    pub budget_bytes: u64,
    pub oldest_ms: Option<i64>,
    /// When audit events were last evicted for space, if ever.
    pub audit_evicted_ms: Option<i64>,
    /// Online peers whose copy of this host's records lags.
    pub lagging: Vec<PeerLag>,
    /// Online peers whose holdings this host could read.
    pub peers_checked: u32,
}

/// `notify`: the state of email notifications, from the host that sends
/// them (the duty host, or each host without quorum).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct NotifyObservation {
    pub at_ms: i64,
    pub token_set: bool,
    pub sender_set: bool,
    /// Postmark refused the token (`ErrorCode` 10).
    pub unauthorized: bool,
    pub templates_missing: bool,
    /// Postmark refused the sender address (`ErrorCode` 400 or 401).
    pub sender_refused: bool,
    pub held: u32,
    pub inactive_recipients: Vec<String>,
    /// Since when every attempt has failed to reach Postmark.
    pub failing_since_ms: Option<i64>,
    pub last_error: Option<String>,
}

/// `pmxcfs`: how much of pmxcfs BMAC uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PmxcfsObservation {
    pub at_ms: i64,
    pub bmac_bytes: Option<u64>,
    pub errors: Vec<String>,
}

/// `cleanup`: deferred cleanups waiting on this host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct CleanupObservation {
    pub at_ms: i64,
    pub waiting: u32,
    /// When the oldest waiting cleanup was created.
    pub oldest_ms: Option<i64>,
    pub errors: Vec<String>,
}

/// `vrrp`: whether this host holds the guest-egress VIP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct VrrpObservation {
    pub at_ms: i64,
    pub vip: Option<String>,
    pub holds_vip: Option<bool>,
    pub keepalived: Option<String>,
    pub errors: Vec<String>,
}

/// `ingress`: whether this host applied the desired ingress generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct IngressObservation {
    pub at_ms: i64,
    /// This host serves ingress in the desired generation.
    pub serving: bool,
    pub desired_generation: Option<String>,
    pub applied_generation: Option<String>,
    pub state: Option<String>,
    pub haproxy: Option<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GuestAgent {
    pub vmid: Vmid,
    pub name: GuestName,
    pub answering: bool,
    pub error: Option<String>,
}

/// `agents`: the guest agent of each running production VM on this host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct AgentsObservation {
    pub at_ms: i64,
    pub agents: Vec<GuestAgent>,
    pub errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// Snapshots: the cluster as one host knows it.

/// How fresh one host's part of a snapshot is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Freshness {
    /// When its newest observation used here was made.
    pub observed_at_ms: Option<i64>,
    /// Its host-to-host stream is up, or it is the host answering.
    pub reachable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ClusterSummary {
    pub name: Option<String>,
    pub quorate: Option<bool>,
    pub expected_votes: Option<u32>,
    pub total_votes: Option<u32>,
    pub quorum: Option<u32>,
    pub host_count: u32,
    pub online_count: u32,
    /// The lowest-numbered online host of the quorate partition.
    pub duty_host: Option<HostSlot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct QdeviceSummary {
    pub registered: Option<bool>,
    pub address: Option<String>,
    /// An even number of hosts needs one; an odd number must not have one.
    pub needed: Option<bool>,
    pub voting: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct HostRow {
    pub name: HostSlot,
    /// Online in the cluster membership.
    pub online: bool,
    pub address: Option<String>,
    /// The registry's host slot state, such as `member` or `joining`.
    pub slot_state: Option<String>,
    pub is_duty: bool,
    /// The host this snapshot came from.
    pub is_serving: bool,
    pub freshness: Freshness,
    pub observation: Option<HostObservation>,
}

/// `GET /api/v1/hosts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct HostsSnapshot {
    pub at_ms: i64,
    pub served_by: Option<HostSlot>,
    pub cluster: ClusterSummary,
    pub qdevice: QdeviceSummary,
    pub hosts: Vec<HostRow>,
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct HaSummary {
    pub configured: bool,
    pub requested_state: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ReplicationSummary {
    pub jobs: u32,
    pub failing: u32,
    pub targets: Vec<String>,
    pub oldest_last_sync_ms: Option<i64>,
    pub errors: Vec<String>,
}

/// What production and staging rows share.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct GuestCommon {
    pub name: GuestName,
    pub vmid: Vmid,
    /// The registry state, such as `active`.
    pub registry_state: String,
    /// `running`, `stopped`, or `absent` when no host has it.
    pub live_status: String,
    /// The host it runs on.
    pub node: Option<HostSlot>,
    pub ip: String,
    pub placement: Vec<HostSlot>,
    pub routes_enabled: bool,
    pub domain: Option<String>,
    pub cores: u32,
    pub memory_mb: u64,
    /// The current disk size; larger than at creation after an online
    /// growth.
    pub disk_bytes: u64,
    pub uptime_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ProductionVm {
    #[serde(flatten)]
    pub common: GuestCommon,
    pub purpose: String,
    pub aliases: Vec<String>,
    /// The registry's owner, which should be where it runs.
    pub owner_node: Option<HostSlot>,
    pub ha: HaSummary,
    pub replication: ReplicationSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct StagingSnapshot {
    pub name: String,
    /// From the UTC stamp at the end of the snapshot name.
    pub created_at_ms: Option<i64>,
    pub verified: bool,
    /// Other staging VMs cloned from it; it is deleted with the last one.
    pub shared_with: Vec<GuestName>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct StagingVm {
    #[serde(flatten)]
    pub common: GuestCommon,
    pub source: Option<GuestName>,
    pub url: Option<String>,
    pub snapshot: Option<StagingSnapshot>,
}

/// A QEMU guest the registry doesn't know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UnregisteredGuest {
    pub vmid: Vmid,
    pub name: Option<String>,
    pub node: HostSlot,
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ReplicationJob {
    #[serde(flatten)]
    pub job: ReplicationObservation,
    pub source: HostSlot,
    pub guest_name: Option<GuestName>,
}

/// `GET /api/v1/vms`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct VmsSnapshot {
    pub at_ms: i64,
    pub served_by: Option<HostSlot>,
    pub production: Vec<ProductionVm>,
    pub staging: Vec<StagingVm>,
    pub unregistered: Vec<UnregisteredGuest>,
    pub replication: Vec<ReplicationJob>,
    /// Hosts whose guests are missing or old, because their stream is down.
    pub stale_hosts: Vec<HostSlot>,
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct HostStorage {
    pub host: HostSlot,
    pub online: bool,
    pub freshness: Freshness,
    pub observation: Option<StorageObservation>,
}

/// `GET /api/v1/storage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct StorageSnapshot {
    pub at_ms: i64,
    pub served_by: Option<HostSlot>,
    pub hosts: Vec<HostStorage>,
    pub conditions: Vec<Condition>,
}

/// `GET /api/v1/cluster`: the cluster at a glance, and every warning and
/// critical condition for the "Needs attention" panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ClusterSnapshot {
    pub at_ms: i64,
    pub served_by: Option<HostSlot>,
    pub cluster: ClusterSummary,
    pub qdevice: QdeviceSummary,
    pub conditions: Vec<Condition>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn production_rows_flatten_the_common_fields() {
        let vm = ProductionVm {
            common: GuestCommon {
                name: "prod1".parse().unwrap(),
                vmid: Vmid::new(101).unwrap(),
                registry_state: "active".into(),
                live_status: "running".into(),
                node: Some(HostSlot::new(1).unwrap()),
                ip: "10.10.0.101".into(),
                placement: vec![HostSlot::new(1).unwrap(), HostSlot::new(2).unwrap()],
                routes_enabled: true,
                domain: Some("shop.example.com".into()),
                cores: 4,
                memory_mb: 8192,
                disk_bytes: 64 << 30,
                uptime_seconds: None,
            },
            purpose: "shop".into(),
            aliases: vec![],
            owner_node: Some(HostSlot::new(1).unwrap()),
            ha: HaSummary {
                configured: true,
                requested_state: Some("started".into()),
                state: Some("started".into()),
            },
            replication: ReplicationSummary {
                jobs: 1,
                failing: 0,
                targets: vec!["mox2".into()],
                oldest_last_sync_ms: Some(5),
                errors: vec![],
            },
        };
        let value = serde_json::to_value(&vm).unwrap();
        assert_eq!(value["name"], "prod1");
        assert_eq!(value["placement"], json!(["mox1", "mox2"]));
        assert_eq!(
            value["uptime_seconds"],
            json!(null),
            "absent values are null"
        );
        let back: ProductionVm = serde_json::from_value(value).unwrap();
        assert_eq!(back, vm);
    }

    #[test]
    fn next_steps_name_an_operation() {
        let step = NextStep::run(
            "Inspect prod1",
            "diagnostics.prod_vm_state",
            &[("resource", "prod1")],
        );
        assert_eq!(
            serde_json::to_value(&step).unwrap(),
            json!({"text": "Inspect prod1", "operation": "diagnostics.prod_vm_state", "args": {"resource": "prod1"}})
        );
    }
}
