// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Layout checks and the review summary, with the operator-facing wording.

use bmac_core::{Bytes, DiskIdentity};
use bmac_storage::checks::{Code, check, installer_raid, required_ashift};
use bmac_storage::geometry::BlockSize;
use bmac_storage::layout::{ById, Encryption, PoolLayout, Vdev, VdevClass, VdevType};
use bmac_storage::summary::summarize;
use bmac_storage::{Disk, Target, Transport};

const TB: u64 = 1_000_000_000_000;

fn disk(name: &str, size: u64) -> Disk {
    Disk {
        by_id: name.parse().unwrap(),
        identity: Some(DiskIdentity::Wwn(format!("0x{name}"))),
        size,
        logical_sector: 512,
        physical_sector: 4096,
        transport: Transport::Sata,
        rotational: true,
        model: "ST4000NM0035".to_string(),
        hardware_raid: false,
        in_use: None,
    }
}

fn disks(prefix: &str, n: usize, size: u64) -> Vec<Disk> {
    (0..n)
        .map(|i| disk(&format!("{prefix}{i}"), size))
        .collect()
}

fn ids(disks: &[Disk]) -> Vec<ById> {
    disks.iter().map(|d| d.by_id.clone()).collect()
}

fn layout(encryption: Encryption, vdevs: Vec<Vdev>) -> PoolLayout {
    PoolLayout::new("rpool".parse().unwrap(), encryption, vdevs)
}

fn codes(findings: &[bmac_storage::Finding]) -> Vec<Code> {
    findings.iter().map(|f| f.code).collect()
}

#[test]
fn a_mirror_host_has_no_findings() {
    let inv = disks("a", 2, 2 * TB);
    let l = layout(
        Encryption::Luks2,
        vec![Vdev::new(VdevClass::Data, VdevType::Mirror, ids(&inv)).boot()],
    );
    let f = check(&l, &inv, Target::Host);
    assert!(f.is_allowed(), "{f:?}");
    assert!(f.warnings.is_empty(), "{f:?}");
    assert!(!f.needs_force);
}

#[test]
fn refusals() {
    let mut inv = disks("a", 4, 2 * TB);
    inv[0].identity = None;
    inv[1].hardware_raid = true;
    inv[2].in_use = Some("member of pool tank".to_string());
    inv[3].size = 4 * Bytes::GIB;
    let mut members = ids(&inv);
    members.push(members[3].clone());
    members.push("wwn-missing".parse().unwrap());
    let l = layout(
        Encryption::None,
        vec![Vdev::new(VdevClass::Data, VdevType::Raidz2, members).boot()],
    );
    let f = check(&l, &inv, Target::Host);
    assert_eq!(
        codes(&f.refused),
        [
            Code::NoIdentity,
            Code::HardwareRaid,
            Code::DiskInUse,
            Code::DiskTooSmall,
            Code::DuplicateDisk,
            Code::UnknownDisk
        ]
    );
    let messages: Vec<&str> = f.refused.iter().map(|r| r.message.as_str()).collect();
    assert_eq!(
        messages[0],
        "disk a0 reports no serial number and no WWN, so BMAC cannot tell it apart from others"
    );
    assert_eq!(
        messages[1],
        "disk a1 is a logical disk presented by a hardware RAID controller; put the controller in HBA or \
         passthrough mode so ZFS and SMART see the real drive"
    );
    assert_eq!(
        messages[2],
        "disk a2 is already in use: member of pool tank"
    );
    assert_eq!(
        messages[3],
        "disk a3 has 4 GiB, less than the 8 GiB minimum"
    );
    assert_eq!(messages[4], "disk a3 is listed twice");
    assert_eq!(messages[5], "disk wwn-missing is not in the inventory");
}

#[test]
fn two_names_for_one_disk_are_refused() {
    let mut inv = disks("a", 2, 2 * TB);
    inv[1].identity = inv[0].identity.clone();
    let l = layout(
        Encryption::None,
        vec![Vdev::new(VdevClass::Data, VdevType::Mirror, ids(&inv)).boot()],
    );
    let f = check(&l, &inv, Target::Host);
    assert_eq!(codes(&f.refused), [Code::DuplicateDisk]);
    assert_eq!(
        f.refused[0].message,
        "disks a0 and a1 report the same identity, 0xa0; they are one disk"
    );
}

