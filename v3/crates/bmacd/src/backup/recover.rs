// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `backup.recover` (`BACKUP.md`, "Recovering BMAC after losing the whole
//! cluster"): on a cluster the Deployer built new, everything of a lost
//! cluster that an appliance kept.
//!
//! 1. The job shows this host's certificate fingerprint; the appliance's
//!    administrator issues a recovery grant for it on the appliance's UI.
//! 2. The operator gives the appliance's address, its certificate
//!    fingerprint (pinned for the connection), and the grant.
//! 3. The appliance answers with the newest recovery manifest of the lost
//!    cluster and every PIT it has of it. The operator picks the VMs.
//! 4. After a typed confirmation: this cluster takes the lost cluster's ID,
//!    BMAC's cluster-wide configuration and resource identities are written
//!    back, and the appliance binds the namespace to this cluster.
//! 5. Each VM is restored on this host under its own VMID and name, its
//!    replication to the other placement hosts and its HA resource are
//!    added, and backups continue from the PIT on a new branch.
//!
//! What the manifest can't carry (secrets, DNS, Cloudflare, provider
//! networking, per-host settings) is listed as a manual prerequisite.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::time::Duration;

use bmac_backup::manifest::Manifest;
use bmac_backup::records::{ApplianceRecord, Enrollment, RECORD_SCHEMA, RestoreRecord};
use bmac_backup::restore::{Mode, human_bytes};
use bmac_core::job::{FieldType, PhaseStatus, Severity};
use bmac_core::{ApplianceAddress, HostSlot};
use bmac_exec::BoxFuture;
use bmac_proto::api::Role;
use bmac_proto::appliance::{
    GrantedIndex, PeerPit, RecoveryBind, RecoveryIndex, SourceHost, normalized_fingerprint,
};
use bmac_proto::backups::display_fingerprint;
use bmac_proto::operations::{
    CancelRule, Category, Operation, OperationKind, ResumeRule, TargetRule,
};
use serde_json::{Value, json};
use uuid::Uuid;

use super::capture::Guests;
use super::replace::{self, Job, ReplaceHost};
use super::restore::{self as job, Plan};
use crate::native::disks::{choice, field, list, text};
use crate::native::{Stop, Workflow};

pub const OPERATION: &str = "backup.recover";
const CONFIGURE_PHASE: (&str, &str) = ("configure", "Take over the lost cluster's configuration");
const REPLICATION_SCHEDULE: &str = "*/15";

/// What the manifest carries that a recovery writes back: cluster-wide
/// state only. Per-host settings, leases, live state, and the PIT catalog
/// stay behind; the new hosts and their snapshots are what they are.
pub fn written_back(path: &str) -> bool {
    if !super::recovery::restorable(path) {
        return false;
    }
    let in_dir = |dir: &str| {
        path.strip_prefix(dir)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
    };
    matches!(
        path,
        "registry/policy.json"
            | "registry/ingress/desired.json"
            | "config/cluster.json"
            | "backup/settings.json"
    ) || in_dir("registry/resources")
        || in_dir("registry/history")
        || in_dir("registry/ingress/generations")
        || in_dir("backup/policies")
        || in_dir("backup/appliances")
        || path.starts_with("schedules/")
}

pub const PREREQUISITES: &[&str] = &[
    "Secrets aren't backed up: enter the Postmark token and any Tailscale auth keys again.",
    "DNS and Cloudflare records still point where the lost cluster was: update them.",
    "Provider networking (routes, failover IPs, firewalls outside the hosts) is outside BMAC.",
    "Each host's own settings came from the Deployer, not from the lost cluster.",
];

