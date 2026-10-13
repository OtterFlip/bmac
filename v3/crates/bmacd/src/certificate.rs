// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Rotating this host's Dashboard certificate (`SECURITY.md`, "Rotation").
//! The engine makes a new key and certificate next to the current pair
//! (`https.next.key`, `https.next.pem`), the Dashboard shows the new
//! fingerprint over the current certificate, and on the operator's confirm
//! the engine moves the new pair into place, keeps the old one as
//! `https.previous.*`, and restarts the front end, which reads the pair as
//! credentials only at startup.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::certificate::{CertificateInfo, CertificateView};
use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use time::OffsetDateTime;

use crate::web::tls;

pub const TLS_DIR: &str = "/etc/bmac/tls";
/// How long a new certificate is valid, as the Deployer's are.
pub const VALIDITY_DAYS: i64 = 3650;

/// Called after a switch, to restart the front end and publish the new
/// fingerprint.
pub type AfterSwitch = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct Certificates {
    dir: PathBuf,
    host: String,
    after_switch: AfterSwitch,
}

impl std::fmt::Debug for Certificates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Certificates")
            .field("dir", &self.dir)
            .field("host", &self.host)
            .finish()
    }
}

fn internal(what: &str, e: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorCode::Internal, format!("{what}: {e}"))
}

/// The fingerprint, names, and validity of the first certificate in `pem`.
pub fn info(pem: &[u8]) -> Result<CertificateInfo, String> {
    let der = CertificateDer::pem_slice_iter(pem)
        .next()
        .ok_or("no certificate")?
        .map_err(|e| e.to_string())?;
    let params = CertificateParams::from_ca_cert_der(&der).map_err(|e| e.to_string())?;
    let names = params
        .subject_alt_names
        .iter()
        .filter_map(|san| match san {
            SanType::DnsName(name) => Some(name.as_str().to_string()),
            SanType::IpAddress(ip) => Some(ip.to_string()),
            _ => None,
        })
        .collect();
    let ms = |t: OffsetDateTime| (t.unix_timestamp_nanos() / 1_000_000) as i64;
    Ok(CertificateInfo {
        fingerprint: tls::fingerprint(&der),
        names,
        not_before_ms: ms(params.not_before),
        not_after_ms: ms(params.not_after),
    })
}

/// Writes `contents` to `path` through a temporary file, with `mode`.
fn write_file(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temporary, path)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

impl Certificates {
    pub fn new(
        dir: impl Into<PathBuf>,
        host: impl Into<String>,
        after_switch: AfterSwitch,
    ) -> Certificates {
        Certificates {
            dir: dir.into(),
            host: host.into(),
            after_switch,
        }
    }

