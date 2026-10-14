// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The setup job's storage phase (`STORAGE.md`): encrypt the installed boot
//! vdev in place when the layout asks for LUKS, create the other vdevs,
//! prove the single-prompt unlock, and add the per-block-size datasets.
//!
//! Every step is chosen from the host's live state (the pool's config, each
//! disk's partitions and LUKS header, the open mappers), never from what an
//! earlier run said it did, so a job resumed after any interruption carries
//! on from wherever the disks are. The choosing is pure ([`boot_step`],
//! [`vdev_step`]); [`Storage`] gathers the facts and runs one step at a time.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bmac_adapters::programs;
use bmac_adapters::zpool::{ConfigNode, PoolStatus, Zpool};
use bmac_core::job::Instruction;
use bmac_exec::{Command, CommandRunner};
use bmac_proto::inventory::InventoryDisk;
use bmac_storage::geometry::BlockSize;
use bmac_storage::luks::{self, Entry};
use bmac_storage::partition::{FIRST_PARTITION_START_BYTES, member_end_bytes};
use bmac_storage::{ById, Encryption, PoolLayout, Vdev, VdevClass, VdevType};
use serde_json::{Value, json};

use super::setup::Host;

mod drills;
use super::{Stop, Workflow};

/// Where `bmac luks-enter` leaves the passphrase (`bmac-cli`'s
/// `PASSPHRASE_FILE`).
pub const KEY_FILE: &str = "/run/bmac/luks-passphrase";
pub const HEADER_DIR: &str = "/var/lib/bmac/luks-headers";
pub(crate) const CRYPTTAB: &str = "/etc/crypttab";
const STORAGE_CFG: &str = "/etc/pve/storage.cfg";
const DECRYPT_KEYCTL: &str = "/usr/lib/cryptsetup/scripts/decrypt_keyctl";
const KEYCTL: &str = "/usr/bin/keyctl";
const OSRELEASE: &str = "/proc/sys/kernel/osrelease";
/// The partition the Proxmox installer puts the pool on.
pub(crate) const BOOT_PARTITION: u32 = 3;
const GIB: u64 = bmac_core::Bytes::GIB;
/// GPT type codes: Linux LUKS, and Solaris /usr (ZFS).
pub(crate) const TYPE_LUKS: &str = "8309";
pub(crate) const TYPE_ZFS: &str = "BF01";
pub const PACKAGES: &[&str] = &["cryptsetup", "cryptsetup-initramfs", "keyutils", "gdisk"];

pub(crate) const CRYPTSETUP: &str = "/usr/sbin/cryptsetup";
pub(crate) const SGDISK: &str = "/usr/sbin/sgdisk";
pub(crate) const PARTX: &str = "/usr/bin/partx";
pub(crate) const UDEVADM: &str = "/usr/bin/udevadm";
pub(crate) const WIPEFS: &str = "/usr/sbin/wipefs";
pub(crate) const UPDATE_INITRAMFS: &str = "/usr/sbin/update-initramfs";
pub(crate) const UNMKINITRAMFS: &str = "/usr/bin/unmkinitramfs";
pub(crate) const PROXMOX_BOOT_TOOL: &str = "/usr/sbin/proxmox-boot-tool";
const PVESM: &str = "/usr/sbin/pvesm";
pub(crate) const SLOW: Duration = Duration::from_secs(15 * 60);

/// Where a boot member is in the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// Still the installer's ZFS partition, under the name the pool uses.
    Raw {
        leaf: String,
        online: bool,
    },
    /// Already its LUKS mapper.
    Mapper {
        online: bool,
    },
    /// Half of a `replacing-N` grouping: the resilver is under way.
    Replacing,
    Missing,
}

