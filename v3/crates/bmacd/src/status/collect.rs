// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The `cluster`, `guests`, and `storage` collectors. Each reads only this
//! host, with local read-only commands (`MONITORING.md`, "Collectors"); a
//! failed command becomes an error in the observation and the rest carries
//! on, as v2's `quick_state.py` does (123–134).

use std::path::Path;

use bmac_adapters::corosync::{Corosync, node_names, parse_qdevice_entry};
use bmac_adapters::layout::{self, HostFiles, Layout};
use bmac_adapters::proxmox::{HaResource, HaStatusEntry, Pvesh};
use bmac_adapters::zpool::{Zpool, parse_status, split_pools};
use bmac_adapters::{Collection, programs, run_text};
use bmac_core::{HostSlot, PoolName, Vmid};
use bmac_exec::{Command, CommandRunner};
use bmac_proto::status::{
    ClusterObservation, CorosyncPeer, GuestObservation, GuestsObservation, PitSnapshotSpace, Pool,
    PoolFinding, ProxmoxStorage, ReplicationObservation, StorageObservation,
};
use serde::Deserialize;
use serde_json::Value;

use bmac_storage::journal;

use super::disks;

pub const COROSYNC_CONF: &str = "/etc/pve/corosync.conf";
const RPOOL: &str = "rpool";

/// `cluster`: quorum and votes from `pvecm status`, the QDevice from
/// `corosync.conf` (v2 `quick_state.py`, `qdevice_host_from_corosync` and
/// `render_hosts` 383–388).
pub async fn cluster(
    runner: &dyn CommandRunner,
    corosync_conf: &Path,
    at_ms: i64,
) -> ClusterObservation {
    let mut errors = Collection::default();
    let corosync = Corosync::new(runner);
    let status = errors.take(corosync.pvecm_status().await);
    let conf = std::fs::read_to_string(corosync_conf).ok();
    let entry = conf.as_deref().map(parse_qdevice_entry);
    let names = conf.as_deref().map(node_names).unwrap_or_default();
    let peers = errors
        .take(corosync.links().await)
        .unwrap_or_default()
        .into_iter()
        .map(|(node_id, peer)| CorosyncPeer {
            node_id,
            name: names.get(&node_id).and_then(|n| n.parse().ok()),
            reachable: peer.reachable,
            links: peer.links,
        })
        .collect();
    let voting = match (&status, &entry) {
        (Some(status), Some(entry)) if entry.registered => Some(
            status.has_qdevice_flag()
                && status
                    .members
                    .iter()
                    .any(|m| m.nodeid == 0 && m.votes == 1 && m.name == "Qdevice"),
        ),
        _ => None,
    };
    ClusterObservation {
        at_ms,
        name: status
            .as_ref()
            .and_then(|s| s.value("Name"))
            .map(str::to_string),
        quorate: status.as_ref().map(|s| s.quorate()),
        expected_votes: status.as_ref().and_then(|s| s.expected_votes()),
        total_votes: status.as_ref().and_then(|s| s.total_votes()),
        quorum: status
            .as_ref()
            .and_then(|s| s.value("Quorum"))
            .and_then(|v| v.parse().ok()),
        qdevice_registered: entry.as_ref().map(|e| e.registered),
        qdevice_address: entry.and_then(|e| e.address),
        qdevice_voting: voting,
        peers,
        errors: errors.errors,
    }
}

/// `GET /nodes/{node}/qemu`: the guests whose configuration is on the node.
#[derive(Debug, Clone, Deserialize)]
struct NodeGuest {
    vmid: Vmid,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    cpus: Option<f64>,
    #[serde(default)]
    maxmem: Option<u64>,
    #[serde(default)]
    maxdisk: Option<u64>,
    #[serde(default)]
    uptime: Option<u64>,
    #[serde(default)]
    template: Option<Value>,
}

fn seconds_to_ms(seconds: Option<u64>) -> Option<i64> {
    seconds.filter(|s| *s > 0).map(|s| s as i64 * 1000)
}

