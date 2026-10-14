// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_adapters::layout::{
    AuxiliaryVdev, CrypttabEntry, DiskRow, Layout, LayoutVdev, PoolSummary,
};
use bmac_exec::{ExecError, FakeRunner, Output, Reply};
use bmac_proto::bmac_ui::WorkflowEvent;
use bmac_proto::inventory::{ClearReport, ClearRequest, DiskPartition, LuksHeader, SystemInfo};
use bmac_storage::journal::{RemovalState, RemovedMember, SimulationStage};
use tokio::io::DuplexStream;

use super::*;
use crate::native::tests::{Shared, send, workflow};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
const DISK: u64 = 16 * GIB;
const NOW_MS: i64 = 1_800_000_000_000;
const SECTOR: u64 = 512;
const CLEAN_SCRUB: &str = "scrub repaired 0B in 00:00:01 with 0 errors on Tue Oct 13 12:00:00 2026";

// ---- a host in memory ----

#[derive(Debug, Clone)]
struct Part {
    number: u32,
    start: u64,
    size: u64,
    luks: bool,
}

#[derive(Debug, Clone)]
struct Drive {
    name: String,
    serial: String,
    size: u64,
    parts: Vec<Part>,
    esp: bool,
}

impl Drive {
    fn by_id(&self) -> String {
        by_id_of(&self.serial)
    }

    fn path(&self) -> String {
        format!("/dev/disk/by-id/{}", self.by_id())
    }

    fn part_path(&self, number: u32) -> String {
        format!("{}-part{number}", self.path())
    }
}

fn by_id_of(serial: &str) -> String {
    format!("ata-QEMU_HARDDISK_{serial}")
}

fn path_of(serial: &str) -> String {
    format!("/dev/disk/by-id/{}", by_id_of(serial))
}

fn mapper_of(serial: &str) -> String {
    bmac_storage::luks::mapper_name(&pool_name(), &DiskIdentity::Serial(serial.into()))
}

#[derive(Debug, Clone)]
struct Leaf {
    path: String,
    state: String,
    group: Option<String>,
}

#[derive(Debug, Clone)]
struct WTop {
    name: String,
    /// `disk`, `mirror`, `raidz1`, ...
    kind: String,
    /// `None` for data; `logs`, `cache`, `spares`, `special`.
    section: Option<String>,
    leaves: Vec<Leaf>,
    allocated: u64,
}

type Extra = Box<dyn FnMut(&[String]) -> Option<Reply> + Send>;

struct World {
    root: PathBuf,
    drives: Vec<Drive>,
    tops: Vec<WTop>,
    /// Open mappers and the partition each is open on.
    mappers: BTreeMap<String, String>,
    scan: Option<String>,
    available: u64,
    raidz_expansion: bool,
    extra: Option<Extra>,
}

fn leaf(path: &str) -> Leaf {
    Leaf {
        path: path.into(),
        state: "ONLINE".into(),
        group: None,
    }
}

impl World {
    fn new(root: &Path) -> World {
        World {
            root: root.to_path_buf(),
            drives: Vec::new(),
            tops: Vec::new(),
            mappers: BTreeMap::new(),
            scan: None,
            available: 200 * GIB,
            raidz_expansion: false,
            extra: None,
        }
    }

    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    fn touch(&self, path: &str, size: u64) {
        let file = self.at(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&file)
            .unwrap()
            .set_len(size)
            .unwrap();
    }

    fn drive(&mut self, name: &str, serial: &str, size: u64) -> &mut World {
        let drive = Drive {
            name: name.into(),
            serial: serial.into(),
            size,
            parts: Vec::new(),
            esp: false,
        };
        self.touch(&drive.path(), size);
        self.drives.push(drive);
        self
    }

    fn set_parts(&mut self, serial: &str, parts: Vec<Part>) {
        let drive = self.drives.iter_mut().find(|d| d.serial == serial).unwrap();
        drive.parts = parts;
        let drive = drive.clone();
        for p in &drive.parts {
            self.touch(&drive.part_path(p.number), p.size);
        }
    }

    /// The installer's boot disk: BIOS boot, ESP, and the pool partition.
    fn boot_drive(&mut self, name: &str, serial: &str, luks: bool) -> String {
        self.drive(name, serial, DISK);
        let start = GIB + 2 * MIB;
        self.set_parts(
            serial,
            vec![
                Part {
                    number: 1,
                    start: 32 * 1024,
                    size: MIB,
                    luks: false,
                },
                Part {
                    number: 2,
                    start: 2 * MIB,
                    size: GIB,
                    luks: false,
                },
                Part {
                    number: 3,
                    start,
                    size: 12 * GIB,
                    luks,
                },
            ],
        );
        self.drives.last_mut().unwrap().esp = true;
        self.member(serial, 3, luks)
    }

    /// A non-boot member's one partition, as BMAC lays it out.
    fn member_drive(&mut self, name: &str, serial: &str, luks: bool) -> String {
        self.drive(name, serial, DISK);
        let (first, last) =
            bmac_storage::partition::member_partition(&[DISK, DISK], SECTOR).unwrap();
        self.set_parts(
            serial,
            vec![Part {
                number: 1,
                start: first * SECTOR,
                size: (last - first + 1) * SECTOR,
                luks,
            }],
        );
        self.member(serial, 1, luks)
    }

    /// The leaf path of the member on `serial`'s partition, opening its
    /// mapper when it is encrypted.
    fn member(&mut self, serial: &str, number: u32, luks: bool) -> String {
        let partition = format!("{}-part{number}", path_of(serial));
        if !luks {
            return partition;
        }
        let mapper = mapper_of(serial);
        self.touch(&format!("/dev/mapper/{mapper}"), 0);
        self.mappers.insert(mapper.clone(), partition);
        format!("/dev/mapper/{mapper}")
    }

    fn top(&mut self, name: &str, kind: &str, section: Option<&str>, leaves: Vec<String>) {
        self.tops.push(WTop {
            name: name.into(),
            kind: kind.into(),
            section: section.map(str::to_string),
            leaves: leaves.iter().map(|p| leaf(p)).collect(),
            allocated: GIB,
        });
    }

    /// The drive and partition a leaf path is on now.
    fn trace(&self, path: &str) -> Option<(&Drive, &Part)> {
        let partition = match path.strip_prefix("/dev/mapper/") {
            Some(mapper) => self.mappers.get(mapper)?.clone(),
            None => path.to_string(),
        };
        self.drives.iter().find_map(|d| {
            d.parts
                .iter()
                .find(|p| d.part_path(p.number) == partition)
                .map(|p| (d, p))
        })
    }

    fn in_pool(&self, drive: &Drive) -> bool {
        self.tops.iter().flat_map(|t| &t.leaves).any(|l| {
            self.trace(&l.path)
                .is_some_and(|(d, _)| d.name == drive.name)
        })
    }

    fn crypttab(&self) -> String {
        std::fs::read_to_string(self.at(CRYPTTAB_PATH)).unwrap_or_default()
    }

    fn member_of(&self, l: &Leaf) -> Member {
        let traced = self.trace(&l.path);
        let mapper = l.path.strip_prefix("/dev/mapper/").map(str::to_string);
        Member {
            path: l.path.clone(),
            state: Some(l.state.clone()),
            device: l.path.clone(),
            luks: mapper.is_some(),
            mapper: mapper
                .filter(|m| self.mappers.contains_key(m))
                .map(|m| format!("/dev/mapper/{m}")),
            backing: traced.map(|(d, p)| format!("/dev/{}{}", d.name, p.number)),
            // `lsblk -p` names, as on a real host.
            disk: traced.map(|(d, _)| format!("/dev/{}", d.name)),
            serial: traced.map(|(d, _)| d.serial.clone()),
            identity: traced.map(|(d, _)| DiskIdentity::Serial(d.serial.clone())),
            model: Some("QEMU HARDDISK".into()),
            disk_size: traced.map(|(d, _)| d.size),
            member_size: traced.map(|(_, p)| {
                if p.luks {
                    p.size - LUKS2_HEADER_BYTES
                } else {
                    p.size
                }
            }),
            group: l.group.clone(),
        }
    }

