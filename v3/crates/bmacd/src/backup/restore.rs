// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `backup.restore` (`BACKUP.md`, "Restore"): a PIT from an appliance, read
//! under a grant its administrator issued, restored as a new VM on the host
//! the job runs on.
//!
//! 1. The operator picks the appliance and pastes the grant; the appliance
//!    lists the PITs it names, with their manifests.
//! 2. The operator picks the PIT, the new VM's name, and whether its network
//!    stays disconnected.
//! 3. Preflight: a storage here for each volume's block size, space, and the
//!    bridges the VM uses. Each volume gets the cheapest method the
//!    snapshots here allow, and the appliance sizes each stream.
//! 4. After a confirmation that shows the methods and bytes, each volume is
//!    cloned or received under `<pool>/bmac-restore/<job>/`, resumably,
//!    with `volmode=none`, then checked against the manifest's GUID and
//!    size.
//! 5. Each volume is renamed into the storage for its block size as
//!    `vm-<vmid>-disk-<n>`, the VM is created without disks from the PIT's
//!    configuration, and the volumes are attached at their slots.
//! 6. The VM boots, and the guest agent is asked to answer.
//!
//! A failure before the VM exists destroys what was received; after, the
//! VM and its disks.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use bmac_backup::manifest::Manifest;
use bmac_backup::records::{
    ApplianceRecord, Enrollment, RECORD_SCHEMA, RestoreRecord, TestOutcome,
};
use bmac_backup::restore::{
    self, Base, LocalSnapshot, Method, Mode, VmConfig, disk_name, human_bytes, staging,
};
use bmac_core::job::{FieldType, PhaseStatus, Severity};
use bmac_core::{HostSlot, SnapshotGuid};
use bmac_exec::{BoxFuture, Command, CommandRunner};
use bmac_proto::api::Role;
use bmac_proto::appliance::{ExportRequest, GrantedIndex, PeerPit};
use bmac_proto::operations::{
    CancelRule, Category, Operation, OperationKind, ResumeRule, TargetRule,
};
use serde_json::{Value, json};
use uuid::Uuid;

use super::exports::ApplianceClient;
use super::store::BackupStore;
use crate::native::disks::{choice, field, text};
use crate::native::{Stop, Workflow};

pub const OPERATION: &str = "backup.restore";
pub const TEST_OPERATION: &str = "backup.restore_test";
const RECEIVE_PHASE: (&str, &str) = ("receive", "Bring each volume to the PIT");
const CREATE_PHASE: (&str, &str) = ("create", "Create the VM");
/// Interrupted receives are resumed this many times.
const ATTEMPTS: u32 = 3;
const ZFS: &str = "/usr/sbin/zfs";
const QM: &str = "/usr/sbin/qm";
const PVESH: &str = "/usr/bin/pvesh";
const STORAGE_CFG: &str = "/etc/pve/storage.cfg";

pub fn operation() -> Operation {
    spec(
        OPERATION,
        "Restore a PIT",
        "Restore a PIT from a backup appliance as a new VM on this host.",
        "Needs a restore grant from the appliance's administrator. Each volume comes by the \
         cheapest method the snapshots on this host allow: a clone of the PIT's own snapshot, \
         an incremental from an older one, or a full stream. The new VM gets new MAC \
         addresses and IDs, and its network can stay disconnected until you choose.",
    )
}

pub fn test_operation() -> Operation {
    spec(
        TEST_OPERATION,
        "Test a restore",
        "Restore a PIT into a scratch VM on this host, boot it, check it, and destroy it.",
        "Needs a restore grant from the appliance's administrator. The scratch VM's network \
         stays disconnected. The result (whether it booted and its guest agent answered) is \
         kept with the cluster's restores; only the scratch VM is destroyed.",
    )
}

fn spec(id: &str, title: &str, summary: &str, description: &str) -> Operation {
    Operation {
        id: id.parse().expect("a valid operation id"),
        title: title.into(),
        summary: summary.into(),
        description: description.into(),
        category: Category::Backups,
        kind: OperationKind::Native,
        script: None,
        min_role: Role::Admin,
        mutating: true,
        destructive: false,
        supports_dry_run: false,
        params: vec![
            serde_json::from_value(json!({
                "id": "host",
                "label": "Host",
                "type": "host",
                "help": "The host the restored VM runs on.",
                "required": true,
            }))
            .expect("a valid parameter"),
        ],
        locks: vec!["restore:{host}".into()],
        target: TargetRule::Host {
            param: "host".into(),
        },
        cancel: CancelRule::Phases {
            not_during: vec![CREATE_PHASE.0.into()],
        },
        resume: ResumeRule::Never,
        notes: Some("New in v3.".into()),
    }
}

/// A Proxmox ZFS storage on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Storage {
    pub id: String,
    pub dataset: String,
    pub block: u64,
}

impl Storage {
    pub fn pool(&self) -> &str {
        self.dataset.split('/').next().unwrap_or_default()
    }
}

fn block_bytes(text: &str) -> Option<u64> {
    let t = text.trim().to_ascii_lowercase();
    let (number, unit) = match t.strip_suffix('k') {
        Some(n) => (n, 1024),
        None => (t.as_str(), 1),
    };
    number.parse::<u64>().ok().map(|n| n * unit)
}

/// The enabled `zfspool` storages of `storage.cfg` that `node` may use;
/// Proxmox makes zvols of 16 KiB blocks where a storage sets none.
pub fn parse_storages(cfg: &str, node: &str) -> Vec<Storage> {
    let mut out = Vec::new();
    let mut current: Option<(String, BTreeMap<String, String>)> = None;
    let mut finish = |entry: Option<(String, BTreeMap<String, String>)>| {
        let Some((id, keys)) = entry else { return };
        let allowed = keys
            .get("nodes")
            .is_none_or(|n| n.split(',').any(|x| x.trim() == node));
        if keys.contains_key("disable") || !allowed {
            return;
        }
        if let Some(pool) = keys.get("pool") {
            out.push(Storage {
                id,
                dataset: pool.clone(),
                block: keys
                    .get("blocksize")
                    .and_then(|b| block_bytes(b))
                    .unwrap_or(16384),
            });
        }
    };
    for line in cfg.lines() {
        if !line.starts_with([' ', '\t']) {
            if let Some((kind, id)) = line.split_once(':') {
                finish(current.take());
                if kind.trim() == "zfspool" {
                    current = Some((id.trim().to_string(), BTreeMap::new()));
                }
            }
            continue;
        }
        if let Some((_, keys)) = &mut current {
            let mut words = line.split_whitespace();
            if let Some(key) = words.next() {
                keys.insert(key.to_string(), words.collect::<Vec<_>>().join(" "));
            }
        }
    }
    finish(current);
    out
}

