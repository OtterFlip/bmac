// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bmac_exec::{BoxFuture, ExecError, FakeRunner, Output, Reply};
use bmac_proto::inventory::{ClearReport, ClearRequest, DiskPartition, SystemInfo};
use bmac_storage::{Vdev, VdevClass, VdevType};

use super::*;

fn id(name: &str) -> ById {
    name.parse().unwrap()
}

fn raw(online: bool) -> Place {
    Place::Raw {
        leaf: "/dev/disk/by-id/nvme-B_S1-part3".into(),
        online,
    }
}

fn facts(place: Place) -> BootFacts {
    BootFacts {
        place,
        others_online: true,
        resilvering: false,
        luks: false,
        open: false,
        end: End::Short,
    }
}

#[test]
fn a_boot_member_goes_offline_grows_is_encrypted_and_replaced() {
    let leaf = "/dev/disk/by-id/nvme-B_S1-part3".to_string();
    assert_eq!(
        boot_step(&facts(raw(true))),
        BootStep::Offline(leaf.clone())
    );
    assert_eq!(boot_step(&facts(raw(false))), BootStep::Grow);
    let exact = BootFacts {
        end: End::Exact,
        ..facts(raw(false))
    };
    assert_eq!(boot_step(&exact), BootStep::Format);
    let formatted = BootFacts {
        luks: true,
        ..exact.clone()
    };
    assert_eq!(boot_step(&formatted), BootStep::Open);
    let open = BootFacts {
        open: true,
        ..formatted.clone()
    };
    assert_eq!(boot_step(&open), BootStep::Replace(leaf));
    assert_eq!(boot_step(&facts(Place::Replacing)), BootStep::Wait);
    let resilvering = BootFacts {
        resilvering: true,
        ..facts(Place::Mapper { online: true })
    };
    assert_eq!(boot_step(&resilvering), BootStep::Wait);
    assert_eq!(
        boot_step(&facts(Place::Mapper { online: true })),
        BootStep::Done
    );
}

#[test]
fn a_boot_member_is_never_taken_offline_without_a_copy_left() {
    let alone = BootFacts {
        others_online: false,
        ..facts(raw(true))
    };
    assert!(matches!(boot_step(&alone), BootStep::Refuse(_)));
    let resilvering = BootFacts {
        resilvering: true,
        ..facts(raw(true))
    };
    assert_eq!(boot_step(&resilvering), BootStep::Wait);
    let replace_alone = BootFacts {
        luks: true,
        open: true,
        others_online: false,
        end: End::Exact,
        ..facts(raw(false))
    };
    assert!(matches!(boot_step(&replace_alone), BootStep::Refuse(_)));
    let past = BootFacts {
        end: End::Past,
        ..facts(raw(false))
    };
    assert!(matches!(boot_step(&past), BootStep::Refuse(_)));
    assert!(matches!(
        boot_step(&facts(Place::Missing)),
        BootStep::Refuse(_)
    ));
    assert!(matches!(
        boot_step(&facts(Place::Mapper { online: false })),
        BootStep::Refuse(_)
    ));
}

fn member(in_pool: bool, partitioned: bool, luks: bool, open: bool) -> MemberFacts {
    MemberFacts {
        in_pool,
        partitioned,
        luks,
        open,
    }
}

#[test]
fn a_new_vdev_is_built_member_by_member_then_added_at_once() {
    let fresh = member(false, false, false, false);
    let parted = member(false, true, false, false);
    let formatted = member(false, true, true, false);
    let open = member(false, true, true, true);
    assert_eq!(
        vdev_step(true, &[fresh.clone(), fresh.clone()]),
        VdevStep::Partition(0)
    );
    assert_eq!(
        vdev_step(true, &[parted.clone(), fresh.clone()]),
        VdevStep::Format(0)
    );
    assert_eq!(
        vdev_step(true, &[open.clone(), formatted.clone()]),
        VdevStep::Open(1)
    );
    assert_eq!(
        vdev_step(true, &[open.clone(), open.clone()]),
        VdevStep::Add
    );
    assert_eq!(
        vdev_step(false, &[parted.clone(), parted.clone()]),
        VdevStep::Add
    );
    assert_eq!(
        vdev_step(false, &[formatted.clone(), parted.clone()]),
        VdevStep::Partition(0),
        "left-over LUKS on a disk of an unencrypted pool is erased"
    );
    let added = member(true, true, false, false);
    assert_eq!(
        vdev_step(false, &[added.clone(), added.clone()]),
        VdevStep::Done
    );
    assert!(matches!(
        vdev_step(false, &[added, parted]),
        VdevStep::Refuse(_)
    ));
}