/// Where the boot partition ends against where it should.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Short,
    Exact,
    Past,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootFacts {
    pub place: Place,
    /// Every other member of the boot vdev is online.
    pub others_online: bool,
    pub resilvering: bool,
    /// The boot partition holds a LUKS header.
    pub luks: bool,
    /// The mapper is open on the boot partition.
    pub open: bool,
    pub end: End,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootStep {
    Done,
    Wait,
    Offline(String),
    /// Move the partition's end out, leaving its start.
    Grow,
    Format,
    Open,
    /// Record the mapper for boot, prove the initramfs unlocks it, then
    /// `zpool replace` the old partition with it.
    Replace(String),
    Refuse(String),
}

/// The next step for one boot member, converted in place
/// (`STORAGE.md`, "Encrypting the boot vdev in place").
pub fn boot_step(f: &BootFacts) -> BootStep {
    match &f.place {
        Place::Replacing => BootStep::Wait,
        Place::Mapper { online: true } if f.resilvering => BootStep::Wait,
        Place::Mapper { online: true } => BootStep::Done,
        Place::Mapper { online: false } => {
            BootStep::Refuse("is in the pool as its LUKS device but isn't online".into())
        }
        Place::Missing => BootStep::Refuse(
            "isn't in the pool, raw or encrypted; put it back by hand and resume".into(),
        ),
        Place::Raw { .. } if f.resilvering => BootStep::Wait,
        Place::Raw { leaf, online: true } => {
            if f.others_online {
                BootStep::Offline(leaf.clone())
            } else {
                BootStep::Refuse(
                    "can't be taken offline: another member of the boot vdev isn't online, \
                     so the pool would have no copy left"
                        .into(),
                )
            }
        }
        Place::Raw {
            leaf,
            online: false,
        } => {
            if !f.luks {
                match f.end {
                    End::Short => BootStep::Grow,
                    End::Exact => BootStep::Format,
                    End::Past => BootStep::Refuse(
                        "has a boot partition that ends past where this layout puts it".into(),
                    ),
                }
            } else if !f.open {
                BootStep::Open
            } else if !f.others_online {
                BootStep::Refuse(
                    "is offline, and so is another member of the boot vdev; bring that one \
                     back first"
                        .into(),
                )
            } else {
                BootStep::Replace(leaf.clone())
            }
        }
    }
}

/// One member of a vdev the layout adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFacts {
    pub in_pool: bool,
    /// The disk has exactly this layout's member partition.
    pub partitioned: bool,
    pub luks: bool,
    pub open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VdevStep {
    Done,
    /// Erase the disk and give it its one member partition.
    Partition(usize),
    Format(usize),
    Open(usize),
    /// Record the mappers for boot and prove the initramfs unlocks them,
    /// then `zpool add`.
    Add,
    Refuse(String),
}

/// The next step for a vdev the installer didn't create. `zpool add` puts
/// all its members in at once, so they are all in or all out.
pub fn vdev_step(encrypted: bool, members: &[MemberFacts]) -> VdevStep {
    let in_pool = members.iter().filter(|m| m.in_pool).count();
    if in_pool == members.len() {
        return VdevStep::Done;
    }
    if in_pool > 0 {
        return VdevStep::Refuse(
            "has some of its disks in the pool and not others; the pool was changed by hand".into(),
        );
    }
    for (i, m) in members.iter().enumerate() {
        if !m.partitioned || (!encrypted && m.luks) {
            return VdevStep::Partition(i);
        }
        if encrypted && !m.luks {
            return VdevStep::Format(i);
        }
        if encrypted && !m.open {
            return VdevStep::Open(i);
        }
    }
    VdevStep::Add
}

/// How ZFS compares redundancy across top-level vdevs.
fn replication(vdev: &Vdev) -> (VdevType, usize) {
    match vdev.kind {
        VdevType::Mirror => (vdev.kind, vdev.members.len()),
        kind => (kind, 0),
    }
}

/// `zpool add` refuses a vdev whose redundancy differs from the pool's
/// without `-f`; the Deployer's layout review already had that
/// acknowledged.
pub fn needs_force(layout: &PoolLayout, index: usize) -> bool {
    let vdev = &layout.vdevs[index];
    if !matches!(vdev.class, VdevClass::Data | VdevClass::Special) {
        return false;
    }
    layout.vdevs[..index]
        .iter()
        .filter(|v| v.class == VdevClass::Data)
        .any(|v| replication(v) != replication(vdev))
}

/// The `zpool add` arguments for vdev `index`, given its member devices.
pub fn add_args(layout: &PoolLayout, index: usize, devices: &[String]) -> Vec<String> {
    let vdev = &layout.vdevs[index];
    let mut args = vec![
        "add".to_string(),
        "-o".into(),
        format!("ashift={}", layout.ashift),
    ];
    if needs_force(layout, index) {
        args.push("-f".into());
    }
    args.push(layout.pool.to_string());
    args.extend(vdev.class.zpool_keyword().map(str::to_string));
    args.extend(vdev.kind.zpool_keyword().map(str::to_string));
    args.extend(devices.iter().cloned());
    args
}

/// Whether an unpacked initramfs unlocks every one of `mappers` at boot
/// with the single prompt. `Err` names what's missing.
pub fn check_unpacked(dir: &Path, mappers: &[String]) -> Result<(), String> {
    fn find(dir: &Path, wanted: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(at) = stack.pop() {
            for entry in std::fs::read_dir(&at).ok()?.flatten() {
                let path = entry.path();
                let kind = entry.file_type().ok()?;
                if kind.is_dir() {
                    stack.push(path);
                } else if kind.is_file() && wanted(&path) {
                    return Some(path);
                }
            }
        }
        None
    }
    let crypttab = find(dir, &|p| p.ends_with("cryptroot/crypttab"))
        .ok_or("the initramfs has no cryptroot/crypttab")?;
    let text = std::fs::read_to_string(&crypttab).map_err(|e| e.to_string())?;
    let listed = luks::initramfs_names(&text);
    let missing: Vec<&str> = mappers
        .iter()
        .filter(|m| !listed.contains(m))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "the initramfs wouldn't unlock {} at boot",
            missing.join(", ")
        ));
    }
    find(dir, &|p| {
        p.file_name().is_some_and(|n| n == "decrypt_keyctl")
    })
    .ok_or("the initramfs lacks decrypt_keyctl, so each disk would ask separately")?;
    Ok(())
}

/// A vdev's members, ready to join the pool or in it already.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prepared {
    pub in_pool: bool,
    pub encrypted: bool,
    /// Each member's mapper name and partition.
    pub members: Vec<(String, String)>,
}

impl Prepared {
    /// The devices the pool takes: the mappers, or the partitions.
    pub fn devices(&self) -> Vec<String> {
        self.members
            .iter()
            .map(|(m, p)| {
                if self.encrypted {
                    mapper_path(m)
                } else {
                    p.clone()
                }
            })
            .collect()
    }

    pub fn mappers(&self) -> Vec<String> {
        if self.encrypted {
            self.members.iter().map(|(m, _)| m.clone()).collect()
        } else {
            Vec::new()
        }
    }
}

/// Whether an unpacked initramfs leaves every one of `mappers` alone at
/// boot. One with no `cryptroot/crypttab` unlocks nothing.
pub fn check_absent(dir: &Path, mappers: &[String]) -> Result<(), String> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() && path.ends_with("cryptroot/crypttab") {
                let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                let listed = luks::initramfs_names(&text);
                let still: Vec<&str> = mappers
                    .iter()
                    .filter(|m| listed.contains(m))
                    .map(String::as_str)
                    .collect();
                if !still.is_empty() {
                    return Err(format!(
                        "the initramfs still unlocks {} at boot",
                        still.join(", ")
                    ));
                }
            }
        }
    }
    Ok(())
}

