// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The storage operations (`STORAGE.md`, "Workflows"): add a vdev, attach a
//! disk, replace a disk, remove a vdev, take inventory, and drill a disk
//! failure, each run as a job on the host whose disks it changes.
//!
//! Every phase starts from the host's live state: the pool's layout as
//! `zpool status` and `lsblk` show it, the disks as the inventory reads
//! them, and the host's storage journal (`bmac_storage::journal`), which
//! says what an earlier run chose when the disks alone can't. Disks are
//! named by their identity (WWN or serial) and resolved again before every
//! destructive step. The member preparation (partition, LUKS, the
//! single-prompt unlock and its initramfs proof) is the setup phase's
//! ([`Storage`]), run on a layout of the live pool plus the new disks.

mod add;
mod inventory;
mod remove;
mod replace;
mod simulate;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use bmac_adapters::layout::{self, DiskRow, Layout, LocalHost, Member};
use bmac_adapters::programs;
use bmac_adapters::zpool::Zpool;
use bmac_core::job::{Field, FieldOption, FieldType, PhaseStatus};
use bmac_core::{Bytes, DiskIdentity, HostSlot, PoolName};
use bmac_exec::{BoxFuture, Command, CommandRunner};
use bmac_proto::api::Role;
use bmac_proto::inventory::InventoryDisk;
use bmac_proto::operations::{
    CancelRule, Category, Operation, OperationKind, ResumeRule, TargetRule,
};
use bmac_storage::checks::{self, Findings, Transport};
use bmac_storage::journal::{Journal, State};
use bmac_storage::{ById, Encryption, PoolLayout, Vdev, VdevClass, VdevType};
use serde_json::{Value, json};

use super::setup::{Host, RealHost};
use super::storage::{self as setup_storage, KEY_FILE, Storage};
use super::{Stop, Workflow};

pub use remove::{Cluster, GuestFacts};

pub const ADD_VDEV: &str = "storage.add_vdev";
pub const ATTACH_DISK: &str = "storage.attach_disk";
pub const REPLACE_DISK: &str = "storage.replace_disk";
pub const REMOVE_VDEV: &str = "storage.remove_vdev";
pub const INVENTORY: &str = "storage.inventory";
pub const SIMULATE_FAILURE: &str = "storage.simulate_failure";
pub const OPERATIONS: [&str; 6] = [
    ADD_VDEV,
    ATTACH_DISK,
    REPLACE_DISK,
    REMOVE_VDEV,
    INVENTORY,
    SIMULATE_FAILURE,
];

/// The phase that changes the pool; a cancel waits for it to end.
pub const POOL_PHASE: (&str, &str) = ("pool", "Change the pool");
/// The LUKS2 header in front of a mapper's data.
pub const LUKS2_HEADER_BYTES: u64 = 16 << 20;
const BLKDISCARD: &str = "/usr/sbin/blkdiscard";
const WIPE_EDGE_BYTES: u64 = 4 << 20;

struct Spec {
    id: &'static str,
    title: &'static str,
    summary: &'static str,
    description: &'static str,
    replaces: &'static str,
}

