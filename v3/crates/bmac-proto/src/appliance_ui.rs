// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The appliance's own HTTPS UI on port 8443 (`BACKUP.md`, "Appliance
//! platform"): the administrator logs in with the appliance password, and
//! the token lives only in page memory, as in the Web Dashboard. Every path
//! is under [`APPLIANCE_API`].

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const APPLIANCE_API: &str = "/api/v1";

/// `GET /hello`, without a login: enough to say whether to log in or to
/// unlock at the console first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApplianceHello {
    pub name: String,
    pub appliance_uuid: String,
    pub version: String,
    /// The pool waits for `bmac-backupd unlock` at the console; nobody can
    /// log in until then, since the password is kept on the pool.
    pub locked: bool,
    /// No administrator password is set yet (`bmac-backupd admin-password`).
    pub no_password: bool,
    /// The appliance certificate's SHA-256, colon-separated.
    pub fingerprint: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApplianceLogin {
    pub password: String,
}

impl std::fmt::Debug for ApplianceLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplianceLogin").finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApplianceSession {
    pub token: String,
    pub idle_timeout_seconds: u32,
    pub expires_at_ms: i64,
}

impl std::fmt::Debug for ApplianceSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplianceSession")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ChangePassword {
    pub current: String,
    pub new: String,
}

