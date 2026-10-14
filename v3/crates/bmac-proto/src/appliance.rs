// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The backup protocol between `bmac-source` on each host and the appliances
//! (`BACKUP.md`, "Protocol"). The request types here are the ones the design
//! fixes; manifests and status are defined with the backup work (M9).

use std::collections::BTreeMap;

use bmac_core::{ApplianceAddress, HostSlot, SnapshotGuid};
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

pub const SOURCE_PROTOCOL: &str = "bmac-source";
pub const APPLIANCE_PROTOCOL: &str = "bmac-appliance";
pub const SOURCE_PORT: u16 = 8445;
pub const APPLIANCE_UI_PORT: u16 = 8443;

/// Served by `bmac-source` to an enrolled appliance. The cluster never
/// connects to an appliance except to restore under a grant.
pub mod on_source {
    pub const CHECKIN: &str = "/v1/checkin";
    pub const CLAIM: &str = "/v1/claim";
    pub const INDEX: &str = "/v1/index";
    pub const CATALOG: &str = "/v1/catalog";
    pub const LOCATE: &str = "/v1/locate";
    pub const SEND: &str = "/v1/send";
    pub const RECOVERY_MANIFEST: &str = "/v1/recovery-manifest";
    pub const LUKS_HEADERS: &str = "/v1/luks-headers";
    pub const HEARTBEAT: &str = "/v1/heartbeat";
}

/// Served by an appliance on [`SOURCE_PORT`], only to cluster restore jobs
/// under its grants and to its trusted peers (`BACKUP.md`, "Trusted peers").
pub mod on_appliance {
    pub const EXPORT: &str = "/v1/export";
    pub const PEER_INDEX: &str = "/v1/peer-index";
    pub const GRANTED: &str = "/v1/granted";
}

/// One entry of an appliance's trusted-peer list. Only that appliance's
/// administrator adds or removes entries; trust goes one way, so a repair
/// between two appliances needs an entry on each. Trust is all or nothing:
/// a trusted peer reads every published PIT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedPeer {
    pub appliance_uuid: String,
    /// SHA-256 of the peer's certificate, typed from the peer's own UI.
    pub fingerprint: String,
    /// Where the peer's 8445 answers: an IP address or a DNS name.
    pub address: ApplianceAddress,
}

/// The trusted peer a client certificate belongs to, if any. Anything else
/// is refused in the handshake.
pub fn trusted_peer<'a>(peers: &'a [TrustedPeer], fingerprint: &str) -> Option<&'a TrustedPeer> {
    let wanted = normalized_fingerprint(fingerprint)?;
    peers
        .iter()
        .find(|peer| normalized_fingerprint(&peer.fingerprint).as_deref() == Some(wanted.as_str()))
}

/// Lowercase hex without separators, or `None` unless it is a SHA-256.
pub fn normalized_fingerprint(text: &str) -> Option<String> {
    let hex: String = text
        .chars()
        .filter(|c| !matches!(c, ':' | ' '))
        .map(|c| c.to_ascii_lowercase())
        .collect();
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
}

/// The prefix every snapshot `bmac-source` will send carries.
pub const PIT_SNAPSHOT_PREFIX: &str = "bmac-pit-";

/// `GET /v1/locate`: which hosts hold these snapshots or bookmarks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocateRequest {
    pub guids: Vec<SnapshotGuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocateResponse {
    pub hosts: BTreeMap<SnapshotGuid, Vec<HostSlot>>,
}

/// Whether an appliance is catching up normally, repairing from a peer, or
/// needs a full send (`BACKUP.md`, "Repairing an appliance that fell
/// behind").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplianceProgress {
    #[default]
    Normal,
    Repairing,
    ReseedRequired,
}

/// The appliance's newest published PIT of one VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadReport {
    pub vm_uuid: Uuid,
    pub branch_uuid: Uuid,
    pub pit_uuid: Uuid,
}

/// A pull in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferReport {
    pub transfer_uuid: Uuid,
    pub vm_uuid: Uuid,
    pub pit_uuid: Uuid,
    /// Received so far, over every volume of the PIT.
    pub bytes: u64,
}

/// An offered PIT the appliance gave up on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailedReport {
    pub vm_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capacity {
    pub pool_size_bytes: u64,
    pub pool_free_bytes: u64,
    /// The cluster's namespace quota on the appliance, if it has one.
    pub quota_bytes: Option<u64>,
    pub used_bytes: u64,
}

/// `POST /v1/checkin`, once per check-in interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckinRequest {
    pub appliance_uuid: Uuid,
    pub version: String,
    /// The pool waits for the console passphrase.
    pub locked: bool,
    pub pool_health: String,
    pub capacity: Capacity,
    pub progress: ApplianceProgress,
    pub heads: Vec<HeadReport>,
    pub transfers: Vec<TransferReport>,
    #[serde(default)]
    pub failed: Vec<FailedReport>,
    pub index_revision: u64,
    #[serde(default)]
    pub scrub: Option<ScrubReport>,
}