const SPECS: [Spec; 6] = [
    Spec {
        id: ADD_VDEV,
        title: "Add a vdev",
        summary: "Add blank disks to the pool as a new vdev of any class and type.",
        description: "Shows what each vdev type allows later before one is chosen, checks the \
                      disks, erases them, encrypts them when the pool is encrypted (proving the \
                      initramfs unlocks them first), and adds them with zpool add. A raidz vdev \
                      rules out ever removing a vdev from the pool.",
        replaces: "add_new_disk_vdev.sh",
    },
    Spec {
        id: ATTACH_DISK,
        title: "Attach a disk",
        summary: "Widen a mirror, turn a single disk into a mirror, or add a disk to a raidz vdev.",
        description: "Erases and prepares the new disk as Add a vdev does, then attaches it with \
                      zpool attach. Adding a disk to raidz needs OpenZFS's raidz_expansion \
                      feature, checked on the host first; existing data keeps its old \
                      data-to-parity ratio until it is rewritten.",
        replaces: "(new)",
    },
    Spec {
        id: REPLACE_DISK,
        title: "Replace a disk",
        summary: "Replace a failed, missing, or unwanted member with a blank disk.",
        description: "For a boot member, copies the surviving member's partitions and sets up \
                      the new disk's boot partition first. Encrypts the new member when the \
                      pool is encrypted and proves the initramfs unlocks it before zpool \
                      replace. Doesn't wait for the resilver, which shows on the Storage page. \
                      A hot spare ZFS already put in returns to the spares when the resilver \
                      finishes.",
        replaces: "add_replacement_disk.sh",
    },
    Spec {
        id: REMOVE_VDEV,
        title: "Remove a vdev",
        summary: "Start removing a vdev, after trimming and replicating guests and a clean scrub.",
        description: "Data and special vdevs can be removed only from pools without raidz. \
                      Related staging VMs must be gone first. Trims every production guest on \
                      or replicating to this host, forces replication, scrubs (reusing trims and \
                      a clean scrub from the last day), checks the minimum free space, and runs \
                      zpool remove. Run Storage inventory when the removal finishes.",
        replaces: "decommission_disks.sh",
    },
    Spec {
        id: INVENTORY,
        title: "Storage inventory",
        summary: "List the disks, finish removals and replacements, and say which disks can be pulled.",
        description: "For a finished removal, closes its LUKS mappings, takes them out of \
                      crypttab and the initramfs, and erases the disks' metadata (never their \
                      data). Does the same for the old disk of a finished replacement.",
        replaces: "inventory_disks.sh",
    },
    Spec {
        id: SIMULATE_FAILURE,
        title: "Simulate a disk failure",
        summary: "Take a redundant member offline and erase it, as if it had been pulled and swapped.",
        description: "A drill for the replacement workflow on hosts whose disks can't be pulled \
                      while running. Takes one member of a healthy redundant vdev offline, then \
                      either erases it so Replace a disk can put it back as a new disk, or \
                      brings it back online.",
        replaces: "simulate_disk_failure_and_replacement.sh",
    },
];

fn spec(id: &str) -> &'static Spec {
    SPECS
        .iter()
        .find(|s| s.id == id)
        .expect("a storage operation")
}

pub fn operations() -> Vec<Operation> {
    SPECS
        .iter()
        .map(|s| Operation {
            id: s.id.parse().expect("a valid operation id"),
            title: s.title.into(),
            summary: s.summary.into(),
            description: s.description.into(),
            category: Category::Storage,
            kind: OperationKind::Native,
            script: None,
            min_role: Role::Admin,
            mutating: true,
            destructive: s.id != INVENTORY,
            supports_dry_run: false,
            params: vec![
                serde_json::from_value(json!({
                    "id": "host",
                    "label": "Host",
                    "type": "host",
                    "help": "The host whose disks to change.",
                    "required": true,
                }))
                .expect("a valid parameter"),
            ],
            locks: vec!["storage:{host}".into()],
            target: TargetRule::Host {
                param: "host".into(),
            },
            cancel: CancelRule::Phases {
                not_during: vec![POOL_PHASE.0.into()],
            },
            resume: ResumeRule::Checkpoint,
            notes: Some(format!("Replaces v2's {}.", s.replaces)),
        })
        .collect()
}

/// The pool's live layout, read again for every phase.
pub trait Live: Send + Sync {
    fn layout(&self) -> BoxFuture<'_, Result<Layout, String>>;
}

pub struct LiveHost<'a> {
    pub runner: &'a dyn CommandRunner,
    pub pool: PoolName,
}

impl Live for LiveHost<'_> {
    fn layout(&self) -> BoxFuture<'_, Result<Layout, String>> {
        Box::pin(async move {
            layout::collect(self.runner, &LocalHost, &self.pool, false)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// What a storage operation works with; `root` is `/` except in tests.
pub struct Context<'a> {
    pub flow: &'a Workflow,
    pub host: &'a dyn Host,
    pub live: &'a dyn Live,
    pub runner: &'a dyn CommandRunner,
    pub cluster: &'a dyn Cluster,
    pub journal: Journal,
    pub root: PathBuf,
    pub node: HostSlot,
    pub pool: PoolName,
    pub poll: Duration,
}

/// A top-level vdev of any class.
#[derive(Debug, Clone)]
pub struct Top<'s> {
    pub name: &'s str,
    pub class: VdevClass,
    /// `None` for types BMAC doesn't build, such as dRAID.
    pub kind: Option<VdevType>,
    pub state: Option<&'s str>,
    pub removing: bool,
    pub replacing: bool,
    pub holds_esp: bool,
    pub allocated: Option<u64>,
    pub members: &'s [Member],
}

