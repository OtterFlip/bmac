// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use bmac_core::ApplianceAddress;

use super::*;
use crate::client::{CallError, ClusterClient};
use crate::fake_pool::FakePool;
use crate::journal::{StoredPit, StoredVolume};

const CLUSTER: &str = "c1";

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn pit(n: u128) -> StoredPit {
    StoredPit {
        cluster_uuid: CLUSTER.into(),
        vm_uuid: id(7),
        branch_uuid: id(0xb),
        pit_uuid: id(n),
        parent: None,
        name: format!("bmac-pit-{n}"),
        taken_s: n as i64,
        published_at_ms: 0,
        volumes: vec![StoredVolume {
            volume_uuid: id(0xd),
            dataset: "bk/bmac/clusters/c1/vms/v/b/d".into(),
            guid: SnapshotGuid::new(100 + n as u64),
            volsize: 1 << 20,
            volblocksize: 16384,
        }],
        bytes: 1,
        manifest_sha256: String::new(),
        received_from: None,
    }
}

fn journal() -> Journal {
    let mut j = Journal::new(id(1), "g");
    j.pits = vec![pit(1), pit(2)];
    j
}

fn request(pit: u128) -> ExportRequest {
    ExportRequest {
        cluster_uuid: CLUSTER.into(),
        pit_uuid: id(pit),
        volume_uuid: id(0xd),
        from_guid: None,
        resume_token: None,
        estimate: false,
        grant: None,
    }
}

fn peer() -> Client {
    Client::Peer {
        appliance_uuid: id(9).to_string(),
        fingerprint: "ab".repeat(32),
    }
}

fn host(cluster: &str) -> Client {
    Client::Cluster {
        cluster_uuid: cluster.into(),
        fingerprint: "cd".repeat(32),
    }
}

#[test]
fn a_peer_reads_any_published_pit_and_a_host_only_what_its_grant_names() {
    let j = journal();
    let mut grants = Grants::default();
    let plan = plan_export(&j, &grants, &peer(), &request(2), 0).unwrap();
    assert_eq!(plan.snapshot, "bk/bmac/clusters/c1/vms/v/b/d@bmac-pit-2");
    assert_eq!(plan.guid, SnapshotGuid::new(102));
    assert!(plan.grant_id.is_none());

    let refused = plan_export(&j, &grants, &host(CLUSTER), &request(2), 0).unwrap_err();
    assert_eq!(refused.code, ErrorCode::Forbidden);
    let (grant, token) = grants
        .issue(&j, CLUSTER, &[id(2)], None, "admin", 0)
        .unwrap();
    let with = ExportRequest {
        grant: Some(token.clone()),
        ..request(2)
    };
    let plan = plan_export(&j, &grants, &host(CLUSTER), &with, 1).unwrap();
    assert_eq!(plan.grant_id, Some(grant.grant_id));
    let other_pit = ExportRequest {
        grant: Some(token.clone()),
        ..request(1)
    };
    assert!(plan_export(&j, &grants, &host(CLUSTER), &other_pit, 1).is_err());
    let other_cluster = plan_export(&j, &grants, &host("c2"), &with, 1).unwrap_err();
    assert!(other_cluster.message.contains("its own cluster"));
    assert_eq!(
        plan_export(&j, &grants, &peer(), &request(3), 0)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
}

#[test]
fn a_grant_lists_its_pits_with_manifests_and_their_earlier_pits_as_bases() {
    let dir = tempfile::tempdir().unwrap();
    let metadata = Metadata::new(dir.path());
    let mut j = journal();
    j.pits.push(pit(3));
    for n in 1..=3 {
        metadata
            .write_manifest(CLUSTER, &id(n), &format!("{{\"m\":{n}}}"))
            .unwrap();
    }
    let mut grants = Grants::default();
    let (_, token) = grants
        .issue(&j, CLUSTER, &[id(2)], Some(1), "admin", 0)
        .unwrap();
    let grant = grants.live_for(&token, CLUSTER, 1).unwrap();
    let index = granted_index(&j, &metadata, grant);
    assert_eq!(index.pits.len(), 1);
    assert_eq!(index.pits[0].pit_uuid, id(2));
    assert_eq!(index.pits[0].manifest, "{\"m\":2}");
    let bases: Vec<Uuid> = index.bases.iter().map(|b| b.pit_uuid).collect();
    assert_eq!(bases, vec![id(1)], "only earlier PITs, and without manifests");
    assert_eq!(index.bases[0].volumes[0].guid, SnapshotGuid::new(101));
    assert!(grants.live_for(&token, "c2", 1).is_err());
    assert!(grants.live_for(&token, CLUSTER, 3_600_001).is_err());
    assert!(grants.live_for("bmac-grant-nope", CLUSTER, 1).is_err());
}

#[test]
fn an_incremental_starts_only_from_an_earlier_published_snapshot_of_the_volume() {
    let j = journal();
    let grants = Grants::default();
    let from = |guid| ExportRequest {
        from_guid: Some(SnapshotGuid::new(guid)),
        ..request(2)
    };
    let plan = plan_export(&j, &grants, &peer(), &from(101), 0).unwrap();
    assert_eq!(
        plan.base,
        Some((
            "bk/bmac/clusters/c1/vms/v/b/d@bmac-pit-1".into(),
            SnapshotGuid::new(101)
        ))
    );
    assert!(plan_export(&j, &grants, &peer(), &from(102), 0).is_err());
    assert!(plan_export(&j, &grants, &peer(), &from(555), 0).is_err());
    let args = send_args(&plan, false).join(" ");
    assert_eq!(
        args,
        "send -L -c -e -i bk/bmac/clusters/c1/vms/v/b/d@bmac-pit-1 bk/bmac/clusters/c1/vms/v/b/d@bmac-pit-2"
    );
    assert!(!args.split(' ').any(|a| a == "-R" || a == "-p"));
    let resumed = ExportPlan {
        resume_token: Some("1-abc".into()),
        ..plan
    };
    assert_eq!(send_args(&resumed, true).join(" "), "send -nP -t 1-abc");
}

#[test]
fn a_resume_token_dump_names_its_stream() {
    let dump = "resume token contents:\nnvlist version: 0\n\tobject = 0x1\n\ttoguid = 0x66\n\ttoname = bk/x@bmac-pit-2\n\tfromguid = 0x65\n";
    let f = token_fields(dump);
    assert_eq!(f["toname"], "bk/x@bmac-pit-2");
    assert_eq!(parse_guid(&f["toguid"]), Some(102));
    assert_eq!(parse_guid(&f["fromguid"]), Some(101));
    assert_eq!(parse_guid("17"), Some(17));
}

/// A cluster CA and a host certificate it issued for server use only, as
/// Proxmox issues them.
fn cluster_chain() -> (CertificateDer<'static>, CertificateDer<'static>) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(rcgen::DnType::CommonName, "Proxmox Virtual Environment");
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let host_key = rcgen::KeyPair::generate().unwrap();
    let mut host = rcgen::CertificateParams::new(vec!["mox1".to_string()]).unwrap();
    host.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let host_cert = host.signed_by(&host_key, &ca_cert, &ca_key).unwrap();
    (host_cert.der().clone(), ca_cert.der().clone())
}

