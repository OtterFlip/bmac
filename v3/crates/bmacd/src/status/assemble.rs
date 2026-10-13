// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Builds the status snapshots from what one host knows: every host's
//! newest observations, the cluster membership, and the registry. The
//! conditions keep the wording of v2's `quick_state.py` problems
//! (`render_hosts` 377–396, `render_guests` 588–610, `render_storage`
//! 764–787) with the severities of `MONITORING.md`, "Default conditions".

use std::collections::{BTreeMap, BTreeSet};

use bmac_core::{GuestName, HostSlot, Vmid};
use bmac_proto::status::{
    CLUSTER_COLLECTOR, ClusterObservation, ClusterSnapshot, ClusterSummary, Condition,
    ConditionSeverity, Freshness, GUESTS_COLLECTOR, GuestCommon, GuestObservation,
    GuestsObservation, HOST_COLLECTOR, HaSummary, HostObservation, HostRow, HostStorage,
    HostsSnapshot, NextStep, ProductionVm, QdeviceSummary, ReplicationJob, ReplicationSummary,
    STORAGE_COLLECTOR, StagingSnapshot, StagingVm, StorageObservation, StorageSnapshot,
    UnregisteredGuest, VmsSnapshot,
};
use bmac_registry::{HostSlotRecord, Kind, Resource, ResourceState};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::conditions::Thresholds;
use crate::cluster::Membership;
use crate::engine::events::Reachability;

/// Free space below which a pool is a warning, and a critical condition.
pub const POOL_FREE_WARNING: f64 = 0.20;
pub const POOL_FREE_CRITICAL: f64 = 0.10;

/// Everything one host knows, at one moment.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub now_ms: i64,
    /// The host answering, whose stream state needs no peer.
    pub served_by: Option<HostSlot>,
    pub membership: Option<Membership>,
    pub membership_error: Option<String>,
    pub observations: BTreeMap<(HostSlot, String), Value>,
    pub peers: BTreeMap<HostSlot, Reachability>,
    pub resources: Vec<Resource>,
    pub registry_error: Option<String>,
    pub slots: Vec<HostSlotRecord>,
    /// Each disk's reallocated sectors when first seen, so growth shows.
    pub smart_baseline: BTreeMap<(HostSlot, String), u64>,
    /// Days until each pool is full at its recent rate of growth.
    pub pool_full_days: BTreeMap<(HostSlot, String), f64>,
}

pub(super) fn condition(
    id: &str,
    resource: impl Into<String>,
    severity: ConditionSeverity,
    message: impl Into<String>,
    next_steps: Vec<NextStep>,
) -> Condition {
    Condition {
        id: id.to_string(),
        resource: resource.into(),
        severity,
        message: message.into(),
        since_ms: 0,
        next_steps,
    }
}

impl View {
    pub(super) fn observed<T: DeserializeOwned>(
        &self,
        host: HostSlot,
        collector: &str,
    ) -> Option<T> {
        let value = self.observations.get(&(host, collector.to_string()))?;
        serde_json::from_value(value.clone()).ok()
    }

    /// The cluster members: from the membership, else every host heard of.
    pub(super) fn hosts(&self) -> Vec<HostSlot> {
        let mut hosts: BTreeSet<HostSlot> = match &self.membership {
            Some(m) => m.members.keys().copied().collect(),
            None => self.observations.keys().map(|(h, _)| *h).collect(),
        };
        hosts.extend(self.served_by);
        hosts.into_iter().collect()
    }

    pub(super) fn online(&self, host: HostSlot) -> bool {
        match &self.membership {
            Some(m) => m.online.contains(&host),
            None => Some(host) == self.served_by,
        }
    }

    fn reachable(&self, host: HostSlot) -> bool {
        Some(host) == self.served_by || self.peers.get(&host).is_some_and(|p| p.reachable)
    }

    fn freshness(&self, host: HostSlot, collector: &str) -> Freshness {
        Freshness {
            observed_at_ms: self
                .observations
                .get(&(host, collector.to_string()))
                .and_then(|v| v.get("at_ms")?.as_i64()),
            reachable: self.reachable(host),
        }
    }

    fn cluster_observation(&self) -> Option<ClusterObservation> {
        self.served_by
            .and_then(|h| self.observed(h, CLUSTER_COLLECTOR))
    }