impl Top<'_> {
    pub fn redundant(&self) -> bool {
        matches!(self.kind, Some(k) if k.failures_tolerated(self.members.len()) > 0)
    }

    pub fn describe(&self) -> String {
        format!(
            "{} ({} {})",
            self.name,
            self.class,
            self.kind.map_or("vdev", VdevType::as_str)
        )
    }
}

pub fn kind_of(text: &str) -> Option<VdevType> {
    Some(match text {
        "disk" => VdevType::Single,
        "mirror" => VdevType::Mirror,
        "raidz1" | "raidz" => VdevType::Raidz1,
        "raidz2" => VdevType::Raidz2,
        "raidz3" => VdevType::Raidz3,
        _ => return None,
    })
}

fn class_of(section: &str) -> Option<VdevClass> {
    Some(match section {
        "special" => VdevClass::Special,
        "logs" => VdevClass::Log,
        "cache" => VdevClass::Cache,
        "spares" => VdevClass::Spare,
        _ => return None,
    })
}

pub fn online(member: &Member) -> bool {
    member.state.as_deref() == Some("ONLINE")
}

/// The member's LUKS mapper as crypttab names it: `lsblk -p` gives the
/// layout `/dev/mapper/NAME`, and crypttab and the initramfs list `NAME`.
pub fn mapper_of(member: &Member) -> Option<String> {
    member
        .mapper
        .as_deref()
        .or_else(|| member.path.strip_prefix("/dev/mapper/"))
        .map(|m| m.strip_prefix("/dev/mapper/").unwrap_or(m).to_string())
}

/// `/dev/sda3` for a kernel name, unchanged for a path.
pub fn dev(name: &str) -> String {
    if name.starts_with('/') {
        name.to_string()
    } else {
        format!("/dev/{name}")
    }
}

/// The host's storage at one moment.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub layout: Layout,
    pub disks: Vec<InventoryDisk>,
    pub journal: State,
    pub ashift: u8,
}

