// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! What the backup work asks of the cluster's hosts, as one trait: tests
//! answer it from a model of the hosts' datasets, replication, and guest
//! agents; [`RealCluster`] runs `zfs`, `pvesr`, `pvesh`, and `qm` on this
//! host, or on another through `/etc/bmac/ssh_config` as v2's `mox_ssh` does.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use bmac_core::{HostSlot, SnapshotGuid};
use bmac_exec::{BoxFuture, Command, CommandRunner};
use serde_json::Value;

use crate::settings::SSH_CONFIG;

const SSH: &str = "/usr/bin/ssh";
const ZFS: &str = "/usr/sbin/zfs";
const PVESH: &str = "/usr/bin/pvesh";
const PVESM: &str = "/usr/sbin/pvesm";
const PVESR: &str = "/usr/bin/pvesr";
const QM: &str = "/usr/sbin/qm";
const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
const PVEVERSION: &str = "/usr/bin/pveversion";

/// A guest as `/cluster/resources` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveGuest {
    pub node: HostSlot,
    pub running: bool,
}

/// One storage replication job of a guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationJob {
    pub id: String,
    pub target: HostSlot,
}

pub trait BackupCluster: Send + Sync {
    fn this_host(&self) -> HostSlot;
    fn live_guest(&self, vmid: u32) -> BoxFuture<'_, Result<Option<LiveGuest>, String>>;
    /// The guest's configuration file as stored, on its owner (this host).
    fn config_text(&self, vmid: u32) -> BoxFuture<'_, Result<String, String>>;
    /// The dataset of a ZFS-backed Proxmox volume.
    fn dataset(&self, volume_id: &str) -> BoxFuture<'_, Result<String, String>>;
    /// `(volsize, volblocksize)` of a zvol on this host.
    fn volume_props(&self, dataset: &str) -> BoxFuture<'_, Result<(u64, u64), String>>;
    /// Freezes the guest's filesystems; the guest agent's answer.
    fn freeze(&self, vmid: u32) -> BoxFuture<'_, Result<Value, String>>;
    fn thaw(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>>;
    /// Arms a timer that thaws the guest after `after` even if `bmacd` dies;
    /// returns its unit to disarm.
    fn arm_thaw(&self, vmid: u32, after: Duration) -> BoxFuture<'_, Result<String, String>>;
    fn disarm_thaw(&self, unit: &str) -> BoxFuture<'_, Result<(), String>>;
    /// `zfs snapshot` of every name in one command, on this host.
    fn snapshot(&self, names: &[String]) -> BoxFuture<'_, Result<(), String>>;
    /// The GUIDs of those of `names` (snapshots or bookmarks) that exist on
    /// `node`.
    fn guids(
        &self,
        node: HostSlot,
        names: &[String],
    ) -> BoxFuture<'_, Result<BTreeMap<String, SnapshotGuid>, String>>;
    fn bookmark(
        &self,
        node: HostSlot,
        snapshot: &str,
        bookmark: &str,
    ) -> BoxFuture<'_, Result<(), String>>;
    /// Destroys a snapshot or bookmark on `node`.
    fn destroy(&self, node: HostSlot, name: &str) -> BoxFuture<'_, Result<(), String>>;
    /// What a receive resume token continues on this host: the snapshot it
    /// sends (`dataset@name`) and that snapshot's GUID.
    fn resume_target(&self, token: &str) -> BoxFuture<'_, Result<(String, SnapshotGuid), String>>;
    fn replication_jobs(&self, vmid: u32) -> BoxFuture<'_, Result<Vec<ReplicationJob>, String>>;
    /// Asks for a run now; fails while another run holds the guest's lock.
    fn schedule_replication(&self, job: &str) -> BoxFuture<'_, Result<(), String>>;
    /// Proxmox VE's and OpenZFS's versions on this host.
    fn versions(&self) -> BoxFuture<'_, Result<(String, String), String>>;
    /// The cluster's hosts, online or not.
    fn members(&self) -> Result<BTreeSet<HostSlot>, String>;
}

/// The VM's placement hosts that are in the cluster. One the registry
/// still names after a recovery onto fewer hosts can't hold a replica.
pub fn placed_members(
    cluster: &dyn BackupCluster,
    placement: &[HostSlot],
) -> Result<Vec<HostSlot>, String> {
    let members = cluster.members()?;
    Ok(placement
        .iter()
        .copied()
        .filter(|h| members.contains(h))
        .collect())
}

/// `qm guest cmd`'s names for the guest agent's freeze and thaw.
const GUEST_FREEZE: &str = "fsfreeze-freeze";
const GUEST_THAW: &str = "fsfreeze-thaw";

/// `'text'` for a POSIX shell.
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

pub struct RealCluster<'a> {
    pub runner: &'a dyn CommandRunner,
    pub node: HostSlot,
}

