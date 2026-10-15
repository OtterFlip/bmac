// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The conditions drawn from the health collectors (`MONITORING.md`,
//! "Default conditions"), with v2's wording where v2 reported the same
//! thing (`show_cluster_health.sh`, `show_proxmox_host_state.sh`). The
//! thresholds are [`Thresholds`]; every one of them is meant to become a
//! cluster setting.

use std::collections::{BTreeMap, BTreeSet};

use bmac_core::HostSlot;
use bmac_proto::status::{
    AGENTS_COLLECTOR, AgentsObservation, BACKUPS_COLLECTOR, BackupsObservation,
    CERTIFICATES_COLLECTOR, CLEANUP_COLLECTOR, CLOCK_COLLECTOR, CLUSTER_COLLECTOR,
    CertificatesObservation, CleanupObservation, ClockObservation, ClusterObservation, Condition,
    ConditionSeverity, HOST_COLLECTOR, HostObservation, INGRESS_COLLECTOR, IngressObservation,
    LOGS_COLLECTOR, LogObservation, NOTIFY_COLLECTOR, NextStep, NotifyObservation,
    PMXCFS_COLLECTOR, PmxcfsObservation, QDEVICE_COLLECTOR, QdeviceObservation, REGISTRY_COLLECTOR,
    RegistryObservation, SMART_COLLECTOR, STORAGE_COLLECTOR, SmartObservation, StorageObservation,
    TAILSCALE_COLLECTOR, TailscaleObservation, UNITS_COLLECTOR, UnitsObservation, VRRP_COLLECTOR,
    VrrpObservation,
};

use super::assemble::{View, condition};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    pub certificate_warning_days: i64,
    pub certificate_critical_days: i64,
    pub pmxcfs_warning_bytes: u64,
    pub pmxcfs_critical_bytes: u64,
    pub cleanup_stuck_ms: i64,
    pub pool_full_days: f64,
    /// A peer's copy of a host's records lagging this long is a condition.
    pub copy_lag_ms: i64,
    /// How long evicting audit events stays a condition.
    pub audit_evicted_ms: i64,
}

impl Default for Thresholds {
    fn default() -> Thresholds {
        Thresholds {
            certificate_warning_days: 60,
            certificate_critical_days: 14,
            pmxcfs_warning_bytes: 8 * MIB,
            pmxcfs_critical_bytes: 14 * MIB,
            cleanup_stuck_ms: DAY_MS,
            pool_full_days: 14.0,
            copy_lag_ms: 5 * 60 * 1000,
            audit_evicted_ms: 7 * DAY_MS,
        }
    }
}

fn check_host(host: HostSlot) -> Vec<NextStep> {
    vec![NextStep::run(
        format!("Check {host}'s state."),
        "diagnostics.host_state",
        &[("host", &host.to_string())],
    )]
}

/// Conditions about each host itself.
pub fn host_health(view: &View, limits: &Thresholds) -> Vec<Condition> {
    let mut found = Vec::new();
    links(view, &mut found);
    let mut versions: BTreeMap<String, Vec<HostSlot>> = BTreeMap::new();
    for host in view.hosts() {
        let name = host.to_string();
        if let Some(seen) = view.observed::<HostObservation>(host, HOST_COLLECTOR) {
            versions.entry(seen.bmac_version).or_default().push(host);
        }
        if let Some(units) = view.observed::<UnitsObservation>(host, UNITS_COLLECTOR) {
            let mut named = BTreeSet::new();
            for (unit, state) in &units.required {
                if state != "active" {
                    named.insert(unit.clone());
                    found.push(condition(
                        "unit.inactive",
                        format!("{name}/{unit}"),
                        ConditionSeverity::Warning,
                        format!("{name}: {unit} is {state}"),
                        check_host(host),
                    ));
                }
            }
            for unit in units.failed.iter().filter(|u| !named.contains(*u)) {
                found.push(condition(
                    "unit.failed",
                    format!("{name}/{unit}"),
                    ConditionSeverity::Warning,
                    format!("{name}: {unit} failed"),
                    check_host(host),
                ));
            }
        }
        if let Some(logs) = view.observed::<LogObservation>(host, LOGS_COLLECTOR) {
            for lag in logs
                .lagging
                .iter()
                .filter(|l| l.behind_ms > limits.copy_lag_ms)
            {
                found.push(condition(
                    "logs.copy_lag",
                    format!("{name}/{}", lag.peer),
                    ConditionSeverity::Warning,
                    format!(
                        "{}'s copy of {name}'s logs is {} minute(s) behind ({} record(s))",
                        lag.peer,
                        lag.behind_ms / 60_000,
                        lag.behind_records
                    ),
                    vec![NextStep {
                        text: "Open Settings, then Log replication.".to_string(),
                        operation: None,
                        args: BTreeMap::new(),
                    }],
                ));
            }
            if let Some(at) = logs.audit_evicted_ms
                && view.now_ms - at < limits.audit_evicted_ms
            {
                found.push(condition(
                    "logs.audit_evicted",
                    &name,
                    ConditionSeverity::Warning,
                    format!(
                        "{name}: log storage ran out of room and evicted audit events or failure records"
                    ),
                    vec![NextStep {
                        text: "Free space on the host's root filesystem, or export the logs you need from the Logs page.".to_string(),
                        operation: None,
                        args: BTreeMap::new(),
                    }],
                ));
            }
        }
        if let Some(tailscale) = view.observed::<TailscaleObservation>(host, TAILSCALE_COLLECTOR)
            && let Some(state) = tailscale.backend_state.filter(|s| s != "Running")
        {
            found.push(condition(
                "tailscale.down",
                &name,
                ConditionSeverity::Warning,
                format!("{name}: Tailscale is {state}, so admins can't reach it over the tailnet"),
                check_host(host),
            ));
        }
        if let Some(clock) = view.observed::<ClockObservation>(host, CLOCK_COLLECTOR)
            && clock.synchronized == Some(false)
        {
            found.push(condition(
                "clock.unsynchronized",
                &name,
                ConditionSeverity::Warning,
                format!(
                    "{name}: the clock is not synchronized ({})",
                    clock.leap_status.as_deref().unwrap_or("unknown")
                ),
                check_host(host),
            ));
        }
        if let Some(certificates) =
            view.observed::<CertificatesObservation>(host, CERTIFICATES_COLLECTOR)
        {
            for certificate in certificates.certificates {
                let Some(after) = certificate.not_after_ms else {
                    continue;
                };
                let days = (after - view.now_ms).div_euclid(DAY_MS);
                let severity = if days < limits.certificate_critical_days {
                    ConditionSeverity::Critical
                } else if days < limits.certificate_warning_days {
                    ConditionSeverity::Warning
                } else {
                    continue;
                };
                let which = match certificate.name.as_str() {
                    "dashboard" => "Dashboard HTTPS certificate",
                    "proxmox" => "Proxmox web certificate",
                    other => other,
                };
                let when = if days < 0 {
                    "has expired".to_string()
                } else {
                    format!("expires in {days} days")
                };
                found.push(condition(
                    "certificate.expiry",
                    format!("{name}/{}", certificate.name),
                    severity,
                    format!("{name}: the {which} {when}"),
                    vec![NextStep {
                        text: "Rotate the certificate in Settings.".to_string(),
                        operation: None,
                        args: BTreeMap::new(),
                    }],
                ));
            }
        }
        if let Some(cleanup) = view.observed::<CleanupObservation>(host, CLEANUP_COLLECTOR)
            && let Some(oldest) = cleanup.oldest_ms
            && view.now_ms - oldest > limits.cleanup_stuck_ms
        {
            found.push(condition(
                "cleanup.stuck",
                &name,
                ConditionSeverity::Warning,
                format!(
                    "{name}: {} deferred cleanup(s) waiting, the oldest for over {} hours",
                    cleanup.waiting,
                    (view.now_ms - oldest) / (60 * 60 * 1000)
                ),
                check_host(host),
            ));
        }
        if let Some(qdevice) = view.observed::<QdeviceObservation>(host, QDEVICE_COLLECTOR) {
            qdevice_reach(host, &qdevice, &mut found);
        }
    }
    if versions.len() > 1 {
        let listed: Vec<String> = versions
            .iter()
            .map(|(version, hosts)| {
                let hosts: Vec<String> = hosts.iter().map(|h| h.to_string()).collect();
                format!("{version} on {}", hosts.join(", "))
            })
            .collect();
        found.push(condition(
            "cluster.versions",
            "cluster",
            ConditionSeverity::Warning,
            format!("hosts run different BMAC versions: {}", listed.join("; ")),
            vec![],
        ));
    }
    pmxcfs(view, limits, &mut found);
    egress(view, &mut found);
    ingress(view, &mut found);
    notifications(view, &mut found);
    for collector in [
        UNITS_COLLECTOR,
        TAILSCALE_COLLECTOR,
        CLOCK_COLLECTOR,
        CLEANUP_COLLECTOR,
        PMXCFS_COLLECTOR,
        QDEVICE_COLLECTOR,
        VRRP_COLLECTOR,
        INGRESS_COLLECTOR,
        AGENTS_COLLECTOR,
        SMART_COLLECTOR,
        REGISTRY_COLLECTOR,
    ] {
        view.errors(collector, &mut found);
    }
    found
}

