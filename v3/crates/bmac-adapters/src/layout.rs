// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! One host's storage as it is: rpool's vdevs with each member resolved
//! through LUKS to a physical disk, the ESPs, every disk and why it is in
//! use, LUKS mappings, crypttab, and zvols (v2 `host_storage.py`,
//! `build_layout` and `collect`). The serialized form matches v2's JSON
//! document, schema version 1.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::LazyLock;

use bmac_core::{DatasetName, DiskIdentity, PoolName};
use bmac_exec::CommandRunner;
use regex::Regex;
use serde::Serialize;

use crate::disks::{BlockDevice, BlockDevices, Disks};
use crate::zfs::{Volume, Zfs};
use crate::zpool::{
    ConfigNode, ListRow, PoolStatus, VdevClass, Zpool, int_property, parse_list, parse_properties,
    parse_status, split_pools,
};
use crate::{AdapterError, programs};

pub const SCHEMA_VERSION: u32 = 1;
pub const BOOT_UUIDS_FILE: &str = "/etc/kernel/proxmox-boot-uuids";
pub const CRYPTTAB_FILE: &str = "/etc/crypttab";
/// Pool use at or above this percentage is an attention item.
pub const CAPACITY_ATTENTION_PERCENT: u64 = 80;

static RPOOL_MAPPER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:bmac-rpool-[ws][A-Za-z0-9]{1,24}|crypt-rpool-(?:a|b|mirror[1-9][0-9]*-[12]))$")
        .unwrap()
});
static ZVOL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/dev/zd[0-9]+$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Layout {
    pub schema_version: u32,
    pub host: String,
    pub collected_at: i64,
    pub pool: PoolSummary,
    pub vdevs: Vec<LayoutVdev>,
    pub auxiliary_vdevs: BTreeMap<String, Vec<AuxiliaryVdev>>,
    pub imported_pool_vdevs: Vec<ImportedVdev>,
    pub esp_partitions: Vec<String>,
    pub all_disks: Vec<DiskRow>,
    /// Disks with no reason to be in use, which may be pulled.
    pub physically_removable_disks: Vec<DiskRow>,
    /// Disks in no imported pool and holding no registered ESP.
    pub unassigned_disks: Vec<DiskRow>,
    /// Disks holding a registered ESP but in no pool: a boot-mirror
    /// replacement that has not joined rpool yet.
    pub esp_only_disks: Vec<DiskRow>,
    pub luks_mappings: Vec<LuksMapping>,
    pub crypttab: Vec<CrypttabEntry>,
    pub volumes: Vec<Volume>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PoolSummary {
    pub name: String,
    pub state: Option<String>,
    pub health: Option<String>,
    pub size: Option<u64>,
    pub allocated: Option<u64>,
    pub free: Option<u64>,
    pub capacity_percent: Option<u64>,
    pub fragmentation_percent: Option<u64>,
    /// What new data and a vdev removal can use.
    pub dataset_available: Option<u64>,
    pub dataset_used: Option<u64>,
    pub status: Option<String>,
    pub scan: Option<String>,
    pub remove: Option<String>,
    pub removal_in_progress: bool,
    pub errors: Option<String>,
}

/// A vdev member traced to its physical disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Member {
    /// The path `zpool status -P` prints.
    pub path: String,
    pub state: Option<String>,
    /// The resolved kernel device.
    pub device: String,
    pub luks: bool,
    pub mapper: Option<String>,
    /// The partition under the LUKS mapping, or the device itself.
    pub backing: Option<String>,
    pub disk: Option<String>,
    pub serial: Option<String>,
    /// The disk's WWN, or its serial when it has none.
    pub identity: Option<DiskIdentity>,
    pub model: Option<String>,
    pub disk_size: Option<u64>,
    pub member_size: Option<u64>,
    /// The `replacing-N` or `spare-N` grouping the leaf is under, if any.
    pub group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LayoutVdev {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub state: Option<String>,
    pub removing: bool,
    pub replacing: bool,
    pub size: Option<u64>,
    pub allocated: Option<u64>,
    pub free: Option<u64>,
    /// A member's disk holds a registered ESP; such a vdev is never
    /// removable.
    pub holds_esp: bool,
    pub luks: bool,
    pub members: Vec<Member>,
}

/// A log, cache, spare, special, or dedup vdev of rpool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuxiliaryVdev {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub state: Option<String>,
    pub removing: bool,
    pub replacing: bool,
    pub members: Vec<Member>,
}