impl RealCluster<'_> {
    fn command(&self, node: HostSlot, program: &str, args: &[String]) -> Command {
        if node == self.node {
            return Command::new(program).args(args.iter().cloned());
        }
        let remote: Vec<String> = std::iter::once(program.to_string())
            .chain(args.iter().cloned())
            .map(|a| quote(&a))
            .collect();
        Command::new(SSH)
            .args([
                "-F",
                SSH_CONFIG,
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
            ])
            .arg(node.to_string())
            .arg("--")
            .arg(remote.join(" "))
    }

    async fn run(&self, node: HostSlot, program: &str, args: &[String]) -> Result<String, String> {
        let what = format!("{program} {}", args.first().map_or("", String::as_str));
        let command = self
            .command(node, program, args)
            .timeout(Duration::from_secs(300));
        match self.runner.run(command).await {
            Ok(output) => Ok(output.stdout_text()),
            Err(e) => Err(format!("{what} on {node}: {e}")),
        }
    }

    async fn local(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        self.run(self.node, program, &args).await
    }
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// Rows of `zfs get -Hp -o name,value guid`.
/// `toname` and `toguid` (hex) from `zfs send -nv -t TOKEN`'s listing of
/// the token's contents.
pub fn parse_resume_token(text: &str) -> Option<(String, SnapshotGuid)> {
    let field = |key: &str| {
        text.lines().find_map(|line| {
            let (k, v) = line.trim().split_once('=')?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
    };
    let name = field("toname").filter(|n| n.contains('@'))?;
    let guid = field("toguid")?;
    let guid = u64::from_str_radix(guid.strip_prefix("0x")?, 16).ok()?;
    Some((name, SnapshotGuid::new(guid)))
}

pub fn parse_guids(text: &str) -> BTreeMap<String, SnapshotGuid> {
    text.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once('\t')?;
            Some((name.to_string(), value.trim().parse().ok()?))
        })
        .collect()
}

/// `/cluster/resources --type vm` rows for one VMID.
pub fn parse_live_guest(json: &str, vmid: u32) -> Result<Option<LiveGuest>, String> {
    let rows: Vec<Value> =
        serde_json::from_str(json).map_err(|e| format!("/cluster/resources: {e}"))?;
    for row in rows {
        if row["vmid"].as_u64() == Some(u64::from(vmid)) && row["type"] == "qemu" {
            let node = row["node"]
                .as_str()
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| format!("VM {vmid} is on a node BMAC doesn't know"))?;
            return Ok(Some(LiveGuest {
                node,
                running: row["status"] == "running",
            }));
        }
    }
    Ok(None)
}

pub fn parse_replication_jobs(json: &str, vmid: u32) -> Result<Vec<ReplicationJob>, String> {
    let rows: Vec<Value> =
        serde_json::from_str(json).map_err(|e| format!("/cluster/replication: {e}"))?;
    let mut jobs = Vec::new();
    for row in rows {
        let guest = row["guest"]
            .as_u64()
            .or_else(|| row["guest"].as_str().and_then(|g| g.parse().ok()));
        if guest != Some(u64::from(vmid)) || row["disable"].as_u64() == Some(1) {
            continue;
        }
        let (Some(id), Some(target)) = (
            row["id"].as_str(),
            row["target"].as_str().and_then(|t| t.parse().ok()),
        ) else {
            return Err(format!("a replication job of VM {vmid} is malformed"));
        };
        jobs.push(ReplicationJob {
            id: id.to_string(),
            target,
        });
    }
    Ok(jobs)
}

