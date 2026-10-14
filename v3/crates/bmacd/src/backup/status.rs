// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The `backups` observation (`BACKUP.md`, "Alerts"): each appliance's
//! state, and for each VM with a policy its last attempt, its PITs on the
//! cluster, and which appliances are behind. The duty host makes it; the
//! conditions come from it.

use bmac_backup::lifetime::Offer;
use bmac_backup::records::{ApplianceRecord, Attempt, PitRecord, Policy, Presence};
use bmac_backup::settings::BackupSettings;
use bmac_backup::states::ApplianceState;
use bmac_proto::status::{
    BackupApplianceStatus, BackupAttempt, BackupVmStatus, BackupsObservation,
};
use uuid::Uuid;

use super::store::BackupStore;
use super::upkeep::appliance_state;
use crate::schedules::{Recurrence, Schedules};

const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 24 * 60 * MINUTE_MS;

/// The longest a schedule waits between slots.
pub fn period_ms(every: &Recurrence) -> i64 {
    match every {
        Recurrence::Interval { minutes } => i64::from(*minutes) * MINUTE_MS,
        Recurrence::Daily { .. } => DAY_MS,
        Recurrence::Weekly { .. } => 7 * DAY_MS,
        Recurrence::Monthly { .. } => 31 * DAY_MS,
    }
}

