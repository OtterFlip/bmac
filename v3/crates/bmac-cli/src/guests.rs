// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `bmac guests` and `bmac guest-host-key`: the registered guests joined with
//! the live cluster view, and a guest's SSH host key read from inside it
//! through the guest agent (v2 `setup_jump_ssh_access.sh` `guest_rows` and
//! `attest_guest_host_key`). The Deployer runs both over SSH; they read
//! pmxcfs and Proxmox directly, so they work with `bmacd` stopped.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::LazyLock;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use bmac_adapters::proxmox::GuestResource;
use bmac_adapters::qga::ExecStatus;
use bmac_core::{HostSlot, Vmid};
use bmac_registry::{Kind, Resource};
use regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};

pub const HOST_KEY_FILE: &str = "/etc/ssh/ssh_host_ed25519_key.pub";

static HOST_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ssh-ed25519 [A-Za-z0-9+/=]+(?: [^\r\n]+)?$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuestRow {
    pub name: String,
    pub kind: Kind,
    /// The registry state.
    pub state: String,
    pub vmid: Vmid,
    pub ip: Ipv4Addr,
    /// The node running the VM, when Proxmox has a VM with this VMID and name.
    pub node: Option<String>,
    /// Proxmox's status for the VM, or `absent`.
    pub status: String,
    pub primary_domain: String,
    pub aliases: Vec<String>,
    pub placement: Vec<HostSlot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuestList {
    pub guests: Vec<GuestRow>,
    /// Each host's `MAX_STAGING_VM_COUNT_ON_THIS_HOST`, for the staging
    /// names the Deployer's hello app covers.
    pub staging_limits: BTreeMap<String, u32>,
}

/// The staging limits from each host's settings; unreadable values are
/// left out.
pub fn staging_limits<'a>(
    hosts: impl IntoIterator<Item = (HostSlot, &'a BTreeMap<String, String>)>,
) -> BTreeMap<String, u32> {
    hosts
        .into_iter()
        .filter_map(|(host, values)| {
            let limit = values
                .get("MAX_STAGING_VM_COUNT_ON_THIS_HOST")?
                .parse()
                .ok()?;
            Some((host.to_string(), limit))
        })
        .collect()
}

/// Production guests first, then staging, each by number. A live VM whose
/// name differs from the registry's is someone else's VMID, so the guest
/// reads as absent.
pub fn guest_rows(resources: &[Resource], live: &[GuestResource]) -> Vec<GuestRow> {
    let mut resources: Vec<&Resource> = resources.iter().collect();
    resources.sort_by_key(|r| r.sort_key());
    resources
        .into_iter()
        .map(|resource| {
            let name = resource.name.to_string();
            let vm = live
                .iter()
                .find(|vm| vm.kind == "qemu" && vm.vmid == resource.vmid)
                .filter(|vm| vm.name.as_deref() == Some(name.as_str()));
            GuestRow {
                kind: resource.kind,
                state: resource.state.to_string(),
                vmid: resource.vmid,
                ip: resource.ip,
                node: vm.map(|vm| vm.node.clone()).filter(|n| !n.is_empty()),
                status: vm
                    .and_then(|vm| vm.status.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "absent".to_string()),
                primary_domain: resource.domains.primary.clone(),
                aliases: resource.domains.aliases.clone(),
                placement: resource.placement.clone(),
                name,
            }
        })
        .collect()
}

/// The table v2 printed.
pub fn table(rows: &[GuestRow]) -> String {
    let mut text = format!(
        "{:<20} {:<10} {:<16} {:<6} {:<15} {:<6} STATUS\n",
        "NAME", "KIND", "REGISTRY-STATE", "VMID", "PRIVATE-IP", "NODE"
    );
    for row in rows {
        let kind = match row.kind {
            Kind::Production => "production",
            Kind::Staging => "staging",
        };
        text.push_str(&format!(
            "{:<20} {:<10} {:<16} {:<6} {:<15} {:<6} {}\n",
            row.name,
            kind,
            row.state,
            row.vmid.to_string(),
            row.ip.to_string(),
            row.node.as_deref().unwrap_or("-"),
            row.status
        ));
    }
    text
}

/// The guest named `name`, which must be running on a BMAC host.
pub fn running_guest<'a>(
    rows: &'a [GuestRow],
    name: &str,
) -> Result<(&'a GuestRow, HostSlot), String> {
    let row = rows
        .iter()
        .find(|r| r.name == name)
        .ok_or_else(|| format!("{name} is not a registered production or staging guest"))?;
    if row.status != "running" {
        return Err(format!(
            "{name} is not running (live status: {}); start it and rerun",
            row.status
        ));
    }
    let node = row.node.as_deref().unwrap_or("-");
    let slot = node
        .parse::<HostSlot>()
        .map_err(|_| format!("{name} is running on an unexpected node: {node}"))?;
    Ok((row, slot))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuestHostKey {
    pub guest: String,
    pub vmid: Vmid,
    pub node: HostSlot,
    /// `ssh-ed25519 <base64>`, without the comment.
    pub key: String,
    /// `SHA256:<base64>`, as `ssh-keygen -l` prints it.
    pub fingerprint: String,
}

/// Checks what `cat` of the host key printed inside the guest, as v2 does
/// (`exited == 1`, `exitcode == 0`, the Ed25519 line format), and also that
/// the key decodes to an Ed25519 public key.
pub fn attest(status: &ExecStatus) -> Result<(String, String), String> {
    let invalid = || "the guest agent returned an invalid Ed25519 host key".to_string();
    if !status.succeeded() {
        let why = status
            .err_data
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(|e| format!(": {e}"))
            .unwrap_or_default();
        return Err(format!("{}{why}", invalid()));
    }
    let line = status.out_data.as_deref().unwrap_or_default().trim();
    if !HOST_KEY_RE.is_match(line) {
        return Err(invalid());
    }
    let encoded = line.split(' ').nth(1).ok_or_else(invalid)?;
    let blob = STANDARD.decode(encoded).map_err(|_| invalid())?;
    if !is_ed25519_blob(&blob) {
        return Err(invalid());
    }
    let fingerprint = format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&blob)));
    Ok((format!("ssh-ed25519 {encoded}"), fingerprint))
}

