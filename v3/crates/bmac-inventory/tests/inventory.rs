// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The inventory against fixture `/sys` and `/dev` trees. Ports v2's
//! `test_cluster_setup_prereq.py` where it still applies.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use bmac_inventory::fixture::{DiskSpec, Fixture};
use bmac_inventory::nics::{
    LeaveAsIs, LinkControl, LinkVerdict, single_with_carrier, wait_for_links,
};
use bmac_inventory::zfs::member_label;
use bmac_inventory::{Root, take};
use bmac_proto::inventory::{
    FirmwareMode, Inventory, LinkWait, NicLink, PartitionTable, SecureBoot, ZfsPoolState,
};

const GB: u64 = 1_000_000_000;
const MIB: u64 = 1024 * 1024;

fn inventory(root: &Root) -> Inventory {
    take(root, None, &LeaveAsIs, &mut |_| {}, 1)
}

/// Records which NICs were brought up.
#[derive(Default)]
struct Recorder(RefCell<Vec<String>>);

impl LinkControl for Recorder {
    fn set_up(&self, nic: &str) -> Result<(), String> {
        self.0.borrow_mut().push(nic.to_string());
        if nic == "broken0" {
            Err("RTNETLINK answers: Operation not supported".into())
        } else {
            Ok(())
        }
    }
}

#[test]
fn lists_nvme_and_sata_disks_with_identity_and_geometry() {
    let dir = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(dir.path());
    fx.disk(&DiskSpec::nvme("nvme0n1", "S6BOOTA", 2 * GB));
    fx.disk(&DiskSpec::sata("sda", "ZA1B2C3D", GB));
    let disks = inventory(&fx.root()).disks;
    assert_eq!(
        disks.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        ["nvme0n1", "sda"]
    );

    let nvme = &disks[0];
    assert_eq!(nvme.serial.as_deref(), Some("S6BOOTA"));
    assert_eq!(nvme.size_bytes, 2 * GB);
    assert_eq!(nvme.transport.as_deref(), Some("nvme"));
    assert!(!nvme.rotational);
    assert!(nvme.wwn.as_deref().unwrap().starts_with("eui."));
    assert_eq!(
        nvme.identity.as_ref().unwrap().to_string(),
        format!("wwn:{}", nvme.wwn.as_deref().unwrap())
    );
    assert!(nvme.problems.is_empty());

    let sata = &disks[1];
    assert_eq!(sata.model.as_deref(), Some("ST4000NM0035"));
    assert_eq!(sata.transport.as_deref(), Some("sata"));
    assert_eq!(
        (sata.logical_sector_bytes, sata.physical_sector_bytes),
        (512, 4096)
    );
    assert!(sata.rotational);
    assert!(sata.wwn.as_deref().unwrap().starts_with("0x5000c500"));
}

#[test]
fn a_scsi_disk_reports_its_serial_not_its_wwn() {
    // Seen in the lab: udev's ID_SERIAL_SHORT for scsi-hd is the WWN.
    let dir = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(dir.path());
    fx.disk(&DiskSpec::scsi("sda", "MOX2S0D82E", GB));
    let disk = &inventory(&fx.root()).disks[0];
    assert_eq!(disk.serial.as_deref(), Some("MOX2S0D82E"));
    assert_eq!(disk.model.as_deref(), Some("QEMU HARDDISK"));
    assert_eq!(disk.transport.as_deref(), Some("scsi"));
}

#[test]
fn falls_back_to_sysfs_when_udev_has_no_entry() {
    let dir = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(dir.path());
    let mut spec = DiskSpec::nvme("nvme2n1", "S6SYSFS", 960_197_124_096);
    spec.udev = false;
    spec.model = Some("Samsung SSD 990 PRO 1TB".into());
    spec.wwn = None;
    fx.disk(&spec);
    let disk = &inventory(&fx.root()).disks[0];
    assert_eq!(disk.serial.as_deref(), Some("S6SYSFS"));
    assert_eq!(disk.model.as_deref(), Some("Samsung SSD 990 PRO 1TB"));
    assert_eq!(disk.size_bytes, 960_197_124_096);
    assert_eq!(
        disk.identity.as_ref().unwrap().to_string(),
        "serial:S6SYSFS"
    );
}