fn layout_with(encryption: Encryption, extra: Vec<Vdev>) -> PoolLayout {
    let boot = Vdev::new(
        VdevClass::Data,
        VdevType::Mirror,
        vec![id("nvme-B_S1"), id("nvme-B_S2")],
    )
    .boot();
    let mut vdevs = vec![boot];
    vdevs.extend(extra);
    PoolLayout::new("rpool".parse().unwrap(), encryption, vdevs)
}

#[test]
fn zpool_add_is_forced_only_for_mismatched_redundancy() {
    let layout = layout_with(
        Encryption::None,
        vec![
            Vdev::new(
                VdevClass::Data,
                VdevType::Mirror,
                vec![id("ata-D_1"), id("ata-D_2")],
            ),
            Vdev::new(
                VdevClass::Data,
                VdevType::Raidz1,
                vec![id("ata-D_3"), id("ata-D_4"), id("ata-D_5")],
            ),
            Vdev::new(VdevClass::Log, VdevType::Single, vec![id("ata-D_6")]),
            Vdev::new(
                VdevClass::Special,
                VdevType::Mirror,
                vec![id("ata-D_7"), id("ata-D_8")],
            ),
        ],
    );
    let devices = |n: usize| (0..n).map(|i| format!("/d{i}")).collect::<Vec<_>>();
    assert_eq!(
        add_args(&layout, 1, &devices(2)),
        ["add", "-o", "ashift=12", "rpool", "mirror", "/d0", "/d1"]
    );
    assert_eq!(
        add_args(&layout, 2, &devices(3)),
        [
            "add",
            "-o",
            "ashift=12",
            "-f",
            "rpool",
            "raidz1",
            "/d0",
            "/d1",
            "/d2"
        ]
    );
    assert_eq!(
        add_args(&layout, 3, &devices(1)),
        ["add", "-o", "ashift=12", "rpool", "log", "/d0"]
    );
    assert!(
        needs_force(&layout, 4),
        "a special mirror after a raidz data vdev"
    );
}

#[test]
fn the_unpacked_initramfs_must_list_every_mapper_and_carry_decrypt_keyctl() {
    let dir = tempfile::tempdir().unwrap();
    let mappers = vec!["bmac-rpool-sS1".to_string(), "bmac-rpool-sS2".to_string()];
    assert!(
        check_unpacked(dir.path(), &mappers)
            .unwrap_err()
            .contains("cryptroot")
    );
    let main = dir.path().join("main");
    std::fs::create_dir_all(main.join("cryptroot")).unwrap();
    std::fs::write(
        main.join("cryptroot/crypttab"),
        "bmac-rpool-sS1 UUID=1 bmac-rpool luks\n",
    )
    .unwrap();
    let error = check_unpacked(dir.path(), &mappers).unwrap_err();
    assert!(error.contains("bmac-rpool-sS2"), "{error}");
    std::fs::write(
        main.join("cryptroot/crypttab"),
        "bmac-rpool-sS1 UUID=1 bmac-rpool luks\nbmac-rpool-sS2 UUID=2 bmac-rpool luks\n",
    )
    .unwrap();
    assert!(
        check_unpacked(dir.path(), &mappers)
            .unwrap_err()
            .contains("decrypt_keyctl")
    );
    std::fs::create_dir_all(main.join("lib/cryptsetup/scripts")).unwrap();
    std::fs::write(main.join("lib/cryptsetup/scripts/decrypt_keyctl"), "").unwrap();
    check_unpacked(dir.path(), &mappers).unwrap();
}

// ---- the executor, against a scripted host ----

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

type Effect = Box<dyn FnOnce() + Send>;

