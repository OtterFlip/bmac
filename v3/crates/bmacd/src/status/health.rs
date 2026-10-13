// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The health collectors beyond the four the Dashboard pages are built
//! from (`MONITORING.md`, "Collectors"): units, SMART, Tailscale, clock,
//! certificates, pmxcfs use, deferred cleanup, the egress VIP, ingress, and
//! guest agents. Like the others, each reads only this host with read-only
//! commands and files, and reports a failed read as an error in its
//! observation.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bmac_adapters::disks::{Disks, SmartHealth};
use bmac_adapters::systemd::Systemd;
use bmac_adapters::tailscale::Tailscale;
use bmac_adapters::{AdapterError, Collection, programs, run_text};
use bmac_core::HostSlot;
use bmac_exec::{Command, CommandRunner};
use bmac_proto::status::{
    AgentsObservation, CertificateExpiry, CertificatesObservation, CleanupObservation,
    ClockObservation, GuestAgent, IngressObservation, PmxcfsObservation, QdeviceObservation,
    RegistryIssue, RegistryObservation, SmartDisk, SmartObservation, TailscaleObservation,
    UnitsObservation, VrrpObservation,
};
use bmac_registry::{CleanupState, Kind, Registry, ResourceState};

/// The units every host runs (v2 `show_proxmox_host_state.sh` 175–196).
pub const REQUIRED_UNITS: &[&str] = &[
    "pve-cluster.service",
    "corosync.service",
    "pve-ha-lrm.service",
    "pve-ha-crm.service",
    "keepalived.service",
    "bmac-deferred-cleanup.timer",
    "bmac-haproxy-route-sync.timer",
];
/// Required as well while a QDevice is registered.
pub const QDEVICE_UNIT: &str = "corosync-qdevice.service";
pub const KEEPALIVED_UNIT: &str = "keepalived.service";
pub const INGRESS_UNIT: &str = "bmac-haproxy-ingress.service";
const AGENT_TIMEOUT: Duration = Duration::from_secs(10);
pub const QNETD_PORT: u16 = 5403;
const QNETD_TIMEOUT: Duration = Duration::from_secs(5);

fn adapter_error(command: &str, message: impl std::fmt::Display) -> AdapterError {
    AdapterError::Parse {
        command: command.to_string(),
        message: message.to_string(),
    }
}

/// `units`: each required unit's state, and every failed unit.
pub async fn units(
    runner: &dyn CommandRunner,
    qdevice_registered: bool,
    at_ms: i64,
) -> UnitsObservation {
    let systemd = Systemd::new(runner);
    let mut errors = Collection::default();
    let mut required = std::collections::BTreeMap::new();
    let wanted = REQUIRED_UNITS
        .iter()
        .copied()
        .chain(qdevice_registered.then_some(QDEVICE_UNIT));
    for unit in wanted {
        if let Some(state) = errors.take(systemd.active_state(unit).await) {
            required.insert(unit.to_string(), state);
        }
    }
    let failed = errors
        .take(systemd.failed_units().await)
        .unwrap_or_default();
    UnitsObservation {
        at_ms,
        required,
        failed,
        errors: errors.errors,
    }
}

/// `smart`: every physical disk's health and wear counters.
pub async fn smart(runner: &dyn CommandRunner, at_ms: i64) -> SmartObservation {
    let adapter = Disks::new(runner);
    let mut errors = Collection::default();
    let devices = errors
        .take(adapter.block_devices().await)
        .unwrap_or_default();
    let mut disks = Vec::new();
    for device in devices.physical_disks() {
        let path = device.name.clone().unwrap_or_else(|| device.kname.clone());
        let (health, counters) = adapter.smart_report(&path).await;
        let (health, verdict) = match health {
            SmartHealth::Passed { verdict } => ("passed", Some(verdict)),
            SmartHealth::Failing { verdict } => ("failing", Some(verdict)),
            SmartHealth::Unavailable { .. } => ("unavailable", None),
            SmartHealth::TimedOut => ("timed_out", None),
            SmartHealth::NotInstalled => ("not_installed", None),
        };
        disks.push(SmartDisk {
            disk: path,
            serial: device.serial.clone(),
            model: device.model.clone(),
            health: health.to_string(),
            verdict,
            reallocated_sectors: counters.reallocated_sectors,
            pending_sectors: counters.pending_sectors,
            media_errors: counters.media_errors,
        });
    }
    SmartObservation {
        at_ms,
        disks,
        errors: errors.errors,
    }
}