/// `guests`: the guests on this host, their HA state, and the replication
/// jobs from it (v2 `collect_guests` and `collect_replication_status`).
pub async fn guests(runner: &dyn CommandRunner, host: HostSlot, at_ms: i64) -> GuestsObservation {
    let pvesh = Pvesh::new(runner);
    let mut errors = Collection::default();
    let listed: Vec<NodeGuest> = errors
        .take(pvesh.get(&format!("/nodes/{host}/qemu"), &[]).await)
        .unwrap_or_default();
    let ha_resources: Vec<HaResource> = errors.take(pvesh.ha_resources().await).unwrap_or_default();
    let ha_status: Vec<HaStatusEntry> = errors.take(pvesh.ha_status().await).unwrap_or_default();
    let replication = errors
        .take(pvesh.replication(&host.to_string()).await)
        .unwrap_or_default();
    let mut guests: Vec<GuestObservation> = listed
        .into_iter()
        .filter(|g| {
            !g.template
                .as_ref()
                .is_some_and(|t| t.as_u64() == Some(1) || t == &Value::Bool(true))
        })
        .map(|g| {
            let sid = format!("vm:{}", g.vmid);
            let configured = ha_resources.iter().find(|r| r.sid == sid);
            let reported = ha_status
                .iter()
                .find(|s| s.sid.as_deref() == Some(sid.as_str()));
            GuestObservation {
                vmid: g.vmid,
                name: g.name,
                status: g.status,
                cores: g.cpus.map(|c| c.round() as u32),
                memory_bytes: g.maxmem,
                disk_bytes: g.maxdisk,
                uptime_seconds: g.uptime.filter(|u| *u > 0),
                ha_configured: configured.is_some(),
                ha_requested: configured.and_then(|r| r.state.clone()),
                ha_state: reported.and_then(|s| s.state.clone()),
            }
        })
        .collect();
    guests.sort_by_key(|g| g.vmid);
    GuestsObservation {
        at_ms,
        guests,
        replication: replication
            .into_iter()
            .map(|job| ReplicationObservation {
                id: job.id,
                guest: job.guest,
                target: job.target,
                schedule: job.schedule,
                last_sync_ms: seconds_to_ms(job.last_sync),
                last_try_ms: seconds_to_ms(job.last_try),
                next_sync_ms: seconds_to_ms(job.next_sync),
                duration_seconds: job.duration,
                fail_count: job.fail_count.unwrap_or(0),
                error: job.error.filter(|e| !e.is_empty()),
                disabled: job.disable,
            })
            .collect(),
        errors: errors.errors,
    }
}

/// `zpool list -H -p -o name,size,allocated,free,fragmentation,health`.
pub fn parse_pools(text: &str) -> Vec<Pool> {
    let number = |text: &str| text.trim().trim_end_matches('%').parse::<u64>().ok();
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [name, size, allocated, free, frag, health] = fields.as_slice() else {
                return None;
            };
            let (size, free) = (number(size), number(free));
            Some(Pool {
                name: name.trim().to_string(),
                health: Some(health.trim().to_string()).filter(|h| h != "-"),
                size,
                allocated: number(allocated),
                free,
                fragmentation_percent: number(frag),
                free_fraction: match (size, free) {
                    (Some(size), Some(free)) if size > 0 => Some(free as f64 / size as f64),
                    _ => None,
                },
                scan: None,
                remove: None,
                errors: None,
            })
        })
        .collect()
}

/// `GET /nodes/{node}/storage`.
#[derive(Debug, Clone, Deserialize)]
struct StorageRow {
    storage: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    active: Option<Value>,
    #[serde(default)]
    enabled: Option<Value>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    used: Option<u64>,
    #[serde(default)]
    avail: Option<u64>,
    #[serde(default)]
    content: Option<String>,
}

fn flag(value: &Option<Value>, absent: bool) -> bool {
    match value {
        None | Some(Value::Null) => absent,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_u64() != Some(0),
        Some(Value::String(s)) => !matches!(s.as_str(), "" | "0"),
        Some(_) => absent,
    }
}

