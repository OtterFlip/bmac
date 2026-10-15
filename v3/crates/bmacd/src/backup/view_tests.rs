// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::collections::BTreeMap;

use bmac_backup::lifetime::Offer;
use bmac_backup::manifest::Consistency;
use bmac_backup::records::{
    Lineage, PitRecord, Presence, RECORD_SCHEMA, RestoreRecord, TestOutcome,
};
use bmac_proto::backups::{ApplianceList, ApplianceView, TransferView};
use bmac_proto::status::{
    BackupApplianceStatus, BackupsObservation, PitSnapshotSpace, StorageObservation,
};
use uuid::Uuid;

use super::*;
use crate::schedules::Recurrence;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn pit(n: u128, parent: Option<u128>, taken_s: i64, offers: BTreeMap<Uuid, Offer>) -> PitRecord {
    PitRecord {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 1,
        pit_uuid: id(n),
        vm_uuid: id(7),
        branch_uuid: id(0xb),
        parent: parent.map(id),
        name: format!("bmac-pit-20261013T03000{n}Z-a1b2c3")
            .parse()
            .unwrap(),
        taken_s,
        consistency: Consistency::Filesystem,
        manifest: String::new(),
        presence: Presence::Snapshot,
        offers,
        release_blocked: None,
    }
}

fn storage(pit_snapshots: Vec<PitSnapshotSpace>) -> StorageObservation {
    StorageObservation {
        at_ms: 1,
        pools: vec![],
        storages: vec![],
        vdevs: vec![],
        disks: vec![],
        findings: vec![],
        luks_mismatches: vec![],
        pit_snapshots,
        errors: vec![],
    }
}

fn appliance(uuid: Uuid) -> ApplianceView {
    ApplianceView {
        appliance_uuid: uuid.to_string(),
        name: "vault".into(),
        fingerprint: "AB:CD".into(),
        address: Some("100.64.0.9".into()),
        enrollment: "approved".into(),
        enrolled_at_ms: 1,
        last_checkin_ms: Some(2),
        version: None,
        locked: false,
        paused: false,
        retired: false,
    }
}

#[test]
fn schedules_read_as_words() {
    assert_eq!(
        describe(&Recurrence::Interval { minutes: 15 }),
        "every 15 minutes"
    );
    assert_eq!(describe(&Recurrence::Interval { minutes: 60 }), "hourly");
    assert_eq!(
        describe(&Recurrence::Interval { minutes: 360 }),
        "every 6 hours"
    );
    assert_eq!(
        describe(&Recurrence::Daily { at: "03:00".into() }),
        "daily at 03:00"
    );
    assert_eq!(
        describe(&Recurrence::Monthly {
            day: 1,
            at: "04:30".into()
        }),
        "monthly on day 1 at 04:30"
    );
}