/// `tailscale`: whether this host's client is connected, and as what.
pub async fn tailscale(runner: &dyn CommandRunner, at_ms: i64) -> TailscaleObservation {
    let mut errors = Collection::default();
    let status = errors.take(Tailscale::new(runner).status().await);
    let node = status.as_ref().and_then(|s| s.self_node.as_ref());
    TailscaleObservation {
        at_ms,
        backend_state: status.as_ref().map(|s| s.backend_state.clone()),
        name: node.map(|n| n.short_name().to_string()),
        ipv4: node.and_then(|n| n.ipv4()).map(|ip| ip.to_string()),
        errors: errors.errors,
    }
}

/// Parses `chronyc -c tracking`: reference ID, reference name, stratum,
/// reference time, system time offset, ..., leap status (field 14).
pub fn parse_tracking(text: &str, at_ms: i64) -> Result<ClockObservation, String> {
    let fields: Vec<&str> = text.trim().split(',').collect();
    if fields.len() < 14 {
        return Err(format!("expected 14 fields, got {}", fields.len()));
    }
    let leap = fields[13].trim().to_string();
    let stratum: Option<u32> = fields[2].trim().parse().ok();
    Ok(ClockObservation {
        at_ms,
        synchronized: Some(leap != "Not synchronised" && stratum.is_some_and(|s| s > 0)),
        leap_status: Some(leap),
        offset_seconds: fields[4].trim().parse().ok(),
        reference: Some(fields[1].trim().to_string()).filter(|r| !r.is_empty()),
        errors: Vec::new(),
    })
}

/// `clock`: time synchronization.
pub async fn clock(runner: &dyn CommandRunner, at_ms: i64) -> ClockObservation {
    let command = Command::new(programs::CHRONYC).args(["-c", "tracking"]);
    let display = command.to_string();
    let parsed = run_text(runner, command)
        .await
        .and_then(|text| parse_tracking(&text, at_ms).map_err(|e| adapter_error(&display, e)));
    parsed.unwrap_or_else(|error| ClockObservation {
        at_ms,
        synchronized: None,
        leap_status: None,
        offset_seconds: None,
        reference: None,
        errors: vec![error.to_string()],
    })
}

/// `certificates`: when each named certificate file expires. Of several
/// paths for one name, the first that exists is used.
pub fn certificates(files: &[(&str, Vec<PathBuf>)], at_ms: i64) -> CertificatesObservation {
    let certificates = files
        .iter()
        .map(|(name, paths)| {
            let path = paths.iter().find(|p| p.exists()).or(paths.first());
            let read = match path {
                Some(path) => std::fs::read(path)
                    .map_err(|e| e.to_string())
                    .and_then(|pem| crate::certificate::info(&pem)),
                None => Err("no path".to_string()),
            };
            CertificateExpiry {
                name: name.to_string(),
                path: path.map(|p| p.display().to_string()).unwrap_or_default(),
                not_after_ms: read.as_ref().ok().map(|i| i.not_after_ms),
                error: read.err(),
            }
        })
        .collect();
    CertificatesObservation {
        at_ms,
        certificates,
    }
}

fn tree_size(path: &Path) -> std::io::Result<u64> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        total += tree_size(&entry?.path())?;
    }
    Ok(total)
}

/// `pmxcfs`: the bytes BMAC keeps in pmxcfs, which holds 128 MiB in all.
pub fn pmxcfs(dir: &Path, at_ms: i64) -> PmxcfsObservation {
    match tree_size(dir) {
        Ok(bytes) => PmxcfsObservation {
            at_ms,
            bmac_bytes: Some(bytes),
            errors: Vec::new(),
        },
        Err(e) => PmxcfsObservation {
            at_ms,
            bmac_bytes: None,
            errors: vec![format!("{}: {e}", dir.display())],
        },
    }
}