    fn row(&self, d: &Drive) -> DiskRow {
        let mut in_use = Vec::new();
        if self.in_pool(d) {
            in_use.push("member of rpool".to_string());
        }
        if self
            .mappers
            .values()
            .any(|p| d.parts.iter().any(|q| d.part_path(q.number) == *p))
            && !self.in_pool(d)
        {
            in_use.push("LUKS mapping open".to_string());
        }
        DiskRow {
            disk: format!("/dev/{}", d.name),
            serial: Some(d.serial.clone()),
            identity: Some(DiskIdentity::Serial(d.serial.clone())),
            model: Some("QEMU HARDDISK".into()),
            size: Some(d.size),
            tran: Some("sata".into()),
            fstype: None,
            partitions_or_holders: d.parts.len(),
            mounted: false,
            in_use_reasons: in_use,
        }
    }

    fn layout(&self) -> Layout {
        let degraded = self
            .tops
            .iter()
            .flat_map(|t| &t.leaves)
            .any(|l| l.state != "ONLINE");
        let health = if degraded { "DEGRADED" } else { "ONLINE" };
        let mut vdevs = Vec::new();
        let mut auxiliary: BTreeMap<String, Vec<AuxiliaryVdev>> = BTreeMap::new();
        for top in &self.tops {
            let members: Vec<Member> = top.leaves.iter().map(|l| self.member_of(l)).collect();
            let state = if top.leaves.iter().all(|l| l.state == "ONLINE") {
                "ONLINE"
            } else {
                "DEGRADED"
            };
            let replacing = top.leaves.iter().any(|l| {
                l.group
                    .as_deref()
                    .is_some_and(|g| g.starts_with("replacing-"))
            });
            match &top.section {
                None => vdevs.push(LayoutVdev {
                    name: top.name.clone(),
                    kind: top.kind.clone(),
                    state: Some(state.into()),
                    removing: false,
                    replacing,
                    size: None,
                    allocated: Some(top.allocated),
                    free: None,
                    holds_esp: top
                        .leaves
                        .iter()
                        .any(|l| self.trace(&l.path).is_some_and(|(d, _)| d.esp)),
                    luks: members.iter().any(|m| m.luks),
                    members,
                }),
                Some(section) => {
                    auxiliary
                        .entry(section.clone())
                        .or_default()
                        .push(AuxiliaryVdev {
                            name: top.name.clone(),
                            kind: top.kind.clone(),
                            state: Some(state.into()),
                            removing: false,
                            replacing,
                            members,
                        })
                }
            }
        }
        let rows: Vec<DiskRow> = self.drives.iter().map(|d| self.row(d)).collect();
        let unassigned: Vec<DiskRow> = self
            .drives
            .iter()
            .filter(|d| !self.in_pool(d))
            .map(|d| self.row(d))
            .collect();
        Layout {
            schema_version: 1,
            host: "mox1".into(),
            collected_at: NOW_MS / 1000,
            pool: PoolSummary {
                name: "rpool".into(),
                state: Some(health.into()),
                health: Some(health.into()),
                size: None,
                allocated: None,
                free: None,
                capacity_percent: None,
                fragmentation_percent: None,
                dataset_available: Some(self.available),
                dataset_used: None,
                status: None,
                scan: self.scan.clone(),
                remove: None,
                removal_in_progress: false,
                errors: None,
            },
            vdevs,
            auxiliary_vdevs: auxiliary,
            imported_pool_vdevs: Vec::new(),
            esp_partitions: Vec::new(),
            physically_removable_disks: unassigned
                .iter()
                .filter(|r| r.in_use_reasons.is_empty())
                .cloned()
                .collect(),
            unassigned_disks: unassigned,
            all_disks: rows,
            esp_only_disks: Vec::new(),
            luks_mappings: Vec::new(),
            crypttab: bmac_storage::luks::initramfs_names(&self.crypttab())
                .into_iter()
                .map(|mapper| CrypttabEntry {
                    mapper,
                    shared_unlock: true,
                })
                .collect(),
            volumes: Vec::new(),
        }
    }

    fn disks(&self) -> Vec<InventoryDisk> {
        self.drives
            .iter()
            .map(|d| InventoryDisk {
                name: d.name.clone(),
                path: format!("/dev/{}", d.name),
                identity: Some(DiskIdentity::Serial(d.serial.clone())),
                model: Some("QEMU HARDDISK".into()),
                serial: Some(d.serial.clone()),
                wwn: None,
                size_bytes: d.size,
                logical_sector_bytes: SECTOR as u32,
                physical_sector_bytes: SECTOR as u32,
                rotational: false,
                removable: false,
                transport: Some("sata".into()),
                by_id: vec![d.by_id()],
                udev_identity: BTreeMap::new(),
                partition_table: None,
                partitions: d
                    .parts
                    .iter()
                    .map(|p| DiskPartition {
                        name: format!("{}-part{}", d.by_id(), p.number),
                        path: d.part_path(p.number),
                        number: p.number,
                        start_bytes: p.start,
                        size_bytes: p.size,
                        luks: p.luks.then_some(LuksHeader {
                            version: 2,
                            uuid: None,
                        }),
                    })
                    .collect(),
                luks: None,
                zfs_labels: Vec::new(),
                problems: Vec::new(),
            })
            .collect()
    }

    /// `zpool status -P -p rpool` for the data vdevs.
    fn status(&self) -> String {
        let health = self.layout().pool.health.unwrap_or_default();
        let mut text = format!("  pool: rpool\n state: {health}\n");
        if let Some(scan) = &self.scan {
            text += &format!("  scan: {scan}\n");
        }
        text += &format!("config:\n\n\tNAME  STATE  READ WRITE CKSUM\n\trpool  {health}  0 0 0\n");
        for top in self.tops.iter().filter(|t| t.section.is_none()) {
            if top.kind == "disk" {
                for l in &top.leaves {
                    text += &format!("\t  {}  {}  0 0 0\n", l.path, l.state);
                }
                continue;
            }
            text += &format!("\t  {}  ONLINE  0 0 0\n", top.name);
            for l in &top.leaves {
                text += &format!("\t    {}  {}  0 0 0\n", l.path, l.state);
            }
        }
        text += "\nerrors: No known data errors\n";
        text
    }

    fn part_mut(&mut self, path: &str) -> Option<&mut Part> {
        self.drives.iter_mut().find_map(|d| {
            let base = d.path();
            d.parts
                .iter_mut()
                .find(|p| format!("{base}-part{}", p.number) == path)
        })
    }

    fn drive_at(&mut self, path: &str) -> Option<&mut Drive> {
        self.drives.iter_mut().find(|d| d.path() == path)
    }

    fn leaf_mut(&mut self, path: &str) -> Option<&mut Leaf> {
        self.tops
            .iter_mut()
            .flat_map(|t| t.leaves.iter_mut())
            .find(|l| l.path == path)
    }

