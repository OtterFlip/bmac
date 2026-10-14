// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The engine's answers on the source socket, with the real cluster,
//! registry, and settings behind [`super::source::Source`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use bmac_core::HostSlot;
use bmac_exec::{BoxFuture, RealRunner};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::appliance::{Heartbeat, LuksHeader, LuksHeaderDigest, LuksHeaders};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::capture::RegistryGuests;
use super::cluster::RealCluster;
use super::source::{Source, TransferDone, enrolled_fingerprints};
use super::source_socket::{MAX_HEADER, SourceCall, SourceHandler};
use super::store::BackupStore;
use crate::cluster::ClusterView;
use crate::log;

pub struct EngineSource {
    pub host: HostSlot,
    pub cluster: Arc<dyn ClusterView>,
    pub store: BackupStore,
    pub settings: crate::settings::Settings,
    pub luks_headers: PathBuf,
    /// `/etc/pve/priv/bmac`, for the recovery manifest.
    pub recovery_root: PathBuf,
    /// The engine's own listen addresses, whose IPs 8445 binds too.
    pub listen: Option<tokio::sync::watch::Receiver<Vec<std::net::SocketAddr>>>,
}

impl EngineSource {
    pub fn system(host: HostSlot, cluster: Arc<dyn ClusterView>) -> EngineSource {
        EngineSource {
            host,
            cluster,
            store: BackupStore::system(),
            settings: crate::settings::Settings::system(),
            luks_headers: PathBuf::from(crate::native::storage::HEADER_DIR),
            recovery_root: PathBuf::from(super::recovery::ROOT),
            listen: None,
        }
    }

    fn addresses(&self) -> BTreeMap<HostSlot, Vec<String>> {
        let mut out = BTreeMap::new();
        for facts in self.settings.hosts().unwrap_or_default() {
            if let Some(name) = facts
                .values
                .get("TAILSCALE_HOSTNAME")
                .filter(|n| !n.is_empty())
            {
                out.insert(facts.host, vec![name.clone()]);
            }
        }
        out
    }

