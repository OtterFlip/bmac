// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::collections::BTreeMap;

use bmac_proto::inventory::{INVENTORY_VERSION, ZfsLabel, ZfsPoolState};
use bmac_storage::{Encryption, Vdev, VdevClass, VdevType};

use super::*;

fn disk(name: &str, serial: &str, by_id: &str) -> InventoryDisk {
    InventoryDisk {
        name: name.into(),
        path: format!("/dev/{name}"),
        identity: Some(DiskIdentity::Serial(serial.into())),
        model: Some("QEMU NVMe Ctrl".into()),
        serial: Some(serial.into()),
        wwn: None,
        size_bytes: 64 << 30,
        logical_sector_bytes: 512,
        physical_sector_bytes: 512,
        rotational: false,
        removable: false,
        transport: Some("nvme".into()),
        by_id: vec![by_id.into()],
        udev_identity: BTreeMap::new(),
        partition_table: None,
        partitions: Vec::new(),
        luks: None,
        zfs_labels: Vec::new(),
        problems: Vec::new(),
    }
}

fn inventory(disks: Vec<InventoryDisk>) -> Inventory {
    Inventory {
        version: INVENTORY_VERSION,
        taken_ms: 1,
        system: SystemInfo {
            vendor: Some("Dell Inc.".into()),
            product: Some("PowerEdge R640".into()),
            serial: Some("ABC1234".into()),
        },
        cpu: Default::default(),
        memory_bytes: None,
        firmware: serde_json::from_value(
            serde_json::json!({"mode": "uefi", "secure_boot": "disabled"}),
        )
        .unwrap(),
        disks,
        nics: Vec::new(),
        link_wait: None,
    }
}

fn id(name: &str) -> ById {
    name.parse().unwrap()
}

/// A boot mirror and a single data disk, named as discovery saw them.
fn layout() -> PoolLayout {
    let mut boot = Vdev::new(
        VdevClass::Data,
        VdevType::Mirror,
        vec![id("nvme-QEMU_NVMe_Ctrl_S1"), id("nvme-QEMU_NVMe_Ctrl_S2")],
    );
    boot.boot = true;
    let data = Vdev::new(VdevClass::Data, VdevType::Single, vec![id("ata-DATA_S3")]);
    PoolLayout::new("rpool".parse().unwrap(), Encryption::None, vec![boot, data])
}

fn discovered() -> Inventory {
    inventory(vec![
        disk("nvme0n1", "S1", "nvme-QEMU_NVMe_Ctrl_S1"),
        disk("nvme1n1", "S2", "nvme-QEMU_NVMe_Ctrl_S2"),
        disk("sda", "S3", "ata-DATA_S3"),
    ])
}

/// The installed system's udev names the NVMe disks differently, and the
/// kernel numbered them the other way round.
fn installed() -> Vec<InventoryDisk> {
    vec![
        disk("nvme1n1", "S1", "nvme-QEMU_NVMe_Ctrl_S1_1"),
        disk("nvme0n1", "S2", "nvme-QEMU_NVMe_Ctrl_S2_1"),
        disk("sda", "S3", "ata-DATA_S3"),
    ]
}

fn members(layout: &PoolLayout) -> Vec<Vec<&str>> {
    layout
        .vdevs
        .iter()
        .map(|v| v.members.iter().map(ById::as_str).collect())
        .collect()
}

#[test]
fn members_are_found_again_by_serial_under_this_host_s_names() {
    let resolved = resolve_layout(&layout(), &discovered(), &installed()).unwrap();
    assert_eq!(
        members(&resolved),
        [
            vec!["nvme-QEMU_NVMe_Ctrl_S1_1", "nvme-QEMU_NVMe_Ctrl_S2_1"],
            vec!["ata-DATA_S3"],
        ]
    );
    assert!(resolved.vdevs[0].boot);
}

#[test]
fn a_missing_or_resized_or_doubled_disk_stops_the_job() {
    let mut live = installed();
    live.remove(2);
    let problems = resolve_layout(&layout(), &discovered(), &live).unwrap_err();
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("isn't on this host"), "{problems:?}");
    assert!(problems[0].contains("serial S3"), "{problems:?}");

    let mut live = installed();
    live[2].size_bytes += 1;
    let problems = resolve_layout(&layout(), &discovered(), &live).unwrap_err();
    assert!(problems[0].contains("bytes at discovery"), "{problems:?}");

    let mut live = installed();
    live.push(disk("sdb", "S3", "ata-OTHER_S3"));
    let problems = resolve_layout(&layout(), &discovered(), &live).unwrap_err();
    assert!(
        problems[0].contains("2 disks on this host report serial S3"),
        "{problems:?}"
    );
}

#[test]
fn a_member_discovery_never_saw_is_refused() {
    let mut seen = discovered();
    seen.disks.remove(0);
    let problems = resolve_layout(&layout(), &seen, &installed()).unwrap_err();
    assert!(problems[0].contains("doesn't list"), "{problems:?}");
}