/// What a restore asks of this host.
pub trait RestoreHost: Send + Sync {
    fn this_host(&self) -> HostSlot;
    fn storages(&self) -> BoxFuture<'_, Result<Vec<Storage>, String>>;
    /// Every snapshot in `pool`, with its GUID.
    fn snapshots(&self, pool: &str) -> BoxFuture<'_, Result<Vec<LocalSnapshot>, String>>;
    /// Bytes free for new data under `dataset`.
    fn available(&self, dataset: &str) -> BoxFuture<'_, Result<u64, String>>;
    /// The names of the datasets right under `parent`.
    fn children(&self, parent: &str) -> BoxFuture<'_, Result<BTreeSet<String>, String>>;
    fn bridges(&self) -> BoxFuture<'_, Result<BTreeSet<String>, String>>;
    /// `<pool>/bmac-restore/<job>`, unmounted, for the staging volumes.
    fn staging_parent(&self, pool: &str, job: &str) -> BoxFuture<'_, Result<(), String>>;
    fn clone_snapshot(&self, snapshot: &str, target: &str) -> BoxFuture<'_, Result<(), String>>;
    fn resume_token(&self, dataset: &str) -> BoxFuture<'_, Result<Option<String>, String>>;
    /// The GUID of a snapshot, if it exists.
    fn guid(&self, snapshot: &str) -> BoxFuture<'_, Result<Option<SnapshotGuid>, String>>;
    /// The GUID of a clone's origin snapshot.
    fn origin_guid(&self, dataset: &str) -> BoxFuture<'_, Result<Option<SnapshotGuid>, String>>;
    fn volsize(&self, dataset: &str) -> BoxFuture<'_, Result<u64, String>>;
    /// Renames a zvol and lets it inherit `volmode` again.
    fn place(&self, from: &str, to: &str) -> BoxFuture<'_, Result<(), String>>;
    /// `zfs destroy -r`, when it exists.
    fn destroy_tree(&self, dataset: &str) -> BoxFuture<'_, Result<(), String>>;
    fn next_vmid(&self) -> BoxFuture<'_, Result<u32, String>>;
    fn create_vm(
        &self,
        vmid: u32,
        options: &[(String, String)],
    ) -> BoxFuture<'_, Result<(), String>>;
    fn set_vm(&self, vmid: u32, key: &str, value: &str) -> BoxFuture<'_, Result<(), String>>;
    fn start_vm(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>>;
    fn agent_answers(&self, vmid: u32) -> BoxFuture<'_, bool>;
    /// `qm destroy` with the disks it owns.
    fn destroy_vm(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>>;
}

/// What a restore asks of an appliance; `receive` also runs `zfs receive`
/// on this host, since the stream goes straight into it.
pub trait Transfers: Send + Sync {
    fn granted(
        &self,
        appliance: &ApplianceRecord,
        cluster_uuid: &str,
        grant: &str,
    ) -> BoxFuture<'_, Result<GrantedIndex, String>>;
    fn estimate(
        &self,
        appliance: &ApplianceRecord,
        request: &ExportRequest,
    ) -> BoxFuture<'_, Result<u64, String>>;
    /// Receives the export into `target` (a clone of `origin` for an
    /// incremental); the bytes moved.
    fn receive(
        &self,
        appliance: &ApplianceRecord,
        request: &ExportRequest,
        target: &str,
        origin: Option<&str>,
    ) -> BoxFuture<'_, Result<u64, String>>;
}

pub struct Context<'a> {
    pub flow: &'a Workflow,
    pub host: &'a dyn RestoreHost,
    pub transfers: &'a dyn Transfers,
    pub store: &'a BackupStore,
    pub cluster_uuid: String,
    pub now_ms: &'a (dyn Fn() -> i64 + Send + Sync),
    /// How long the restored VM's guest agent has to answer.
    pub agent_wait: Duration,
    pub poll: Duration,
}

/// A volume's part in the plan.
#[derive(Debug, Clone)]
pub struct Planned {
    pub volume: bmac_backup::manifest::Volume,
    pub storage: Storage,
    pub method: Method,
    /// What the appliance sends; 0 for a clone.
    pub bytes: u64,
}

/// What the operator chose and the plan for it.
#[derive(Debug)]
pub struct Plan {
    pub appliance: ApplianceRecord,
    pub grant: String,
    pub pit: PeerPit,
    pub manifest: Manifest,
    pub name: String,
    pub isolate: bool,
    pub start: bool,
    /// A restore test's scratch VM.
    pub test: bool,
    /// The VMID to take; otherwise the next free one.
    pub vmid: Option<u32>,
    pub volumes: Vec<Planned>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    pub vmid: u32,
    pub name: String,
    pub booted: bool,
    pub agent: bool,
    pub agent_after_s: Option<u64>,
    /// Each slot's dataset.
    pub datasets: BTreeMap<String, String>,
}

pub(super) fn vm_name(manifest: &Manifest) -> String {
    manifest
        .config
        .get("name")
        .cloned()
        .unwrap_or_else(|| format!("vm{}", manifest.vmid))
}

/// A restore test's scratch VM's name.
fn scratch_name(manifest: &Manifest) -> String {
    let mut name = format!("bmac-test-{}", vm_name(manifest));
    name.truncate(63);
    name.trim_end_matches(['-', '.']).to_string()
}

fn is_vm_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .split('.')
            .all(|l| !l.is_empty() && !l.starts_with('-') && !l.ends_with('-'))
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

pub(super) fn when(taken_s: i64) -> String {
    jiff::Timestamp::from_second(taken_s)
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

/// The PIT's ancestors among the appliance's earlier PITs, across the
/// branches it descends from, with the volume's GUID in each.
pub fn bases_for(index: &GrantedIndex, pit: &PeerPit, volume_uuid: &Uuid) -> Vec<Base> {
    let mut out = Vec::new();
    let mut next = pit.parent;
    let mut seen = BTreeSet::new();
    while let Some(id) = next {
        if !seen.insert(id) {
            break;
        }
        let Some(base) = index.bases.iter().find(|b| b.pit_uuid == id) else {
            break;
        };
        if let Some(v) = base.volumes.iter().find(|v| v.volume_uuid == *volume_uuid) {
            out.push(Base {
                pit_uuid: base.pit_uuid,
                taken_s: base.taken_s,
                guid: v.guid,
            });
        }
        next = base.parent;
    }
    out
}

pub(super) fn request(
    cx: &Context<'_>,
    plan: &Plan,
    planned: &Planned,
) -> Result<ExportRequest, Stop> {
    Ok(ExportRequest {
        cluster_uuid: cx.cluster_uuid.clone(),
        pit_uuid: plan.pit.pit_uuid,
        volume_uuid: planned
            .volume
            .volume_uuid
            .parse()
            .map_err(|_| Stop::failed("the manifest's volume ID isn't a UUID"))?,
        from_guid: match &planned.method {
            Method::Incremental { base_guid, .. } => Some(*base_guid),
            _ => None,
        },
        resume_token: None,
        estimate: false,
        grant: Some(plan.grant.clone()),
    })
}

pub(super) async fn source(
    cx: &Context<'_>,
) -> Result<(ApplianceRecord, String, GrantedIndex), Stop> {
    let appliances: Vec<ApplianceRecord> = cx
        .store
        .appliances()
        .map_err(|e| Stop::failed(e.message))?
        .into_iter()
        .filter(|a| a.enrollment == Enrollment::Approved && a.address.is_some())
        .collect();
    if appliances.is_empty() {
        return Err(Stop::failed(
            "No approved appliance has an address. Set its private or public address in the \
             appliance's settings, so the hosts and a restore can reach its port 8445.",
        ));
    }
    let mut problem: Option<String> = None;
    for _ in 0..3 {
        let mut appliance = field("appliance", FieldType::Select, "Appliance");
        appliance.options = appliances
            .iter()
            .map(|a| {
                choice(
                    &a.appliance_uuid.to_string(),
                    &a.name,
                    a.address.as_ref().map(|addr| format!("at {addr}")),
                )
            })
            .collect();
        if appliances.len() == 1 {
            appliance.default = Some(json!(appliances[0].appliance_uuid.to_string()));
        }
        let mut grant = field("grant", FieldType::Password, "Restore grant");
        grant.help = Some(
            "Issued on the appliance's UI, on its Grants page, for the PITs to restore. It is \
             shown there only once."
                .into(),
        );
        let values = cx
            .flow
            .input_group(
                "Where the PIT comes from",
                problem.as_deref(),
                vec![appliance, grant],
                |v| {
                    let mut errors = BTreeMap::new();
                    if !text(v, "grant").starts_with("bmac-grant-") {
                        errors.insert("grant".into(), "A grant starts with bmac-grant-.".into());
                    }
                    if errors.is_empty() {
                        Ok(())
                    } else {
                        Err(errors)
                    }
                },
            )
            .await?;
        let chosen = appliances
            .iter()
            .find(|a| a.appliance_uuid.to_string() == text(&values, "appliance"))
            .ok_or_else(|| Stop::failed("that appliance is no longer offered"))?;
        let grant = text(&values, "grant");
        match cx.transfers.granted(chosen, &cx.cluster_uuid, &grant).await {
            Ok(index) if index.pits.is_empty() => {
                problem = Some("That grant names no PIT the appliance still has.".into());
            }
            Ok(index) => return Ok((chosen.clone(), grant, index)),
            Err(e) => problem = Some(e),
        }
        if let Some(p) = &problem {
            cx.flow.warning(p.clone());
        }
    }
    Err(Stop::failed(problem.unwrap_or_else(|| {
        "the appliance didn't accept the grant".into()
    })))
}

async fn choose(
    cx: &Context<'_>,
    appliance: ApplianceRecord,
    grant: String,
    index: GrantedIndex,
    test: bool,
) -> Result<Plan, Stop> {
    let mut pits = index.pits.clone();
    pits.sort_by_key(|p| std::cmp::Reverse((p.taken_s, p.pit_uuid)));
    let mut manifests = BTreeMap::new();
    for p in &pits {
        let manifest = Manifest::open(&p.manifest)
            .map_err(|e| Stop::failed(format!("PIT {}'s manifest: {e}", p.name)))?;
        if manifest.pit_uuid != p.pit_uuid.to_string() || manifest.cluster_uuid != cx.cluster_uuid {
            return Err(Stop::failed(format!(
                "PIT {}'s manifest is for another PIT or cluster",
                p.name
            )));
        }
        manifests.insert(p.pit_uuid, manifest);
    }
    let mut pit = field("pit", FieldType::Select, "PIT");
    pit.options = pits
        .iter()
        .map(|p| {
            let m = &manifests[&p.pit_uuid];
            choice(
                &p.pit_uuid.to_string(),
                &format!("{} at {}", vm_name(m), when(p.taken_s)),
                Some(format!("{}, {} volumes", p.name, m.volumes.len())),
            )
        })
        .collect();
    pit.default = Some(json!(pits[0].pit_uuid.to_string()));
    let mut name = field("name", FieldType::Hostname, "New VM's name");
    name.default = Some(json!(format!(
        "{}-restored",
        vm_name(&manifests[&pits[0].pit_uuid])
    )));
    let mut isolate = field(
        "isolate",
        FieldType::Boolean,
        "Keep its network disconnected",
    );
    isolate.default = Some(json!(true));
    isolate.help = Some(
        "Every network link starts down, so the restored VM can't collide with production. \
         Bring them up in Proxmox when you choose."
            .into(),
    );
    let mut start = field(
        "start",
        FieldType::Boolean,
        "Start it and check its guest agent",
    );
    start.default = Some(json!(true));
    let fields = if test {
        vec![pit]
    } else {
        vec![pit, name, isolate, start]
    };
    let values = cx
        .flow
        .input_group(
            if test {
                "What to test"
            } else {
                "What to restore"
            },
            Some(&format!(
                "The grant lets this cluster read {} PIT{} until {}.",
                pits.len(),
                if pits.len() == 1 { "" } else { "s" },
                when(index.expires_at_ms.div_euclid(1000))
            )),
            fields,
            |v| {
                let mut errors = BTreeMap::new();
                if !test && !is_vm_name(&text(v, "name")) {
                    errors.insert(
                        "name".into(),
                        "Letters, digits, dots, and dashes, up to 63.".into(),
                    );
                }
                if errors.is_empty() {
                    Ok(())
                } else {
                    Err(errors)
                }
            },
        )
        .await?;
    let chosen = pits
        .iter()
        .find(|p| p.pit_uuid.to_string() == text(&values, "pit"))
        .cloned()
        .ok_or_else(|| Stop::failed("that PIT is no longer offered"))?;
    let manifest = manifests.remove(&chosen.pit_uuid).expect("checked above");
    let flag = |id: &str| test || values.get(id).and_then(Value::as_bool).unwrap_or(true);
    let name = if test {
        scratch_name(&manifest)
    } else {
        text(&values, "name")
    };
    let mut plan = Plan {
        appliance,
        grant,
        pit: chosen,
        manifest,
        name,
        isolate: flag("isolate"),
        start: flag("start"),
        test,
        vmid: None,
        volumes: Vec::new(),
    };
    plan.volumes = preflight(cx, &plan, &index).await?;
    Ok(plan)
}

pub(super) async fn preflight(
    cx: &Context<'_>,
    plan: &Plan,
    index: &GrantedIndex,
) -> Result<Vec<Planned>, Stop> {
    let me = cx.host.this_host();
    let storages = cx.host.storages().await.map_err(Stop::failed)?;
    let mut problems = Vec::new();
    let mut placed = Vec::new();
    for volume in &plan.manifest.volumes {
        let Some(storage) = storages.iter().find(|s| s.block == volume.volblocksize) else {
            problems.push(format!(
                "{}: {} has no ZFS storage with {} blocks, and a zvol's block size can't change",
                volume.slot,
                me,
                human_bytes(volume.volblocksize)
            ));
            continue;
        };
        placed.push((volume.clone(), storage.clone()));
    }
    let bridges = cx.host.bridges().await.map_err(Stop::failed)?;
    for (key, value) in &plan.manifest.config {
        if key.starts_with("net")
            && let Some(bridge) = value.split(',').find_map(|p| p.strip_prefix("bridge="))
            && !bridges.contains(bridge)
        {
            problems.push(format!("{key}: {me} has no bridge {bridge}"));
        }
    }
    if !problems.is_empty() {
        return Err(Stop::Failed {
            message: format!("{me} can't take this restore: {}", problems.join("; ")),
            details: None,
        });
    }
    size_up(cx, plan, index, placed).await
}

/// Each volume's method in the storage chosen for it, the stream sizes,
/// and a check that the storages have the space.
pub(super) async fn size_up(
    cx: &Context<'_>,
    plan: &Plan,
    index: &GrantedIndex,
    placed: Vec<(bmac_backup::manifest::Volume, Storage)>,
) -> Result<Vec<Planned>, Stop> {
    let mut snapshots: BTreeMap<String, Vec<LocalSnapshot>> = BTreeMap::new();
    let mut out = Vec::new();
    for (volume, storage) in placed {
        let pool = storage.pool().to_string();
        if !snapshots.contains_key(&pool) {
            let found = cx.host.snapshots(&pool).await.map_err(Stop::failed)?;
            snapshots.insert(pool.clone(), found);
        }
        let volume_uuid: Uuid = volume
            .volume_uuid
            .parse()
            .map_err(|_| Stop::failed("the manifest's volume ID isn't a UUID"))?;
        let method = restore::choose(
            &volume,
            &pool,
            &snapshots[&pool],
            &bases_for(index, &plan.pit, &volume_uuid),
        );
        out.push(Planned {
            volume,
            storage,
            method,
            bytes: 0,
        });
    }
    for planned in &mut out {
        if planned.method.from_appliance() {
            let req = request(cx, plan, planned)?;
            planned.bytes = cx
                .transfers
                .estimate(&plan.appliance, &req)
                .await
                .map_err(Stop::failed)?;
        }
    }
    let mut needed: BTreeMap<&str, u64> = BTreeMap::new();
    for p in &out {
        *needed.entry(p.storage.dataset.as_str()).or_default() += p.bytes;
    }
    for (dataset, bytes) in needed {
        let free = cx.host.available(dataset).await.map_err(Stop::failed)?;
        if bytes > free {
            return Err(Stop::failed(format!(
                "{dataset} has {} free, and the restore writes {}",
                human_bytes(free),
                human_bytes(bytes)
            )));
        }
    }
    Ok(out)
}

pub(super) fn describe(p: &Planned, index_name: impl Fn(&Uuid) -> String) -> String {
    match &p.method {
        Method::Local { snapshot } => format!(
            "{}: method 1, a clone of {snapshot}; nothing crosses the network",
            p.volume.slot
        ),
        Method::Incremental { base_pit, .. } => format!(
            "{}: method 2, an incremental from {}: {} to transfer",
            p.volume.slot,
            index_name(base_pit),
            human_bytes(p.bytes)
        ),
        Method::Full => format!(
            "{}: method 3, a full stream: {} to transfer",
            p.volume.slot,
            human_bytes(p.bytes)
        ),
    }
}

pub(super) async fn bring(
    cx: &Context<'_>,
    plan: &Plan,
    planned: &Planned,
    target: &str,
) -> Result<(), Stop> {
    let slot = &planned.volume.slot;
    match &planned.method {
        Method::Local { snapshot } => {
            cx.host
                .clone_snapshot(snapshot, target)
                .await
                .map_err(Stop::failed)?;
            let origin = cx.host.origin_guid(target).await.map_err(Stop::failed)?;
            if origin != Some(planned.volume.snapshot_guid) {
                return Err(Stop::failed(format!(
                    "{slot}: the clone's origin doesn't have the PIT's GUID"
                )));
            }
        }
        Method::Incremental { .. } | Method::Full => {
            let origin = match &planned.method {
                Method::Incremental { base, .. } => Some(base.as_str()),
                _ => None,
            };
            let mut req = request(cx, plan, planned)?;
            let started = Instant::now();
            let mut moved = 0;
            let mut last_error = String::new();
            for attempt in 1..=ATTEMPTS {
                cx.flow.check()?;
                match cx
                    .transfers
                    .receive(&plan.appliance, &req, target, origin)
                    .await
                {
                    Ok(bytes) => {
                        moved += bytes;
                        last_error.clear();
                        break;
                    }
                    Err(e) => {
                        last_error = e;
                        let token = cx.host.resume_token(target).await.map_err(Stop::failed)?;
                        if token.is_none() || attempt == ATTEMPTS {
                            break;
                        }
                        cx.flow.warning(format!(
                            "{slot}: the stream stopped ({last_error}); resuming it"
                        ));
                        req.resume_token = token;
                        req.from_guid = None;
                    }
                }
            }
            if !last_error.is_empty() {
                return Err(Stop::failed(format!("{slot}: {last_error}")));
            }
            let seconds = started.elapsed().as_secs_f64().max(0.001);
            cx.flow.info(format!(
                "{slot}: received {} in {:.0} s ({}/s).",
                human_bytes(moved),
                seconds,
                human_bytes((moved as f64 / seconds) as u64)
            ));
            let snapshot = format!("{target}@{}", plan.pit.name);
            let guid = cx.host.guid(&snapshot).await.map_err(Stop::failed)?;
            if guid != Some(planned.volume.snapshot_guid) {
                return Err(Stop::failed(format!(
                    "{slot}: the received snapshot doesn't have the PIT's GUID"
                )));
            }
        }
    }
    let size = cx.host.volsize(target).await.map_err(Stop::failed)?;
    if size != planned.volume.volsize {
        return Err(Stop::failed(format!(
            "{slot}: the restored volume is {size} bytes, and the manifest says {}",
            planned.volume.volsize
        )));
    }
    Ok(())
}

pub(super) fn record_restore(cx: &Context<'_>, record: &RestoreRecord) -> RestoreRecord {
    match cx.store.locked(|l| l.save_restore(record.clone())) {
        Ok(saved) => saved,
        Err(e) => {
            cx.flow
                .warning(format!("Could not record the restore: {}", e.message));
            record.clone()
        }
    }
}

pub async fn restore(cx: &Context<'_>, test: bool) -> Result<Restored, Stop> {
    cx.flow
        .phase("plan", "Choose the PIT and plan", PhaseStatus::Running);
    let (appliance, grant, index) = source(cx).await?;
    let plan = choose(cx, appliance, grant, index.clone(), test).await?;
    cx.flow
        .phase("plan", "Choose the PIT and plan", PhaseStatus::Complete);

    let names: BTreeMap<Uuid, String> = index
        .bases
        .iter()
        .map(|b| (b.pit_uuid, when(b.taken_s)))
        .collect();
    let mut details: Vec<String> = plan
        .volumes
        .iter()
        .map(|p| {
            describe(p, |id| {
                names
                    .get(id)
                    .map_or_else(|| id.to_string(), |n| format!("the PIT of {n}"))
            })
        })
        .collect();
    let total: u64 = plan.volumes.iter().map(|p| p.bytes).sum();
    details.push(format!("{} to transfer in all", human_bytes(total)));
    if plan.volumes.iter().any(|p| p.method == Method::Full) {
        details.push("A full stream may take a long time.".into());
    }
    let translated_preview = translate(&plan, 0)?;
    details.extend(
        translated_preview
            .left_out
            .iter()
            .map(|l| format!("Left out: {l}")),
    );
    let confirmed = cx
        .flow
        .confirm(
            if test {
                "Test a restore"
            } else {
                "Restore as a new VM"
            },
            &if test {
                format!(
                    "{} at {} is restored as {} on {}, with its network disconnected, booted, \
                     checked, and destroyed.",
                    vm_name(&plan.manifest),
                    when(plan.pit.taken_s),
                    plan.name,
                    cx.host.this_host(),
                )
            } else {
                format!(
                    "{} at {} becomes {} on {}{}.",
                    vm_name(&plan.manifest),
                    when(plan.pit.taken_s),
                    plan.name,
                    cx.host.this_host(),
                    if plan.isolate {
                        ", with its network disconnected"
                    } else {
                        ""
                    }
                )
            },
            Severity::Warning,
            None,
            details,
        )
        .await?;
    if !confirmed {
        return Err(Stop::Cancelled);
    }

    let restore_uuid = Uuid::new_v4();
    let job = restore_uuid.simple().to_string();
    let mut record = record_restore(
        cx,
        &RestoreRecord {
            schema_version: RECORD_SCHEMA,
            record_type: String::new(),
            revision: 0,
            restore_uuid,
            vm_uuid: plan.pit.vm_uuid,
            pit_uuid: plan.pit.pit_uuid,
            branch_uuid: plan.pit.branch_uuid,
            appliance_uuid: plan.appliance.appliance_uuid,
            mode: if test { Mode::Test } else { Mode::NewVm },
            host: cx.host.this_host().to_string(),
            vmid: None,
            name: Some(plan.name.clone()),
            methods: plan
                .volumes
                .iter()
                .map(|p| (p.volume.slot.clone(), p.method.number()))
                .collect(),
            started_at_ms: (cx.now_ms)(),
            finished_at_ms: None,
            error: None,
            ended_at_ms: None,
            replaced: BTreeMap::new(),
            test: None,
            manifest: Some(plan.pit.manifest.clone()),
        },
    );
    let outcome = carry_out(cx, &plan, &job).await;
    match &outcome {
        Ok(restored) => {
            record.vmid = Some(restored.vmid);
            record.finished_at_ms = Some((cx.now_ms)());
            if test {
                record.test = Some(TestOutcome {
                    booted: restored.booted,
                    agent: restored.agent,
                    agent_after_s: restored.agent_after_s,
                });
                cx.flow
                    .phase("destroy", "Destroy the scratch VM", PhaseStatus::Running);
                match cx.host.destroy_vm(restored.vmid).await {
                    Ok(()) => {
                        record.ended_at_ms = Some((cx.now_ms)());
                        cx.flow
                            .phase("destroy", "Destroy the scratch VM", PhaseStatus::Complete);
                    }
                    Err(e) => {
                        cx.flow.warning(format!(
                            "Could not destroy the scratch VM {}: {e}. Remove it by hand; \
                             until then it keeps what it was restored from.",
                            restored.vmid
                        ));
                        cx.flow
                            .phase("destroy", "Destroy the scratch VM", PhaseStatus::Failed);
                    }
                }
            }
        }
        Err(Stop::Failed { message, .. }) => {
            record.error = Some(message.clone());
            record.ended_at_ms = Some((cx.now_ms)());
        }
        Err(Stop::Cancelled) => {
            record.error = Some("cancelled".into());
            record.ended_at_ms = Some((cx.now_ms)());
        }
    }
    record_restore(cx, &record);
    if let Ok(restored) = &outcome {
        cx.flow.result(json!({
            "vmid": restored.vmid,
            "name": restored.name,
            "host": cx.host.this_host().to_string(),
            "booted": restored.booted,
            "agent": restored.agent,
            "agent_after_s": restored.agent_after_s,
            "destroyed": test && record.ended_at_ms.is_some(),
            "methods": record.methods,
        }));
    }
    outcome
}

fn translate(plan: &Plan, vmid: u32) -> Result<VmConfig, Stop> {
    let volumes: BTreeMap<String, String> = plan
        .volumes
        .iter()
        .map(|p| {
            (
                p.volume.slot.clone(),
                format!("{}:vm-{vmid}-disk-x", p.storage.id),
            )
        })
        .collect();
    restore::translate(
        &plan.manifest,
        &plan.name,
        &volumes,
        plan.isolate,
        Uuid::new_v4(),
    )
    .map_err(Stop::failed)
}

pub(super) async fn carry_out(cx: &Context<'_>, plan: &Plan, job: &str) -> Result<Restored, Stop> {
    let pools: BTreeSet<String> = plan
        .volumes
        .iter()
        .map(|p| p.storage.pool().to_string())
        .collect();
    let mut placed: Vec<String> = Vec::new();
    let mut datasets = BTreeMap::new();
    let mut vmid = None;
    let result = async {
        cx.flow
            .phase_with(RECEIVE_PHASE.0, RECEIVE_PHASE.1, PhaseStatus::Running, true);
        for pool in &pools {
            cx.host
                .staging_parent(pool, job)
                .await
                .map_err(Stop::failed)?;
        }
        let mut staged = Vec::new();
        for planned in &plan.volumes {
            let target = staging(planned.storage.pool(), job, &planned.volume.volume_uuid);
            bring(cx, plan, planned, &target).await?;
            staged.push(target);
        }
        cx.flow.phase_with(
            RECEIVE_PHASE.0,
            RECEIVE_PHASE.1,
            PhaseStatus::Complete,
            true,
        );

        cx.flow
            .phase_with(CREATE_PHASE.0, CREATE_PHASE.1, PhaseStatus::Running, false);
        let id = match plan.vmid {
            Some(id) => id,
            None => cx.host.next_vmid().await.map_err(Stop::failed)?,
        };
        let mut volumes = BTreeMap::new();
        for (planned, from) in plan.volumes.iter().zip(&staged) {
            let taken = cx
                .host
                .children(&planned.storage.dataset)
                .await
                .map_err(Stop::failed)?;
            let mut taken = taken;
            taken.extend(
                placed
                    .iter()
                    .filter_map(|p| p.rsplit('/').next().map(str::to_string)),
            );
            let disk = disk_name(id, &taken);
            let to = format!("{}/{disk}", planned.storage.dataset);
            cx.host.place(from, &to).await.map_err(Stop::failed)?;
            datasets.insert(planned.volume.slot.clone(), to.clone());
            placed.push(to);
            volumes.insert(
                planned.volume.slot.clone(),
                format!("{}:{disk}", planned.storage.id),
            );
        }
        let config = restore::translate(
            &plan.manifest,
            &plan.name,
            &volumes,
            plan.isolate,
            Uuid::new_v4(),
        )
        .map_err(Stop::failed)?;
        cx.host
            .create_vm(id, &config.options)
            .await
            .map_err(Stop::failed)?;
        vmid = Some(id);
        for (slot, value) in &config.disks {
            cx.host
                .set_vm(id, slot, value)
                .await
                .map_err(Stop::failed)?;
        }
        cx.flow
            .phase_with(CREATE_PHASE.0, CREATE_PHASE.1, PhaseStatus::Complete, true);
        Ok::<u32, Stop>(id)
    }
    .await;
    for pool in &pools {
        let parent = format!("{pool}/bmac-restore/{job}");
        if let Err(e) = cx.host.destroy_tree(&parent).await {
            cx.flow.warning(format!("Could not remove {parent}: {e}"));
        }
    }
    let id = match result {
        Ok(id) => id,
        Err(stop) => {
            if let Some(id) = vmid
                && let Err(e) = cx.host.destroy_vm(id).await
            {
                cx.flow.warning(format!("Could not remove VM {id}: {e}"));
            }
            for dataset in &placed {
                let _ = cx.host.destroy_tree(dataset).await;
            }
            return Err(stop);
        }
    };
    cx.flow.info(format!(
        "{} is VM {id} on {}.",
        plan.name,
        cx.host.this_host()
    ));
    let mut booted = false;
    let mut agent = false;
    let mut agent_after_s = None;
    if plan.start {
        cx.flow.phase(
            "boot",
            "Start the VM and check its guest agent",
            PhaseStatus::Running,
        );
        match cx.host.start_vm(id).await {
            Ok(()) => booted = true,
            Err(e) if plan.test => cx.flow.warning(format!("The scratch VM didn't start: {e}")),
            Err(e) => return Err(Stop::failed(e)),
        }
        let started = Instant::now();
        let deadline = started + cx.agent_wait;
        while booted && Instant::now() < deadline {
            if cx.host.agent_answers(id).await {
                agent = true;
                agent_after_s = Some(started.elapsed().as_secs());
                break;
            }
            tokio::time::sleep(cx.poll).await;
        }
        if agent {
            cx.flow.info("The guest agent answers.");
        } else if booted {
            cx.flow.warning(format!(
                "The guest agent didn't answer within {} s; the VM may still be booting, or it \
                 has no agent.",
                cx.agent_wait.as_secs()
            ));
        }
        cx.flow.phase(
            "boot",
            "Start the VM and check its guest agent",
            PhaseStatus::Complete,
        );
    }
    Ok(Restored {
        vmid: id,
        name: plan.name.clone(),
        booted,
        agent,
        agent_after_s,
        datasets,
    })
}

// The real host and appliance.

pub struct RealHost<'a> {
    pub runner: &'a dyn CommandRunner,
    pub node: HostSlot,
}

impl RealHost<'_> {
    async fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let command = Command::new(program)
            .args(args.iter().map(|a| a.to_string()))
            .timeout(Duration::from_secs(600));
        self.runner
            .run(command)
            .await
            .map(|o| o.stdout_text())
            .map_err(|e| {
                format!(
                    "{program} {}: {e}",
                    args.first().copied().unwrap_or_default()
                )
            })
    }

    /// Exit 1 means it doesn't exist.
    async fn maybe(&self, args: &[&str]) -> Result<Option<String>, String> {
        let command = Command::new(ZFS)
            .args(args.iter().map(|a| a.to_string()))
            .expect_codes(&[0, 1])
            .timeout(Duration::from_secs(120));
        let output = self
            .runner
            .run(command)
            .await
            .map_err(|e| format!("zfs {}: {e}", args.first().copied().unwrap_or_default()))?;
        Ok((output.code() == Some(0)).then(|| output.stdout_text()))
    }

    async fn value(&self, property: &str, name: &str) -> Result<Option<String>, String> {
        Ok(self
            .maybe(&["get", "-Hp", "-o", "value", property, name])
            .await?
            .map(|t| t.trim().to_string()))
    }
}