/// [`FakeRunner`] plus what some commands do to the host's files.
struct Effects {
    fake: FakeRunner,
    on: Mutex<Vec<(Vec<String>, Effect)>>,
}

impl Effects {
    fn new() -> Effects {
        Effects {
            fake: FakeRunner::new(),
            on: Mutex::new(Vec::new()),
        }
    }

    fn on(&self, line: &[&str], effect: impl FnOnce() + Send + 'static) {
        let line = line.iter().map(|s| s.to_string()).collect();
        self.on.lock().unwrap().push((line, Box::new(effect)));
    }
}

impl CommandRunner for Effects {
    fn run(&self, command: Command) -> BoxFuture<'_, Result<Output, ExecError>> {
        let line: Vec<String> = std::iter::once(command.program().display().to_string())
            .chain(command.arguments().iter().cloned())
            .collect();
        let effect = {
            let mut on = self.on.lock().unwrap();
            on.iter()
                .position(|(l, _)| *l == line)
                .map(|i| on.remove(i).1)
        };
        if let Some(effect) = effect {
            effect();
        }
        self.fake.run(command)
    }
}

/// Answers `disks()` from a script of the host's disks by call number.
struct Disks {
    calls: AtomicUsize,
    script: Box<dyn Fn(usize) -> Vec<InventoryDisk> + Send + Sync>,
}

impl Host for Disks {
    fn system(&self) -> SystemInfo {
        SystemInfo::default()
    }
    fn disks(&self) -> Vec<InventoryDisk> {
        (self.script)(self.calls.fetch_add(1, Ordering::SeqCst))
    }
    fn clear_labels(&self, _: &ClearRequest) -> ClearReport {
        ClearReport::default()
    }
    fn now_ms(&self) -> i64 {
        0
    }
}

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn disk(by_id: &str, serial: &str, size: u64, partitions: Vec<(u32, u64, u64)>) -> InventoryDisk {
    InventoryDisk {
        name: serial.to_lowercase(),
        path: format!("/dev/{}", serial.to_lowercase()),
        identity: Some(bmac_core::disk::DiskIdentity::Serial(serial.into())),
        model: None,
        serial: Some(serial.into()),
        wwn: None,
        size_bytes: size,
        logical_sector_bytes: 512,
        physical_sector_bytes: 512,
        rotational: false,
        removable: false,
        transport: None,
        by_id: vec![by_id.into()],
        udev_identity: BTreeMap::new(),
        partition_table: None,
        partitions: partitions
            .into_iter()
            .map(|(number, start_bytes, size_bytes)| DiskPartition {
                name: format!("{by_id}-part{number}"),
                path: format!("/dev/disk/by-id/{by_id}-part{number}"),
                number,
                start_bytes,
                size_bytes,
                luks: None,
            })
            .collect(),
        luks: None,
        zfs_labels: Vec::new(),
        problems: Vec::new(),
    }
}

/// The installer's layout: BIOS boot, ESP, and the pool partition ending
/// at `end`.
fn boot_disk(by_id: &str, serial: &str, end: u64) -> InventoryDisk {
    let start = GIB + 2 * MIB;
    disk(
        by_id,
        serial,
        64 * GIB,
        vec![
            (1, 32 * 1024, MIB),
            (2, 2 * MIB, GIB),
            (3, start, end - start),
        ],
    )
}

/// `zpool status -P -p rpool`, rows as (depth, name, state).
fn pool(state: &str, scan: &str, rows: &[(usize, &str, &str)]) -> Reply {
    let mut text = format!("  pool: rpool\n state: {state}\n");
    if !scan.is_empty() {
        text += &format!("  scan: {scan}\n");
    }
    text += &format!("config:\n\n\tNAME  STATE  READ WRITE CKSUM\n\trpool  {state}  0 0 0\n");
    for (depth, name, state) in rows {
        text += &format!("\t{}{name}  {state}  0 0 0\n", "  ".repeat(*depth));
    }
    text += "\nerrors: No known data errors\n";
    Reply::ok(text)
}

const STATUS: [&str; 4] = ["status", "-P", "-p", "rpool"];
const P1: &str = "/dev/disk/by-id/nvme-B_S1-part3";
const P2: &str = "/dev/disk/by-id/nvme-B_S2-part3";
const M1: &str = "/dev/mapper/bmac-rpool-sS1";

