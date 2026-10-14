// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::net::IpAddr;
use std::path::Path;

use bmac_core::HostSlot;
use bmac_pmxcfs::{Pmxcfs, RealFs};
use bmac_proto::peer::EnrollRequest;
use serde_json::{Value, json};

use super::*;
use crate::backup::source::enrolled_targets;

fn enrolled_fingerprints(store: &BackupStore) -> Result<Vec<String>, ApiError> {
    Ok(enrolled_targets(store)?
        .into_iter()
        .map(|t| t.fingerprint)
        .collect())
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

fn self_signed(name: &str) -> String {
    let key = rcgen::KeyPair::generate().unwrap();
    rcgen::CertificateParams::new(vec![name.to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap()
        .pem()
}

fn fixture(root: &Path) -> Appliances {
    let pmxcfs = Pmxcfs::new(root, Arc::new(RealFs));
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
    pmxcfs
        .write_json(
            &root.join("identity/mox1.json"),
            &json!({"schema_version": 1, "record_type": "host_identity", "revision": 1,
                    "host": "mox1", "https_fingerprint": vec!["AB"; 32].join(":"),
                    "ssh_host_key": null, "ssh_host_key_fingerprint": null,
                    "tailnet_name": "bmaclab-mox1.tail1234.ts.net", "updated_at_ms": 1}),
        )
        .unwrap();
    let ca = root.join("pve-root-ca.pem");
    std::fs::write(&ca, self_signed("ca")).unwrap();
    let clock: Clock = Arc::new(|| 1_000);
    let joins = JoinStore::new(
        pmxcfs.clone(),
        root.join("joins"),
        root.join("cluster.json"),
    );
    joins.record_cluster("c0ffee", &[]).unwrap();
    Appliances {
        store: BackupStore::new(pmxcfs.clone(), root.join("backup")),
        joins,
        tokens: Arc::new(Tokens::new(
            pmxcfs.clone(),
            root.join("enrollment"),
            HostSlot::new(1).unwrap(),
            clock.clone(),
        )),
        settings: Settings::new(pmxcfs.clone(), &config),
        identities: Identities::new(pmxcfs, root.join("identity")),
        clock,
        ca,
    }
}

fn from() -> IpAddr {
    "100.64.0.9".parse().unwrap()
}

/// Issues a bundle and enrolls `name` with it; the request's ID.
fn enroll(appliances: &Appliances, name: &str, id: Uuid, cert: &str) -> String {
    let issued = appliances
        .issue(&IssueEnroll::default(), "root@pam")
        .unwrap();
    let bundle: EnrollBundle = serde_json::from_str(&issued.content).unwrap();
    appliances
        .tokens
        .redeem_enroll(
            &EnrollRequest {
                token: bundle.enroll_token,
                appliance_uuid: id,
                name: name.into(),
                certificate: cert.into(),
                address: None,
            },
            from(),
        )
        .unwrap()
        .request_id
}

fn files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files(&path));
        } else {
            out.push(std::fs::read_to_string(&path).unwrap_or_default());
        }
    }
    out
}

#[test]
fn a_bundle_names_the_ca_and_each_hosts_addresses_and_keeps_only_the_tokens_hash() {
    let dir = tempfile::tempdir().unwrap();
    let appliances = fixture(dir.path());
    let issued = appliances
        .issue(&IssueEnroll::default(), "root@pam")
        .unwrap();
    assert_eq!(issued.file_name, "lab-appliance-enroll.json");
    let bundle: EnrollBundle = serde_json::from_str(&issued.content).unwrap();
    let ca = ca_fingerprint(&std::fs::read(&appliances.ca).unwrap()).unwrap();
    assert_eq!(
        Some(bundle.proxmox_ca_fingerprint.clone()),
        normalized_fingerprint(&ca)
    );
    assert_eq!(bundle.cluster_uuid, "c0ffee");
    assert_eq!(bundle.cluster_name, "lab");
    assert_eq!(
        bundle.expires_ms,
        1_000 + i64::from(DEFAULT_TTL_HOURS) * 3_600_000
    );
    assert_eq!(
        bundle.hosts,
        [SourceHost {
            host: HostSlot::new(1).unwrap(),
            addresses: vec!["bmaclab-mox1.tail1234.ts.net".into(), "10.213.0.11".into()],
        }]
    );
    assert!(!format!("{bundle:?}").contains(&bundle.enroll_token));
    assert!(
        files(dir.path())
            .iter()
            .all(|text| !text.contains(&bundle.enroll_token)),
        "the token is stored in clear"
    );
    for ttl in [0, MAX_TTL_HOURS + 1] {
        let request = IssueEnroll {
            ttl_hours: Some(ttl),
        };
        assert!(appliances.issue(&request, "root@pam").is_err());
    }
}

