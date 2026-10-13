// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Each host's published keys (`SECURITY.md`, "Certificates"): the
//! fingerprint of its Dashboard certificate and its SSH host key, written by
//! its own engine to `/etc/pve/priv/bmac/identity/moxN.json`. Once an
//! operator has verified one host's certificate in the browser, that host's
//! Settings page lists every other host's fingerprints from these records.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use bmac_core::HostSlot;
use bmac_pmxcfs::{LockOptions, Pmxcfs, PmxcfsError, RegistryLock, read_record, write_record};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::settings::HostIdentity;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::Clock;
use crate::log;

pub const IDENTITY_DIR: &str = "/etc/pve/priv/bmac/identity";
/// The front end's certificate, which its unit passes it as a credential.
pub const HTTPS_CERT: &str = "/etc/bmac/tls/https.pem";
pub const SSH_HOST_KEY: &str = "/etc/ssh/ssh_host_ed25519_key.pub";
const LOCK_PREFIX: &str = "bmac-identity-";
const RECORD_SCHEMA: u64 = 1;
/// How often a host checks its keys again, so a rotated certificate shows.
pub const REPUBLISH: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityRecord {
    schema_version: u64,
    record_type: String,
    #[serde(default)]
    revision: u64,
    host: HostSlot,
    https_fingerprint: Option<String>,
    ssh_host_key: Option<String>,
    ssh_host_key_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tailnet_name: Option<String>,
    updated_at_ms: i64,
}

impl IdentityRecord {
    fn view(&self) -> HostIdentity {
        HostIdentity {
            host: self.host,
            https_fingerprint: self.https_fingerprint.clone(),
            ssh_host_key: self.ssh_host_key.clone(),
            ssh_host_key_fingerprint: self.ssh_host_key_fingerprint.clone(),
            tailnet_name: self.tailnet_name.clone(),
            updated_at_ms: self.updated_at_ms,
        }
    }
}

/// An Ed25519 public key line, without its comment, and its fingerprint as
/// `ssh-keygen -l` prints it.
pub fn ssh_host_key(line: &str) -> Option<(String, String)> {
    let mut fields = line.split_whitespace();
    let (kind, encoded) = (fields.next()?, fields.next()?);
    if kind != "ssh-ed25519" {
        return None;
    }
    let blob = STANDARD.decode(encoded).ok()?;
    // A length-prefixed "ssh-ed25519", then a length-prefixed 32-byte key.
    let expected: Vec<u8> = [
        &11u32.to_be_bytes()[..],
        b"ssh-ed25519",
        &32u32.to_be_bytes()[..],
    ]
    .concat();
    if blob.len() != expected.len() + 32 || !blob.starts_with(&expected) {
        return None;
    }
    let fingerprint = format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&blob)));
    Some((format!("ssh-ed25519 {encoded}"), fingerprint))
}

fn storage(e: PmxcfsError) -> ApiError {
    ApiError::new(ErrorCode::Internal, format!("host identities: {e}"))
}

#[derive(Clone)]
pub struct Identities {
    pmxcfs: Pmxcfs,
    dir: PathBuf,
}

impl Identities {
    pub fn new(pmxcfs: Pmxcfs, dir: impl Into<PathBuf>) -> Identities {
        Identities {
            pmxcfs,
            dir: dir.into(),
        }
    }

    pub fn system() -> Identities {
        Identities::new(Pmxcfs::system(), IDENTITY_DIR)
    }

    fn path(&self, host: HostSlot) -> PathBuf {
        self.dir.join(format!("{host}.json"))
    }

    fn read(&self, host: HostSlot) -> Result<Option<IdentityRecord>, ApiError> {
        let Some(value) = read_record(
            &self.pmxcfs,
            &self.path(host),
            "host identity",
            RECORD_SCHEMA,
        )
        .map_err(storage)?
        else {
            return Ok(None);
        };
        let record: IdentityRecord = serde_json::from_value(value).map_err(|e| {
            ApiError::new(
                ErrorCode::Internal,
                format!("{}: {e}", self.path(host).display()),
            )
        })?;
        Ok((record.host == host).then_some(record))
    }

    /// Every host's published keys, by slot.
    pub fn all(&self) -> Result<Vec<HostIdentity>, ApiError> {
        let mut out = Vec::new();
        for host in HostSlot::all() {
            if let Some(record) = self.read(host)? {
                out.push(record.view());
            }
        }
        Ok(out)
    }

    /// Publishes `host`'s keys from the files and its MagicDNS name, when
    /// they differ from what is published. A name that can't be read now
    /// keeps the one published before. Returns whether anything was written.
    pub fn publish(
        &self,
        host: HostSlot,
        cert: &Path,
        ssh_key: &Path,
        tailnet_name: Option<String>,
        now_ms: i64,
    ) -> Result<bool, ApiError> {
        let https_fingerprint = std::fs::read(cert)
            .ok()
            .and_then(|pem| crate::web::tls::pem_fingerprint(&pem).ok());
        let ssh = std::fs::read_to_string(ssh_key)
            .ok()
            .and_then(|line| ssh_host_key(&line));
        let (ssh_host_key, ssh_host_key_fingerprint) = ssh.unzip();
        let current = self.read(host)?;
        let tailnet_name = tailnet_name.or_else(|| current.as_ref()?.tailnet_name.clone());
        if let Some(c) = &current
            && c.https_fingerprint == https_fingerprint
            && c.ssh_host_key == ssh_host_key
            && c.tailnet_name == tailnet_name
        {
            return Ok(false);
        }
        self.pmxcfs
            .ensure_directory(&self.dir, 0o700)
            .map_err(storage)?;
        let options = LockOptions {
            timeout: Duration::from_secs(15),
            prefix: LOCK_PREFIX.to_string(),
            short_lived: true,
            ..LockOptions::default()
        };
        let lock = RegistryLock::acquire(&self.pmxcfs, &self.dir, &options).map_err(storage)?;
        let record = IdentityRecord {
            schema_version: RECORD_SCHEMA,
            record_type: "host_identity".into(),
            revision: 0,
            host,
            https_fingerprint,
            ssh_host_key,
            ssh_host_key_fingerprint,
            tailnet_name,
            updated_at_ms: now_ms,
        };
        let value = serde_json::to_value(&record).expect("identity serializes");
        let written = write_record(&lock, &self.path(host), value, current.map(|c| c.revision))
            .map_err(storage);
        let released = lock.release().map_err(storage);
        written?;
        released?;
        Ok(true)
    }