/// A data vdev of any imported pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportedVdev {
    pub pool: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub state: Option<String>,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiskRow {
    pub disk: String,
    pub serial: Option<String>,
    pub identity: Option<DiskIdentity>,
    pub model: Option<String>,
    pub size: Option<u64>,
    pub tran: Option<String>,
    pub fstype: Option<String>,
    pub partitions_or_holders: usize,
    pub mounted: bool,
    /// Why the disk must not be pulled; empty when it may be.
    pub in_use_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LuksMapping {
    pub mapper: String,
    pub backing: Option<String>,
    pub disk: Option<String>,
    pub serial: Option<String>,
    pub size: Option<u64>,
    pub in_pool: bool,
}

/// An rpool line of `/etc/crypttab`. Key material is never stored there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrypttabEntry {
    pub mapper: String,
    /// Unlocked with the shared passphrase from the kernel keyring.
    pub shared_unlock: bool,
}

/// The UUIDs `proxmox-boot-tool` keeps in sync, upper-cased.
pub fn parse_boot_uuids(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| line.trim().to_uppercase())
        .collect()
}

pub fn parse_crypttab(text: &str) -> Vec<CrypttabEntry> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let mapper = *fields.first()?;
            if mapper.starts_with('#') || !RPOOL_MAPPER_RE.is_match(mapper) {
                return None;
            }
            let options = fields.get(3).copied().unwrap_or_default();
            Some(CrypttabEntry {
                mapper: mapper.to_string(),
                shared_unlock: fields.get(2) == Some(&"bmac-rpool")
                    && options.split(',').any(|o| o == "keyscript=decrypt_keyctl"),
            })
        })
        .collect()
}

fn crypttab_uuids(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields[..] {
                [first, second, ..] if !first.starts_with('#') => {
                    second.strip_prefix("UUID=").map(str::to_string)
                }
                _ => None,
            }
        })
        .collect()
}

/// What [`build`] combines.
pub struct Inputs<'a> {
    /// `zpool status -P -p` of the pool.
    pub status: &'a PoolStatus,
    /// `zpool list -v -H -p -P` of the pool.
    pub list: &'a BTreeMap<String, ListRow>,
    pub pool_properties: &'a BTreeMap<String, String>,
    pub dataset_properties: &'a BTreeMap<String, String>,
    pub devices: &'a BlockDevices,
    pub boot_uuids: &'a BTreeSet<String>,
    pub volumes: Vec<Volume>,
    pub crypttab: &'a str,
    /// `zpool status` of every imported pool.
    pub all_pools: &'a [PoolStatus],
    /// Resolves a `/dev/...` path to its kernel device.
    pub realpath: &'a dyn Fn(&str) -> String,
    pub host: String,
    pub collected_at: i64,
}

struct Resolver<'a> {
    devices: &'a BlockDevices,
    list: &'a BTreeMap<String, ListRow>,
    realpath: &'a dyn Fn(&str) -> String,
    member_disks: BTreeSet<String>,
}

impl Resolver<'_> {
    fn member(&mut self, leaf: &ConfigNode) -> Member {
        let path = &leaf.name;
        let kname = if path.starts_with("/dev/") {
            (self.realpath)(path)
        } else {
            path.clone()
        };
        let mut row = Member {
            path: path.clone(),
            state: leaf.state.clone(),
            device: kname.clone(),
            luks: false,
            mapper: None,
            backing: None,
            disk: None,
            serial: None,
            identity: None,
            model: None,
            disk_size: None,
            member_size: self.list.get(path).and_then(|r| r.size),
            group: None,
        };
        let Some(device) = self.devices.get(&kname) else {
            return row;
        };
        if device.kind.as_deref() == Some("crypt") {
            row.luks = true;
            row.mapper = device.name.clone();
            row.backing = device.parent.clone();
        } else {
            row.backing = Some(kname);
        }
        if let Some(disk) = device.disk.as_deref().and_then(|d| self.devices.get(d)) {
            self.member_disks.insert(disk.kname.clone());
            row.disk = Some(disk.kname.clone());
            row.serial = disk.serial.clone();
            row.identity = disk.identity();
            row.model = disk.model.clone();
            row.disk_size = disk.size;
        }
        row
    }

    fn members(&mut self, vdev: &ConfigNode) -> Vec<Member> {
        let mut out = Vec::new();
        self.walk(vdev, None, &mut out);
        out
    }

    /// The leaves in [`ConfigNode::leaves`] order, each with its grouping.
    fn walk(&mut self, node: &ConfigNode, group: Option<&str>, out: &mut Vec<Member>) {
        if node.children.is_empty() {
            let mut member = self.member(node);
            member.group = group.map(str::to_string);
            out.push(member);
            return;
        }
        let group = if node.name.starts_with("replacing-") || node.name.starts_with("spare-") {
            Some(node.name.as_str())
        } else {
            group
        };
        for child in &node.children {
            self.walk(child, group, out);
        }
    }
}