/// `storage`: pools, rpool's vdevs and free disks, and the Proxmox
/// storages (v2 `collect_storage` and `list_disks.sh`).
pub async fn storage(
    runner: &dyn CommandRunner,
    files: &dyn HostFiles,
    host: HostSlot,
    at_ms: i64,
) -> StorageObservation {
    let mut errors = Collection::default();
    let listed = run_text(
        runner,
        Command::new(programs::ZPOOL).args([
            "list",
            "-H",
            "-p",
            "-o",
            "name,size,allocated,free,fragmentation,health",
        ]),
    )
    .await;
    let mut pools = errors
        .take(listed)
        .map(|t| parse_pools(&t))
        .unwrap_or_default();
    let rows: Vec<StorageRow> = errors
        .take(
            Pvesh::new(runner)
                .get(&format!("/nodes/{host}/storage"), &[])
                .await,
        )
        .unwrap_or_default();
    let pool: PoolName = RPOOL.parse().expect("rpool is a pool name");
    let layout = errors.take(layout::collect(runner, files, &pool, true).await);
    let findings = match &layout {
        Some(layout) => pool_findings(runner, layout, &mut errors).await,
        None => Vec::new(),
    };
    let luks_mismatches = layout.as_ref().map(luks_mismatches).unwrap_or_default();
    let (vdevs, disks) = match &layout {
        Some(layout) => {
            if let Some(rpool) = pools.iter_mut().find(|p| p.name == layout.pool.name) {
                rpool.scan = layout.pool.scan.clone();
                rpool.remove = layout.pool.remove.clone();
                rpool.errors = layout.pool.errors.clone();
            }
            let journal = match files.read(Path::new(journal::JOURNAL_FILE)) {
                None => journal::State::default(),
                Some(text) => match serde_json::from_str::<journal::State>(&text)
                    .map_err(|e| e.to_string())
                    .and_then(|s| s.validate().map(|()| s).map_err(|e| e.to_string()))
                {
                    Ok(state) => state,
                    Err(e) => {
                        errors
                            .errors
                            .push(format!("{}: {e}", journal::JOURNAL_FILE));
                        journal::State::default()
                    }
                },
            };
            (disks::vdevs(layout), disks::disks(layout, &journal))
        }
        None => (Vec::new(), Vec::new()),
    };
    let mut storages: Vec<ProxmoxStorage> = rows
        .into_iter()
        .map(|row| ProxmoxStorage {
            active: flag(&row.active, false),
            enabled: flag(&row.enabled, true),
            storage: row.storage,
            kind: row.kind,
            total: row.total,
            used: row.used,
            avail: row.avail,
            content: row.content,
        })
        .collect();
    storages.sort_by(|a, b| a.storage.cmp(&b.storage));
    let snapshots = run_text(
        runner,
        Command::new(programs::ZFS).args(["list", "-H", "-p", "-t", "snapshot", "-o", "name,used"]),
    )
    .await;
    let pit_snapshots = errors
        .take(snapshots)
        .map(|t| parse_pit_snapshots(&t))
        .unwrap_or_default();
    StorageObservation {
        at_ms,
        pools,
        storages,
        vdevs,
        disks,
        findings,
        luks_mismatches,
        pit_snapshots,
        errors: errors.errors,
    }
}

/// Sums the `bmac-pit-` snapshots of each dataset from `zfs list -H -p -t
/// snapshot -o name,used`.
fn parse_pit_snapshots(text: &str) -> Vec<PitSnapshotSpace> {
    let mut by_dataset: std::collections::BTreeMap<&str, (u32, u64)> = Default::default();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let (Some(name), Some(used)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Some((dataset, snapshot)) = name.split_once('@') else {
            continue;
        };
        if !snapshot.starts_with(bmac_backup::names::PREFIX) {
            continue;
        }
        let entry = by_dataset.entry(dataset).or_default();
        entry.0 += 1;
        entry.1 += used.trim().parse::<u64>().unwrap_or(0);
    }
    by_dataset
        .into_iter()
        .map(|(dataset, (snapshots, used_bytes))| PitSnapshotSpace {
            dataset: dataset.to_string(),
            snapshots,
            used_bytes,
        })
        .collect()
}

/// Every imported pool's member states, error counts, data errors, and
/// scans, as `show_cluster_health.sh` judges them (692–743).
async fn pool_findings(
    runner: &dyn CommandRunner,
    layout: &Layout,
    errors: &mut Collection,
) -> Vec<PoolFinding> {
    let Some(text) = errors.take(Zpool::new(runner).status_all_text().await) else {
        return Vec::new();
    };
    let members: std::collections::BTreeMap<&str, String> = layout
        .vdevs
        .iter()
        .flat_map(|v| v.members.iter())
        .chain(
            layout
                .auxiliary_vdevs
                .values()
                .flatten()
                .flat_map(|v| v.members.iter()),
        )
        .chain(
            layout
                .imported_pool_vdevs
                .iter()
                .flat_map(|v| v.members.iter()),
        )
        .map(|m| {
            let disk = m.disk.as_deref().unwrap_or("?").trim_start_matches("/dev/");
            let label = match &m.serial {
                Some(serial) => format!("{disk} {serial}"),
                None => disk.to_string(),
            };
            (m.path.as_str(), label)
        })
        .collect();
    let describe = |path: &str| members.get(path).cloned();
    let mut found = Vec::new();
    for block in split_pools(&text) {
        match parse_status(&block) {
            Ok(status) => {
                found.extend(status.findings(&describe, None).into_iter().map(|message| {
                    PoolFinding {
                        pool: status.pool.clone(),
                        message,
                    }
                }))
            }
            Err(e) => errors.errors.push(format!("zpool status: {e}")),
        }
    }
    found
}

