// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::sync::Arc;

use bmac_exec::{FakeRunner, Reply};
use bmac_pmxcfs::RealFs;
use serde_json::{Value, json};

use super::*;

const SUFFIX: &str = "tail1234.ts.net";

fn ca_pem() -> String {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.self_signed(&key).unwrap().pem()
}

fn host(n: u8) -> HostSlot {
    HostSlot::new(n).unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    ca_pem: String,
    pmxcfs: Pmxcfs,
    root: PathBuf,
    runner: Arc<FakeRunner>,
    joins: Joins,
}

fn cluster_values() -> Value {
    json!({
        "PROXMOX_CLUSTER_NAME": "lab",
        "PROXMOX_INTERNAL_DOMAIN": "pve.internal",
        "MAX_MOX_HOSTS": "3",
        "PRIVATE_SUBNET_CIDR": "10.213.0.0/24",
        "MOX_IP_START": "10.213.0.11",
        "MOX_IP_END": "10.213.0.20",
        "MOX_REPLICATION_IP_START": "10.213.0.241",
        "MOX_REPLICATION_IP_END": "10.213.0.250",
        "PROXMOX_MIGRATION_NETWORK": "10.213.0.240/28",
        "DATACENTER_PRIVATE_VLAN_GATEWAY": "10.213.0.1",
        "GUEST_EGRESS_VIP": "10.213.0.10/24",
        "HAPROXY_IP_START": "10.213.0.21",
        "HAPROXY_IP_END": "10.213.0.30",
        "PRODUCTION_IP_START": "10.213.0.31",
        "PRODUCTION_IP_END": "10.213.0.50",
        "STAGING_IP_START": "10.213.0.51",
        "STAGING_IP_END": "10.213.0.200",
    })
}

fn record(record_type: &str, extra: Value) -> Value {
    let mut value = json!({
        "schema_version": 1, "record_type": record_type, "revision": 1,
        "updated_by": "setup", "updated_at_ms": 1,
    });
    for (k, v) in extra.as_object().unwrap() {
        value[k] = v.clone();
    }
    value
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let pmxcfs = Pmxcfs::new(&root, Arc::new(RealFs));
    let config = root.join("config");
    pmxcfs
        .write_json(
            &config.join("cluster.json"),
            &record("cluster_settings", json!({"values": cluster_values()})),
        )
        .unwrap();
    pmxcfs
        .write_json(
            &config.join("hosts/mox1.json"),
            &record("host_facts", json!({"host": "mox1", "values": {}})),
        )
        .unwrap();
    let ca = root.join("pve-root-ca.pem");
    let ca_pem = ca_pem();
    std::fs::write(&ca, &ca_pem).unwrap();
    let clock: Clock = Arc::new(|| 1_000);
    let runner = Arc::new(FakeRunner::new());
    let joins = Joins {
        store: JoinStore::new(
            pmxcfs.clone(),
            root.join("joins"),
            root.join("cluster.json"),
        ),
        tokens: Arc::new(Tokens::new(
            pmxcfs.clone(),
            root.join("enrollment"),
            host(1),
            clock.clone(),
        )),
        settings: Settings::new(pmxcfs.clone(), &config),
        identities: Identities::new(pmxcfs.clone(), root.join("identity")),
        runner: runner.clone(),
        clock,
        ca,
    };
    Fixture {
        _dir: dir,
        ca_pem,
        pmxcfs,
        root,
        runner,
        joins,
    }
}

fn publish_mox1(f: &Fixture) {
    f.pmxcfs
        .write_json(
            &f.root.join("identity/mox1.json"),
            &json!({"schema_version": 1, "record_type": "host_identity", "revision": 1,
                    "host": "mox1", "https_fingerprint": vec!["AB"; 32].join(":"),
                    "ssh_host_key": null, "ssh_host_key_fingerprint": null,
                    "tailnet_name": format!("bmaclab-mox1.{SUFFIX}"), "updated_at_ms": 1}),
        )
        .unwrap();
}

fn tailscale_status() -> Reply {
    Reply::ok(
        json!({"BackendState": "Running", "Self": null,
               "CurrentTailnet": {"Name": "example.org", "MagicDNSSuffix": SUFFIX}})
        .to_string(),
    )
}

fn issue_request() -> IssueJoin {
    IssueJoin {
        host: host(2),
        max_prod_vms: 2,
        max_staging_vms: 3,
        idrac_ip: Some("192.0.2.9".into()),
        ttl_hours: Some(24),
    }
}

#[test]
fn the_cluster_record_keeps_its_uuid_and_gathers_digests() {
    let f = fixture();
    let store = &f.joins.store;
    assert_eq!(store.cluster().unwrap(), None);
    store.record_cluster("u1", &["a".into()]).unwrap();
    store
        .record_cluster("u1", &["a".into(), "b".into()])
        .unwrap();
    let record = store.cluster().unwrap().unwrap();
    assert_eq!(record.used_tailscale_key_sha256, ["a", "b"]);
    assert_eq!(record.revision, 2);
    store.record_cluster("u1", &["b".into()]).unwrap();
    assert_eq!(store.cluster().unwrap().unwrap().revision, 2, "nothing new");
    let error = store.record_cluster("u2", &[]).unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict);
}

