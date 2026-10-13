// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Records written by the legacy `cluster_registry.py` (copied from v2),
//! read back by the Rust reader: the typed records must match what it
//! reports, and every invalid form must be refused with its message.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use bmac_core::GuestName;
use bmac_pmxcfs::{Pmxcfs, RealFs};
use bmac_registry::{InstallPhase, Kind, Registry, ResourceState};
use serde_json::{Value, json};

const H1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const H2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const H3: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const NONCE: &str = "11111111111111111111111111111111";
const ROOT_VOLUME: &str = "local-zfs:vm-1001-disk-0";

fn legacy_registry() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../legacy/scripts/host_runtime/cluster_registry.py")
}

fn v2(state: &Path, args: &[&str]) -> Value {
    let output = Command::new("python3")
        .arg(legacy_registry())
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .output()
        .expect("python3 runs");
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("v2 prints JSON")
}

/// A registry holding every record type, built only through v2's CLI.
struct Fixture {
    _temporary: tempfile::TempDir,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("registry");
        let guids: Vec<String> = (1..=3).map(|n| format!("mox{n}=123")).collect();
        let volume_guids: Vec<String> = (1..=3)
            .map(|n| format!("{ROOT_VOLUME},mox{n}=123"))
            .collect();
        let mut snapshot = vec![
            "record-staging-snapshot",
            "stage1prod1",
            "--snapshot-name",
            "s1",
            "--snapshot-owner-node",
            "mox1",
            "--source-volume-id",
            ROOT_VOLUME,
        ];
        for guid in &guids {
            snapshot.extend(["--snapshot-guid", guid]);
        }
        for guid in &volume_guids {
            snapshot.extend(["--snapshot-volume-guid", guid]);
        }
        let all_nodes = [
            "--node", "mox1", "--node", "mox2", "--node", "mox3", "--node", "mox4",
        ];
        let compatible = format!("mox2={H1}");
        let steps: Vec<Vec<&str>> = vec![
            vec![
                "init",
                "--network",
                "10.20.30.0/24",
                "--guest-gateway",
                "10.20.30.1/24",
                "--mox-ip-start",
                "10.20.30.11",
                "--mox-ip-end",
                "10.20.30.20",
                "--haproxy-ip-start",
                "10.20.30.21",
                "--haproxy-ip-end",
                "10.20.30.30",
                "--production-ip-start",
                "10.20.30.101",
                "--production-ip-end",
                "10.20.30.150",
                "--staging-ip-start",
                "10.20.30.151",
                "--staging-ip-end",
                "10.20.30.250",
                "--max-hosts",
                "4",
            ],
            vec![
                "allocate-prod",
                "--vmid",
                "1001",
                "--purpose",
                "shop",
                "--primary-domain",
                "shop.example.com",
                "--alias",
                "www.shop.example.com",
                "--placement",
                "mox1,mox2,mox3",
            ],
            vec![
                "allocate-prod",
                "--vmid",
                "1002",
                "--purpose",
                "blog",
                "--primary-domain",
                "blog.example.com",
                "--placement",
                "mox1,mox2,mox3",
            ],
            vec![
                "allocate-prod",
                "--vmid",
                "1003",
                "--purpose",
                "temp",
                "--primary-domain",
                "temp.example.com",
                "--placement",
                "mox1",
            ],
            vec![
                "update",
                "prod1",
                "--expected-revision",
                "1",
                "--state",
                "provisioning",
                "--volume-id",
                ROOT_VOLUME,
                "--owner-node",
                "mox1",
                "--ha-nodes",
                "mox1,mox2,mox3",
                "--replication-targets",
                "mox2,mox3",
            ],
            vec![
                "update",
                "prod1",
                "--expected-revision",
                "2",
                "--state",
                "stopped",
            ],
            vec![
                "update",
                "prod1",
                "--expected-revision",
                "3",
                "--routes-enabled",
            ],
            vec![
                "allocate-staging",
                "--source",
                "prod1",
                "--vmid",
                "2001",
                "--placement",
                "mox2",
                "--placement-limit",
                "2",
            ],
            vec!["update", "stage1prod1", "--state", "snapshotting"],
            vec![
                "record-staging-snapshot-intent",
                "stage1prod1",
                "--snapshot-name",
                "s1",
                "--snapshot-owner-node",
                "mox1",
                "--source-volume-id",
                ROOT_VOLUME,
                "--snapshotted-volume-id",
                ROOT_VOLUME,
            ],
            snapshot,
            vec![
                "allocate-staging",
                "--source",
                "prod1",
                "--vmid",
                "2002",
                "--placement",
                "mox3",
                "--placement-limit",
                "2",
            ],
            vec![
                "share-staging-snapshot",
                "stage2prod1",
                "--from",
                "stage1prod1",
            ],
            vec![
                "orchestration-acquire",
                "prod2",
                "--nonce",
                NONCE,
                "--owner",
                "job-1",
                "--source-iso-sha256",
                H1,
                "--install-mode",
                "manual",
                "--initial-install-phase",
                "unstarted",
            ],
            vec![
                "orchestration-set-phase",
                "prod2",
                "--nonce",
                NONCE,
                "--phase",
                "media-attached",
            ],
            vec![
                "defer-cleanup",
                "--resource",
                "prod1",
                "--node",
                "mox3",
                "--action",
                "destroy-vm",
                "--target",
                "vm:1001",
                "--reason",
                "node offline",
            ],
            vec!["release", "prod3"],
            vec!["host-sync", "--member", "mox1,mox2,mox3"],
            vec!["host-reserve", "mox4"],
            vec!["host-release", "mox4", "--expected-state", "joining"],
            vec!["control-set", "mox1"],
            [
                &[
                    "ingress-begin",
                    "--generation",
                    H1,
                    "--bundle-sha256",
                    H2,
                    "--route-count",
                    "3",
                ][..],
                &all_nodes,
            ]
            .concat(),
            vec![
                "ingress-mark",
                "--generation",
                H1,
                "--node",
                "mox1",
                "--state",
                "applied",
            ],
            vec![
                "ingress-mark",
                "--generation",
                H1,
                "--node",
                "mox2",
                "--state",
                "applied",
            ],
            [
                &[
                    "ingress-begin",
                    "--generation",
                    H3,
                    "--bundle-sha256",
                    H2,
                    "--route-count",
                    "3",
                ][..],
                &all_nodes,
                &["--compatible", &compatible],
            ]
            .concat(),
            vec![
                "ingress-mark",
                "--generation",
                H3,
                "--node",
                "mox1",
                "--state",
                "staged",
            ],
        ];
        for step in steps {
            v2(&state, &step);
        }
        Fixture {
            _temporary: temporary,
            state,
        }
    }

    fn registry(&self) -> Registry {
        Registry::new(
            Pmxcfs::new("/nonexistent-pmxcfs", Arc::new(RealFs)),
            &self.state,
        )
    }

    fn v2(&self, args: &[&str]) -> Value {
        v2(&self.state, args)
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.state.join(relative)
    }

    fn file(&self, relative: &str) -> Value {
        serde_json::from_slice(&std::fs::read(self.path(relative)).unwrap()).unwrap()
    }
}