#[test]
fn skips_what_is_not_a_physical_disk() {
    let dir = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(dir.path());
    fx.disk(&DiskSpec::nvme("nvme0n1", "S6BOOTA", GB));
    for name in ["nvme0c0n1", "loop0", "sr0", "dm-0", "zd0"] {
        fx.disk(&DiskSpec::nvme(name, &format!("X{name}"), GB));
    }
    let mut empty = DiskSpec::sata("sdb", "CARDREADER", 0);
    empty.removable = true;
    fx.disk(&empty);
    let names: Vec<String> = inventory(&fx.root())
        .disks
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(names, ["nvme0n1"]);
}

#[test]
fn disks_that_cannot_be_told_apart_are_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(dir.path());
    for name in ["sda", "sdb"] {
        let mut spec = DiskSpec::sata(name, "SAMESERIAL", GB);
        spec.wwn = None;
        fx.disk(&spec);
    }
    // Same serial, different WWNs: the WWN tells them apart.
    fx.disk(&DiskSpec::sata("sdc", "DUP2", GB));
    let mut other = DiskSpec::sata("sdd", "DUP2", GB);
    other.wwn = Some("0x5000c500deadbeef".into());
    fx.disk(&other);
    let mut anonymous = DiskSpec::sata("sde", "x", GB);
    anonymous.serial = None;
    anonymous.wwn = None;
    fx.disk(&anonymous);

    let disks = inventory(&fx.root()).disks;
    let problems: BTreeMap<&str, &Vec<String>> = disks
        .iter()
        .map(|d| (d.name.as_str(), &d.problems))
        .collect();
    assert_eq!(problems["sda"].len(), 1);
    assert!(
        problems["sda"][0].contains("same serial number (SAMESERIAL)"),
        "{:?}",
        problems["sda"]
    );
    assert!(problems["sda"][0].contains("/dev/sdb"));
    assert!(problems["sdb"][0].contains("/dev/sda"));
    assert!(problems["sdc"].is_empty());
    assert!(problems["sdd"].is_empty());
    assert!(problems["sde"][0].contains("no serial number and no WWN"));
}