    pub(super) fn errors(&self, collector: &str, conditions: &mut Vec<Condition>) {
        for host in self.hosts() {
            let Some(value) = self.observations.get(&(host, collector.to_string())) else {
                continue;
            };
            for error in value["errors"].as_array().into_iter().flatten() {
                if let Some(error) = error.as_str() {
                    conditions.push(condition(
                        &format!("collector.{collector}"),
                        host.to_string(),
                        ConditionSeverity::Warning,
                        format!("{host}: {error}"),
                        vec![],
                    ));
                }
            }
        }
    }
}

pub fn cluster_summary(view: &View) -> ClusterSummary {
    let observed = view.cluster_observation();
    let hosts = view.hosts();
    ClusterSummary {
        name: observed.as_ref().and_then(|o| o.name.clone()),
        quorate: observed
            .as_ref()
            .and_then(|o| o.quorate)
            .or(view.membership.as_ref().map(|m| m.quorate)),
        expected_votes: observed.as_ref().and_then(|o| o.expected_votes),
        total_votes: observed.as_ref().and_then(|o| o.total_votes),
        quorum: observed.as_ref().and_then(|o| o.quorum),
        host_count: hosts.len() as u32,
        online_count: hosts.iter().filter(|h| view.online(**h)).count() as u32,
        duty_host: view.membership.as_ref().and_then(Membership::duty),
    }
}

pub fn qdevice_summary(view: &View, cluster: &ClusterSummary) -> QdeviceSummary {
    let observed = view.cluster_observation();
    QdeviceSummary {
        registered: observed.as_ref().and_then(|o| o.qdevice_registered),
        address: observed.as_ref().and_then(|o| o.qdevice_address.clone()),
        needed: (cluster.host_count > 0).then_some(cluster.host_count.is_multiple_of(2)),
        voting: observed.as_ref().and_then(|o| o.qdevice_voting),
    }
}

fn host_conditions(
    view: &View,
    cluster: &ClusterSummary,
    qdevice: &QdeviceSummary,
) -> Vec<Condition> {
    let mut conditions = Vec::new();
    if let Some(error) = &view.membership_error {
        conditions.push(condition(
            "cluster.membership",
            "cluster",
            ConditionSeverity::Critical,
            format!("the cluster membership can't be read: {error}"),
            vec![],
        ));
    }
    if cluster.quorate == Some(false) {
        conditions.push(condition(
            "cluster.quorum",
            "cluster",
            ConditionSeverity::Critical,
            "the cluster is not quorate",
            vec![NextStep::run(
                "Check quorum and the Corosync links.",
                "diagnostics.cluster_health",
                &[],
            )],
        ));
    }
    for host in view.hosts() {
        let name = host.to_string();
        if !view.online(host) {
            conditions.push(condition(
                "host.offline",
                &name,
                ConditionSeverity::Critical,
                format!("{name} is offline in the Proxmox cluster"),
                vec![NextStep::run(
                    format!("Check {name}'s state."),
                    "diagnostics.host_state",
                    &[("host", &name)],
                )],
            ));
        } else if view.peers.get(&host).is_some_and(|p| !p.reachable)
            && Some(host) != view.served_by
        {
            conditions.push(condition(
                "host.unreachable",
                &name,
                ConditionSeverity::Warning,
                format!("BMAC on {name} isn't answering this host, so what it shows may be old"),
                vec![],
            ));
        }
    }
    let count = cluster.host_count;
    match (qdevice.registered, qdevice.needed) {
        (Some(false), Some(true)) => conditions.push(condition(
            "qdevice.missing",
            "qdevice",
            ConditionSeverity::Critical,
            format!("the {count}-host cluster needs a QDevice and none is registered"),
            vec![NextStep::run("Add a QDevice.", "qdevice.add", &[])],
        )),
        (Some(true), Some(false)) => conditions.push(condition(
            "qdevice.unneeded",
            "qdevice",
            ConditionSeverity::Warning,
            format!("a QDevice is registered but the {count}-host cluster must not use one"),
            vec![NextStep::run("Remove the QDevice.", "qdevice.remove", &[])],
        )),
        (Some(true), Some(true)) if qdevice.voting == Some(false) => conditions.push(condition(
            "qdevice.not_voting",
            "qdevice",
            ConditionSeverity::Critical,
            "the registered QDevice is not voting",
            vec![NextStep::run(
                "Check the QDevice.",
                "diagnostics.qdevice_state",
                &[],
            )],
        )),
        _ => {}
    }
    view.errors(HOST_COLLECTOR, &mut conditions);
    view.errors(CLUSTER_COLLECTOR, &mut conditions);
    conditions
}