fn qdevice_reach(host: HostSlot, seen: &QdeviceObservation, found: &mut Vec<Condition>) {
    let (Some(registered), Some(tailnet)) = (&seen.registered_address, &seen.tailnet_address)
    else {
        if seen.qnetd_reachable == Some(false) {
            found.push(qnetd_unreachable(host, seen));
        }
        return;
    };
    if registered != tailnet {
        found.push(condition(
            "qdevice.address",
            "qdevice",
            ConditionSeverity::Critical,
            format!(
                "the QDevice is registered at {registered}, but its tailnet address is now {tailnet}"
            ),
            vec![
                NextStep::run("Remove the QDevice.", "qdevice.remove", &[]),
                NextStep::run("Then add it again.", "qdevice.add", &[]),
            ],
        ));
    }
    if seen.qnetd_reachable == Some(false) {
        found.push(qnetd_unreachable(host, seen));
    }
}

fn qnetd_unreachable(host: HostSlot, seen: &QdeviceObservation) -> Condition {
    condition(
        "qdevice.unreachable",
        format!("{host}/qdevice"),
        ConditionSeverity::Critical,
        format!(
            "{host} can't reach corosync-qnetd at {}:5403",
            seen.registered_address.as_deref().unwrap_or("?")
        ),
        vec![NextStep::run(
            "Check the QDevice.",
            "diagnostics.qdevice_state",
            &[],
        )],
    )
}

/// A down link is reported once per pair of hosts and link, however many
/// of the pair see it. A peer with every link down from every host that
/// sees it is offline, which `host.offline` reports, so its links are not
/// listed (v2 `show_cluster_health.sh` 514–527).
fn links(view: &View, found: &mut Vec<Condition>) {
    let mut down: BTreeMap<(HostSlot, HostSlot, u32), ()> = BTreeMap::new();
    let mut cut_off: BTreeMap<HostSlot, (usize, usize)> = BTreeMap::new();
    for observer in view.hosts() {
        let Some(cluster) = view.observed::<ClusterObservation>(observer, CLUSTER_COLLECTOR) else {
            continue;
        };
        for peer in &cluster.peers {
            let Some(name) = peer.name else {
                continue;
            };
            let all_down = peer.reachable == Some(false)
                || (!peer.links.is_empty() && peer.links.values().all(|up| !up));
            let counts = cut_off.entry(name).or_default();
            counts.0 += 1;
            counts.1 += usize::from(all_down);
            for (link, up) in &peer.links {
                if !up {
                    let pair = if observer < name {
                        (observer, name)
                    } else {
                        (name, observer)
                    };
                    down.insert((pair.0, pair.1, *link), ());
                }
            }
        }
    }
    let offline: BTreeSet<HostSlot> = cut_off
        .into_iter()
        .filter(|(_, (seen, all_down))| seen == all_down)
        .map(|(host, _)| host)
        .collect();
    for (a, b, link) in down.into_keys() {
        if offline.contains(&a) || offline.contains(&b) {
            continue;
        }
        found.push(condition(
            "corosync.link",
            format!("{a}-{b}/link{link}"),
            ConditionSeverity::Warning,
            format!("Corosync link {link} between {a} and {b} is down"),
            vec![NextStep::run(
                "Check quorum and the Corosync links.",
                "diagnostics.cluster_health",
                &[],
            )],
        ));
    }
}