fn timestamp_ms(text: &str) -> Option<i64> {
    text.parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_millisecond())
}

/// `cleanup`: deferred cleanups still waiting to run on this host.
pub fn cleanup(registry: &Registry, host: HostSlot, at_ms: i64) -> CleanupObservation {
    if !registry.root().exists() {
        return CleanupObservation {
            at_ms,
            waiting: 0,
            oldest_ms: None,
            errors: Vec::new(),
        };
    }
    match registry.cleanup_records() {
        Ok(records) => {
            let waiting: Vec<_> = records
                .iter()
                .filter(|r| r.node == host && r.state == CleanupState::Pending)
                .collect();
            CleanupObservation {
                at_ms,
                waiting: waiting.len() as u32,
                oldest_ms: waiting
                    .iter()
                    .filter_map(|r| timestamp_ms(&r.created_at))
                    .min(),
                errors: Vec::new(),
            }
        }
        Err(e) => CleanupObservation {
            at_ms,
            waiting: 0,
            oldest_ms: None,
            errors: vec![format!("the deferred cleanup records can't be read: {e}")],
        },
    }
}

/// Whether `ip -4 -o address show` lists `vip` (v2 `add_proxmox_host.sh`
/// 5031–5039 matches field 4 against `<vip>/`).
pub fn holds_address(ip_output: &str, vip: &str) -> bool {
    let prefix = format!("{vip}/");
    ip_output.lines().any(|line| {
        line.split_whitespace()
            .nth(3)
            .is_some_and(|a| a.starts_with(&prefix))
    })
}

/// `vrrp`: whether this host holds the guest-egress VIP on the private
/// bridge. The duty host checks that exactly one host does.
pub async fn vrrp(
    runner: &dyn CommandRunner,
    vip: Option<&str>,
    bridge: Option<&str>,
    at_ms: i64,
) -> VrrpObservation {
    let mut errors = Collection::default();
    let keepalived = errors.take(Systemd::new(runner).active_state(KEEPALIVED_UNIT).await);
    let holds_vip = match (vip, bridge) {
        (Some(vip), Some(bridge)) => errors
            .take(
                run_text(
                    runner,
                    Command::new(programs::IP).args(["-4", "-o", "address", "show", "dev", bridge]),
                )
                .await,
            )
            .map(|text| holds_address(&text, vip)),
        _ => None,
    };
    VrrpObservation {
        at_ms,
        vip: vip.map(str::to_string),
        holds_vip,
        keepalived,
        errors: errors.errors,
    }
}

/// `ingress`: this host's part in the desired ingress generation.
pub async fn ingress(
    runner: &dyn CommandRunner,
    registry: &Registry,
    host: HostSlot,
    at_ms: i64,
) -> IngressObservation {
    let mut observation = IngressObservation {
        at_ms,
        serving: false,
        desired_generation: None,
        applied_generation: None,
        state: None,
        haproxy: None,
        errors: Vec::new(),
    };
    if !registry.root().exists() {
        return observation;
    }
    match registry.ingress_desired() {
        Ok(Some(desired)) => {
            observation.serving = desired.nodes.contains(&host);
            observation.desired_generation = Some(desired.generation);
        }
        Ok(None) => {}
        Err(e) => observation
            .errors
            .push(format!("the desired ingress can't be read: {e}")),
    }
    match registry.ingress_node_statuses() {
        Ok(statuses) => {
            if let Some(status) = statuses.into_iter().find(|s| s.node == host) {
                observation.applied_generation = status.applied_generation;
                observation.state = Some(status.state.as_str().to_string());
            }
        }
        Err(e) => observation
            .errors
            .push(format!("the ingress status can't be read: {e}")),
    }
    if observation.serving {
        match Systemd::new(runner).active_state(INGRESS_UNIT).await {
            Ok(state) => observation.haproxy = Some(state),
            Err(e) => observation.errors.push(e.to_string()),
        }
    }
    observation
}