pub fn hosts(view: &View) -> HostsSnapshot {
    let cluster = cluster_summary(view);
    let qdevice = qdevice_summary(view, &cluster);
    let slots: BTreeMap<HostSlot, &HostSlotRecord> =
        view.slots.iter().map(|s| (s.node, s)).collect();
    let rows = view
        .hosts()
        .into_iter()
        .map(|host| HostRow {
            name: host,
            online: view.online(host),
            address: view
                .membership
                .as_ref()
                .and_then(|m| m.members.get(&host).copied().flatten())
                .map(|ip| ip.to_string()),
            slot_state: slots.get(&host).map(|s| s.state.as_str().to_string()),
            is_duty: cluster.duty_host == Some(host),
            is_serving: view.served_by == Some(host),
            freshness: view.freshness(host, HOST_COLLECTOR),
            observation: view.observed::<HostObservation>(host, HOST_COLLECTOR),
        })
        .collect();
    HostsSnapshot {
        at_ms: view.now_ms,
        served_by: view.served_by,
        conditions: host_conditions(view, &cluster, &qdevice)
            .into_iter()
            .chain(super::conditions::host_health(view, &Thresholds::default()))
            .collect(),
        cluster,
        qdevice,
        hosts: rows,
    }
}

/// The UTC stamp `add_staging_vm.sh` puts at the end of a snapshot name,
/// `-YYMMDDTHHMMSSZ`, as milliseconds.
fn snapshot_stamp_ms(name: &str) -> Option<i64> {
    let stamp = name.rsplit_once('-')?.1;
    let bytes = stamp.as_bytes();
    if bytes.len() != 14 || bytes[6] != b'T' || bytes[13] != b'Z' {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| stamp.get(range)?.parse::<i16>().ok();
    let date = jiff::civil::Date::new(
        2000 + digits(0..2)?,
        digits(2..4)? as i8,
        digits(4..6)? as i8,
    )
    .ok()?;
    let time = jiff::civil::Time::new(
        digits(7..9)? as i8,
        digits(9..11)? as i8,
        digits(11..13)? as i8,
        0,
    )
    .ok()?;
    let instant = date
        .to_datetime(time)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .ok()?;
    Some(instant.timestamp().as_millisecond())
}

fn staging_snapshot(resource: &Resource, resources: &[Resource]) -> Option<StagingSnapshot> {
    let snapshot = resource.proxmox.snapshot.as_ref()?;
    let mut shared_with: Vec<GuestName> = resources
        .iter()
        .filter(|r| {
            r.kind == Kind::Staging
                && r.name != resource.name
                && r.source == resource.source
                && r.proxmox
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.name == snapshot.name)
        })
        .map(|r| r.name)
        .collect();
    shared_with.sort_by_key(|n| n.to_string());
    Some(StagingSnapshot {
        name: snapshot.name.clone(),
        created_at_ms: snapshot_stamp_ms(&snapshot.name),
        verified: snapshot.verified == Some(true),
        shared_with,
    })
}

