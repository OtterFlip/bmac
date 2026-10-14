// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Cluster status (`MONITORING.md`): this host's collectors, published as
//! observations that the host-to-host streams carry to every peer, and the
//! snapshots the API serves from every host's newest observations.

pub mod assemble;
pub mod collect;
pub mod conditions;
pub mod disks;
pub mod health;
pub mod host;
pub mod journal;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_adapters::layout::LocalHost;
use bmac_core::HostSlot;
use bmac_exec::CommandRunner;
use bmac_proto::status::{
    AGENTS_COLLECTOR, BACKUPS_COLLECTOR, CERTIFICATES_COLLECTOR, CLEANUP_COLLECTOR,
    CLOCK_COLLECTOR, CLUSTER_COLLECTOR, ClusterSnapshot, Condition, GUESTS_COLLECTOR,
    HOST_COLLECTOR, HostsSnapshot, INGRESS_COLLECTOR, PMXCFS_COLLECTOR, QDEVICE_COLLECTOR,
    REGISTRY_COLLECTOR, SMART_COLLECTOR, STORAGE_COLLECTOR, StorageSnapshot, TAILSCALE_COLLECTOR,
    UNITS_COLLECTOR, VRRP_COLLECTOR, VmsSnapshot,
};
use bmac_proto::stream::StreamBody;
use bmac_registry::Registry;
use serde_json::Value;

use self::assemble::View;
use crate::auth::Clock;
use crate::cluster::ClusterView;
use crate::engine::events::EventHub;

/// How often each collector runs.
pub const HOST_EVERY: Duration = Duration::from_secs(15);
pub const CLUSTER_EVERY: Duration = Duration::from_secs(15);
pub const GUESTS_EVERY: Duration = Duration::from_secs(30);
pub const STORAGE_EVERY: Duration = Duration::from_secs(60);
/// The health collectors' intervals (`MONITORING.md`, "Collectors").
pub const SCHEDULE: &[(&str, Duration)] = &[
    (HOST_COLLECTOR, HOST_EVERY),
    (CLUSTER_COLLECTOR, CLUSTER_EVERY),
    (GUESTS_COLLECTOR, GUESTS_EVERY),
    (STORAGE_COLLECTOR, STORAGE_EVERY),
    (VRRP_COLLECTOR, Duration::from_secs(15)),
    (INGRESS_COLLECTOR, Duration::from_secs(30)),
    (AGENTS_COLLECTOR, Duration::from_secs(60)),
    (UNITS_COLLECTOR, Duration::from_secs(60)),
    (TAILSCALE_COLLECTOR, Duration::from_secs(60)),
    (QDEVICE_COLLECTOR, Duration::from_secs(60)),
    (CLEANUP_COLLECTOR, Duration::from_secs(5 * 60)),
    (REGISTRY_COLLECTOR, Duration::from_secs(5 * 60)),
    (BACKUPS_COLLECTOR, Duration::from_secs(60)),
    (CLOCK_COLLECTOR, Duration::from_secs(5 * 60)),
    (PMXCFS_COLLECTOR, Duration::from_secs(5 * 60)),
    (SMART_COLLECTOR, Duration::from_secs(60 * 60)),
    (CERTIFICATES_COLLECTOR, Duration::from_secs(24 * 60 * 60)),
];
/// An unchanged observation is published again after this long, so peers
/// can tell it is current.
pub const HEARTBEAT: Duration = Duration::from_secs(120);
/// A condition not seen for this long starts afresh when it returns.
const FORGET_CONDITION_MS: i64 = 10 * 60 * 1000;

/// Builds the snapshots for the engine's status calls.
pub struct StatusSource {
    host: Option<HostSlot>,
    hub: Arc<EventHub>,
    cluster: Arc<dyn ClusterView>,
    registry: Registry,
    clock: Clock,
    /// Each condition's first and last sighting, for its `since_ms`.
    seen: Mutex<HashMap<(String, String), (i64, i64)>>,
    /// Each disk's lowest reallocated-sector count seen.
    smart_baseline: Mutex<std::collections::BTreeMap<(HostSlot, String), u64>>,
    projections: crate::metrics::Projections,
}

impl std::fmt::Debug for StatusSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatusSource")
            .field("host", &self.host)
            .finish()
    }
}