fn pmxcfs(view: &View, limits: &Thresholds, found: &mut Vec<Condition>) {
    let used = view
        .hosts()
        .into_iter()
        .filter_map(|h| {
            view.observed::<PmxcfsObservation>(h, PMXCFS_COLLECTOR)?
                .bmac_bytes
        })
        .max();
    let Some(used) = used else {
        return;
    };
    let severity = if used > limits.pmxcfs_critical_bytes {
        ConditionSeverity::Critical
    } else if used > limits.pmxcfs_warning_bytes {
        ConditionSeverity::Warning
    } else {
        return;
    };
    found.push(condition(
        "pmxcfs.usage",
        "cluster",
        severity,
        format!(
            "BMAC keeps {:.1} MiB in pmxcfs, which holds 128 MiB for everything",
            used as f64 / MIB as f64
        ),
        vec![],
    ));
}

/// Exactly one online host holds the guest-egress VIP (v2
/// `add_proxmox_host.sh` 5010–5069).
fn egress(view: &View, found: &mut Vec<Condition>) {
    let mut vip = None;
    let mut holders = Vec::new();
    let mut known = 0;
    for host in view.hosts().into_iter().filter(|h| view.online(*h)) {
        let Some(seen) = view.observed::<VrrpObservation>(host, VRRP_COLLECTOR) else {
            continue;
        };
        if seen.vip.is_some() {
            vip = seen.vip.clone();
        }
        match seen.holds_vip {
            Some(true) => {
                known += 1;
                holders.push(host.to_string());
            }
            Some(false) => known += 1,
            None => {}
        }
    }
    let Some(vip) = vip else {
        return;
    };
    if known == 0 {
        return;
    }
    let message = match holders.len() {
        0 => format!("no online host holds the guest-egress VIP {vip}, so guests have no egress"),
        1 => return,
        _ => format!(
            "more than one host holds the guest-egress VIP {vip}: {}",
            holders.join(", ")
        ),
    };
    let id = if holders.is_empty() {
        "vrrp.no_owner"
    } else {
        "vrrp.duplicate_owner"
    };
    found.push(condition(
        id,
        "egress",
        ConditionSeverity::Critical,
        message,
        vec![NextStep::run(
            "Check quorum and the Corosync links.",
            "diagnostics.cluster_health",
            &[],
        )],
    ));
}

fn ingress(view: &View, found: &mut Vec<Condition>) {
    for host in view.hosts().into_iter().filter(|h| view.online(*h)) {
        let Some(seen) = view.observed::<IngressObservation>(host, INGRESS_COLLECTOR) else {
            continue;
        };
        if !seen.serving {
            continue;
        }
        let name = host.to_string();
        if seen.applied_generation != seen.desired_generation {
            found.push(condition(
                "ingress.not_applied",
                format!("{name}/ingress"),
                ConditionSeverity::Critical,
                format!(
                    "{name} has not applied ingress generation {} (it has {})",
                    seen.desired_generation.as_deref().unwrap_or("?"),
                    seen.applied_generation.as_deref().unwrap_or("none")
                ),
                check_host(host),
            ));
        }
        if let Some(state) = seen.haproxy.filter(|s| s != "active") {
            found.push(condition(
                "ingress.haproxy",
                format!("{name}/ingress"),
                ConditionSeverity::Critical,
                format!("{name}: HAProxy ingress is {state}"),
                check_host(host),
            ));
        }
    }
}

/// Email's own trouble, from the host that sends it: the duty host, or
/// this one without quorum. None of these is emailed.
fn notifications(view: &View, found: &mut Vec<Condition>) {
    let sender = view
        .membership
        .as_ref()
        .and_then(|m| m.duty())
        .or(view.served_by);
    let Some(seen) = sender.and_then(|h| view.observed::<NotifyObservation>(h, NOTIFY_COLLECTOR))
    else {
        return;
    };
    let settings = || {
        vec![NextStep {
            text: "Open Settings, then Notifications.".to_string(),
            operation: None,
            args: BTreeMap::new(),
        }]
    };
    if seen.unauthorized {
        found.push(condition(
            "notify.unauthorized",
            "postmark",
            ConditionSeverity::Critical,
            "Postmark refused the server token, so no email is sent until it is replaced",
            settings(),
        ));
    }
    if seen.sender_refused {
        found.push(condition(
            "notify.sender_refused",
            "postmark",
            ConditionSeverity::Critical,
            "Postmark refused the sender address, so no email is sent until it is confirmed in Postmark or changed",
            settings(),
        ));
    }
    if seen.templates_missing {
        found.push(condition(
            "notify.templates_missing",
            "postmark",
            ConditionSeverity::Warning,
            format!(
                "Notification templates missing on the Postmark server; {} message(s) are held until they are created",
                seen.held
            ),
            settings(),
        ));
    }
    for address in &seen.inactive_recipients {
        found.push(condition(
            "notify.inactive_recipient",
            address,
            ConditionSeverity::Warning,
            format!(
                "{address} is inactive in Postmark and gets no email until it is reactivated there"
            ),
            settings(),
        ));
    }
    if let Some(since) = seen.failing_since_ms
        && view.now_ms - since > 60 * 60 * 1000
    {
        found.push(condition(
            "notify.failing",
            "postmark",
            ConditionSeverity::Warning,
            format!(
                "email has not reached Postmark for {} hour(s): {}",
                (view.now_ms - since) / (60 * 60 * 1000),
                seen.last_error.as_deref().unwrap_or("unknown error")
            ),
            settings(),
        ));
    }
}