    async fn answer(&self, call: SourceCall) -> Result<Value, ApiError> {
        let runner = RealRunner;
        let cluster = RealCluster {
            runner: &runner,
            node: self.host,
        };
        let guests = RegistryGuests {
            registry: bmac_registry::Registry::system(),
            runner: &runner,
            owner: format!("bmac-source-{}", self.host),
        };
        let known = self.addresses();
        let addresses = move |host: HostSlot| known.get(&host).cloned().unwrap_or_default();
        let cluster_uuid = crate::joins::JoinStore::system()
            .cluster()?
            .map(|c| c.cluster_uuid)
            .unwrap_or_default();
        let now = || jiff::Timestamp::now().as_millisecond();
        let source = Source {
            store: &self.store,
            cluster: &cluster,
            guests: &guests,
            addresses: &addresses,
            cluster_uuid: cluster_uuid.clone(),
            now_ms: &now,
        };
        match call {
            SourceCall::Fingerprints => Ok(json(&enrolled_fingerprints(&self.store)?)),
            SourceCall::Listen => {
                let mut ips: Vec<std::net::IpAddr> = self
                    .listen
                    .as_ref()
                    .map(|l| l.borrow().iter().map(|a| a.ip()).collect())
                    .unwrap_or_default();
                ips.sort();
                ips.dedup();
                Ok(json(&ips))
            }
            SourceCall::Checkin {
                fingerprint,
                request,
            } => {
                let mut reply = source.checkin(&fingerprint, &request)?;
                reply.recovery_revision = super::recovery::files(&self.recovery_root)
                    .ok()
                    .map(|f| bmac_proto::appliance::RecoveryManifest::revision_of(&f));
                Ok(json(&reply))
            }
            SourceCall::Claim {
                fingerprint,
                request,
            } => source.claim(&fingerprint, &request).map(|()| json!({})),
            SourceCall::Index {
                fingerprint,
                request,
            } => source.index(&fingerprint, request).map(|()| json!({})),
            SourceCall::Catalog { fingerprint } => Ok(json(&source.catalog(&fingerprint)?)),
            SourceCall::Locate {
                fingerprint,
                request,
            } => Ok(json(&source.locate(&fingerprint, &request).await?)),
            SourceCall::AuthorizeSend {
                fingerprint,
                request,
            } => Ok(json(&source.authorize_send(&fingerprint, &request).await?)),
            SourceCall::TransferDone {
                fingerprint,
                report,
            } => {
                let appliance = source.enrolled(&fingerprint)?;
                log_transfer(&appliance.name, &report);
                let transfer = bmac_proto::backups::TransferView {
                    at_ms: now(),
                    host: self.host.to_string(),
                    appliance: appliance.name.clone(),
                    vm_uuid: report.vm_uuid.to_string(),
                    pit_uuid: report.pit_uuid.to_string(),
                    volume_uuid: report.volume_uuid.to_string(),
                    bytes: report.bytes,
                    duration_ms: report.duration_ms,
                    error: report.error.clone(),
                };
                let store = self.store.clone();
                let recorded = tokio::task::spawn_blocking(move || {
                    store.locked(|l| l.record_transfer(transfer))
                })
                .await;
                if let Ok(Err(e)) = recorded {
                    log::warning(&format!("backup: the transfer history: {}", e.message));
                }
                Ok(json!({}))
            }
            SourceCall::Heartbeat { fingerprint } => {
                source.enrolled(&fingerprint)?;
                let membership = self
                    .cluster
                    .membership()
                    .map_err(|e| ApiError::new(ErrorCode::Unavailable, e))?;
                Ok(json(&Heartbeat {
                    host: self.host,
                    now_ms: now(),
                    quorate: membership.quorate,
                    online: membership.online.into_iter().collect(),
                }))
            }
            SourceCall::LuksHeaders { fingerprint } => {
                source.enrolled(&fingerprint)?;
                Ok(json(&self.luks_headers()?))
            }
            SourceCall::LuksHeader {
                fingerprint,
                request,
            } => {
                source.enrolled(&fingerprint)?;
                Ok(json(&self.luks_header(&request.name)?))
            }
            SourceCall::RecoveryManifest { fingerprint } => {
                source.enrolled(&fingerprint)?;
                if cluster_uuid.is_empty() {
                    return Err(ApiError::new(
                        ErrorCode::Unavailable,
                        "the cluster has no identity record yet",
                    ));
                }
                let manifest =
                    super::recovery::manifest(&self.recovery_root, &cluster_uuid, self.host, now())
                        .map_err(|e| ApiError::new(ErrorCode::Internal, e))?;
                Ok(json(&manifest))
            }
        }
    }

    fn luks_headers(&self) -> Result<LuksHeaders, ApiError> {
        list_headers(&self.luks_headers, self.host)
    }

    fn luks_header(&self, name: &str) -> Result<LuksHeader, ApiError> {
        read_header(&self.luks_headers, self.host, name)
    }
}

/// The header backups in `dir`, each with its digest.
pub fn list_headers(dir: &std::path::Path, host: HostSlot) -> Result<LuksHeaders, ApiError> {
    let mut headers = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LuksHeaders { host, headers });
        }
        Err(e) => return Err(ApiError::new(ErrorCode::Internal, e.to_string())),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(bytes) = header_bytes(dir, &name) {
            headers.insert(
                name,
                LuksHeaderDigest {
                    sha256: sha256_hex(&bytes),
                    size: bytes.len() as u64,
                },
            );
        }
    }
    Ok(LuksHeaders { host, headers })
}

fn header_bytes(dir: &std::path::Path, name: &str) -> Result<Vec<u8>, ApiError> {
    let missing = || ApiError::new(ErrorCode::NotFound, format!("no LUKS header backup {name}"));
    if name.is_empty() || name.starts_with('.') || name.contains('/') {
        return Err(missing());
    }
    let path = dir.join(name);
    let meta = std::fs::symlink_metadata(&path).map_err(|_| missing())?;
    if !meta.is_file() || meta.len() > MAX_HEADER {
        return Err(missing());
    }
    std::fs::read(&path).map_err(|e| ApiError::new(ErrorCode::Internal, e.to_string()))
}