struct Rig {
    root: tempfile::TempDir,
    work: tempfile::TempDir,
    runner: Effects,
    _input: tokio::io::DuplexStream,
    flow: Workflow,
    out: Sink,
}

fn rig() -> Rig {
    let root = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("proc/sys/kernel")).unwrap();
    std::fs::write(
        root.path().join("proc/sys/kernel/osrelease"),
        "6.14.8-2-pve\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join("etc")).unwrap();
    let out = Sink::default();
    let (input, theirs) = tokio::io::duplex(4096);
    let flow = Workflow::new(
        Box::new(out.clone()),
        tokio::io::BufReader::new(theirs),
        work.path().to_path_buf(),
    );
    Rig {
        root,
        work,
        runner: Effects::new(),
        _input: input,
        flow,
        out,
    }
}

impl Rig {
    fn storage<'a>(&'a self, host: &'a dyn Host, layout: &'a PoolLayout) -> Storage<'a> {
        Storage {
            flow: &self.flow,
            host,
            runner: &self.runner,
            root: self.root.path().to_path_buf(),
            layout,
            define_storages: true,
            poll: Duration::ZERO,
        }
    }

    fn expect(&self, line: &[&str], reply: Reply) {
        self.runner
            .fake
            .expect(line[0], line[1..].iter().copied(), reply);
    }

    fn output(&self) -> String {
        String::from_utf8(self.out.0.lock().unwrap().clone()).unwrap()
    }
}

const ZPOOL: &str = programs::ZPOOL;