impl StatusSource {
    pub fn new(
        host: Option<HostSlot>,
        hub: Arc<EventHub>,
        cluster: Arc<dyn ClusterView>,
        registry: Registry,
        clock: Clock,
    ) -> StatusSource {
        StatusSource {
            host,
            hub,
            cluster,
            registry,
            clock,
            seen: Mutex::new(HashMap::new()),
            smart_baseline: Mutex::new(Default::default()),
            projections: Default::default(),
        }
    }

    /// Reads pool-full projections from the metric history's task.
    pub fn with_projections(mut self, projections: crate::metrics::Projections) -> StatusSource {
        self.projections = projections;
        self
    }

    fn smart_baseline(
        &self,
        observations: &std::collections::BTreeMap<(HostSlot, String), Value>,
    ) -> std::collections::BTreeMap<(HostSlot, String), u64> {
        let mut baseline = self
            .smart_baseline
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for ((host, collector), data) in observations {
            if collector != SMART_COLLECTOR {
                continue;
            }
            for disk in data["disks"].as_array().into_iter().flatten() {
                let (Some(path), Some(count)) =
                    (disk["disk"].as_str(), disk["reallocated_sectors"].as_u64())
                else {
                    continue;
                };
                baseline
                    .entry((*host, path.to_string()))
                    .and_modify(|first| *first = (*first).min(count))
                    .or_insert(count);
            }
        }
        baseline.clone()
    }