/// One header backup, compressed.
pub fn read_header(
    dir: &std::path::Path,
    host: HostSlot,
    name: &str,
) -> Result<LuksHeader, ApiError> {
    use std::io::Write as _;
    let bytes = header_bytes(dir, name)?;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let compressed = gzip
        .write_all(&bytes)
        .and_then(|()| gzip.finish())
        .map_err(|e| ApiError::new(ErrorCode::Internal, e.to_string()))?;
    Ok(LuksHeader {
        host,
        name: name.to_string(),
        sha256: sha256_hex(&bytes),
        gzip: base64::engine::general_purpose::STANDARD.encode(compressed),
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// One event per transfer: bytes, duration, and the average rate.
pub fn transfer_line(appliance: &str, report: &TransferDone) -> String {
    let seconds = report.duration_ms as f64 / 1000.0;
    let rate = if report.duration_ms == 0 {
        0.0
    } else {
        report.bytes as f64 / seconds / (1024.0 * 1024.0)
    };
    let what = format!(
        "backup: sent PIT {} volume {} of VM {} to {appliance}: {} bytes in {seconds:.1} s ({rate:.1} MiB/s)",
        report.pit_uuid, report.volume_uuid, report.vm_uuid, report.bytes
    );
    match &report.error {
        None => what,
        Some(e) => format!("{what}, then it failed: {e}"),
    }
}

fn log_transfer(appliance: &str, report: &TransferDone) {
    let line = transfer_line(appliance, report);
    if report.error.is_some() {
        log::warning(&line);
    } else {
        log::info(&line);
    }
}

/// The datasets of the `zfspool` storages in `storage.cfg` that `host`
/// may use: those without a `nodes` line, or naming it.
pub fn zfs_pools(storage_cfg: &str, host: &str) -> Vec<String> {
    let mut pools = Vec::new();
    let mut section: Option<(Option<String>, bool)> = None;
    let mut finish = |section: &mut Option<(Option<String>, bool)>| {
        if let Some((Some(pool), true)) = section.take()
            && !pools.contains(&pool)
        {
            pools.push(pool);
        }
    };
    for line in storage_cfg.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            finish(&mut section);
            section = line
                .split_once(':')
                .filter(|(kind, _)| kind.trim() == "zfspool")
                .map(|_| (None, true));
            continue;
        }
        let Some((pool, here)) = section.as_mut() else {
            continue;
        };
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("pool"), Some(dataset)) => *pool = Some(dataset.to_string()),
            (Some("nodes"), Some(nodes)) => {
                *here = nodes.split(',').any(|n| n.trim() == host);
            }
            _ => {}
        }
    }
    finish(&mut section);
    pools
}

/// What `bmac-source` may do on the guest storages, and nothing more.
pub const DELEGATED: &str = "send,hold,release";

/// Lets `bmac-source` send, hold, and release snapshots on this host's ZFS
/// guest storages; `zfs allow` is idempotent.
pub async fn delegate(
    runner: &dyn bmac_exec::CommandRunner,
    storage_cfg: &str,
    host: &str,
) -> Vec<(String, Result<(), String>)> {
    let mut out = Vec::new();
    for pool in zfs_pools(storage_cfg, host) {
        let command = bmac_exec::Command::new(super::daemon::ZFS)
            .args([
                "allow",
                "-u",
                super::source_socket::SOURCE_USER,
                DELEGATED,
                pool.as_str(),
            ])
            .timeout(std::time::Duration::from_secs(60));
        let done = runner
            .run(command)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string());
        out.push((pool, done));
    }
    out
}

/// How often the delegation is reapplied, for storages added since.
const DELEGATE_EVERY: std::time::Duration = std::time::Duration::from_secs(600);