fn prod(number: u32) -> GuestName {
    GuestName::Production { number }
}

#[test]
fn typed_records_match_what_v2_reports() {
    let fixture = Fixture::new();
    let registry = fixture.registry();
    let as_json = |value: &dyn erased::Serialize| value.to_json();

    let policy = registry.policy().unwrap();
    assert_eq!(as_json(&policy), fixture.file("policy.json"));
    assert_eq!(policy.limits.max_hosts, 4);
    assert_eq!(policy.defaults.production.replication_interval, 15);

    let resources = registry.resources().unwrap();
    assert_eq!(as_json(&resources), fixture.v2(&["list"]));
    let names: Vec<String> = resources.iter().map(|r| r.name.to_string()).collect();
    assert_eq!(names, ["prod1", "prod2", "stage1prod1", "stage2prod1"]);
    assert_eq!(resources[0].state, ResourceState::Stopped);
    assert!(resources[0].routes_enabled);
    assert_eq!(resources[2].kind, Kind::Staging);
    assert_eq!(
        resources[3].proxmox.snapshot.as_ref().unwrap().verified,
        Some(true)
    );
    assert_eq!(
        as_json(&registry.resource(prod(2)).unwrap()),
        fixture.v2(&["get", "prod2"])
    );

    assert_eq!(
        as_json(&registry.cleanup_records().unwrap()),
        fixture.v2(&["list", "--record-type", "cleanup"])
    );
    let history = registry.history().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(
        as_json(&history),
        fixture.v2(&["list", "--record-type", "history"])
    );

    let orchestration = registry.orchestration(prod(2)).unwrap().unwrap();
    assert_eq!(orchestration.install_phase, InstallPhase::MediaAttached);
    assert_eq!(orchestration.startup_sha256, Some(None));
    assert_eq!(
        as_json(&orchestration),
        fixture.v2(&["orchestration-get", "prod2"])
    );
    assert_eq!(registry.orchestration(prod(1)).unwrap(), None);
    let staging = registry
        .orchestration(GuestName::Staging {
            number: 1,
            source: 1,
        })
        .unwrap_err();
    assert_eq!(
        staging.to_string(),
        "production orchestration is only valid for production"
    );
    assert_eq!(
        registry.orchestration(prod(3)).unwrap_err().to_string(),
        "resource does not exist: prod3"
    );
    assert!(registry.find_resource(prod(3)).unwrap().is_none());
    assert!(registry.find_resource(prod(1)).unwrap().is_some());

    assert_eq!(
        as_json(&registry.host_slots().unwrap()),
        fixture.v2(&["host-list"])
    );
    assert_eq!(
        as_json(&registry.cluster_control().unwrap()),
        fixture.v2(&["control-get"])
    );

    let status = fixture.v2(&["ingress-status"]);
    assert_eq!(
        as_json(&registry.ingress_desired().unwrap()),
        status["desired"]
    );
    assert_eq!(
        as_json(&registry.ingress_node_statuses().unwrap()),
        status["nodes"]
    );
    assert_eq!(
        as_json(&registry.ingress_transition().unwrap()),
        fixture.file("ingress/transition.json")
    );
    assert_eq!(registry.ingress_generations().unwrap().len(), 2);
}