#[test]
fn issuing_checks_the_request() {
    let taken = [host(1)];
    let mut request = issue_request();
    assert_eq!(check_issue(&request, &taken).unwrap(), 24);
    request.host = host(1);
    assert!(
        check_issue(&request, &taken)
            .unwrap_err()
            .field_errors
            .contains_key("host")
    );
    let mut request = issue_request();
    request.ttl_hours = Some(MAX_TTL_HOURS + 1);
    assert!(check_issue(&request, &taken).is_err());
    request.ttl_hours = None;
    assert_eq!(check_issue(&request, &taken).unwrap(), DEFAULT_TTL_HOURS);
    request.idrac_ip = Some("idrac".into());
    assert!(
        check_issue(&request, &taken)
            .unwrap_err()
            .field_errors
            .contains_key("idrac_ip")
    );
    request.idrac_ip = None;
    request.max_prod_vms = 0;
    assert!(check_issue(&request, &taken).is_err());
}

#[tokio::test]
async fn a_bundle_names_the_cluster_and_its_hosts_and_keeps_only_hashes() {
    let f = fixture();
    publish_mox1(&f);
    f.joins
        .store
        .record_cluster("c0ffee", &["d1".into()])
        .unwrap();
    f.runner
        .expect(
            bmac_adapters::programs::TAILSCALE,
            ["status", "--json"],
            tailscale_status(),
        )
        .expect(PYTHON, [REGISTRY, "host-reserve", "mox2"], Reply::ok("{}"));
    let issued = f.joins.issue(issue_request(), "root@pam").await.unwrap();
    f.runner.assert_done();
    assert_eq!(issued.file_name, "lab-mox2-join.json");
    let bundle: JoinBundle = serde_json::from_str(&issued.content).unwrap();
    assert_eq!(bundle.cluster_uuid, "c0ffee");
    assert_eq!(bundle.slot, host(2));
    assert_eq!(bundle.used_tailscale_key_sha256, ["d1"]);
    assert_eq!(bundle.host_settings["MAX_PROD_VM_COUNT_ON_THIS_HOST"], "2");
    assert_eq!(bundle.host_settings["IDRAC_IP"], "192.0.2.9");
    assert_eq!(bundle.expires_ms, 1_000 + 24 * 3_600_000);
    assert_eq!(bundle.hosts.len(), 1);
    assert_eq!(
        bundle.hosts[0].dashboard_url,
        format!("https://bmaclab-mox1.{SUFFIX}:8443")
    );
    assert_eq!(bundle.hosts[0].private_ip, "10.213.0.11");
    assert_eq!(
        bundle.proxmox_ca_fingerprint,
        ca_fingerprint(f.ca_pem.as_bytes()).unwrap()
    );
    assert!(bundle.setup_ssh_public_key.starts_with("ssh-ed25519 "));

    let stored = f.joins.store.join(host(2)).unwrap().unwrap();
    assert_eq!(stored.setup_ssh_public_key, bundle.setup_ssh_public_key);
    assert_eq!(
        Some(stored.ticket_sha256.clone()),
        crate::mesh::tokens::token_hash(&bundle.join_token)
    );
    let key = std::fs::read_to_string(f.joins.store.key_path(host(2))).unwrap();
    assert!(key.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
    for entry in walk(&f.root) {
        if entry.ends_with("mox2.key") {
            continue;
        }
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        assert!(
            !text.contains(&bundle.join_token),
            "{} holds the token",
            entry.display()
        );
        assert!(
            !text.contains("OPENSSH PRIVATE KEY"),
            "{} holds the key",
            entry.display()
        );
    }

    let list = f.joins.list(true).unwrap();
    assert_eq!(list.joins.len(), 1);
    assert_eq!(list.joins[0].host, host(2));
    assert_eq!(list.joins[0].used_at_ms, None);
    assert_eq!(list.joins[0].request, None);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test]
async fn an_installed_slot_and_a_cluster_without_identity_are_refused() {
    let f = fixture();
    publish_mox1(&f);
    let mut request = issue_request();
    request.host = host(1);
    assert!(f.joins.issue(request, "root@pam").await.is_err());
    let error = f
        .joins
        .issue(issue_request(), "root@pam")
        .await
        .unwrap_err();
    assert!(
        error.message.contains("identity record"),
        "{}",
        error.message
    );
    f.runner.assert_done();
}

#[tokio::test]
async fn revoking_forgets_the_token_the_key_and_the_reservation() {
    let f = fixture();
    publish_mox1(&f);
    f.joins.store.record_cluster("c0ffee", &[]).unwrap();
    f.runner
        .expect(
            bmac_adapters::programs::TAILSCALE,
            ["status", "--json"],
            tailscale_status(),
        )
        .expect(PYTHON, [REGISTRY, "host-reserve", "mox2"], Reply::ok("{}"));
    let issued = f.joins.issue(issue_request(), "root@pam").await.unwrap();
    let bundle: JoinBundle = serde_json::from_str(&issued.content).unwrap();
    let hash = crate::mesh::tokens::token_hash(&bundle.join_token).unwrap();
    assert!(f.joins.tokens.record(&hash).unwrap().is_some());

    f.runner.expect(
        PYTHON,
        [
            REGISTRY,
            "host-release",
            "mox2",
            "--expected-state",
            "joining",
            "--reason",
            "join bundle revoked",
        ],
        Reply::ok("{}"),
    );
    f.joins.revoke(host(2)).await.unwrap();
    f.runner.assert_done();
    assert!(f.joins.tokens.record(&hash).unwrap().is_none());
    assert!(f.joins.store.join(host(2)).unwrap().is_none());
    assert!(!f.joins.store.key_path(host(2)).exists());
    assert_eq!(
        f.joins.revoke(host(2)).await.unwrap_err().code,
        ErrorCode::NotFound
    );
}