/// `agents`: pings the guest agent of each active production VM that runs
/// on this host.
pub async fn agents(
    runner: &dyn CommandRunner,
    registry: &Registry,
    running: &[bmac_core::Vmid],
    at_ms: i64,
) -> AgentsObservation {
    let mut observation = AgentsObservation {
        at_ms,
        agents: Vec::new(),
        errors: Vec::new(),
    };
    if !registry.root().exists() {
        return observation;
    }
    let resources = match registry.resources() {
        Ok(resources) => resources,
        Err(e) => {
            observation
                .errors
                .push(format!("the registry can't be read: {e}"));
            return observation;
        }
    };
    for resource in resources {
        if resource.kind != Kind::Production
            || resource.state != ResourceState::Active
            || !running.contains(&resource.vmid)
        {
            continue;
        }
        let command = Command::new(programs::QM)
            .args(["guest", "cmd", &resource.vmid.to_string(), "ping"])
            .timeout(AGENT_TIMEOUT);
        let result = bmac_adapters::run(runner, command).await;
        observation.agents.push(GuestAgent {
            vmid: resource.vmid,
            name: resource.name,
            answering: result.is_ok(),
            error: result.err().map(|e| e.to_string()),
        });
    }
    observation
}

/// `qdevice`: whether the registered address is still the QDevice's
/// tailnet address, and whether qnetd answers there. Without a registered
/// QDevice there is nothing to check.
pub async fn qdevice(
    runner: &dyn CommandRunner,
    name: Option<&str>,
    registered_address: Option<&str>,
    at_ms: i64,
) -> QdeviceObservation {
    qdevice_on(runner, name, registered_address, QNETD_PORT, at_ms).await
}

async fn qdevice_on(
    runner: &dyn CommandRunner,
    name: Option<&str>,
    registered_address: Option<&str>,
    port: u16,
    at_ms: i64,
) -> QdeviceObservation {
    let mut observation = QdeviceObservation {
        at_ms,
        name: name.map(str::to_string),
        registered_address: registered_address.map(str::to_string),
        tailnet_address: None,
        qnetd_reachable: None,
        errors: Vec::new(),
    };
    let Some(address) = registered_address else {
        return observation;
    };
    if let Some(name) = name {
        match Tailscale::new(runner).status().await {
            Ok(status) => {
                observation.tailnet_address = status
                    .peers_named(name)
                    .find_map(|n| n.ipv4())
                    .map(|ip| ip.to_string());
            }
            Err(e) => observation.errors.push(e.to_string()),
        }
    }
    let target = match address.parse::<std::net::IpAddr>() {
        Ok(ip) => std::net::SocketAddr::new(ip, port),
        Err(_) => {
            observation.errors.push(format!(
                "the registered QDevice address {address} is not an IP address"
            ));
            return observation;
        }
    };
    let connected =
        tokio::time::timeout(QNETD_TIMEOUT, tokio::net::TcpStream::connect(target)).await;
    observation.qnetd_reachable = Some(matches!(connected, Ok(Ok(_))));
    observation
}

/// `GET /nodes`.
#[derive(Debug, serde::Deserialize)]
struct NodeRow {
    node: String,
    #[serde(default)]
    status: Option<String>,
}