pub fn vms(view: &View) -> VmsSnapshot {
    let mut live: BTreeMap<Vmid, (HostSlot, GuestObservation)> = BTreeMap::new();
    let mut replication: Vec<ReplicationJob> = Vec::new();
    let mut stale_hosts = Vec::new();
    let names: BTreeMap<Vmid, GuestName> =
        view.resources.iter().map(|r| (r.vmid, r.name)).collect();
    for host in view.hosts() {
        let observed = view.observed::<GuestsObservation>(host, GUESTS_COLLECTOR);
        if view.online(host) && (observed.is_none() || !view.reachable(host)) {
            stale_hosts.push(host);
        }
        let Some(observed) = observed else { continue };
        for guest in observed.guests {
            live.insert(guest.vmid, (host, guest));
        }
        replication.extend(observed.replication.into_iter().map(|job| ReplicationJob {
            guest_name: names.get(&job.guest).copied(),
            source: host,
            job,
        }));
    }
    let missing_is_unknown = !stale_hosts.is_empty();
    let mut conditions = Vec::new();
    if let Some(error) = &view.registry_error {
        conditions.push(condition(
            "registry.unreadable",
            "registry",
            ConditionSeverity::Critical,
            format!("the BMAC registry can't be read: {error}"),
            vec![],
        ));
    }
    let mut production = Vec::new();
    let mut staging = Vec::new();
    for resource in &view.resources {
        let observed = live.get(&resource.vmid);
        let name = resource.name.to_string();
        let live_status = match observed {
            Some((_, g)) => g.status.clone().unwrap_or_else(|| "unknown".into()),
            None if missing_is_unknown => "unknown".into(),
            None => "absent".into(),
        };
        let common = GuestCommon {
            name: resource.name,
            vmid: resource.vmid,
            registry_state: resource.state.as_str().to_string(),
            live_status: live_status.clone(),
            node: observed.map(|(h, _)| *h),
            ip: resource.ip.to_string(),
            placement: resource.placement.clone(),
            routes_enabled: resource.routes_enabled,
            domain: Some(resource.domains.primary.clone()).filter(|d| !d.is_empty()),
            cores: resource.spec.cores,
            memory_mb: resource.spec.memory_mb,
            disk_bytes: resource.spec.effective_disk_bytes(),
            uptime_seconds: observed.and_then(|(_, g)| g.uptime_seconds),
        };
        match resource.kind {
            Kind::Production => {
                let jobs: Vec<&ReplicationJob> = replication
                    .iter()
                    .filter(|j| j.job.guest == resource.vmid)
                    .collect();
                let failing: Vec<&&ReplicationJob> = jobs
                    .iter()
                    .filter(|j| j.job.fail_count > 0 || j.job.error.is_some())
                    .collect();
                let mut targets: Vec<String> = jobs.iter().map(|j| j.job.target.clone()).collect();
                targets.sort_by_key(|t| bmac_adapters::proxmox::node_sort_key(t));
                targets.dedup();
                let ha = HaSummary {
                    configured: observed.is_some_and(|(_, g)| g.ha_configured),
                    requested_state: observed.and_then(|(_, g)| g.ha_requested.clone()),
                    state: observed.and_then(|(_, g)| g.ha_state.clone()),
                };
                let active = resource.state == ResourceState::Active;
                let inspect = || {
                    vec![NextStep::run(
                        format!("Inspect {name} across HA, replication, routing, and SSH."),
                        "diagnostics.prod_vm_state",
                        &[("resource", &name)],
                    )]
                };
                if active && live_status != "running" && live_status != "unknown" {
                    conditions.push(condition(
                        "vm.not_running",
                        &name,
                        ConditionSeverity::Critical,
                        format!("{name} is registered active but is {live_status}"),
                        inspect(),
                    ));
                }
                if !failing.is_empty() {
                    let errors: Vec<&str> = failing
                        .iter()
                        .filter_map(|j| j.job.error.as_deref())
                        .collect();
                    let detail = if errors.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", errors.join("; "))
                    };
                    conditions.push(condition(
                        "replication.failing",
                        &name,
                        ConditionSeverity::Warning,
                        format!(
                            "{name} has {} failing replication job(s){detail}",
                            failing.len()
                        ),
                        inspect(),
                    ));
                }
                if active && observed.is_some() && !ha.configured {
                    conditions.push(condition(
                        "vm.ha_missing",
                        &name,
                        ConditionSeverity::Critical,
                        format!("{name} has no HA resource"),
                        inspect(),
                    ));
                }
                if matches!(ha.state.as_deref(), Some("error" | "fence")) {
                    conditions.push(condition(
                        "vm.ha_error",
                        &name,
                        ConditionSeverity::Critical,
                        format!(
                            "HA reports {name} as {}",
                            ha.state.as_deref().unwrap_or_default()
                        ),
                        inspect(),
                    ));
                }
                if let (Some(owner), Some(node)) = (resource.owner_node, common.node)
                    && owner != node
                {
                    conditions.push(condition(
                        "vm.owner_mismatch",
                        &name,
                        ConditionSeverity::Warning,
                        format!("{name} runs on {node} but the registry owner is {owner}"),
                        inspect(),
                    ));
                }
                production.push(ProductionVm {
                    common,
                    purpose: resource.purpose.slug.clone(),
                    aliases: resource.domains.aliases.clone(),
                    owner_node: resource.owner_node,
                    ha,
                    replication: ReplicationSummary {
                        jobs: jobs.len() as u32,
                        failing: failing.len() as u32,
                        targets,
                        oldest_last_sync_ms: jobs
                            .iter()
                            .map(|j| j.job.last_sync_ms.unwrap_or(0))
                            .min(),
                        errors: failing
                            .iter()
                            .filter_map(|j| j.job.error.clone())
                            .take(3)
                            .collect(),
                    },
                });
            }
            Kind::Staging => {
                if matches!(
                    resource.state,
                    ResourceState::CleanupPending | ResourceState::Failed
                ) {
                    conditions.push(condition(
                        "staging.cleanup",
                        &name,
                        ConditionSeverity::Warning,
                        format!("staging {name} is {}", resource.state.as_str()),
                        vec![NextStep::run(
                            format!("Finish removing {name}."),
                            "staging.remove",
                            &[("resource", &name)],
                        )],
                    ));
                }
                staging.push(StagingVm {
                    url: common.domain.as_ref().map(|d| format!("https://{d}")),
                    common,
                    source: resource.source,
                    snapshot: staging_snapshot(resource, &view.resources),
                });
            }
        }
    }
    let unregistered = live
        .iter()
        .filter(|(vmid, _)| !names.contains_key(vmid))
        .map(|(vmid, (host, guest))| UnregisteredGuest {
            vmid: *vmid,
            name: guest.name.clone(),
            node: *host,
            status: guest.status.clone(),
        })
        .collect();
    conditions.extend(super::conditions::vm_health(view));
    view.errors(GUESTS_COLLECTOR, &mut conditions);
    VmsSnapshot {
        at_ms: view.now_ms,
        served_by: view.served_by,
        production,
        staging,
        unregistered,
        replication,
        stale_hosts,
        conditions,
    }
}