/// The appliance pool's last or current scrub (`BACKUP.md`, "Scrubs and
/// integrity").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrubReport {
    /// `running`, `paused`, `finished`, or `canceled`.
    pub state: String,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    /// Thousandths done, while it runs.
    pub done_permille: Option<u32>,
    pub repaired_bytes: u64,
    /// Unrepairable errors ZFS reports for the pool.
    pub errors: u64,
    /// The datasets ZFS names for them.
    pub affected: Vec<String>,
    /// Published PITs that may hold a damaged block.
    pub suspect_pits: Vec<Uuid>,
}

/// Where a PIT can be pulled from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceHost {
    pub host: HostSlot,
    /// Names or addresses its `bmac-source` answers on, best first.
    pub addresses: Vec<String>,
}

/// What the cluster still has of a base PIT on the volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseKind {
    Snapshot,
    Bookmark,
}

/// The base an incremental pull starts from: the appliance's head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferBase {
    pub pit_uuid: Uuid,
    pub kind: BaseKind,
}

/// One PIT the appliance should pull.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedPit {
    pub vm_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub branch_uuid: Uuid,
    pub name: String,
    pub taken_s: i64,
    pub parent: Option<Uuid>,
    /// `None` is a full send: the first PIT of a branch for this appliance.
    /// An appliance with a head on the branch that gets `None` has fallen
    /// too far behind; it repairs from a peer or reseeds.
    pub base: Option<OfferBase>,
    pub hosts: Vec<SourceHost>,
    /// The sealed manifest, as the catalog holds it.
    pub manifest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckinReply {
    pub cluster_uuid: String,
    pub now_ms: i64,
    pub checkin_interval_s: i64,
    /// The appliance's state as the cluster sees it, such as `ACTIVE`.
    pub state: String,
    pub offers: Vec<OfferedPit>,
    /// Send the PIT index next (`POST /v1/index`).
    pub want_index: bool,
}

/// `POST /v1/claim`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRequest {
    pub vm_uuid: Uuid,
    pub pit_uuid: Uuid,
}

/// One published PIT in an appliance's index: metadata only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct IndexedPit {
    pub vm_uuid: String,
    pub branch_uuid: String,
    pub pit_uuid: String,
    pub parent: Option<String>,
    pub name: String,
    pub taken_s: i64,
    pub bytes: u64,
}

/// `POST /v1/index`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexRequest {
    pub revision: u64,
    pub pits: Vec<IndexedPit>,
}

/// `POST /v1/send`: one volume of an offered PIT, full, incremental from a
/// base PIT the catalog shows as one of its ancestors (on its branch or a
/// branch it descends from), or resumed. The cluster resolves every name
/// itself; never a replication (`-R`) stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendRequest {
    pub transfer_uuid: Uuid,
    pub vm_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub volume_uuid: Uuid,
    pub base: Option<Uuid>,
    /// `receive_resume_token` from the appliance's partial receive.
    pub resume_token: Option<String>,
    /// `-L` and `-c`, when the appliance's OpenZFS takes them.
    #[serde(default)]
    pub large_blocks: bool,
    #[serde(default)]
    pub compressed: bool,
}

/// `POST /v1/export` on an appliance: one volume of a published PIT, full,
/// incremental from the snapshot with `from_guid` (which the client holds),
/// or resumed; or with `estimate`, only the stream's size. A cluster host
/// must present a restore grant naming the PIT; a trusted peer presents
/// none. Never a replication stream, never the appliance's properties.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    pub cluster_uuid: String,
    pub pit_uuid: Uuid,
    pub volume_uuid: Uuid,
    pub from_guid: Option<SnapshotGuid>,
    pub resume_token: Option<String>,
    #[serde(default)]
    pub estimate: bool,
    #[serde(default)]
    pub grant: Option<String>,
}

impl std::fmt::Debug for ExportRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportRequest")
            .field("cluster_uuid", &self.cluster_uuid)
            .field("pit_uuid", &self.pit_uuid)
            .field("volume_uuid", &self.volume_uuid)
            .field("from_guid", &self.from_guid)
            .field("estimate", &self.estimate)
            .field("grant", &self.grant.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

/// The answer to an export with `estimate`: `zfs send -nP`'s size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportEstimate {
    pub bytes: u64,
}

/// One volume of a PIT in a peer index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerVolume {
    pub volume_uuid: Uuid,
    pub guid: SnapshotGuid,
    pub volsize: u64,
    pub volblocksize: u64,
}

/// One published PIT in a peer index, with its sealed manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerPit {
    pub cluster_uuid: String,
    pub vm_uuid: Uuid,
    pub branch_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub parent: Option<Uuid>,
    pub name: String,
    pub taken_s: i64,
    pub volumes: Vec<PeerVolume>,
    pub manifest: String,
}

/// `GET /v1/peer-index`: every published PIT, metadata only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerIndex {
    pub appliance_uuid: Uuid,
    pub revision: u64,
    pub pits: Vec<PeerPit>,
}