/// `GET /cluster/resources --type vm`.
#[derive(Debug, serde::Deserialize)]
struct VmRow {
    vmid: bmac_core::Vmid,
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// `registry`: the registry against the nodes, VMs, and VM configurations
/// Proxmox reports (v2 `collect_live_observation` 4071–4193).
pub async fn registry(
    runner: &dyn CommandRunner,
    registry: &Registry,
    at_ms: i64,
) -> RegistryObservation {
    use bmac_registry::reconcile::{self, Observed, ObservedVm, Severity};
    let mut observation = RegistryObservation {
        at_ms,
        issues: Vec::new(),
        errors: Vec::new(),
    };
    if !registry.root().exists() {
        return observation;
    }
    let (resources, policy) = match (registry.resources(), registry.policy()) {
        (Ok(r), Ok(p)) => (r, p),
        (Err(e), _) | (_, Err(e)) => {
            observation
                .errors
                .push(format!("the registry can't be read: {e}"));
            return observation;
        }
    };
    let pvesh = bmac_adapters::proxmox::Pvesh::new(runner);
    let mut errors = Collection::default();
    let (Some(nodes), Some(vms)) = (
        errors.take(pvesh.get::<Vec<NodeRow>>("/nodes", &[]).await),
        errors.take(
            pvesh
                .get::<Vec<VmRow>>("/cluster/resources", &["--type", "vm"])
                .await,
        ),
    ) else {
        observation.errors = errors.errors;
        return observation;
    };
    let mut observed = Observed {
        nodes: nodes
            .into_iter()
            .map(|n| (n.node, n.status.as_deref() == Some("online")))
            .collect(),
        vms: Default::default(),
    };
    for resource in &resources {
        let Some(vm) = vms.iter().find(|v| v.vmid == resource.vmid) else {
            continue;
        };
        let Some(node) = vm.node.clone() else {
            continue;
        };
        let path = format!("/nodes/{node}/qemu/{}/config", resource.vmid);
        let Some(config) = errors.take(
            pvesh
                .get::<serde_json::Map<String, serde_json::Value>>(&path, &[])
                .await,
        ) else {
            // Left out, the VM would read as missing, so this round reports
            // only the error.
            return RegistryObservation {
                at_ms,
                issues: Vec::new(),
                errors: errors.errors,
            };
        };
        observed.vms.insert(
            resource.vmid,
            ObservedVm {
                vmid: resource.vmid,
                name: config
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
                    .or(vm.name.clone())
                    .unwrap_or_default(),
                node,
                status: vm.status.clone().unwrap_or_else(|| "unknown".to_string()),
                mac: config
                    .get("net0")
                    .and_then(|n| n.as_str())
                    .and_then(reconcile::net0_mac),
                tags: reconcile::config_tags(&config),
                volume_ids: reconcile::disk_volumes(&config),
            },
        );
    }
    observation.issues = reconcile::live(&resources, &policy, &observed, at_ms)
        .into_iter()
        .map(|i| RegistryIssue {
            severity: match i.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            }
            .to_string(),
            code: i.code.to_string(),
            resource: i.resource,
            message: i.message,
        })
        .collect();
    observation.errors = errors.errors;
    observation
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_exec::{FakeRunner, Reply};

    #[tokio::test]
    async fn units_report_each_required_unit_and_the_failed_ones() {
        let fake = FakeRunner::new();
        for unit in REQUIRED_UNITS.iter().chain([&QDEVICE_UNIT]) {
            let state = if *unit == "pve-ha-lrm.service" {
                "failed\n"
            } else {
                "active\n"
            };
            fake.expect(programs::SYSTEMCTL, ["is-active", unit], Reply::ok(state));
        }
        fake.expect(
            programs::SYSTEMCTL,
            [
                "list-units",
                "--failed",
                "--plain",
                "--no-legend",
                "--no-pager",
            ],
            Reply::ok("pve-ha-lrm.service loaded failed failed PVE LRM\n"),
        );
        let seen = units(&fake, true, 3).await;
        assert!(seen.errors.is_empty(), "{:?}", seen.errors);
        assert_eq!(seen.required.len(), REQUIRED_UNITS.len() + 1);
        assert_eq!(seen.required["pve-ha-lrm.service"], "failed");
        assert_eq!(seen.required[QDEVICE_UNIT], "active");
        assert_eq!(seen.failed, ["pve-ha-lrm.service"]);
    }