pub fn state_text(state: ApplianceState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The state an appliance has across the VMs assigned to it: the worst one.
pub fn overall(
    appliance: &ApplianceRecord,
    policies: &[Policy],
    catalogs: &[(Uuid, Vec<PitRecord>)],
    now_s: i64,
    settings: &BackupSettings,
) -> ApplianceState {
    let rank = |s: &ApplianceState| match s {
        ApplianceState::Delinquent => 6,
        ApplianceState::ReseedRequired => 5,
        ApplianceState::Late => 4,
        ApplianceState::Repairing => 3,
        ApplianceState::Seeding => 2,
        _ => 1,
    };
    policies
        .iter()
        .zip(catalogs)
        .filter(|(p, _)| p.appliances.contains(&appliance.appliance_uuid))
        .map(|(_, (vm, pits))| appliance_state(appliance, vm, pits, now_s, settings))
        .max_by_key(rank)
        .unwrap_or_else(|| appliance_state(appliance, &Uuid::nil(), &[], now_s, settings))
}

fn ok<T>(read: Result<T, bmac_proto::api::ApiError>, errors: &mut Vec<String>) -> Option<T> {
    read.map_err(|e| errors.push(e.message)).ok()
}

pub fn observe(store: &BackupStore, schedules: &Schedules, now_ms: i64) -> BackupsObservation {
    let mut errors = Vec::new();
    let settings = ok(store.settings(), &mut errors).unwrap_or_default();
    let appliances: Vec<ApplianceRecord> = ok(store.appliances(), &mut errors).unwrap_or_default();
    let policies: Vec<Policy> = ok(store.policies(), &mut errors).unwrap_or_default();
    let now_s = now_ms.div_euclid(1000);
    let catalogs: Vec<(Uuid, Vec<PitRecord>)> = policies
        .iter()
        .map(|p| {
            (
                p.vm_uuid,
                ok(store.pits(&p.vm_uuid), &mut errors).unwrap_or_default(),
            )
        })
        .collect();

    let any_suspect = appliances
        .iter()
        .any(|a| a.scrub.as_ref().is_some_and(|s| !s.suspect_pits.is_empty()));
    let held: Vec<(Uuid, std::collections::BTreeSet<String>)> = appliances
        .iter()
        .filter(|a| any_suspect && !a.retired)
        .map(|a| {
            let pits = ok(store.index(&a.appliance_uuid), &mut errors)
                .flatten()
                .map(|i| i.pits.into_iter().map(|p| p.pit_uuid).collect())
                .unwrap_or_default();
            (a.appliance_uuid, pits)
        })
        .collect();
    let appliance_status = appliances
        .iter()
        .map(|a| {
            let scrub = a.scrub.clone().unwrap_or_default();
            let other_copies = if scrub.suspect_pits.is_empty() {
                Vec::new()
            } else {
                appliances
                    .iter()
                    .filter(|o| o.appliance_uuid != a.appliance_uuid && !o.retired)
                    .filter(|o| {
                        held.iter()
                            .find(|(id, _)| *id == o.appliance_uuid)
                            .is_some_and(|(_, pits)| {
                                scrub
                                    .suspect_pits
                                    .iter()
                                    .any(|p| pits.contains(&p.to_string()))
                            })
                    })
                    .map(|o| o.name.clone())
                    .collect()
            };
            BackupApplianceStatus {
                appliance_uuid: a.appliance_uuid.to_string(),
                name: a.name.clone(),
                state: state_text(overall(a, &policies, &catalogs, now_s, &settings)),
                locked: a.locked,
                last_checkin_ms: a.last_checkin_ms,
                scrub_errors: scrub.errors,
                suspect_pits: scrub.suspect_pits.len() as u32,
                other_copies,
            }
        })
        .collect();

    let mut vms = Vec::new();
    for (policy, (_, pits)) in policies.iter().zip(&catalogs) {
        let lineage = ok(store.lineage(&policy.vm_uuid), &mut errors).flatten();
        let period = ok(schedules.get(&policy.schedule_id), &mut errors)
            .flatten()
            .map(|s| period_ms(&s.every))
            .unwrap_or(DAY_MS);
        let name_of = |id: &Uuid| {
            appliances
                .iter()
                .find(|a| a.appliance_uuid == *id)
                .map_or_else(|| id.to_string(), |a| a.name.clone())
        };
        let mut healthy = 0;
        let mut overdue = Vec::new();
        for id in &policy.appliances {
            let Some(appliance) = appliances.iter().find(|a| a.appliance_uuid == *id) else {
                continue;
            };
            let state = appliance_state(appliance, &policy.vm_uuid, pits, now_s, &settings);
            if state == ApplianceState::Active && !appliance.locked {
                healthy += 1;
            }
            let newest = pits
                .iter()
                .filter(|p| p.offers.get(id) == Some(&Offer::Published))
                .map(|p| p.taken_s * 1000)
                .max();
            let since = newest.unwrap_or(policy.updated_at_ms);
            if policy.enabled && now_ms - since > 2 * period {
                overdue.push(name_of(id));
            }
        }
        let unclaimed = pits
            .last()
            .map(|newest| {
                newest
                    .offers
                    .iter()
                    .filter(|(_, o)| match o {
                        Offer::Failed => true,
                        Offer::Open { offered_s } => now_s - offered_s > settings.claim_window_s,
                        _ => false,
                    })
                    .map(|(id, _)| name_of(id))
                    .collect()
            })
            .unwrap_or_default();
        vms.push(BackupVmStatus {
            vm: policy.vm.clone(),
            vm_uuid: policy.vm_uuid.to_string(),
            enabled: policy.enabled,
            last_attempt: lineage.and_then(|l| l.last_attempt).map(|a| match a {
                Attempt::Captured { at_ms, .. } => BackupAttempt {
                    outcome: "captured".into(),
                    at_ms,
                    message: None,
                },
                Attempt::Skipped { at_ms } => BackupAttempt {
                    outcome: "skipped".into(),
                    at_ms,
                    message: None,
                },
                Attempt::Failed { at_ms, message } => BackupAttempt {
                    outcome: "failed".into(),
                    at_ms,
                    message: Some(message),
                },
            }),
            pits: pits.len() as u32,
            snapshots: pits
                .iter()
                .filter(|p| p.presence == Presence::Snapshot)
                .count() as u32,
            release_blocked: pits
                .iter()
                .filter_map(|p| {
                    p.release_blocked
                        .as_ref()
                        .map(|why| format!("{}: {why}", p.name))
                })
                .collect(),
            healthy_appliances: healthy,
            min_healthy: settings.min_healthy_appliances,
            overdue,
            unclaimed,
        });
    }
    BackupsObservation {
        at_ms: now_ms,
        appliances: appliance_status,
        vms,
        errors,
    }
}