impl std::fmt::Debug for ChangePassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangePassword").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PoolView {
    pub name: String,
    pub guid: String,
    pub health: String,
    pub size_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ClusterView {
    pub cluster_uuid: String,
    pub cluster_name: String,
    /// This appliance's state as the cluster last said, such as `ACTIVE`.
    pub state: Option<String>,
    pub last_checkin_ms: Option<i64>,
    pub last_error: Option<String>,
    pub offers: u32,
    pub pulling: Option<String>,
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LiveTransfer {
    pub cluster_uuid: String,
    pub vm_uuid: String,
    pub pit_uuid: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ScrubView {
    /// `running`, `paused`, `finished`, or `canceled`.
    pub state: String,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub done_permille: Option<u32>,
    pub repaired_bytes: u64,
    pub errors: u64,
    pub affected: Vec<String>,
    pub suspect_pits: Vec<String>,
    pub next_due_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct WatchView {
    /// `cluster` or `url`.
    pub kind: String,
    pub subject: String,
    /// Set while it gives no good answer.
    pub down_since_ms: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RetentionCounts {
    pub daily: u32,
    pub weekly: u32,
    pub monthly: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PendingRetention {
    pub policy: RetentionCounts,
    pub effective_at_ms: i64,
    pub by: String,
}

/// `GET /status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApplianceStatusView {
    pub hello: ApplianceHello,
    pub pool: Option<PoolView>,
    pub clusters: Vec<ClusterView>,
    pub transfers: Vec<LiveTransfer>,
    pub pits: u32,
    pub scrub: Option<ScrubView>,
    pub watch: Vec<WatchView>,
    /// Why the watchdog's email is stopped, if it is.
    pub notify_error: Option<String>,
    pub pending_retention: Option<PendingRetention>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PitView {
    pub cluster_uuid: String,
    pub cluster_name: String,
    pub vm_uuid: String,
    pub branch_uuid: String,
    pub pit_uuid: String,
    pub parent: Option<String>,
    pub name: String,
    pub taken_s: i64,
    pub published_at_ms: i64,
    pub bytes: u64,
    pub volumes: u32,
    /// The PIT it was received on top of, if any.
    pub received_from: Option<String>,
    pub held: bool,
    pub suspect: bool,
    /// A live restore grant names it.
    pub granted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ReceivingView {
    pub cluster_uuid: String,
    pub vm_uuid: String,
    pub pit_uuid: String,
    pub started_at_ms: i64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct QuarantinedView {
    pub cluster_uuid: String,
    pub vm_uuid: String,
    pub pit_uuid: String,
    pub reason: String,
    pub at_ms: i64,
}

/// `GET /pits`: the PIT browser and graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PitsView {
    pub pits: Vec<PitView>,
    pub receiving: Vec<ReceivingView>,
    pub quarantined: Vec<QuarantinedView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TransferRecord {
    pub at_ms: i64,
    pub cluster_uuid: String,
    pub vm_uuid: String,
    pub pit_uuid: String,
    pub bytes: u64,
    pub duration_ms: u64,
    pub from_peer: Option<String>,
}

/// `GET /throughput`: finished pulls from the audit log, newest last, and
/// the ones running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ThroughputView {
    pub recent: Vec<TransferRecord>,
    pub live: Vec<LiveTransfer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct HoldView {
    pub cluster_uuid: String,
    pub pit_uuid: String,
    pub by: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CoverageView {
    pub cluster_uuid: String,
    pub vm_uuid: String,
    pub pits: u32,
    pub oldest_s: i64,
    pub newest_s: i64,
}

/// `GET /retention`: the policy and what a prune would do now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApplianceRetentionView {
    pub policy: RetentionCounts,
    /// The least the UI may set; changed only at the console.
    pub floor: RetentionCounts,
    pub delay_hours: u32,
    pub pending: Option<PendingRetention>,
    pub holds: Vec<HoldView>,
    pub prune: Vec<String>,
    pub reclaim_bytes: u64,
    pub coverage: Vec<CoverageView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SetRetention {
    pub policy: RetentionCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RetentionChange {
    /// Applied now, or delayed until `effective_at_ms` because it keeps less.
    pub applied: bool,
    pub effective_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SetHold {
    pub pit_uuid: String,
    pub hold: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PruneResult {
    pub pruned: u32,
    pub reclaim_bytes: u64,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GrantView {
    pub grant_id: String,
    pub cluster_uuid: String,
    pub pits: Vec<String>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub by: String,
    /// `live`, `revoked`, or `ended`.
    pub state: String,
    pub last_used_ms: Option<i64>,
    /// `restore`, or `recovery` for a cluster rebuilt on new hardware.
    pub kind: String,
    /// A recovery grant's host certificate, SHA-256.
    pub host_fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct IssueGrant {
    pub cluster_uuid: String,
    pub hours: u32,
    pub pits: Vec<String>,
}

/// A recovery grant: the namespace's recovery manifest and every PIT of
/// it, for the host of a rebuilt cluster whose certificate has
/// `host_fingerprint`, which may then bind the namespace to that cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct IssueRecoveryGrant {
    pub cluster_uuid: String,
    pub hours: u32,
    pub host_fingerprint: String,
}

/// The token is shown once and kept only as a hash.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct IssuedGrant {
    pub grant: GrantView,
    pub token: String,
}

impl std::fmt::Debug for IssuedGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedGrant")
            .field("grant", &self.grant)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RevokeGrant {
    pub grant_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PeerView {
    pub appliance_uuid: String,
    pub fingerprint: String,
    pub address: String,
    pub added_at_ms: i64,
    pub by: String,
    pub last_served_ms: Option<i64>,
    /// Whether it accepted this appliance's last handshake.
    pub trusts_us: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RefusalView {
    pub fingerprint: String,
    /// The name its certificate claimed, which proves nothing.
    pub claimed: String,
    pub from: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PeersView {
    /// This appliance's fingerprint, to read out to a peer's administrator.
    pub fingerprint: String,
    pub peers: Vec<PeerView>,
    pub refusals: Vec<RefusalView>,
    pub reminder: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct TrustPeer {
    pub appliance_uuid: String,
    /// Typed from the peer's own UI or console.
    pub fingerprint: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TrustResult {
    pub trusted: bool,
    /// What the address presented when it didn't match.
    pub presented: Option<String>,
    pub message: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct UntrustPeer {
    pub appliance_uuid: String,
    /// The administrator's password again.
    pub password: String,
}

impl std::fmt::Debug for UntrustPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UntrustPeer")
            .field("appliance_uuid", &self.appliance_uuid)
            .finish_non_exhaustive()
    }
}

/// `GET /settings`: scrubs, the I/O policy, the watchdog, and its email.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PlatformView {
    pub scrub_every_days: u32,
    /// `pulls_first`, `scrub_first`, or `together`.
    pub io_policy: String,
    pub down_after_minutes: u32,
    pub urls: Vec<String>,
    pub from: String,
    pub to: Vec<String>,
    pub token_set: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SavePlatform {
    pub scrub_every_days: u32,
    pub io_policy: String,
    pub down_after_minutes: u32,
    pub urls: Vec<String>,
    pub from: String,
    pub to: Vec<String>,
}

/// A new Postmark server token, or `null` to remove it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SetApplianceToken {
    pub token: Option<String>,
}

impl std::fmt::Debug for SetApplianceToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetApplianceToken")
            .field("set", &self.token.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApplianceTestEmail {
    pub sent: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct AuditEntryView {
    pub at_ms: i64,
    pub event: String,
    pub by: String,
    #[ts(type = "unknown")]
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct AuditView {
    pub entries: Vec<AuditEntryView>,
}

/// `GET /recovery`: what this appliance keeps to rebuild each cluster, and
/// the restore tests the cluster reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RecoveryView {
    pub clusters: Vec<ClusterRecoveryView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ClusterRecoveryView {
    pub cluster_uuid: String,
    pub cluster_name: String,
    /// Newest first.
    pub manifests: Vec<ManifestKept>,
    pub headers: Vec<HeaderKept>,
    /// Newest first.
    pub restore_tests: Vec<ReportedRestoreTest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ManifestKept {
    pub generated_at_ms: i64,
    /// The start of its revision.
    pub revision: String,
}

/// One LUKS header backup file of a host, kept by content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct HeaderKept {
    pub host: String,
    pub file: String,
    pub newest_at_ms: i64,
    pub versions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ReportedRestoreTest {
    pub restore_uuid: String,
    pub vm_uuid: String,
    pub pit_uuid: String,
    pub host: String,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub booted: bool,
    pub agent: bool,
    pub agent_after_s: Option<u64>,
    pub error: Option<String>,
}