impl BackupCluster for RealCluster<'_> {
    fn this_host(&self) -> HostSlot {
        self.node
    }

    fn live_guest(&self, vmid: u32) -> BoxFuture<'_, Result<Option<LiveGuest>, String>> {
        Box::pin(async move {
            let json = self
                .local(
                    PVESH,
                    &[
                        "get",
                        "/cluster/resources",
                        "--type",
                        "vm",
                        "--output-format",
                        "json",
                    ],
                )
                .await?;
            parse_live_guest(&json, vmid)
        })
    }

    fn config_text(&self, vmid: u32) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let path = format!("/etc/pve/nodes/{}/qemu-server/{vmid}.conf", self.node);
            std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))
        })
    }

    fn dataset(&self, volume_id: &str) -> BoxFuture<'_, Result<String, String>> {
        let volume_id = volume_id.to_string();
        Box::pin(async move {
            let path = self.local(PVESM, &["path", &volume_id]).await?;
            path.trim()
                .strip_prefix("/dev/zvol/")
                .map(str::to_string)
                .ok_or_else(|| {
                    format!("{volume_id} is not a ZFS volume, so a PIT can't capture it")
                })
        })
    }

    fn volume_props(&self, dataset: &str) -> BoxFuture<'_, Result<(u64, u64), String>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            let text = self
                .local(
                    ZFS,
                    &[
                        "get",
                        "-Hp",
                        "-o",
                        "value",
                        "volsize,volblocksize",
                        &dataset,
                    ],
                )
                .await?;
            let values: Vec<u64> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
            match values[..] {
                [size, block] => Ok((size, block)),
                _ => Err(format!("{dataset}: unexpected zfs get output")),
            }
        })
    }

    fn freeze(&self, vmid: u32) -> BoxFuture<'_, Result<Value, String>> {
        Box::pin(async move {
            let text = self
                .local(QM, &["guest", "cmd", &vmid.to_string(), GUEST_FREEZE])
                .await?;
            Ok(serde_json::from_str(text.trim()).unwrap_or(Value::String(text.trim().into())))
        })
    }

    fn thaw(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.local(QM, &["guest", "cmd", &vmid.to_string(), GUEST_THAW])
                .await
                .map(drop)
        })
    }

    fn arm_thaw(&self, vmid: u32, after: Duration) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let unit = format!("bmac-thaw-{vmid}-{}", uuid::Uuid::new_v4().simple());
            let delay = format!("--on-active={}", after.as_secs().max(1));
            self.local(
                SYSTEMD_RUN,
                &[
                    "--quiet",
                    "--collect",
                    &format!("--unit={unit}"),
                    &delay,
                    "--timer-property=AccuracySec=1s",
                    QM,
                    "guest",
                    "cmd",
                    &vmid.to_string(),
                    GUEST_THAW,
                ],
            )
            .await?;
            Ok(unit)
        })
    }

    fn disarm_thaw(&self, unit: &str) -> BoxFuture<'_, Result<(), String>> {
        let timer = format!("{unit}.timer");
        Box::pin(async move { self.local(SYSTEMCTL, &["stop", &timer]).await.map(drop) })
    }

    fn snapshot(&self, names: &[String]) -> BoxFuture<'_, Result<(), String>> {
        let mut args = vec!["snapshot".to_string()];
        args.extend(names.iter().cloned());
        Box::pin(async move { self.run(self.node, ZFS, &args).await.map(drop) })
    }

    fn guids(
        &self,
        node: HostSlot,
        names: &[String],
    ) -> BoxFuture<'_, Result<BTreeMap<String, SnapshotGuid>, String>> {
        let mut args = owned(&["get", "-Hp", "-o", "name,value", "guid"]);
        args.extend(names.iter().cloned());
        Box::pin(async move {
            // Exit 1 when some don't exist: those are left out.
            let command = self
                .command(node, ZFS, &args)
                .expect_codes(&[0, 1])
                .timeout(Duration::from_secs(120));
            let output = self
                .runner
                .run(command)
                .await
                .map_err(|e| format!("zfs get guid on {node}: {e}"))?;
            Ok(parse_guids(&output.stdout_text()))
        })
    }

    fn bookmark(
        &self,
        node: HostSlot,
        snapshot: &str,
        bookmark: &str,
    ) -> BoxFuture<'_, Result<(), String>> {
        let args = owned(&["bookmark", snapshot, bookmark]);
        Box::pin(async move { self.run(node, ZFS, &args).await.map(drop) })
    }

    fn destroy(&self, node: HostSlot, name: &str) -> BoxFuture<'_, Result<(), String>> {
        let args = owned(&["destroy", name]);
        Box::pin(async move { self.run(node, ZFS, &args).await.map(drop) })
    }

    fn resume_target(&self, token: &str) -> BoxFuture<'_, Result<(String, SnapshotGuid), String>> {
        let args = vec!["send".into(), "-nv".into(), "-t".into(), token.to_string()];
        Box::pin(async move {
            let text = self.run(self.node, ZFS, &args).await?;
            parse_resume_token(&text).ok_or_else(|| "the resume token names no snapshot".into())
        })
    }

    fn replication_jobs(&self, vmid: u32) -> BoxFuture<'_, Result<Vec<ReplicationJob>, String>> {
        Box::pin(async move {
            let json = self
                .local(
                    PVESH,
                    &["get", "/cluster/replication", "--output-format", "json"],
                )
                .await?;
            parse_replication_jobs(&json, vmid)
        })
    }

    fn schedule_replication(&self, job: &str) -> BoxFuture<'_, Result<(), String>> {
        let job = job.to_string();
        Box::pin(async move { self.local(PVESR, &["schedule-now", &job]).await.map(drop) })
    }

    fn versions(&self) -> BoxFuture<'_, Result<(String, String), String>> {
        Box::pin(async move {
            let pve = self.local(PVEVERSION, &[]).await?;
            let zfs = self.local(ZFS, &["version"]).await?;
            Ok((
                pve.trim().to_string(),
                zfs.lines().next().unwrap_or_default().trim().to_string(),
            ))
        })
    }

    fn members(&self) -> Result<BTreeSet<HostSlot>, String> {
        use crate::cluster::ClusterView;
        crate::cluster::PveCluster::system()
            .membership()
            .map(|m| m.members.into_keys().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers_read_proxmox_and_zfs_output() {
        let guids = parse_guids(
            "rpool/data/vm-100-disk-0@bmac-pit-x\t123\nrpool/data/vm-100-disk-0#bmac-pit-x\t123\n",
        );
        assert_eq!(guids.len(), 2);
        assert_eq!(
            guids["rpool/data/vm-100-disk-0#bmac-pit-x"].to_string(),
            "123"
        );
        let resources = r#"[{"vmid":100,"type":"qemu","node":"mox3","status":"running"},{"vmid":9112,"type":"lxc","node":"mox2"}]"#;
        let live = parse_live_guest(resources, 100).unwrap().unwrap();
        assert_eq!((live.node.to_string(), live.running), ("mox3".into(), true));
        assert_eq!(parse_live_guest(resources, 101).unwrap(), None);
        let jobs = r#"[{"id":"100-0","guest":100,"target":"mox3"},{"id":"101-0","guest":"101","target":"mox3"},{"id":"100-1","guest":100,"target":"mox2","disable":1}]"#;
        let found = parse_replication_jobs(jobs, 100).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "100-0");
        assert_eq!(
            parse_replication_jobs(jobs, 101).unwrap()[0]
                .target
                .to_string(),
            "mox3"
        );
        assert_eq!(quote("a'b"), r"'a'\''b'");
    }
}