#[tokio::test]
async fn a_boot_member_is_encrypted_in_place_and_resilvered() {
    let rig = rig();
    let layout = layout_with(Encryption::Luks2, Vec::new());
    let host = Disks {
        calls: AtomicUsize::new(0),
        script: Box::new(|call| {
            let end = if call < 2 { 60 * GIB } else { 63 * GIB };
            vec![
                boot_disk("nvme-B_S1", "S1", end),
                boot_disk("nvme-B_S2", "S2", 60 * GIB),
            ]
        }),
    };
    let both = |s1: &'static str, s1_state: &'static str| {
        vec![
            (1, "mirror-0", "ONLINE"),
            (2, s1, s1_state),
            (2, P2, "ONLINE"),
        ]
    };
    let key = format!("--key-file={KEY_FILE}");
    let start = (GIB + 2 * MIB) / 512;
    let last = 63 * GIB / 512 - 1;
    let disk = "/dev/disk/by-id/nvme-B_S1";

    rig.expect(
        &[ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]],
        pool("ONLINE", "", &both(P1, "ONLINE")),
    );
    rig.expect(&[ZPOOL, "offline", "rpool", P1], Reply::ok(""));

    rig.expect(
        &[ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]],
        pool("DEGRADED", "", &both(P1, "OFFLINE")),
    );
    rig.expect(&[CRYPTSETUP, "isLuks", P1], Reply::fail(1, ""));
    rig.expect(
        &[
            SGDISK,
            "--backup=/var/lib/bmac/luks-headers/bmac-rpool-sS1.pre-resize.gpt",
            disk,
        ],
        Reply::ok(""),
    );
    let new = format!("--new=3:{start}:{last}");
    rig.expect(
        &[SGDISK, "--delete=3", &new, "--typecode=3:8309", disk],
        Reply::ok(""),
    );
    rig.expect(&[PARTX, "--update", "--nr", "3", disk], Reply::ok(""));
    rig.expect(&[UDEVADM, "settle"], Reply::ok(""));

    rig.expect(
        &[ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]],
        pool("DEGRADED", "", &both(P1, "OFFLINE")),
    );
    rig.expect(&[CRYPTSETUP, "isLuks", P1], Reply::fail(1, ""));
    rig.expect(&[WIPEFS, "--all", "--force", P1], Reply::ok(""));
    rig.expect(
        &[
            CRYPTSETUP,
            "luksFormat",
            "--batch-mode",
            "--type",
            "luks2",
            &key,
            P1,
        ],
        Reply::ok(""),
    );

    rig.expect(
        &[ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]],
        pool("DEGRADED", "", &both(P1, "OFFLINE")),
    );
    rig.expect(&[CRYPTSETUP, "isLuks", P1], Reply::ok(""));
    rig.expect(
        &[CRYPTSETUP, "open", "--test-passphrase", &key, P1],
        Reply::ok(""),
    );
    rig.expect(
        &[CRYPTSETUP, "open", &key, P1, "bmac-rpool-sS1"],
        Reply::ok(""),
    );
    let mapper_file = rig.root.path().join("dev/mapper/bmac-rpool-sS1");
    rig.runner.on(
        &[CRYPTSETUP, "open", &key, P1, "bmac-rpool-sS1"],
        move || {
            std::fs::create_dir_all(mapper_file.parent().unwrap()).unwrap();
            std::fs::write(&mapper_file, "").unwrap();
        },
    );
    let device = Reply::ok(format!(
        "/dev/mapper/bmac-rpool-sS1 is active.\n  type:    LUKS2\n  device:  {P1}\n"
    ));
    rig.expect(&[CRYPTSETUP, "status", "bmac-rpool-sS1"], device.clone());

    rig.expect(
        &[ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]],
        pool("DEGRADED", "", &both(P1, "OFFLINE")),
    );
    rig.expect(&[CRYPTSETUP, "isLuks", P1], Reply::ok(""));
    rig.expect(&[CRYPTSETUP, "status", "bmac-rpool-sS1"], device);
    rig.expect(
        &[CRYPTSETUP, "luksUUID", P1],
        Reply::ok("0b5e7a1c-1111-2222-3333-444455556666\n"),
    );
    rig.expect(&[UPDATE_INITRAMFS, "-u", "-k", "all"], Reply::ok(""));
    rig.expect(&[PROXMOX_BOOT_TOOL, "refresh"], Reply::ok(""));
    let unpacked = rig.work.path().join("initramfs");
    let into = unpacked.to_string_lossy().to_string();
    rig.expect(
        &[UNMKINITRAMFS, "/boot/initrd.img-6.14.8-2-pve", &into],
        Reply::ok(""),
    );
    let crypttab = rig.root.path().join("etc/crypttab");
    rig.runner.on(
        &[UNMKINITRAMFS, "/boot/initrd.img-6.14.8-2-pve", &into],
        move || {
            let main = unpacked.join("main");
            std::fs::create_dir_all(main.join("cryptroot")).unwrap();
            std::fs::copy(&crypttab, main.join("cryptroot/crypttab")).unwrap();
            std::fs::write(main.join("decrypt_keyctl"), "").unwrap();
        },
    );
    rig.expect(&[ZPOOL, "replace", "rpool", P1, M1], Reply::ok(""));

    let replacing = vec![
        (1, "mirror-0", "DEGRADED"),
        (2, "replacing-0", "DEGRADED"),
        (3, P1, "OFFLINE"),
        (3, M1, "ONLINE"),
        (2, P2, "ONLINE"),
    ];
    for _ in 0..2 {
        rig.expect(
            &[ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]],
            pool(
                "DEGRADED",
                "resilver in progress since Mon Oct 12",
                &replacing,
            ),
        );
    }
    rig.expect(
        &[ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]],
        pool(
            "ONLINE",
            "resilvered 1.2G in 00:00:31 with 0 errors",
            &both(M1, "ONLINE"),
        ),
    );

    let partition = rig.root.path().join(P1.trim_start_matches('/'));
    std::fs::create_dir_all(partition.parent().unwrap()).unwrap();
    std::fs::File::create(&partition)
        .unwrap()
        .set_len(4 * MIB)
        .unwrap();
    bmac_inventory::zfs::write_label(
        &partition,
        &bmac_inventory::zfs::member_label("rpool", 7, 8, 0, 100),
    )
    .unwrap();

    let storage = rig.storage(&host, &layout);
    let mapper = storage.encrypt_boot_member(&id("nvme-B_S1")).await.unwrap();
    assert_eq!(mapper, "bmac-rpool-sS1");
    rig.runner.fake.assert_done();
    assert_eq!(
        bmac_inventory::zfs::read_label(&partition, 0).unwrap(),
        None,
        "the old pool's labels are cleared before the partition grows"
    );
    let crypttab = std::fs::read_to_string(rig.root.path().join("etc/crypttab")).unwrap();
    assert_eq!(
        crypttab,
        "bmac-rpool-sS1\tUUID=0b5e7a1c-1111-2222-3333-444455556666\tbmac-rpool\t\
         luks,initramfs,nofail,keyscript=decrypt_keyctl\n"
    );
    assert!(
        !rig.work.path().join("initramfs").exists(),
        "the unpacked initramfs is removed"
    );
    let output = rig.output();
    assert!(
        output.contains("Replacing /dev/disk/by-id/nvme-B_S1-part3 with bmac-rpool-sS1."),
        "{output}"
    );
}