#[test]
fn the_view_joins_catalog_offers_space_restores_and_transfers() {
    let vault = id(0xa);
    let offers = |offer| BTreeMap::from([(vault, offer)]);
    let lineage = Lineage {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 1,
        vm_uuid: id(7),
        branch_uuid: id(0xb),
        head: Some(id(2)),
        volumes: BTreeMap::from([("vm-100-disk-0".to_string(), id(0xd))]),
        last_attempt: None,
    };
    let restore = |n: u128, started_at_ms| RestoreRecord {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 1,
        restore_uuid: id(0x100 + n),
        vm_uuid: id(7),
        pit_uuid: id(1),
        branch_uuid: id(0xb),
        appliance_uuid: vault,
        mode: bmac_backup::restore::Mode::Test,
        host: "mox2".into(),
        vmid: Some(900),
        name: Some("bmac-test-prod1".into()),
        methods: BTreeMap::from([("scsi0".to_string(), 3)]),
        started_at_ms,
        finished_at_ms: Some(started_at_ms + 10),
        error: None,
        ended_at_ms: Some(started_at_ms + 20),
        replaced: BTreeMap::new(),
        test: Some(TestOutcome {
            booted: true,
            agent: true,
            agent_after_s: Some(12),
        }),
        manifest: None,
    };
    let transfer = |at_ms| TransferView {
        at_ms,
        host: "mox1".into(),
        appliance: "vault".into(),
        vm_uuid: id(7).to_string(),
        pit_uuid: id(1).to_string(),
        volume_uuid: id(0xd).to_string(),
        bytes: 1 << 20,
        duration_ms: 1000,
        error: None,
    };
    let view = assemble(Inputs {
        now_ms: 99,
        time_zone: None,
        appliances: Some(ApplianceList {
            appliances: vec![appliance(vault)],
            requests: vec![],
            can_manage: true,
        }),
        vms: BTreeMap::from([(
            id(7),
            VmRecords {
                policy: None,
                every: None,
                lineage: Some(lineage),
                pits: vec![
                    pit(1, None, 10, offers(Offer::Published)),
                    pit(
                        2,
                        Some(1),
                        20,
                        offers(Offer::Claimed {
                            progress_s: 25,
                            bytes: 4096,
                        }),
                    ),
                ],
            },
        )]),
        restores: vec![restore(1, 100), restore(2, 200)],
        transfers: vec![transfer(5), transfer(50)],
        backups: Some(BackupsObservation {
            at_ms: 98,
            appliances: vec![BackupApplianceStatus {
                appliance_uuid: vault.to_string(),
                name: "vault".into(),
                state: "ACTIVE".into(),
                locked: false,
                last_checkin_ms: Some(97),
                scrub_errors: 0,
                suspect_pits: 0,
                other_copies: vec![],
            }],
            vms: vec![],
            errors: vec!["mox3: not answering".into()],
        }),
        storage: vec![
            (
                "mox1".into(),
                storage(vec![
                    PitSnapshotSpace {
                        dataset: "rpool/data/vm-100-disk-0".into(),
                        snapshots: 2,
                        used_bytes: 300,
                    },
                    PitSnapshotSpace {
                        dataset: "rpool/data/vm-101-disk-0".into(),
                        snapshots: 5,
                        used_bytes: 9999,
                    },
                ]),
            ),
            ("mox3".into(), storage(vec![])),
        ],
        names: BTreeMap::from([(id(7), ("prod1".to_string(), 100))]),
        can_manage: false,
        errors: vec![],
        settings: bmac_backup::settings::BackupSettings {
            checkin_interval_s: 10,
            test_day_s: Some(300),
            ..Default::default()
        },
        settings_revision: 4,
    });
    assert_eq!(
        (
            view.settings.checkin_interval_s,
            view.settings.late_after_s,
            view.settings.test_day_s,
            view.settings_revision
        ),
        (10, 180, Some(300), 4)
    );
    let [vm] = view.vms.as_slice() else {
        panic!("{:?}", view.vms)
    };
    assert_eq!((vm.vm.as_str(), vm.vmid), ("prod1", Some(100)));
    assert_eq!(vm.pits.len(), 2);
    assert_eq!(vm.pits[1].parent, Some(id(1).to_string()));
    assert_eq!(vm.pits[0].offers[0].state, "published");
    assert_eq!(vm.pits[1].offers[0].state, "claimed");
    assert_eq!(vm.pits[1].offers[0].bytes, Some(4096));
    assert_eq!(vm.pits[0].presence, "snapshot");
    assert_eq!(vm.pits[0].consistency, "filesystem");
    assert_eq!(vm.local.len(), 1, "only hosts holding its PIT snapshots");
    assert_eq!(
        (
            vm.local[0].host.as_str(),
            vm.local[0].snapshots,
            vm.local[0].bytes
        ),
        ("mox1", 2, 300)
    );
    let [shown] = view.appliances.as_slice() else {
        panic!()
    };
    assert_eq!(shown.status.as_ref().unwrap().state, "ACTIVE");
    assert_eq!(shown.live.len(), 1);
    assert_eq!(
        (shown.live[0].pit_uuid.clone(), shown.live[0].bytes),
        (id(2).to_string(), 4096)
    );
    assert_eq!(
        view.restores[0].restore_uuid,
        id(0x102).to_string(),
        "newest first"
    );
    assert_eq!(view.restores[0].mode, "test");
    assert!(view.restores[0].test.as_ref().unwrap().booted);
    assert_eq!(view.transfers[0].at_ms, 50, "newest first");
    assert_eq!(view.errors, ["mox3: not answering"]);
    assert!(!view.can_manage);
}

