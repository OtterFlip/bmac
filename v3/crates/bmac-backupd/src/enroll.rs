// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Enrollment (`DEPLOYMENT.md`, "Backup appliance ISO"): the appliance
//! presents the bundle's token and its certificate to a host's
//! `POST /v1/enroll`, after checking that host's chain against the bundle's
//! CA fingerprint, and keeps everything but the token. The administrator
//! then approves the request in the Dashboard.

use bmac_core::ApplianceAddress;
use bmac_proto::appliance::{ENROLL_BUNDLE_VERSION, EnrollBundle};
use bmac_proto::peer::{ENROLL_PATH, EnrollRequest, PEER_PORT, Pending};

use crate::client::{CallError, ClusterClient};
use crate::state::{self, Enrollment, Identity, Paths, SCHEMA};

/// What the operator sees after enrolling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    pub cluster_name: String,
    pub request_id: String,
    pub fingerprint: String,
    pub answered_by: String,
}

pub fn read_bundle(text: &str, now_ms: i64) -> Result<EnrollBundle, String> {
    let bundle: EnrollBundle =
        serde_json::from_str(text).map_err(|e| format!("not an enrollment bundle: {e}"))?;
    if bundle.version != ENROLL_BUNDLE_VERSION {
        return Err(format!(
            "bundle version {} is not supported (expected {ENROLL_BUNDLE_VERSION})",
            bundle.version
        ));
    }
    if bundle.expires_ms <= now_ms {
        return Err("the bundle has expired; issue a new one in the Dashboard".into());
    }
    if bundle.hosts.is_empty() {
        return Err("the bundle names no host".into());
    }
    let mut bundle = bundle;
    bundle.proxmox_ca_fingerprint =
        bmac_proto::appliance::normalized_fingerprint(&bundle.proxmox_ca_fingerprint)
            .ok_or("the bundle's CA fingerprint is not a SHA-256")?;
    Ok(bundle)
}

pub async fn enroll(
    paths: &Paths,
    identity: &Identity,
    bundle: &EnrollBundle,
    address: Option<ApplianceAddress>,
    port: u16,
    now_ms: i64,
) -> Result<Enrolled, String> {
    let cert =
        std::fs::read_to_string(paths.cert()).map_err(|e| format!("the certificate: {e}"))?;
    let client = ClusterClient::new(&bundle.proxmox_ca_fingerprint, None, port)?;
    let request = EnrollRequest {
        token: bundle.enroll_token.clone(),
        appliance_uuid: identity.appliance_uuid,
        name: identity.name.clone(),
        certificate: cert.clone(),
        address,
    };
    let mut last = String::from("no host answered");
    for host in &bundle.hosts {
        match client
            .call::<_, Pending>(host, "POST", ENROLL_PATH, Some(&request))
            .await
        {
            Ok(pending) => {
                let enrollment = Enrollment {
                    schema_version: SCHEMA,
                    cluster_uuid: bundle.cluster_uuid.clone(),
                    cluster_name: bundle.cluster_name.clone(),
                    proxmox_ca_fingerprint: bundle.proxmox_ca_fingerprint.to_ascii_lowercase(),
                    hosts: bundle.hosts.clone(),
                    request_id: Some(pending.request_id.clone()),
                    enrolled_at_ms: now_ms,
                    quota_bytes: None,
                };
                state::save(&paths.enrollment(&bundle.cluster_uuid), &enrollment)
                    .map_err(|e| format!("cannot save the enrollment: {e}"))?;
                return Ok(Enrolled {
                    cluster_name: bundle.cluster_name.clone(),
                    request_id: pending.request_id,
                    fingerprint: state::pem_fingerprint(&cert)?,
                    answered_by: host.host.to_string(),
                });
            }
            Err(CallError::Refused(e)) => {
                return Err(format!("{} refused: {}", host.host, e.message));
            }
            Err(CallError::Unreachable(e)) => last = format!("{}: {e}", host.host),
        }
    }
    Err(last)
}

/// The port enrollment uses.
pub const PORT: u16 = PEER_PORT;

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(expires_ms: i64) -> String {
        serde_json::json!({
            "version": ENROLL_BUNDLE_VERSION,
            "cluster_uuid": "c",
            "cluster_name": "lab",
            "enroll_token": "t",
            "expires_ms": expires_ms,
            "proxmox_ca_fingerprint": "ab".repeat(32),
            "hosts": [{"host": "mox1", "addresses": ["bmaclab-mox1"]}],
        })
        .to_string()
    }

    #[test]
    fn a_bundle_is_checked_before_use() {
        assert!(read_bundle(&bundle(10), 5).is_ok());
        assert!(
            read_bundle(&bundle(10), 10)
                .unwrap_err()
                .contains("expired")
        );
        assert!(read_bundle("{}", 0).is_err());
        let short = bundle(10).replace(&"ab".repeat(32), "ab");
        assert!(read_bundle(&short, 0).unwrap_err().contains("SHA-256"));
    }
}