#[tokio::test]
async fn a_new_mirror_is_partitioned_and_added_and_the_block_size_storages_made() {
    let rig = rig();
    let layout = layout_with(
        Encryption::None,
        vec![Vdev::new(
            VdevClass::Data,
            VdevType::Mirror,
            vec![id("ata-D_1"), id("ata-D_2")],
        )],
    );
    let end = 99 * GIB;
    let host = Disks {
        calls: AtomicUsize::new(0),
        script: Box::new(move |call| {
            let parted = |done: bool| {
                if done {
                    vec![(1, MIB, end - MIB)]
                } else {
                    vec![(1, MIB, 10 * GIB)]
                }
            };
            vec![
                boot_disk("nvme-B_S1", "S1", 60 * GIB),
                boot_disk("nvme-B_S2", "S2", 60 * GIB),
                disk("ata-D_1", "D1", 100 * GIB, parted(call >= 1)),
                disk(
                    "ata-D_2",
                    "D2",
                    100 * GIB,
                    if call >= 2 { parted(true) } else { Vec::new() },
                ),
            ]
        }),
    };
    let boot = vec![
        (1, "mirror-0", "ONLINE"),
        (2, P1, "ONLINE"),
        (2, P2, "ONLINE"),
    ];
    let d = |n: u32| format!("/dev/disk/by-id/ata-D_{n}");
    let p = |n: u32| format!("/dev/disk/by-id/ata-D_{n}-part1");
    let new = format!("--new=1:2048:{}", end / 512 - 1);

    rig.expect(
        &[ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]],
        pool("ONLINE", "", &boot),
    );
    rig.expect(&[WIPEFS, "--all", "--force", &p(1)], Reply::ok(""));
    for n in [1, 2] {
        rig.expect(&[WIPEFS, "--all", "--force", &d(n)], Reply::ok(""));
        rig.expect(&[SGDISK, "--zap-all", &d(n)], Reply::ok(""));
        rig.expect(&[SGDISK, &new, "--typecode=1:BF01", &d(n)], Reply::ok(""));
        rig.expect(&[PARTX, "--update", &d(n)], Reply::ok(""));
        rig.expect(&[UDEVADM, "settle"], Reply::ok(""));
        rig.expect(&[WIPEFS, "--all", "--force", &p(n)], Reply::ok(""));
    }
    rig.expect(
        &[ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]],
        pool("ONLINE", "", &boot),
    );
    rig.expect(
        &[ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]],
        pool("ONLINE", "", &boot),
    );
    rig.expect(
        &[
            ZPOOL,
            "add",
            "-o",
            "ashift=12",
            "rpool",
            "mirror",
            &p(1),
            &p(2),
        ],
        Reply::ok(""),
    );
    let mut added = boot.clone();
    let (p1, p2) = (p(1), p(2));
    let (p1, p2): (&'static str, &'static str) = (p1.leak(), p2.leak());
    added.extend([
        (1, "mirror-1", "ONLINE"),
        (2, p1, "ONLINE"),
        (2, p2, "ONLINE"),
    ]);
    rig.expect(
        &[ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]],
        pool("ONLINE", "", &added),
    );

    std::fs::create_dir_all(rig.root.path().join("etc/pve")).unwrap();
    std::fs::write(
        rig.root.path().join("etc/pve/storage.cfg"),
        "zfspool: local-zfs\n\tpool rpool/data\n\nzfspool: bmac-zfs-64k\n\tpool rpool/data-64k\n",
    )
    .unwrap();
    for (size, exists) in [("32k", false), ("64k", true), ("128k", false)] {
        let dataset = format!("rpool/data-{size}");
        let listed = if exists {
            Reply::ok(dataset.clone())
        } else {
            Reply::fail(1, "")
        };
        rig.expect(
            &[programs::ZFS, "list", "-H", "-o", "name", &dataset],
            listed,
        );
        if !exists {
            rig.expect(&[programs::ZFS, "create", &dataset], Reply::ok(""));
            let storage = format!("bmac-zfs-{size}");
            rig.expect(
                &[
                    PVESM,
                    "add",
                    "zfspool",
                    &storage,
                    "--pool",
                    &dataset,
                    "--blocksize",
                    size,
                    "--content",
                    "images,rootdir",
                    "--sparse",
                    "1",
                ],
                Reply::ok(""),
            );
        }
    }

    let storage = rig.storage(&host, &layout);
    storage.add_vdev(1).await.unwrap();
    let made = storage.block_size_storages().await.unwrap();
    assert_eq!(
        made,
        [
            "rpool/data-32k",
            "bmac-zfs-32k",
            "rpool/data-128k",
            "bmac-zfs-128k"
        ]
    );
    rig.runner.fake.assert_done();
}