/// rpool's LUKS mappings against `/etc/crypttab`: an open mapping or an
/// encrypted member must be in crypttab with the shared unlock, or the
/// host won't unlock it at boot.
pub fn luks_mismatches(layout: &Layout) -> Vec<String> {
    let mut found = Vec::new();
    let listed = |mapper: &str| layout.crypttab.iter().any(|e| e.mapper == mapper);
    for entry in &layout.crypttab {
        if !layout
            .luks_mappings
            .iter()
            .any(|m| m.mapper == entry.mapper)
        {
            found.push(format!("{} is in /etc/crypttab but not open", entry.mapper));
        }
        if !entry.shared_unlock {
            found.push(format!(
                "{} in /etc/crypttab is not unlocked with the shared passphrase",
                entry.mapper
            ));
        }
    }
    let members: Vec<_> = layout.vdevs.iter().flat_map(|v| v.members.iter()).collect();
    for member in &members {
        if let Some(mapper) = &member.mapper {
            let name = mapper.rsplit('/').next().unwrap_or(mapper);
            if !listed(name) {
                found.push(format!(
                    "rpool member {} uses {name}, which is not in /etc/crypttab, so it won't unlock at boot",
                    member.path
                ));
            }
        }
    }
    let encrypted = members.iter().filter(|m| m.luks).count();
    if encrypted > 0 && encrypted < members.len() {
        found.push(format!(
            "rpool mixes encrypted and unencrypted members ({encrypted} of {} encrypted)",
            members.len()
        ));
    }
    found
}