pub fn build(inputs: Inputs<'_>) -> Layout {
    let Inputs {
        status,
        list,
        pool_properties,
        dataset_properties,
        devices,
        boot_uuids,
        volumes,
        crypttab,
        all_pools,
        realpath,
        host,
        collected_at,
    } = inputs;
    let removing = status.removing_vdev();
    let esp_partitions: Vec<String> = devices
        .0
        .values()
        .filter(|d| {
            d.uuid
                .as_ref()
                .is_some_and(|u| !u.is_empty() && boot_uuids.contains(&u.to_uppercase()))
        })
        .map(|d| d.kname.clone())
        .collect();
    let esp_disks: BTreeSet<String> = esp_partitions
        .iter()
        .filter_map(|kname| devices.get(kname)?.disk.clone())
        .collect();
    let mut resolver = Resolver {
        devices,
        list,
        realpath,
        member_disks: BTreeSet::new(),
    };

    let vdevs: Vec<LayoutVdev> = status
        .vdevs(VdevClass::Data)
        .iter()
        .map(|vdev| {
            let members = resolver.members(vdev);
            let sizes = list.get(&vdev.name);
            LayoutVdev {
                name: vdev.name.clone(),
                kind: vdev.vdev_type(),
                state: vdev.state.clone(),
                removing: removing.as_deref() == Some(vdev.name.as_str()),
                replacing: vdev.replacing(),
                size: sizes.and_then(|r| r.size),
                allocated: sizes.and_then(|r| r.allocated),
                free: sizes.and_then(|r| r.free),
                holds_esp: members
                    .iter()
                    .any(|m| m.disk.as_ref().is_some_and(|d| esp_disks.contains(d))),
                luks: !members.is_empty() && members.iter().all(|m| m.luks),
                members,
            }
        })
        .collect();
    let auxiliary_vdevs: BTreeMap<String, Vec<AuxiliaryVdev>> = status
        .sections
        .iter()
        .filter(|section| section.class != VdevClass::Data)
        .map(|section| {
            let rows = section
                .vdevs
                .iter()
                .map(|vdev| AuxiliaryVdev {
                    name: vdev.name.clone(),
                    kind: vdev.vdev_type(),
                    state: vdev.state.clone(),
                    removing: removing.as_deref() == Some(vdev.name.as_str()),
                    replacing: vdev.replacing(),
                    members: resolver.members(vdev),
                })
                .collect();
            (section.class.as_str().to_string(), rows)
        })
        .collect();
    let imported_pool_vdevs: Vec<ImportedVdev> = all_pools
        .iter()
        .flat_map(|pool| {
            pool.vdevs(VdevClass::Data)
                .iter()
                .map(|vdev| (pool.pool.clone(), vdev))
                .collect::<Vec<_>>()
        })
        .map(|(pool, vdev)| ImportedVdev {
            pool,
            name: vdev.name.clone(),
            kind: vdev.vdev_type(),
            state: vdev.state.clone(),
            members: resolver.members(vdev),
        })
        .collect();

    let imported_pool_disks: BTreeSet<String> = all_pools
        .iter()
        .flat_map(|pool| pool.sections.iter())
        .flat_map(|section| section.vdevs.iter())
        .flat_map(ConfigNode::leaves)
        .filter(|leaf| leaf.name.starts_with("/dev/"))
        .filter_map(|leaf| devices.get(&realpath(&leaf.name))?.disk.clone())
        .collect();
    let crypttab_uuids = crypttab_uuids(crypttab);

    let mut all_disks = Vec::new();
    let mut unassigned_disks = Vec::new();
    let mut esp_only_disks = Vec::new();
    for (kname, device) in &devices.0 {
        if device.kind.as_deref() != Some("disk") || device.parent.is_some() {
            continue;
        }
        // zvols are block disks too; they belong to guests, not to the host.
        if ZVOL_RE.is_match(kname) {
            continue;
        }
        let row = disk_row(
            devices,
            device,
            &imported_pool_disks,
            &esp_disks,
            &crypttab_uuids,
        );
        all_disks.push(row.clone());
        if resolver.member_disks.contains(kname) {
            continue;
        }
        if esp_disks.contains(kname) {
            esp_only_disks.push(row);
        } else {
            unassigned_disks.push(row);
        }
    }

    let pool_devices: BTreeSet<&str> = vdevs
        .iter()
        .flat_map(|vdev| vdev.members.iter())
        .map(|member| member.device.as_str())
        .collect();
    let luks_mappings = devices
        .0
        .iter()
        .filter_map(|(kname, device)| {
            let name = device.name.as_deref().unwrap_or_default();
            let name = name.rsplit('/').next().unwrap_or_default();
            if device.kind.as_deref() != Some("crypt") || !RPOOL_MAPPER_RE.is_match(name) {
                return None;
            }
            let disk = device.disk.as_deref().and_then(|d| devices.get(d));
            Some(LuksMapping {
                mapper: name.to_string(),
                backing: device.parent.clone(),
                disk: disk.map(|d| d.kname.clone()),
                serial: disk.and_then(|d| d.serial.clone()),
                size: disk.and_then(|d| d.size),
                in_pool: pool_devices.contains(kname.as_str()),
            })
        })
        .collect();

    Layout {
        schema_version: SCHEMA_VERSION,
        host,
        collected_at,
        pool: PoolSummary {
            name: status.pool.clone(),
            state: status.state().map(str::to_string),
            health: pool_properties.get("health").cloned(),
            size: int_property(pool_properties, "size"),
            allocated: int_property(pool_properties, "allocated"),
            free: int_property(pool_properties, "free"),
            capacity_percent: int_property(pool_properties, "capacity"),
            fragmentation_percent: int_property(pool_properties, "fragmentation"),
            dataset_available: int_property(dataset_properties, "available"),
            dataset_used: int_property(dataset_properties, "used"),
            status: status.status(),
            scan: status.scan(),
            remove: status.remove(),
            removal_in_progress: removing.is_some(),
            errors: status.errors(),
        },
        vdevs,
        auxiliary_vdevs,
        imported_pool_vdevs,
        esp_partitions,
        physically_removable_disks: all_disks
            .iter()
            .filter(|disk| disk.in_use_reasons.is_empty())
            .cloned()
            .collect(),
        all_disks,
        unassigned_disks,
        esp_only_disks,
        luks_mappings,
        crypttab: parse_crypttab(crypttab),
        volumes,
    }
}