impl RestoreHost for RealHost<'_> {
    fn this_host(&self) -> HostSlot {
        self.node
    }

    fn storages(&self) -> BoxFuture<'_, Result<Vec<Storage>, String>> {
        Box::pin(async move {
            let cfg =
                std::fs::read_to_string(STORAGE_CFG).map_err(|e| format!("{STORAGE_CFG}: {e}"))?;
            Ok(parse_storages(&cfg, &self.node.to_string()))
        })
    }

    fn snapshots(&self, pool: &str) -> BoxFuture<'_, Result<Vec<LocalSnapshot>, String>> {
        let pool = pool.to_string();
        Box::pin(async move {
            let text = self
                .run(
                    ZFS,
                    &[
                        "list",
                        "-Hp",
                        "-t",
                        "snapshot",
                        "-o",
                        "name,guid",
                        "-r",
                        &pool,
                    ],
                )
                .await?;
            Ok(text
                .lines()
                .filter_map(|l| {
                    let (name, guid) = l.split_once('\t')?;
                    Some(LocalSnapshot {
                        name: name.to_string(),
                        guid: guid.trim().parse().ok()?,
                    })
                })
                .collect())
        })
    }

    fn available(&self, dataset: &str) -> BoxFuture<'_, Result<u64, String>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            self.value("available", &dataset)
                .await?
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| format!("{dataset} doesn't exist"))
        })
    }

    fn children(&self, parent: &str) -> BoxFuture<'_, Result<BTreeSet<String>, String>> {
        let parent = parent.to_string();
        Box::pin(async move {
            let text = self
                .maybe(&["list", "-H", "-o", "name", "-d", "1", &parent])
                .await?
                .unwrap_or_default();
            Ok(text
                .lines()
                .filter(|l| *l != parent)
                .filter_map(|l| l.rsplit('/').next().map(str::to_string))
                .collect())
        })
    }

    fn bridges(&self) -> BoxFuture<'_, Result<BTreeSet<String>, String>> {
        Box::pin(async move {
            let entries =
                std::fs::read_dir("/sys/class/net").map_err(|e| format!("/sys/class/net: {e}"))?;
            Ok(entries
                .flatten()
                .filter(|e| e.path().join("bridge").exists())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect())
        })
    }

    fn staging_parent(&self, pool: &str, job: &str) -> BoxFuture<'_, Result<(), String>> {
        let top = format!("{pool}/bmac-restore");
        let parent = format!("{top}/{job}");
        Box::pin(async move {
            if self.value("name", &top).await?.is_none() {
                self.run(
                    ZFS,
                    &[
                        "create",
                        "-o",
                        "mountpoint=none",
                        "-o",
                        "canmount=off",
                        &top,
                    ],
                )
                .await?;
            }
            self.run(ZFS, &["create", "-o", "canmount=off", &parent])
                .await
                .map(drop)
        })
    }

    fn clone_snapshot(&self, snapshot: &str, target: &str) -> BoxFuture<'_, Result<(), String>> {
        let (snapshot, target) = (snapshot.to_string(), target.to_string());
        Box::pin(async move {
            self.run(ZFS, &["clone", "-o", "volmode=none", &snapshot, &target])
                .await
                .map(drop)
        })
    }

    fn resume_token(&self, dataset: &str) -> BoxFuture<'_, Result<Option<String>, String>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            Ok(self
                .value("receive_resume_token", &dataset)
                .await?
                .filter(|t| t != "-" && !t.is_empty()))
        })
    }

    fn guid(&self, snapshot: &str) -> BoxFuture<'_, Result<Option<SnapshotGuid>, String>> {
        let snapshot = snapshot.to_string();
        Box::pin(async move {
            Ok(self
                .value("guid", &snapshot)
                .await?
                .and_then(|v| v.parse().ok()))
        })
    }

    fn origin_guid(&self, dataset: &str) -> BoxFuture<'_, Result<Option<SnapshotGuid>, String>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            let Some(origin) = self.value("origin", &dataset).await?.filter(|o| o != "-") else {
                return Ok(None);
            };
            Ok(self
                .value("guid", &origin)
                .await?
                .and_then(|v| v.parse().ok()))
        })
    }

    fn volsize(&self, dataset: &str) -> BoxFuture<'_, Result<u64, String>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            self.value("volsize", &dataset)
                .await?
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| format!("{dataset} isn't a volume"))
        })
    }

    fn place(&self, from: &str, to: &str) -> BoxFuture<'_, Result<(), String>> {
        let (from, to) = (from.to_string(), to.to_string());
        Box::pin(async move {
            self.run(ZFS, &["rename", &from, &to]).await?;
            self.run(ZFS, &["inherit", "volmode", &to]).await.map(drop)
        })
    }

    fn destroy_tree(&self, dataset: &str) -> BoxFuture<'_, Result<(), String>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            if self.value("name", &dataset).await?.is_some() {
                self.run(ZFS, &["destroy", "-r", &dataset]).await?;
            }
            Ok(())
        })
    }

    fn next_vmid(&self) -> BoxFuture<'_, Result<u32, String>> {
        Box::pin(async move {
            let text = self
                .run(
                    PVESH,
                    &["get", "/cluster/nextid", "--output-format", "json"],
                )
                .await?;
            text.trim()
                .trim_matches('"')
                .parse()
                .map_err(|_| format!("/cluster/nextid answered {text}"))
        })
    }

    fn create_vm(
        &self,
        vmid: u32,
        options: &[(String, String)],
    ) -> BoxFuture<'_, Result<(), String>> {
        let mut args = vec!["create".to_string(), vmid.to_string()];
        for (key, value) in options {
            args.push(format!("--{key}"));
            args.push(value.clone());
        }
        Box::pin(async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            self.run(QM, &args).await.map(drop)
        })
    }

    fn set_vm(&self, vmid: u32, key: &str, value: &str) -> BoxFuture<'_, Result<(), String>> {
        let (vmid, key, value) = (vmid.to_string(), format!("--{key}"), value.to_string());
        Box::pin(async move { self.run(QM, &["set", &vmid, &key, &value]).await.map(drop) })
    }

    fn start_vm(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move { self.run(QM, &["start", &vmid.to_string()]).await.map(drop) })
    }

    fn agent_answers(&self, vmid: u32) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            let command = Command::new(QM)
                .args([
                    "guest".to_string(),
                    "cmd".into(),
                    vmid.to_string(),
                    "ping".into(),
                ])
                .timeout(Duration::from_secs(20));
            self.runner.run(command).await.is_ok()
        })
    }

    fn destroy_vm(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let id = vmid.to_string();
            let _ = self.run(QM, &["stop", &id]).await;
            self.run(
                QM,
                &[
                    "destroy",
                    &id,
                    "--destroy-unreferenced-disks",
                    "1",
                    "--purge",
                    "1",
                ],
            )
            .await
            .map(drop)
        })
    }
}

