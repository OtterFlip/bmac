// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The native live reconciliation reports exactly what v2's
//! `cluster_registry.py reconcile --observed` reports for the same registry
//! and observation: codes, resources, severities, and wording.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use bmac_pmxcfs::{Pmxcfs, RealFs};
use bmac_registry::Registry;
use bmac_registry::reconcile::{Observed, ObservedVm, Severity, live};
use serde_json::{Value, json};

const VOLUME: &str = "local-zfs:vm-1001-disk-0";

fn v2(state: &Path, args: &[&str]) -> Value {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../legacy/scripts/host_runtime/cluster_registry.py");
    let output = Command::new("python3")
        .arg(script)
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

fn build(state: &Path) {
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
            "--placement",
            "mox1,mox2,mox3",
        ],
        vec![
            "update",
            "prod1",
            "--expected-revision",
            "1",
            "--state",
            "provisioning",
            "--volume-id",
            VOLUME,
            "--owner-node",
            "mox1",
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
            "allocate-prod",
            "--vmid",
            "1002",
            "--purpose",
            "blog",
            "--primary-domain",
            "blog.example.com",
            "--placement",
            "mox1,mox2",
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
            "prod3",
            "--expected-revision",
            "1",
            "--state",
            "provisioning",
        ],
        vec![
            "update",
            "prod3",
            "--expected-revision",
            "2",
            "--state",
            "stopped",
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
    ];
    for step in steps {
        v2(state, &step);
    }
}

#[test]
fn live_reconciliation_matches_v2() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("registry");
    build(&state);
    let registry = Registry::new(Pmxcfs::new("/nonexistent-pmxcfs", Arc::new(RealFs)), &state);
    let policy = registry.policy().unwrap();
    let resources = registry.resources().unwrap();

    // mox2 offline and mox3 absent; prod1 renamed, re-addressed, without its
    // volume; prod3 gone; stage1prod1 on mox3 without its evictable tag.
    let vms = vec![
        json!({"vmid": 1001, "name": "prod1x", "node": "mox1", "status": "stopped", "ip": null,
               "mac": "aa:bb:cc:dd:ee:ff", "tags": [policy.tags.production], "volume_ids": []}),
        json!({"vmid": 2001, "name": "stage1prod1", "node": "mox3", "status": "stopped", "ip": null,
               "mac": null, "tags": [policy.tags.staging], "volume_ids": ["local-zfs:vm-2001-disk-0"]}),
    ];
    let observed_json = json!({
        "schema_version": 1,
        "nodes": [{"name": "mox1", "online": true}, {"name": "mox2", "online": false}],
        "vms": vms,
        "volumes": [{"id": "local-zfs:vm-2001-disk-0"}],
    });
    let file: PathBuf = temporary.path().join("observed.json");
    std::fs::write(&file, observed_json.to_string()).unwrap();
    let report = v2(&state, &["reconcile", "--observed", file.to_str().unwrap()]);
    let theirs: Vec<(String, String, String, String)> = report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["severity"].as_str().unwrap().to_string(),
                i["code"].as_str().unwrap().to_string(),
                i["resource"].as_str().unwrap_or_default().to_string(),
                i["message"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    let observed = Observed {
        nodes: BTreeMap::from([("mox1".into(), true), ("mox2".into(), false)]),
        vms: vms
            .iter()
            .map(|vm| {
                let strings = |key: &str| -> BTreeSet<String> {
                    vm[key]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect()
                };
                let vmid = vm["vmid"].as_u64().unwrap().to_string().parse().unwrap();
                (
                    vmid,
                    ObservedVm {
                        vmid,
                        name: vm["name"].as_str().unwrap().into(),
                        node: vm["node"].as_str().unwrap().into(),
                        status: vm["status"].as_str().unwrap().into(),
                        mac: vm["mac"].as_str().map(str::to_string),
                        tags: strings("tags"),
                        volume_ids: strings("volume_ids"),
                    },
                )
            })
            .collect(),
    };
    let now = jiff::Timestamp::now().as_millisecond();
    let ours: Vec<(String, String, String, String)> = live(&resources, &policy, &observed, now)
        .into_iter()
        .map(|i| {
            let severity = match i.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            };
            (severity.into(), i.code.into(), i.resource, i.message)
        })
        .collect();
    assert!(!ours.is_empty());
    assert_eq!(ours, theirs);
}