    pub fn system(host: impl Into<String>, after_switch: AfterSwitch) -> Certificates {
        Certificates::new(TLS_DIR, host, after_switch)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn read_info(&self, name: &str) -> Option<CertificateInfo> {
        fs::read(self.file(name))
            .ok()
            .and_then(|pem| info(&pem).ok())
    }

    /// The pending pair, only when both files are there.
    fn pending(&self) -> Option<CertificateInfo> {
        self.file("https.next.key")
            .exists()
            .then(|| self.read_info("https.next.pem"))
            .flatten()
    }

    pub fn view(&self, can_rotate: bool) -> CertificateView {
        CertificateView {
            host: self.host.clone(),
            current: self.read_info("https.pem"),
            pending: self.pending(),
            can_rotate,
        }
    }

    /// Makes a new key and certificate with the current certificate's names,
    /// replacing any pending pair.
    pub fn rotate(&self, now_ms: i64) -> Result<CertificateInfo, ApiError> {
        let current = fs::read(self.file("https.pem")).map_err(|e| {
            ApiError::new(
                ErrorCode::Unavailable,
                format!(
                    "this host's certificate can't be read, so there are no names to copy: {e}"
                ),
            )
        })?;
        let der = CertificateDer::pem_slice_iter(&current)
            .next()
            .and_then(Result::ok)
            .ok_or_else(|| internal("the current certificate", "no certificate in the file"))?;
        let old = CertificateParams::from_ca_cert_der(&der)
            .map_err(|e| internal("the current certificate", e))?;
        let now = OffsetDateTime::from_unix_timestamp(now_ms.div_euclid(1000))
            .map_err(|e| internal("the clock", e))?;
        let mut params = CertificateParams::default();
        params.subject_alt_names = old.subject_alt_names;
        params.distinguished_name = old.distinguished_name;
        params.not_before = now;
        params.not_after = now + time::Duration::days(VALIDITY_DAYS);
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().map_err(|e| internal("a new key", e))?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| internal("a new certificate", e))?;
        let made = info(cert.pem().as_bytes()).map_err(|e| internal("the new certificate", e))?;
        // The key goes first: a certificate without its key is how a switch
        // interrupted between its two renames looks (see `repair`).
        let write = || -> io::Result<()> {
            write_file(
                &self.file("https.next.key"),
                key.serialize_pem().as_bytes(),
                0o600,
            )?;
            write_file(&self.file("https.next.pem"), cert.pem().as_bytes(), 0o644)?;
            sync_dir(&self.dir)
        };
        write().map_err(|e| internal("writing the new certificate", e))?;
        Ok(made)
    }