/// What the storage phase works with on the host; `root` is `/` except in
/// tests.
pub struct Storage<'a> {
    pub flow: &'a Workflow,
    pub host: &'a dyn Host,
    pub runner: &'a dyn CommandRunner,
    pub root: PathBuf,
    pub layout: &'a PoolLayout,
    /// The first host defines the cluster's storages; a joining host gets
    /// them from the cluster.
    pub define_storages: bool,
    pub poll: Duration,
}

pub(crate) fn part(member: &ById, number: u32) -> String {
    format!("{}-part{number}", member.path())
}

/// Where partition `number` starts on `disk`, as the inventory read it.
pub(crate) fn start_of(disk: &InventoryDisk, number: u32) -> u64 {
    disk.partitions
        .iter()
        .find(|p| p.number == number)
        .map_or(0, |p| p.start_bytes)
}

pub(crate) fn mapper_path(name: &str) -> String {
    format!("/dev/mapper/{name}")
}

pub(crate) fn fail(what: &str, e: impl std::fmt::Display) -> Stop {
    Stop::Failed {
        message: format!("{what} failed"),
        details: Some(e.to_string()),
    }
}

impl Storage<'_> {
    pub(crate) fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    /// Whether two device paths name the same device, following symlinks.
    pub(crate) fn same(&self, a: &str, b: &str) -> bool {
        match (
            std::fs::canonicalize(self.at(a)),
            std::fs::canonicalize(self.at(b)),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => a == b,
        }
    }

    pub(crate) async fn run(&self, command: Command) -> Result<String, Stop> {
        let shown = command.to_string();
        let output = self
            .runner
            .run(command)
            .await
            .map_err(|e| fail(&shown, e))?;
        Ok(output.stdout_text())
    }

    pub(crate) async fn code(&self, command: Command, codes: &[i32]) -> Result<i32, Stop> {
        let shown = command.to_string();
        let output = self
            .runner
            .run(command.expect_codes(codes))
            .await
            .map_err(|e| fail(&shown, e))?;
        Ok(output.code().unwrap_or(-1))
    }

    pub(crate) async fn status(&self) -> Result<PoolStatus, Stop> {
        Zpool::new(self.runner)
            .status(&self.layout.pool)
            .await
            .map_err(|e| fail("zpool status", e))
    }

    pub(crate) fn disk<'d>(
        &self,
        disks: &'d [InventoryDisk],
        member: &ById,
    ) -> Result<&'d InventoryDisk, Stop> {
        disks
            .iter()
            .find(|d| d.by_id.iter().any(|n| n == member.as_str()))
            .ok_or_else(|| Stop::failed(format!("{} is no longer on this host", member.as_str())))
    }

    pub(crate) fn mapper(&self, disk: &InventoryDisk) -> Result<String, Stop> {
        disk.identity
            .as_ref()
            .map(|id| luks::mapper_name(&self.layout.pool, id))
            .ok_or_else(|| Stop::failed(format!("{} has no serial number or WWN", disk.name)))
    }

    pub(crate) async fn open_on(&self, mapper: &str, partition: &str) -> Result<bool, Stop> {
        if !self.at(&mapper_path(mapper)).exists() {
            return Ok(false);
        }
        let text = self
            .run(Command::new(CRYPTSETUP).args(["status", mapper]))
            .await?;
        let backing = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("device:"))
            .map(str::trim)
            .unwrap_or_default();
        if !self.same(backing, partition) {
            return Err(Stop::failed(format!(
                "{mapper} is open on {backing}, not on {partition}"
            )));
        }
        Ok(true)
    }

    pub(crate) async fn is_luks(&self, partition: &str) -> Result<bool, Stop> {
        let code = self
            .code(
                Command::new(CRYPTSETUP).args(["isLuks", partition]),
                &[0, 1],
            )
            .await?;
        Ok(code == 0)
    }

    pub(crate) async fn test_passphrase(&self, partition: &str) -> Result<(), Stop> {
        let code = self
            .code(
                Command::new(CRYPTSETUP).args([
                    "open",
                    "--test-passphrase",
                    &format!("--key-file={KEY_FILE}"),
                    partition,
                ]),
                &[0, 2],
            )
            .await?;
        if code != 0 {
            return Err(Stop::failed(format!(
                "The passphrase doesn't open the LUKS on {partition}. Every disk takes the \
                 passphrase this host's first disk was encrypted with; run bmac luks-enter \
                 again with that one and resume."
            )));
        }
        Ok(())
    }

    // ---- the passphrase ----

    pub(crate) fn key_ready(&self) -> Result<bool, Stop> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let path = self.at(KEY_FILE);
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Stop::failed(format!(
                "cannot check {}: {e}",
                path.display()
            ))),
            Ok(meta) => {
                let private = meta.is_file()
                    && meta.len() > 0
                    && meta.permissions().mode() & 0o7777 == 0o600
                    && meta.uid() == rustix::process::geteuid().as_raw();
                if private {
                    Ok(true)
                } else {
                    Err(Stop::failed(format!(
                        "{KEY_FILE} isn't a private file written by bmac luks-enter; delete it \
                         and run bmac luks-enter again"
                    )))
                }
            }
        }
    }

    pub(crate) async fn wait_for_key(&self) -> Result<(), Stop> {
        while !self.key_ready()? {
            let host = crate::engine::listen::node_name();
            self.flow
                .manual(
                    "Type the LUKS passphrase at the console",
                    vec![
                        Instruction::Text(format!(
                            "Open {host}'s console: iDRAC's virtual console, another remote \
                             console, or a keyboard and screen. Not SSH."
                        )),
                        Instruction::WithCopy {
                            text: "Log in as root and run:".into(),
                            copy: "bmac luks-enter".into(),
                            run_on: Some(host),
                        },
                        Instruction::Text(
                            "Type the passphrase twice. Keep it safe: every boot of this host \
                             waits for it at the console, and nothing else can unlock the \
                             disks. BMAC deletes its copy when the storage is built."
                                .into(),
                        ),
                    ],
                    Some("I entered the passphrase"),
                )
                .await?;
            if !self.key_ready()? {
                self.flow.warning(format!(
                    "{KEY_FILE} isn't there yet; run bmac luks-enter first."
                ));
            }
        }
        Ok(())
    }

    /// Deletes the passphrase file and checks it is gone.
    pub fn remove_key(&self) -> Result<(), Stop> {
        let path = self.at(KEY_FILE);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Stop::failed(format!("cannot delete {KEY_FILE}: {e}"))),
        }
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(Stop::failed(format!("{KEY_FILE} is still there")));
        }
        Ok(())
    }

    // ---- boot unlock ----

    pub(crate) fn read_crypttab(&self) -> Result<String, Stop> {
        match std::fs::read_to_string(self.at(CRYPTTAB)) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(Stop::failed(format!("cannot read {CRYPTTAB}: {e}"))),
        }
    }

    pub(crate) async fn entry(&self, mapper: &str, partition: &str) -> Result<Entry, Stop> {
        let uuid = self
            .run(Command::new(CRYPTSETUP).args(["luksUUID", partition]))
            .await?
            .trim()
            .to_string();
        if uuid.is_empty() || !uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Err(Stop::failed(format!(
                "{partition} has no readable LUKS UUID"
            )));
        }
        Ok(Entry {
            name: mapper.into(),
            luks_uuid: uuid,
        })
    }

    async fn unpack_with(&self, check: impl Fn(&Path) -> Result<(), String>) -> Result<(), Stop> {
        let release = std::fs::read_to_string(self.at(OSRELEASE))
            .map_err(|e| Stop::failed(format!("cannot read {OSRELEASE}: {e}")))?;
        let initrd = format!("/boot/initrd.img-{}", release.trim());
        let into = self.flow.work_dir().join("initramfs");
        let _ = std::fs::remove_dir_all(&into);
        self.run(
            Command::new(UNMKINITRAMFS)
                .args([initrd.as_str(), &into.to_string_lossy()])
                .timeout(SLOW),
        )
        .await?;
        let checked = check(&into);
        let _ = std::fs::remove_dir_all(&into);
        checked.map_err(|why| Stop::failed(format!("{why} ({initrd})")))
    }

    pub(crate) async fn unpack_and_check(&self, mappers: &[String]) -> Result<(), Stop> {
        self.unpack_with(|dir| check_unpacked(dir, mappers)).await
    }

    /// Takes retired mappers out of crypttab, rebuilds the initramfs, and
    /// proves it no longer waits for them at boot. The rebuild runs even
    /// when crypttab had no line left to drop, so an interrupted earlier
    /// run is finished.
    pub(crate) async fn unregister(&self, mappers: &[String]) -> Result<(), Stop> {
        let text = self.read_crypttab()?;
        let updated = luks::without_entries(&text, mappers);
        if updated != text {
            super::write_atomic(&self.at(CRYPTTAB), updated.as_bytes())
                .map_err(|e| Stop::failed(format!("cannot write {CRYPTTAB}: {e}")))?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(self.at(CRYPTTAB), std::fs::Permissions::from_mode(0o600))
                .map_err(|e| Stop::failed(format!("cannot restrict {CRYPTTAB}: {e}")))?;
        }
        self.flow.info("Rebuilding the initramfs.");
        self.run(
            Command::new(UPDATE_INITRAMFS)
                .args(["-u", "-k", "all"])
                .timeout(SLOW),
        )
        .await?;
        self.run(Command::new(PROXMOX_BOOT_TOOL).arg("refresh").timeout(SLOW))
            .await?;
        self.unpack_with(|dir| check_absent(dir, mappers)).await?;
        self.flow.info(format!(
            "The initramfs no longer unlocks {}.",
            mappers.join(", ")
        ));
        Ok(())
    }

    /// Lists the mappers in crypttab in the shared form, rebuilds the
    /// initramfs, and proves it unlocks them, before the pool depends on
    /// them.
    pub(crate) async fn register(&self, mappers: &[(String, String)]) -> Result<(), Stop> {
        let mut entries = Vec::new();
        for (mapper, partition) in mappers {
            entries.push(self.entry(mapper, partition).await?);
        }
        let text = self.read_crypttab()?;
        let updated = luks::with_entries(&text, &self.layout.pool, &entries);
        if updated != text {
            let path = self.at(CRYPTTAB);
            super::write_atomic(&path, updated.as_bytes())
                .map_err(|e| Stop::failed(format!("cannot write {CRYPTTAB}: {e}")))?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| Stop::failed(format!("cannot restrict {CRYPTTAB}: {e}")))?;
        }
        self.flow.info("Rebuilding the initramfs.");
        self.run(
            Command::new(UPDATE_INITRAMFS)
                .args(["-u", "-k", "all"])
                .timeout(SLOW),
        )
        .await?;
        self.run(Command::new(PROXMOX_BOOT_TOOL).arg("refresh").timeout(SLOW))
            .await?;
        let names: Vec<String> = mappers.iter().map(|(m, _)| m.clone()).collect();
        self.unpack_and_check(&names).await?;
        self.flow.info(format!(
            "The initramfs unlocks {} with the one console passphrase.",
            names.join(", ")
        ));
        Ok(())
    }

    // ---- the boot vdev ----

    fn boot_vdev(&self) -> Result<&Vdev, Stop> {
        self.layout
            .vdevs
            .iter()
            .find(|v| v.boot)
            .ok_or_else(|| Stop::failed("the layout has no boot vdev"))
    }

    /// The pool's top-level vdev holding `device`, and the leaf for it.
    pub(crate) fn find<'s>(
        &self,
        status: &'s PoolStatus,
        device: &str,
    ) -> Option<(&'s ConfigNode, &'s ConfigNode)> {
        status
            .sections
            .iter()
            .flat_map(|s| &s.vdevs)
            .find_map(|top| {
                top.leaves()
                    .into_iter()
                    .find(|leaf| self.same(&leaf.name, device))
                    .map(|leaf| (top, leaf))
            })
    }

    fn reference_end(&self, disks: &[InventoryDisk]) -> Result<u64, Stop> {
        let mut smallest = u64::MAX;
        for member in &self.boot_vdev()?.members {
            smallest = smallest.min(self.disk(disks, member)?.size_bytes);
        }
        Ok(smallest / GIB * GIB - GIB)
    }

    async fn boot_facts(
        &self,
        member: &ById,
        disks: &[InventoryDisk],
    ) -> Result<(BootFacts, String, u64), Stop> {
        let disk = self.disk(disks, member)?;
        let mapper = self.mapper(disk)?;
        let partition = part(member, BOOT_PARTITION);
        let status = self.status().await?;
        let resilvering = status
            .scan()
            .is_some_and(|s| s.contains("resilver in progress"));
        let online = |n: &ConfigNode| n.state.as_deref() == Some("ONLINE");
        let encrypted = self.find(&status, &mapper_path(&mapper));
        let raw = self.find(&status, &partition);
        let top = encrypted.or(raw).map(|(top, _)| top);
        let place = match (encrypted, raw) {
            _ if top.is_some_and(ConfigNode::replacing) => Place::Replacing,
            (Some((_, leaf)), _) => Place::Mapper {
                online: online(leaf),
            },
            (None, Some((_, leaf))) => Place::Raw {
                leaf: leaf.name.clone(),
                online: online(leaf),
            },
            (None, None) => Place::Missing,
        };
        let others_online = top.is_some_and(|top| {
            top.leaves()
                .into_iter()
                .filter(|l| {
                    !self.same(&l.name, &partition) && !self.same(&l.name, &mapper_path(&mapper))
                })
                .all(online)
        });
        let sector = u64::from(disk.logical_sector_bytes.max(512));
        let reference = (self.reference_end(disks)? / sector) * sector;
        let current = disk
            .partitions
            .iter()
            .find(|p| p.number == BOOT_PARTITION)
            .ok_or_else(|| {
                Stop::failed(format!(
                    "{} has no partition {BOOT_PARTITION}; it isn't laid out as the installer \
                     does",
                    member.as_str()
                ))
            })?;
        let end_bytes = current.start_bytes + current.size_bytes;
        let end = match end_bytes.cmp(&reference) {
            std::cmp::Ordering::Less => End::Short,
            std::cmp::Ordering::Equal => End::Exact,
            std::cmp::Ordering::Greater => End::Past,
        };
        let luks =
            matches!(place, Place::Raw { online: false, .. }) && self.is_luks(&partition).await?;
        let open = luks && self.open_on(&mapper, &partition).await?;
        let facts = BootFacts {
            place,
            others_online,
            resilvering,
            luks,
            open,
            end,
        };
        Ok((facts, mapper, current.start_bytes / sector))
    }

    async fn grow(
        &self,
        member: &ById,
        disk: &InventoryDisk,
        mapper: &str,
        first: u64,
        end: u64,
    ) -> Result<(), Stop> {
        let sector = u64::from(disk.logical_sector_bytes.max(512));
        let backup = format!("{HEADER_DIR}/{mapper}.pre-resize.gpt");
        if !self.at(&backup).exists() {
            self.run(Command::new(SGDISK).args([format!("--backup={backup}"), member.path()]))
                .await?;
        }
        let n = BOOT_PARTITION;
        let last = end / sector - 1;
        self.run(Command::new(SGDISK).args([
            format!("--delete={n}"),
            format!("--new={n}:{first}:{last}"),
            format!("--typecode={n}:{TYPE_LUKS}"),
            member.path(),
        ]))
        .await?;
        self.settle(member, Some(n)).await
    }

    pub(crate) async fn settle(&self, member: &ById, number: Option<u32>) -> Result<(), Stop> {
        let mut partx = Command::new(PARTX).arg("--update");
        if let Some(n) = number {
            partx = partx.args(["--nr".to_string(), n.to_string()]);
        }
        self.code(partx.arg(member.path()), &[0, 1]).await?;
        self.run(
            Command::new(UDEVADM)
                .arg("settle")
                .timeout(Duration::from_secs(120)),
        )
        .await?;
        Ok(())
    }

    /// Zeroes the ZFS label slots of a partition about to be encrypted, at
    /// its size now. Growing a boot member's partition would otherwise
    /// strand its end copies inside it, outside what the mapper writes:
    /// readable, and found by a later Proxmox install as a second pool
    /// named rpool. `zpool labelclear` refuses a member of an imported pool.
    fn clear_labels(&self, partition: &str, start: u64) -> Result<(), Stop> {
        bmac_inventory::zfs::zero_labels(&self.at(partition), start)
            .map_err(|e| fail(&format!("clearing the old ZFS labels on {partition}"), e))
    }

    pub(crate) async fn format(&self, partition: &str, start: u64) -> Result<(), Stop> {
        self.clear_labels(partition, start)?;
        self.run(Command::new(WIPEFS).args(["--all", "--force", partition]))
            .await?;
        self.run(
            Command::new(CRYPTSETUP)
                .args([
                    "luksFormat",
                    "--batch-mode",
                    "--type",
                    "luks2",
                    &format!("--key-file={KEY_FILE}"),
                    partition,
                ])
                .timeout(SLOW),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn open(&self, partition: &str, mapper: &str) -> Result<(), Stop> {
        self.test_passphrase(partition).await?;
        self.run(Command::new(CRYPTSETUP).args([
            "open",
            &format!("--key-file={KEY_FILE}"),
            partition,
            mapper,
        ]))
        .await?;
        if !self.open_on(mapper, partition).await? {
            return Err(Stop::failed(format!(
                "{mapper} didn't appear after opening it"
            )));
        }
        Ok(())
    }

    async fn encrypt_boot_member(&self, member: &ById) -> Result<String, Stop> {
        let mut waited = 0u32;
        loop {
            let disks = self.host.disks();
            let reference = self.reference_end(&disks)?;
            let (facts, mapper, first) = self.boot_facts(member, &disks).await?;
            let partition = part(member, BOOT_PARTITION);
            let step = boot_step(&facts);
            match step {
                BootStep::Done => return Ok(mapper),
                BootStep::Wait => {
                    if waited.is_multiple_of(20) {
                        let scan = self.status().await?.scan().unwrap_or_default();
                        self.flow.info(format!("Resilvering onto {mapper}: {scan}"));
                    }
                    waited += 1;
                    tokio::time::sleep(self.poll).await;
                }
                BootStep::Offline(leaf) => {
                    self.flow
                        .info(format!("Taking {} offline to encrypt it.", member.as_str()));
                    let pool = self.layout.pool.to_string();
                    self.run(Command::new(programs::ZPOOL).args(["offline", pool.as_str(), &leaf]))
                        .await?;
                }
                BootStep::Grow => {
                    let disk = self.disk(&disks, member)?;
                    self.flow.info(format!(
                        "Growing partition {BOOT_PARTITION} of {} to leave room for the LUKS header.",
                        member.as_str()
                    ));
                    self.clear_labels(&partition, start_of(disk, BOOT_PARTITION))?;
                    self.grow(member, disk, &mapper, first, reference).await?;
                }
                BootStep::Format => {
                    self.flow.info(format!("Encrypting {partition}."));
                    let start = start_of(self.disk(&disks, member)?, BOOT_PARTITION);
                    self.format(&partition, start).await?;
                }
                BootStep::Open => self.open(&partition, &mapper).await?,
                BootStep::Replace(leaf) => {
                    self.register(&[(mapper.clone(), partition.clone())])
                        .await?;
                    self.flow.info(format!("Replacing {leaf} with {mapper}."));
                    let pool = self.layout.pool.to_string();
                    let device = mapper_path(&mapper);
                    self.run(Command::new(programs::ZPOOL).args([
                        "replace",
                        pool.as_str(),
                        &leaf,
                        &device,
                    ]))
                    .await?;
                }
                BootStep::Refuse(why) => {
                    return Err(Stop::failed(format!("{} {why}.", member.as_str())));
                }
            }
        }
    }

    // ---- other vdevs ----

    async fn member_facts(
        &self,
        vdev: &Vdev,
        disks: &[InventoryDisk],
        status: &PoolStatus,
    ) -> Result<Vec<(MemberFacts, String, String)>, Stop> {
        let sizes: Vec<u64> = vdev
            .members
            .iter()
            .map(|m| self.disk(disks, m).map(|d| d.size_bytes))
            .collect::<Result<_, _>>()?;
        let end = member_end_bytes(&sizes).map_err(|e| Stop::failed(e.to_string()))?;
        let mut out = Vec::new();
        for member in &vdev.members {
            let disk = self.disk(disks, member)?;
            let mapper = self.mapper(disk)?;
            let partition = part(member, 1);
            let present = |path: &str| {
                self.find(status, path).is_some_and(|(_, leaf)| {
                    !matches!(
                        leaf.state.as_deref(),
                        Some("OFFLINE" | "UNAVAIL" | "FAULTED" | "REMOVED")
                    )
                })
            };
            let in_pool = present(&partition) || present(&mapper_path(&mapper));
            let partitioned = matches!(disk.partitions.as_slice(),
                [p] if p.number == 1
                    && p.start_bytes == FIRST_PARTITION_START_BYTES
                    && p.start_bytes + p.size_bytes == end);
            // An unencrypted host may not have cryptsetup installed.
            let luks = partitioned
                && !in_pool
                && match self.layout.encryption {
                    Encryption::Luks2 => self.is_luks(&partition).await?,
                    Encryption::None => disk.partitions.iter().any(|p| p.luks.is_some()),
                };
            let open = luks && self.open_on(&mapper, &partition).await?;
            out.push((
                MemberFacts {
                    in_pool,
                    partitioned,
                    luks,
                    open,
                },
                mapper,
                partition,
            ));
        }
        Ok(out)
    }

    pub(crate) async fn partition(
        &self,
        vdev: &Vdev,
        member: &ById,
        disks: &[InventoryDisk],
        encrypted: bool,
    ) -> Result<(), Stop> {
        let disk = self.disk(disks, member)?;
        let sizes: Vec<u64> = vdev
            .members
            .iter()
            .map(|m| self.disk(disks, m).map(|d| d.size_bytes))
            .collect::<Result<_, _>>()?;
        let sector = u64::from(disk.logical_sector_bytes.max(512));
        let (first, last) = bmac_storage::partition::member_partition(&sizes, sector)
            .map_err(|e| Stop::failed(e.to_string()))?;
        self.flow.info(format!("Partitioning {}.", member.as_str()));
        for p in &disk.partitions {
            self.run(Command::new(WIPEFS).args(["--all", "--force", p.path.as_str()]))
                .await?;
        }
        self.run(Command::new(WIPEFS).args(["--all".to_string(), "--force".into(), member.path()]))
            .await?;
        self.run(Command::new(SGDISK).args(["--zap-all".to_string(), member.path()]))
            .await?;
        let typecode = if encrypted { TYPE_LUKS } else { TYPE_ZFS };
        self.run(Command::new(SGDISK).args([
            format!("--new=1:{first}:{last}"),
            format!("--typecode=1:{typecode}"),
            member.path(),
        ]))
        .await?;
        self.settle(member, None).await?;
        self.run(Command::new(WIPEFS).args([
            "--all".to_string(),
            "--force".into(),
            part(member, 1),
        ]))
        .await?;
        Ok(())
    }

    /// Partitions, encrypts, and opens vdev `index`'s members, one step at a
    /// time from what the disks show, until they are ready to join the pool
    /// or are in it already.
    pub(crate) async fn prepare(&self, index: usize) -> Result<Prepared, Stop> {
        let vdev = &self.layout.vdevs[index];
        let encrypted = self.layout.encryption == Encryption::Luks2;
        loop {
            self.flow.check()?;
            let disks = self.host.disks();
            let status = self.status().await?;
            let members = self.member_facts(vdev, &disks, &status).await?;
            let facts: Vec<MemberFacts> = members.iter().map(|(f, _, _)| f.clone()).collect();
            let prepared = |in_pool| Prepared {
                in_pool,
                encrypted,
                members: members
                    .iter()
                    .map(|(_, m, p)| (m.clone(), p.clone()))
                    .collect(),
            };
            match vdev_step(encrypted, &facts) {
                VdevStep::Done => return Ok(prepared(true)),
                VdevStep::Add => return Ok(prepared(false)),
                VdevStep::Partition(i) => {
                    self.partition(vdev, &vdev.members[i], &disks, encrypted)
                        .await?;
                }
                VdevStep::Format(i) => {
                    self.flow.info(format!("Encrypting {}.", members[i].2));
                    self.format(&members[i].2, FIRST_PARTITION_START_BYTES)
                        .await?;
                }
                VdevStep::Open(i) => self.open(&members[i].2, &members[i].1).await?,
                VdevStep::Refuse(why) => {
                    return Err(Stop::failed(format!("{} {why}.", vdev.describe())));
                }
            }
        }
    }

    /// Readies vdev `index`, proves the initramfs unlocks it, and adds it.
    pub(crate) async fn add_vdev(&self, index: usize) -> Result<Vec<String>, Stop> {
        let vdev = &self.layout.vdevs[index];
        loop {
            let prepared = self.prepare(index).await?;
            if prepared.in_pool {
                return Ok(prepared.mappers());
            }
            if prepared.encrypted {
                self.register(&prepared.members).await?;
            }
            self.flow.info(format!(
                "Adding {} to {}.",
                vdev.describe(),
                self.layout.pool
            ));
            self.run(
                Command::new(programs::ZPOOL)
                    .args(add_args(self.layout, index, &prepared.devices()))
                    .timeout(SLOW),
            )
            .await?;
        }
    }

    // ---- the rest ----

    pub(crate) async fn backup_header(&self, mapper: &str, partition: &str) -> Result<(), Stop> {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.at(HEADER_DIR);
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)))
            .map_err(|e| Stop::failed(format!("cannot create {HEADER_DIR}: {e}")))?;
        let file = format!("{HEADER_DIR}/{mapper}.luks-header");
        let _ = std::fs::remove_file(self.at(&file));
        self.run(Command::new(CRYPTSETUP).args([
            "luksHeaderBackup",
            partition,
            "--header-backup-file",
            &file,
        ]))
        .await?;
        if self.at(&file).exists() {
            std::fs::set_permissions(self.at(&file), std::fs::Permissions::from_mode(0o600))
                .map_err(|e| Stop::failed(format!("cannot restrict {file}: {e}")))?;
        }
        Ok(())
    }

    /// The setup job's header phase: a fresh backup of every member's LUKS
    /// header, since the storage phase's may predate later key changes.
    pub async fn headers(&self) -> Result<Value, Stop> {
        if self.layout.encryption == Encryption::None {
            self.flow
                .info("The pool isn't encrypted, so there are no LUKS headers to back up.");
            return Ok(json!({ "headers": [] }));
        }
        let mut files = Vec::new();
        for (mapper, partition) in self.encrypted_members()? {
            self.backup_header(&mapper, &partition).await?;
            let file = format!("{HEADER_DIR}/{mapper}.luks-header");
            let size = std::fs::metadata(self.at(&file)).map_or(0, |m| m.len());
            if size == 0 {
                return Err(Stop::failed(format!(
                    "cryptsetup left no header backup in {file}"
                )));
            }
            files.push(file);
        }
        self.flow.info(format!(
            "Backed up {} LUKS headers in {HEADER_DIR}. Keep a copy somewhere other than this \
             host: a pool whose headers are damaged can't be opened without one.",
            files.len()
        ));
        Ok(json!({ "headers": files }))
    }

    /// Every member's mapper, with its LUKS partition, in layout order.
    fn encrypted_members(&self) -> Result<Vec<(String, String)>, Stop> {
        let disks = self.host.disks();
        let mut out = Vec::new();
        for vdev in &self.layout.vdevs {
            let number = if vdev.boot { BOOT_PARTITION } else { 1 };
            for member in &vdev.members {
                let mapper = self.mapper(self.disk(&disks, member)?)?;
                out.push((mapper, part(member, number)));
            }
        }
        Ok(out)
    }

    /// The passphrase must open every member already unlocked, so a resumed
    /// job never mixes two passphrases. Members formatted but not open are
    /// tested when they are opened.
    async fn prove_passphrase(&self) -> Result<(), Stop> {
        for (mapper, partition) in self.encrypted_members()? {
            if self.open_on(&mapper, &partition).await? {
                self.test_passphrase(&partition).await?;
                self.flow.info(format!("The passphrase opens {mapper}."));
            }
        }
        Ok(())
    }

    async fn prove_shared_unlock(&self, members: &[(String, String)]) -> Result<(), Stop> {
        for program in [DECRYPT_KEYCTL, KEYCTL] {
            if !self.at(program).exists() {
                return Err(Stop::failed(format!("{program} is missing")));
            }
        }
        let text = self.read_crypttab()?;
        for (mapper, partition) in members {
            let entry = self.entry(mapper, partition).await?;
            if !luks::lists(&text, &self.layout.pool, &entry) {
                return Err(Stop::failed(format!(
                    "{CRYPTTAB} doesn't list {mapper} once, in the single-prompt form"
                )));
            }
        }
        let names: Vec<String> = members.iter().map(|(m, _)| m.clone()).collect();
        self.unpack_and_check(&names).await
    }

    async fn block_size_storages(&self) -> Result<Vec<String>, Stop> {
        let configured = std::fs::read_to_string(self.at(STORAGE_CFG)).unwrap_or_default();
        let mut made = Vec::new();
        for size in BlockSize::OFFERED {
            if size == BlockSize::K16 {
                continue;
            }
            let dataset = format!("{}/{}", self.layout.pool, size.dataset_suffix());
            let listed = self
                .code(
                    Command::new(programs::ZFS).args([
                        "list",
                        "-H",
                        "-o",
                        "name",
                        dataset.as_str(),
                    ]),
                    &[0, 1],
                )
                .await?;
            if listed != 0 {
                self.run(Command::new(programs::ZFS).args(["create", dataset.as_str()]))
                    .await?;
                made.push(dataset.clone());
            }
            let storage = size.storage();
            let defined = configured
                .lines()
                .any(|l| l.trim() == format!("zfspool: {storage}"));
            if self.define_storages && !defined {
                self.run(Command::new(PVESM).args([
                    "add",
                    "zfspool",
                    storage.as_str(),
                    "--pool",
                    dataset.as_str(),
                    "--blocksize",
                    &format!("{}k", size.bytes() / 1024),
                    "--content",
                    "images,rootdir",
                    "--sparse",
                    "1",
                ]))
                .await?;
                made.push(storage);
            }
        }
        Ok(made)
    }

    async fn build(&self) -> Result<Value, Stop> {
        let encrypted = self.layout.encryption == Encryption::Luks2;
        if encrypted {
            let boot = self.boot_vdev()?;
            if boot.members.len() < 2 {
                return Err(Stop::failed(
                    "LUKS needs a mirrored boot vdev: a single boot disk has no copy to \
                     encrypt in place from (STORAGE.md)",
                ));
            }
            let installed = super::packages::Apt {
                runner: self.runner,
                root: self.root.clone(),
            }
            .ensure(PACKAGES)
            .await?;
            if !installed.is_empty() {
                self.flow
                    .info(format!("Installed {}.", installed.join(", ")));
            }
            self.wait_for_key().await?;
            self.prove_passphrase().await?;
            for member in &self.boot_vdev()?.members.clone() {
                let mapper = self.encrypt_boot_member(member).await?;
                self.flow
                    .info(format!("{} is encrypted as {mapper}.", member.as_str()));
            }
            self.run(Command::new(PROXMOX_BOOT_TOOL).arg("refresh").timeout(SLOW))
                .await?;
        }
        for (index, vdev) in self.layout.vdevs.iter().enumerate() {
            if !vdev.boot {
                self.add_vdev(index).await?;
            }
        }
        let mut mappers = Vec::new();
        if encrypted {
            let members = self.encrypted_members()?;
            self.prove_shared_unlock(&members).await?;
            for (mapper, partition) in &members {
                self.backup_header(mapper, partition).await?;
            }
            mappers = members.into_iter().map(|(m, _)| m).collect();
            self.remove_key()?;
            self.flow.info(
                "Deleted the passphrase file; every boot asks for the passphrase at the console.",
            );
        }
        let made = self.block_size_storages().await?;
        for name in &made {
            self.flow.info(format!("Created {name}."));
        }
        Ok(json!({ "mappers": mappers }))
    }

    /// Builds the storage. The passphrase file is deleted however the phase
    /// ends, so a resumed job asks for it again rather than finding it left
    /// behind.
    pub async fn run_phase(&self) -> Result<Value, Stop> {
        let outcome = self.build().await;
        if self.layout.encryption == Encryption::Luks2 && outcome.is_err() {
            self.remove_key()?;
        }
        outcome
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