/// The appliance over its port 8445, and `zfs receive` here.
pub struct RealTransfers;

impl Transfers for RealTransfers {
    fn granted(
        &self,
        appliance: &ApplianceRecord,
        cluster_uuid: &str,
        grant: &str,
    ) -> BoxFuture<'_, Result<GrantedIndex, String>> {
        let (appliance, cluster_uuid, grant) = (
            appliance.clone(),
            cluster_uuid.to_string(),
            grant.to_string(),
        );
        Box::pin(async move {
            ApplianceClient::for_host(&appliance)?
                .granted(&cluster_uuid, &grant)
                .await
        })
    }

    fn estimate(
        &self,
        appliance: &ApplianceRecord,
        request: &ExportRequest,
    ) -> BoxFuture<'_, Result<u64, String>> {
        let (appliance, request) = (appliance.clone(), request.clone());
        Box::pin(async move {
            ApplianceClient::for_host(&appliance)?
                .estimate(&request)
                .await
        })
    }

    fn receive(
        &self,
        appliance: &ApplianceRecord,
        request: &ExportRequest,
        target: &str,
        origin: Option<&str>,
    ) -> BoxFuture<'_, Result<u64, String>> {
        let (appliance, request) = (appliance.clone(), request.clone());
        let mut args = vec!["receive".to_string(), "-s".into(), "-u".into()];
        args.extend(["-o".to_string(), "volmode=none".into()]);
        if let Some(origin) = origin.filter(|_| request.resume_token.is_none()) {
            args.extend(["-o".to_string(), format!("origin={origin}")]);
        }
        args.push(target.to_string());
        Box::pin(async move {
            let client = ApplianceClient::for_host(&appliance)?;
            let mut child = tokio::process::Command::new(ZFS)
                .args(&args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("cannot run zfs receive: {e}"))?;
            let mut stdin = child.stdin.take().ok_or("zfs receive has no stdin")?;
            let streamed = client.export_into(&request, &mut stdin, &|_| {}).await;
            drop(stdin);
            let output = child
                .wait_with_output()
                .await
                .map_err(|e| format!("zfs receive: {e}"))?;
            let bytes = streamed?;
            if !output.status.success() {
                return Err(format!(
                    "zfs receive: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Ok(bytes)
        })
    }
}

pub async fn run(flow: &Workflow, operation_id: &str, args: &[String]) -> ExitCode {
    let test = operation_id == TEST_OPERATION;
    let spec = if test { test_operation() } else { operation() };
    flow.begin(operation_id, &spec.title, &spec.description);
    let outcome = async {
        let [host] = args else {
            return Err(Stop::failed(format!("usage: {operation_id} HOST")));
        };
        let node: HostSlot = crate::engine::listen::node_name()
            .parse()
            .map_err(|_| Stop::failed("this host's name is not a BMAC host name"))?;
        if host != &node.to_string() {
            return Err(Stop::failed(format!("this job runs on {host}, not {node}")));
        }
        let runner = bmac_exec::RealRunner;
        let cluster_uuid = crate::joins::JoinStore::system()
            .cluster()
            .map_err(|e| Stop::failed(e.message))?
            .map(|c| c.cluster_uuid)
            .ok_or_else(|| Stop::failed("the cluster has no identity record yet"))?;
        let now = || jiff::Timestamp::now().as_millisecond();
        let cx = Context {
            flow,
            host: &RealHost {
                runner: &runner,
                node,
            },
            transfers: &RealTransfers,
            store: &BackupStore::system(),
            cluster_uuid,
            now_ms: &now,
            agent_wait: Duration::from_secs(300),
            poll: Duration::from_secs(10),
        };
        restore(&cx, test).await.map(drop)
    }
    .await;
    flow.finish(&outcome)
}

#[cfg(test)]
#[path = "restore_tests.rs"]
pub(crate) mod tests;