    /// Publishes this host's keys now and every [`REPUBLISH`].
    pub fn spawn_publisher(self, host: HostSlot, clock: Clock) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REPUBLISH);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let name = own_tailnet_name(&bmac_exec::RealRunner).await;
                let (me, now) = (self.clone(), clock());
                let published = tokio::task::spawn_blocking(move || {
                    me.publish(
                        host,
                        Path::new(HTTPS_CERT),
                        Path::new(SSH_HOST_KEY),
                        name,
                        now,
                    )
                })
                .await;
                match published {
                    Ok(Ok(true)) => {
                        log::info("published this host's certificate and SSH host key fingerprints")
                    }
                    Ok(Ok(false)) => {}
                    Ok(Err(e)) => log::warning(&format!(
                        "cannot publish this host's fingerprints: {}",
                        e.message
                    )),
                    Err(e) => log::error(&format!("cannot publish this host's fingerprints: {e}")),
                }
            }
        })
    }
}

/// This host's MagicDNS name, without the trailing dot.
pub async fn own_tailnet_name(runner: &dyn bmac_exec::CommandRunner) -> Option<String> {
    let status = bmac_adapters::tailscale::Tailscale::new(runner)
        .status()
        .await
        .ok()?;
    let name = status.self_node?.dns_name;
    let name = name.trim_end_matches('.');
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_pmxcfs::RealFs;
    use std::sync::Arc;

    /// v2 test_setup_jump_ssh_access.py's fixture key and its fingerprint
    /// from `ssh-keygen -lf`.
    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBhE1+iRU5AP8b2hmfLfAHJQ/BVRAR0n4ddx0DFCnkg4 root@mox2";

    fn identities(dir: &Path) -> Identities {
        Identities::new(Pmxcfs::new(dir, Arc::new(RealFs)), dir.join("identity"))
    }

    #[test]
    fn ssh_host_keys_are_ed25519_lines() {
        let (key, fingerprint) = ssh_host_key(KEY).unwrap();
        assert_eq!(
            key,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBhE1+iRU5AP8b2hmfLfAHJQ/BVRAR0n4ddx0DFCnkg4"
        );
        assert_eq!(
            fingerprint,
            "SHA256:6pyryPWb/Q2XEYAJxJkDp9mclGwSmIvrNn6QEmRg9gY"
        );
        assert_eq!(ssh_host_key("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ"), None);
        assert_eq!(
            ssh_host_key("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5"),
            None,
            "a truncated key"
        );
        assert_eq!(ssh_host_key("ssh-ed25519 not-base64!"), None);
    }

    #[test]
    fn a_host_publishes_its_keys_once_until_they_change() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh_host_ed25519_key.pub");
        std::fs::write(&ssh, format!("{KEY}\n")).unwrap();
        let cert = dir.path().join("missing.pem");
        let ids = identities(dir.path());
        let mox2 = HostSlot::new(2).unwrap();
        assert!(ids.publish(mox2, &cert, &ssh, None, 10).unwrap());
        assert!(
            !ids.publish(mox2, &cert, &ssh, None, 20).unwrap(),
            "nothing changed"
        );
        let all = ids.all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].host, mox2);
        assert_eq!(all[0].https_fingerprint, None, "no certificate yet");
        assert_eq!(
            all[0].ssh_host_key_fingerprint,
            ssh_host_key(KEY).map(|k| k.1)
        );
        assert_eq!(all[0].updated_at_ms, 10);

        std::fs::write(&ssh, "garbage\n").unwrap();
        assert!(ids.publish(mox2, &cert, &ssh, None, 30).unwrap());
        assert_eq!(ids.all().unwrap()[0].ssh_host_key, None);
    }

    #[test]
    fn a_tailnet_name_is_published_and_kept_when_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("key.pub");
        std::fs::write(&ssh, KEY).unwrap();
        let cert = dir.path().join("missing.pem");
        let ids = identities(dir.path());
        let mox3 = HostSlot::new(3).unwrap();
        let name = || Some("bmaclab-mox3.example.ts.net".to_string());
        assert!(ids.publish(mox3, &cert, &ssh, None, 1).unwrap());
        assert!(ids.publish(mox3, &cert, &ssh, name(), 2).unwrap());
        assert!(!ids.publish(mox3, &cert, &ssh, None, 3).unwrap());
        assert_eq!(ids.all().unwrap()[0].tailnet_name, name());
    }

    #[test]
    fn a_record_under_another_hosts_name_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("key.pub");
        std::fs::write(&ssh, KEY).unwrap();
        let ids = identities(dir.path());
        ids.publish(
            HostSlot::new(1).unwrap(),
            &dir.path().join("none"),
            &ssh,
            None,
            1,
        )
        .unwrap();
        std::fs::rename(
            dir.path().join("identity/mox1.json"),
            dir.path().join("identity/mox3.json"),
        )
        .unwrap();
        assert!(ids.all().unwrap().is_empty());
    }
}