pub fn operation() -> Operation {
    Operation {
        id: OPERATION.parse().expect("a valid operation id"),
        title: "Recover from an appliance".into(),
        summary: "Rebuild a lost cluster's BMAC configuration and VMs from a backup appliance."
            .into(),
        description: "For a cluster the Deployer built new after the old one was lost. Needs a \
                      recovery grant from the appliance's administrator for this host. This \
                      cluster takes over the lost cluster's ID, configuration, and backup \
                      history; the chosen VMs are restored on this host with their own VMIDs \
                      and names, and replicated to their placement hosts."
            .into(),
        category: Category::Backups,
        kind: OperationKind::Native,
        script: None,
        min_role: Role::Admin,
        mutating: true,
        destructive: true,
        supports_dry_run: false,
        params: vec![
            serde_json::from_value(json!({
                "id": "host",
                "label": "Host",
                "type": "host",
                "help": "The host the VMs are restored on.",
                "required": true,
            }))
            .expect("a valid parameter"),
        ],
        locks: vec!["restore:{host}".into()],
        target: TargetRule::Host {
            param: "host".into(),
        },
        cancel: CancelRule::Phases {
            not_during: vec![CONFIGURE_PHASE.0.into()],
        },
        resume: ResumeRule::Never,
        notes: Some("New in v3.".into()),
    }
}

/// What a recovery asks of this cluster, beyond a restore's.
pub trait RecoverHost: Send + Sync {
    /// SHA-256 of this host's Proxmox certificate, which the recovery
    /// grant names.
    fn host_fingerprint(&self) -> Result<String, String>;
    /// SHA-256 of this cluster's Proxmox CA.
    fn ca_fingerprint(&self) -> Result<String, String>;
    /// This cluster's own ID now.
    fn cluster_uuid(&self) -> Result<Option<String>, String>;
    /// Where an appliance reaches each host's `bmac-source`.
    fn source_hosts(&self) -> Result<Vec<SourceHost>, String>;
    fn recovery_index<'a>(
        &'a self,
        appliance: &'a ApplianceRecord,
        grant: &'a str,
    ) -> BoxFuture<'a, Result<RecoveryIndex, String>>;
    fn bind<'a>(
        &'a self,
        appliance: &'a ApplianceRecord,
        bind: &'a RecoveryBind,
    ) -> BoxFuture<'a, Result<(), String>>;
    /// Whether the file exists under `/etc/pve/priv/bmac/`.
    fn exists(&self, path: &str) -> bool;
    /// Writes a file under `/etc/pve/priv/bmac/`.
    fn write(&self, path: &str, text: &str) -> Result<(), String>;
    /// This cluster takes `uuid` as its ID.
    fn take_cluster_uuid(&self, uuid: &str) -> Result<(), String>;
    fn vmid_taken(&self, vmid: u32) -> BoxFuture<'_, Result<bool, String>>;
    /// `ha-manager add vm:ID --state started`.
    fn ha_add(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>>;
}

pub struct Context<'a> {
    pub restore: job::Context<'a>,
    pub replace: &'a dyn ReplaceHost,
    pub host: &'a dyn RecoverHost,
    pub guests: &'a dyn Guests,
}

fn failed(message: impl Into<String>) -> Stop {
    Stop::failed(message)
}

/// One VM the appliance can bring back: its newest PIT.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub pit: PeerPit,
    pub manifest: Manifest,
}

impl Candidate {
    fn name(&self) -> String {
        job::vm_name(&self.manifest)
    }
}

/// The newest PIT of each VM, by name.
pub fn candidates(index: &RecoveryIndex) -> Result<Vec<Candidate>, String> {
    let mut newest: BTreeMap<Uuid, &PeerPit> = BTreeMap::new();
    for pit in &index.pits {
        let entry = newest.entry(pit.vm_uuid).or_insert(pit);
        if (pit.taken_s, pit.pit_uuid) > (entry.taken_s, entry.pit_uuid) {
            *entry = pit;
        }
    }
    let mut out = Vec::new();
    for pit in newest.values() {
        let manifest = Manifest::open(&pit.manifest)
            .map_err(|e| format!("PIT {}'s manifest: {e}", pit.name))?;
        if manifest.cluster_uuid != index.cluster_uuid
            || manifest.pit_uuid != pit.pit_uuid.to_string()
        {
            return Err(format!(
                "PIT {}'s manifest is for another PIT or cluster",
                pit.name
            ));
        }
        out.push(Candidate {
            pit: (*pit).clone(),
            manifest,
        });
    }
    out.sort_by_key(Candidate::name);
    Ok(out)
}