fn disk_row(
    devices: &BlockDevices,
    device: &BlockDevice,
    imported_pool_disks: &BTreeSet<String>,
    esp_disks: &BTreeSet<String>,
    crypttab_uuids: &BTreeSet<String>,
) -> DiskRow {
    let kname = &device.kname;
    let descendants = devices.subtree(kname);
    let mounted = devices.subtree_mounted(kname);
    let mut reasons = Vec::new();
    if imported_pool_disks.contains(kname) {
        reasons.push("member of an imported ZFS pool".to_string());
    }
    if mounted {
        reasons.push("mounted filesystem or active swap".to_string());
    }
    let holders: BTreeSet<&str> = descendants[1..]
        .iter()
        .filter_map(|d| d.kind.as_deref())
        .filter(|kind| *kind != "part")
        .collect();
    if !holders.is_empty() {
        let holders: Vec<&str> = holders.into_iter().collect();
        reasons.push(format!("active {} holder", holders.join("/")));
    }
    if esp_disks.contains(kname) {
        reasons.push("registered Proxmox boot ESP".to_string());
    }
    if descendants
        .iter()
        .any(|d| d.uuid.as_ref().is_some_and(|u| crypttab_uuids.contains(u)))
    {
        reasons.push("configured in /etc/crypttab".to_string());
    }
    DiskRow {
        disk: kname.clone(),
        serial: device.serial.clone(),
        identity: device.identity(),
        model: device.model.clone(),
        size: device.size,
        tran: device.tran.clone(),
        fstype: device.fstype.clone(),
        partitions_or_holders: device.children.len(),
        mounted,
        in_use_reasons: reasons,
    }
}