    fn view(&self) -> View {
        let (membership, membership_error) = match self.cluster.membership() {
            Ok(m) => (Some(m), None),
            Err(e) => (None, Some(e)),
        };
        // A cluster without a registry yet has no guests, which is not an
        // error.
        let (resources, registry_error) = if self.registry.root().exists() {
            match self.registry.resources() {
                Ok(r) => (r, None),
                Err(e) => (Vec::new(), Some(e.to_string())),
            }
        } else {
            (Vec::new(), None)
        };
        let slots = if self.registry.root().exists() {
            self.registry.host_slots().unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut observations = std::collections::BTreeMap::new();
        let hosts = membership
            .as_ref()
            .map(|m| m.members.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for host in hosts.into_iter().chain(self.host) {
            for (collector, data) in self.hub.observations_of(host) {
                observations.insert((host, collector), data);
            }
        }
        let smart_baseline = self.smart_baseline(&observations);
        let pool_full_days = self
            .projections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        View {
            smart_baseline,
            pool_full_days,
            now_ms: (self.clock)(),
            served_by: self.host,
            membership,
            membership_error,
            observations,
            peers: self.hub.reachability(),
            resources,
            registry_error,
            slots,
        }
    }

    /// Gives each condition the time it was first seen, unless it went
    /// away for a while.
    fn date(&self, now: i64, conditions: &mut [Condition]) {
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        seen.retain(|_, (_, last)| now - *last < FORGET_CONDITION_MS);
        for condition in conditions {
            let key = (condition.id.clone(), condition.resource.clone());
            let entry = seen.entry(key).or_insert((now, now));
            entry.1 = now;
            condition.since_ms = entry.0;
        }
    }

    pub fn hosts(&self) -> HostsSnapshot {
        let mut snapshot = assemble::hosts(&self.view());
        self.date(snapshot.at_ms, &mut snapshot.conditions);
        snapshot
    }

    pub fn vms(&self) -> VmsSnapshot {
        let mut snapshot = assemble::vms(&self.view());
        self.date(snapshot.at_ms, &mut snapshot.conditions);
        snapshot
    }

    pub fn storage(&self) -> StorageSnapshot {
        let mut snapshot = assemble::storage(&self.view());
        self.date(snapshot.at_ms, &mut snapshot.conditions);
        snapshot
    }

    pub fn cluster(&self) -> ClusterSnapshot {
        let mut snapshot = assemble::cluster(&self.view());
        self.date(snapshot.at_ms, &mut snapshot.conditions);
        snapshot
    }
}

/// Publishes an observation when it changed, or as a heartbeat.
pub(crate) struct Publisher {
    hub: Arc<EventHub>,
    host: HostSlot,
    clock: Clock,
    last: HashMap<&'static str, (Value, i64)>,
}

impl Publisher {
    pub(crate) fn new(hub: Arc<EventHub>, host: HostSlot, clock: Clock) -> Publisher {
        Publisher {
            hub,
            host,
            clock,
            last: HashMap::new(),
        }
    }

    pub(crate) fn publish(&mut self, collector: &'static str, data: impl serde::Serialize) {
        let Ok(data) = serde_json::to_value(data) else {
            return;
        };
        let now = (self.clock)();
        if let Some((previous, at)) = self.last.get(collector)
            && collect::same_apart_from_time(previous, &data)
            && now - at < HEARTBEAT.as_millis() as i64
        {
            return;
        }
        self.last.insert(collector, (data.clone(), now));
        self.hub.publish(StreamBody::Observation {
            host: self.host,
            collector: collector.to_string(),
            data,
        });
    }
}

/// Where the collectors read; tests point them elsewhere.
pub struct CollectorConfig {
    pub host: HostSlot,
    pub version: String,
    pub runner: Arc<dyn CommandRunner>,
    pub proc: PathBuf,
    pub corosync_conf: PathBuf,
    pub registry: Registry,
    pub settings: crate::settings::Settings,
    /// BMAC's part of pmxcfs.
    pub pmxcfs_dir: PathBuf,
    /// Each certificate's name and the files it may be in, first found used.
    pub certificates: Vec<(&'static str, Vec<PathBuf>)>,
    /// Decides whether this host is the duty host, which alone makes the
    /// cluster-wide observations; without one, this host makes them.
    pub cluster: Option<Arc<dyn ClusterView>>,
    pub clock: Clock,
}

impl CollectorConfig {
    pub fn system(
        host: HostSlot,
        version: &str,
        runner: Arc<dyn CommandRunner>,
        clock: Clock,
    ) -> CollectorConfig {
        CollectorConfig {
            host,
            version: version.to_string(),
            runner,
            proc: PathBuf::from("/proc"),
            corosync_conf: PathBuf::from(collect::COROSYNC_CONF),
            registry: Registry::system(),
            settings: crate::settings::Settings::system(),
            pmxcfs_dir: PathBuf::from("/etc/pve/priv/bmac"),
            certificates: vec![
                (
                    "dashboard",
                    vec![PathBuf::from(crate::certificate::TLS_DIR).join("https.pem")],
                ),
                (
                    "proxmox",
                    crate::auth::proxmox::NODE_CERTIFICATES
                        .iter()
                        .map(PathBuf::from)
                        .collect(),
                ),
            ],
            cluster: None,
            clock,
        }
    }

    fn is_duty(&self) -> bool {
        match &self.cluster {
            Some(cluster) => crate::cluster::holds_duty(cluster.as_ref(), self.host).is_ok(),
            None => true,
        }
    }

    /// A setting for this host: its own fact, else the cluster's value.
    fn setting(&self, key: &str) -> Option<String> {
        let own = self
            .settings
            .host(self.host)
            .ok()
            .flatten()
            .and_then(|f| f.values.get(key).cloned());
        own.or_else(|| {
            self.settings
                .cluster()
                .ok()
                .flatten()
                .and_then(|c| c.values.get(key).cloned())
        })
        .filter(|v| !v.is_empty())
    }

    fn qdevice_entry(&self) -> bmac_adapters::corosync::QdeviceEntry {
        let text = std::fs::read_to_string(&self.corosync_conf).unwrap_or_default();
        bmac_adapters::corosync::parse_qdevice_entry(&text)
    }
}

/// Runs this host's collectors until aborted.
pub fn spawn_collectors(
    config: CollectorConfig,
    hub: Arc<EventHub>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut publisher = Publisher {
            hub,
            host: config.host,
            clock: config.clock.clone(),
            last: HashMap::new(),
        };
        let mut host = host::HostCollector::new(&config.proc, "/");
        let start = tokio::time::Instant::now();
        let mut due: HashMap<&'static str, tokio::time::Instant> =
            SCHEDULE.iter().map(|(name, _)| (*name, start)).collect();
        let mut running = Vec::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            for &(name, every) in SCHEDULE {
                let now = tokio::time::Instant::now();
                if due[name] > now {
                    continue;
                }
                due.insert(name, now + every);
                let at = (config.clock)();
                let runner = config.runner.as_ref();
                let c = &config;
                match name {
                    HOST_COLLECTOR => publisher.publish(name, host.observe(at, &c.version)),
                    CLUSTER_COLLECTOR => publisher
                        .publish(name, collect::cluster(runner, &c.corosync_conf, at).await),
                    GUESTS_COLLECTOR => {
                        let seen = collect::guests(runner, c.host, at).await;
                        running = seen
                            .guests
                            .iter()
                            .filter(|g| g.status.as_deref() == Some("running"))
                            .map(|g| g.vmid)
                            .collect();
                        publisher.publish(name, seen);
                    }
                    STORAGE_COLLECTOR => publisher
                        .publish(name, collect::storage(runner, &LocalHost, c.host, at).await),
                    VRRP_COLLECTOR => {
                        let (vip, bridge) = (
                            c.setting("GUEST_EGRESS_VIP"),
                            c.setting("PROXMOX_PRIVATE_BRIDGE"),
                        );
                        publisher.publish(
                            name,
                            health::vrrp(runner, vip.as_deref(), bridge.as_deref(), at).await,
                        );
                    }
                    INGRESS_COLLECTOR => publisher
                        .publish(name, health::ingress(runner, &c.registry, c.host, at).await),
                    AGENTS_COLLECTOR => publisher.publish(
                        name,
                        health::agents(runner, &c.registry, &running, at).await,
                    ),
                    UNITS_COLLECTOR => publisher.publish(
                        name,
                        health::units(runner, c.qdevice_entry().registered, at).await,
                    ),
                    QDEVICE_COLLECTOR => {
                        let entry = c.qdevice_entry();
                        let address = entry.address.filter(|_| entry.registered);
                        let host_name = c.setting("PROXMOX_QDEVICE_HOST");
                        publisher.publish(
                            name,
                            health::qdevice(runner, host_name.as_deref(), address.as_deref(), at)
                                .await,
                        );
                    }
                    TAILSCALE_COLLECTOR => {
                        publisher.publish(name, health::tailscale(runner, at).await)
                    }
                    CLEANUP_COLLECTOR => {
                        publisher.publish(name, health::cleanup(&c.registry, c.host, at))
                    }
                    REGISTRY_COLLECTOR => {
                        if c.is_duty() {
                            publisher
                                .publish(name, health::registry(runner, &c.registry, at).await);
                        }
                    }
                    BACKUPS_COLLECTOR => {
                        if c.is_duty() {
                            publisher.publish(
                                name,
                                crate::backup::status::observe(
                                    &crate::backup::store::BackupStore::system(),
                                    &crate::schedules::Schedules::system(),
                                    at,
                                ),
                            );
                        }
                    }
                    CLOCK_COLLECTOR => publisher.publish(name, health::clock(runner, at).await),
                    PMXCFS_COLLECTOR => publisher.publish(name, health::pmxcfs(&c.pmxcfs_dir, at)),
                    SMART_COLLECTOR => publisher.publish(name, health::smart(runner, at).await),
                    _ => publisher.publish(name, health::certificates(&c.certificates, at)),
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::Membership;
    use bmac_core::GuestName;
    use std::sync::atomic::{AtomicI64, Ordering};

    struct Fixed;

    impl ClusterView for Fixed {
        fn membership(&self) -> Result<Membership, String> {
            let slot = |n| HostSlot::new(n).unwrap();
            Ok(Membership {
                quorate: true,
                online: [slot(1)].into_iter().collect(),
                members: [(slot(1), None), (slot(2), None)].into_iter().collect(),
            })
        }
        fn guest_owner(&self, _guest: GuestName) -> Result<Option<HostSlot>, String> {
            Ok(None)
        }
        fn placement(&self, _guest: GuestName) -> Result<Vec<HostSlot>, String> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn conditions_keep_the_time_they_were_first_seen() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(1_000));
        let clock: Clock = {
            let now = now.clone();
            Arc::new(move || now.load(Ordering::SeqCst))
        };
        let pmxcfs = bmac_pmxcfs::Pmxcfs::new(dir.path(), Arc::new(bmac_pmxcfs::RealFs));
        let registry = Registry::new(pmxcfs, dir.path().join("none"));
        let source = StatusSource::new(
            HostSlot::new(1).ok(),
            EventHub::with_epoch("e"),
            Arc::new(Fixed),
            registry,
            clock,
        );
        let first = source.hosts();
        assert_eq!(first.conditions[0].id, "host.offline");
        assert_eq!(first.conditions[0].since_ms, 1_000);
        now.store(5_000, Ordering::SeqCst);
        assert_eq!(source.cluster().conditions[0].since_ms, 1_000);
        now.store(5_000 + FORGET_CONDITION_MS, Ordering::SeqCst);
        assert_eq!(
            source.hosts().conditions[0].since_ms,
            5_000 + FORGET_CONDITION_MS
        );
        assert!(
            source.vms().production.is_empty(),
            "no registry yet means no guests"
        );
        assert!(source.vms().conditions.is_empty());
    }

    #[test]
    fn unchanged_observations_wait_for_the_heartbeat() {
        let hub = EventHub::with_epoch("e");
        let now = Arc::new(AtomicI64::new(0));
        let clock: Clock = {
            let now = now.clone();
            Arc::new(move || now.load(Ordering::SeqCst))
        };
        let mut publisher = Publisher {
            hub: hub.clone(),
            host: HostSlot::new(1).unwrap(),
            clock,
            last: HashMap::new(),
        };
        let mut tab = hub.subscribe("s", None);
        publisher.publish("host", serde_json::json!({"at_ms": 1, "x": 1}));
        now.store(1_000, Ordering::SeqCst);
        publisher.publish("host", serde_json::json!({"at_ms": 2, "x": 1}));
        publisher.publish("host", serde_json::json!({"at_ms": 3, "x": 2}));
        now.store(1_000 + HEARTBEAT.as_millis() as i64, Ordering::SeqCst);
        publisher.publish("host", serde_json::json!({"at_ms": 4, "x": 2}));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let seen: Vec<i64> = rt.block_on(async {
            let mut seen = Vec::new();
            for _ in 0..3 {
                if let crate::engine::events::Next::Message(m) = tab.next().await
                    && let StreamBody::Observation { data, .. } = &m.body
                {
                    seen.push(data["at_ms"].as_i64().unwrap());
                }
            }
            seen
        });
        assert_eq!(seen, [1, 3, 4]);
    }

    /// Collectors never change anything (v2 `test_only_read_only_commands_run`):
    /// every command they try, given a disk, a VIP, and a running guest, is a
    /// known read-only one.
    #[tokio::test]
    async fn collectors_only_run_read_only_commands() {
        use bmac_adapters::programs as p;
        use bmac_exec::{FakeRunner, Reply};
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeRunner::new();
        for _ in 0..2 {
            fake.expect(
                p::LSBLK,
                ["-J", "-b", "-p", "-o", bmac_adapters::disks::LSBLK_COLUMNS],
                Reply::ok(r#"{"blockdevices":[{"name":"/dev/sda","kname":"/dev/sda","type":"disk","size":1}]}"#),
            );
        }
        let host = HostSlot::new(1).unwrap();
        let pmxcfs = bmac_pmxcfs::Pmxcfs::new(dir.path(), Arc::new(bmac_pmxcfs::RealFs));
        let registry = Registry::new(pmxcfs, dir.path().join("registry"));
        let conf = dir.path().join("corosync.conf");
        collect::cluster(&fake, &conf, 1).await;
        collect::guests(&fake, host, 1).await;
        collect::storage(&fake, &LocalHost, host, 1).await;
        health::units(&fake, true, 1).await;
        health::smart(&fake, 1).await;
        health::tailscale(&fake, 1).await;
        health::clock(&fake, 1).await;
        health::vrrp(&fake, Some("10.0.0.254"), Some("vmbr1"), 1).await;
        health::ingress(&fake, &registry, host, 1).await;
        health::agents(&fake, &registry, &[], 1).await;
        let allowed = [
            format!("^{} status$", p::PVECM),
            format!("^{} -n$", p::COROSYNC_CFGTOOL),
            format!("^{} get /\\S+( --type vm)? --output-format json$", p::PVESH),
            format!("^{} (list|status|get) ", p::ZPOOL),
            format!("^{} (list|get) ", p::ZFS),
            format!("^{} -J ", p::LSBLK),
            format!("^{} -j -HA? /dev/\\S+$", p::SMARTCTL),
            format!(
                "^{} (is-active \\S+|list-units --failed .*|show .*)$",
                p::SYSTEMCTL
            ),
            format!("^{} (status --json|ip -4)$", p::TAILSCALE),
            format!("^{} -c tracking$", p::CHRONYC),
            format!("^{} -4 -o address show dev \\S+$", p::IP),
            format!("^{} guest cmd \\d+ ping$", p::QM),
        ]
        .map(|pattern| regex::Regex::new(&pattern).unwrap());
        let lines = fake.call_lines();
        assert!(lines.iter().any(|l| l.contains("smartctl")), "{lines:?}");
        for line in lines {
            assert!(
                allowed.iter().any(|re| re.is_match(&line)),
                "a collector ran a command not known to be read-only: {line}"
            );
        }
    }
}