impl Snapshot {
    pub fn tops(&self) -> Vec<Top<'_>> {
        let mut tops: Vec<Top<'_>> = self
            .layout
            .vdevs
            .iter()
            .map(|v| Top {
                name: &v.name,
                class: VdevClass::Data,
                kind: kind_of(&v.kind),
                state: v.state.as_deref(),
                removing: v.removing,
                replacing: v.replacing,
                holds_esp: v.holds_esp,
                allocated: v.allocated,
                members: &v.members,
            })
            .collect();
        for (section, vdevs) in &self.layout.auxiliary_vdevs {
            let Some(class) = class_of(section) else {
                continue;
            };
            tops.extend(vdevs.iter().map(|v| Top {
                name: &v.name,
                class,
                kind: kind_of(&v.kind),
                state: v.state.as_deref(),
                removing: v.removing,
                replacing: v.replacing,
                holds_esp: false,
                allocated: None,
                members: &v.members,
            }));
        }
        tops
    }

    pub fn top(&self, name: &str) -> Option<Top<'_>> {
        self.tops().into_iter().find(|t| t.name == name)
    }

    /// The disk `lsblk -p` names `name` (`/dev/sda`), or its kernel name.
    pub fn disk_named(&self, name: &str) -> Option<&InventoryDisk> {
        self.disks.iter().find(|d| d.path == name || d.name == name)
    }

    /// The one installed disk with this identity.
    pub fn disk_with(&self, identity: &DiskIdentity) -> Option<&InventoryDisk> {
        let mut found = self
            .disks
            .iter()
            .filter(|d| d.identity.as_ref() == Some(identity));
        let first = found.next()?;
        found.next().is_none().then_some(first)
    }

    pub fn row(&self, disk: &InventoryDisk) -> Option<&DiskRow> {
        self.layout
            .all_disks
            .iter()
            .find(|r| r.disk == disk.path || r.disk == disk.name)
    }

    /// Whether the disk holds a member of this pool.
    pub fn holds_member(&self, disk: &InventoryDisk) -> bool {
        let members = self.member_disks();
        members.contains(disk.path.as_str()) || members.contains(disk.name.as_str())
    }

    /// The kernel names of every disk holding a member of this pool.
    pub fn member_disks(&self) -> BTreeSet<&str> {
        self.tops()
            .iter()
            .flat_map(|t| t.members.iter())
            .filter_map(|m| m.disk.as_deref())
            .collect()
    }

    pub fn encryption(&self) -> Encryption {
        let luks = self
            .layout
            .vdevs
            .iter()
            .flat_map(|v| &v.members)
            .any(|m| m.luks);
        if luks || self.layout.crypttab.iter().any(|e| e.shared_unlock) {
            Encryption::Luks2
        } else {
            Encryption::None
        }
    }

    /// Disks BMAC may erase for a new member: in no pool, recognized by
    /// identity, with nothing using them, and chosen by no journal record.
    pub fn candidates(&self) -> Vec<Candidate<'_>> {
        let esp_only: BTreeSet<&str> = self
            .layout
            .esp_only_disks
            .iter()
            .map(|r| r.disk.as_str())
            .collect();
        let journaled = self.journaled_disks();
        self.layout
            .unassigned_disks
            .iter()
            .filter(|row| row.in_use_reasons.is_empty() && !esp_only.contains(row.disk.as_str()))
            .filter_map(|row| {
                let disk = self.disk_named(&row.disk)?;
                let identity = disk.identity.clone()?;
                let by_id = by_id(disk)?;
                let usable = disk.problems.is_empty()
                    && !disk.removable
                    && disk.size_bytes >= bmac_storage::partition::MIN_DISK_BYTES
                    && !journaled.contains(&identity)
                    && self.disk_with(&identity).is_some();
                usable.then_some(Candidate {
                    disk,
                    by_id,
                    identity,
                    row,
                })
            })
            .collect()
    }

    fn journaled_disks(&self) -> BTreeSet<DiskIdentity> {
        let j = &self.journal;
        j.additions
            .iter()
            .flat_map(|a| a.members.iter().cloned())
            .chain(j.replacements.keys().cloned())
            .chain(
                j.removal_in_progress()
                    .into_iter()
                    .flat_map(|r| r.members.iter().filter_map(|m| m.identity.clone())),
            )
            .collect()
    }

    /// The live pool as a layout, with `extra` appended; members whose
    /// disk is gone get placeholder names, so only `extra`'s findings are
    /// worth reading.
    pub fn pool_layout(&self, pool: &PoolName, extra: Option<Vdev>) -> PoolLayout {
        let mut missing = 0;
        let mut vdevs = Vec::new();
        for top in self.tops() {
            let Some(kind) = top.kind else { continue };
            let members = top
                .members
                .iter()
                .map(|m| {
                    m.disk
                        .as_deref()
                        .and_then(|d| self.disk_named(d))
                        .and_then(by_id)
                        .unwrap_or_else(|| {
                            missing += 1;
                            format!("bmac-missing-{missing}")
                                .parse()
                                .expect("a placeholder name")
                        })
                })
                .collect();
            let mut vdev = Vdev::new(top.class, kind, members);
            vdev.boot = top.holds_esp;
            vdevs.push(vdev);
        }
        vdevs.extend(extra);
        PoolLayout {
            ashift: self.ashift,
            ..PoolLayout::new(pool.clone(), self.encryption(), vdevs)
        }
    }

    /// The layout checks for `vdev` joining the pool: only what concerns
    /// it, since the pool already is what it is.
    pub fn findings(&self, pool: &PoolName, vdev: Vdev) -> (PoolLayout, Findings) {
        let layout = self.pool_layout(pool, Some(vdev));
        let index = layout.vdevs.len() - 1;
        let new: BTreeSet<&ById> = layout.vdevs[index].members.iter().collect();
        let all = checks::check(&layout, &self.check_disks(), checks::Target::Host);
        let concerns = |f: &checks::Finding| {
            f.vdev == Some(index)
                || f.disk.as_ref().is_some_and(|d| new.contains(d))
                || (f.vdev.is_none() && f.disk.is_none() && f.code != checks::Code::Structure)
        };
        let findings = Findings {
            refused: all
                .refused
                .iter()
                .filter(|f| concerns(f))
                .cloned()
                .collect(),
            warnings: all
                .warnings
                .iter()
                .filter(|f| concerns(f))
                .cloned()
                .collect(),
            needs_force: all.needs_force,
        };
        (layout, findings)
    }

    fn check_disks(&self) -> Vec<checks::Disk> {
        self.disks
            .iter()
            .filter_map(|d| {
                let in_use = if self.holds_member(d) {
                    None
                } else if !d.problems.is_empty() {
                    Some(d.problems.join("; "))
                } else {
                    self.row(d)
                        .filter(|r| !r.in_use_reasons.is_empty())
                        .map(|r| r.in_use_reasons.join("; "))
                };
                Some(checks::Disk {
                    by_id: by_id(d)?,
                    identity: d.identity.clone(),
                    size: d.size_bytes,
                    logical_sector: d.logical_sector_bytes,
                    physical_sector: d.physical_sector_bytes,
                    transport: match d.transport.as_deref() {
                        Some("nvme") => Transport::Nvme,
                        Some("sata" | "ata") => Transport::Sata,
                        Some("sas") => Transport::Sas,
                        Some("usb") => Transport::Usb,
                        _ => Transport::Other,
                    },
                    rotational: d.rotational,
                    model: d.model.clone().unwrap_or_default(),
                    hardware_raid: false,
                    in_use,
                })
            })
            .collect()
    }
}