/// An index for one PIT, with the earlier PITs of its VM as bases.
fn granted_for(index: &RecoveryIndex, pit: &PeerPit) -> GrantedIndex {
    GrantedIndex {
        appliance_uuid: index.appliance_uuid,
        grant_id: index.grant_id.clone(),
        expires_at_ms: index.expires_at_ms,
        pits: vec![pit.clone()],
        bases: index
            .pits
            .iter()
            .filter(|p| p.vm_uuid == pit.vm_uuid && p.taken_s < pit.taken_s)
            .map(|p| bmac_proto::appliance::GrantedBase {
                vm_uuid: p.vm_uuid,
                branch_uuid: p.branch_uuid,
                pit_uuid: p.pit_uuid,
                parent: p.parent,
                taken_s: p.taken_s,
                volumes: p.volumes.clone(),
            })
            .collect(),
    }
}

fn appliance_record(uuid: Uuid, address: ApplianceAddress, fingerprint: &str) -> ApplianceRecord {
    ApplianceRecord {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 0,
        appliance_uuid: uuid,
        name: "appliance".into(),
        fingerprint: fingerprint.to_string(),
        address: Some(address),
        enrollment: Enrollment::Approved,
        enrolled_at_ms: 0,
        paused: false,
        retired: false,
        last_checkin_ms: None,
        version: None,
        locked: false,
        progress: Default::default(),
        heads: BTreeMap::new(),
        index_revision: 0,
        scrub: None,
    }
}