#[test]
fn structure_errors_stop_the_checks() {
    let inv = disks("a", 2, 2 * TB);
    let l = layout(
        Encryption::None,
        vec![Vdev::new(VdevClass::Data, VdevType::Raidz1, ids(&inv)).boot()],
    );
    let f = check(&l, &inv, Target::Host);
    assert_eq!(codes(&f.refused), [Code::Structure]);
    assert_eq!(
        f.refused[0].message,
        "vdev 0: raidz1 needs at least 3 disks"
    );
}

#[test]
fn spares_must_cover_the_largest_data_member() {
    let mut inv = disks("a", 2, 2 * TB);
    inv[1].size = 2 * TB + Bytes::GIB;
    let spare = disk("s0", 2 * TB);
    let l = layout(
        Encryption::None,
        vec![
            Vdev::new(VdevClass::Data, VdevType::Mirror, ids(&inv)).boot(),
            Vdev::new(
                VdevClass::Spare,
                VdevType::Single,
                vec![spare.by_id.clone()],
            ),
        ],
    );
    inv.push(spare);
    let f = check(&l, &inv, Target::Host);
    assert_eq!(codes(&f.refused), [Code::SpareTooSmall]);
    assert_eq!(
        f.refused[0].message,
        format!(
            "spare s0 ({}) is smaller than data member a1 ({}), which it could be asked to replace",
            Bytes(2 * TB),
            Bytes(2 * TB + Bytes::GIB)
        )
    );
}

#[test]
fn boot_vdev_rules() {
    let inv = disks("a", 3, 2 * TB);
    let single = |enc| {
        layout(
            enc,
            vec![Vdev::new(VdevClass::Data, VdevType::Single, ids(&inv[..1])).boot()],
        )
    };
    let f = check(&single(Encryption::Luks2), &inv, Target::Host);
    assert_eq!(codes(&f.refused), [Code::LuksSingleBoot]);
    assert_eq!(
        f.refused[0].message,
        "an encrypted host needs a redundant boot vdev (a mirror or raidz), because the boot vdev is \
         encrypted in place one disk at a time after installation"
    );
    let f = check(&single(Encryption::None), &inv, Target::Host);
    assert!(f.is_allowed());
    assert_eq!(codes(&f.warnings), [Code::NonRedundantData]);

    let none = layout(
        Encryption::None,
        vec![Vdev::new(VdevClass::Data, VdevType::Mirror, ids(&inv[..2]))],
    );
    let f = check(&none, &inv, Target::Host);
    assert_eq!(
        f.refused[0].message,
        "a Proxmox host's pool needs exactly one boot vdev, not 0"
    );
    assert!(check(&none, &inv, Target::Appliance).is_allowed());
    let f = check(&single(Encryption::None), &inv, Target::Appliance);
    assert_eq!(codes(&f.refused), [Code::BootOnAppliance]);

    let mut mixed = inv.clone();
    mixed[1].logical_sector = 4096;
    let l = layout(
        Encryption::None,
        vec![Vdev::new(VdevClass::Data, VdevType::Mirror, ids(&mixed[..2])).boot()],
    );
    let f = check(&l, &mixed, Target::Host);
    assert_eq!(codes(&f.refused), [Code::BootSectorMismatch]);
    assert!(
        f.refused[0]
            .message
            .starts_with("the boot vdev mixes 512-byte and 4096-byte logical sectors")
    );

    let installer: Vec<_> = VdevClass::Data
        .allowed_types()
        .iter()
        .map(|k| installer_raid(*k).unwrap())
        .collect();
    assert_eq!(
        installer,
        ["raid0", "raid1", "raidz-1", "raidz-2", "raidz-3"]
    );
}

#[test]
fn ashift_follows_the_largest_physical_sector() {
    let mut inv = disks("a", 2, 2 * TB);
    assert_eq!(required_ashift(&inv), 12);
    inv[1].physical_sector = 8192;
    assert_eq!(required_ashift(&inv), 13);
    let l = layout(
        Encryption::None,
        vec![Vdev::new(VdevClass::Data, VdevType::Mirror, ids(&inv)).boot()],
    );
    let f = check(&l, &inv, Target::Host);
    assert_eq!(codes(&f.refused), [Code::AshiftTooSmall]);
    assert_eq!(
        f.refused[0].message,
        "disk a1 has 8192-byte physical sectors, which needs ashift 13; the layout uses 12"
    );
}