/// A disk the operator may choose for a new member.
#[derive(Debug, Clone)]
pub struct Candidate<'s> {
    pub disk: &'s InventoryDisk,
    pub by_id: ById,
    pub identity: DiskIdentity,
    pub row: &'s DiskRow,
}

impl Candidate<'_> {
    pub fn describe(&self) -> String {
        describe_disk(self.disk)
    }

    pub fn blank(&self) -> bool {
        self.row.partitions_or_holders == 0 && self.row.fstype.is_none()
    }
}

/// The disk's name for pool layouts.
pub fn by_id(disk: &InventoryDisk) -> Option<ById> {
    disk.by_id.iter().find_map(|n| n.parse().ok())
}

/// "WWN 0x5000c500a1b2c3d4" or "serial S1".
pub fn named(identity: &DiskIdentity) -> String {
    match identity {
        DiskIdentity::Wwn(w) => format!("WWN {w}"),
        DiskIdentity::Serial(s) => format!("serial {s}"),
    }
}

/// "sdc: QEMU HARDDISK, 16.0 GiB, serial S1".
pub fn describe_disk(disk: &InventoryDisk) -> String {
    let id = disk.identity.as_ref().map_or("no serial".into(), named);
    format!(
        "{}: {}, {}, {id}",
        disk.name,
        disk.model.as_deref().unwrap_or("unknown model"),
        Bytes(disk.size_bytes)
    )
}

pub fn field(id: &str, kind: FieldType, label: &str) -> Field {
    Field {
        id: id.into(),
        kind,
        label: label.into(),
        help: None,
        required: true,
        default: None,
        placeholder: None,
        suffix: None,
        min: None,
        max: None,
        min_selected: None,
        max_selected: None,
        pattern: None,
        sensitive: false,
        disabled: false,
        options: Vec::new(),
        filters: Vec::new(),
    }
}

pub fn choice(value: &str, label: &str, help: Option<String>) -> FieldOption {
    FieldOption {
        value: value.into(),
        label: label.into(),
        help,
    }
}

