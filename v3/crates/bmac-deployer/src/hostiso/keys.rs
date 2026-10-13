// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The keys made fresh for every ISO: the host's Ed25519 SSH host key, so
//! the workstation can pin it without trusting on first use, and the HTTPS
//! key and self-signed certificate (`SECURITY.md`, "Browser HTTPS"). An
//! older ISO may still exist, and its keys must not identify this install.
//! Both are made in memory; the private halves are written only into the
//! ISO's first-boot payload.

use std::fmt;
use std::net::IpAddr;

use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use sha2::{Digest, Sha256};

/// How long a certificate is valid, as `bmacd`'s rotation makes them.
pub const VALIDITY_DAYS: i64 = 3650;

pub use bmac_core::sshkey::{SshHostKey, ssh_fingerprint};

/// The HTTPS key and certificate for one host.
pub struct HttpsCertificate {
    pub key_pem: String,
    pub cert_pem: String,
    /// SHA-256 of the certificate's DER, as the host card and the browser
    /// show it: uppercase hex pairs joined by colons.
    pub fingerprint: String,
}

impl fmt::Debug for HttpsCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpsCertificate")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

pub fn der_fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

impl HttpsCertificate {
    /// An ECDSA P-256 certificate for `names` (DNS names or IP addresses),
    /// the first one also its common name.
    pub fn generate(names: &[String], now_ms: i64) -> Result<HttpsCertificate, String> {
        let first = names
            .first()
            .ok_or("a certificate needs at least one name")?;
        let mut params = CertificateParams::default();
        params.subject_alt_names = names
            .iter()
            .map(|name| match name.parse::<IpAddr>() {
                Ok(ip) => Ok(SanType::IpAddress(ip)),
                Err(_) => name
                    .clone()
                    .try_into()
                    .map(SanType::DnsName)
                    .map_err(|e| format!("{name:?} can't be a certificate name: {e}")),
            })
            .collect::<Result<_, _>>()?;
        let mut subject = DistinguishedName::new();
        subject.push(DnType::CommonName, first.as_str());
        params.distinguished_name = subject;
        let now = time::OffsetDateTime::from_unix_timestamp(now_ms.div_euclid(1000))
            .map_err(|e| format!("the clock: {e}"))?;
        params.not_before = now;
        params.not_after = now + time::Duration::days(VALIDITY_DAYS);
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|e| format!("a new key: {e}"))?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| format!("a new certificate: {e}"))?;
        Ok(HttpsCertificate {
            key_pem: key.serialize_pem(),
            cert_pem: cert.pem(),
            fingerprint: der_fingerprint(cert.der()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    use super::*;

    #[test]
    fn the_ssh_key_is_one_openssh_accepts() {
        let key = SshHostKey::generate("root@mox1.example.internal").unwrap();
        assert!(
            key.public
                .starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA")
        );
        assert!(key.public.ends_with(" root@mox1.example.internal"));
        assert!(!format!("{key:?}").contains("PRIVATE"));
        assert_ne!(
            key.public,
            SshHostKey::generate("root@mox1.example.internal")
                .unwrap()
                .public
        );
        // ssh-keygen derives the same public key from the private file.
        if Command::new("ssh-keygen").arg("-?").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host");
        std::fs::write(&path, &key.private).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let derived = Command::new("ssh-keygen")
            .arg("-y")
            .arg("-f")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            derived.status.success(),
            "{}",
            String::from_utf8_lossy(&derived.stderr)
        );
        let derived = String::from_utf8(derived.stdout).unwrap();
        let first_two = |s: &str| s.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        assert_eq!(first_two(&derived), first_two(&key.public));
        let listed = Command::new("ssh-keygen")
            .arg("-lf")
            .arg(&path)
            .output()
            .unwrap();
        let listed = String::from_utf8(listed.stdout).unwrap();
        assert!(listed.contains(&key.fingerprint()), "{listed}");
    }

    #[test]
    fn the_certificate_names_the_host_and_its_fingerprint_is_the_ders() {
        let names = vec![
            "mox1".to_string(),
            "mox1.example.internal".to_string(),
            "mox1.tail1234.ts.net".to_string(),
            "10.10.0.11".to_string(),
        ];
        let made = HttpsCertificate::generate(&names, 1_760_000_000_000).unwrap();
        assert!(made.key_pem.contains("PRIVATE KEY"));
        assert!(!format!("{made:?}").contains("PRIVATE"));
        let der = rustls_pemfile_der(&made.cert_pem);
        assert_eq!(made.fingerprint, der_fingerprint(&der));
        assert_eq!(made.fingerprint.len(), 32 * 3 - 1);
        let params = CertificateParams::from_ca_cert_der(&der.into()).unwrap();
        let sans: Vec<String> = params
            .subject_alt_names
            .iter()
            .map(|s| match s {
                SanType::DnsName(n) => n.as_str().to_string(),
                SanType::IpAddress(ip) => ip.to_string(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(sans, names);
        let days = (params.not_after - params.not_before).whole_days();
        assert_eq!(days, VALIDITY_DAYS);
    }

    fn rustls_pemfile_der(pem: &str) -> Vec<u8> {
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        STANDARD.decode(body).unwrap()
    }
}
