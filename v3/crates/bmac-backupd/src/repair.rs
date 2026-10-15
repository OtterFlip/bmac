// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Repair from a trusted peer (`BACKUP.md`, "Repairing an appliance that
//! fell behind", step 2). When the cluster can offer only a full send of a
//! branch this appliance has a head on, a peer this appliance trusts, and
//! that trusts it back, may hold the head with the same GUIDs and a later
//! PIT of the branch, the offered one or an earlier one; then the peer
//! sends the incremental between them, and the pull is checked and
//! published as any other. From an earlier one, the cluster's next offer is
//! an incremental from it while its bookmark lasts.

use std::collections::BTreeMap;

use bmac_core::{ApplianceAddress, SnapshotGuid};
use bmac_exec::BoxFuture;
use bmac_proto::appliance::{
    BaseKind, ClaimRequest, ExportRequest, OfferBase, OfferedPit, PeerIndex, PeerPit, SendRequest,
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

/// The PIT the peer can send an incremental to from `head`: the offered
/// one if it holds it, otherwise its newest PIT of the branch between the
/// head and the offer. Either way the peer's copy of the head has every
/// volume's GUID, which proves the ancestry on a branch's single line.
pub fn target<'a>(
    index: &'a PeerIndex,
    cluster: &str,
    head: &StoredPit,
    offer: &OfferedPit,
) -> Option<&'a PeerPit> {
    let theirs = index
        .pits
        .iter()
        .find(|p| p.cluster_uuid == cluster && p.pit_uuid == head.pit_uuid)?;
    let same_head = head.volumes.iter().all(|v| {
        theirs
            .volumes
            .iter()
            .any(|t| t.volume_uuid == v.volume_uuid && t.guid == v.guid)
    });
    if !same_head {
        return None;
    }
    let later = index.pits.iter().filter(|p| {
        p.cluster_uuid == cluster
            && p.vm_uuid == offer.vm_uuid
            && p.branch_uuid == offer.branch_uuid
            && p.taken_s > head.taken_s
            && p.taken_s <= offer.taken_s
    });
    let mut newest: Option<&PeerPit> = None;
    for p in later {
        if p.pit_uuid == offer.pit_uuid {
            return (p.name == offer.name).then_some(p);
        }
        if newest.is_none_or(|n| (p.taken_s, p.pit_uuid) > (n.taken_s, n.pit_uuid)) {
            newest = Some(p);
        }
    }
    newest
}

/// `target` as a pull from the peer, incremental from `head`. The offered
/// PIT keeps the cluster's manifest; an earlier one carries the peer's.
pub fn repaired_offer(
    target: &PeerPit,
    offer: &OfferedPit,
    head: &StoredPit,
) -> Option<OfferedPit> {
    let host = offer.hosts.first().cloned()?;
    let base = Some(OfferBase {
        pit_uuid: head.pit_uuid,
        kind: BaseKind::Snapshot,
    });
    if target.pit_uuid == offer.pit_uuid {
        return Some(OfferedPit {
            base,
            hosts: vec![host],
            ..offer.clone()
        });
    }
    Some(OfferedPit {
        vm_uuid: target.vm_uuid,
        pit_uuid: target.pit_uuid,
        branch_uuid: target.branch_uuid,
        name: target.name.clone(),
        taken_s: target.taken_s,
        parent: target.parent,
        base,
        hosts: vec![host],
        manifest: target.manifest.clone(),
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
    use bmac_proto::appliance::PeerVolume;

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
        let target_of =
            |index: &PeerIndex| target(index, "c1", head, &offer(5, None)).map(|p| p.pit_uuid);
        assert_eq!(target_of(&index), Some(id(5)));
        // The peer's copy of the head is another snapshot: no.
        index.pits[0].volumes[0].guid = SnapshotGuid::new(999);
        assert_eq!(target_of(&index), None);
        index.pits[0].volumes[0].guid = SnapshotGuid::new(102);

        // Without the offered PIT, the newest one between the head and it.
        index.pits.pop();
        assert_eq!(target_of(&index), None, "nothing after the head");
        let mut other_branch = peer_pit(&stored(4, 104));
        other_branch.branch_uuid = id(0xc);
        index.pits.extend([
            peer_pit(&stored(3, 103)),
            peer_pit(&stored(4, 104)),
            other_branch,
            peer_pit(&stored(6, 106)),
        ]);
        index.pits[2].parent = Some(id(3));
        index.pits[2].manifest = "the peer's manifest".into();
        assert_eq!(target_of(&index), Some(id(4)));

        let earlier = repaired_offer(&index.pits[2], &offer(5, None), head).unwrap();
        assert_eq!(
            (earlier.pit_uuid, earlier.parent, earlier.manifest.as_str()),
            (id(4), Some(id(3)), "the peer's manifest")
        );
        assert_eq!(earlier.base.unwrap().pit_uuid, id(2));
        let repaired = repaired_offer(&peer_pit(&stored(5, 105)), &offer(5, None), head).unwrap();
        assert_eq!(repaired.pit_uuid, id(5));
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