pub fn text(values: &serde_json::Map<String, Value>, id: &str) -> String {
    values
        .get(id)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub fn list(values: &serde_json::Map<String, Value>, id: &str) -> Vec<String> {
    values
        .get(id)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn refuse(field: &str, message: impl Into<String>) -> BTreeMap<String, String> {
    BTreeMap::from([(field.to_string(), message.into())])
}

impl Context<'_> {
    pub fn now(&self) -> i64 {
        self.host.now_ms() / 1000
    }

    pub async fn snapshot(&self) -> Result<Snapshot, Stop> {
        self.flow.check()?;
        let layout = self.live.layout().await.map_err(|e| Stop::Failed {
            message: format!("cannot read {}'s layout", self.pool),
            details: Some(e),
        })?;
        let journal = self.journal.read().map_err(|e| Stop::Failed {
            message: "cannot read the storage journal".into(),
            details: Some(e.to_string()),
        })?;
        let ashift = Zpool::new(self.runner)
            .properties(&self.pool, &["ashift"])
            .await
            .ok()
            .and_then(|p| p.get("ashift").and_then(|v| v.parse().ok()))
            .filter(|a| *a >= bmac_storage::layout::MIN_ASHIFT)
            .unwrap_or(bmac_storage::layout::DEFAULT_ASHIFT);
        Ok(Snapshot {
            layout,
            disks: self.host.disks(),
            journal,
            ashift,
        })
    }

    pub fn update<T>(
        &self,
        change: impl FnOnce(&mut State) -> bmac_storage::journal::Result<T>,
    ) -> Result<T, Stop> {
        self.journal.update(change).map_err(|e| Stop::Failed {
            message: "cannot update the storage journal".into(),
            details: Some(e.to_string()),
        })
    }

    pub fn storage<'s>(&'s self, layout: &'s PoolLayout) -> Storage<'s> {
        Storage {
            flow: self.flow,
            host: self.host,
            runner: self.runner,
            root: self.root.clone(),
            layout,
            define_storages: false,
            poll: self.poll,
        }
    }

    pub fn phase(&self, (id, label): (&str, &str)) {
        self.flow
            .phase_with(id, label, PhaseStatus::Running, id != POOL_PHASE.0);
    }

    pub fn done(&self, (id, label): (&str, &str)) {
        self.flow.phase(id, label, PhaseStatus::Complete);
    }

    pub async fn run(&self, command: Command) -> Result<String, Stop> {
        self.storage_free().run(command).await
    }

    pub async fn code(&self, command: Command, codes: &[i32]) -> Result<i32, Stop> {
        self.storage_free().code(command, codes).await
    }

    /// A [`Storage`] for the commands that need no layout.
    fn storage_free(&self) -> Storage<'_> {
        static EMPTY: std::sync::OnceLock<PoolLayout> = std::sync::OnceLock::new();
        let layout = EMPTY.get_or_init(|| {
            PoolLayout::new(
                "rpool".parse().expect("a pool name"),
                Encryption::None,
                Vec::new(),
            )
        });
        self.storage(layout)
    }

    pub async fn zpool(&self, args: &[&str]) -> Result<String, Stop> {
        self.run(
            Command::new(programs::ZPOOL)
                .args(args.iter().copied())
                .timeout(setup_storage::SLOW),
        )
        .await
    }

    /// The disk with this identity, read again just before it is changed.
    pub fn resolve(&self, identity: &DiskIdentity) -> Result<InventoryDisk, Stop> {
        let disks = self.host.disks();
        let found: Vec<&InventoryDisk> = disks
            .iter()
            .filter(|d| d.identity.as_ref() == Some(identity))
            .collect();
        match found.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(Stop::failed(format!(
                "no disk with {} is installed now",
                named(identity)
            ))),
            _ => Err(Stop::failed(format!(
                "more than one disk reports {}; BMAC won't guess which to change",
                named(identity)
            ))),
        }
    }

    /// Asks for the passphrase at the console, then proves it opens every
    /// online encrypted member, so a new disk never gets a second
    /// passphrase (v2 `obtain_shared_key`).
    pub async fn unlock(&self, snapshot: &Snapshot) -> Result<(), Stop> {
        let storage = self.storage_free();
        storage.wait_for_key().await?;
        let members: Vec<&Member> = snapshot
            .tops()
            .iter()
            .flat_map(|t| t.members.iter())
            .filter(|m| m.luks && online(m))
            .collect();
        for member in members {
            let Some(backing) = member.backing.as_deref() else {
                continue;
            };
            storage.test_passphrase(&dev(backing)).await?;
        }
        self.flow
            .info("The passphrase opens every encrypted member of the pool.");
        Ok(())
    }

    pub fn forget_key(&self) -> Result<(), Stop> {
        self.storage_free().remove_key()
    }

    /// Runs `work` with the passphrase file deleted however it ends, when
    /// the pool is encrypted.
    pub async fn with_key<T>(
        &self,
        snapshot: &Snapshot,
        work: impl std::future::Future<Output = Result<T, Stop>>,
    ) -> Result<T, Stop> {
        if snapshot.encryption() == Encryption::None {
            return work.await;
        }
        let outcome = async {
            self.unlock(snapshot).await?;
            work.await
        }
        .await;
        let removed = self.forget_key();
        let value = outcome?;
        removed?;
        self.flow.info(format!(
            "Deleted {KEY_FILE}; every boot still asks for the passphrase at the console."
        ));
        Ok(value)
    }

    /// Clears a ZFS label if there is one. It fails on a device that has
    /// none, with an exit status that varies by release; wipefs and sgdisk
    /// do the rest (v2 `rpool_mirror.sh` ignores it too).
    async fn labelclear(&self, device: &str) {
        let _ = self
            .code(
                Command::new(programs::ZPOOL).args(["labelclear", "-f", device]),
                &[0],
            )
            .await;
    }

    /// Erases a disk's metadata, never its data (v2
    /// `command_release_disks`): LUKS headers, ZFS labels, signatures, and
    /// the partition table.
    pub async fn wipe_metadata(&self, disk: &InventoryDisk) -> Result<(), Stop> {
        let by_id = by_id(disk)
            .ok_or_else(|| Stop::failed(format!("{} has no /dev/disk/by-id name", disk.name)))?;
        let path = by_id.path();
        for p in disk.partitions.iter().rev() {
            let part = format!("{path}-part{}", p.number);
            if p.luks.is_some() {
                self.run(Command::new(setup_storage::CRYPTSETUP).args([
                    "erase",
                    "--batch-mode",
                    part.as_str(),
                ]))
                .await?;
            }
            self.labelclear(&part).await;
            self.run(Command::new(setup_storage::WIPEFS).args(["--all", "--force", part.as_str()]))
                .await?;
        }
        if disk.luks.is_some() {
            self.run(Command::new(setup_storage::CRYPTSETUP).args([
                "erase",
                "--batch-mode",
                path.as_str(),
            ]))
            .await?;
        }
        self.labelclear(&path).await;
        self.run(Command::new(setup_storage::WIPEFS).args(["--all", "--force", path.as_str()]))
            .await?;
        self.run(Command::new(setup_storage::SGDISK).args(["--zap-all", path.as_str()]))
            .await?;
        self.code(
            Command::new(setup_storage::PARTX).args(["--update", path.as_str()]),
            &[0, 1],
        )
        .await?;
        self.run(
            Command::new(setup_storage::UDEVADM)
                .arg("settle")
                .timeout(Duration::from_secs(120)),
        )
        .await?;
        Ok(())
    }

    /// Erases the disk as a drill would find a new one: metadata, then the
    /// first and last 4 MiB where ZFS keeps its labels, then a discard
    /// where the disk supports it.
    pub async fn erase(&self, disk: &InventoryDisk) -> Result<(), Stop> {
        self.wipe_metadata(disk).await?;
        let path = by_id(disk).map(|b| b.path()).unwrap_or_default();
        zero_edges(
            &self.root.join(path.trim_start_matches('/')),
            disk.size_bytes,
        )
        .map_err(|e| Stop::failed(format!("cannot zero the ends of {path}: {e}")))?;
        self.code(
            Command::new(BLKDISCARD).args(["--force", path.as_str()]),
            &[0, 1],
        )
        .await?;
        Ok(())
    }

    /// Backs up the LUKS headers of `members` ((mapper, partition) pairs).
    pub async fn backup_headers(&self, members: &[(String, String)]) -> Result<(), Stop> {
        let storage = self.storage_free();
        for (mapper, partition) in members {
            storage.backup_header(mapper, partition).await?;
        }
        if !members.is_empty() {
            self.flow.info(format!(
                "Backed up the new LUKS headers in {}. Copy them somewhere other than this host.",
                setup_storage::HEADER_DIR
            ));
        }
        Ok(())
    }

    pub fn delete_header(&self, mapper: &str) {
        let file = format!("{}/{mapper}.luks-header", setup_storage::HEADER_DIR);
        let _ = std::fs::remove_file(self.root.join(file.trim_start_matches('/')));
    }
}