    /// What the host answers to one command, and what it does.
    fn respond(&mut self, line: &[String]) -> Reply {
        if let Some(extra) = self.extra.as_mut()
            && let Some(reply) = extra(line)
        {
            return reply;
        }
        let program = Path::new(&line[0])
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let args: Vec<&str> = line[1..].iter().map(String::as_str).collect();
        match (program.as_str(), args.as_slice()) {
            ("zpool", ["get", "-H", "-p", "-o", "property,value", "ashift", "rpool"]) => {
                Reply::ok("ashift\t12\n")
            }
            (
                "zpool",
                [
                    "get",
                    "-H",
                    "-o",
                    "value",
                    "feature@raidz_expansion",
                    "rpool",
                ],
            ) => Reply::ok(if self.raidz_expansion {
                "enabled\n"
            } else {
                "disabled\n"
            }),
            ("zpool", ["status", "-P", "-p", "rpool"]) => Reply::ok(self.status()),
            ("zpool", ["add", "-o", "ashift=12", rest @ ..]) => {
                let words: Vec<&str> = rest.iter().copied().filter(|w| *w != "-f").collect();
                let mut section = None;
                let mut kind = "disk";
                let mut devices = Vec::new();
                for w in &words[1..] {
                    match *w {
                        "log" => section = Some("logs"),
                        "cache" => section = Some("cache"),
                        "spare" => section = Some("spares"),
                        "special" => section = Some("special"),
                        "mirror" | "raidz1" | "raidz2" | "raidz3" => kind = w,
                        device => devices.push(device.to_string()),
                    }
                }
                let name = if kind == "disk" {
                    devices[0].clone()
                } else {
                    format!("{kind}-{}", self.tops.len())
                };
                self.top(&name, kind, section, devices);
                Reply::ok("")
            }
            ("zpool", ["offline", "rpool", path]) => {
                self.leaf_mut(path).unwrap().state = "OFFLINE".into();
                Reply::ok("")
            }
            ("zpool", ["online", "rpool", path]) => {
                self.leaf_mut(path).unwrap().state = "ONLINE".into();
                Reply::ok("")
            }
            ("zpool", ["replace", "rpool", path]) => {
                self.leaf_mut(path).unwrap().state = "ONLINE".into();
                Reply::ok("")
            }
            ("zpool", ["replace", "rpool", old, new]) => {
                let l = self.leaf_mut(old).unwrap();
                l.path = new.to_string();
                l.state = "ONLINE".into();
                Reply::ok("")
            }
            ("zpool", ["attach", "rpool", target, device]) => {
                let top = self
                    .tops
                    .iter_mut()
                    .find(|t| t.name == *target || t.leaves.iter().any(|l| l.path == *target))
                    .unwrap();
                if top.kind == "disk" {
                    top.kind = "mirror".into();
                }
                top.leaves.push(leaf(device));
                Reply::ok("")
            }
            ("zpool", ["remove", "rpool", name]) => {
                self.tops.retain(|t| t.name != *name);
                Reply::ok("")
            }
            ("zpool", ["scrub", "-w", "rpool"]) => {
                self.scan = Some(CLEAN_SCRUB.into());
                Reply::ok("")
            }
            ("sgdisk", ["--zap-all", path]) => {
                if let Some(d) = self.drive_at(path) {
                    d.parts.clear();
                }
                Reply::ok("")
            }
            ("sgdisk", [new, _typecode, path]) if new.starts_with("--new=1:") => {
                let span = new.trim_start_matches("--new=1:");
                let (first, last) = span.split_once(':').unwrap();
                let (first, last): (u64, u64) = (first.parse().unwrap(), last.parse().unwrap());
                let serial = self.drive_at(path).unwrap().serial.clone();
                self.set_parts(
                    &serial,
                    vec![Part {
                        number: 1,
                        start: first * SECTOR,
                        size: (last - first + 1) * SECTOR,
                        luks: false,
                    }],
                );
                Reply::ok("")
            }
            ("sgdisk", [replicate, survivor]) if replicate.starts_with("--replicate=") => {
                let new = replicate.trim_start_matches("--replicate=").to_string();
                let parts: Vec<Part> = self
                    .drives
                    .iter()
                    .find(|d| d.path() == *survivor)
                    .unwrap()
                    .parts
                    .iter()
                    .map(|p| Part {
                        luks: false,
                        ..p.clone()
                    })
                    .collect();
                let serial = self.drive_at(&new).unwrap().serial.clone();
                self.set_parts(&serial, parts);
                Reply::ok("")
            }
            ("cryptsetup", ["isLuks", part]) => {
                if self.part_mut(part).is_some_and(|p| p.luks) {
                    Reply::ok("")
                } else {
                    Reply::fail(1, "")
                }
            }
            ("cryptsetup", ["luksFormat", .., part]) => {
                self.part_mut(part).unwrap().luks = true;
                Reply::ok("")
            }
            ("cryptsetup", ["open", "--test-passphrase", ..]) => Reply::ok(""),
            ("cryptsetup", ["open", _key, part, mapper]) => {
                self.touch(&format!("/dev/mapper/{mapper}"), 0);
                self.mappers.insert(mapper.to_string(), part.to_string());
                Reply::ok("")
            }
            ("cryptsetup", ["status", mapper]) => match self.mappers.get(*mapper) {
                Some(part) => Reply::ok(format!("  type:    LUKS2\n  device:  {part}\n")),
                None => Reply::fail(4, "inactive"),
            },
            ("cryptsetup", ["close", mapper]) => {
                self.mappers.remove(*mapper);
                let _ = std::fs::remove_file(self.at(&format!("/dev/mapper/{mapper}")));
                Reply::ok("")
            }
            ("cryptsetup", ["erase", "--batch-mode", part]) => {
                if let Some(p) = self.part_mut(part) {
                    p.luks = false;
                }
                Reply::ok("")
            }
            ("cryptsetup", ["luksUUID", _]) => Reply::ok("0b5c2a1e-7d3f-4c1a-9e2b-6f8d4a3c2b1e\n"),
            ("cryptsetup", ["luksHeaderBackup", _, "--header-backup-file", file]) => {
                self.touch(file, 1);
                Reply::ok("")
            }
            ("unmkinitramfs", [_, into]) => {
                let main = Path::new(into).join("main");
                std::fs::create_dir_all(main.join("cryptroot")).unwrap();
                std::fs::write(main.join("cryptroot/crypttab"), self.crypttab()).unwrap();
                std::fs::create_dir_all(main.join("usr/lib/cryptsetup/scripts")).unwrap();
                std::fs::write(main.join("usr/lib/cryptsetup/scripts/decrypt_keyctl"), "").unwrap();
                Reply::ok("")
            }
            ("pvesh", _) => Reply::fail(2, format!("no answer for {}", line.join(" "))),
            _ => Reply::ok(""),
        }
    }
}

const CRYPTTAB_PATH: &str = "/etc/crypttab";

#[derive(Clone)]
struct Machine(Arc<Mutex<World>>);

impl Host for Machine {
    fn system(&self) -> SystemInfo {
        SystemInfo::default()
    }
    fn disks(&self) -> Vec<InventoryDisk> {
        self.0.lock().unwrap().disks()
    }
    fn clear_labels(&self, _: &ClearRequest) -> ClearReport {
        ClearReport::default()
    }
    fn now_ms(&self) -> i64 {
        NOW_MS
    }
}

impl Live for Machine {
    fn layout(&self) -> BoxFuture<'_, Result<Layout, String>> {
        let layout = self.0.lock().unwrap().layout();
        Box::pin(async move { Ok(layout) })
    }
}

/// Answers every command from the world, which it changes, and keeps the
/// lines in order.
struct Runner {
    world: Arc<Mutex<World>>,
    fake: FakeRunner,
}

impl Runner {
    fn lines(&self) -> Vec<String> {
        self.fake.call_lines()
    }
}

impl CommandRunner for Runner {
    fn run(&self, command: Command) -> BoxFuture<'_, Result<Output, ExecError>> {
        let line: Vec<String> = std::iter::once(command.program().display().to_string())
            .chain(command.arguments().iter().cloned())
            .collect();
        let reply = self.world.lock().unwrap().respond(&line);
        self.fake
            .expect(command.program(), command.arguments().to_vec(), reply);
        self.fake.run(command)
    }
}

