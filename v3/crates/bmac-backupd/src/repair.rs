// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Repair from a trusted peer (`BACKUP.md`, "Repairing an appliance that
//! fell behind", step 2). When the cluster can offer only a full send of a
//! branch this appliance has a head on, a peer this appliance trusts, and
//! that trusts it back, may hold both the head and the offered PIT with
//! the same GUIDs; then the peer sends the incremental between them, and
//! the pull is checked and published as any other.

use std::collections::BTreeMap;

use bmac_core::{ApplianceAddress, SnapshotGuid};
use bmac_exec::BoxFuture;
use bmac_proto::appliance::{
    BaseKind, ClaimRequest, ExportRequest, OfferBase, OfferedPit, PeerIndex, SendRequest,
    SourceHost, on_appliance,
};
use uuid::Uuid;

use crate::client::{CallError, ClusterClient};
use crate::journal::{Journal, StoredPit};
use crate::pool::Stream;
use crate::pull::Link;

/// The head a full offer would abandon: this appliance's newest PIT of the
/// VM on the offered branch, older than the offer.
pub fn behind<'a>(
    journal: &'a Journal,
    cluster: &str,
    offer: &OfferedPit,
) -> Option<&'a StoredPit> {
    if offer.base.is_some() {
        return None;
    }
    journal
        .pits
        .iter()
        .filter(|p| {
            p.cluster_uuid == cluster
                && p.vm_uuid == offer.vm_uuid
                && p.branch_uuid == offer.branch_uuid
                && p.taken_s < offer.taken_s
        })
        .max_by_key(|p| (p.taken_s, p.published_at_ms))
}

/// Whether the peer holds the offered PIT, and `head` with every volume's
/// GUID the same, so it can send the incremental between them.
pub fn can_serve(index: &PeerIndex, cluster: &str, head: &StoredPit, offer: &OfferedPit) -> bool {
    let find = |pit: &Uuid| {
        index
            .pits
            .iter()
            .find(|p| p.cluster_uuid == cluster && &p.pit_uuid == pit)
    };
    let (Some(theirs), Some(target)) = (find(&head.pit_uuid), find(&offer.pit_uuid)) else {
        return false;
    };
    target.name == offer.name
        && target.branch_uuid == offer.branch_uuid
        && head.volumes.iter().all(|v| {
            theirs
                .volumes
                .iter()
                .any(|t| t.volume_uuid == v.volume_uuid && t.guid == v.guid)
        })
}

/// The offer as a pull from the peer: incremental from `head`.
pub fn from_head(offer: &OfferedPit, head: &StoredPit) -> Option<OfferedPit> {
    let host = offer.hosts.first().cloned()?;
    Some(OfferedPit {
        base: Some(OfferBase {
            pit_uuid: head.pit_uuid,
            kind: BaseKind::Snapshot,
        }),
        hosts: vec![host],
        ..offer.clone()
    })
}

/// A trusted peer, as a pull needs it: no claims, and each send an export
/// from the head's GUID on that volume.
pub struct PeerLink {
    pub client: ClusterClient,
    pub address: ApplianceAddress,
    pub cluster_uuid: String,
    /// The head's GUID on each of its volumes.
    pub base_guids: BTreeMap<Uuid, SnapshotGuid>,
}

impl PeerLink {
    pub fn new(
        client: ClusterClient,
        address: ApplianceAddress,
        cluster: &str,
        head: &StoredPit,
    ) -> PeerLink {
        PeerLink {
            client,
            address,
            cluster_uuid: cluster.to_string(),
            base_guids: head
                .volumes
                .iter()
                .map(|v| (v.volume_uuid, v.guid))
                .collect(),
        }
    }

    pub fn export_request(&self, request: &SendRequest) -> ExportRequest {
        ExportRequest {
            cluster_uuid: self.cluster_uuid.clone(),
            pit_uuid: request.pit_uuid,
            volume_uuid: request.volume_uuid,
            from_guid: request
                .base
                .and_then(|_| self.base_guids.get(&request.volume_uuid).copied()),
            resume_token: request.resume_token.clone(),
            estimate: false,
            grant: None,
        }
    }
}