fn zero_edges(path: &std::path::Path, size: u64) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    let zeros = vec![0u8; WIPE_EDGE_BYTES as usize];
    let edge = WIPE_EDGE_BYTES.min(size);
    file.write_all(&zeros[..edge as usize])?;
    if size > WIPE_EDGE_BYTES {
        file.seek(SeekFrom::Start(size - WIPE_EDGE_BYTES))?;
        file.write_all(&zeros)?;
    }
    file.sync_all()
}

fn parse_host(args: &[String]) -> Result<HostSlot, Stop> {
    let [host] = args else {
        return Err(Stop::failed("usage: storage.OPERATION HOST"));
    };
    let host: HostSlot = host
        .parse()
        .map_err(|_| Stop::failed(format!("{host} is not a BMAC host name")))?;
    let me = crate::engine::listen::node_name();
    if host.to_string() != me {
        return Err(Stop::failed(format!(
            "this job is for {host}'s disks, but it is running on {me}"
        )));
    }
    Ok(host)
}

/// The production and staging guests from the cluster registry.
struct RegistryCluster(bmac_registry::Registry);

impl Cluster for RegistryCluster {
    fn guests(&self) -> Result<Vec<GuestFacts>, String> {
        Ok(self
            .0
            .resources()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|r| GuestFacts {
                name: r.name,
                vmid: r.vmid,
                owner: r.owner_node,
                placement: r.placement,
            })
            .collect())
    }

    fn pending_cleanups(&self, node: HostSlot) -> Result<Vec<String>, String> {
        Ok(self
            .0
            .cleanup_records()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|c| c.node == node && c.state == bmac_registry::CleanupState::Pending)
            .map(|c| format!("{} ({})", c.target, c.resource))
            .collect())
    }
}