#[test]
fn approval_records_exactly_the_certificate_the_administrator_compared() {
    let dir = tempfile::tempdir().unwrap();
    let appliances = fixture(dir.path());
    let id = Uuid::new_v4();
    let cert = self_signed("vault");
    let fingerprint = normalized_fingerprint(&ca_fingerprint(cert.as_bytes()).unwrap()).unwrap();
    let request = enroll(&appliances, "vault", id, &cert);

    let list = appliances.list(true).unwrap();
    assert!(list.appliances.is_empty());
    assert_eq!(list.requests.len(), 1);
    assert_eq!(
        list.requests[0].fingerprint,
        Some(display_fingerprint(&fingerprint))
    );
    assert_eq!(list.requests[0].appliance_uuid, Some(id.to_string()));
    assert_eq!(list.requests[0].from, "100.64.0.9");

    let other =
        normalized_fingerprint(&ca_fingerprint(self_signed("x").as_bytes()).unwrap()).unwrap();
    let wrong = appliances.approve(&request, &ApproveAppliance { fingerprint: other });
    assert!(wrong.unwrap_err().message.contains("doesn't match"));
    assert!(enrolled_fingerprints(&appliances.store).unwrap().is_empty());

    let record = appliances
        .approve(
            &request,
            &ApproveAppliance {
                fingerprint: display_fingerprint(&fingerprint),
            },
        )
        .unwrap();
    assert_eq!(record.appliance_uuid, id);
    assert_eq!(record.name, "vault");
    assert_eq!(record.enrollment, Enrollment::Approved);
    assert_eq!(
        enrolled_fingerprints(&appliances.store).unwrap(),
        std::slice::from_ref(&fingerprint)
    );
    let list = appliances.list(false).unwrap();
    assert!(list.requests.is_empty());
    assert_eq!(list.appliances[0].enrollment, "approved");
    assert!(
        appliances
            .approve(&request, &ApproveAppliance { fingerprint })
            .is_err()
    );

    appliances.revoke(&id).unwrap();
    assert!(enrolled_fingerprints(&appliances.store).unwrap().is_empty());
    assert_eq!(
        appliances.list(false).unwrap().appliances[0].enrollment,
        "revoked"
    );
}

#[test]
fn a_second_appliance_cannot_take_a_name_and_a_rejected_request_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let appliances = fixture(dir.path());
    let cert = self_signed("vault");
    let first = enroll(&appliances, "vault", Uuid::new_v4(), &cert);
    let fingerprint = ca_fingerprint(cert.as_bytes()).unwrap();
    appliances
        .approve(&first, &ApproveAppliance { fingerprint })
        .unwrap();

    let cert = self_signed("vault");
    let second = enroll(&appliances, "vault", Uuid::new_v4(), &cert);
    let fingerprint = ca_fingerprint(cert.as_bytes()).unwrap();
    let clash = appliances
        .approve(&second, &ApproveAppliance { fingerprint })
        .unwrap_err();
    assert!(
        clash.message.contains("already has that name"),
        "{}",
        clash.message
    );

    appliances.reject(&second).unwrap();
    assert!(appliances.list(true).unwrap().requests.is_empty());
    assert!(appliances.reject(&second).is_err());
}

#[test]
fn an_appliance_enrolling_again_keeps_its_record_with_the_new_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let appliances = fixture(dir.path());
    let id = Uuid::new_v4();
    let old = self_signed("vault");
    let request = enroll(&appliances, "vault", id, &old);
    let fingerprint = ca_fingerprint(old.as_bytes()).unwrap();
    appliances
        .approve(&request, &ApproveAppliance { fingerprint })
        .unwrap();
    appliances.revoke(&id).unwrap();

    let new = self_signed("vault");
    let request = enroll(&appliances, "vault", id, &new);
    let fingerprint = ca_fingerprint(new.as_bytes()).unwrap();
    appliances
        .approve(
            &request,
            &ApproveAppliance {
                fingerprint: fingerprint.clone(),
            },
        )
        .unwrap();
    assert_eq!(appliances.store.appliances().unwrap().len(), 1);
    assert_eq!(
        enrolled_fingerprints(&appliances.store).unwrap(),
        [normalized_fingerprint(&fingerprint).unwrap()]
    );
}

#[test]
fn an_appliance_address_is_any_ip_address_or_dns_name() {
    let dir = tempfile::tempdir().unwrap();
    let appliances = fixture(dir.path());
    let issued = appliances
        .issue(&IssueEnroll::default(), "root@pam")
        .unwrap();
    let bundle: EnrollBundle = serde_json::from_str(&issued.content).unwrap();
    let id = Uuid::new_v4();
    let cert = self_signed("vault");
    let request = EnrollRequest {
        token: bundle.enroll_token,
        appliance_uuid: id,
        name: "vault".into(),
        certificate: cert.clone(),
        address: Some("100.100.7.7".parse().unwrap()),
    };
    let pending = appliances.tokens.redeem_enroll(&request, from()).unwrap();
    let fingerprint = normalized_fingerprint(&ca_fingerprint(cert.as_bytes()).unwrap()).unwrap();
    let record = appliances
        .approve(&pending.request_id, &ApproveAppliance { fingerprint })
        .unwrap();
    assert_eq!(
        record.address.unwrap().to_string(),
        "100.100.7.7",
        "the suggested address is kept"
    );

    for address in [
        "10.213.0.220",
        "203.0.113.5:9445",
        "[fd7a:115c:a1e0::9]:8445",
        "vault.tail1234.ts.net",
        "backup.example.org",
    ] {
        let record = appliances.set_address(&id, Some(address)).unwrap();
        assert_eq!(record.address.unwrap().to_string(), address);
    }
    let error = appliances
        .set_address(&id, Some("not an address!"))
        .unwrap_err();
    assert!(error.field_errors.contains_key("address"));
    assert_eq!(
        appliances.set_address(&id, Some(" ")).unwrap().address,
        None
    );
}