#[test]
fn reports_partitions_luks_and_old_zfs_labels() {
    let dir = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(dir.path());
    fx.disk(&DiskSpec::nvme("nvme0n1", "OLDRPOOL", 4 * 1024 * MIB).proxmox_layout());
    fx.gpt("nvme0n1", 512);
    fx.label(
        "nvme0n1p3",
        &member_label("rpool", 9_876_543_210_123_456_789, 11, 0, 500),
    );
    fx.disk(&DiskSpec::sata("sda", "LUKSDISK", GB).partition(1, MIB, 512 * MIB));
    fx.luks("sda1", 2, "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0");
    fx.disk(&DiskSpec::sata("sdb", "WHOLEZFS", GB));
    fx.label("sdb", &member_label("tank", 5, 6, 1, 70));

    let disks = inventory(&fx.root()).disks;
    let old = disks.iter().find(|d| d.name == "nvme0n1").unwrap();
    assert_eq!(old.partition_table, Some(PartitionTable::Gpt));
    assert_eq!(
        old.partitions.iter().map(|p| p.number).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(old.partitions[2].start_bytes, 1025 * MIB);
    assert_eq!(old.zfs_labels.len(), 1);
    let label = &old.zfs_labels[0];
    assert!(label.device.ends_with("/dev/nvme0n1p3"));
    assert_eq!(label.pool.as_deref(), Some("rpool"));
    assert_eq!(label.pool_guid.as_deref(), Some("9876543210123456789"));
    assert_eq!(label.state, ZfsPoolState::Active);

    let luks = disks.iter().find(|d| d.name == "sda").unwrap();
    assert_eq!(luks.partitions[0].luks.as_ref().unwrap().version, 2);
    assert!(luks.luks.is_none());
    assert!(luks.zfs_labels.is_empty());

    let whole = disks.iter().find(|d| d.name == "sdb").unwrap();
    assert_eq!(whole.zfs_labels[0].pool.as_deref(), Some("tank"));
    assert_eq!(whole.zfs_labels[0].state, ZfsPoolState::Exported);
    assert!(whole.zfs_labels[0].device.ends_with("/dev/sdb"));
}

#[test]
fn a_partition_running_to_the_end_is_not_a_whole_disk_label() {
    // Seen in the lab: the last copy of the rpool partition's label sits in
    // the whole disk's last label slot.
    use bmac_inventory::zfs::{NVLIST_OFFSET, label_offsets};
    use std::io::{Seek, SeekFrom, Write};
    let dir = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(dir.path());
    let size = 4 * 1024 * MIB;
    fx.disk(&DiskSpec::nvme("nvme0n1", "BOOT", size).proxmox_layout());
    fx.label("nvme0n1p3", &member_label("rpool", 1, 42, 0, 9));
    let last = *label_offsets(size).last().unwrap();
    let mut disk = fs::OpenOptions::new()
        .write(true)
        .open(fx.device("nvme0n1"))
        .unwrap();
    disk.seek(SeekFrom::Start(last + NVLIST_OFFSET)).unwrap();
    disk.write_all(&member_label("rpool", 1, 42, 0, 9)).unwrap();
    drop(disk);

    let disks = inventory(&fx.root()).disks;
    let devices: Vec<&str> = disks[0]
        .zfs_labels
        .iter()
        .map(|l| l.device.as_str())
        .collect();
    assert_eq!(devices.len(), 1, "{devices:?}");
    assert!(devices[0].ends_with("nvme0n1p3"));

    // A whole-disk label of another vdev is still reported.
    let mut disk = fs::OpenOptions::new()
        .write(true)
        .open(fx.device("nvme0n1"))
        .unwrap();
    disk.seek(SeekFrom::Start(last + NVLIST_OFFSET)).unwrap();
    disk.write_all(&member_label("old", 2, 43, 1, 9)).unwrap();
    drop(disk);
    assert_eq!(inventory(&fx.root()).disks[0].zfs_labels.len(), 2);
}

#[test]
fn lists_physical_nics_only_with_lowercase_macs() {
    let dir = tempfile::tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    fx.nic("eno1", "AA:BB:CC:DD:EE:01", Some(true), 10_000, "ice");
    fx.nic("eno2", "aa:bb:cc:dd:ee:02", Some(false), -1, "ice");
    fx.nic("eno3", "aa:bb:cc:dd:ee:03", None, -1, "tg3");
    fx.virtual_nic("lo", "00:00:00:00:00:00", "772");
    fx.virtual_nic("vmbr0", "aa:bb:cc:dd:ee:01", "1");
    fx.wireless_nic("wlp2s0", "aa:bb:cc:dd:ee:09");
    let nics = inventory(&fx.root()).nics;
    assert_eq!(
        nics.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
        ["eno1", "eno2", "eno3"]
    );
    assert_eq!(nics[0].mac, "aa:bb:cc:dd:ee:01");
    assert_eq!(nics[0].driver.as_deref(), Some("ice"));
    assert_eq!(nics[0].bus.as_deref(), Some("pci"));
    assert_eq!(
        nics[0].link,
        NicLink::Up {
            speed_mbps: Some(10_000),
            duplex: Some("full".into()),
            waited_seconds: 0
        }
    );
    assert_eq!(nics[1].link, NicLink::NoCarrier { waited_seconds: 0 });
    assert_eq!(nics[2].link, NicLink::NoCarrier { waited_seconds: 0 });
}

#[test]
fn link_wait_brings_nics_up_and_waits_for_slow_carrier() {
    let dir = tempfile::tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    fx.nic("slow0", "aa:bb:cc:dd:ee:01", None, -1, "ice");
    fx.nic("dead0", "aa:bb:cc:dd:ee:02", Some(false), -1, "ice");
    fx.nic("gig0", "aa:bb:cc:dd:ee:03", Some(true), 1000, "tg3");
    fx.nic("broken0", "aa:bb:cc:dd:ee:04", None, -1, "tg3");
    let nics: Vec<String> = ["slow0", "dead0", "gig0", "broken0"]
        .map(String::from)
        .to_vec();
    let control = Recorder::default();
    let mut sleeps = Vec::new();
    let links = wait_for_links(
        &fx.root(),
        &nics,
        &LinkWait::default(),
        &control,
        &mut |d| {
            sleeps.push(d);
            // Carrier on the 10 Gb/s link appears on the fourth read.
            if sleeps.len() == 3 {
                fx.set_link("slow0", true, 10_000);
            }
        },
    );
    assert_eq!(*control.0.borrow(), nics);
    assert_eq!(sleeps.len(), 9, "ten reads, three seconds apart");
    assert!(sleeps.iter().all(|d| *d == Duration::from_secs(3)));

    assert_eq!(
        links["slow0"],
        NicLink::Up {
            speed_mbps: Some(10_000),
            duplex: Some("full".into()),
            waited_seconds: 9
        }
    );
    assert_eq!(
        LinkVerdict::of(&links["slow0"], Some(10_000)),
        LinkVerdict::Ok
    );

    assert_eq!(links["dead0"], NicLink::NoCarrier { waited_seconds: 27 });
    // Never reported as the wrong speed, even when a speed is expected.
    let dead = LinkVerdict::of(&links["dead0"], Some(10_000));
    assert_eq!(dead, LinkVerdict::NoCarrier { waited_seconds: 27 });
    assert!(
        dead.describe("dead0")
            .contains("no carrier after about 27 seconds")
    );

    let gig = LinkVerdict::of(&links["gig0"], Some(10_000));
    assert_eq!(
        gig,
        LinkVerdict::WrongSpeed {
            expected_mbps: 10_000,
            actual_mbps: Some(1000)
        }
    );
    assert!(
        gig.describe("gig0")
            .contains("1000 Mb/s, not the expected 10000 Mb/s")
    );

    assert!(
        matches!(&links["broken0"], NicLink::Error { message } if message.contains("not supported"))
    );
}

#[test]
fn link_wait_stops_as_soon_as_every_nic_has_carrier() {
    let dir = tempfile::tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    fx.nic("eno1", "aa:bb:cc:dd:ee:01", Some(true), 1000, "ice");
    let mut sleeps = 0;
    let links = wait_for_links(
        &fx.root(),
        &["eno1".into()],
        &LinkWait::default(),
        &LeaveAsIs,
        &mut |_| sleeps += 1,
    );
    assert_eq!(sleeps, 0);
    assert!(matches!(
        links["eno1"],
        NicLink::Up {
            waited_seconds: 0,
            ..
        }
    ));
}

#[test]
fn speed_is_read_only_after_carrier() {
    let dir = tempfile::tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    // Speed reads as a stale value while negotiating; carrier never comes.
    fx.nic("eno1", "aa:bb:cc:dd:ee:01", Some(false), 10_000, "ice");
    let wait = LinkWait {
        attempts: 2,
        interval_seconds: 1,
    };
    let links = wait_for_links(&fx.root(), &["eno1".into()], &wait, &LeaveAsIs, &mut |_| {});
    assert_eq!(links["eno1"], NicLink::NoCarrier { waited_seconds: 1 });
}

#[test]
fn automatic_detection_needs_exactly_one_nic_with_carrier() {
    let up = || NicLink::Up {
        speed_mbps: Some(10_000),
        duplex: None,
        waited_seconds: 0,
    };
    let none = || NicLink::NoCarrier { waited_seconds: 27 };
    let links = BTreeMap::from([("eno1".to_string(), none()), ("eno2".to_string(), up())]);
    assert_eq!(single_with_carrier(&links), Ok("eno2".into()));
    let links = BTreeMap::from([("eno1".to_string(), up()), ("eno2".to_string(), up())]);
    assert_eq!(
        single_with_carrier(&links),
        Err(vec!["eno1".into(), "eno2".into()])
    );
    let links = BTreeMap::from([("eno1".to_string(), none())]);
    assert_eq!(single_with_carrier(&links), Err(vec![]));
}

#[test]
fn reports_cpu_memory_firmware_and_system() {
    let dir = tempfile::tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    fx.cpuinfo(2, 16, 2, "Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz");
    fx.meminfo(263_921_208);
    fx.uefi(Some(true));
    fx.dmi("Dell Inc.", "PowerEdge R650", "7XYZ123");
    let inv = inventory(&fx.root());
    assert_eq!(
        inv.cpu.model.as_deref(),
        Some("Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz")
    );
    assert_eq!(
        (inv.cpu.sockets, inv.cpu.cores, inv.cpu.threads),
        (2, 32, 64)
    );
    assert_eq!(inv.memory_bytes, Some(263_921_208 * 1024));
    assert_eq!(inv.firmware.mode, FirmwareMode::Uefi);
    assert_eq!(inv.firmware.secure_boot, SecureBoot::Enabled);
    assert_eq!(inv.system.serial.as_deref(), Some("7XYZ123"));

    fx.uefi(Some(false));
    assert_eq!(
        inventory(&fx.root()).firmware.secure_boot,
        SecureBoot::Disabled
    );
    fs::remove_dir_all(dir.path().join("sys/firmware")).unwrap();
    let bios = inventory(&fx.root()).firmware;
    assert_eq!(
        (bios.mode, bios.secure_boot),
        (FirmwareMode::Bios, SecureBoot::NotApplicable)
    );
}

#[test]
fn placeholder_dmi_values_are_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    fx.dmi(
        "QEMU",
        "Standard PC (Q35 + ICH9, 2009)",
        "To Be Filled By O.E.M.",
    );
    let system = inventory(&fx.root()).system;
    assert_eq!(system.vendor.as_deref(), Some("QEMU"));
    assert_eq!(system.serial, None);
}