fn der(pem: &str) -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()
}

#[test]
fn the_handshake_admits_trusted_peers_and_enrolled_clusters_hosts_only() {
    let (host_cert, ca) = cluster_chain();
    let (peer_pem, _) = state::certificate("vault2", &id(9)).unwrap();
    let (stranger_pem, _) = state::certificate("intruder", &id(66)).unwrap();
    let verifier = ExportVerifier {
        peers: vec![TrustedPeer {
            appliance_uuid: id(9).to_string(),
            fingerprint: fingerprint(&der(&peer_pem)),
            address: "vault2.example.org".parse().unwrap(),
        }],
        clusters: vec![ClusterCa {
            cluster_uuid: CLUSTER.into(),
            ca_fingerprint: fingerprint(&ca),
        }],
        provider: provider(),
        outcome: Mutex::default(),
    };
    let now = UnixTime::now();
    assert!(matches!(
        verifier.decide(&der(&peer_pem), &[], now),
        Ok(Client::Peer { appliance_uuid, .. }) if appliance_uuid == id(9).to_string()
    ));
    assert!(matches!(
        verifier.decide(&host_cert, std::slice::from_ref(&ca), now),
        Ok(Client::Cluster { cluster_uuid, .. }) if cluster_uuid == CLUSTER
    ));
    // Without the CA in the chain, or with another CA, a host is a stranger.
    assert!(verifier.decide(&host_cert, &[], now).is_err());
    let (_, other_ca) = cluster_chain();
    assert!(verifier.decide(&host_cert, &[other_ca], now).is_err());
    let (fp, claimed) = verifier.decide(&der(&stranger_pem), &[], now).unwrap_err();
    assert_eq!(fp, fingerprint(&der(&stranger_pem)));
    assert!(claimed.contains("intruder") && claimed.contains(&id(66).to_string()));
}

