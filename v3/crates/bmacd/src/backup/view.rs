// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `GET /api/v1/backups`: the Backups page's view, assembled from the backup
//! records, the duty host's `backups` observation, and each host's storage
//! observation.

use std::collections::BTreeMap;

use bmac_backup::lifetime::Offer;
use bmac_backup::records::{Lineage, PitRecord, Policy, RestoreRecord};
use bmac_proto::backups::{
    ApplianceList, BackupApplianceView, BackupVmView, BackupsView, LiveTransferView,
    LocalSpaceView, PitNodeView, PitOfferView, PolicyView, RestoreTestView, RestoreView,
    TransferView,
};
use bmac_proto::status::{BackupsObservation, StorageObservation};
use uuid::Uuid;

use crate::schedules::{Day, Recurrence};

/// Restores shown, newest first.
pub const SHOWN_RESTORES: usize = 50;
/// Transfers shown, newest first.
pub const SHOWN_TRANSFERS: usize = 200;

/// One VM's backup records.
#[derive(Debug, Clone, Default)]
pub struct VmRecords {
    pub policy: Option<Policy>,
    pub every: Option<Recurrence>,
    pub lineage: Option<Lineage>,
    /// Oldest first.
    pub pits: Vec<PitRecord>,
}

#[derive(Debug, Default)]
pub struct Inputs {
    pub now_ms: i64,
    pub appliances: Option<ApplianceList>,
    pub vms: BTreeMap<Uuid, VmRecords>,
    pub restores: Vec<RestoreRecord>,
    pub transfers: Vec<TransferView>,
    pub backups: Option<BackupsObservation>,
    pub storage: Vec<(String, StorageObservation)>,
    /// Production VMs' names and VMIDs.
    pub names: BTreeMap<Uuid, (String, u32)>,
    pub can_manage: bool,
    pub errors: Vec<String>,
    /// The cluster's time zone, which schedules run in.
    pub time_zone: Option<String>,
    pub settings: bmac_backup::settings::BackupSettings,
    pub settings_revision: u64,
}

/// An enum's name on the wire, such as `if_available`.
fn wire_name<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(serde_json::Value::Object(map)) => map
            .get("state")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn day_name(day: &Day) -> String {
    let name = wire_name(day);
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => name,
    }
}

/// Such as "every 15 minutes" or "daily at 03:00".
pub fn describe(every: &Recurrence) -> String {
    match every {
        Recurrence::Interval { minutes } if *minutes == 1 => "every minute".into(),
        Recurrence::Interval { minutes } if minutes % 60 == 0 && *minutes == 60 => "hourly".into(),
        Recurrence::Interval { minutes } if minutes % 60 == 0 => {
            format!("every {} hours", minutes / 60)
        }
        Recurrence::Interval { minutes } => format!("every {minutes} minutes"),
        Recurrence::Daily { at } => format!("daily at {at}"),
        Recurrence::Weekly { days, at } => format!(
            "{} at {at}",
            days.iter().map(day_name).collect::<Vec<_>>().join(", ")
        ),
        Recurrence::Monthly { day, at } => format!("monthly on day {day} at {at}"),
    }
}

/// [`describe`], naming the zone of a time of day.
fn describe_in(every: &Recurrence, zone: Option<&str>) -> String {
    match (every, zone) {
        (Recurrence::Interval { .. }, _) | (_, None) => describe(every),
        (_, Some(zone)) => format!("{} {zone}", describe(every)),
    }
}

fn policy_view(policy: &Policy, every: Option<&Recurrence>, zone: Option<&str>) -> PolicyView {
    PolicyView {
        revision: policy.revision,
        enabled: policy.enabled,
        appliances: policy.appliances.iter().map(Uuid::to_string).collect(),
        local_newest: policy.local_retention.newest,
        local_days: policy.local_retention.days,
        local_weeks: policy.local_retention.weeks,
        every: every
            .map(|e| describe_in(e, zone))
            .unwrap_or_else(|| "no schedule".into()),
        every_json: every
            .and_then(|e| serde_json::to_string(e).ok())
            .unwrap_or_default(),
        freeze: wire_name(&policy.freeze),
        accept_multi_pool: policy.accept_multi_pool,
        updated_by: policy.updated_by.clone(),
        updated_at_ms: policy.updated_at_ms,
    }
}