#[test]
fn a_wide_network_for_32_hosts_is_read_back() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("registry");
    v2(
        &state,
        &[
            "init",
            "--network",
            "10.40.0.0/22",
            "--guest-gateway",
            "10.40.0.10/22",
            "--mox-ip-start",
            "10.40.0.11",
            "--mox-ip-end",
            "10.40.0.60",
            "--haproxy-ip-start",
            "10.40.0.71",
            "--haproxy-ip-end",
            "10.40.0.120",
            "--production-ip-start",
            "10.40.0.255",
            "--production-ip-end",
            "10.40.1.254",
            "--staging-ip-start",
            "10.40.2.0",
            "--staging-ip-end",
            "10.40.3.150",
            "--max-hosts",
            "32",
        ],
    );
    for (vmid, purpose) in [("1001", "shop"), ("1002", "blog")] {
        let domain = format!("{purpose}.example.com");
        v2(
            &state,
            &[
                "allocate-prod",
                "--vmid",
                vmid,
                "--purpose",
                purpose,
                "--primary-domain",
                &domain,
                "--placement",
                "mox30,mox31,mox32",
            ],
        );
    }
    let registry = Registry::new(Pmxcfs::new("/nonexistent-pmxcfs", Arc::new(RealFs)), &state);
    let policy = registry.policy().unwrap();
    assert_eq!(policy.limits.max_hosts, 32);
    assert_eq!(policy.limits.max_production, 256);
    assert_eq!(policy.network.production_range.start, 255);
    assert_eq!(
        (
            policy.network.haproxy_vmid_range.start,
            policy.network.haproxy_vmid_range.end
        ),
        (9111, 9142)
    );
    let ips: Vec<String> = registry
        .resources()
        .unwrap()
        .iter()
        .map(|r| r.ip.to_string())
        .collect();
    assert_eq!(ips, ["10.40.0.255", "10.40.1.0"]);
}

#[test]
fn an_empty_registry_has_no_optional_records() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("registry");
    v2(
        &state,
        &[
            "init",
            "--network",
            "10.0.0.0/24",
            "--guest-gateway",
            "10.0.0.1/24",
            "--mox-ip-start",
            "10.0.0.11",
            "--mox-ip-end",
            "10.0.0.20",
            "--haproxy-ip-start",
            "10.0.0.21",
            "--haproxy-ip-end",
            "10.0.0.30",
            "--production-ip-start",
            "10.0.0.101",
            "--production-ip-end",
            "10.0.0.150",
            "--staging-ip-start",
            "10.0.0.151",
            "--staging-ip-end",
            "10.0.0.250",
        ],
    );
    let registry = Registry::new(Pmxcfs::new("/nonexistent-pmxcfs", Arc::new(RealFs)), &state);
    assert!(registry.resources().unwrap().is_empty());
    assert!(registry.cleanup_records().unwrap().is_empty());
    assert!(registry.history().unwrap().is_empty());
    assert!(registry.host_slots().unwrap().is_empty());
    assert_eq!(registry.cluster_control().unwrap(), None);
    assert_eq!(registry.ingress_desired().unwrap(), None);
    assert_eq!(registry.ingress_transition().unwrap(), None);
    let missing = Registry::new(
        Pmxcfs::new("/nonexistent-pmxcfs", Arc::new(RealFs)),
        temporary.path().join("none"),
    );
    assert!(
        missing
            .policy()
            .unwrap_err()
            .to_string()
            .contains("does not exist")
    );
}

type Mutation = fn(&mut Value);
type Reader = fn(&Registry) -> Result<(), bmac_registry::RegistryError>;

fn resources(r: &Registry) -> Result<(), bmac_registry::RegistryError> {
    r.resources().map(drop)
}
fn policy(r: &Registry) -> Result<(), bmac_registry::RegistryError> {
    r.policy().map(drop)
}
fn orchestration(r: &Registry) -> Result<(), bmac_registry::RegistryError> {
    r.orchestration(prod(2)).map(drop)
}
fn cleanup(r: &Registry) -> Result<(), bmac_registry::RegistryError> {
    r.cleanup_records().map(drop)
}
fn hosts(r: &Registry) -> Result<(), bmac_registry::RegistryError> {
    r.host_slots().map(drop)
}
fn control(r: &Registry) -> Result<(), bmac_registry::RegistryError> {
    r.cluster_control().map(drop)
}
fn desired(r: &Registry) -> Result<(), bmac_registry::RegistryError> {
    r.ingress_desired().map(drop)
}
fn nodes(r: &Registry) -> Result<(), bmac_registry::RegistryError> {
    r.ingress_node_statuses().map(drop)
}
fn transition(r: &Registry) -> Result<(), bmac_registry::RegistryError> {
    r.ingress_transition().map(drop)
}
fn history(r: &Registry) -> Result<(), bmac_registry::RegistryError> {
    r.history().map(drop)
}