/// Storage problems for an operator (v2 `host_storage.py`,
/// `attention_items` 753–808). v2's comparison with the serials in a host's
/// `.conf` file is left to the storage layout record (`STORAGE.md`), which
/// replaces those files.
pub fn attention_items(layout: &Layout) -> Vec<String> {
    let mut items = Vec::new();
    let pool = &layout.pool;
    let name = &pool.name;
    if pool.health.as_deref() != Some("ONLINE") {
        items.push(format!(
            "{name} health is {}",
            pool.health.as_deref().unwrap_or("unknown")
        ));
    }
    if let Some(errors) = &pool.errors
        && errors != "No known data errors"
    {
        items.push(format!("{name} reports errors: {errors}"));
    }
    if let Some(capacity) = pool.capacity_percent
        && capacity >= CAPACITY_ATTENTION_PERCENT
    {
        items.push(format!(
            "{name} is {capacity}% allocated (attention at {CAPACITY_ATTENTION_PERCENT}%); \
             sparse production zvols can exhaust it"
        ));
    }
    if pool.removal_in_progress {
        let first = pool
            .remove
            .as_deref()
            .and_then(|text| text.lines().next())
            .unwrap_or_default();
        items.push(format!("a top-level vdev removal is in progress: {first}"));
    }
    for vdev in &layout.vdevs {
        if vdev.state.as_deref() != Some("ONLINE") {
            items.push(format!(
                "vdev {} is {}",
                vdev.name,
                vdev.state.as_deref().unwrap_or("unknown")
            ));
        }
        for member in vdev.members.iter().filter(|m| m.serial.is_none()) {
            items.push(format!(
                "member {} of {} could not be resolved to a physical disk serial",
                member.path, vdev.name
            ));
        }
    }
    if !layout.vdevs.iter().any(|vdev| vdev.holds_esp) {
        items.push("no rpool vdev could be matched to a proxmox-boot-tool ESP".to_string());
    }
    for disk in &layout.esp_only_disks {
        items.push(format!(
            "disk {} ({}) holds a registered ESP but is not an rpool member; \
             finish its replacement with the Replace disk operation",
            disk.disk,
            disk.serial.as_deref().unwrap_or("no serial")
        ));
    }
    items
}

/// What [`collect`] reads from the host besides command output.
pub trait HostFiles: Sync {
    fn read(&self, path: &Path) -> Option<String>;
    /// Resolves symlinks, or returns the path unchanged when it cannot.
    fn realpath(&self, path: &str) -> String;
    fn hostname(&self) -> String;
    fn now(&self) -> i64;
}

/// The host BMAC runs on.
pub struct LocalHost;

impl HostFiles for LocalHost {
    fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn realpath(&self, path: &str) -> String {
        std::fs::canonicalize(path)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.to_string())
    }

    fn hostname(&self) -> String {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|name| {
                name.trim()
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .unwrap_or_default()
    }

    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default()
    }
}