#[tokio::test]
async fn the_passphrase_file_is_waited_for_and_removed() {
    use std::os::unix::fs::PermissionsExt;
    let rig = rig();
    let layout = layout_with(Encryption::Luks2, Vec::new());
    let host = Disks {
        calls: AtomicUsize::new(0),
        script: Box::new(|_| Vec::new()),
    };
    let storage = rig.storage(&host, &layout);
    assert!(!storage.key_ready().unwrap());
    let key = rig.root.path().join(KEY_FILE.trim_start_matches('/'));
    std::fs::create_dir_all(key.parent().unwrap()).unwrap();
    std::fs::write(&key, "secret").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(storage.key_ready().is_err(), "a readable file is refused");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(storage.key_ready().unwrap());
    storage.remove_key().unwrap();
    assert!(!key.exists());
    storage.remove_key().unwrap();
}

fn flow_in(work: &Path) -> (Workflow, Sink, tokio::io::DuplexStream) {
    let out = Sink::default();
    let (input, theirs) = tokio::io::duplex(4096);
    let flow = Workflow::new(
        Box::new(out.clone()),
        tokio::io::BufReader::new(theirs),
        work.to_path_buf(),
    );
    (flow, out, input)
}

#[tokio::test(start_paused = true)]
async fn each_drill_restarts_the_host_and_restores_the_member_after_the_boot() {
    use tokio::io::AsyncWriteExt;
    let rig = rig();
    let layout = layout_with(Encryption::None, Vec::new());
    let host = Disks {
        calls: AtomicUsize::new(0),
        script: Box::new(|_| {
            vec![
                boot_disk("nvme-B_S1", "S1", 60 * GIB),
                boot_disk("nvme-B_S2", "S2", 60 * GIB),
            ]
        }),
    };
    std::fs::create_dir_all(rig.root.path().join("sys/firmware/efi")).unwrap();
    let esp = rig.root.path().join("run/bmac/esp-drill");
    std::fs::create_dir_all(esp.join("EFI")).unwrap();
    let rows = |s1: &'static str, s2: &'static str| {
        vec![(1, "mirror-0", "ONLINE"), (2, P1, s1), (2, P2, s2)]
    };
    let status = [ZPOOL, STATUS[0], STATUS[1], STATUS[2], STATUS[3]];
    let unit = format!("--unit=bmac-drill-restart-{}", std::process::id());
    let restart = [
        "/usr/bin/systemd-run",
        &unit,
        "--on-active=1s",
        "--timer-property=AccuracySec=100ms",
        "--collect",
        programs::SYSTEMCTL,
        "reboot",
    ];
    let mount = |n: u32| {
        rig.expect(
            &[
                "/usr/bin/mount",
                &format!("/dev/disk/by-id/nvme-B_S{n}-part2"),
                "/run/bmac/esp-drill",
            ],
            Reply::ok(""),
        );
        rig.expect(&["/usr/bin/umount", "/run/bmac/esp-drill"], Reply::ok(""));
    };

    rig.expect(&status, pool("ONLINE", "", &rows("ONLINE", "ONLINE")));
    mount(1);
    rig.expect(&status, pool("ONLINE", "", &rows("ONLINE", "ONLINE")));
    rig.expect(&[ZPOOL, "offline", "rpool", P1], Reply::ok(""));
    rig.expect(&restart, Reply::ok(""));

    let (flow, _, mut input) = flow_in(rig.work.path());
    input
        .write_all(b"{\"type\":\"response\",\"request_id\":\"r1\",\"confirmed\":true}\n")
        .await
        .unwrap();
    let storage = Storage {
        flow: &flow,
        ..rig.storage(&host, &layout)
    };
    let error = storage.drills("boot-a").await.unwrap_err();
    assert!(format!("{error:?}").contains("didn't restart"), "{error:?}");
    assert!(esp.join("EFI.BMAC-DRILL").is_dir() && !esp.join("EFI").exists());
    assert!(crate::native::restarted_since_asked(
        rig.work.path(),
        "boot-b"
    ));
    rig.runner.fake.assert_done();

    rig.expect(&status, pool("DEGRADED", "", &rows("OFFLINE", "ONLINE")));
    mount(1);
    rig.expect(&[ZPOOL, "online", "rpool", P1], Reply::ok(""));
    rig.expect(
        &status,
        pool(
            "ONLINE",
            "resilvered 4K with 0 errors",
            &rows("ONLINE", "ONLINE"),
        ),
    );
    rig.expect(&[PROXMOX_BOOT_TOOL, "refresh"], Reply::ok(""));
    rig.expect(&status, pool("ONLINE", "", &rows("ONLINE", "ONLINE")));
    mount(2);
    rig.expect(&status, pool("ONLINE", "", &rows("ONLINE", "ONLINE")));
    rig.expect(&[ZPOOL, "offline", "rpool", P2], Reply::ok(""));
    rig.expect(&restart, Reply::ok(""));

    let (flow, out, _input) = flow_in(rig.work.path());
    let storage = Storage {
        flow: &flow,
        ..rig.storage(&host, &layout)
    };
    let error = storage.drills("boot-b").await.unwrap_err();
    assert!(format!("{error:?}").contains("didn't restart"), "{error:?}");
    rig.runner.fake.assert_done();
    let done = flow.checkpoints();
    assert!(done.is_done("drills/member-1/passed"));
    assert!(!done.is_done("drills/member-2/booted"));
    let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("booted without nvme-B_S1"), "{text}");
    assert!(!text.contains("Run the boot drills?"), "asked only once");
}