    /// Throws away the pending pair. Returns whether there was one.
    pub fn discard(&self) -> Result<bool, ApiError> {
        let had = self.pending().is_some();
        // The certificate goes first, for the same reason as in `rotate`.
        for name in ["https.next.pem", "https.next.key"] {
            match fs::remove_file(self.file(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(internal("removing the new certificate", e)),
            }
        }
        Ok(had)
    }

    /// Moves the pending pair into place when its fingerprint is
    /// `fingerprint`, keeping the current pair as `https.previous.*`, then
    /// runs the after-switch hook.
    pub fn switch(&self, fingerprint: &str) -> Result<CertificateInfo, ApiError> {
        let Some(pending) = self.pending() else {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                "there is no new certificate to switch to; make one first",
            ));
        };
        if !pending.fingerprint.eq_ignore_ascii_case(fingerprint.trim()) {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                "the new certificate changed since you saw it; check its fingerprint again",
            ));
        }
        let read = |name: &str| fs::read(self.file(name)).map_err(|e| internal(name, e));
        tls::server_config(&read("https.next.pem")?, &read("https.next.key")?)
            .map_err(|e| internal("the new certificate and key don't load together", e))?;
        let switch = || -> io::Result<()> {
            for (from, to) in [
                ("https.key", "https.previous.key"),
                ("https.pem", "https.previous.pem"),
            ] {
                if self.file(from).exists() {
                    fs::copy(self.file(from), self.file(to))?;
                }
            }
            fs::rename(self.file("https.next.key"), self.file("https.key"))?;
            fs::rename(self.file("https.next.pem"), self.file("https.pem"))?;
            sync_dir(&self.dir)
        };
        switch().map_err(|e| internal("switching certificates", e))?;
        (self.after_switch)();
        Ok(pending)
    }

    /// Finishes a switch that stopped between moving the key and moving the
    /// certificate, so the front end starts with a matching pair. Returns
    /// whether it did.
    pub fn repair(&self) -> io::Result<bool> {
        if self.file("https.next.pem").exists() && !self.file("https.next.key").exists() {
            fs::rename(self.file("https.next.pem"), self.file("https.pem"))?;
            sync_dir(&self.dir)?;
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const NOW_MS: i64 = 1_790_000_000_000;

    fn deployed(dir: &Path) {
        let mut params =
            CertificateParams::new(vec!["mox1".into(), "mox1.example.ts.net".into()]).unwrap();
        params
            .subject_alt_names
            .push(SanType::IpAddress("10.10.0.11".parse().unwrap()));
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        fs::write(dir.join("https.key"), key.serialize_pem()).unwrap();
        fs::write(dir.join("https.pem"), cert.pem()).unwrap();
    }

    fn certificates(dir: &Path) -> (Certificates, Arc<AtomicUsize>) {
        let switched = Arc::new(AtomicUsize::new(0));
        let count = switched.clone();
        let hook: AfterSwitch = Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        });
        (Certificates::new(dir, "mox1", hook), switched)
    }

    #[test]
    fn a_rotation_keeps_the_names_and_switches_on_the_shown_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        deployed(dir.path());
        let (certs, switched) = certificates(dir.path());
        let before = certs.view(true);
        let current = before.current.clone().unwrap();
        assert_eq!(current.names, ["mox1", "mox1.example.ts.net", "10.10.0.11"]);
        assert_eq!(before.pending, None);

        let made = certs.rotate(NOW_MS).unwrap();
        assert_ne!(made.fingerprint, current.fingerprint);
        assert_eq!(made.names, current.names);
        assert_eq!(made.not_before_ms, NOW_MS);
        assert_eq!(made.not_after_ms, NOW_MS + VALIDITY_DAYS * 86_400_000);
        let mode = |name: &str| {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("https.next.key"), 0o600);
        assert_eq!(certs.view(true).pending, Some(made.clone()));
        assert_eq!(
            certs.view(true).current,
            Some(current.clone()),
            "nothing switched yet"
        );

        let again = certs.rotate(NOW_MS).unwrap();
        let refused = certs.switch(&made.fingerprint).unwrap_err();
        assert_eq!(
            refused.code,
            ErrorCode::Conflict,
            "replaced since it was shown"
        );
        assert_eq!(switched.load(Ordering::SeqCst), 0);

        let done = certs.switch(&again.fingerprint.to_lowercase()).unwrap();
        assert_eq!(done, again);
        let after = certs.view(true);
        assert_eq!(after.current, Some(again));
        assert_eq!(after.pending, None);
        assert_eq!(certs.read_info("https.previous.pem"), Some(current));
        tls::server_config(
            &fs::read(dir.path().join("https.pem")).unwrap(),
            &fs::read(dir.path().join("https.key")).unwrap(),
        )
        .expect("the new pair loads");
        assert_eq!(switched.load(Ordering::SeqCst), 1);
        assert_eq!(
            certs.switch(&done.fingerprint).unwrap_err().code,
            ErrorCode::Conflict
        );
    }

    #[test]
    fn a_pending_certificate_can_be_discarded() {
        let dir = tempfile::tempdir().unwrap();
        deployed(dir.path());
        let (certs, _) = certificates(dir.path());
        assert!(!certs.discard().unwrap());
        certs.rotate(NOW_MS).unwrap();
        assert!(certs.discard().unwrap());
        assert_eq!(certs.view(true).pending, None);
        assert!(!dir.path().join("https.next.key").exists());
    }

    #[test]
    fn without_a_certificate_there_is_nothing_to_rotate() {
        let dir = tempfile::tempdir().unwrap();
        let (certs, _) = certificates(dir.path());
        assert_eq!(certs.view(false).current, None);
        assert_eq!(
            certs.rotate(NOW_MS).unwrap_err().code,
            ErrorCode::Unavailable
        );
    }

    #[test]
    fn an_interrupted_switch_is_finished_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        deployed(dir.path());
        let (certs, _) = certificates(dir.path());
        let made = certs.rotate(NOW_MS).unwrap();
        assert!(!certs.repair().unwrap(), "a pending pair is left alone");
        fs::rename(
            dir.path().join("https.next.key"),
            dir.path().join("https.key"),
        )
        .unwrap();
        assert!(certs.repair().unwrap());
        assert_eq!(certs.view(true).current, Some(made));
        tls::server_config(
            &fs::read(dir.path().join("https.pem")).unwrap(),
            &fs::read(dir.path().join("https.key")).unwrap(),
        )
        .expect("a matching pair");
    }
}
