// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The backup protocol between `bmac-source` on each host and the appliances
//! (`BACKUP.md`, "Protocol"). The request types here are the ones the design
//! fixes; manifests and status are defined with the backup work (M9).

use std::collections::BTreeMap;

use bmac_core::{DatasetName, HostSlot, SnapshotGuid, SnapshotName};
use serde::{Deserialize, Serialize};

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
    pub address: String,
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

/// `POST /v1/send`: a full, incremental, or resumed send of one BMAC
/// snapshot on this host. Never a replication (`-R`) stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SendRequest {
    Full {
        dataset: DatasetName,
        snapshot: SnapshotName,
    },
    /// From the appliance's head, which the cluster keeps as a snapshot or,
    /// once the appliances have it, as a bookmark of the same name.
    Incremental {
        dataset: DatasetName,
        snapshot: SnapshotName,
        from: SnapshotName,
        from_kind: BaseKind,
        from_guid: SnapshotGuid,
    },
    Resume {
        token: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseKind {
    Snapshot,
    Bookmark,
}

impl SendRequest {
    /// Whether `bmac-source` may serve it: only BMAC snapshots.
    pub fn names_bmac_snapshots(&self) -> bool {
        let pit = |name: &SnapshotName| name.as_str().starts_with(PIT_SNAPSHOT_PREFIX);
        match self {
            SendRequest::Full { snapshot, .. } => pit(snapshot),
            SendRequest::Incremental { snapshot, from, .. } => pit(snapshot) && pit(from),
            SendRequest::Resume { .. } => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_bmac_snapshots_are_sendable() {
        let send = |value| serde_json::from_value::<SendRequest>(value).unwrap();
        assert!(
            send(json!({"kind": "full", "dataset": "rpool/data/vm-101-disk-0", "snapshot": "bmac-pit-20261011"}))
                .names_bmac_snapshots()
        );
        assert!(
            !send(json!({"kind": "incremental", "dataset": "rpool/data/vm-101-disk-0", "snapshot": "bmac-pit-2",
                         "from": "daily-1", "from_kind": "snapshot", "from_guid": "123"}))
            .names_bmac_snapshots()
        );
        assert!(
            send(json!({"kind": "incremental", "dataset": "rpool/data/vm-101-disk-0", "snapshot": "bmac-pit-2",
                         "from": "bmac-pit-1", "from_kind": "bookmark", "from_guid": "123"}))
            .names_bmac_snapshots()
        );
        assert!(
            serde_json::from_value::<SendRequest>(
                json!({"kind": "full", "dataset": "rpool", "snapshot": "bmac-pit-1", "flags": "-R"})
            )
            .is_err()
        );
    }

    fn peer(fingerprint: &str) -> TrustedPeer {
        TrustedPeer {
            appliance_uuid: "b".into(),
            fingerprint: fingerprint.into(),
            address: "bmac-appliance-b".into(),
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