/// Reads `pool`'s layout with read-only commands. With
/// `allow_missing_pool`, a host whose pool is not imported still reports
/// its disks, as before a host is set up.
pub async fn collect(
    runner: &dyn CommandRunner,
    files: &dyn HostFiles,
    pool: &PoolName,
    allow_missing_pool: bool,
) -> Result<Layout, AdapterError> {
    let boot_uuids = parse_boot_uuids(&files.read(Path::new(BOOT_UUIDS_FILE)).unwrap_or_default());
    let crypttab = files.read(Path::new(CRYPTTAB_FILE)).unwrap_or_default();
    let devices = Disks::new(runner).block_devices().await?;
    let zpool = Zpool::new(runner);
    let realpath = |path: &str| files.realpath(path);
    let no_rows = BTreeMap::new();
    let no_properties = BTreeMap::new();

    let status = match zpool.status(pool).await {
        Ok(status) => status,
        Err(_) if allow_missing_pool => {
            let absent = parse_status(&format!(
                "pool: {pool}\nstate: UNAVAILABLE\nconfig:\n\n\
                 \tNAME        STATE\n\t{pool}       UNAVAIL\n\n\
                 errors: no pool is imported\n"
            ))
            .expect("the placeholder status parses");
            return Ok(build(Inputs {
                status: &absent,
                list: &no_rows,
                pool_properties: &no_properties,
                dataset_properties: &no_properties,
                devices: &devices,
                boot_uuids: &boot_uuids,
                volumes: Vec::new(),
                crypttab: &crypttab,
                all_pools: &[],
                realpath: &realpath,
                host: files.hostname(),
                collected_at: files.now(),
            }));
        }
        Err(error) => return Err(error),
    };
    let all_pools = match zpool.status_all_text().await {
        Ok(text) => split_pools(&text)
            .iter()
            .map(|block| parse_status(block))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AdapterError::Parse {
                command: format!("{} status -L -P -p", programs::ZPOOL),
                message: e.0,
            })?,
        Err(_) => vec![status.clone()],
    };
    let list = parse_list(&zpool.list_text(pool).await?);
    let pool_properties = zpool
        .properties(
            pool,
            &[
                "size",
                "allocated",
                "free",
                "capacity",
                "fragmentation",
                "health",
            ],
        )
        .await?;
    let root: DatasetName = pool
        .as_str()
        .parse()
        .map_err(|e: bmac_core::InvalidValue| AdapterError::Parse {
            command: "pool name".to_string(),
            message: e.to_string(),
        })?;
    let zfs = Zfs::new(runner);
    let dataset_properties = zfs.properties(&root, &["available", "used"]).await?;
    let volumes = zfs.volumes(&root).await?;
    Ok(build(Inputs {
        status: &status,
        list: &list,
        pool_properties: &pool_properties,
        dataset_properties: &dataset_properties,
        devices: &devices,
        boot_uuids: &boot_uuids,
        volumes,
        crypttab: &crypttab,
        all_pools: &all_pools,
        realpath: &realpath,
        host: files.hostname(),
        collected_at: files.now(),
    }))
}

/// Parses the outputs [`collect`] gathers; for tests and recorded reports.
pub struct Texts<'a> {
    pub status: &'a str,
    pub list: &'a str,
    pub pool_properties: &'a str,
    pub dataset_properties: &'a str,
    pub lsblk: &'a str,
    pub boot_uuids: &'a str,
    pub volumes: &'a str,
    pub snapshots: &'a str,
    pub crypttab: &'a str,
    /// `None` uses `status`, as v2 does when `zpool status -LP` fails.
    pub all_pools: Option<&'a str>,
}

pub fn build_from_texts(
    texts: &Texts<'_>,
    realpath: &dyn Fn(&str) -> String,
    host: &str,
    collected_at: i64,
) -> Result<Layout, String> {
    let status = parse_status(texts.status).map_err(|e| e.0)?;
    let all_pools = split_pools(texts.all_pools.unwrap_or(texts.status))
        .iter()
        .map(|block| parse_status(block).map_err(|e| e.0))
        .collect::<Result<Vec<_>, _>>()?;
    let devices = BlockDevices::parse(texts.lsblk)?;
    Ok(build(Inputs {
        status: &status,
        list: &parse_list(texts.list),
        pool_properties: &parse_properties(texts.pool_properties),
        dataset_properties: &parse_properties(texts.dataset_properties),
        devices: &devices,
        boot_uuids: &parse_boot_uuids(texts.boot_uuids),
        volumes: crate::zfs::parse_volumes(texts.volumes, texts.snapshots),
        crypttab: texts.crypttab,
        all_pools: &all_pools,
        realpath,
        host: host.to_string(),
        collected_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_members_are_rpool_crypttab_entries() {
        let text = "bmac-rpool-w5000c500a1b2c3d4\tUUID=u1\tbmac-rpool\tluks,initramfs,nofail,keyscript=decrypt_keyctl\n\
                    bmac-rpool-sQM00001 UUID=u2 none luks,initramfs\n\
                    bmac-other-sX UUID=u3 none luks\n";
        let entries = parse_crypttab(text);
        let found: Vec<(&str, bool)> = entries
            .iter()
            .map(|e| (e.mapper.as_str(), e.shared_unlock))
            .collect();
        assert_eq!(
            found,
            [
                ("bmac-rpool-w5000c500a1b2c3d4", true),
                ("bmac-rpool-sQM00001", false)
            ]
        );
    }
}