/// Asks for the appliance and the grant until the appliance answers.
async fn connect(cx: &Context<'_>) -> Result<(ApplianceRecord, String, RecoveryIndex), Stop> {
    let flow = cx.restore.flow;
    let mine = cx.host.host_fingerprint().map_err(failed)?;
    flow.info(format!(
        "This host's certificate fingerprint (SHA-256) is {}. On the appliance's UI, open \
         Grants, then Grant a recovery, and give it this fingerprint.",
        display_fingerprint(&mine)
    ));
    let mut problem: Option<String> = None;
    for _ in 0..3 {
        let mut address = field("address", FieldType::String, "Appliance address");
        address.help = Some("Its IP address or DNS name, as its port 8445 answers.".into());
        let mut pin = field(
            "fingerprint",
            FieldType::String,
            "Appliance certificate fingerprint",
        );
        pin.help = Some(
            "SHA-256, as the appliance's console and its UI's Settings page show it. Compare \
             it in your browser too: open the appliance's UI and check that its certificate \
             has the same fingerprint."
                .into(),
        );
        let mut grant = field("grant", FieldType::Password, "Recovery grant");
        grant.help = Some("Shown once on the appliance's UI when it was issued.".into());
        let values = flow
            .input_group(
                "The appliance to recover from",
                problem.as_deref(),
                vec![address, pin, grant],
                |v| {
                    let mut errors = BTreeMap::new();
                    if let Err(e) = text(v, "address").parse::<ApplianceAddress>() {
                        errors.insert("address".into(), e.to_string());
                    }
                    if normalized_fingerprint(&text(v, "fingerprint")).is_none() {
                        errors.insert("fingerprint".into(), "A SHA-256 fingerprint.".into());
                    }
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
        let address = text(&values, "address")
            .parse::<ApplianceAddress>()
            .map_err(|e| failed(e.to_string()))?;
        let fingerprint = normalized_fingerprint(&text(&values, "fingerprint"))
            .ok_or_else(|| failed("the fingerprint isn't a SHA-256"))?;
        let grant = text(&values, "grant");
        let appliance = appliance_record(Uuid::nil(), address, &fingerprint);
        match cx.host.recovery_index(&appliance, &grant).await {
            Ok(index) => {
                flow.info(format!(
                    "The appliance presented the certificate {}. If your browser showed \
                     another one, stop here: something stands between you and it.",
                    display_fingerprint(&fingerprint)
                ));
                let appliance = ApplianceRecord {
                    appliance_uuid: index.appliance_uuid,
                    ..appliance
                };
                return Ok((appliance, grant, index));
            }
            Err(e) => {
                flow.warning(e.clone());
                problem = Some(e);
            }
        }
    }
    Err(failed(problem.unwrap_or_else(|| {
        "the appliance didn't accept the grant".into()
    })))
}

async fn choose(
    cx: &Context<'_>,
    index: &RecoveryIndex,
    found: &[Candidate],
) -> Result<(Vec<Candidate>, bool), Stop> {
    let mut vms = field("vms", FieldType::Multiselect, "VMs to restore");
    vms.options = found
        .iter()
        .map(|c| {
            choice(
                &c.pit.vm_uuid.to_string(),
                &format!("{} (VM {})", c.name(), c.manifest.vmid),
                Some(format!(
                    "{}, {} volumes",
                    job::when(c.pit.taken_s),
                    c.manifest.volumes.len()
                )),
            )
        })
        .collect();
    vms.default = Some(json!(
        found
            .iter()
            .map(|c| c.pit.vm_uuid.to_string())
            .collect::<Vec<_>>()
    ));
    let mut configure = field(
        "configure",
        FieldType::Boolean,
        "Take over the lost cluster's configuration",
    );
    configure.default = Some(json!(index.manifest.is_some()));
    configure.help = Some(
        "Its ID, cluster-wide settings, registry, schedules, backup policies, and appliances. \
         Without it, the VMs come back with new backup histories."
            .into(),
    );
    let summary = match &index.manifest {
        Some(m) => format!(
            "The appliance has {} PITs of {} ({}); its recovery manifest is from {} on {}.",
            index.pits.len(),
            index.cluster_name,
            index.cluster_uuid,
            job::when(m.generated_at_ms.div_euclid(1000)),
            m.host
        ),
        None => format!(
            "The appliance has {} PITs of {} ({}), and no recovery manifest of it: only the \
             VMs can come back.",
            index.pits.len(),
            index.cluster_name,
            index.cluster_uuid
        ),
    };
    let has_manifest = index.manifest.is_some();
    let values = cx
        .restore
        .flow
        .input_group(
            "What to recover",
            Some(&summary),
            vec![vms, configure],
            |v| {
                if !has_manifest && v.get("configure").and_then(Value::as_bool) == Some(true) {
                    return Err(BTreeMap::from([(
                        "configure".into(),
                        "The appliance has no recovery manifest of this cluster.".into(),
                    )]));
                }
                Ok(())
            },
        )
        .await?;
    let chosen: BTreeSet<String> = list(&values, "vms").into_iter().collect();
    let configure = values
        .get("configure")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((
        found
            .iter()
            .filter(|c| chosen.contains(&c.pit.vm_uuid.to_string()))
            .cloned()
            .collect(),
        configure,
    ))
}

/// The configuration files to write back: `(path, text, replaces)`.
fn configuration<'a>(cx: &Context<'_>, index: &'a RecoveryIndex) -> Vec<(&'a str, &'a str, bool)> {
    index
        .manifest
        .iter()
        .flat_map(|m| m.files.iter())
        .filter(|(path, _)| written_back(path))
        .map(|(path, text)| (path.as_str(), text.as_str(), cx.host.exists(path)))
        .collect()
}

pub async fn recover(cx: &Context<'_>) -> Result<Value, Stop> {
    let flow = cx.restore.flow;
    if !cx
        .restore
        .store
        .policies()
        .map_err(|e| failed(e.message))?
        .is_empty()
    {
        return Err(failed(
            "This cluster already has backup policies. A recovery is for a cluster built new \
             in place of a lost one.",
        ));
    }
    flow.phase("connect", "Reach the appliance", PhaseStatus::Running);
    let (appliance, grant, index) = connect(cx).await?;
    let found = candidates(&index).map_err(failed)?;
    flow.phase("connect", "Reach the appliance", PhaseStatus::Complete);
    flow.phase("plan", "Choose and plan", PhaseStatus::Running);
    let (chosen, configure) = choose(cx, &index, &found).await?;
    if chosen.is_empty() && !configure {
        return Err(Stop::Cancelled);
    }
    // Exports are of the lost cluster's namespace.
    let restore_cx = job::Context {
        flow,
        host: cx.restore.host,
        transfers: cx.restore.transfers,
        store: cx.restore.store,
        cluster_uuid: index.cluster_uuid.clone(),
        now_ms: cx.restore.now_ms,
        agent_wait: cx.restore.agent_wait,
        poll: cx.restore.poll,
    };
    let mut plans = Vec::new();
    for c in &chosen {
        if cx.host.vmid_taken(c.manifest.vmid).await.map_err(failed)? {
            return Err(failed(format!(
                "VM {} already exists here; {} can't come back under its own VMID",
                c.manifest.vmid,
                c.name()
            )));
        }
        let mut plan = Plan {
            appliance: appliance.clone(),
            grant: grant.clone(),
            pit: c.pit.clone(),
            manifest: c.manifest.clone(),
            name: c.name(),
            isolate: false,
            start: false,
            test: false,
            vmid: Some(c.manifest.vmid),
            volumes: Vec::new(),
        };
        let granted = granted_for(&index, &c.pit);
        plan.volumes = job::preflight(&restore_cx, &plan, &granted).await?;
        plans.push(plan);
    }
    let files = if configure {
        configuration(cx, &index)
    } else {
        Vec::new()
    };
    flow.phase("plan", "Choose and plan", PhaseStatus::Complete);

    let mut details: Vec<String> = Vec::new();
    if configure {
        let replaces = files.iter().filter(|(_, _, r)| *r).count();
        details.push(format!(
            "This cluster takes the ID {} and {} configuration files ({} replace this \
             cluster's own).",
            index.cluster_uuid,
            files.len(),
            replaces
        ));
        details.push(format!(
            "The appliance's backups of {} continue on this cluster.",
            index.cluster_name
        ));
    }
    for plan in &plans {
        let total: u64 = plan.volumes.iter().map(|p| p.bytes).sum();
        details.push(format!(
            "{} comes back as VM {} on {} from its PIT of {}: {} to transfer.",
            plan.name,
            plan.manifest.vmid,
            cx.restore.host.this_host(),
            job::when(plan.pit.taken_s),
            human_bytes(total)
        ));
    }
    details.extend(PREREQUISITES.iter().map(|p| format!("Then by hand: {p}")));
    let typed = if index.cluster_name.is_empty() {
        index.cluster_uuid.clone()
    } else {
        index.cluster_name.clone()
    };
    if !flow
        .confirm(
            "Recover the lost cluster",
            &format!(
                "This cluster becomes {typed} as far as BMAC and its appliances are concerned."
            ),
            Severity::Critical,
            Some(&typed),
            details,
        )
        .await?
    {
        return Err(Stop::Cancelled);
    }

    let mut bound = false;
    if configure {
        flow.phase_with(
            CONFIGURE_PHASE.0,
            CONFIGURE_PHASE.1,
            PhaseStatus::Running,
            false,
        );
        take_over(cx, &appliance, &grant, &index, &files).await?;
        bound = true;
        flow.phase_with(
            CONFIGURE_PHASE.0,
            CONFIGURE_PHASE.1,
            PhaseStatus::Complete,
            true,
        );
    }
    let mut restored = Vec::new();
    let mut problems = Vec::new();
    for plan in &plans {
        flow.check()?;
        let phase = format!("vm-{}", plan.manifest.vmid);
        let label = format!("Restore {}", plan.name);
        flow.phase(&phase, &label, PhaseStatus::Running);
        match bring_back(cx, &restore_cx, plan, bound).await {
            Ok(value) => {
                flow.phase(&phase, &label, PhaseStatus::Complete);
                restored.push(value);
            }
            Err(Stop::Cancelled) => return Err(Stop::Cancelled),
            Err(Stop::Failed { message, .. }) => {
                flow.warning(format!("{}: {message}", plan.name));
                flow.phase(&phase, &label, PhaseStatus::Failed);
                problems.push(plan.name.clone());
            }
        }
    }
    for p in PREREQUISITES {
        flow.warning(format!("By hand: {p}"));
    }
    let result = json!({
        "cluster_uuid": index.cluster_uuid,
        "configured": bound,
        "restored": restored,
        "failed": problems,
    });
    if !problems.is_empty() {
        return Err(Stop::Failed {
            message: format!("{} didn't come back", problems.join(", ")),
            details: Some(result.to_string()),
        });
    }
    flow.result(result.clone());
    Ok(result)
}

async fn take_over(
    cx: &Context<'_>,
    appliance: &ApplianceRecord,
    grant: &str,
    index: &RecoveryIndex,
    files: &[(&str, &str, bool)],
) -> Result<(), Stop> {
    let flow = cx.restore.flow;
    let mine = cx.host.cluster_uuid().map_err(failed)?;
    if mine.as_deref() != Some(index.cluster_uuid.as_str()) {
        cx.host
            .take_cluster_uuid(&index.cluster_uuid)
            .map_err(failed)?;
        flow.info(format!(
            "This cluster's ID is {} now (it was {}).",
            index.cluster_uuid,
            mine.unwrap_or_else(|| "unset".into())
        ));
    }
    for (path, text, _) in files {
        cx.host
            .write(path, text)
            .map_err(|e| failed(format!("{path}: {e}")))?;
    }
    flow.info(format!(
        "Wrote back {} configuration files of the lost cluster.",
        files.len()
    ));
    let bind = RecoveryBind {
        cluster_uuid: index.cluster_uuid.clone(),
        grant: grant.to_string(),
        cluster_name: index.cluster_name.clone(),
        proxmox_ca_fingerprint: cx.host.ca_fingerprint().map_err(failed)?,
        hosts: cx.host.source_hosts().map_err(failed)?,
    };
    if bind.hosts.is_empty() {
        return Err(failed(
            "no host here has an address an appliance could reach",
        ));
    }
    cx.host.bind(appliance, &bind).await.map_err(failed)?;
    flow.info(
        "The appliance checks in with this cluster from now on. Any other appliance of the \
         lost cluster needs its own recovery grant and binding.",
    );
    Ok(())
}

async fn bring_back(
    cx: &Context<'_>,
    restore_cx: &job::Context<'_>,
    plan: &Plan,
    continues: bool,
) -> Result<Value, Stop> {
    let flow = cx.restore.flow;
    let restore_uuid = Uuid::new_v4();
    let me = cx.restore.host.this_host();
    let mut record = job::record_restore(
        restore_cx,
        &RestoreRecord {
            schema_version: RECORD_SCHEMA,
            record_type: String::new(),
            revision: 0,
            restore_uuid,
            vm_uuid: plan.pit.vm_uuid,
            pit_uuid: plan.pit.pit_uuid,
            branch_uuid: plan.pit.branch_uuid,
            appliance_uuid: plan.appliance.appliance_uuid,
            mode: Mode::Recovery,
            host: me.to_string(),
            vmid: Some(plan.manifest.vmid),
            name: Some(plan.name.clone()),
            methods: plan
                .volumes
                .iter()
                .map(|p| (p.volume.slot.clone(), p.method.number()))
                .collect(),
            started_at_ms: (cx.restore.now_ms)(),
            finished_at_ms: None,
            error: None,
            ended_at_ms: None,
            replaced: BTreeMap::new(),
            test: None,
            manifest: Some(plan.pit.manifest.clone()),
        },
    );
    let outcome = async {
        let restored = job::carry_out(restore_cx, plan, &restore_uuid.simple().to_string()).await?;
        let vmid = restored.vmid;
        let placement = cx
            .guests
            .by_uuid(&plan.pit.vm_uuid)
            .ok()
            .flatten()
            .map(|p| p.placement)
            .unwrap_or_default();
        let targets: Vec<HostSlot> = placement.into_iter().filter(|h| *h != me).collect();
        for (n, target) in targets.iter().enumerate() {
            let job = Job {
                id: format!("{vmid}-{n}"),
                target: *target,
                schedule: REPLICATION_SCHEDULE.into(),
                rate: None,
                comment: Some(format!("{} replica", plan.name)),
            };
            cx.replace.create_job(&job).await.map_err(failed)?;
        }
        if targets.is_empty() {
            flow.warning(format!(
                "{} has no other placement host in the registry; it isn't replicated",
                plan.name
            ));
        }
        cx.host.ha_add(vmid).await.map_err(failed)?;
        Ok::<_, Stop>((restored, targets))
    }
    .await;
    match &outcome {
        Ok(_) if !continues => {
            record.finished_at_ms = Some((cx.restore.now_ms)());
        }
        Ok((restored, _)) => {
            record.finished_at_ms = Some((cx.restore.now_ms)());
            match replace::continue_from(
                cx.restore.store,
                plan.pit.vm_uuid,
                &plan.pit.manifest,
                &restored.datasets,
            ) {
                Ok(branch) => flow.info(format!(
                    "{}: backups continue from the PIT on a new branch, {branch}.",
                    plan.name
                )),
                Err(e) => flow.warning(format!(
                    "{}: backups couldn't be set to continue from the PIT: {e}",
                    plan.name
                )),
            }
        }
        Err(Stop::Failed { message, .. }) => {
            record.error = Some(message.clone());
            record.ended_at_ms = Some((cx.restore.now_ms)());
        }
        Err(Stop::Cancelled) => {
            record.error = Some("cancelled".into());
            record.ended_at_ms = Some((cx.restore.now_ms)());
        }
    }
    job::record_restore(restore_cx, &record);
    let (restored, targets) = outcome?;
    Ok(json!({
        "name": plan.name,
        "vmid": restored.vmid,
        "host": me.to_string(),
        "replicas": targets.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "methods": record.methods,
    }))
}

// The real cluster.

pub struct RealHost<'a> {
    pub runner: &'a dyn bmac_exec::CommandRunner,
    pub pmxcfs: bmac_pmxcfs::Pmxcfs,
    pub joins: crate::joins::JoinStore,
}