/// `POST /v1/granted` on an appliance, from a cluster host: what a restore
/// grant lets that cluster read.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantedRequest {
    pub cluster_uuid: String,
    pub grant: String,
}

impl std::fmt::Debug for GrantedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrantedRequest")
            .field("cluster_uuid", &self.cluster_uuid)
            .field("grant", &"<redacted>")
            .finish()
    }
}

/// An earlier published PIT of a granted PIT's VM: an incremental base the
/// restore may already hold, by its volumes' GUIDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantedBase {
    pub vm_uuid: Uuid,
    pub branch_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub parent: Option<Uuid>,
    pub taken_s: i64,
    pub volumes: Vec<PeerVolume>,
}

/// The answer to `POST /v1/granted`: the granted PITs with their manifests,
/// and the earlier PITs of the same VMs, metadata only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantedIndex {
    pub appliance_uuid: Uuid,
    pub grant_id: String,
    pub expires_at_ms: i64,
    pub pits: Vec<PeerPit>,
    pub bases: Vec<GrantedBase>,
}

/// `GET /v1/heartbeat`, for the appliance's watchdog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub host: HostSlot,
    pub now_ms: i64,
    pub quorate: bool,
    pub online: Vec<HostSlot>,
}

/// `GET /v1/luks-headers`: this host's LUKS header backups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LuksHeaders {
    pub host: HostSlot,
    /// File name and its contents, base64.
    pub headers: BTreeMap<String, String>,
}

/// The enrollment bundle the Dashboard issues for a new appliance, shown
/// once. `bmac-backupd enroll` presents its token to a host's
/// `POST /v1/enroll` (port 8444), checking the host's chain against the
/// Proxmox CA fingerprint first, and keeps everything but the token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollBundle {
    pub version: u32,
    pub cluster_uuid: String,
    pub cluster_name: String,
    pub enroll_token: String,
    pub expires_ms: i64,
    /// SHA-256 of the Proxmox cluster CA, lowercase hex, which the
    /// appliance pins for every connection to the cluster.
    pub proxmox_ca_fingerprint: String,
    pub hosts: Vec<SourceHost>,
}

pub const ENROLL_BUNDLE_VERSION: u32 = 1;

impl std::fmt::Debug for EnrollBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollBundle")
            .field("cluster_uuid", &self.cluster_uuid)
            .field("cluster_name", &self.cluster_name)
            .field("expires_ms", &self.expires_ms)
            .field("hosts", &self.hosts)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_enrollment_bundle_never_prints_its_token() {
        let bundle = EnrollBundle {
            version: ENROLL_BUNDLE_VERSION,
            cluster_uuid: "c".into(),
            cluster_name: "lab".into(),
            enroll_token: "secret-token-value".into(),
            expires_ms: 1,
            proxmox_ca_fingerprint: "ab".into(),
            hosts: vec![],
        };
        assert!(!format!("{bundle:?}").contains("secret-token-value"));
    }
    use serde_json::json;

    #[test]
    fn a_send_names_ids_and_nothing_else() {
        let send = json!({"transfer_uuid": "6f9619ff-8b86-4011-b42d-00c04fc964ff",
                          "vm_uuid": "6f9619ff-8b86-4011-b42d-00c04fc964fe",
                          "pit_uuid": "6f9619ff-8b86-4011-b42d-00c04fc964fd",
                          "volume_uuid": "6f9619ff-8b86-4011-b42d-00c04fc964fc",
                          "base": null, "resume_token": null});
        assert!(serde_json::from_value::<SendRequest>(send.clone()).is_ok());
        let mut flags = send.clone();
        flags["flags"] = json!("-R");
        assert!(serde_json::from_value::<SendRequest>(flags).is_err());
        let mut dataset = send;
        dataset["dataset"] = json!("rpool");
        assert!(serde_json::from_value::<SendRequest>(dataset).is_err());
    }

    fn peer(fingerprint: &str) -> TrustedPeer {
        TrustedPeer {
            appliance_uuid: "b".into(),
            fingerprint: fingerprint.into(),
            address: "bmac-appliance-b".parse().unwrap(),
        }
    }

    #[test]
    fn only_a_listed_fingerprint_is_a_trusted_peer() {
        let listed = "AB:".repeat(31) + "AB";
        let peers = [peer(&listed)];
        assert!(trusted_peer(&peers, &"ab".repeat(32)).is_some());
        assert!(trusted_peer(&peers, &"cd".repeat(32)).is_none());
        assert!(trusted_peer(&peers, "").is_none());
        assert!(trusted_peer(&[peer("")], "").is_none());
        assert!(trusted_peer(&[], &"ab".repeat(32)).is_none());
    }

    #[test]
    fn a_trusted_peer_entry_has_nothing_but_identity() {
        let entry = json!({"appliance_uuid": "b", "fingerprint": "ab".repeat(32),
                           "address": "bmac-appliance-b", "namespaces": ["cluster-1"]});
        assert!(serde_json::from_value::<TrustedPeer>(entry).is_err());
    }
}