    #[test]
    fn chrony_tracking_says_whether_the_clock_is_synchronized() {
        let synced = "A9FEA97B,169.254.169.123,4,1760220000.1,-0.000012345,0.000001,0.00002,-5.1,0.001,0.02,0.0003,0.00001,64.2,Normal\n";
        let seen = parse_tracking(synced, 1).unwrap();
        assert_eq!(seen.synchronized, Some(true));
        assert_eq!(seen.offset_seconds, Some(-0.000012345));
        assert_eq!(seen.reference.as_deref(), Some("169.254.169.123"));
        let unsynced = "00000000,,0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,1.0,1.0,0.0,Not synchronised\n";
        assert_eq!(
            parse_tracking(unsynced, 1).unwrap().synchronized,
            Some(false)
        );
        assert!(parse_tracking("garbage", 1).is_err());
    }

    #[test]
    fn the_vip_is_held_only_when_listed_with_its_prefix() {
        let text = "4: vmbr1    inet 10.10.0.11/24 brd 10.10.0.255 scope global vmbr1\\       valid_lft forever\n\
                    4: vmbr1    inet 10.10.0.254/32 scope global vmbr1\\       valid_lft forever\n";
        assert!(holds_address(text, "10.10.0.254"));
        assert!(!holds_address(text, "10.10.0.25"));
        assert!(!holds_address("", "10.10.0.254"));
    }

    #[test]
    fn pmxcfs_use_adds_up_every_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/one"), [0u8; 100]).unwrap();
        std::fs::write(dir.path().join("a/b/two"), [0u8; 23]).unwrap();
        assert_eq!(pmxcfs(dir.path(), 1).bmac_bytes, Some(123));
        let missing = pmxcfs(&dir.path().join("none"), 1);
        assert_eq!(missing.bmac_bytes, None);
        assert_eq!(missing.errors.len(), 1);
    }

    #[test]
    fn certificates_use_the_first_file_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["mox1".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        std::fs::write(dir.path().join("pve-ssl.pem"), cert.pem()).unwrap();
        let seen = certificates(
            &[
                (
                    "proxmox",
                    vec![
                        dir.path().join("pveproxy-ssl.pem"),
                        dir.path().join("pve-ssl.pem"),
                    ],
                ),
                ("dashboard", vec![dir.path().join("https.pem")]),
            ],
            1,
        );
        assert!(seen.certificates[0].path.ends_with("pve-ssl.pem"));
        assert!(seen.certificates[0].not_after_ms.is_some());
        assert!(seen.certificates[1].error.is_some());
    }

    #[tokio::test]
    async fn the_qdevice_is_checked_at_its_registered_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let fake = FakeRunner::new();
        fake.expect(
            programs::TAILSCALE,
            ["status", "--json"],
            Reply::ok(r#"{"BackendState":"Running","Peer":{"k":{"DNSName":"qdevice.example.ts.net.","TailscaleIPs":["100.64.0.9"]}}}"#),
        );
        let seen = qdevice_on(&fake, Some("qdevice"), Some("127.0.0.1"), port, 1).await;
        assert_eq!(seen.tailnet_address.as_deref(), Some("100.64.0.9"));
        assert!(seen.errors.is_empty(), "{:?}", seen.errors);
        assert_eq!(seen.qnetd_reachable, Some(true));
        drop(listener);
        let closed = qdevice_on(&FakeRunner::new(), None, Some("127.0.0.1"), port, 1).await;
        assert_eq!(closed.qnetd_reachable, Some(false));
        let none = qdevice(&FakeRunner::new(), Some("qdevice"), None, 1).await;
        assert_eq!(
            none.qnetd_reachable, None,
            "no registered QDevice, nothing to check"
        );
    }

    #[tokio::test]
    async fn tailscale_reports_its_state_name_and_address() {
        let fake = FakeRunner::new();
        fake.expect(
            programs::TAILSCALE,
            ["status", "--json"],
            Reply::ok(r#"{"BackendState":"Running","Self":{"HostName":"mox1","DNSName":"mox1.example.ts.net.","TailscaleIPs":["100.64.0.11","fd7a::1"],"Online":true}}"#),
        );
        let seen = tailscale(&fake, 1).await;
        assert_eq!(seen.backend_state.as_deref(), Some("Running"));
        assert_eq!(seen.name.as_deref(), Some("mox1"));
        assert_eq!(seen.ipv4.as_deref(), Some("100.64.0.11"));
    }
}