struct Guests(Vec<GuestFacts>);

impl Cluster for Guests {
    fn guests(&self) -> Result<Vec<GuestFacts>, String> {
        Ok(self.0.clone())
    }
    fn pending_cleanups(&self, _: HostSlot) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

fn guest(name: &str, vmid: u32, owner: &str, placement: &[&str]) -> GuestFacts {
    GuestFacts {
        name: name.parse().unwrap(),
        vmid: bmac_core::Vmid::new(vmid).unwrap(),
        owner: Some(owner.parse().unwrap()),
        placement: placement.iter().map(|p| p.parse().unwrap()).collect(),
    }
}

struct Rig {
    root: tempfile::TempDir,
    world: Arc<Mutex<World>>,
    machine: Machine,
    runner: Runner,
    jobs: Vec<tempfile::TempDir>,
}

/// What one job showed.
struct Job {
    outcome: Result<(), Stop>,
    events: Vec<WorkflowEvent>,
    lines: Vec<String>,
}

impl Job {
    fn texts(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                WorkflowEvent::Info { message } | WorkflowEvent::Warning { message } => {
                    Some(message.clone())
                }
                WorkflowEvent::NextStep { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn said(&self, text: &str) -> bool {
        self.texts().iter().any(|t| t.contains(text))
    }

    fn result(&self) -> Value {
        self.events
            .iter()
            .find_map(|e| match e {
                WorkflowEvent::Result { data } => Some(data.clone()),
                _ => None,
            })
            .unwrap_or(Value::Null)
    }

    /// The position of the first command starting with `prefix`.
    fn at(&self, prefix: &str) -> usize {
        self.lines
            .iter()
            .position(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no command {prefix}:\n{}", self.lines.join("\n")))
    }

    fn ran(&self, prefix: &str) -> bool {
        self.lines.iter().any(|l| l.starts_with(prefix))
    }

    fn ok(&self) {
        if let Err(e) = &self.outcome {
            panic!(
                "{e:?}\n{}\n{}",
                self.texts().join("\n"),
                self.lines.join("\n")
            );
        }
    }
}

fn rig() -> Rig {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("proc/sys/kernel")).unwrap();
    std::fs::write(
        root.path().join("proc/sys/kernel/osrelease"),
        "6.14.8-2-pve\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join("etc")).unwrap();
    let world = Arc::new(Mutex::new(World::new(root.path())));
    Rig {
        machine: Machine(world.clone()),
        runner: Runner {
            world: world.clone(),
            fake: FakeRunner::new(),
        },
        world,
        root,
        jobs: Vec::new(),
    }
}

/// Answers each request as it appears, in order.
async fn drive(out: Shared, mut input: DuplexStream, answers: Vec<Value>) {
    let mut answered = 0;
    loop {
        let events = out.events();
        if let Some(refused) = events.iter().find_map(|e| match e {
            WorkflowEvent::ValidationError { field_errors, .. } => Some(field_errors.clone()),
            _ => None,
        }) {
            panic!("the answer was refused: {refused:?}");
        }
        let requests: Vec<&WorkflowEvent> = events.iter().filter(|e| e.is_request()).collect();
        while answered < requests.len() {
            let request = requests[answered];
            let Some(answer) = answers.get(answered) else {
                panic!("no answer for {request:?}");
            };
            let mut answer = answer.clone();
            answer["type"] = json!("response");
            answer["request_id"] = json!(request.request_id().unwrap());
            send(&mut input, answer).await;
            answered += 1;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

impl Rig {
    fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.world.lock().unwrap()
    }

    fn journal(&self) -> Journal {
        Journal::new(self.root.path().join("var/lib/bmac/storage-state.json"))
    }

    fn state(&self) -> State {
        self.journal().read().unwrap()
    }

    fn key(&self) -> PathBuf {
        self.root.path().join(KEY_FILE.trim_start_matches('/'))
    }

    /// `bmac luks-enter` at the console.
    fn enter_key(&self) {
        use std::os::unix::fs::PermissionsExt;
        let key = self.key();
        std::fs::create_dir_all(key.parent().unwrap()).unwrap();
        std::fs::write(&key, "made-up passphrase").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn snapshot(&self) -> Snapshot {
        let world = self.world();
        Snapshot {
            layout: world.layout(),
            disks: world.disks(),
            journal: self.state(),
            ashift: 12,
        }
    }

    async fn run(&mut self, operation: &str, answers: Vec<Value>, cluster: &dyn Cluster) -> Job {
        let work = tempfile::tempdir().unwrap();
        let (flow, out, input) = workflow(work.path());
        let before = self.runner.lines().len();
        let cx = Context {
            flow: &flow,
            host: &self.machine,
            live: &self.machine,
            runner: &self.runner,
            cluster,
            journal: self.journal(),
            root: self.root.path().to_path_buf(),
            node: "mox1".parse().unwrap(),
            pool: "rpool".parse().unwrap(),
            poll: Duration::from_millis(1),
        };
        let outcome = tokio::select! {
            outcome = dispatch(&cx, operation) => outcome,
            () = drive(out.clone(), input, answers.clone()) => unreachable!(),
        };
        let events = out.events();
        let asked = events.iter().filter(|e| e.is_request()).count();
        assert_eq!(asked, answers.len(), "requests asked: {events:#?}");
        drop(cx);
        self.jobs.push(work);
        Job {
            outcome,
            events,
            lines: self.runner.lines()[before..].to_vec(),
        }
    }
}

fn values(v: Value) -> Value {
    json!({ "values": v })
}

fn yes() -> Value {
    json!({ "confirmed": true })
}

fn no_guests() -> Guests {
    Guests(Vec::new())
}

/// A boot mirror of sda and sdb, and blank sdc and sdd.
fn plain_pool(rig: &Rig) {
    let mut w = rig.world();
    let a = w.boot_drive("sda", "SN-A", false);
    let b = w.boot_drive("sdb", "SN-B", false);
    w.top("mirror-0", "mirror", None, vec![a, b]);
    w.drive("sdc", "SN-C", DISK).drive("sdd", "SN-D", DISK);
}

/// An encrypted boot mirror, an encrypted second mirror of sdc and sdd,
/// and a blank sde.
fn encrypted_pool(rig: &Rig) {
    let mut w = rig.world();
    let a = w.boot_drive("sda", "SN-A", true);
    let b = w.boot_drive("sdb", "SN-B", true);
    w.top("mirror-0", "mirror", None, vec![a, b]);
    let c = w.member_drive("sdc", "SN-C", true);
    let d = w.member_drive("sdd", "SN-D", true);
    w.top("mirror-1", "mirror", None, vec![c, d]);
    w.drive("sde", "SN-E", DISK);
    let crypttab: String = ["SN-A", "SN-B", "SN-C", "SN-D"]
        .iter()
        .map(|s| {
            format!(
                "{} UUID=1 bmac-rpool luks,discard,keyscript=decrypt_keyctl\n",
                mapper_of(s)
            )
        })
        .collect();
    std::fs::write(w.at(CRYPTTAB_PATH), crypttab).unwrap();
}

fn serial(s: &str) -> DiskIdentity {
    DiskIdentity::Serial(s.into())
}

fn pool_name() -> PoolName {
    "rpool".parse().unwrap()
}

fn form(class: &str, kind: &str, disks: &[&str]) -> serde_json::Map<String, Value> {
    json!({
        "class": class,
        "type": kind,
        "disks": disks.iter().map(|s| path_of(s).trim_start_matches("/dev/disk/by-id/").to_string()).collect::<Vec<_>>(),
    })
    .as_object()
    .unwrap()
    .clone()
}

// ---- what each workflow offers and refuses ----

#[test]
fn a_new_vdev_takes_blank_disks_of_a_type_its_class_allows() {
    let rig = rig();
    plain_pool(&rig);
    let snapshot = rig.snapshot();
    let pool = pool_name();

    let (vdev, disks, _) =
        add::proposal(&snapshot, &pool, &form("data", "mirror", &["SN-C", "SN-D"])).unwrap();
    assert_eq!(vdev.describe(), "2-way data mirror");
    assert_eq!(disks, vec![serial("SN-C"), serial("SN-D")]);

    let refused = add::proposal(
        &snapshot,
        &pool,
        &form("cache", "mirror", &["SN-C", "SN-D"]),
    )
    .unwrap_err();
    assert!(refused["type"].contains("cache"), "{refused:?}");
    let refused = add::proposal(&snapshot, &pool, &form("data", "mirror", &["SN-C"])).unwrap_err();
    assert!(refused["disks"].contains("at least 2"), "{refused:?}");
    let refused =
        add::proposal(&snapshot, &pool, &form("data", "mirror", &["SN-A", "SN-C"])).unwrap_err();
    assert!(
        refused["disks"].contains("can't be used now"),
        "{refused:?}"
    );
}

#[test]
fn disks_in_use_too_small_or_chosen_by_a_workflow_are_not_offered() {
    let rig = rig();
    plain_pool(&rig);
    {
        let mut w = rig.world();
        w.drive("sde", "SN-E", 4 * GIB);
        w.member_drive("sdf", "SN-F", true);
        w.drive("sdg", "SN-G", DISK);
    }
    rig.journal()
        .update(|s| s.begin_addition(VdevClass::Data, VdevType::Single, vec![serial("SN-G")], 0))
        .unwrap();
    let snapshot = rig.snapshot();
    let offered: Vec<&str> = snapshot
        .candidates()
        .iter()
        .map(|c| c.disk.name.as_str())
        .collect();
    assert_eq!(offered, vec!["sdc", "sdd"]);
}

#[test]
fn raidz_and_single_disk_data_vdevs_say_what_they_rule_out() {
    let rig = rig();
    plain_pool(&rig);
    rig.world().drive("sde", "SN-E", DISK);
    let snapshot = rig.snapshot();
    let pool = pool_name();
    let (raidz, _, _) = add::proposal(
        &snapshot,
        &pool,
        &form("data", "raidz1", &["SN-C", "SN-D", "SN-E"]),
    )
    .unwrap();
    let said = add::consequences(&snapshot, &pool, &raidz).join(" ");
    assert!(said.contains("can ever be removed"), "{said}");
    let (single, _, _) =
        add::proposal(&snapshot, &pool, &form("data", "single", &["SN-C"])).unwrap();
    let said = add::consequences(&snapshot, &pool, &single).join(" ");
    assert!(said.contains("survives no disk failure"), "{said}");
    let (log, _, _) =
        add::proposal(&snapshot, &pool, &form("log", "mirror", &["SN-C", "SN-D"])).unwrap();
    assert!(add::consequences(&snapshot, &pool, &log).is_empty());
}

#[test]
fn a_disk_attaches_to_mirrors_and_singles_and_to_raidz_only_with_the_feature() {
    let rig = rig();
    plain_pool(&rig);
    {
        let mut w = rig.world();
        let e = w.member_drive("sde", "SN-E", false);
        let f = w.member_drive("sdf", "SN-F", false);
        let g = w.member_drive("sdg", "SN-G", false);
        w.top("raidz1-1", "raidz1", None, vec![e, f, g]);
        let h = w.member_drive("sdh", "SN-H", false);
        w.top("cache-h", "disk", Some("cache"), vec![h]);
        let i = w.member_drive("sdi", "SN-I", false);
        let j = w.member_drive("sdj", "SN-J", false);
        w.top("mirror-3", "mirror", None, vec![i, j]);
        w.tops[3].leaves[1].state = "FAULTED".into();
    }
    let snapshot = rig.snapshot();
    let top = |name: &str| snapshot.top(name).unwrap();
    assert!(
        add::attachable(&top("mirror-0"), true)
            .unwrap_err()
            .contains("boot vdev")
    );
    assert!(
        add::attachable(&top("raidz1-1"), false)
            .unwrap_err()
            .contains("raidz_expansion")
    );
    add::attachable(&top("raidz1-1"), true).unwrap();
    assert!(
        add::attachable(&top("cache-h"), true)
            .unwrap_err()
            .contains("cache")
    );
    assert!(
        add::attachable(&top("mirror-3"), true)
            .unwrap_err()
            .contains("online")
    );

    let raidz = top("raidz1-1");
    add::big_enough(&raidz, DISK, Encryption::None).unwrap();
    assert!(
        add::big_enough(&raidz, 12 * GIB, Encryption::None)
            .unwrap_err()
            .contains("needs")
    );
}

#[test]
fn replacing_skips_spares_standing_in_and_members_already_being_replaced() {
    let rig = rig();
    plain_pool(&rig);
    {
        let mut w = rig.world();
        let e = w.member_drive("sde", "SN-E", false);
        let f = w.member_drive("sdf", "SN-F", false);
        let s = w.member_drive("sds", "SN-S", false);
        w.top(
            "mirror-1",
            "mirror",
            None,
            vec![e.clone(), f.clone(), s.clone()],
        );
        w.tops[1].leaves[0].state = "FAULTED".into();
        w.tops[1].leaves[0].group = Some("spare-0".into());
        w.tops[1].leaves[2].group = Some("spare-0".into());
        w.tops[1].leaves[1].group = Some("replacing-1".into());
        w.top("spare-s", "disk", Some("spares"), vec![s]);
        let g = w.member_drive("sdg", "SN-G", false);
        w.top(&g, "disk", None, vec![g.clone()]);
        w.tops[3].leaves[0].state = "FAULTED".into();
    }
    let snapshot = rig.snapshot();
    let (ok, not) = replace::choices(&snapshot);
    let offered: Vec<&str> = ok.iter().map(|c| c.leaf.as_str()).collect();
    assert_eq!(
        offered,
        vec![
            path_of("SN-A") + "-part3",
            path_of("SN-B") + "-part3",
            path_of("SN-E") + "-part1",
        ]
    );
    let not = not.join("\n");
    assert!(not.contains("already being replaced"), "{not}");
    assert!(not.contains("hot spare standing in"), "{not}");
    assert!(not.contains("removed and added"), "{not}");
    assert!(not.contains("nothing to rebuild it from"), "{not}");
}

#[test]
fn only_pools_without_raidz_give_up_data_vdevs_and_only_with_room_to_spare() {
    let rig = rig();
    plain_pool(&rig);
    {
        let mut w = rig.world();
        let c = w.member_drive("sdc2", "SN-C2", false);
        let d = w.member_drive("sdd2", "SN-D2", false);
        w.top("mirror-1", "mirror", None, vec![c, d]);
        let l = w.member_drive("sdl", "SN-L", false);
        w.top(&l, "disk", Some("logs"), vec![l.clone()]);
    }
    let snapshot = rig.snapshot();
    let top = |name: &str| snapshot.top(name).unwrap();
    remove::removable(&snapshot, &top("mirror-1")).unwrap();
    assert!(
        remove::removable(&snapshot, &top("mirror-0"))
            .unwrap_err()
            .contains("boot")
    );
    remove::fits(&snapshot, &top("mirror-1"), 50 * GIB).unwrap();
    assert!(
        remove::fits(&snapshot, &top("mirror-1"), 200 * GIB)
            .unwrap_err()
            .contains("room")
    );

    {
        let mut w = rig.world();
        let e = w.member_drive("sde", "SN-E", false);
        let f = w.member_drive("sdf", "SN-F", false);
        let g = w.member_drive("sdg", "SN-G", false);
        w.top("raidz1-3", "raidz1", None, vec![e, f, g]);
    }
    let snapshot = rig.snapshot();
    let log = path_of("SN-L") + "-part1";
    let refused = remove::removable(&snapshot, &snapshot.top("mirror-1").unwrap()).unwrap_err();
    assert!(refused.contains("raidz"), "{refused}");
    remove::removable(&snapshot, &snapshot.top(&log).unwrap()).unwrap();
}

#[test]
fn the_only_data_vdev_stays() {
    let rig = rig();
    plain_pool(&rig);
    rig.world().drives.iter_mut().for_each(|d| d.esp = false);
    let snapshot = rig.snapshot();
    let refused = remove::removable(&snapshot, &snapshot.top("mirror-0").unwrap()).unwrap_err();
    assert!(refused.contains("only data vdev"), "{refused}");
}

#[test]
fn drills_need_a_healthy_pool_and_a_redundant_whole_vdev() {
    let rig = rig();
    plain_pool(&rig);
    {
        let mut w = rig.world();
        let c = w.member_drive("sdc2", "SN-C2", false);
        w.top(&c, "disk", None, vec![c.clone()]);
    }
    let snapshot = rig.snapshot();
    simulate::pool_ready(&snapshot).unwrap();
    let mirror = snapshot.top("mirror-0").unwrap();
    simulate::drillable(&mirror, &mirror.members[0]).unwrap();
    let single = snapshot.tops()[1].clone();
    assert!(
        simulate::drillable(&single, &single.members[0])
            .unwrap_err()
            .contains("redundancy")
    );

    rig.world().scan = Some("resilver in progress since Tue Oct 13".into());
    assert!(
        simulate::pool_ready(&rig.snapshot())
            .unwrap_err()
            .contains("running")
    );
    rig.world().scan = None;
    rig.world().tops[0].leaves[1].state = "OFFLINE".into();
    let snapshot = rig.snapshot();
    assert!(
        simulate::pool_ready(&snapshot)
            .unwrap_err()
            .contains("DEGRADED")
    );
    let mirror = snapshot.top("mirror-0").unwrap();
    assert!(
        simulate::drillable(&mirror, &mirror.members[0])
            .unwrap_err()
            .contains("whole")
    );
}

#[test]
fn inventory_finishes_a_removal_only_once_the_vdev_is_gone() {
    let rig = rig();
    plain_pool(&rig);
    let removal = bmac_storage::journal::Removal {
        id: "r1".into(),
        vdev: "mirror-0".into(),
        members: Vec::new(),
        state: RemovalState::Requested,
        requested_at: 0,
        updated_at: 0,
        notes: Vec::new(),
    };
    let snapshot = rig.snapshot();
    assert_eq!(
        inventory::removal_next(&snapshot, &removal),
        inventory::RemovalNext::Stopped
    );
    let gone = bmac_storage::journal::Removal {
        vdev: "mirror-9".into(),
        ..removal
    };
    assert_eq!(
        inventory::removal_next(&snapshot, &gone),
        inventory::RemovalNext::Finish
    );
}

#[test]
fn a_replacement_is_done_when_the_new_disk_is_in_and_resilvered() {
    let rig = rig();
    plain_pool(&rig);
    let r = bmac_storage::journal::Replacement {
        leaf: "/dev/old".into(),
        old: None,
        mapper: None,
        vdev: "mirror-0".into(),
        recorded_at: 0,
    };
    let snapshot = rig.snapshot();
    assert!(inventory::replacement_done(&snapshot, &serial("SN-A"), &r));
    assert!(!inventory::replacement_done(&snapshot, &serial("SN-C"), &r));
    rig.world().scan = Some("resilver in progress since Tue Oct 13".into());
    assert!(!inventory::replacement_done(
        &rig.snapshot(),
        &serial("SN-A"),
        &r
    ));
}

// ---- the workflows, run against the host in memory ----

#[tokio::test]
async fn adding_a_mirror_erases_the_disks_then_partitions_and_adds_them() {
    let mut rig = rig();
    plain_pool(&rig);
    rig.world().set_parts(
        "SN-C",
        vec![Part {
            number: 1,
            start: MIB,
            size: GIB,
            luks: false,
        }],
    );
    let disks = form("data", "mirror", &["SN-C", "SN-D"]);
    let job = rig
        .run(
            ADD_VDEV,
            vec![values(Value::Object(disks)), yes()],
            &no_guests(),
        )
        .await;
    job.ok();
    let c = path_of("SN-C");
    let d = path_of("SN-D");
    assert!(
        job.at(&format!("/usr/sbin/zpool labelclear -f {c}-part1"))
            < job.at(&format!("/usr/sbin/sgdisk --zap-all {c}"))
    );
    assert!(
        job.at(&format!("/usr/sbin/wipefs --all --force {d}"))
            < job.at("/usr/sbin/sgdisk --new=1:")
    );
    assert!(job.at("/usr/sbin/sgdisk --new=1:") < job.at("/usr/sbin/zpool add"));
    assert!(job.lines.contains(&format!(
        "/usr/sbin/zpool add -o ashift=12 rpool mirror {c}-part1 {d}-part1"
    )));
    // Proxmox doesn't install cryptsetup; only encrypted hosts get it.
    assert!(!job.ran("/usr/sbin/cryptsetup"), "{:?}", job.lines);
    assert!(job.said("Added a 2-way data mirror to rpool."));
    let state = rig.state();
    assert!(state.additions.is_empty());
    assert_eq!(state.last_addition, 1);
    assert_eq!(rig.world().tops[1].leaves.len(), 2);
}

#[tokio::test]
async fn an_encrypted_vdev_unlocks_at_boot_before_it_joins_the_pool() {
    let mut rig = rig();
    encrypted_pool(&rig);
    rig.world().drive("sdf", "SN-F", DISK);
    rig.enter_key();
    let disks = form("data", "mirror", &["SN-E", "SN-F"]);
    let job = rig
        .run(
            ADD_VDEV,
            vec![values(Value::Object(disks)), yes()],
            &no_guests(),
        )
        .await;
    job.ok();
    let e = path_of("SN-E");
    let tested = job.at(&format!(
        "/usr/sbin/cryptsetup open --test-passphrase --key-file={KEY_FILE} /dev/sda3"
    ));
    let formatted = job.at(&format!(
        "/usr/sbin/cryptsetup luksFormat --batch-mode --type luks2 --key-file={KEY_FILE} {e}-part1"
    ));
    let proved = job.at("/usr/bin/unmkinitramfs");
    let added = job.at("/usr/sbin/zpool add");
    assert!(tested < formatted && formatted < proved && proved < added);
    assert_eq!(
        job.lines[added],
        format!(
            "/usr/sbin/zpool add -o ashift=12 rpool mirror /dev/mapper/{} /dev/mapper/{}",
            mapper_of("SN-E"),
            mapper_of("SN-F")
        )
    );
    assert!(job.at("/usr/sbin/cryptsetup luksHeaderBackup") > added);
    let crypttab = rig.world().crypttab();
    assert!(
        crypttab.contains(&mapper_of("SN-E")) && crypttab.contains(&mapper_of("SN-A")),
        "{crypttab}"
    );
    assert!(!rig.key().exists(), "the passphrase file outlived the job");
}

#[tokio::test]
async fn an_addition_cut_short_is_offered_again_and_carries_on() {
    let mut rig = rig();
    plain_pool(&rig);
    rig.journal()
        .update(|s| {
            s.begin_addition(
                VdevClass::Data,
                VdevType::Mirror,
                vec![serial("SN-C"), serial("SN-D")],
                0,
            )
        })
        .unwrap();
    let job = rig
        .run(
            ADD_VDEV,
            vec![values(json!({"choice": "resume"}))],
            &no_guests(),
        )
        .await;
    job.ok();
    assert!(
        !job.ran("/usr/sbin/zpool labelclear"),
        "a recorded disk is erased again"
    );
    assert!(job.ran("/usr/sbin/zpool add -o ashift=12 rpool mirror"));
    assert!(rig.state().additions.is_empty());
}

#[tokio::test]
async fn a_drill_takes_a_member_offline_erases_it_and_replace_puts_it_back() {
    let mut rig = rig();
    encrypted_pool(&rig);
    let c = path_of("SN-C");
    let leaf = format!("/dev/mapper/{}", mapper_of("SN-C"));
    let job = rig
        .run(
            SIMULATE_FAILURE,
            vec![
                values(json!({"member": leaf})),
                yes(),
                values(json!({"choice": "erase"})),
                yes(),
            ],
            &no_guests(),
        )
        .await;
    job.ok();
    let offline = job.at(&format!("/usr/sbin/zpool offline rpool {leaf}"));
    let closed = job.at(&format!("/usr/sbin/cryptsetup close {}", mapper_of("SN-C")));
    let erased = job.at(&format!(
        "/usr/sbin/cryptsetup erase --batch-mode {c}-part1"
    ));
    let discarded = job.at(&format!("/usr/sbin/blkdiscard --force {c}"));
    assert!(offline < closed && closed < erased && erased < discarded);
    let drill = rig.state().simulation.unwrap();
    assert_eq!(drill.stage, SimulationStage::Erased);
    assert_eq!(drill.mapper.as_deref(), Some(mapper_of("SN-C").as_str()));
    assert!(rig.world().drives[2].parts.is_empty());
    assert!(job.said("Run Replace a disk"));

    rig.enter_key();
    let job = rig
        .run(
            REPLACE_DISK,
            vec![
                values(json!({"member": format!("mirror-1\t{leaf}"), "disk": by_id_of("SN-C")})),
                yes(),
            ],
            &no_guests(),
        )
        .await;
    job.ok();
    assert!(job.said("This finishes the failure drill"));
    assert!(job.at("/usr/sbin/sgdisk --new=1:") < job.at("/usr/sbin/cryptsetup luksFormat"));
    assert!(job.at("/usr/bin/unmkinitramfs") < job.at("/usr/sbin/zpool replace"));
    assert!(
        job.lines
            .contains(&format!("/usr/sbin/zpool replace rpool {leaf}"))
    );
    let state = rig.state();
    assert!(state.simulation.is_none());
    assert_eq!(
        state.replacements[&serial("SN-C")].mapper.as_deref(),
        Some(mapper_of("SN-C").as_str())
    );
    assert!(!rig.key().exists());

    let job = rig.run(INVENTORY, vec![yes()], &no_guests()).await;
    job.ok();
    assert!(
        !job.ran("/usr/sbin/cryptsetup close"),
        "the drilled mapper is the new member's"
    );
    assert!(!job.ran("/usr/sbin/update-initramfs"));
    assert!(rig.state().replacements.is_empty());
    assert!(rig.world().crypttab().contains(&mapper_of("SN-C")));
}

#[tokio::test]
async fn a_drilled_member_can_come_back_online_instead() {
    let mut rig = rig();
    encrypted_pool(&rig);
    let leaf = format!("/dev/mapper/{}", mapper_of("SN-D"));
    rig.enter_key();
    let job = rig
        .run(
            SIMULATE_FAILURE,
            vec![
                values(json!({"member": leaf})),
                yes(),
                values(json!({"choice": "restore"})),
            ],
            &no_guests(),
        )
        .await;
    job.ok();
    assert!(
        job.at("/usr/sbin/cryptsetup close")
            < job.at(&format!("/usr/sbin/cryptsetup open --key-file={KEY_FILE}"))
    );
    assert!(job.ran(&format!("/usr/sbin/zpool online rpool {leaf}")));
    assert!(!job.ran("/usr/sbin/cryptsetup erase"));
    assert!(rig.state().simulation.is_none());
    assert_eq!(rig.world().tops[1].leaves[1].state, "ONLINE");
    assert!(!rig.key().exists());
}

#[tokio::test]
async fn replacing_a_failed_member_retires_the_old_disk_once_resilvered() {
    let mut rig = rig();
    encrypted_pool(&rig);
    let old = format!("/dev/mapper/{}", mapper_of("SN-D"));
    rig.world().tops[1].leaves[1].state = "FAULTED".into();
    rig.enter_key();
    let job = rig
        .run(
            REPLACE_DISK,
            vec![
                values(json!({"member": format!("mirror-1\t{old}"), "disk": by_id_of("SN-E")})),
                yes(),
            ],
            &no_guests(),
        )
        .await;
    job.ok();
    let new = format!("/dev/mapper/{}", mapper_of("SN-E"));
    assert!(
        job.lines
            .contains(&format!("/usr/sbin/zpool replace rpool {old} {new}"))
    );
    let r = rig.state().replacements[&serial("SN-E")].clone();
    assert_eq!(r.old, Some(serial("SN-D")));
    assert!(job.said("run Storage inventory"));

    let job = rig.run(INVENTORY, vec![yes()], &no_guests()).await;
    job.ok();
    let d = path_of("SN-D");
    let closed = job.at(&format!("/usr/sbin/cryptsetup close {}", mapper_of("SN-D")));
    let rebuilt = job.at("/usr/sbin/update-initramfs -u -k all");
    let erased = job.at(&format!(
        "/usr/sbin/cryptsetup erase --batch-mode {d}-part1"
    ));
    assert!(closed < rebuilt && rebuilt < erased);
    let crypttab = rig.world().crypttab();
    assert!(
        !crypttab.contains(&mapper_of("SN-D")) && crypttab.contains(&mapper_of("SN-E")),
        "{crypttab}"
    );
    assert!(rig.state().replacements.is_empty());
    let pullable = job.result()["safe_to_pull"].clone();
    assert_eq!(pullable, json!(["/dev/sdd"]));
}

#[tokio::test]
async fn replacing_a_boot_member_copies_the_survivor_and_sets_up_its_esp() {
    let mut rig = rig();
    plain_pool(&rig);
    let old = path_of("SN-B") + "-part3";
    rig.world().tops[0].leaves[1].state = "UNAVAIL".into();
    rig.world().drives.remove(1);
    let job = rig
        .run(
            REPLACE_DISK,
            vec![
                values(json!({"member": format!("mirror-0\t{old}"), "disk": by_id_of("SN-C")})),
                yes(),
            ],
            &no_guests(),
        )
        .await;
    job.ok();
    let c = path_of("SN-C");
    let a = path_of("SN-A");
    let copied = job.at(&format!("/usr/sbin/sgdisk --replicate={c} {a}"));
    let esp = job.at(&format!(
        "/usr/sbin/proxmox-boot-tool format {c}-part2 --force"
    ));
    let replaced = job.at("/usr/sbin/zpool replace");
    assert!(copied < esp && esp < replaced);
    assert!(job.ran(&format!("/usr/sbin/proxmox-boot-tool init {c}-part2 grub")));
    assert_eq!(
        job.lines[replaced],
        format!("/usr/sbin/zpool replace rpool {old} {c}-part3")
    );
}

fn remove_answers() -> Vec<Value> {
    vec![
        values(json!({"vdev": "mirror-1", "min_free_gib": 50})),
        yes(),
    ]
}

#[tokio::test]
async fn removing_a_vdev_trims_replicates_and_scrubs_first_then_inventory_releases_it() {
    let mut rig = rig();
    plain_pool(&rig);
    {
        let mut w = rig.world();
        w.drives.retain(|d| d.name != "sdc" && d.name != "sdd");
        let c = w.member_drive("sdc", "SN-C", false);
        let d = w.member_drive("sdd", "SN-D", false);
        w.top("mirror-1", "mirror", None, vec![c, d]);
        let mut synced = false;
        w.extra = Some(Box::new(move |line: &[String]| {
            let line = line.join(" ");
            let path = line.split(' ').nth(2).unwrap_or_default().to_string();
            Some(match (line.split(' ').nth(1)?, path.as_str()) {
                ("create", "/nodes/mox1/qemu/101/agent/exec") => Reply::ok(r#"{"pid":7}"#),
                ("get", "/nodes/mox1/qemu/101/agent/exec-status") => {
                    Reply::ok(r#"{"exited":1,"exitcode":0}"#)
                }
                ("get", "/nodes/mox1/time") => Reply::ok(r#"{"time":1000}"#),
                ("create", "/nodes/mox1/replication/101-0/schedule_now") => {
                    synced = true;
                    Reply::ok("")
                }
                ("get", "/nodes/mox1/replication") => Reply::ok(
                    json!([{"id": "101-0", "guest": 101, "target": "mox2",
                            "last_sync": if synced { 1001 } else { 10 }}])
                    .to_string(),
                ),
                _ => return None,
            })
        }));
    }
    let cluster = Guests(vec![
        guest("prod101", 101, "mox1", &["mox1", "mox2"]),
        guest("prod102", 102, "mox2", &["mox2", "mox3"]),
    ]);
    let job = rig.run(REMOVE_VDEV, remove_answers(), &cluster).await;
    job.ok();
    let trimmed = job
        .at("/usr/bin/pvesh create /nodes/mox1/qemu/101/agent/exec --command fstrim --command -av");
    let replicated = job.at("/usr/bin/pvesh create /nodes/mox1/replication/101-0/schedule_now");
    let scrubbed = job.at("/usr/sbin/zpool scrub -w rpool");
    let removed = job.at("/usr/sbin/zpool remove rpool mirror-1");
    assert!(trimmed < replicated && replicated < scrubbed && scrubbed < removed);
    assert!(
        !job.ran("/usr/bin/pvesh create /nodes/mox2"),
        "a guest elsewhere was trimmed"
    );
    let state = rig.state();
    assert!(state.recently_trimmed("prod101".parse().unwrap(), NOW_MS / 1000));
    assert!(state.recent_clean_scrub(NOW_MS / 1000).is_some());
    assert_eq!(state.removals[0].state, RemovalState::Requested);
    assert!(job.said("run Storage inventory"));

    let job = rig.run(INVENTORY, vec![yes()], &no_guests()).await;
    job.ok();
    assert!(job.ran(&format!("/usr/sbin/sgdisk --zap-all {}", path_of("SN-C"))));
    assert!(job.ran(&format!("/usr/sbin/sgdisk --zap-all {}", path_of("SN-D"))));
    assert!(!job.ran("/usr/sbin/update-initramfs"));
    assert_eq!(rig.state().removals[0].state, RemovalState::Retired);
    assert_eq!(
        job.result()["safe_to_pull"],
        json!(["/dev/sdc", "/dev/sdd"])
    );
}

#[tokio::test]
async fn a_staging_vm_on_the_host_stops_a_removal_before_anything_runs() {
    let mut rig = rig();
    plain_pool(&rig);
    {
        let mut w = rig.world();
        w.drives.retain(|d| d.name != "sdc" && d.name != "sdd");
        let c = w.member_drive("sdc", "SN-C", false);
        let d = w.member_drive("sdd", "SN-D", false);
        w.top("mirror-1", "mirror", None, vec![c, d]);
    }
    let cluster = Guests(vec![guest("stage1prod101", 201, "mox1", &["mox1"])]);
    let job = rig.run(REMOVE_VDEV, remove_answers(), &cluster).await;
    let Err(Stop::Failed { message, .. }) = &job.outcome else {
        panic!("{:?}", job.outcome);
    };
    assert!(message.contains("stage1prod101"), "{message}");
    assert!(
        !job.ran("/usr/bin/pvesh")
            && !job.ran("/usr/sbin/zpool scrub")
            && !job.ran("/usr/sbin/zpool remove")
    );
    assert!(rig.state().removals.is_empty());
}

#[tokio::test]
async fn inventory_retires_an_encrypted_vdev_once_its_removal_finishes() {
    let mut rig = rig();
    encrypted_pool(&rig);
    rig.run(REMOVE_VDEV, remove_answers(), &no_guests())
        .await
        .ok();
    let recorded: Vec<_> = rig.state().removals[0]
        .members
        .iter()
        .map(|m| m.mapper.clone())
        .collect();
    assert_eq!(
        recorded,
        vec![Some(mapper_of("SN-C")), Some(mapper_of("SN-D"))]
    );
    rig.world().tops.retain(|t| t.name != "mirror-1");
    let job = rig.run(INVENTORY, vec![yes()], &no_guests()).await;
    job.ok();
    let closed = job.at(&format!("/usr/sbin/cryptsetup close {}", mapper_of("SN-C")));
    let proved = job.at("/usr/bin/unmkinitramfs");
    let erased = job.at(&format!(
        "/usr/sbin/cryptsetup erase --batch-mode {}-part1",
        path_of("SN-D")
    ));
    assert!(closed < proved && proved < erased);
    let crypttab = rig.world().crypttab();
    assert!(!crypttab.contains(&mapper_of("SN-C")) && !crypttab.contains(&mapper_of("SN-D")));
    assert!(crypttab.contains(&mapper_of("SN-A")), "{crypttab}");
    assert_eq!(rig.state().removals[0].state, RemovalState::Retired);
    assert_eq!(
        job.result()["safe_to_pull"],
        json!(["/dev/sdc", "/dev/sdd", "/dev/sde"])
    );
}

#[tokio::test]
async fn inventory_closes_a_removal_that_stopped_with_the_vdev_in_the_pool() {
    let mut rig = rig();
    encrypted_pool(&rig);
    let members = vec![RemovedMember {
        identity: Some(serial("SN-C")),
        mapper: Some(mapper_of("SN-C")),
        size: Some(DISK),
    }];
    rig.journal()
        .update(|s| s.record_removal("mirror-1", members, 0))
        .unwrap();
    let job = rig.run(INVENTORY, Vec::new(), &no_guests()).await;
    job.ok();
    assert!(job.said("stopped with the vdev still in the pool"));
    assert_eq!(rig.state().removals[0].state, RemovalState::Failed);
    assert!(!job.ran("/usr/sbin/cryptsetup close"));
}

#[tokio::test]
async fn attaching_widens_a_mirror_from_one_of_its_online_members() {
    let mut rig = rig();
    plain_pool(&rig);
    {
        let mut w = rig.world();
        w.drives.retain(|d| d.name != "sdc" && d.name != "sdd");
        let c = w.member_drive("sdc", "SN-C", false);
        let d = w.member_drive("sdd", "SN-D", false);
        w.top("mirror-1", "mirror", None, vec![c, d]);
        w.drive("sde", "SN-E", DISK);
    }
    let job = rig
        .run(
            ATTACH_DISK,
            vec![
                values(json!({"vdev": "mirror-1", "disk": by_id_of("SN-E")})),
                yes(),
            ],
            &no_guests(),
        )
        .await;
    job.ok();
    let attach = format!(
        "/usr/sbin/zpool attach rpool {}-part1 {}-part1",
        path_of("SN-C"),
        path_of("SN-E")
    );
    assert!(job.at(&format!("/usr/sbin/sgdisk --zap-all {}", path_of("SN-E"))) < job.at(&attach));
    assert_eq!(rig.world().tops[1].leaves.len(), 3);
    assert!(job.said("resilvering mirror-1"));
}