/// Conditions about guests: their agents and the registry's agreement
/// with Proxmox.
pub fn vm_health(view: &View) -> Vec<Condition> {
    let mut found = Vec::new();
    for host in view.hosts() {
        let Some(agents) = view.observed::<AgentsObservation>(host, AGENTS_COLLECTOR) else {
            continue;
        };
        for agent in agents.agents.iter().filter(|a| !a.answering) {
            let name = agent.name.to_string();
            found.push(condition(
                "vm.agent",
                &name,
                ConditionSeverity::Warning,
                format!("{name}'s guest agent on {host} isn't answering"),
                vec![NextStep::run(
                    format!("Check {name}'s state."),
                    "diagnostics.prod_vm_state",
                    &[("vm", &name)],
                )],
            ));
        }
    }
    let duty = view.membership.as_ref().and_then(|m| m.duty());
    if let Some(registry) =
        duty.and_then(|h| view.observed::<RegistryObservation>(h, REGISTRY_COLLECTOR))
    {
        for issue in registry.issues {
            found.push(condition(
                "registry.disagrees",
                format!("{}/{}", issue.resource, issue.code),
                ConditionSeverity::Warning,
                format!("{}: {} ({})", issue.resource, issue.message, issue.code),
                vec![],
            ));
        }
    }
    backups(view, &mut found);
    found
}

/// `BACKUP.md`, "Alerts". The duty host observes the backup records.
fn backups(view: &View, found: &mut Vec<Condition>) {
    let duty = view.membership.as_ref().and_then(|m| m.duty());
    let Some(seen) = duty.and_then(|h| view.observed::<BackupsObservation>(h, BACKUPS_COLLECTOR))
    else {
        return;
    };
    for appliance in &seen.appliances {
        let name = &appliance.name;
        let resource = format!("appliance/{}", appliance.appliance_uuid);
        match appliance.state.as_str() {
            "LATE" => found.push(condition(
                "backup.appliance.late",
                &resource,
                ConditionSeverity::Warning,
                format!("backup appliance {name} hasn't checked in"),
                vec![],
            )),
            "DELINQUENT" => found.push(condition(
                "backup.appliance.delinquent",
                &resource,
                ConditionSeverity::Critical,
                format!(
                    "backup appliance {name} is delinquent; the cluster no longer keeps \
                     snapshots for it"
                ),
                vec![],
            )),
            "RESEED_REQUIRED" => found.push(condition(
                "backup.appliance.reseed",
                &resource,
                ConditionSeverity::Critical,
                format!("backup appliance {name} needs a full send to continue"),
                vec![],
            )),
            _ => {}
        }
        if appliance.locked {
            found.push(condition(
                "backup.appliance.locked",
                &resource,
                ConditionSeverity::Warning,
                format!("backup appliance {name} is locked and can't receive backups"),
                vec![],
            ));
        }
        if appliance.scrub_errors > 0 || appliance.suspect_pits > 0 {
            let copies = if appliance.suspect_pits == 0 {
                String::new()
            } else if appliance.other_copies.is_empty() {
                format!(
                    "; {} PITs are suspect and no other appliance holds them",
                    appliance.suspect_pits
                )
            } else {
                format!(
                    "; {} PITs are suspect; other copies are on {}",
                    appliance.suspect_pits,
                    appliance.other_copies.join(", ")
                )
            };
            found.push(condition(
                "backup.appliance.scrub_errors",
                &resource,
                ConditionSeverity::Critical,
                format!(
                    "backup appliance {name}'s scrub found {} unrepairable errors{copies}",
                    appliance.scrub_errors
                ),
                vec![],
            ));
        }
    }
    for vm in &seen.vms {
        let name = &vm.vm;
        let resource = format!("vm/{name}");
        let back_up = || {
            vec![NextStep::run(
                format!("Back up {name} now."),
                "backup.capture",
                &[("vm", name)],
            )]
        };
        match &vm.last_attempt {
            Some(a) if a.outcome == "failed" => found.push(condition(
                "backup.capture.failed",
                &resource,
                ConditionSeverity::Critical,
                format!(
                    "{name}'s last backup failed: {}",
                    a.message.as_deref().unwrap_or("no reason given")
                ),
                back_up(),
            )),
            Some(a) if a.outcome == "skipped" => found.push(condition(
                "backup.skipped",
                &resource,
                ConditionSeverity::Critical,
                format!(
                    "{name}'s last scheduled backup was skipped: none of its appliances was available"
                ),
                vec![],
            )),
            _ => {}
        }
        for appliance in &vm.unclaimed {
            found.push(condition(
                "backup.unclaimed",
                format!("{resource}/{appliance}"),
                ConditionSeverity::Warning,
                format!("{appliance} didn't take {name}'s newest backup"),
                vec![],
            ));
        }
        for appliance in &vm.overdue {
            found.push(condition(
                "backup.overdue",
                format!("{resource}/{appliance}"),
                ConditionSeverity::Critical,
                format!("{name}'s newest backup on {appliance} is older than its schedule allows"),
                back_up(),
            ));
        }
        if vm.enabled && vm.healthy_appliances < vm.min_healthy {
            found.push(condition(
                "backup.too_few",
                &resource,
                ConditionSeverity::Critical,
                format!(
                    "{name} has {} healthy backup appliance(s); at least {} are wanted",
                    vm.healthy_appliances, vm.min_healthy
                ),
                vec![],
            ));
        }
        for blocked in &vm.release_blocked {
            found.push(condition(
                "backup.release_blocked",
                &resource,
                ConditionSeverity::Warning,
                format!("{name}: a backup snapshot can't be released ({blocked})"),
                vec![],
            ));
        }
    }
    view.errors(BACKUPS_COLLECTOR, found);
}