const HOST_CERT: &str = "/etc/pve/local/pve-ssl.pem";
const QM: &str = "/usr/sbin/qm";
const HA_MANAGER: &str = "/usr/sbin/ha-manager";

impl RealHost<'_> {
    async fn run(&self, program: &str, args: &[&str], codes: &[i32]) -> Result<i32, String> {
        let command = bmac_exec::Command::new(program)
            .args(args.iter().map(|a| a.to_string()))
            .expect_codes(codes)
            .timeout(Duration::from_secs(120));
        let output = self
            .runner
            .run(command)
            .await
            .map_err(|e| format!("{program}: {e}"))?;
        Ok(output.code().unwrap_or(-1))
    }
}

fn fingerprint_of(path: &str) -> Result<String, String> {
    let pem = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    crate::joins::ca_fingerprint(&pem)
        .ok()
        .and_then(|f| normalized_fingerprint(&f))
        .ok_or_else(|| format!("{path} has no certificate"))
}

impl RecoverHost for RealHost<'_> {
    fn host_fingerprint(&self) -> Result<String, String> {
        fingerprint_of(HOST_CERT)
    }

    fn ca_fingerprint(&self) -> Result<String, String> {
        fingerprint_of(crate::joins::CLUSTER_CA)
    }

    fn cluster_uuid(&self) -> Result<Option<String>, String> {
        self.joins
            .cluster()
            .map(|c| c.map(|c| c.cluster_uuid))
            .map_err(|e| e.message)
    }

    fn source_hosts(&self) -> Result<Vec<SourceHost>, String> {
        super::enrollment::source_hosts(
            &crate::settings::Settings::system(),
            &crate::identity::Identities::system(),
        )
        .map_err(|e| e.message)
    }

    fn recovery_index<'a>(
        &'a self,
        appliance: &'a ApplianceRecord,
        grant: &'a str,
    ) -> BoxFuture<'a, Result<RecoveryIndex, String>> {
        Box::pin(async move {
            super::exports::ApplianceClient::for_host(appliance)?
                .recovery_index(grant)
                .await
        })
    }

    fn bind<'a>(
        &'a self,
        appliance: &'a ApplianceRecord,
        bind: &'a RecoveryBind,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            super::exports::ApplianceClient::for_host(appliance)?
                .recovery_bind(bind)
                .await
        })
    }

    fn exists(&self, path: &str) -> bool {
        self.pmxcfs
            .exists(&std::path::Path::new(super::recovery::ROOT).join(path))
    }

    fn write(&self, path: &str, text: &str) -> Result<(), String> {
        if !written_back(path) {
            return Err("not a path a recovery writes".into());
        }
        let value: Value =
            serde_json::from_str(text).map_err(|e| format!("not a JSON record: {e}"))?;
        let full = std::path::Path::new(super::recovery::ROOT).join(path);
        self.pmxcfs
            .write_json(&full, &value)
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn take_cluster_uuid(&self, uuid: &str) -> Result<(), String> {
        self.joins.replace_cluster_uuid(uuid).map_err(|e| e.message)
    }

    fn vmid_taken(&self, vmid: u32) -> BoxFuture<'_, Result<bool, String>> {
        Box::pin(async move {
            let code = self
                .run(QM, &["config", &vmid.to_string()], &[0, 2])
                .await?;
            Ok(code == 0)
        })
    }

    fn ha_add(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.run(
                HA_MANAGER,
                &["add", &format!("vm:{vmid}"), "--state", "started"],
                &[0],
            )
            .await
            .map(drop)
        })
    }
}

