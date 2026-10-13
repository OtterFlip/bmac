// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Proxmox API through `pvesh get ... --output-format json`: nodes,
//! cluster status, guests and their configuration, HA, and replication.
//!
//! Rows keep only the fields BMAC reads and ignore the rest, so a field a
//! later Proxmox release adds does not break parsing. The API writes
//! booleans as `0`/`1`.

use std::collections::BTreeMap;

use bmac_core::Vmid;
use bmac_exec::{Command, CommandRunner};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{AdapterError, programs, run_text};

fn flag<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Bool(value) => value,
        Value::Number(n) => n.as_u64() != Some(0),
        Value::String(s) => !matches!(s.as_str(), "" | "0"),
        _ => false,
    })
}

/// A number the API writes either as a number or as text.
fn loose_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    Ok(match Option::<Value>::deserialize(deserializer)? {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    })
}

/// `GET /nodes`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct NodeRow {
    pub node: String,
    /// `online`, `offline`, or `unknown`.
    pub status: String,
    #[serde(default)]
    pub uptime: Option<u64>,
    #[serde(default)]
    pub cpu: Option<f64>,
    #[serde(default)]
    pub maxcpu: Option<u32>,
    #[serde(default)]
    pub mem: Option<u64>,
    #[serde(default)]
    pub maxmem: Option<u64>,
    #[serde(default)]
    pub ssl_fingerprint: Option<String>,
}

impl NodeRow {
    pub fn online(&self) -> bool {
        self.status.eq_ignore_ascii_case("online")
    }
}

/// `GET /cluster/status`: one cluster entry and one entry per node.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClusterStatusEntry {
    Cluster {
        name: String,
        #[serde(default, deserialize_with = "flag")]
        quorate: bool,
        #[serde(default)]
        nodes: Option<u32>,
        #[serde(default)]
        version: Option<u64>,
    },
    Node {
        name: String,
        #[serde(default, deserialize_with = "loose_u64")]
        nodeid: Option<u64>,
        #[serde(default, deserialize_with = "flag")]
        online: bool,
        #[serde(default, deserialize_with = "flag")]
        local: bool,
        #[serde(default)]
        ip: Option<String>,
    },
    #[serde(other)]
    Other,
}

/// `GET /cluster/resources --type vm`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct GuestResource {
    pub vmid: Vmid,
    /// `qemu` or `lxc`.
    #[serde(rename = "type")]
    pub kind: String,
    pub node: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "flag")]
    pub template: bool,
    #[serde(default)]
    pub hastate: Option<String>,
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default)]
    pub maxdisk: Option<u64>,
    #[serde(default)]
    pub maxmem: Option<u64>,
    #[serde(default)]
    pub uptime: Option<u64>,
}

/// `GET /cluster/ha/status/current`: quorum, master, one `lrm` per node, and
/// one `service` per HA resource.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct HaStatusEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub sid: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub crm_state: Option<String>,
    #[serde(default)]
    pub request_state: Option<String>,
    #[serde(default, deserialize_with = "flag")]
    pub quorate: bool,
}

/// `GET /cluster/ha/resources`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct HaResource {
    /// `vm:100`.
    pub sid: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub max_relocate: Option<u32>,
    #[serde(default)]
    pub max_restart: Option<u32>,
}

/// `GET /cluster/ha/rules` (Proxmox VE 9): node-affinity and
/// resource-affinity rules.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct HaRule {
    pub rule: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub resources: Option<String>,
    #[serde(default)]
    pub nodes: Option<String>,
    #[serde(default)]
    pub affinity: Option<String>,
    #[serde(default, deserialize_with = "flag")]
    pub strict: bool,
    #[serde(default, deserialize_with = "flag")]
    pub disable: bool,
}

/// `GET /nodes/{node}/replication`: the jobs whose source is that node.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ReplicationJob {
    /// `100-0`.
    pub id: String,
    pub guest: Vmid,
    pub target: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub last_sync: Option<u64>,
    #[serde(default)]
    pub last_try: Option<u64>,
    #[serde(default)]
    pub next_sync: Option<u64>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub fail_count: Option<u32>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, deserialize_with = "flag")]
    pub disable: bool,
}

/// A guest's configuration: option name to value, as the API returns it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct GuestConfig(pub BTreeMap<String, Value>);