/// `bmacd native storage.OPERATION --json HOST`.
pub async fn run(operation: &str, flow: &Workflow, args: &[String]) -> ExitCode {
    let spec = spec(operation);
    flow.begin(operation, spec.title, spec.description);
    let outcome = async {
        let node = parse_host(args)?;
        let runner = bmac_exec::RealRunner;
        let pool: PoolName = "rpool".parse().expect("a pool name");
        let live = LiveHost {
            runner: &runner,
            pool: pool.clone(),
        };
        let cluster = RegistryCluster(bmac_registry::Registry::system());
        let cx = Context {
            flow,
            host: &RealHost,
            live: &live,
            runner: &runner,
            cluster: &cluster,
            journal: Journal::system(),
            root: PathBuf::from("/"),
            node,
            pool,
            poll: Duration::from_secs(10),
        };
        dispatch(&cx, operation).await
    }
    .await;
    flow.finish(&outcome)
}

pub async fn dispatch(cx: &Context<'_>, operation: &str) -> Result<(), Stop> {
    match operation {
        ADD_VDEV => add::add_vdev(cx).await,
        ATTACH_DISK => add::attach_disk(cx).await,
        REPLACE_DISK => replace::replace_disk(cx).await,
        REMOVE_VDEV => remove::remove_vdev(cx).await,
        INVENTORY => inventory::inventory(cx).await,
        SIMULATE_FAILURE => simulate::simulate_failure(cx).await,
        other => Err(Stop::failed(format!("{other} is not a storage operation"))),
    }
}

/// The guests a vdev removal must trim and replicate: production guests
/// placed on this host.
pub fn guests_on(guests: &[GuestFacts], node: HostSlot) -> Vec<&GuestFacts> {
    guests
        .iter()
        .filter(|g| g.name.is_production() && g.placement.contains(&node))
        .collect()
}

/// Staging guests whose disks could live on this host.
pub fn staging_on(guests: &[GuestFacts], node: HostSlot) -> Vec<&GuestFacts> {
    guests
        .iter()
        .filter(|g| {
            !g.name.is_production() && (g.owner == Some(node) || g.placement.contains(&node))
        })
        .collect()
}