/// `bmacd native backup.recover HOST`.
pub async fn run(flow: &Workflow, args: &[String]) -> ExitCode {
    let spec = operation();
    flow.begin(OPERATION, &spec.title, &spec.description);
    let outcome = async {
        let [host] = args else {
            return Err(failed("usage: backup.recover HOST"));
        };
        let node: HostSlot = crate::engine::listen::node_name()
            .parse()
            .map_err(|_| failed("this host's name is not a BMAC host name"))?;
        if host != &node.to_string() {
            return Err(failed(format!("this job runs on {host}, not {node}")));
        }
        let runner = bmac_exec::RealRunner;
        let now = || jiff::Timestamp::now().as_millisecond();
        let store = super::store::BackupStore::system();
        let restore_host = job::RealHost {
            runner: &runner,
            node,
        };
        let replace_host = replace::RealHost {
            runner: &runner,
            node,
        };
        let guests = super::capture::RegistryGuests {
            registry: bmac_registry::Registry::system(),
            runner: &runner,
            owner: format!("{OPERATION}-{node}-{}", std::process::id()),
        };
        let host = RealHost {
            runner: &runner,
            pmxcfs: bmac_pmxcfs::Pmxcfs::system(),
            joins: crate::joins::JoinStore::system(),
        };
        let cx = Context {
            restore: job::Context {
                flow,
                host: &restore_host,
                transfers: &job::RealTransfers,
                store: &store,
                cluster_uuid: String::new(),
                now_ms: &now,
                agent_wait: Duration::from_secs(300),
                poll: Duration::from_secs(10),
            },
            replace: &replace_host,
            host: &host,
            guests: &guests,
        };
        recover(&cx).await.map(drop)
    }
    .await;
    flow.finish(&outcome)
}

#[cfg(test)]
#[path = "recover_tests.rs"]
mod tests;