fn pit_view(pit: &PitRecord) -> PitNodeView {
    PitNodeView {
        pit_uuid: pit.pit_uuid.to_string(),
        branch_uuid: pit.branch_uuid.to_string(),
        parent: pit.parent.map(|p| p.to_string()),
        name: pit.name.to_string(),
        taken_s: pit.taken_s,
        presence: wire_name(&pit.presence),
        consistency: wire_name(&pit.consistency),
        offers: pit
            .offers
            .iter()
            .map(|(appliance, offer)| PitOfferView {
                appliance_uuid: appliance.to_string(),
                state: wire_name(offer),
                bytes: match offer {
                    Offer::Claimed { bytes, .. } => Some(*bytes),
                    _ => None,
                },
            })
            .collect(),
        release_blocked: pit.release_blocked.clone(),
    }
}

/// Whether `dataset` is the volume `key` names: the same, or `key` is its
/// path under the pool.
fn same_dataset(dataset: &str, key: &str) -> bool {
    dataset == key || dataset.ends_with(&format!("/{key}"))
}

fn local_space(
    lineage: Option<&Lineage>,
    storage: &[(String, StorageObservation)],
) -> Vec<LocalSpaceView> {
    let Some(lineage) = lineage else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (host, observation) in storage {
        let mut space = LocalSpaceView {
            host: host.clone(),
            snapshots: 0,
            bytes: 0,
        };
        for pit in &observation.pit_snapshots {
            if lineage
                .volumes
                .keys()
                .any(|key| same_dataset(&pit.dataset, key))
            {
                space.snapshots += pit.snapshots;
                space.bytes += pit.used_bytes;
            }
        }
        if space.snapshots > 0 {
            out.push(space);
        }
    }
    out
}

fn restore_view(restore: &RestoreRecord, names: &BTreeMap<Uuid, (String, u32)>) -> RestoreView {
    RestoreView {
        restore_uuid: restore.restore_uuid.to_string(),
        vm_uuid: restore.vm_uuid.to_string(),
        vm: names.get(&restore.vm_uuid).map(|(n, _)| n.clone()),
        pit_uuid: restore.pit_uuid.to_string(),
        appliance_uuid: restore.appliance_uuid.to_string(),
        mode: wire_name(&restore.mode),
        host: restore.host.clone(),
        vmid: restore.vmid,
        name: restore.name.clone(),
        methods: restore.methods.clone(),
        started_at_ms: restore.started_at_ms,
        finished_at_ms: restore.finished_at_ms,
        ended_at_ms: restore.ended_at_ms,
        error: restore.error.clone(),
        test: restore.test.as_ref().map(|t| RestoreTestView {
            booted: t.booted,
            agent: t.agent,
            agent_after_s: t.agent_after_s,
        }),
    }
}

/// Reads each VM's policy, schedule, lineage, and catalog, the restores,
/// and the transfers; what can't be read becomes an error on the page.
pub fn read(
    store: &super::store::BackupStore,
    schedules: &crate::schedules::Schedules,
    inputs: &mut Inputs,
) {
    let mut note = |what: &str, e: bmac_proto::api::ApiError| {
        inputs.errors.push(format!("{what}: {}", e.message));
    };
    let mut vms: BTreeMap<Uuid, VmRecords> = BTreeMap::new();
    match store.policies() {
        Ok(policies) => {
            for policy in policies {
                let every = match schedules.get(&policy.schedule_id) {
                    Ok(schedule) => schedule.map(|s| s.every),
                    Err(e) => {
                        note("a backup schedule", e);
                        None
                    }
                };
                let records = vms.entry(policy.vm_uuid).or_default();
                records.every = every;
                records.policy = Some(policy);
            }
        }
        Err(e) => note("the backup policies", e),
    }
    match store.cataloged_vms() {
        Ok(cataloged) => {
            for vm in cataloged {
                vms.entry(vm).or_default();
            }
        }
        Err(e) => note("the PIT catalog", e),
    }
    for (vm, records) in vms.iter_mut() {
        match store.lineage(vm) {
            Ok(lineage) => records.lineage = lineage,
            Err(e) => note("a lineage", e),
        }
        match store.pits(vm) {
            Ok(pits) => records.pits = pits,
            Err(e) => note("a PIT catalog", e),
        }
    }
    match store.restores() {
        Ok(restores) => inputs.restores = restores,
        Err(e) => note("the restores", e),
    }
    match store.transfers() {
        Ok(transfers) => inputs.transfers = transfers,
        Err(e) => note("the transfers", e),
    }
    match store.settings_record() {
        Ok(record) => {
            if let Some(r) = record {
                inputs.settings = r.settings;
                inputs.settings_revision = r.revision;
            }
        }
        Err(e) => note("the backup settings", e),
    }
    inputs.vms = vms;
}