#[test]
fn another_machine_is_refused() {
    let seen = discovered().system;
    assert!(same_machine(&seen, &seen).is_ok());
    let other = SystemInfo {
        serial: Some("XYZ9999".into()),
        ..seen.clone()
    };
    let error = same_machine(&seen, &other).unwrap_err();
    assert!(error.contains("serial number is XYZ9999"), "{error}");
    let unknown = SystemInfo {
        serial: None,
        ..seen.clone()
    };
    assert!(same_machine(&seen, &unknown).is_ok());
}

#[test]
fn only_labelled_data_disks_are_offered_for_clearing() {
    let mut live = installed();
    let label = |device: &str, guid: &str| ZfsLabel {
        device: device.into(),
        pool: Some("old".into()),
        pool_guid: Some(guid.into()),
        vdev_guid: None,
        state: ZfsPoolState::Exported,
        txg: None,
        hostname: None,
        copies: 4,
    };
    live[0].zfs_labels.push(label("/dev/nvme1n1p3", "1"));
    live[2].zfs_labels.push(label("/dev/sda1", "7"));
    live[2].zfs_labels.push(label("/dev/sda", "7"));
    let resolved = resolve_layout(&layout(), &discovered(), &live).unwrap();
    let labelled = labelled_data_disks(&resolved, &live);
    assert_eq!(labelled.len(), 1);
    assert_eq!(labelled[0].0.name, "sda");
    assert_eq!(labelled[0].1, ["7"]);
}

#[test]
fn setup_mode_lasts_until_the_job_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        bootstrap: dir.path().join("bootstrap.json"),
        done: dir.path().join("setup-complete"),
        cluster_record: dir.path().join("cluster.json"),
        join_state: dir.path().join("join.json"),
    };
    assert!(!in_setup_mode(&paths));
    std::fs::write(&paths.bootstrap, b"{}").unwrap();
    assert!(in_setup_mode(&paths));
    std::fs::write(&paths.done, b"").unwrap();
    assert!(!in_setup_mode(&paths));
}

#[test]
fn the_operation_is_native_admin_only_and_resumable() {
    let operation = operation();
    assert_eq!(operation.kind, OperationKind::Native);
    assert!(operation.script.is_none());
    assert_eq!(operation.min_role, Role::Admin);
    assert_eq!(operation.resume, ResumeRule::Checkpoint);
    crate::operations::Registry::new(vec![operation]).unwrap();
}

const FP: &str = "AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89";

#[test]
fn a_driven_run_names_its_driver_whole() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(parse_args(&[]).unwrap(), None);
    let driver = parse_args(&args(&[
        "--driven-by",
        "mox1",
        "--join-address",
        "10.20.0.11",
        "--join-fingerprint",
        &FP.to_lowercase(),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(driver.host, "mox1".parse().unwrap());
    assert_eq!(
        driver.address,
        "10.20.0.11".parse::<std::net::Ipv4Addr>().unwrap()
    );
    assert_eq!(driver.fingerprint, FP);
    assert!(parse_args(&args(&["--driven-by", "mox1"])).is_err());
    assert!(parse_args(&args(&["--driven-by", "mox1", "--driven-by", "mox2"])).is_err());
    assert!(
        parse_args(&args(&[
            "--driven-by",
            "mox1",
            "--join-address",
            "10.20.0.11",
            "--join-fingerprint",
            "AB:CD"
        ]))
        .is_err()
    );
    assert!(parse_args(&args(&["--other"])).is_err());
}

#[test]
fn retiring_the_setup_key_keeps_every_other_key() {
    let setup = "ssh-ed25519 AAAAsetup bmac-setup-mox2";
    let text = "ssh-ed25519 AAAAadmin admin@desk\nssh-ed25519 AAAAsetup other-comment\nssh-rsa AAAAcluster root@mox1\n";
    assert_eq!(
        without_key(text, setup),
        "ssh-ed25519 AAAAadmin admin@desk\nssh-rsa AAAAcluster root@mox1\n"
    );
    assert_eq!(without_key(text, "not a key"), text);
}

#[test]
fn retiring_writes_through_a_link_and_keeps_the_file_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("cluster_keys");
    std::fs::write(&real, "ssh-ed25519 AAAAadmin a\nssh-ed25519 AAAAsetup s\n").unwrap();
    let link = dir.path().join("authorized_keys");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(remove_authorized_key(&link, "ssh-ed25519 AAAAsetup").unwrap());
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        "ssh-ed25519 AAAAadmin a\n"
    );
    assert_eq!(
        std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!remove_authorized_key(&link, "ssh-ed25519 AAAAsetup").unwrap());
    assert!(!remove_authorized_key(&dir.path().join("missing"), "ssh-ed25519 AAAAsetup").unwrap());
}

#[test]
fn openssl_fingerprints_are_read_in_upper_case() {
    use super::super::cluster::openssl_fingerprint;
    assert_eq!(
        openssl_fingerprint(&format!("sha256 Fingerprint={}\n", FP.to_lowercase())).as_deref(),
        Some(FP)
    );
    assert_eq!(openssl_fingerprint("SHA256 Fingerprint=AB:CD"), None);
    assert_eq!(openssl_fingerprint(""), None);
}