pub fn storage(view: &View) -> StorageSnapshot {
    let mut conditions = Vec::new();
    let hosts = view
        .hosts()
        .into_iter()
        .map(|host| {
            let observation = view.observed::<StorageObservation>(host, STORAGE_COLLECTOR);
            let name = host.to_string();
            for pool in observation.iter().flat_map(|o| &o.pools) {
                let inspect = || {
                    vec![NextStep::run(
                        format!("Inspect {name}'s pool members and disk serials."),
                        "diagnostics.host_state",
                        &[("host", &name)],
                    )]
                };
                let resource = format!("{name}/{}", pool.name);
                if let Some(health) = pool.health.as_deref().filter(|h| *h != "ONLINE") {
                    conditions.push(condition(
                        "zfs.pool.health",
                        &resource,
                        ConditionSeverity::Critical,
                        format!("{name}: pool {} is {health}", pool.name),
                        inspect(),
                    ));
                }
                if let Some(free) = pool.free_fraction.filter(|f| *f < POOL_FREE_WARNING) {
                    let severity = if free < POOL_FREE_CRITICAL {
                        ConditionSeverity::Critical
                    } else {
                        ConditionSeverity::Warning
                    };
                    conditions.push(condition(
                        "zfs.pool.capacity",
                        &resource,
                        severity,
                        format!(
                            "{name}: pool {} has only {:.0}% free",
                            pool.name,
                            free * 100.0
                        ),
                        inspect(),
                    ));
                }
            }
            HostStorage {
                host,
                online: view.online(host),
                freshness: view.freshness(host, STORAGE_COLLECTOR),
                observation,
            }
        })
        .collect();
    conditions.extend(super::conditions::storage_health(
        view,
        &Thresholds::default(),
    ));
    view.errors(STORAGE_COLLECTOR, &mut conditions);
    StorageSnapshot {
        at_ms: view.now_ms,
        served_by: view.served_by,
        hosts,
        conditions,
    }
}

/// The cluster at a glance, with every warning and critical condition.
pub fn cluster(view: &View) -> ClusterSnapshot {
    let hosts = hosts(view);
    let mut conditions: Vec<Condition> = hosts
        .conditions
        .into_iter()
        .chain(vms(view).conditions)
        .chain(storage(view).conditions)
        .filter(|c| c.severity >= ConditionSeverity::Warning)
        .collect();
    sort(&mut conditions);
    ClusterSnapshot {
        at_ms: view.now_ms,
        served_by: view.served_by,
        cluster: hosts.cluster,
        qdevice: hosts.qdevice,
        conditions,
    }
}