#[test]
fn warnings() {
    let mut boot = disks("a", 2, 2 * TB);
    boot[1].size = 4 * TB;
    let mut fast = disks("n", 2, TB);
    fast[1].rotational = false;
    fast[1].transport = Transport::Nvme;
    let mut usb = disk("u0", 4 * TB);
    usb.transport = Transport::Usb;
    let log = disk("l0", TB);
    let l = layout(
        Encryption::None,
        vec![
            Vdev::new(VdevClass::Data, VdevType::Mirror, ids(&boot)).boot(),
            Vdev::new(VdevClass::Data, VdevType::Single, vec![usb.by_id.clone()]),
            Vdev::new(VdevClass::Special, VdevType::Mirror, ids(&fast)),
            Vdev::new(VdevClass::Log, VdevType::Single, vec![log.by_id.clone()]),
        ],
    );
    let inv: Vec<Disk> = boot.into_iter().chain(fast).chain([usb, log]).collect();
    let f = check(&l, &inv, Target::Host);
    assert!(f.is_allowed(), "{f:?}");
    assert!(f.needs_force);
    assert_eq!(
        codes(&f.warnings),
        [
            Code::UsbDisk,
            Code::MixedCapacity,
            Code::MixedSpeed,
            Code::MismatchedRedundancy,
            Code::NonRedundantData,
            Code::SingleLog
        ]
    );
    let text: Vec<&str> = f.warnings.iter().map(|w| w.message.as_str()).collect();
    assert_eq!(
        text[0],
        "disk u0 is attached by USB; USB bridges drop out under load and often hide SMART data"
    );
    assert!(text[1].starts_with("the 2-way data mirror mixes disks of 1.82 TiB and 3.64 TiB"));
    assert_eq!(
        text[2],
        "the 2-way special mirror mixes SSDs with spinning disks; it runs at the speed of its slowest member"
    );
    assert_eq!(
        text[3],
        "the data vdevs survive different numbers of disk failures; the pool is only as safe as vdev 1, a \
         single-disk data vdev that survives 0 failures. ZFS itself needs -f for this, and BMAC passes it only \
         after you acknowledge this warning"
    );
    assert_eq!(
        text[4],
        "data vdev 1 is a single disk: if it fails the whole pool is lost, and ZFS can detect but not repair \
         corruption on it"
    );
    assert_eq!(
        text[5],
        "log vdev 3 is a single disk; if it fails while the host crashes, the last few seconds of synchronous \
         writes are lost"
    );
}

#[test]
fn shape_mismatch_and_weak_special_vdev() {
    let a = disks("a", 3, 2 * TB);
    let b = disks("b", 4, 2 * TB);
    let s = disks("s", 2, TB);
    let l = layout(
        Encryption::None,
        vec![
            Vdev::new(VdevClass::Data, VdevType::Raidz2, ids(&b)).boot(),
            Vdev::new(VdevClass::Data, VdevType::Mirror, ids(&a)),
            Vdev::new(VdevClass::Special, VdevType::Mirror, ids(&s)),
        ],
    );
    let inv: Vec<Disk> = a.into_iter().chain(b).chain(s).collect();
    let f = check(&l, &inv, Target::Host);
    assert_eq!(
        codes(&f.warnings),
        [Code::MismatchedShape, Code::HelperLessRedundant]
    );
    assert!(f.needs_force);
    assert_eq!(
        f.warnings[0].message,
        "the data vdevs differ in type or width (3-way data mirror, data raidz2 of 4 disks). ZFS itself needs -f \
         for this, and BMAC passes it only after you acknowledge this warning"
    );
    assert_eq!(
        f.warnings[1].message,
        "special vdev 2 survives 1 failure but the data vdevs survive 2 failures; losing the special vdev loses \
         the whole pool"
    );
}

