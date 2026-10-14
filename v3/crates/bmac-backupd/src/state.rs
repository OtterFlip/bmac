// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! What the appliance keeps on its OS disk (`BACKUP.md`, "Appliance
//! platform"): its identity and certificate, the pool it is bound to by
//! GUID, and one record per cluster it is enrolled with. Backup data and
//! the journal live on the pool, in the metadata dataset.

use std::io;
use std::path::{Path, PathBuf};

use bmac_core::HostSlot;
use bmac_proto::appliance::SourceHost;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fsx;

pub const STATE_DIR: &str = "/var/lib/bmac-backupd";
pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn system() -> Paths {
        Paths::at(STATE_DIR)
    }

    pub fn at(root: impl Into<PathBuf>) -> Paths {
        Paths { root: root.into() }
    }

    pub fn identity(&self) -> PathBuf {
        self.root.join("identity.json")
    }

    pub fn cert(&self) -> PathBuf {
        self.root.join("tls/appliance.pem")
    }

    pub fn key(&self) -> PathBuf {
        self.root.join("tls/appliance.key")
    }

    pub fn pool(&self) -> PathBuf {
        self.root.join("pool.json")
    }

    pub fn enrollments(&self) -> PathBuf {
        self.root.join("enrollments")
    }

    pub fn enrollment(&self, cluster_uuid: &str) -> PathBuf {
        self.enrollments().join(format!("{cluster_uuid}.json"))
    }

    pub fn admin(&self) -> PathBuf {
        self.root.join("admin.json")
    }

    /// Where the pool's metadata dataset is mounted.
    pub fn metadata(&self) -> PathBuf {
        self.root.join("metadata")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub schema_version: u32,
    pub appliance_uuid: Uuid,
    /// A lowercase DNS label, shown in the Dashboard.
    pub name: String,
    pub created_at_ms: i64,
}

/// The pool this appliance writes, by GUID: a pool of the same name but
/// another GUID is never used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolBinding {
    pub schema_version: u32,
    pub pool: String,
    pub pool_guid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    pub schema_version: u32,
    pub cluster_uuid: String,
    pub cluster_name: String,
    pub proxmox_ca_fingerprint: String,
    /// The hosts to check in with, from the bundle and then from offers.
    pub hosts: Vec<SourceHost>,
    pub request_id: Option<String>,
    pub enrolled_at_ms: i64,
    /// The namespace quota for this cluster, if the administrator set one.
    #[serde(default)]
    pub quota_bytes: Option<u64>,
}

impl Enrollment {
    /// Adds hosts an offer named, keeping the known ones' order.
    pub fn learn(&mut self, hosts: &[SourceHost]) -> bool {
        let mut changed = false;
        for host in hosts {
            match self.hosts.iter_mut().find(|h| h.host == host.host) {
                Some(known) if known.addresses != host.addresses && !host.addresses.is_empty() => {
                    known.addresses = host.addresses.clone();
                    changed = true;
                }
                Some(_) => {}
                None => {
                    self.hosts.push(host.clone());
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn host(&self, host: HostSlot) -> Option<&SourceHost> {
        self.hosts.iter().find(|h| h.host == host)
    }
}

pub fn load<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    fsx::read_json(path)
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    fsx::write_json(path, value, 0o600)
}

pub fn enrollments(paths: &Paths) -> io::Result<Vec<Enrollment>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(paths.enrollments()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Some(enrollment) = load::<Enrollment>(&path)?
        {
            out.push(enrollment);
        }
    }
    out.sort_by(|a, b| a.cluster_uuid.cmp(&b.cluster_uuid));
    Ok(out)
}

/// A lowercase DNS label, as the cluster requires of appliance names.
pub fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// A self-signed ECDSA P-256 certificate for `name`, as PEM, with its key.
/// It is the appliance's identity as a client and as a server; the cluster
/// and the peers pin its fingerprint, so it is valid for decades. Its
/// `urn:uuid:` name is what a refused handshake shows it claimed.
pub fn certificate(name: &str, appliance_uuid: &Uuid) -> Result<(String, String), String> {
    let key =
        rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(|e| e.to_string())?;
    let mut params =
        rcgen::CertificateParams::new(vec![name.to_string()]).map_err(|e| e.to_string())?;
    params.subject_alt_names.push(rcgen::SanType::URI(
        format!("urn:uuid:{appliance_uuid}")
            .try_into()
            .map_err(|e: rcgen::Error| e.to_string())?,
    ));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, format!("bmac-appliance {name}"));
    params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    params.not_after = rcgen::date_time_ymd(2076, 1, 1);
    let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// What a certificate says it is: its common name and any `urn:uuid:`
/// name. It proves nothing; the fingerprint is the identity.
pub fn claimed(der: &[u8]) -> String {
    use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};
    let Ok((_, cert)) = X509Certificate::from_der(der) else {
        return "an unreadable certificate".into();
    };
    let mut parts: Vec<String> = cert
        .subject()
        .iter_common_name()
        .filter_map(|cn| cn.as_str().ok().map(String::from))
        .collect();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            if let GeneralName::URI(uri) = name
                && let Some(uuid) = uri.strip_prefix("urn:uuid:")
            {
                parts.push(uuid.to_string());
            }
        }
    }
    let text: String = parts
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    if text.is_empty() {
        "nothing".into()
    } else {
        text
    }
}

/// SHA-256 of the first certificate in `pem`, lowercase hex.
pub fn pem_fingerprint(pem: &str) -> Result<String, String> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    let der = CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|e| e.to_string())?;
    Ok(crate::client::fingerprint(&der))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_dns_labels() {
        assert!(is_name("vault-1"));
        for bad in ["", "Vault", "-a", "a-", "a.b", &"a".repeat(64)] {
            assert!(!is_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_certificate_names_the_appliance_and_has_a_stable_fingerprint() {
        let (cert, key) = certificate("vault", &Uuid::from_u128(5)).unwrap();
        assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(key.contains("PRIVATE KEY"));
        let fp = pem_fingerprint(&cert).unwrap();
        assert_eq!(fp.len(), 64);
        let der = rustls::pki_types::pem::PemObject::from_pem_slice(cert.as_bytes())
            .map(|d: rustls::pki_types::CertificateDer<'static>| d.to_vec())
            .unwrap();
        assert_eq!(
            claimed(&der),
            format!("bmac-appliance vault {}", Uuid::from_u128(5))
        );
        assert_eq!(pem_fingerprint(&cert).unwrap(), fp);
    }

    #[test]
    fn offers_teach_new_hosts_and_new_addresses() {
        let mut e = Enrollment {
            schema_version: SCHEMA,
            cluster_uuid: "c".into(),
            cluster_name: "lab".into(),
            proxmox_ca_fingerprint: "ab".into(),
            hosts: vec![SourceHost {
                host: "mox1".parse().unwrap(),
                addresses: vec!["a".into()],
            }],
            request_id: None,
            enrolled_at_ms: 0,
            quota_bytes: None,
        };
        let mox1 = SourceHost {
            host: "mox1".parse().unwrap(),
            addresses: vec!["a".into()],
        };
        assert!(!e.learn(std::slice::from_ref(&mox1)));
        let mox2 = SourceHost {
            host: "mox2".parse().unwrap(),
            addresses: vec!["b".into()],
        };
        assert!(e.learn(&[mox2]));
        assert_eq!(e.hosts.len(), 2);
    }
}