#[tokio::test]
async fn the_header_phase_backs_up_every_member_again() {
    let rig = rig();
    let layout = layout_with(Encryption::Luks2, Vec::new());
    let host = Disks {
        calls: AtomicUsize::new(0),
        script: Box::new(|_| {
            vec![
                boot_disk("nvme-B_S1", "S1", 60 * GIB),
                boot_disk("nvme-B_S2", "S2", 60 * GIB),
            ]
        }),
    };
    for (n, part) in [(1, P1), (2, P2)] {
        let file = format!("{HEADER_DIR}/bmac-rpool-sS{n}.luks-header");
        let line = [
            CRYPTSETUP,
            "luksHeaderBackup",
            part,
            "--header-backup-file",
            &file,
        ];
        let at = rig.root.path().join(file.trim_start_matches('/'));
        rig.runner
            .on(&line, move || std::fs::write(at, b"LUKS\xba\xbe").unwrap());
        rig.expect(&line, Reply::ok(""));
    }
    let data = rig.storage(&host, &layout).headers().await.unwrap();
    rig.runner.fake.assert_done();
    assert_eq!(data["headers"].as_array().unwrap().len(), 2);
    assert!(rig.output().contains("Backed up 2 LUKS headers"));

    let plain = layout_with(Encryption::None, Vec::new());
    let data = rig.storage(&host, &plain).headers().await.unwrap();
    assert_eq!(data["headers"], json!([]));
}