impl Link for PeerLink {
    fn claim<'a>(&'a self, _request: &'a ClaimRequest) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async { Ok(()) })
    }

    fn send<'a>(
        &'a self,
        _host: &'a SourceHost,
        request: &'a SendRequest,
    ) -> BoxFuture<'a, Result<Stream, CallError>> {
        Box::pin(async move {
            self.client
                .stream_peer(
                    &self.address,
                    on_appliance::EXPORT,
                    &self.export_request(request),
                )
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::StoredVolume;
    use bmac_proto::appliance::{PeerPit, PeerVolume};

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn stored(n: u128, guid: u64) -> StoredPit {
        StoredPit {
            cluster_uuid: "c1".into(),
            vm_uuid: id(7),
            branch_uuid: id(0xb),
            pit_uuid: id(n),
            parent: None,
            name: format!("bmac-pit-{n}"),
            taken_s: n as i64,
            published_at_ms: 0,
            volumes: vec![StoredVolume {
                volume_uuid: id(0xd),
                dataset: "bk/x".into(),
                guid: SnapshotGuid::new(guid),
                volsize: 1,
                volblocksize: 1,
            }],
            bytes: 0,
            manifest_sha256: String::new(),
            received_from: None,
        }
    }

    fn offer(n: u128, base: Option<u128>) -> OfferedPit {
        OfferedPit {
            vm_uuid: id(7),
            pit_uuid: id(n),
            branch_uuid: id(0xb),
            name: format!("bmac-pit-{n}"),
            taken_s: n as i64,
            parent: None,
            base: base.map(|b| OfferBase {
                pit_uuid: id(b),
                kind: BaseKind::Bookmark,
            }),
            hosts: vec![SourceHost {
                host: "mox1".parse().unwrap(),
                addresses: vec!["a".into()],
            }],
            manifest: String::new(),
        }
    }

    fn peer_pit(p: &StoredPit) -> PeerPit {
        PeerPit {
            cluster_uuid: p.cluster_uuid.clone(),
            vm_uuid: p.vm_uuid,
            branch_uuid: p.branch_uuid,
            pit_uuid: p.pit_uuid,
            parent: None,
            name: p.name.clone(),
            taken_s: p.taken_s,
            volumes: p
                .volumes
                .iter()
                .map(|v| PeerVolume {
                    volume_uuid: v.volume_uuid,
                    guid: v.guid,
                    volsize: 1,
                    volblocksize: 1,
                })
                .collect(),
            manifest: String::new(),
        }
    }

    #[test]
    fn a_full_offer_over_a_head_goes_to_a_peer_holding_both_with_the_same_guids() {
        let mut journal = Journal::new(id(1), "g");
        journal.pits = vec![stored(1, 101), stored(2, 102)];
        assert!(behind(&journal, "c1", &offer(5, Some(2))).is_none());
        let head = behind(&journal, "c1", &offer(5, None)).unwrap();
        assert_eq!(head.pit_uuid, id(2));
        assert!(behind(&journal, "c2", &offer(5, None)).is_none());

        let mut index = PeerIndex {
            appliance_uuid: id(9),
            revision: 1,
            pits: vec![peer_pit(&stored(2, 102)), peer_pit(&stored(5, 105))],
        };
        assert!(can_serve(&index, "c1", head, &offer(5, None)));
        // The peer's copy of the head is another snapshot: no.
        index.pits[0].volumes[0].guid = SnapshotGuid::new(999);
        assert!(!can_serve(&index, "c1", head, &offer(5, None)));
        index.pits[0].volumes[0].guid = SnapshotGuid::new(102);
        index.pits.pop();
        assert!(!can_serve(&index, "c1", head, &offer(5, None)));

        let repaired = from_head(&offer(5, None), head).unwrap();
        assert_eq!(repaired.base.unwrap().pit_uuid, id(2));
        let (cert, key) = crate::state::certificate("vault1", &id(1)).unwrap();
        let link = PeerLink::new(
            ClusterClient::peer(&"ab".repeat(32), (&cert, &key), 8445).unwrap(),
            "vault2.example.org".parse().unwrap(),
            "c1",
            head,
        );
        let request = SendRequest {
            transfer_uuid: id(0x77),
            vm_uuid: id(7),
            pit_uuid: id(5),
            volume_uuid: id(0xd),
            base: Some(id(2)),
            resume_token: None,
            large_blocks: true,
            compressed: true,
        };
        let export = link.export_request(&request);
        assert_eq!(export.from_guid, Some(SnapshotGuid::new(102)));
        assert!(export.grant.is_none());
        let full = link.export_request(&SendRequest {
            base: None,
            ..request
        });
        assert_eq!(full.from_guid, None);
    }
}