#[test]
fn reports_an_empty_machine() {
    let dir = tempfile::tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    let inv = inventory(&fx.root());
    assert!(inv.disks.is_empty());
    assert!(inv.nics.is_empty());
    assert_eq!(inv.memory_bytes, None);
}

/// Every file's size and modification time; device files are gigabytes of
/// sparse zeros, too big to compare by content.
fn snapshot(dir: &Path) -> BTreeMap<String, (u64, std::time::SystemTime)> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let meta = entry.metadata().unwrap();
                files.insert(
                    path.display().to_string(),
                    (meta.len(), meta.modified().unwrap()),
                );
            }
        }
    }
    files
}

#[test]
fn taking_the_inventory_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(dir.path());
    fx.disk(&DiskSpec::nvme("nvme0n1", "OLDRPOOL", 2 * 1024 * MIB).proxmox_layout());
    fx.label("nvme0n1p3", &member_label("rpool", 1, 2, 0, 3));
    fx.nic("eno1", "aa:bb:cc:dd:ee:01", Some(true), 1000, "ice");
    let before = snapshot(dir.path());
    inventory(&fx.root());
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn the_binary_prints_the_inventory_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(dir.path());
    fx.disk(&DiskSpec::nvme("nvme0n1", "S6BOOTA", GB));
    fx.nic("eno1", "aa:bb:cc:dd:ee:01", Some(true), 1000, "ice");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bmac-inventory"))
        .args([
            "--root",
            dir.path().to_str().unwrap(),
            "--link-attempts",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let inv: Inventory = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(inv.disks[0].serial.as_deref(), Some("S6BOOTA"));
    assert_eq!(
        inv.link_wait,
        Some(LinkWait {
            attempts: 1,
            interval_seconds: 3
        })
    );
    assert!(matches!(inv.nics[0].link, NicLink::Up { .. }));
}

#[test]
fn the_binary_rejects_bad_arguments() {
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_bmac-inventory"))
            .args(args)
            .output()
            .unwrap()
    };
    let help = run(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("clear-zfs-labels"));
    assert_eq!(run(&["--host"]).status.code(), Some(2));
    assert_eq!(run(&["--link-attempts", "0"]).status.code(), Some(2));
    assert_eq!(run(&["--link-interval", "61"]).status.code(), Some(2));
    assert_eq!(
        run(&["clear-zfs-labels", "--no-link-wait"]).status.code(),
        Some(2)
    );
}