/// The SSH wire form: a length-prefixed `ssh-ed25519`, then a
/// length-prefixed 32-byte key, and nothing else.
fn is_ed25519_blob(blob: &[u8]) -> bool {
    const NAME: &[u8] = b"ssh-ed25519";
    let mut expected = Vec::with_capacity(51);
    expected.extend_from_slice(&(NAME.len() as u32).to_be_bytes());
    expected.extend_from_slice(NAME);
    expected.extend_from_slice(&32u32.to_be_bytes());
    blob.len() == expected.len() + 32 && blob.starts_with(&expected)
}

#[cfg(test)]
mod tests {
    use bmac_registry::Registry;
    use serde_json::{Value, json};

    use super::*;

    /// v2 test_setup_jump_ssh_access.py's fixture key and its fingerprint
    /// from `ssh-keygen -lf`.
    const HOST_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBhE1+iRU5AP8b2hmfLfAHJQ/BVRAR0n4ddx0DFCnkg4";

    fn live(rows: Value) -> Vec<GuestResource> {
        serde_json::from_value(rows).unwrap()
    }

    /// Guests written by the legacy `cluster_registry.py`: prod1 (VMID
    /// 100), prod2 (101), and stage1prod1 (200).
    fn resources() -> Vec<Resource> {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("registry");
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../legacy/scripts/host_runtime/cluster_registry.py");
        let steps: &[&[&str]] = &[
            &[
                "init",
                "--network",
                "10.213.0.0/24",
                "--guest-gateway",
                "10.213.0.1/24",
                "--mox-ip-start",
                "10.213.0.11",
                "--mox-ip-end",
                "10.213.0.20",
                "--haproxy-ip-start",
                "10.213.0.21",
                "--haproxy-ip-end",
                "10.213.0.30",
                "--production-ip-start",
                "10.213.0.31",
                "--production-ip-end",
                "10.213.0.50",
                "--staging-ip-start",
                "10.213.0.61",
                "--staging-ip-end",
                "10.213.0.200",
                "--max-hosts",
                "3",
            ],
            &[
                "allocate-prod",
                "--vmid",
                "100",
                "--purpose",
                "shop",
                "--primary-domain",
                "shop.example.com",
                "--placement",
                "mox1,mox2,mox3",
            ],
            &[
                "allocate-prod",
                "--vmid",
                "101",
                "--purpose",
                "blog",
                "--primary-domain",
                "blog.example.com",
                "--placement",
                "mox1,mox2,mox3",
            ],
            &[
                "update",
                "prod1",
                "--expected-revision",
                "1",
                "--state",
                "provisioning",
                "--volume-id",
                "local-zfs:vm-100-disk-0",
                "--owner-node",
                "mox3",
                "--ha-nodes",
                "mox1,mox2,mox3",
                "--replication-targets",
                "mox1,mox2",
            ],
            &[
                "update",
                "prod1",
                "--expected-revision",
                "2",
                "--state",
                "stopped",
            ],
            &[
                "allocate-staging",
                "--source",
                "prod1",
                "--vmid",
                "200",
                "--placement",
                "mox2",
                "--placement-limit",
                "2",
            ],
        ];
        for step in steps {
            let output = std::process::Command::new("python3")
                .arg(&script)
                .arg("--state-dir")
                .arg(&state)
                .args(*step)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{step:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Registry::new(
            bmac_pmxcfs::Pmxcfs::new(
                "/nonexistent-pmxcfs",
                std::sync::Arc::new(bmac_pmxcfs::RealFs),
            ),
            &state,
        )
        .resources()
        .unwrap()
    }

    fn exec(body: Value) -> ExecStatus {
        serde_json::from_value(body).unwrap()
    }

    #[test]
    fn lists_production_and_staging_with_live_state() {
        let rows = guest_rows(
            &resources(),
            &live(json!([
                {"type": "qemu", "vmid": 100, "name": "prod1", "node": "mox3", "status": "running"},
                {"type": "qemu", "vmid": 200, "name": "stage1prod1", "node": "mox2", "status": "stopped"},
                {"type": "lxc", "vmid": 101, "name": "prod2", "node": "mox1", "status": "running"},
            ])),
        );
        let summary: Vec<(&str, &str, Option<&str>, &str)> = rows
            .iter()
            .map(|r| {
                (
                    r.name.as_str(),
                    r.state.as_str(),
                    r.node.as_deref(),
                    r.status.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("prod1", "stopped", Some("mox3"), "running"),
                ("prod2", "reserved", None, "absent"),
                ("stage1prod1", "reserved", Some("mox2"), "stopped"),
            ]
        );
        assert_eq!(rows[0].ip, Ipv4Addr::new(10, 213, 0, 31));
        let (row, node) = running_guest(&rows, "prod1").unwrap();
        assert_eq!(
            (row.kind, node.to_string().as_str()),
            (Kind::Production, "mox3")
        );
        let stopped = running_guest(&rows, "stage1prod1").unwrap_err();
        assert!(
            stopped.contains("is not running (live status: stopped)"),
            "{stopped}"
        );
        let text = table(&rows);
        assert!(
            text.contains("stage1prod1") && text.starts_with("NAME"),
            "{text}"
        );
        let primary = rows[0].primary_domain.clone();
        let placement = serde_json::to_value(&rows[0].placement).unwrap();
        let aliases = rows[0].aliases.clone();
        let limits = BTreeMap::from([(
            "MAX_STAGING_VM_COUNT_ON_THIS_HOST".to_string(),
            "4".to_string(),
        )]);
        let json = serde_json::to_value(GuestList {
            guests: rows,
            staging_limits: staging_limits([(HostSlot::new(2).unwrap(), &limits)]),
        })
        .unwrap();
        assert!(!primary.is_empty());
        assert_eq!(
            json["guests"][0],
            json!({"name": "prod1", "kind": "production", "state": "stopped", "vmid": 100,
                   "ip": "10.213.0.31", "node": "mox3", "status": "running",
                   "primary_domain": primary, "aliases": aliases, "placement": placement})
        );
        assert_eq!(json["staging_limits"], json!({"mox2": 4}));
    }

    #[test]
    fn a_reused_vmid_or_odd_node_is_not_the_guest() {
        let resources = resources();
        let rows = guest_rows(
            &resources,
            &live(json!([{"type": "qemu", "vmid": 100, "name": "someone-else",
                          "node": "mox1", "status": "running"}])),
        );
        assert_eq!(rows[0].status, "absent");
        let rows = guest_rows(
            &resources,
            &live(json!([{"type": "qemu", "vmid": 100, "name": "prod1",
                          "node": "pve-other", "status": "running"}])),
        );
        let refused = running_guest(&rows, "prod1").unwrap_err();
        assert!(refused.contains("unexpected node: pve-other"), "{refused}");
        let refused = running_guest(&rows, "prod99").unwrap_err();
        assert!(refused.contains("not a registered production or staging guest"));
    }

    #[test]
    fn the_host_key_is_attested_like_v2() {
        let (key, fingerprint) = attest(&exec(json!({
            "exited": 1, "exitcode": 0, "out-data": format!("{HOST_KEY} root@prod1\n")
        })))
        .unwrap();
        assert_eq!(key, HOST_KEY);
        assert_eq!(
            fingerprint,
            "SHA256:6pyryPWb/Q2XEYAJxJkDp9mclGwSmIvrNn6QEmRg9gY"
        );

        for (body, why) in [
            (
                json!({"exited": 1, "exitcode": 1, "err-data": "cat: No such file"}),
                "failed",
            ),
            (json!({"exited": 0}), "still running"),
            (
                json!({"exited": 1, "exitcode": 0, "out-data": "ssh-rsa AAAAB3NzaC1yc2E= x"}),
                "RSA",
            ),
            (
                json!({"exited": 1, "exitcode": 0, "out-data": "ssh-ed25519 AAAA\nssh-ed25519 BBBB"}),
                "two lines",
            ),
            (
                json!({"exited": 1, "exitcode": 0, "out-data": "ssh-ed25519 AAAAB3NzaC1yc2EAAAADAQAB"}),
                "an RSA blob",
            ),
            (
                json!({"exited": 1, "exitcode": 0, "out-data": "ssh-ed25519 !!!!"}),
                "not base64",
            ),
            (
                json!({"exited": 1, "exitcode": 0, "out-data": format!("{HOST_KEY} x"), "out-truncated": 1}),
                "truncated",
            ),
        ] {
            let error = attest(&exec(body)).unwrap_err();
            assert!(error.contains("invalid Ed25519 host key"), "{why}: {error}");
        }
    }
}