impl GuestConfig {
    /// An option as text; numbers are written in decimal.
    pub fn get(&self, key: &str) -> Option<String> {
        match self.0.get(key)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(u8::from(*b).to_string()),
            _ => None,
        }
    }

    /// The comma-separated `key=value` parts of an option such as `scsi0`
    /// or `net0`; a part without `=` is keyed by the empty string.
    pub fn parts(&self, key: &str) -> Vec<(String, String)> {
        self.get(key)
            .map(|value| {
                value
                    .split(',')
                    .map(|part| match part.split_once('=') {
                        Some((k, v)) => (k.to_string(), v.to_string()),
                        None => (String::new(), part.to_string()),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Reads the Proxmox API with `pvesh`.
pub struct Pvesh<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> Pvesh<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Pvesh<'a> {
        Pvesh { runner }
    }

    /// `pvesh get <path> [extra...] --output-format json`. An empty or
    /// `null` answer reads as the type's default.
    pub async fn get<T>(&self, path: &str, extra: &[&str]) -> Result<T, AdapterError>
    where
        T: serde::de::DeserializeOwned + Default,
    {
        let command = Command::new(programs::PVESH)
            .args(["get", path])
            .args(extra.iter().copied())
            .args(["--output-format", "json"]);
        let text = run_text(self.runner, command.clone()).await?;
        if text.trim().is_empty() {
            return Ok(T::default());
        }
        match serde_json::from_str::<Value>(&text) {
            Err(e) => Err(AdapterError::parse(&command, format!("invalid JSON: {e}"))),
            Ok(Value::Null) => Ok(T::default()),
            Ok(value) => serde_json::from_value(value)
                .map_err(|e| AdapterError::parse(&command, format!("unexpected JSON: {e}"))),
        }
    }

    pub async fn nodes(&self) -> Result<Vec<NodeRow>, AdapterError> {
        self.get("/nodes", &[]).await
    }

    pub async fn cluster_status(&self) -> Result<Vec<ClusterStatusEntry>, AdapterError> {
        self.get("/cluster/status", &[]).await
    }

    pub async fn guests(&self) -> Result<Vec<GuestResource>, AdapterError> {
        self.get("/cluster/resources", &["--type", "vm"]).await
    }

    pub async fn guest_config(&self, node: &str, vmid: Vmid) -> Result<GuestConfig, AdapterError> {
        let config: Option<GuestConfig> = self
            .get(&format!("/nodes/{node}/qemu/{vmid}/config"), &[])
            .await?;
        Ok(config.unwrap_or(GuestConfig(BTreeMap::new())))
    }

    pub async fn ha_status(&self) -> Result<Vec<HaStatusEntry>, AdapterError> {
        self.get("/cluster/ha/status/current", &[]).await
    }

    pub async fn ha_resources(&self) -> Result<Vec<HaResource>, AdapterError> {
        self.get("/cluster/ha/resources", &[]).await
    }

    pub async fn ha_rules(&self) -> Result<Vec<HaRule>, AdapterError> {
        self.get("/cluster/ha/rules", &[]).await
    }

    pub async fn replication(&self, node: &str) -> Result<Vec<ReplicationJob>, AdapterError> {
        let mut jobs: Vec<ReplicationJob> =
            self.get(&format!("/nodes/{node}/replication"), &[]).await?;
        for job in &mut jobs {
            if job.source.as_deref().is_none_or(str::is_empty) {
                job.source = Some(node.to_string());
            }
        }
        Ok(jobs)
    }
}

/// Online node names, `moxN` first in numeric order, then the rest by name
/// (v2 `quick_state.py`, `online_nodes` and `node_sort_key`).
pub fn online_nodes(nodes: &[NodeRow]) -> Vec<String> {
    let mut names: Vec<String> = nodes
        .iter()
        .filter(|row| row.online())
        .map(|row| row.node.clone())
        .collect();
    names.sort_by_key(|name| node_sort_key(name));
    names
}

pub fn node_sort_key(name: &str) -> (u8, u64, String) {
    match name.strip_prefix("mox").and_then(|n| {
        (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .then(|| n.parse::<u64>().ok())
            .flatten()
    }) {
        Some(number) => (0, number, String::new()),
        None => (1, 0, name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use bmac_exec::{FakeRunner, Reply};

    use super::*;

    const JSON: [&str; 2] = ["--output-format", "json"];

    fn pvesh_args<'a>(path: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
        let mut args = vec!["get", path];
        args.extend_from_slice(extra);
        args.extend_from_slice(&JSON);
        args
    }

    #[tokio::test]
    async fn nodes_and_cluster_status_read_flags_and_ignore_new_fields() {
        let fake = FakeRunner::new();
        fake.expect(
            programs::PVESH,
            pvesh_args("/nodes", &[]),
            Reply::ok(
                r#"[{"node":"mox10","status":"online","uptime":5,"new_field":1},
                    {"node":"mox2","status":"online"},{"node":"mox3","status":"offline"},
                    {"node":"qdevice","status":"online"}]"#,
            ),
        )
        .expect(
            programs::PVESH,
            pvesh_args("/cluster/status", &[]),
            Reply::ok(
                r#"[{"type":"cluster","name":"MyAppCloud","quorate":1,"nodes":3,"version":7,"id":"cluster"},
                    {"type":"node","name":"mox1","nodeid":1,"online":1,"local":1,"ip":"10.0.0.1"},
                    {"type":"node","name":"mox2","nodeid":"2","online":0},
                    {"type":"future"}]"#,
            ),
        );
        let pvesh = Pvesh::new(&fake);
        let nodes = pvesh.nodes().await.unwrap();
        assert_eq!(online_nodes(&nodes), ["mox2", "mox10", "qdevice"]);
        let status = pvesh.cluster_status().await.unwrap();
        assert_eq!(
            status[0],
            ClusterStatusEntry::Cluster {
                name: "MyAppCloud".into(),
                quorate: true,
                nodes: Some(3),
                version: Some(7)
            }
        );
        assert!(matches!(
            &status[2],
            ClusterStatusEntry::Node {
                nodeid: Some(2),
                online: false,
                ..
            }
        ));
        assert_eq!(status[3], ClusterStatusEntry::Other);
        fake.assert_done();
    }

    #[tokio::test]
    async fn replication_jobs_default_their_source_to_the_node() {
        let fake = FakeRunner::new();
        fake.expect(
            programs::PVESH,
            pvesh_args("/nodes/mox1/replication", &[]),
            Reply::ok(
                r#"[{"id":"100-0","guest":100,"target":"mox2","last_sync":1758703600,
                     "next_sync":1758704500,"duration":1.5,"fail_count":0,"type":"local","jobnum":0}]"#,
            ),
        )
        .expect(
            programs::PVESH,
            pvesh_args("/nodes/mox2/replication", &[]),
            Reply::ok(""),
        );
        let pvesh = Pvesh::new(&fake);
        let jobs = pvesh.replication("mox1").await.unwrap();
        assert_eq!(jobs[0].source.as_deref(), Some("mox1"));
        assert_eq!(jobs[0].guest.get(), 100);
        assert!(pvesh.replication("mox2").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn guests_and_config() {
        let fake = FakeRunner::new();
        fake.expect(
            programs::PVESH,
            pvesh_args("/cluster/resources", &["--type", "vm"]),
            Reply::ok(
                r#"[{"id":"qemu/100","vmid":100,"type":"qemu","node":"mox1","name":"prod1",
                     "status":"running","template":0,"hastate":"started"}]"#,
            ),
        )
        .expect(
            programs::PVESH,
            pvesh_args("/nodes/mox1/qemu/100/config", &[]),
            Reply::ok(
                r#"{"cores":4,"scsi0":"local-zfs:vm-100-disk-0,discard=on,size=64G","agent":"1"}"#,
            ),
        );
        let pvesh = Pvesh::new(&fake);
        let guests = pvesh.guests().await.unwrap();
        assert_eq!(guests[0].name.as_deref(), Some("prod1"));
        let config = pvesh.guest_config("mox1", guests[0].vmid).await.unwrap();
        assert_eq!(config.get("cores").as_deref(), Some("4"));
        assert_eq!(
            config.parts("scsi0")[0],
            (String::new(), "local-zfs:vm-100-disk-0".to_string())
        );
        assert_eq!(config.parts("scsi0")[2], ("size".into(), "64G".into()));
    }

    #[tokio::test]
    async fn a_failed_query_names_the_command() {
        let fake = FakeRunner::new();
        fake.expect(
            programs::PVESH,
            pvesh_args("/cluster/ha/status/current", &[]),
            Reply::ok("{not json"),
        );
        let error = Pvesh::new(&fake).ha_status().await.unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("/usr/bin/pvesh get /cluster/ha/status/current --output-format json printed output BMAC could not read: invalid JSON"),
            "{error}"
        );
    }
}