/// Conditions about pools, disks, and encryption, beyond pool health and
/// free space.
pub fn storage_health(view: &View, limits: &Thresholds) -> Vec<Condition> {
    let mut found = Vec::new();
    for host in view.hosts() {
        let name = host.to_string();
        let inspect = || {
            vec![NextStep::run(
                format!("Inspect {name}'s pool members and disk serials."),
                "diagnostics.host_state",
                &[("host", &name)],
            )]
        };
        if let Some(storage) = view.observed::<StorageObservation>(host, STORAGE_COLLECTOR) {
            for finding in &storage.findings {
                // The pool's own state is `zfs.pool.health`.
                if finding
                    .message
                    .starts_with(&format!("pool {} is ", finding.pool))
                    && !finding.message.contains("resilvering")
                {
                    continue;
                }
                let severity = if finding.message.contains("resilvering") {
                    ConditionSeverity::Warning
                } else {
                    ConditionSeverity::Critical
                };
                found.push(condition(
                    "zfs.pool.member",
                    format!("{name}/{}", finding.pool),
                    severity,
                    format!("{name}: {}", finding.message),
                    inspect(),
                ));
            }
            for mismatch in &storage.luks_mismatches {
                found.push(condition(
                    "luks.mismatch",
                    format!("{name}/luks"),
                    ConditionSeverity::Critical,
                    format!("{name}: {mismatch}"),
                    inspect(),
                ));
            }
        }
        if let Some(smart) = view.observed::<SmartObservation>(host, SMART_COLLECTOR) {
            for disk in &smart.disks {
                let label = format!(
                    "{} {}",
                    disk.disk.trim_start_matches("/dev/"),
                    disk.serial.as_deref().unwrap_or("(no serial)")
                );
                let resource = format!("{name}/{}", disk.disk.trim_start_matches("/dev/"));
                match disk.health.as_str() {
                    "failing" => found.push(condition(
                        "disk.smart",
                        &resource,
                        ConditionSeverity::Critical,
                        format!("{name}: disk {label} reports SMART failure"),
                        inspect(),
                    )),
                    "timed_out" => found.push(condition(
                        "disk.smart",
                        &resource,
                        ConditionSeverity::Warning,
                        format!("{name}: disk {label} did not answer smartctl within 30 seconds"),
                        inspect(),
                    )),
                    _ => {}
                }
                let baseline = view.smart_baseline.get(&(host, disk.disk.clone()));
                let grew = matches!(
                    (disk.reallocated_sectors, baseline),
                    (Some(now), Some(first)) if now > *first
                );
                let pending = disk.pending_sectors.unwrap_or(0) + disk.media_errors.unwrap_or(0);
                if grew || pending > 0 {
                    found.push(condition(
                        "disk.wear",
                        &resource,
                        ConditionSeverity::Warning,
                        format!(
                            "{name}: disk {label} has {} reallocated and {} pending sectors{}",
                            disk.reallocated_sectors.unwrap_or(0),
                            disk.pending_sectors.unwrap_or(0),
                            disk.media_errors
                                .filter(|n| *n > 0)
                                .map(|n| format!(" and {n} media errors"))
                                .unwrap_or_default()
                        ),
                        inspect(),
                    ));
                }
            }
        }
    }
    for ((host, pool), days) in &view.pool_full_days {
        if *days < limits.pool_full_days {
            let name = host.to_string();
            found.push(condition(
                "zfs.pool.projection",
                format!("{name}/{pool}"),
                ConditionSeverity::Warning,
                format!(
                    "{name}: pool {pool} will be full in about {days:.0} days at its current rate"
                ),
                vec![NextStep::run(
                    format!("Inspect {name}'s pool members and disk serials."),
                    "diagnostics.host_state",
                    &[("host", &name)],
                )],
            ));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::Membership;
    use serde_json::{Value, json};

    fn slot(n: u8) -> HostSlot {
        HostSlot::new(n).unwrap()
    }

    fn view(online: &[u8]) -> View {
        View {
            now_ms: 1000 * DAY_MS,
            served_by: Some(slot(1)),
            membership: Some(Membership {
                quorate: true,
                online: online.iter().map(|n| slot(*n)).collect(),
                members: (1..=3)
                    .map(|n| (slot(n), format!("10.0.0.{n}").parse().ok()))
                    .collect(),
            }),
            ..View::default()
        }
    }

    fn observe(view: &mut View, host: u8, collector: &str, data: Value) {
        view.observations
            .insert((slot(host), collector.to_string()), data);
    }

    fn listed(conditions: &[Condition]) -> Vec<String> {
        conditions
            .iter()
            .map(|c| format!("{} {} {:?}: {}", c.id, c.resource, c.severity, c.message))
            .collect()
    }

    fn peers(peers: Value) -> Value {
        json!({"at_ms": 1, "name": "lab", "quorate": true, "expected_votes": 3,
               "total_votes": 3, "quorum": 2, "qdevice_registered": false,
               "qdevice_address": null, "qdevice_voting": null, "peers": peers, "errors": []})
    }

    fn peer(id: u32, links: &[bool]) -> Value {
        let links: serde_json::Map<String, Value> = links
            .iter()
            .enumerate()
            .map(|(i, up)| (i.to_string(), json!(up)))
            .collect();
        json!({"node_id": id, "name": format!("mox{id}"), "reachable": links.values().any(|v| v == true),
               "links": links})
    }

    #[test]
    fn a_healthy_cluster_has_no_conditions() {
        let mut v = view(&[1, 2, 3]);
        for n in 1..=3u32 {
            let others: Vec<Value> = (1..=3)
                .filter(|o| *o != n)
                .map(|o| peer(o, &[true, true]))
                .collect();
            observe(&mut v, n as u8, CLUSTER_COLLECTOR, peers(json!(others)));
            observe(
                &mut v,
                n as u8,
                UNITS_COLLECTOR,
                json!({"at_ms": 1, "required": {"corosync.service": "active"}, "failed": [],
                       "errors": []}),
            );
            observe(
                &mut v,
                n as u8,
                VRRP_COLLECTOR,
                json!({"at_ms": 1, "vip": "10.0.0.250", "holds_vip": n == 1,
                       "keepalived": "active", "errors": []}),
            );
        }
        let limits = Thresholds::default();
        assert_eq!(listed(&host_health(&v, &limits)), Vec::<String>::new());
        assert_eq!(listed(&vm_health(&v)), Vec::<String>::new());
        assert_eq!(listed(&storage_health(&v, &limits)), Vec::<String>::new());
    }

    #[test]
    fn a_down_link_is_named_once_per_pair() {
        let mut v = view(&[1, 2, 3]);
        observe(
            &mut v,
            1,
            CLUSTER_COLLECTOR,
            peers(json!([peer(2, &[true, false]), peer(3, &[true, true])])),
        );
        observe(
            &mut v,
            2,
            CLUSTER_COLLECTOR,
            peers(json!([peer(1, &[true, false]), peer(3, &[true, true])])),
        );
        assert_eq!(
            listed(&host_health(&v, &Thresholds::default())),
            [
                "corosync.link mox1-mox2/link1 Warning: Corosync link 1 between mox1 and mox2 is down"
            ]
        );
    }

    #[test]
    fn a_down_host_is_not_reported_link_by_link() {
        let mut v = view(&[1, 2]);
        observe(
            &mut v,
            1,
            CLUSTER_COLLECTOR,
            peers(json!([peer(2, &[true, true]), peer(3, &[false, false])])),
        );
        observe(
            &mut v,
            2,
            CLUSTER_COLLECTOR,
            peers(json!([peer(1, &[true, true]), peer(3, &[false, false])])),
        );
        assert_eq!(
            listed(&host_health(&v, &Thresholds::default())),
            Vec::<String>::new()
        );
    }

    #[test]
    fn units_tailscale_clock_and_cleanup() {
        let mut v = view(&[1, 2, 3]);
        observe(
            &mut v,
            2,
            UNITS_COLLECTOR,
            json!({"at_ms": 1, "required": {"corosync.service": "failed",
                   "pve-cluster.service": "active"},
                   "failed": ["corosync.service", "zfs-scrub.service"], "errors": []}),
        );
        observe(
            &mut v,
            3,
            TAILSCALE_COLLECTOR,
            json!({"at_ms": 1, "backend_state": "NeedsLogin", "name": null, "ipv4": null,
                   "errors": []}),
        );
        observe(
            &mut v,
            1,
            CLOCK_COLLECTOR,
            json!({"at_ms": 1, "synchronized": false, "leap_status": "Not synchronised",
                   "offset_seconds": null, "reference": null, "errors": []}),
        );
        let now = v.now_ms;
        observe(
            &mut v,
            1,
            CLEANUP_COLLECTOR,
            json!({"at_ms": 1, "waiting": 2, "oldest_ms": now - 30 * 60 * 60 * 1000,
                   "errors": []}),
        );
        assert_eq!(
            listed(&host_health(&v, &Thresholds::default())),
            [
                "clock.unsynchronized mox1 Warning: mox1: the clock is not synchronized (Not synchronised)",
                "cleanup.stuck mox1 Warning: mox1: 2 deferred cleanup(s) waiting, the oldest for over 30 hours",
                "unit.inactive mox2/corosync.service Warning: mox2: corosync.service is failed",
                "unit.failed mox2/zfs-scrub.service Warning: mox2: zfs-scrub.service failed",
                "tailscale.down mox3 Warning: mox3: Tailscale is NeedsLogin, so admins can't reach it over the tailnet",
            ]
        );
    }

    #[test]
    fn certificates_warn_then_go_critical() {
        let mut v = view(&[1, 2, 3]);
        let now = v.now_ms;
        observe(
            &mut v,
            1,
            CERTIFICATES_COLLECTOR,
            json!({"at_ms": 1, "certificates": [
                {"name": "dashboard", "path": "/a", "not_after_ms": now + 30 * DAY_MS, "error": null},
                {"name": "proxmox", "path": "/b", "not_after_ms": now + 3 * DAY_MS, "error": null},
                {"name": "other", "path": "/c", "not_after_ms": now + 365 * DAY_MS, "error": null},
            ]}),
        );
        observe(
            &mut v,
            2,
            CERTIFICATES_COLLECTOR,
            json!({"at_ms": 1, "certificates": [
                {"name": "proxmox", "path": "/b", "not_after_ms": now - DAY_MS, "error": null},
            ]}),
        );
        let found = host_health(&v, &Thresholds::default());
        assert_eq!(
            listed(&found),
            [
                "certificate.expiry mox1/dashboard Warning: mox1: the Dashboard HTTPS certificate expires in 30 days",
                "certificate.expiry mox1/proxmox Critical: mox1: the Proxmox web certificate expires in 3 days",
                "certificate.expiry mox2/proxmox Critical: mox2: the Proxmox web certificate has expired",
            ]
        );
        assert_eq!(found[0].next_steps[0].operation, None);
    }

    #[test]
    fn qdevice_address_drift_and_reachability() {
        let mut v = view(&[1, 2, 3]);
        observe(
            &mut v,
            1,
            QDEVICE_COLLECTOR,
            json!({"at_ms": 1, "name": "bmaclab-qdevice", "registered_address": "100.64.0.9",
                   "tailnet_address": "100.64.0.10", "qnetd_reachable": false, "errors": []}),
        );
        observe(
            &mut v,
            2,
            QDEVICE_COLLECTOR,
            json!({"at_ms": 1, "name": "bmaclab-qdevice", "registered_address": "100.64.0.9",
                   "tailnet_address": null, "qnetd_reachable": true, "errors": []}),
        );
        assert_eq!(
            listed(&host_health(&v, &Thresholds::default())),
            [
                "qdevice.address qdevice Critical: the QDevice is registered at 100.64.0.9, but its tailnet address is now 100.64.0.10",
                "qdevice.unreachable mox1/qdevice Critical: mox1 can't reach corosync-qnetd at 100.64.0.9:5403",
            ]
        );
    }

    fn vrrp(v: &mut View, host: u8, holds: bool) {
        observe(
            v,
            host,
            VRRP_COLLECTOR,
            json!({"at_ms": 1, "vip": "10.0.0.250", "holds_vip": holds, "keepalived": "active",
                   "errors": []}),
        );
    }

    #[test]
    fn exactly_one_online_host_holds_the_egress_vip() {
        let mut none = view(&[1, 2]);
        vrrp(&mut none, 1, false);
        vrrp(&mut none, 2, false);
        vrrp(&mut none, 3, true);
        assert_eq!(
            listed(&host_health(&none, &Thresholds::default())),
            [
                "vrrp.no_owner egress Critical: no online host holds the guest-egress VIP 10.0.0.250, so guests have no egress"
            ]
        );
        let mut two = view(&[1, 2, 3]);
        vrrp(&mut two, 1, true);
        vrrp(&mut two, 2, false);
        vrrp(&mut two, 3, true);
        assert_eq!(
            listed(&host_health(&two, &Thresholds::default())),
            [
                "vrrp.duplicate_owner egress Critical: more than one host holds the guest-egress VIP 10.0.0.250: mox1, mox3"
            ]
        );
    }

    #[test]
    fn ingress_pmxcfs_and_versions() {
        let mut v = view(&[1, 2, 3]);
        observe(
            &mut v,
            1,
            INGRESS_COLLECTOR,
            json!({"at_ms": 1, "serving": true, "desired_generation": "7",
                   "applied_generation": "6", "state": "ok", "haproxy": "failed", "errors": []}),
        );
        observe(
            &mut v,
            2,
            INGRESS_COLLECTOR,
            json!({"at_ms": 1, "serving": false, "desired_generation": "7",
                   "applied_generation": null, "state": null, "haproxy": "inactive",
                   "errors": []}),
        );
        observe(
            &mut v,
            3,
            PMXCFS_COLLECTOR,
            json!({"at_ms": 1, "bmac_bytes": 9 * MIB, "errors": ["du: partial"]}),
        );
        for (n, version) in [(1, "3.0.0"), (2, "3.0.0"), (3, "3.0.1")] {
            observe(
                &mut v,
                n,
                HOST_COLLECTOR,
                json!({"at_ms": 1, "bmac_version": version, "uptime_seconds": 1, "cores": 1,
                       "cpu_fraction": 0.1, "load1": 0.1, "memory_used": 1, "memory_total": 2,
                       "root_used": null, "root_total": null}),
            );
        }
        assert_eq!(
            listed(&host_health(&v, &Thresholds::default())),
            [
                "cluster.versions cluster Warning: hosts run different BMAC versions: 3.0.0 on mox1, mox2; 3.0.1 on mox3",
                "pmxcfs.usage cluster Warning: BMAC keeps 9.0 MiB in pmxcfs, which holds 128 MiB for everything",
                "ingress.not_applied mox1/ingress Critical: mox1 has not applied ingress generation 7 (it has 6)",
                "ingress.haproxy mox1/ingress Critical: mox1: HAProxy ingress is failed",
                "collector.pmxcfs mox3 Warning: mox3: du: partial",
            ]
        );
    }

    #[test]
    fn agents_and_registry_from_the_duty_host_only() {
        let mut v = view(&[1, 2, 3]);
        observe(
            &mut v,
            2,
            AGENTS_COLLECTOR,
            json!({"at_ms": 1, "agents": [
                {"vmid": 101, "name": "prod1", "answering": false, "error": "timeout"},
                {"vmid": 102, "name": "prod2", "answering": true, "error": null},
            ], "errors": []}),
        );
        let issue = |code: &str| {
            json!({"at_ms": 1, "issues": [{"severity": "error", "code": code,
                   "resource": "prod/web", "message": "the VM is missing"}], "errors": []})
        };
        observe(&mut v, 1, REGISTRY_COLLECTOR, issue("vm-missing"));
        observe(&mut v, 2, REGISTRY_COLLECTOR, issue("stale"));
        assert_eq!(
            listed(&vm_health(&v)),
            [
                "vm.agent prod1 Warning: prod1's guest agent on mox2 isn't answering",
                "registry.disagrees prod/web/vm-missing Warning: prod/web: the VM is missing (vm-missing)",
            ]
        );
    }

    #[test]
    fn backup_alerts_from_the_duty_host_only() {
        let mut v = view(&[1, 2, 3]);
        let seen = |name: &str| {
            json!({"at_ms": 1, "appliances": [
                {"appliance_uuid": "a1", "name": name, "state": "LATE", "locked": true,
                 "last_checkin_ms": 1},
                {"appliance_uuid": "a2", "name": "vault", "state": "DELINQUENT", "locked": false,
                 "last_checkin_ms": null},
                {"appliance_uuid": "a3", "name": "fresh", "state": "ACTIVE", "locked": false,
                 "last_checkin_ms": 1, "scrub_errors": 2, "suspect_pits": 3,
                 "other_copies": ["vault"]},
            ], "vms": [{
                "vm": "prod1", "vm_uuid": "u", "enabled": true,
                "last_attempt": {"outcome": "failed", "at_ms": 1, "message": "replication timed out"},
                "pits": 2, "snapshots": 1, "release_blocked": ["p1: dataset is busy"],
                "healthy_appliances": 1, "min_healthy": 2,
                "overdue": ["vault"], "unclaimed": ["offsite"],
            }, {
                "vm": "prod2", "vm_uuid": "v", "enabled": true,
                "last_attempt": {"outcome": "skipped", "at_ms": 1, "message": null},
                "pits": 1, "snapshots": 1, "release_blocked": [],
                "healthy_appliances": 1, "min_healthy": 1, "overdue": [], "unclaimed": [],
            }], "errors": ["policy unreadable"]})
        };
        observe(&mut v, 1, BACKUPS_COLLECTOR, seen("offsite"));
        observe(&mut v, 2, BACKUPS_COLLECTOR, seen("other"));
        assert_eq!(
            listed(&vm_health(&v)),
            [
                "backup.appliance.late appliance/a1 Warning: backup appliance offsite hasn't checked in",
                "backup.appliance.locked appliance/a1 Warning: backup appliance offsite is locked and can't receive backups",
                "backup.appliance.delinquent appliance/a2 Critical: backup appliance vault is delinquent; the cluster no longer keeps snapshots for it",
                "backup.appliance.scrub_errors appliance/a3 Critical: backup appliance fresh's scrub found 2 unrepairable errors; 3 PITs are suspect; other copies are on vault",
                "backup.capture.failed vm/prod1 Critical: prod1's last backup failed: replication timed out",
                "backup.unclaimed vm/prod1/offsite Warning: offsite didn't take prod1's newest backup",
                "backup.overdue vm/prod1/vault Critical: prod1's newest backup on vault is older than its schedule allows",
                "backup.too_few vm/prod1 Critical: prod1 has 1 healthy backup appliance(s); at least 2 are wanted",
                "backup.release_blocked vm/prod1 Warning: prod1: a backup snapshot can't be released (p1: dataset is busy)",
                "backup.skipped vm/prod2 Critical: prod2's last scheduled backup was skipped: none of its appliances was available",
                "collector.backups mox1 Warning: mox1: policy unreadable",
                "collector.backups mox2 Warning: mox2: policy unreadable",
            ]
        );
    }

    fn storage(findings: Value, luks: Value) -> Value {
        json!({"at_ms": 1, "pools": [], "storages": [], "vdevs": [], "disks": [],
               "findings": findings, "luks_mismatches": luks, "errors": []})
    }

    fn smart(reallocated: u64, pending: u64, health: &str) -> Value {
        json!({"at_ms": 1, "disks": [{"disk": "/dev/sda", "serial": "S1", "model": null,
               "health": health, "verdict": null, "reallocated_sectors": reallocated,
               "pending_sectors": pending, "media_errors": 0}], "errors": []})
    }

    #[test]
    fn pool_members_and_luks() {
        let mut v = view(&[1, 2, 3]);
        observe(
            &mut v,
            1,
            STORAGE_COLLECTOR,
            storage(
                json!([
                    {"pool": "rpool", "message": "pool rpool is DEGRADED"},
                    {"pool": "rpool", "message": "pool rpool is resilvering"},
                    {"pool": "rpool", "message": "rpool member sdb (S2) is FAULTED"},
                ]),
                json!(["crypt-sdb is in crypttab but not open"]),
            ),
        );
        assert_eq!(
            listed(&storage_health(&v, &Thresholds::default())),
            [
                "zfs.pool.member mox1/rpool Warning: mox1: pool rpool is resilvering",
                "zfs.pool.member mox1/rpool Critical: mox1: rpool member sdb (S2) is FAULTED",
                "luks.mismatch mox1/luks Critical: mox1: crypt-sdb is in crypttab but not open",
            ]
        );
    }

    #[test]
    fn smart_failure_and_wear_against_the_first_count_seen() {
        let mut v = view(&[1, 2, 3]);
        observe(&mut v, 1, SMART_COLLECTOR, smart(8, 0, "passed"));
        v.smart_baseline.insert((slot(1), "/dev/sda".into()), 8);
        assert_eq!(
            listed(&storage_health(&v, &Thresholds::default())),
            Vec::<String>::new()
        );
        observe(&mut v, 1, SMART_COLLECTOR, smart(9, 1, "failing"));
        assert_eq!(
            listed(&storage_health(&v, &Thresholds::default())),
            [
                "disk.smart mox1/sda Critical: mox1: disk sda S1 reports SMART failure",
                "disk.wear mox1/sda Warning: mox1: disk sda S1 has 9 reallocated and 1 pending sectors",
            ]
        );
    }

    #[test]
    fn notification_trouble_comes_from_the_duty_host() {
        let mut v = view(&[1, 2, 3]);
        let now = v.now_ms;
        let notify = |unauthorized: bool| {
            json!({"at_ms": 1, "token_set": true, "sender_set": true, "unauthorized": unauthorized,
                   "templates_missing": true, "sender_refused": true, "held": 3,
                   "inactive_recipients": ["a@example.com"],
                   "failing_since_ms": now - 2 * 60 * 60 * 1000, "last_error": "HTTP 503"})
        };
        observe(&mut v, 2, NOTIFY_COLLECTOR, notify(true));
        assert_eq!(
            listed(&host_health(&v, &Thresholds::default())),
            Vec::<String>::new(),
            "mox2 isn't sending"
        );
        observe(&mut v, 1, NOTIFY_COLLECTOR, notify(true));
        assert_eq!(
            listed(&host_health(&v, &Thresholds::default())),
            [
                "notify.unauthorized postmark Critical: Postmark refused the server token, so no email is sent until it is replaced",
                "notify.sender_refused postmark Critical: Postmark refused the sender address, so no email is sent until it is confirmed in Postmark or changed",
                "notify.templates_missing postmark Warning: Notification templates missing on the Postmark server; 3 message(s) are held until they are created",
                "notify.inactive_recipient a@example.com Warning: a@example.com is inactive in Postmark and gets no email until it is reactivated there",
                "notify.failing postmark Warning: email has not reached Postmark for 2 hour(s): HTTP 503",
            ]
        );
    }

    #[test]
    fn lagging_copies_and_evicted_audit_events_are_raised() {
        let mut v = view(&[1, 2, 3]);
        let now = v.now_ms;
        observe(
            &mut v,
            1,
            LOGS_COLLECTOR,
            json!({"at_ms": now, "used_bytes": 10, "budget_bytes": 100, "oldest_ms": 1,
                   "audit_evicted_ms": now - DAY_MS, "peers_checked": 2,
                   "lagging": [{"peer": "mox2", "behind_records": 40, "behind_ms": 6 * 60_000},
                               {"peer": "mox3", "behind_records": 2, "behind_ms": 30_000}]}),
        );
        assert_eq!(
            listed(&host_health(&v, &Thresholds::default())),
            [
                "logs.copy_lag mox1/mox2 Warning: mox2's copy of mox1's logs is 6 minute(s) behind (40 record(s))",
                "logs.audit_evicted mox1 Warning: mox1: log storage ran out of room and evicted audit events or failure records",
            ]
        );
        v.now_ms += 7 * DAY_MS;
        assert_eq!(
            listed(&host_health(&v, &Thresholds::default())).len(),
            1,
            "the eviction is old news"
        );
    }

    #[test]
    fn pools_filling_soon_are_projected() {
        let mut v = view(&[1, 2, 3]);
        v.pool_full_days.insert((slot(2), "tank".into()), 9.6);
        v.pool_full_days.insert((slot(2), "rpool".into()), 200.0);
        assert_eq!(
            listed(&storage_health(&v, &Thresholds::default())),
            [
                "zfs.pool.projection mox2/tank Warning: mox2: pool tank will be full in about 10 days at its current rate"
            ]
        );
    }
}