/// Critical first, then by resource.
pub fn sort(conditions: &mut [Condition]) {
    conditions.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.resource.cmp(&b.resource))
            .then_with(|| a.id.cmp(&b.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_proto::status::{Pool, ReplicationObservation};
    use serde_json::json;

    fn slot(n: u8) -> HostSlot {
        HostSlot::new(n).unwrap()
    }

    fn membership(online: &[u8], members: &[u8], quorate: bool) -> Membership {
        Membership {
            quorate,
            online: online.iter().map(|n| slot(*n)).collect(),
            members: members
                .iter()
                .map(|n| (slot(*n), format!("10.0.0.{n}").parse().ok()))
                .collect(),
        }
    }

    fn observe(view: &mut View, host: u8, collector: &str, data: Value) {
        view.observations
            .insert((slot(host), collector.to_string()), data);
    }

    fn cluster_obs(qdevice: bool, voting: Option<bool>) -> Value {
        json!({"at_ms": 1, "name": "lab", "quorate": true, "expected_votes": 3, "total_votes": 3,
               "quorum": 2, "qdevice_registered": qdevice, "qdevice_address": null,
               "qdevice_voting": voting, "errors": []})
    }

    fn base(online: &[u8], members: &[u8]) -> View {
        let mut view = View {
            now_ms: 100,
            served_by: Some(slot(1)),
            membership: Some(membership(online, members, true)),
            ..View::default()
        };
        observe(&mut view, 1, CLUSTER_COLLECTOR, cluster_obs(false, None));
        view
    }

    fn ids(conditions: &[Condition]) -> Vec<String> {
        conditions
            .iter()
            .map(|c| format!("{} {}", c.id, c.resource))
            .collect()
    }

    #[test]
    fn hosts_show_membership_duty_and_observations() {
        let mut view = base(&[1, 2], &[1, 2, 3]);
        observe(
            &mut view,
            2,
            HOST_COLLECTOR,
            json!({"at_ms": 42, "bmac_version": "3.0.0",
            "uptime_seconds": 10, "cores": 4, "cpu_fraction": 0.5, "load1": 1.0, "memory_used": 1,
            "memory_total": 2, "root_used": null, "root_total": null}),
        );
        view.peers.insert(
            slot(2),
            Reachability {
                reachable: true,
                last_seen_ms: 90,
            },
        );
        let snapshot = hosts(&view);
        assert_eq!(snapshot.cluster.host_count, 3);
        assert_eq!(snapshot.cluster.online_count, 2);
        assert_eq!(snapshot.cluster.duty_host, Some(slot(1)));
        assert_eq!(snapshot.cluster.name.as_deref(), Some("lab"));
        let mox2 = &snapshot.hosts[1];
        assert!(mox2.online && mox2.freshness.reachable && !mox2.is_duty);
        assert_eq!(mox2.freshness.observed_at_ms, Some(42));
        assert_eq!(mox2.observation.as_ref().unwrap().cores, Some(4));
        assert!(snapshot.hosts[0].is_serving);
        assert_eq!(snapshot.hosts[0].address.as_deref(), Some("10.0.0.1"));
        assert_eq!(
            ids(&snapshot.conditions),
            ["host.offline mox3"],
            "three hosts: no QDevice needed"
        );
        assert_eq!(
            snapshot.conditions[0].message,
            "mox3 is offline in the Proxmox cluster"
        );
    }

    #[test]
    fn quorum_qdevice_and_unreachable_hosts_are_conditions() {
        let mut view = base(&[1, 2], &[1, 2]);
        view.membership.as_mut().unwrap().quorate = false;
        observe(
            &mut view,
            1,
            CLUSTER_COLLECTOR,
            json!({"at_ms": 1, "name": null, "quorate": false,
            "expected_votes": null, "total_votes": null, "quorum": null, "qdevice_registered": false,
            "qdevice_address": null, "qdevice_voting": null, "errors": ["pvecm status failed"]}),
        );
        view.peers.insert(
            slot(2),
            Reachability {
                reachable: false,
                last_seen_ms: 5,
            },
        );
        let snapshot = hosts(&view);
        assert_eq!(snapshot.qdevice.needed, Some(true));
        assert_eq!(snapshot.cluster.duty_host, None);
        assert_eq!(
            ids(&snapshot.conditions),
            [
                "cluster.quorum cluster",
                "host.unreachable mox2",
                "qdevice.missing qdevice",
                "collector.cluster mox1"
            ]
        );
        assert_eq!(
            snapshot.conditions[2].next_steps[0]
                .operation
                .as_ref()
                .unwrap()
                .as_str(),
            "qdevice.add"
        );

        let mut view = base(&[1, 2, 3], &[1, 2, 3]);
        observe(
            &mut view,
            1,
            CLUSTER_COLLECTOR,
            cluster_obs(true, Some(true)),
        );
        assert_eq!(ids(&hosts(&view).conditions), ["qdevice.unneeded qdevice"]);
        let mut view = base(&[1, 2], &[1, 2]);
        observe(
            &mut view,
            1,
            CLUSTER_COLLECTOR,
            cluster_obs(true, Some(false)),
        );
        assert_eq!(
            ids(&hosts(&view).conditions),
            ["qdevice.not_voting qdevice"]
        );
    }

    fn resource(name: &str, vmid: u32, state: &str, owner: Option<&str>) -> Resource {
        let staging = name.starts_with("stage");
        serde_json::from_value(json!({
            "schema_version": 1, "record_type": "resource", "id": format!("id-{name}"),
            "allocation_id": null, "request_fingerprint": "f",
            "kind": if staging { "staging" } else { "production" },
            "name": name, "index": 1, "source": if staging { Some("prod1") } else { None },
            "vmid": vmid, "ip": "10.10.0.5", "mac": "BC:24:11:00:00:01", "state": state,
            "purpose": {"slug": "shop", "source_resource": null},
            "domains": {"primary": format!("{name}.example.com"), "aliases": [], "staging_base": ""},
            "placement": ["mox1", "mox2"], "initial_node": "mox1", "owner_node": owner,
            "routes_enabled": true,
            "spec": {"cores": 2, "memory_mb": 4096, "disk_gib": 32, "disk_allocation": "sparse", "replication_interval": "15", "storage": "local-zfs"},
            "proxmox": {"ha_nodes": [], "replication_targets": [], "volume_id": null,
                        "snapshot": if staging { json!({"name": "bmac-stage-260101T120000Z", "owner_node": "mox1",
                            "guids": {}, "verified": true}) } else { json!(null) }},
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z", "revision": 1
        }))
        .unwrap()
    }

    fn guests(list: Value, replication: Vec<ReplicationObservation>) -> Value {
        json!({"at_ms": 3, "guests": list, "replication": replication, "errors": []})
    }

    fn guest(vmid: u32, status: &str, ha: bool) -> Value {
        json!({"vmid": vmid, "name": format!("vm{vmid}"), "status": status, "cores": 2,
               "memory_bytes": null, "disk_bytes": null, "uptime_seconds": 5,
               "ha_configured": ha, "ha_requested": if ha { Some("started") } else { None },
               "ha_state": if ha { Some("started") } else { None }})
    }

    fn job(guest: u32, target: &str, error: Option<&str>) -> ReplicationObservation {
        ReplicationObservation {
            id: format!("{guest}-0"),
            guest: Vmid::new(guest).unwrap(),
            target: target.into(),
            schedule: Some("*/15".into()),
            last_sync_ms: Some(1000),
            last_try_ms: None,
            next_sync_ms: None,
            duration_seconds: None,
            fail_count: u32::from(error.is_some()),
            error: error.map(str::to_string),
            disabled: false,
        }
    }

    #[test]
    fn vms_join_the_registry_with_every_hosts_guests() {
        let mut view = base(&[1, 2], &[1, 2]);
        view.peers.insert(
            slot(2),
            Reachability {
                reachable: true,
                last_seen_ms: 90,
            },
        );
        view.resources = vec![
            resource("prod1", 101, "active", Some("mox1")),
            resource("prod2", 102, "active", Some("mox1")),
            resource("prod3", 103, "active", None),
            resource("stage1prod1", 201, "cleanup_pending", None),
            resource("stage2prod1", 202, "active", None),
        ];
        observe(
            &mut view,
            1,
            GUESTS_COLLECTOR,
            guests(
                json!([guest(101, "running", true), guest(201, "running", false)]),
                vec![job(101, "mox2", None)],
            ),
        );
        observe(
            &mut view,
            2,
            GUESTS_COLLECTOR,
            guests(
                json!([guest(102, "stopped", false), guest(999, "running", false)]),
                vec![job(102, "mox1", Some("no space"))],
            ),
        );
        let snapshot = vms(&view);
        assert!(snapshot.stale_hosts.is_empty());
        let prod: BTreeMap<String, &ProductionVm> = snapshot
            .production
            .iter()
            .map(|p| (p.common.name.to_string(), p))
            .collect();
        assert_eq!(prod["prod1"].common.node, Some(slot(1)));
        assert_eq!(prod["prod1"].replication.targets, ["mox2"]);
        assert!(prod["prod1"].ha.configured);
        assert_eq!(prod["prod2"].common.live_status, "stopped");
        assert_eq!(prod["prod3"].common.live_status, "absent");
        assert_eq!(prod["prod2"].replication.errors, ["no space"]);
        assert_eq!(snapshot.unregistered.len(), 1);
        assert_eq!(snapshot.unregistered[0].node, slot(2));
        assert_eq!(
            snapshot.replication[1].guest_name.unwrap().to_string(),
            "prod2"
        );
        let staging = &snapshot.staging[0];
        assert_eq!(
            staging.url.as_deref(),
            Some("https://stage1prod1.example.com")
        );
        let shot = staging.snapshot.as_ref().unwrap();
        assert_eq!(shot.created_at_ms, Some(1_767_268_800_000));
        assert_eq!(shot.shared_with[0].to_string(), "stage2prod1");
        assert_eq!(
            ids(&snapshot.conditions),
            [
                "vm.not_running prod2",
                "replication.failing prod2",
                "vm.ha_missing prod2",
                "vm.owner_mismatch prod2",
                "vm.not_running prod3",
                "staging.cleanup stage1prod1",
            ]
        );
        assert_eq!(
            snapshot.conditions[1].message,
            "prod2 has 1 failing replication job(s): no space"
        );
    }

    #[test]
    fn a_host_without_guests_observed_makes_missing_guests_unknown() {
        let mut view = base(&[1, 2], &[1, 2]);
        view.resources = vec![resource("prod1", 101, "active", None)];
        observe(&mut view, 1, GUESTS_COLLECTOR, guests(json!([]), vec![]));
        let snapshot = vms(&view);
        assert_eq!(snapshot.stale_hosts, [slot(2)]);
        assert_eq!(snapshot.production[0].common.live_status, "unknown");
        assert!(
            snapshot.conditions.is_empty(),
            "unknown isn't reported as stopped"
        );
    }

    #[test]
    fn pool_health_and_free_space_are_conditions() {
        let mut view = base(&[1, 2], &[1, 2]);
        let pool = |name: &str, health: &str, free: f64| Pool {
            name: name.into(),
            health: Some(health.into()),
            size: Some(100),
            allocated: None,
            free: None,
            fragmentation_percent: None,
            free_fraction: Some(free),
            scan: None,
            remove: None,
            errors: None,
        };
        let observation = StorageObservation {
            at_ms: 7,
            pools: vec![
                pool("rpool", "DEGRADED", 0.15),
                pool("tank", "ONLINE", 0.05),
            ],
            storages: vec![],
            vdevs: vec![],
            disks: vec![],
            findings: vec![],
            luks_mismatches: vec![],
            errors: vec![],
        };
        observe(
            &mut view,
            1,
            STORAGE_COLLECTOR,
            serde_json::to_value(observation).unwrap(),
        );
        let snapshot = storage(&view);
        assert_eq!(snapshot.hosts.len(), 2);
        assert!(snapshot.hosts[1].observation.is_none());
        assert_eq!(snapshot.hosts[0].freshness.observed_at_ms, Some(7));
        let found: Vec<(String, ConditionSeverity)> = snapshot
            .conditions
            .iter()
            .map(|c| (c.message.clone(), c.severity))
            .collect();
        assert_eq!(
            found,
            [
                (
                    "mox1: pool rpool is DEGRADED".into(),
                    ConditionSeverity::Critical
                ),
                (
                    "mox1: pool rpool has only 15% free".into(),
                    ConditionSeverity::Warning
                ),
                (
                    "mox1: pool tank has only 5% free".into(),
                    ConditionSeverity::Critical
                ),
            ]
        );
    }

    #[test]
    fn the_cluster_snapshot_gathers_warnings_critical_first() {
        let mut view = base(&[1], &[1, 2, 3]);
        view.resources = vec![resource("stage1prod1", 201, "failed", None)];
        observe(&mut view, 1, GUESTS_COLLECTOR, guests(json!([]), vec![]));
        let snapshot = cluster(&view);
        assert_eq!(
            ids(&snapshot.conditions),
            [
                "host.offline mox2",
                "host.offline mox3",
                "staging.cleanup stage1prod1"
            ]
        );
    }

    #[test]
    fn staging_snapshot_stamps_are_utc() {
        assert_eq!(
            snapshot_stamp_ms("x-260101T120000Z"),
            Some(1_767_268_800_000)
        );
        assert_eq!(snapshot_stamp_ms("x-2601011200"), None);
        assert_eq!(snapshot_stamp_ms("x-261301T120000Z"), None, "month 13");
        assert_eq!(snapshot_stamp_ms("nostamp"), None);
    }
}