fn set_guids(snapshot: &mut Value, guid: &str) {
    for n in 1..=3 {
        snapshot["guids"][format!("mox{n}")] = json!(guid);
        snapshot["volume_guids"][ROOT_VOLUME][format!("mox{n}")] = json!(guid);
    }
}

#[test]
fn every_invalid_form_is_refused_with_v2s_message() {
    let fixture = Fixture::new();
    let registry = fixture.registry();
    let generation = format!("ingress/generations/{H3}.json");
    let cleanup_file = std::fs::read_dir(fixture.path("deferred-cleanup"))
        .unwrap()
        .map(|e| {
            format!(
                "deferred-cleanup/{}",
                e.unwrap().file_name().to_string_lossy()
            )
        })
        .next()
        .unwrap();
    let history_file = std::fs::read_dir(fixture.path("history"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .find(|name| name.contains("prod3"))
        .map(|name| format!("history/{name}"))
        .unwrap();
    let cases: Vec<(&str, Mutation, Reader, &str)> = vec![
        (
            "policy.json",
            |v| v["extra"] = json!(1),
            policy,
            "registry policy has unknown fields: extra",
        ),
        (
            "policy.json",
            |v| drop(v.as_object_mut().unwrap().remove("tags")),
            policy,
            "registry policy is missing fields: tags",
        ),
        (
            "policy.json",
            |v| v["schema_version"] = json!(2),
            policy,
            "unsupported registry policy schema",
        ),
        (
            "policy.json",
            |v| v["registry_id"] = json!("x"),
            policy,
            "policy registry_id is not a UUID",
        ),
        (
            "policy.json",
            |v| v["network"]["cidr"] = json!("10.20.30.0/23"),
            policy,
            "must share an IPv4 network from /16 to /24",
        ),
        (
            "policy.json",
            |v| v["network"]["production_range"]["end"] = json!(160),
            policy,
            "policy production_range and staging_range overlap",
        ),
        (
            "policy.json",
            |v| v["network"]["haproxy_vmid_range"]["end"] = json!(9130),
            policy,
            "haproxy_vmid_range must be 9111-9142",
        ),
        (
            "policy.json",
            |v| v["network"]["guest_gateway"] = json!("10.20.30.105/24"),
            policy,
            "guest gateway is invalid or overlaps",
        ),
        (
            "policy.json",
            |v| v["limits"]["max_hosts"] = json!(33),
            policy,
            "policy max_hosts must be between 1 and 32",
        ),
        (
            "policy.json",
            |v| v["limits"]["max_production"] = json!(3),
            policy,
            "policy allocation limits do not match IP ranges",
        ),
        (
            "policy.json",
            |v| v["limits"]["reservation_ttl_seconds"] = json!(59),
            policy,
            "policy reservation TTL is invalid",
        ),
        (
            "policy.json",
            |v| v["tags"]["staging"] = v["tags"]["production"].clone(),
            policy,
            "policy tags must be distinct",
        ),
        (
            "policy.json",
            |v| v["tags"]["staging"] = json!("bad tag"),
            policy,
            "staging tag contains unsafe characters",
        ),
        (
            "policy.json",
            |v| v["defaults"]["production"]["disk_allocation"] = json!("thick"),
            policy,
            "disk allocation must be sparse or reserved",
        ),
        (
            "policy.json",
            |v| v["defaults"]["staging"]["replication_interval"] = json!("0"),
            policy,
            "replication interval must be a positive minute count",
        ),
        (
            "policy.json",
            |v| v["defaults"]["production"]["storage"] = json!("Local ZFS"),
            policy,
            "resource spec storage must be a Proxmox storage id",
        ),
        (
            "policy.json",
            |v| v["state_transitions"]["production"]["active"] = json!(["ready"]),
            policy,
            "policy state transitions differ from this schema version",
        ),
        (
            "policy.json",
            |v| v["created_at"] = json!("2026-10-11T11:00:00"),
            policy,
            "registry timestamp lacks timezone",
        ),
        (
            "resources/prod1.json",
            |v| v["extra"] = json!(1),
            resources,
            "resource record has unknown fields: extra",
        ),
        (
            "resources/prod1.json",
            |v| v["id"] = json!("not-a-uuid"),
            resources,
            "resource id is not a UUID",
        ),
        (
            "resources/prod1.json",
            |v| v["allocation_id"] = json!("bad id"),
            resources,
            "allocation id contains unsafe characters",
        ),
        (
            "resources/prod1.json",
            |v| v["vmid"] = json!(9115),
            resources,
            "VMID 9115 is reserved for fixed HAProxy LXCs (9111-9142)",
        ),
        (
            "resources/prod1.json",
            |v| v["vmid"] = json!(99),
            resources,
            "VMID must be between 100 and 999999999",
        ),
        (
            "resources/prod1.json",
            |v| v["ip"] = json!("10.20.31.101"),
            resources,
            "resource IP is outside policy network",
        ),
        (
            "resources/prod1.json",
            |v| v["ip"] = json!("10.20.30.140"),
            resources,
            "production resource name/IP formula is invalid",
        ),
        (
            "resources/prod1.json",
            |v| v["mac"] = json!(v["mac"].as_str().unwrap().to_uppercase()),
            resources,
            "resource MAC is invalid or not normalized",
        ),
        (
            "resources/prod1.json",
            |v| v["state"] = json!("running"),
            resources,
            "unknown resource state: 'running'",
        ),
        (
            "resources/prod1.json",
            |v| v["revision"] = json!(0),
            resources,
            "resource revision must be positive",
        ),
        (
            "resources/prod1.json",
            |v| v["index"] = json!(2),
            resources,
            "production resource index does not match its name",
        ),
        (
            "resources/prod1.json",
            |v| v["source"] = json!("prod2"),
            resources,
            "production resource has staging identity",
        ),
        (
            "resources/prod1.json",
            |v| v["purpose"]["slug"] = json!("Bad Slug"),
            resources,
            "purpose must be a lowercase slug",
        ),
        (
            "resources/prod1.json",
            |v| v["purpose"]["slug"] = json!("blog"),
            resources,
            "duplicate production purpose 'blog': prod1 and prod2",
        ),
        (
            "resources/prod1.json",
            |v| v["domains"]["primary"] = json!("Shop.example.com"),
            resources,
            "resource primary domain is not normalized",
        ),
        (
            "resources/prod1.json",
            |v| v["domains"]["primary"] = json!("*.example.com"),
            resources,
            "domain must be an exact hostname, not a wildcard",
        ),
        (
            "resources/prod1.json",
            |v| v["domains"]["aliases"] = json!(["shop.example.com"]),
            resources,
            "resource aliases are not normalized and unique",
        ),
        (
            "resources/prod1.json",
            |v| v["domains"]["aliases"] = json!(["blog.example.com"]),
            resources,
            "duplicate domain 'blog.example.com': prod1 and prod2",
        ),
        (
            "resources/prod1.json",
            |v| v["domains"]["staging_base"] = json!("shop.example.com"),
            resources,
            "resource staging base must differ from routed domains",
        ),
        (
            "resources/prod1.json",
            |v| v["placement"] = json!(["mox1", "mox5"]),
            resources,
            "mox5 exceeds policy max_hosts=4",
        ),
        (
            "resources/prod1.json",
            |v| v["placement"] = json!(["mox1", "mox1"]),
            resources,
            "duplicate placement node: mox1",
        ),
        (
            "resources/prod1.json",
            |v| v["placement"] = json!(["moxa"]),
            resources,
            "node must be named mox1 through mox32: 'moxa'",
        ),
        (
            "resources/prod1.json",
            |v| v["initial_node"] = json!("mox4"),
            resources,
            "resource initial node must belong to placement",
        ),
        (
            "resources/prod1.json",
            |v| v["owner_node"] = json!("mox4"),
            resources,
            "resource owner node must belong to placement",
        ),
        (
            "resources/prod2.json",
            |v| v["routes_enabled"] = json!(true),
            resources,
            "routes may only be enabled for stopped, ready, or active guests",
        ),
        (
            "resources/prod1.json",
            |v| v["spec"]["disk_bytes"] = json!(5),
            resources,
            "disk_bytes must be a whole-MiB byte count larger than disk_gib",
        ),
        (
            "resources/prod1.json",
            |v| v["spec"]["cores"] = json!(0),
            resources,
            "resource spec cores must be a positive integer",
        ),
        (
            "resources/prod1.json",
            |v| v["proxmox"]["ha_nodes"] = json!(["mox4"]),
            resources,
            "Proxmox HA/replication metadata exceeds placement",
        ),
        (
            "resources/prod1.json",
            |v| v["proxmox"]["volume_id"] = json!("bad volume"),
            resources,
            "volume id contains unsafe characters",
        ),
        (
            "resources/prod1.json",
            |v| v["proxmox"]["volume_id"] = json!("local-zfs:vm-1001-disk-1"),
            resources,
            "staging snapshot dependency source volume differs from production",
        ),
        (
            "resources/stage1prod1.json",
            |v| v["source"] = json!("prod2"),
            resources,
            "staging snapshot source must match the resource source",
        ),
        (
            "resources/stage1prod1.json",
            |v| v["ip"] = json!("10.20.30.140"),
            resources,
            "staging IP is outside its configured range",
        ),
        (
            "resources/stage1prod1.json",
            |v| v["placement"] = json!(["mox2", "mox3"]),
            resources,
            "staging placement must contain exactly one node",
        ),
        (
            "resources/stage1prod1.json",
            |v| v["proxmox"]["snapshot"]["refcount"] = json!(2),
            resources,
            "a unique staging snapshot refcount must be exactly one",
        ),
        (
            "resources/stage1prod1.json",
            |v| v["proxmox"]["snapshot"]["dependent_resources"] = json!(["stage2prod1"]),
            resources,
            "must name its one dependent resource",
        ),
        (
            "resources/stage1prod1.json",
            |v| v["proxmox"]["snapshot"]["verified"] = json!(false),
            resources,
            "unverified staging snapshot intent may not claim GUIDs",
        ),
        (
            "resources/stage1prod1.json",
            |v| v["proxmox"]["snapshot"]["name"] = json!("__replicate_1"),
            resources,
            "reserved __replicate_ prefix",
        ),
        (
            "resources/stage1prod1.json",
            |v| v["proxmox"]["snapshot"]["guids"]["mox1"] = json!("456"),
            resources,
            "root snapshot GUIDs must match the root volume GUID map",
        ),
        (
            "resources/stage1prod1.json",
            |v| v["proxmox"]["snapshot"]["volume_guids"][ROOT_VOLUME]["mox1"] = json!("456"),
            resources,
            "snapshot GUID differs between nodes",
        ),
        (
            "resources/stage1prod1.json",
            |v| v["proxmox"]["snapshot"]["source_volume_ids"] = json!([]),
            resources,
            "source volumes must be a non-empty unique array",
        ),
        (
            "resources/stage1prod1.json",
            |v| v["proxmox"]["snapshot"]["extra"] = json!(1),
            resources,
            "snapshot metadata has unknown fields: extra",
        ),
        (
            "resources/stage1prod1.json",
            |v| {
                let snapshot = &mut v["proxmox"]["snapshot"];
                snapshot["guids"].as_object_mut().unwrap().remove("mox3");
                snapshot["volume_guids"][ROOT_VOLUME]
                    .as_object_mut()
                    .unwrap()
                    .remove("mox3");
            },
            resources,
            "staging snapshot GUIDs must cover every production placement node",
        ),
        (
            "resources/stage2prod1.json",
            |v| set_guids(&mut v["proxmox"]["snapshot"], "456"),
            resources,
            "is shared by stage1prod1 and stage2prod1 with differing metadata",
        ),
        (
            "resources/stage2prod1.json",
            |v| {
                v["proxmox"]["snapshot"]["verified"] = json!(false);
                v["proxmox"]["snapshot"]["guids"] = json!({});
                v["proxmox"]["snapshot"]["volume_guids"] = json!({});
                v["state"] = json!("snapshotting");
            },
            resources,
            "is claimed by stage1prod1 and stage2prod1 before verification",
        ),
        (
            "resources/stage2prod1.json",
            |v| v["vmid"] = json!(2001),
            resources,
            "duplicate vmid 2001: stage1prod1 and stage2prod1",
        ),
        (
            "orchestration/prod2.json",
            |v| drop(v.as_object_mut().unwrap().remove("install_mode")),
            orchestration,
            "orchestration record has a partial VM contract",
        ),
        (
            "orchestration/prod2.json",
            |v| v["extra"] = json!(1),
            orchestration,
            "orchestration record has unknown fields: extra",
        ),
        (
            "orchestration/prod2.json",
            |v| v["install_phase"] = json!("booting"),
            orchestration,
            "orchestration install phase is invalid",
        ),
        (
            "orchestration/prod2.json",
            |v| v["install_mode"] = json!("pxe"),
            orchestration,
            "orchestration install_mode is invalid",
        ),
        (
            "orchestration/prod2.json",
            |v| v["startup_sha256"] = json!("ABC"),
            orchestration,
            "startup_sha256 must be lowercase SHA-256 or null",
        ),
        (
            "orchestration/prod2.json",
            |v| v["final_network_enabled"] = json!("yes"),
            orchestration,
            "final_network_enabled must be boolean",
        ),
        (
            "orchestration/prod2.json",
            |v| v["lease"]["nonce"] = json!("zz"),
            orchestration,
            "orchestration lease nonce is invalid",
        ),
        (
            "orchestration/prod2.json",
            |v| v["lease"]["expires_at"] = v["lease"]["acquired_at"].clone(),
            orchestration,
            "lease expiry must follow acquisition",
        ),
        (
            "orchestration/prod2.json",
            |v| v["lease"] = json!("held"),
            orchestration,
            "orchestration lease must be an object or null",
        ),
        (
            "orchestration/prod2.json",
            |v| v["resource_id"] = json!("00000000-0000-4000-8000-000000000000"),
            orchestration,
            "belongs to an obsolete resource",
        ),
        (
            "orchestration/prod2.json",
            |v| v["updated_at"] = json!("2000-01-01T00:00:00+00:00"),
            orchestration,
            "orchestration updated_at predates created_at",
        ),
        (
            "hosts/mox1.json",
            |v| v["state"] = json!("leaving"),
            hosts,
            "unknown host slot state: 'leaving'",
        ),
        (
            "hosts/mox1.json",
            |v| v["schema_version"] = json!(2),
            hosts,
            "host slot schema version is unsupported",
        ),
        (
            "hosts/mox1.json",
            |v| v["node"] = json!("mox2"),
            hosts,
            "host slot filename does not match record",
        ),
        (
            "cluster-control.json",
            |v| v["control_node"] = json!("mox9"),
            control,
            "mox9 exceeds policy max_hosts=4",
        ),
        (
            "cluster-control.json",
            |v| v["record_type"] = json!("control"),
            control,
            "cluster control record has the wrong record_type",
        ),
        (
            "ingress/desired.json",
            |v| v["nodes"] = json!(["mox1", "mox2", "mox3"]),
            desired,
            "must contain every configured mox in order",
        ),
        (
            "ingress/desired.json",
            |v| v["bundle_sha256"] = json!(H1),
            desired,
            "desired ingress pointer differs from generation metadata",
        ),
        (
            "ingress/desired.json",
            |v| v["updated_at"] = json!("2000-01-01T00:00:00+00:00"),
            desired,
            "ingress desired updated_at predates created_at",
        ),
        (
            "ingress/desired.json",
            |v| v["generation"] = json!("ABC"),
            desired,
            "desired ingress generation must be a lowercase SHA-256",
        ),
        (
            "ingress/desired.json",
            |v| v["record_type"] = json!("x"),
            desired,
            "ingress desired record has the wrong record_type",
        ),
        (
            "ingress/nodes/mox1.json",
            |v| v["state"] = json!("applied"),
            nodes,
            "applied ingress node status must match its desired generation",
        ),
        (
            "ingress/nodes/mox1.json",
            |v| v["state"] = json!("done"),
            nodes,
            "ingress node status state is invalid",
        ),
        (
            "ingress/nodes/mox1.json",
            |v| v["record_type"] = json!("x"),
            nodes,
            "ingress node status has the wrong record_type",
        ),
        (
            "ingress/transition.json",
            |v| v["allowances"][0]["generation"] = json!(H3),
            transition,
            "an allowance must name a previous generation",
        ),
        (
            "ingress/transition.json",
            |v| {
                let a = v["allowances"][0].clone();
                v["allowances"].as_array_mut().unwrap().push(a)
            },
            transition,
            "ingress transition allowances must be unique and ordered",
        ),
        (
            "ingress/transition.json",
            |v| v["allowances"] = json!([]),
            transition,
            "ingress transition allowances must be a non-empty list",
        ),
        (
            "deferred-cleanup",
            |v| v["target"] = json!("vm:abc"),
            cleanup,
            "destroy-vm cleanup target must be vm:VMID",
        ),
        (
            "deferred-cleanup",
            |v| {
                v["action"] = json!("delete-snapshot");
                v["target"] = json!("vm-1001@__replicate_1")
            },
            cleanup,
            "reserved __replicate_ prefix",
        ),
        (
            "deferred-cleanup",
            |v| {
                v["action"] = json!("destroy-volume");
                v["target"] = json!("vm:1001")
            },
            cleanup,
            "destroy-volume cleanup target must be STORAGE:vm-VMID-disk-N",
        ),
        (
            "deferred-cleanup",
            |v| {
                v["action"] = json!("remove-replication");
                v["target"] = json!("1001")
            },
            cleanup,
            "remove-replication cleanup target must be VMID-JOB",
        ),
        (
            "deferred-cleanup",
            |v| {
                v["action"] = json!("remove-route");
                v["target"] = json!("web1")
            },
            cleanup,
            "remove-route cleanup target must be a resource name",
        ),
        (
            "deferred-cleanup",
            |v| v["reason"] = json!("two\nlines"),
            cleanup,
            "cleanup reason is invalid",
        ),
        (
            "deferred-cleanup",
            |v| v["attempts"] = json!(-1),
            cleanup,
            "cleanup attempts is invalid",
        ),
        (
            "deferred-cleanup",
            |v| v["node"] = json!("node3"),
            cleanup,
            "cleanup node name is invalid",
        ),
        (
            "deferred-cleanup",
            |v| v["id"] = json!("cleanup-other"),
            cleanup,
            "cleanup filename does not match record id",
        ),
        (
            "history",
            |v| v["record_type"] = json!("other"),
            history,
            "history record has an unknown record_type",
        ),
        (
            "history",
            |v| v["extra"] = json!(1),
            history,
            "release history record has unknown fields: extra",
        ),
    ];
    let mut failures = Vec::new();
    for (file, mutate, read, expected) in cases {
        let relative = match file {
            "deferred-cleanup" => cleanup_file.clone(),
            "history" => history_file.clone(),
            other => other.to_string(),
        };
        let path = fixture.path(&relative);
        let original = std::fs::read(&path).unwrap();
        let mut value: Value = serde_json::from_slice(&original).unwrap();
        mutate(&mut value);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        match read(&registry) {
            Ok(()) => failures.push(format!("{relative}: accepted, expected {expected:?}")),
            Err(e) if !e.to_string().contains(expected) => {
                failures.push(format!("{relative}: {e:?} does not contain {expected:?}"))
            }
            Err(_) => {}
        }
        std::fs::write(&path, &original).unwrap();
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    let generation_record = fixture.file(&generation);
    let mut oversized = generation_record.clone();
    oversized["route_count"] = json!(5000);
    std::fs::write(
        fixture.path(&generation),
        serde_json::to_vec(&oversized).unwrap(),
    )
    .unwrap();
    assert!(
        registry
            .ingress_desired()
            .unwrap_err()
            .to_string()
            .contains("ingress route_count must be between 0 and 4096")
    );
    std::fs::write(
        fixture.path(&generation),
        serde_json::to_vec(&generation_record).unwrap(),
    )
    .unwrap();

    std::fs::copy(
        fixture.path("resources/prod1.json"),
        fixture.path("resources/prod9.json"),
    )
    .unwrap();
    let error = registry.resources().unwrap_err().to_string();
    assert!(
        error.starts_with("resource filename does not match record name"),
        "{error}"
    );
    std::fs::remove_file(fixture.path("resources/prod9.json")).unwrap();

    registry.resources().expect("every mutation was restored");
}

#[test]
fn staging_clones_live_on_their_sources_storage() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("registry");
    #[rustfmt::skip]
    v2(&state, &[
        "init", "--network", "10.0.0.0/24", "--guest-gateway", "10.0.0.1/24",
        "--mox-ip-start", "10.0.0.11", "--mox-ip-end", "10.0.0.20",
        "--haproxy-ip-start", "10.0.0.21", "--haproxy-ip-end", "10.0.0.30",
        "--production-ip-start", "10.0.0.101", "--production-ip-end", "10.0.0.150",
        "--staging-ip-start", "10.0.0.151", "--staging-ip-end", "10.0.0.250",
    ]);
    #[rustfmt::skip]
    v2(&state, &[
        "allocate-prod", "--vmid", "1001", "--purpose", "shop",
        "--primary-domain", "shop.example.com", "--placement", "mox1,mox2",
        "--storage", "bmac-zfs-64k",
    ]);
    #[rustfmt::skip]
    let staging = [
        "allocate-staging", "--source", "prod1", "--vmid", "2001", "--placement", "mox2",
        "--placement-limit", "2",
    ];
    let refused = Command::new("python3")
        .arg(legacy_registry())
        .arg("--state-dir")
        .arg(&state)
        .args(staging)
        .args(["--storage", "local-zfs"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("staging clones live on their source's storage, bmac-zfs-64k"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    v2(&state, &staging);

    let registry = Registry::new(Pmxcfs::new("/nonexistent-pmxcfs", Arc::new(RealFs)), &state);
    let storages: Vec<_> = registry
        .resources()
        .unwrap()
        .iter()
        .map(|r| (r.name.to_string(), r.spec.storage.clone()))
        .collect();
    assert_eq!(
        storages,
        [
            ("prod1".to_string(), "bmac-zfs-64k".to_string()),
            ("stage1prod1".to_string(), "bmac-zfs-64k".to_string()),
        ]
    );
}

#[test]
fn legacy_and_absent_vm_contracts_are_accepted() {
    let fixture = Fixture::new();
    let registry = fixture.registry();
    let path = fixture.path("orchestration/prod2.json");
    let original = fixture.file("orchestration/prod2.json");

    let mut legacy = original.clone();
    legacy.as_object_mut().unwrap().remove("source_iso_sha256");
    legacy.as_object_mut().unwrap().remove("install_mode");
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let record = registry.orchestration(prod(2)).unwrap().unwrap();
    assert_eq!(
        (record.final_network_enabled, record.install_mode),
        (Some(true), None)
    );

    let mut none = legacy.clone();
    none.as_object_mut()
        .unwrap()
        .remove("final_network_enabled");
    none.as_object_mut().unwrap().remove("startup_sha256");
    none["lease"] = Value::Null;
    std::fs::write(&path, serde_json::to_vec(&none).unwrap()).unwrap();
    let record = registry.orchestration(prod(2)).unwrap().unwrap();
    assert_eq!((record.startup_sha256, record.lease), (None, None));
    assert_eq!(
        serde_json::to_value(registry.orchestration(prod(2)).unwrap()).unwrap(),
        none
    );
}

/// `serde_json::to_value` behind a trait object, so one closure serializes
/// every record type.
mod erased {
    pub trait Serialize {
        fn to_json(&self) -> serde_json::Value;
    }

    impl<T: serde::Serialize> Serialize for T {
        fn to_json(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap()
        }
    }
}