#[test]
fn raidz_summary_states_what_it_rules_out() {
    let inv = disks("a", 6, 4 * TB);
    let l = layout(
        Encryption::None,
        vec![Vdev::new(VdevClass::Data, VdevType::Raidz1, ids(&inv))],
    );
    let s = summarize(&l, &inv, Target::Appliance, &BlockSize::OFFERED);
    assert!(s.findings.is_allowed());
    let member = s.vdevs[0].member_bytes.unwrap();
    assert_eq!(member, (4 * TB / Bytes::GIB - 1) * Bytes::GIB - Bytes::MIB);
    assert_eq!(s.nominal, member * 5);
    assert_eq!(s.reported, member * 6 * 4 / 5);
    // Inflation 1.2 at 16 KiB, applied to the reported figure.
    assert_eq!(s.vm[0].bytes, s.reported * 5 / 6);
    assert_eq!(s.vm[1].bytes, s.reported);
    assert_eq!(s.suggested_block_size, Some(BlockSize::K32));
    assert_eq!(
        s.vdevs[0].loss,
        "Survives 1 disk failure; losing more loses the whole pool."
    );
    // Each member's partition is 3724 GiB less 1 MiB.
    assert_eq!(
        s.statements[0],
        "6 × 3.64 TiB raidz1: nominal 18.18 TiB (5 of 6 disks), ZFS will report about 17.46 TiB, \
         VM disks at 16 KiB: 14.55 TiB."
    );
    assert!(
        s.statements[1].starts_with("raidz wastes space on small blocks: VM disks at 16 KiB hold")
    );
    assert!(s.statements[1].contains("; 32 KiB would give"));
    assert!(s.statements[2].starts_with("This pool can never be shrunk."));
    let removal = s
        .operations
        .iter()
        .find(|o| o.name == "Remove a data or special vdev")
        .unwrap();
    assert!(!removal.allowed);
    let expansion = s
        .operations
        .iter()
        .find(|o| o.name == "Add a disk to a raidz vdev")
        .unwrap();
    assert!(expansion.allowed);
}

#[test]
fn mirror_host_summary() {
    let boot = disks("a", 2, 2 * TB);
    let more = disks("b", 2, 2 * TB);
    let l = layout(
        Encryption::Luks2,
        vec![
            Vdev::new(VdevClass::Data, VdevType::Mirror, ids(&boot)).boot(),
            Vdev::new(VdevClass::Data, VdevType::Mirror, ids(&more)),
        ],
    );
    let inv: Vec<Disk> = boot.into_iter().chain(more).collect();
    let s = summarize(&l, &inv, Target::Host, &BlockSize::OFFERED);
    assert!(s.findings.is_allowed() && s.findings.warnings.is_empty());
    assert!(s.luks_possible);
    let gib = Bytes::GIB;
    let luks = 16 * Bytes::MIB;
    let boot_member = (2 * TB / gib - 4) * gib - gib - Bytes::MIB - luks;
    let other_member = (2 * TB / gib - 1) * gib - Bytes::MIB - luks;
    assert_eq!(s.vdevs[0].member_bytes, Some(boot_member));
    assert_eq!(s.vdevs[1].member_bytes, Some(other_member));
    assert_eq!(s.reported, boot_member + other_member);
    assert!(s.vm.iter().all(|f| f.bytes == s.reported));
    assert_eq!(s.suggested_block_size, Some(BlockSize::K16));
    assert_eq!(
        s.statements,
        [
            "Vdevs other than the boot vdev can be removed later, as long as the pool never gets a raidz vdev."
        ]
    );
}

#[test]
fn single_disk_boot_summary_rules_out_luks() {
    let inv = disks("a", 1, 2 * TB);
    let l = layout(
        Encryption::None,
        vec![Vdev::new(VdevClass::Data, VdevType::Single, ids(&inv)).boot()],
    );
    let s = summarize(&l, &inv, Target::Host, &BlockSize::OFFERED);
    assert!(!s.luks_possible);
    assert_eq!(
        s.vdevs[0].loss,
        "No redundancy: losing this disk loses the whole pool."
    );
    assert!(
        s.statements
            .iter()
            .any(|t| t.starts_with("Encryption is not possible with this layout"))
    );
}