/// The engine's side of `bmac-source`: the socket, and the delegation the
/// daemon needs. `None` when the package's user is missing.
pub fn spawn(
    host: HostSlot,
    cluster: Arc<dyn ClusterView>,
    listen: tokio::sync::watch::Receiver<Vec<std::net::SocketAddr>>,
) -> Result<Vec<tokio::task::JoinHandle<()>>, String> {
    use super::source_socket::{SOURCE_DIRECTORY, SOURCE_SOCKET, SOURCE_USER};
    let account = crate::peers::lookup_account("/etc/passwd".as_ref(), SOURCE_USER)
        .map_err(|e| e.to_string())?;
    super::source_socket::prepare_directory(SOURCE_DIRECTORY.as_ref(), account.gid)
        .map_err(|e| format!("cannot prepare {SOURCE_DIRECTORY}: {e}"))?;
    let listener = crate::engine::server::bind(SOURCE_SOCKET.as_ref(), Some(account.gid))
        .map_err(|e| format!("cannot open {SOURCE_SOCKET}: {e}"))?;
    let mut handler = EngineSource::system(host, cluster);
    handler.listen = Some(listen);
    let serve = tokio::spawn(super::source_socket::serve(
        listener,
        crate::peers::AllowedPeers::new([0, account.uid]),
        Arc::new(handler),
        std::future::pending(),
    ));
    let delegation = tokio::spawn(async move {
        let mut last: BTreeMap<String, bool> = BTreeMap::new();
        loop {
            let cfg =
                std::fs::read_to_string(crate::native::storage::STORAGE_CFG).unwrap_or_default();
            for (pool, done) in delegate(&RealRunner, &cfg, &host.to_string()).await {
                let ok = done.is_ok();
                if last.get(&pool) != Some(&ok) {
                    match done {
                        Ok(()) => {
                            log::info(&format!("backup: bmac-source may {DELEGATED} on {pool}"))
                        }
                        Err(e) => log::warning(&format!(
                            "backup: cannot delegate {DELEGATED} on {pool} to bmac-source: {e}"
                        )),
                    }
                }
                last.insert(pool, ok);
            }
            tokio::time::sleep(DELEGATE_EVERY).await;
        }
    });
    Ok(vec![serve, delegation])
}

fn json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("source answers serialize")
}

impl SourceHandler for EngineSource {
    fn handle(&self, call: SourceCall) -> BoxFuture<'_, Result<Value, ApiError>> {
        Box::pin(self.answer(call))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_zfs_pools_are_this_hosts_zfspool_storages() {
        let cfg = "dir: local\n\tpath /var/lib/vz\n\tcontent iso\n\n\
                   zfspool: local-zfs\n\tpool rpool/data\n\tsparse\n\tcontent images,rootdir\n\n\
                   zfspool: fast\n\tpool tank/vm\n\tnodes mox2,mox3\n\n\
                   zfspool: here\n        pool tank/here\n        nodes mox1\n\
                   lvmthin: data\n\tthinpool data\n\tvgname pve\n\
                   zfspool: again\n\tpool rpool/data\n";
        assert_eq!(zfs_pools(cfg, "mox1"), ["rpool/data", "tank/here"]);
        assert_eq!(zfs_pools(cfg, "mox3"), ["rpool/data", "tank/vm"]);
        assert!(zfs_pools("", "mox1").is_empty());
    }

    #[test]
    fn a_luks2_header_backup_fits_one_reply() {
        let dir = tempfile::tempdir().unwrap();
        let host: HostSlot = "mox1".parse().unwrap();
        let mut image = vec![0u8; 16 * 1024 * 1024];
        image[..4096].fill(0x5a);
        std::fs::write(dir.path().join("bmac-rpool-s1.img"), &image).unwrap();
        std::fs::write(dir.path().join(".partial"), b"x").unwrap();
        let listing = list_headers(dir.path(), host).unwrap();
        assert_eq!(listing.headers.len(), 1);
        let digest = &listing.headers["bmac-rpool-s1.img"];
        assert_eq!(digest.size, image.len() as u64);
        let header = read_header(dir.path(), host, "bmac-rpool-s1.img").unwrap();
        assert_eq!(header.sha256, digest.sha256);
        let reply = super::super::source_socket::SourceReply::Ok {
            body: json(&header),
        };
        assert!(
            crate::wire::encode(&reply).unwrap().len() < super::super::source_socket::MAX_FRAME
        );
        for name in ["../etc/passwd", ".partial", "", "missing.img"] {
            assert_eq!(
                read_header(dir.path(), host, name).unwrap_err().code,
                ErrorCode::NotFound,
                "{name}"
            );
        }
        assert!(
            list_headers(&dir.path().join("none"), host)
                .unwrap()
                .headers
                .is_empty()
        );
    }

    #[tokio::test]
    async fn delegation_allows_send_hold_and_release_and_nothing_else() {
        let runner = bmac_exec::FakeRunner::new();
        let done = delegate(&runner, "zfspool: local-zfs\n\tpool rpool/data\n", "mox1").await;
        assert_eq!(done.len(), 1);
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].args,
            [
                "allow",
                "-u",
                "bmac-source",
                "send,hold,release",
                "rpool/data"
            ]
        );
    }
}