/// The observations that differ only in when they were made are the same.
pub fn same_apart_from_time(a: &Value, b: &Value) -> bool {
    let strip = |v: &Value| {
        let mut v = v.clone();
        if let Some(map) = v.as_object_mut() {
            map.remove("at_ms");
        }
        v
    };
    strip(a) == strip(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_exec::{FakeRunner, Reply};

    #[test]
    fn pit_snapshots_are_summed_per_dataset() {
        let text = "rpool/data/vm-100-disk-0@bmac-pit-20261014T030000Z-a1b2c3\t4096\n\
                    rpool/data/vm-100-disk-0@bmac-pit-20261015T030000Z-d4e5f6\t8192\n\
                    rpool/data/vm-100-disk-0@__replicate_100-0_1700000000__\t999\n\
                    rpool/data/vm-101-disk-0@bmac-pit-20261014T030000Z-a1b2c3\t1\n\
                    rpool/ROOT@manual\t5\n";
        assert_eq!(
            parse_pit_snapshots(text),
            vec![
                PitSnapshotSpace {
                    dataset: "rpool/data/vm-100-disk-0".into(),
                    snapshots: 2,
                    used_bytes: 12288,
                },
                PitSnapshotSpace {
                    dataset: "rpool/data/vm-101-disk-0".into(),
                    snapshots: 1,
                    used_bytes: 1,
                },
            ]
        );
    }

    const PVECM: &str = "Cluster information\n-------------------\nName:             bmaclab\n\
        Config Version:   4\nQuorate:          Yes\n\nVotequorum information\n----------------------\n\
        Expected votes:   3\nHighest expected: 3\nTotal votes:      3\nQuorum:           2\n\
        Flags:            Quorate Qdevice\n\nMembership information\n----------------------\n\
            Nodeid      Votes    Qdevice Name\n0x00000001          1    A,V,NMW 10.0.0.1 (local)\n\
        0x00000002          1    A,V,NMW 10.0.0.2\n0x00000000          1            Qdevice\n";

    #[tokio::test]
    async fn the_cluster_collector_reads_quorum_and_the_qdevice() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("corosync.conf");
        std::fs::write(
            &conf,
            "quorum {\n  device {\n    net {\n      host: 100.64.0.9\n    }\n  }\n}\n",
        )
        .unwrap();
        let fake = FakeRunner::new();
        fake.expect(programs::PVECM, ["status"], Reply::ok(PVECM));
        fake.expect(
            programs::COROSYNC_CFGTOOL,
            ["-n"],
            Reply::ok("Local node ID 1, transport knet\nnodeid: 2 reachable\n   LINK: 0 udp (10.0.0.1->10.0.0.2) enabled connected mtu: 1397\n   LINK: 1 udp (100.64.0.1->100.64.0.2) enabled disconnected mtu: 1397\n"),
        );
        let seen = cluster(&fake, &conf, 7).await;
        assert_eq!(seen.name.as_deref(), Some("bmaclab"));
        assert_eq!(seen.quorate, Some(true));
        assert_eq!(
            (seen.expected_votes, seen.total_votes, seen.quorum),
            (Some(3), Some(3), Some(2))
        );
        assert_eq!(seen.qdevice_registered, Some(true));
        assert_eq!(seen.qdevice_address.as_deref(), Some("100.64.0.9"));
        assert_eq!(seen.qdevice_voting, Some(true));
        assert!(seen.errors.is_empty(), "{:?}", seen.errors);
        assert_eq!(seen.peers.len(), 1);
        assert_eq!(seen.peers[0].links, [(0, true), (1, false)].into());

        let fake = FakeRunner::new();
        fake.expect(programs::PVECM, ["status"], Reply::fail(2, "no cluster\n"));
        fake.expect(
            programs::COROSYNC_CFGTOOL,
            ["-n"],
            Reply::fail(1, "no cluster\n"),
        );
        let lone = cluster(&fake, &dir.path().join("missing.conf"), 8).await;
        assert_eq!(lone.quorate, None);
        assert_eq!(lone.qdevice_registered, None);
        assert_eq!(
            lone.errors.len(),
            2,
            "the failed command is reported, not fatal"
        );
    }

    #[tokio::test]
    async fn the_guests_collector_joins_ha_and_replication() {
        let fake = FakeRunner::new();
        let pvesh = |path: &str| {
            [
                "get".to_string(),
                path.to_string(),
                "--output-format".into(),
                "json".into(),
            ]
        };
        fake.expect(
            programs::PVESH,
            pvesh("/nodes/mox1/qemu"),
            Reply::ok(r#"[{"vmid": 102, "name": "stage1prod1", "status": "stopped", "cpus": 2, "maxmem": 2147483648},
                         {"vmid": 101, "name": "prod1", "status": "running", "cpus": 4, "uptime": 60},
                         {"vmid": 9000, "name": "tpl", "status": "stopped", "template": 1}]"#),
        );
        fake.expect(
            programs::PVESH,
            pvesh("/cluster/ha/resources"),
            Reply::ok(r#"[{"sid": "vm:101", "state": "started"}]"#),
        );
        fake.expect(
            programs::PVESH,
            pvesh("/cluster/ha/status/current"),
            Reply::ok(r#"[{"id": "quorum", "type": "quorum", "quorate": 1}, {"id": "service:vm:101", "type": "service", "sid": "vm:101", "state": "started", "node": "mox1"}]"#),
        );
        fake.expect(
            programs::PVESH,
            pvesh("/nodes/mox1/replication"),
            Reply::ok(r#"[{"id": "101-0", "guest": 101, "target": "mox2", "last_sync": 1700000000, "fail_count": 0}]"#),
        );
        let seen = guests(&fake, HostSlot::new(1).unwrap(), 5).await;
        assert!(seen.errors.is_empty(), "{:?}", seen.errors);
        let ids: Vec<u32> = seen.guests.iter().map(|g| g.vmid.get()).collect();
        assert_eq!(ids, [101, 102], "templates are left out, guests in order");
        assert!(seen.guests[0].ha_configured);
        assert_eq!(seen.guests[0].ha_state.as_deref(), Some("started"));
        assert_eq!(seen.guests[1].memory_bytes, Some(2 << 30));
        assert!(!seen.guests[1].ha_configured);
        assert_eq!(seen.replication[0].last_sync_ms, Some(1_700_000_000_000));
        assert_eq!(seen.replication[0].next_sync_ms, None);
    }

    #[test]
    fn pools_come_from_zpool_list() {
        let pools =
            parse_pools("rpool\t1000\t250\t750\t3%\tONLINE\ntank\t-\t-\t-\t-\t-\nbad line\n");
        assert_eq!(pools.len(), 2);
        assert_eq!(pools[0].free_fraction, Some(0.75));
        assert_eq!(pools[0].fragmentation_percent, Some(3));
        assert_eq!(pools[1].health, None);
        assert_eq!(pools[1].free_fraction, None);
    }

    #[test]
    fn only_the_time_differs() {
        let a = serde_json::json!({"at_ms": 1, "x": 2});
        assert!(same_apart_from_time(
            &a,
            &serde_json::json!({"at_ms": 5, "x": 2})
        ));
        assert!(!same_apart_from_time(
            &a,
            &serde_json::json!({"at_ms": 1, "x": 3})
        ));
    }
}