#[test]
fn a_policy_shows_its_schedule_and_retention() {
    let policy = bmac_backup::records::Policy {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 3,
        vm_uuid: id(7),
        vm: "prod1".into(),
        schedule_id: "backup-x".into(),
        appliances: vec![id(0xa)],
        local_retention: bmac_backup::retention::LocalRetention {
            newest: 2,
            days: 7,
            weeks: 0,
        },
        started_s: None,
        freeze: bmac_backup::records::Freeze::Required,
        accept_multi_pool: false,
        enabled: true,
        updated_by: "admin".into(),
        updated_at_ms: 5,
    };
    let every = Recurrence::Interval { minutes: 15 };
    let view = assemble(Inputs {
        vms: BTreeMap::from([(
            id(7),
            VmRecords {
                policy: Some(policy.clone()),
                every: Some(every.clone()),
                ..Default::default()
            },
        )]),
        ..Default::default()
    });
    let shown = view.vms[0].policy.as_ref().unwrap();
    assert_eq!(
        view.vms[0].vm, "prod1",
        "the policy's name without a registry entry"
    );
    assert_eq!(
        (shown.revision, shown.local_newest, shown.local_days),
        (3, 2, 7)
    );
    assert_eq!(shown.every, "every 15 minutes");
    let daily = assemble(Inputs {
        vms: BTreeMap::from([(
            id(7),
            VmRecords {
                policy: Some(policy),
                every: Some(Recurrence::Daily { at: "03:00".into() }),
                ..Default::default()
            },
        )]),
        time_zone: Some("America/Denver".into()),
        ..Default::default()
    });
    assert_eq!(
        daily.vms[0].policy.as_ref().unwrap().every,
        "daily at 03:00 America/Denver",
        "a time of day is the cluster's"
    );
    assert_eq!(
        serde_json::from_str::<Recurrence>(&shown.every_json).unwrap(),
        every
    );
    assert_eq!(shown.freeze, "required");
    assert_eq!(shown.appliances, [id(0xa).to_string()]);
}

#[test]
fn transfers_are_kept_per_host_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let pmxcfs = bmac_pmxcfs::Pmxcfs::new(dir.path(), std::sync::Arc::new(bmac_pmxcfs::RealFs));
    let store = super::super::store::BackupStore::new(pmxcfs, dir.path().join("backup"));
    let transfer = |host: &str, at_ms| TransferView {
        at_ms,
        host: host.into(),
        appliance: "vault".into(),
        vm_uuid: id(7).to_string(),
        pit_uuid: id(1).to_string(),
        volume_uuid: id(0xd).to_string(),
        bytes: 1,
        duration_ms: 1,
        error: None,
    };
    for at in 0..(super::super::store::KEEP_TRANSFERS as i64 + 5) {
        store
            .locked(|l| l.record_transfer(transfer("mox1", at)))
            .unwrap();
    }
    store
        .locked(|l| l.record_transfer(transfer("mox2", 1000)))
        .unwrap();
    let all = store.transfers().unwrap();
    assert_eq!(all.len(), super::super::store::KEEP_TRANSFERS + 1);
    assert_eq!((all[0].host.as_str(), all[0].at_ms), ("mox2", 1000));
    assert_eq!(
        all.iter()
            .filter(|t| t.host == "mox1")
            .map(|t| t.at_ms)
            .min(),
        Some(5)
    );
}