struct Appliance {
    _dir: tempfile::TempDir,
    exporter: Arc<Exporter>,
    address: ApplianceAddress,
    fingerprint: String,
}

async fn appliance() -> Appliance {
    appliance_with(None).await
}

/// `zfs` is a stand-in for `zfs send`.
async fn appliance_with(zfs: Option<&str>) -> Appliance {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::at(dir.path());
    std::fs::create_dir_all(paths.metadata()).unwrap();
    let metadata = Metadata::new(paths.metadata());
    metadata.save(&journal()).unwrap();
    for n in [1, 2] {
        metadata.write_manifest(CLUSTER, &id(n), "{}").unwrap();
    }
    let (cert, key) = state::certificate("vault1", &id(1)).unwrap();
    let identity = Identity {
        schema_version: 1,
        appliance_uuid: id(1),
        name: "vault1".into(),
        created_at_ms: 0,
    };
    let pool = FakePool::new();
    pool.zvols.lock().unwrap().insert(
        "bk/bmac/clusters/c1/vms/v/b/d".into(),
        crate::fake_pool::Zvol {
            snapshots: vec![
                ("bmac-pit-1".into(), SnapshotGuid::new(101)),
                ("bmac-pit-2".into(), SnapshotGuid::new(102)),
            ],
            ..Default::default()
        },
    );
    let mut exporter = Exporter::new(
        paths.clone(),
        identity,
        metadata,
        Arc::new(Audit::new(&paths.metadata())),
        Arc::new(pool),
        Arc::new(|| Box::pin(async { true }) as BoxFuture<'static, bool>),
        Arc::new(|| 5_000_000),
        &cert,
        &key,
    )
    .unwrap();
    if let Some(script) = zfs {
        let path = dir.path().join("zfs");
        std::fs::write(&path, script).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        exporter.zfs = path;
    }
    let exporter = Arc::new(exporter);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(exporter.clone().serve(listener));
    Appliance {
        _dir: dir,
        exporter,
        address: format!("127.0.0.1:{port}").parse().unwrap(),
        fingerprint: crate::state::pem_fingerprint(&cert).unwrap(),
    }
}

#[tokio::test]
async fn a_trusted_peer_reads_the_index_and_a_stranger_is_refused_and_recorded() {
    let a = appliance().await;
    let (peer_cert, peer_key) = state::certificate("vault2", &id(2)).unwrap();
    let (stranger_cert, stranger_key) = state::certificate("intruder", &id(66)).unwrap();
    {
        let mut peers = Peers::default();
        peers
            .trust(
                id(2),
                &crate::state::pem_fingerprint(&peer_cert).unwrap(),
                "vault2.example.org".parse().unwrap(),
                &a.fingerprint,
                "admin",
                0,
            )
            .unwrap();
        peers.save(&a.exporter.metadata).unwrap();
    }
    let as_peer = ClusterClient::peer(&a.fingerprint, (&peer_cert, &peer_key), 8445).unwrap();
    let index: PeerIndex = as_peer
        .call_peer::<(), _>(&a.address, "GET", on_appliance::PEER_INDEX, None)
        .await
        .unwrap();
    assert_eq!(index.appliance_uuid, id(1));
    assert_eq!(index.pits.len(), 2);
    assert_eq!(index.pits[1].volumes[0].guid, SnapshotGuid::new(102));

    let as_stranger =
        ClusterClient::peer(&a.fingerprint, (&stranger_cert, &stranger_key), 8445).unwrap();
    let refused = as_stranger
        .call_peer::<(), PeerIndex>(&a.address, "GET", on_appliance::PEER_INDEX, None)
        .await;
    assert!(
        matches!(refused, Err(CallError::Unreachable(_))),
        "{refused:?}"
    );
    // The refusal is written just after the handshake fails.
    let peers = eventually(|| {
        let peers = Peers::load(&a.exporter.metadata).unwrap();
        (!peers.refusals.is_empty()).then_some(peers)
    })
    .await;
    assert_eq!(peers.refusals.len(), 1);
    assert!(peers.refusals[0].claimed.contains("intruder"));
    assert_eq!(
        peers.refusals[0].fingerprint,
        crate::state::pem_fingerprint(&stranger_cert).unwrap()
    );
    let audit = a.exporter.audit.recent(5).unwrap();
    assert_eq!(audit[0].event, "refused_peer");
    assert_eq!(peers.peers[0].last_served_ms, Some(5_000_000));

    // A client that pins another fingerprint never talks to this appliance.
    let wrong = ClusterClient::peer(&"ee".repeat(32), (&peer_cert, &peer_key), 8445).unwrap();
    assert!(
        wrong
            .call_peer::<(), PeerIndex>(&a.address, "GET", on_appliance::PEER_INDEX, None)
            .await
            .is_err()
    );
    assert_eq!(wrong.presented(), Some(a.fingerprint.clone()));

    // Untrusted, the peer is refused at once.
    let mut peers = Peers::load(&a.exporter.metadata).unwrap();
    peers.untrust(&id(2).to_string());
    peers.save(&a.exporter.metadata).unwrap();
    assert!(
        as_peer
            .call_peer::<(), PeerIndex>(&a.address, "GET", on_appliance::PEER_INDEX, None)
            .await
            .is_err()
    );
}

fn trust(a: &Appliance, uuid: Uuid, cert: &str) {
    let mut peers = Peers::load(&a.exporter.metadata).unwrap();
    peers
        .trust(
            uuid,
            &crate::state::pem_fingerprint(cert).unwrap(),
            "peer.example.org".parse().unwrap(),
            &a.fingerprint,
            "admin",
            0,
        )
        .unwrap();
    peers.save(&a.exporter.metadata).unwrap();
}

#[tokio::test]
async fn trust_goes_one_way_and_a_peer_can_only_read() {
    let a = appliance().await;
    let b = appliance().await;
    let (a_cert, a_key) = state::certificate("vault1", &id(1)).unwrap();
    let (b_cert, b_key) = state::certificate("vault2", &id(2)).unwrap();
    // The appliances' own servers present other certificates than these
    // clients, so each side is pinned to what its server presents.
    trust(&a, id(2), &b_cert);
    let b_to_a = ClusterClient::peer(&a.fingerprint, (&b_cert, &b_key), 8445).unwrap();
    let a_to_b = ClusterClient::peer(&b.fingerprint, (&a_cert, &a_key), 8445).unwrap();
    assert!(
        b_to_a
            .call_peer::<(), PeerIndex>(&a.address, "GET", on_appliance::PEER_INDEX, None)
            .await
            .is_ok()
    );
    assert!(
        a_to_b
            .call_peer::<(), PeerIndex>(&b.address, "GET", on_appliance::PEER_INDEX, None)
            .await
            .is_err()
    );
    // Nothing but the index and exports answers.
    let changed = b_to_a
        .call_peer::<_, serde_json::Value>(
            &a.address,
            "POST",
            "/v1/prune",
            Some(&serde_json::json!({})),
        )
        .await;
    assert!(matches!(
        changed,
        Err(CallError::Refused(_)) | Err(CallError::Unreachable(_))
    ));
}

#[test]
fn an_unpublished_receive_is_never_exported() {
    let mut j = journal();
    j.receiving.push(crate::journal::Receiving {
        cluster_uuid: CLUSTER.into(),
        vm_uuid: id(7),
        branch_uuid: id(0xb),
        pit_uuid: id(3),
        base: Some(id(2)),
        started_at_ms: 0,
        volumes: vec![],
    });
    let refused = plan_export(&j, &Grants::default(), &peer(), &request(3), 0).unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn untrusting_a_peer_ends_its_stream_in_progress() {
    let a = appliance_with(Some(
        "#!/bin/sh\nwhile :; do echo streaming-a-volume; sleep 0.05; done\n",
    ))
    .await;
    let (b_cert, b_key) = state::certificate("vault2", &id(2)).unwrap();
    trust(&a, id(2), &b_cert);
    let b_to_a = ClusterClient::peer(&a.fingerprint, (&b_cert, &b_key), 8445).unwrap();
    let mut stream = b_to_a
        .stream_peer(&a.address, on_appliance::EXPORT, &request(2))
        .await
        .unwrap();
    let first = stream.recv().await.unwrap().unwrap();
    assert!(first.starts_with(b"streaming"));
    let mut peers = Peers::load(&a.exporter.metadata).unwrap();
    peers.untrust(&id(2).to_string());
    peers.save(&a.exporter.metadata).unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match stream.recv().await {
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Some(e),
                None => return None,
            }
        }
    })
    .await
    .expect("the stream ends after the untrust");
    // A cut stream ends in an error, or without its end marker.
    let _ = ended;
    eventually(|| {
        let audit = a.exporter.audit.recent(5).unwrap();
        audit
            .iter()
            .any(|e| e.event == "export" && !e.details["error"].is_null())
            .then_some(())
    })
    .await;
}

/// Waits up to ten seconds for `check` to give something.
async fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(found) = check() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("it never happened");
}