pub fn assemble(mut inputs: Inputs) -> BackupsView {
    let statuses: BTreeMap<String, _> = inputs
        .backups
        .as_ref()
        .map(|b| {
            b.appliances
                .iter()
                .map(|a| (a.appliance_uuid.clone(), a.clone()))
                .collect()
        })
        .unwrap_or_default();
    let vm_statuses: BTreeMap<String, _> = inputs
        .backups
        .as_ref()
        .map(|b| {
            b.vms
                .iter()
                .map(|v| (v.vm_uuid.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    if let Some(backups) = &inputs.backups {
        inputs.errors.extend(backups.errors.iter().cloned());
    }
    let mut live: BTreeMap<String, Vec<LiveTransferView>> = BTreeMap::new();
    let mut vms = Vec::new();
    for (vm_uuid, records) in &inputs.vms {
        let name = inputs
            .names
            .get(vm_uuid)
            .map(|(n, _)| n.clone())
            .or_else(|| records.policy.as_ref().map(|p| p.vm.clone()))
            .unwrap_or_else(|| vm_uuid.to_string());
        for pit in &records.pits {
            for (appliance, offer) in &pit.offers {
                if let Offer::Claimed { progress_s, bytes } = offer {
                    live.entry(appliance.to_string())
                        .or_default()
                        .push(LiveTransferView {
                            vm: name.clone(),
                            vm_uuid: vm_uuid.to_string(),
                            pit_uuid: pit.pit_uuid.to_string(),
                            pit_name: pit.name.to_string(),
                            bytes: *bytes,
                            progress_s: *progress_s,
                        });
                }
            }
        }
        vms.push(BackupVmView {
            vm: name,
            vm_uuid: vm_uuid.to_string(),
            vmid: inputs.names.get(vm_uuid).map(|(_, id)| *id),
            policy: records
                .policy
                .as_ref()
                .map(|p| policy_view(p, records.every.as_ref(), inputs.time_zone.as_deref())),
            status: vm_statuses.get(&vm_uuid.to_string()).cloned(),
            pits: records.pits.iter().map(pit_view).collect(),
            branch_uuid: records.lineage.as_ref().map(|l| l.branch_uuid.to_string()),
            local: local_space(records.lineage.as_ref(), &inputs.storage),
        });
    }
    vms.sort_by(|a, b| a.vm.cmp(&b.vm));
    let appliances = inputs
        .appliances
        .map(|list| list.appliances)
        .unwrap_or_default()
        .into_iter()
        .map(|appliance| BackupApplianceView {
            status: statuses.get(&appliance.appliance_uuid).cloned(),
            live: live.remove(&appliance.appliance_uuid).unwrap_or_default(),
            appliance,
        })
        .collect();
    let mut restores: Vec<RestoreView> = inputs
        .restores
        .iter()
        .map(|r| restore_view(r, &inputs.names))
        .collect();
    restores.sort_by_key(|r| std::cmp::Reverse(r.started_at_ms));
    restores.truncate(SHOWN_RESTORES);
    let mut transfers = inputs.transfers;
    transfers.sort_by_key(|t| std::cmp::Reverse(t.at_ms));
    transfers.truncate(SHOWN_TRANSFERS);
    BackupsView {
        at_ms: inputs.now_ms,
        appliances,
        vms,
        restores,
        transfers,
        can_manage: inputs.can_manage,
        errors: inputs.errors,
        time_zone: inputs.time_zone,
        settings: super::policies::settings_view(&inputs.settings),
        settings_revision: inputs.settings_revision,
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
