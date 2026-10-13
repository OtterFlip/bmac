// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Join bundles (`DEPLOYMENT.md`, "Additional hosts"). An administrator
//! issues one for a free slot: the slot is reserved in the registry, a
//! setup SSH key is made whose private half stays in
//! `/etc/pve/priv/bmac/joins/`, and a single-use join token is issued
//! ([`crate::mesh::tokens`]). The bundle names the current hosts with their
//! Dashboard fingerprints and the Proxmox cluster CA fingerprint, so the
//! Deployer and the new host can check whom they talk to. The new host
//! presents the token on port 8444; approving it is the `hosts.join` job.
//!
//! The cluster's identity (its UUID and the Tailscale auth key digests
//! already used) is kept in `/etc/pve/priv/bmac/cluster.json`, written by
//! the first host's setup and added to by each join.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_core::sshkey::SshHostKey;
use bmac_exec::{Command, CommandRunner};
use bmac_pmxcfs::{LockOptions, Pmxcfs, PmxcfsError, RegistryLock, read_record, write_record};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::bootstrap::{BundleHost, JOIN_BUNDLE_VERSION, JoinBundle, SettingValues};
use bmac_proto::joins::{
    DEFAULT_TTL_HOURS, IssueJoin, IssuedJoin, JoinList, JoinRequestView, JoinView, MAX_TTL_HOURS,
};
use serde::{Deserialize, Serialize};

use crate::auth::Clock;
use crate::identity::Identities;
use crate::mesh::tokens::{TokenKind, Tokens};
use crate::settings::Settings;

pub const JOIN_DIR: &str = "/etc/pve/priv/bmac/joins";
pub const CLUSTER_RECORD: &str = "/etc/pve/priv/bmac/cluster.json";
pub const CLUSTER_CA: &str = "/etc/pve/pve-root-ca.pem";
const REGISTRY: &str = "/usr/lib/bmac/legacy/scripts/host_runtime/cluster_registry.py";
const PYTHON: &str = "/usr/bin/python3";
const LOCK_PREFIX: &str = "bmac-join-";
const RECORD_SCHEMA: u64 = 1;
const DASHBOARD_PORT: u16 = 8443;

fn storage(e: PmxcfsError) -> ApiError {
    match e {
        PmxcfsError::LockTimeout { .. } => {
            ApiError::new(ErrorCode::Unavailable, "the cluster is busy; try again")
        }
        other => ApiError::new(ErrorCode::Internal, format!("join records: {other}")),
    }
}

fn invalid(field: &str, message: impl Into<String>) -> ApiError {
    let message = message.into();
    let mut error = ApiError::new(ErrorCode::Invalid, format!("{field}: {message}"));
    error.field_errors.insert(field.into(), message);
    error
}

/// The cluster's identity, beside its settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterRecord {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub cluster_uuid: String,
    pub used_tailscale_key_sha256: Vec<String>,
}

/// One outstanding bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinRecord {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub host: HostSlot,
    /// The join token's SHA-256, under which [`Tokens`] keeps it.
    pub ticket_sha256: String,
    pub issued_by: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub setup_ssh_public_key: String,
}

/// Reads and writes the cluster record and the join records.
#[derive(Clone)]
pub struct JoinStore {
    pmxcfs: Pmxcfs,
    dir: PathBuf,
    cluster: PathBuf,
}

impl JoinStore {
    pub fn new(pmxcfs: Pmxcfs, dir: impl Into<PathBuf>, cluster: impl Into<PathBuf>) -> JoinStore {
        JoinStore {
            pmxcfs,
            dir: dir.into(),
            cluster: cluster.into(),
        }
    }

    pub fn system() -> JoinStore {
        JoinStore::new(Pmxcfs::system(), JOIN_DIR, CLUSTER_RECORD)
    }

    fn record_path(&self, host: HostSlot) -> PathBuf {
        self.dir.join(format!("{host}.json"))
    }

    /// Where the setup key's private half is kept until the host has joined.
    pub fn key_path(&self, host: HostSlot) -> PathBuf {
        self.dir.join(format!("{host}.key"))
    }

    fn locked<T>(
        &self,
        root: &Path,
        f: impl FnOnce(&RegistryLock) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        self.pmxcfs.ensure_directory(root, 0o700).map_err(storage)?;
        let options = LockOptions {
            timeout: Duration::from_secs(15),
            prefix: LOCK_PREFIX.to_string(),
            ..LockOptions::default()
        };
        let lock = RegistryLock::acquire(&self.pmxcfs, root, &options).map_err(storage)?;
        let result = f(&lock);
        let released = lock.release().map_err(storage);
        let value = result?;
        released?;
        Ok(value)
    }

    pub fn cluster(&self) -> Result<Option<ClusterRecord>, ApiError> {
        let Some(value) = read_record(&self.pmxcfs, &self.cluster, "cluster record", RECORD_SCHEMA)
            .map_err(storage)?
        else {
            return Ok(None);
        };
        serde_json::from_value(value)
            .map(Some)
            .map_err(|e| ApiError::new(ErrorCode::Internal, format!("cluster record: {e}")))
    }

    /// Records the cluster's UUID and adds `digests` to the used Tailscale
    /// keys. A different UUID than the one recorded is refused.
    pub fn record_cluster(&self, uuid: &str, digests: &[String]) -> Result<(), ApiError> {
        let root = self
            .cluster
            .parent()
            .expect("the cluster record has a directory")
            .to_path_buf();
        self.locked(&root, |lock| {
            let current = self.cluster()?;
            let mut record = match &current {
                Some(record) if record.cluster_uuid != uuid => {
                    return Err(ApiError::new(
                        ErrorCode::Conflict,
                        format!("this cluster's UUID is {}, not {uuid}", record.cluster_uuid),
                    ));
                }
                Some(record) => record.clone(),
                None => ClusterRecord {
                    schema_version: RECORD_SCHEMA,
                    record_type: "cluster".into(),
                    revision: 0,
                    cluster_uuid: uuid.into(),
                    used_tailscale_key_sha256: Vec::new(),
                },
            };
            let before = record.used_tailscale_key_sha256.len();
            for digest in digests {
                if !record.used_tailscale_key_sha256.contains(digest) {
                    record.used_tailscale_key_sha256.push(digest.clone());
                }
            }
            if current.is_some() && record.used_tailscale_key_sha256.len() == before {
                return Ok(());
            }
            let expected = current.map(|c| c.revision);
            let value = serde_json::to_value(&record).expect("cluster records serialize");
            write_record(lock, &self.cluster, value, expected).map_err(storage)?;
            Ok(())
        })
    }

    pub fn join(&self, host: HostSlot) -> Result<Option<JoinRecord>, ApiError> {
        let Some(value) = read_record(
            &self.pmxcfs,
            &self.record_path(host),
            "join record",
            RECORD_SCHEMA,
        )
        .map_err(storage)?
        else {
            return Ok(None);
        };
        let record: JoinRecord = serde_json::from_value(value)
            .map_err(|e| ApiError::new(ErrorCode::Internal, format!("join record: {e}")))?;
        Ok((record.host == host).then_some(record))
    }

    pub fn joins(&self) -> Result<Vec<JoinRecord>, ApiError> {
        let mut out = Vec::new();
        for host in HostSlot::all() {
            if let Some(record) = self.join(host)? {
                out.push(record);
            }
        }
        Ok(out)
    }

    fn save_join(&self, record: &JoinRecord, private_key: &str) -> Result<(), ApiError> {
        self.locked(&self.dir, |lock| {
            let path = self.record_path(record.host);
            let current = self.join(record.host)?.map(|r| r.revision);
            self.pmxcfs
                .write_secret(&self.key_path(record.host), private_key.as_bytes())
                .map_err(storage)?;
            let value = serde_json::to_value(record).expect("join records serialize");
            write_record(lock, &path, value, current).map_err(storage)?;
            Ok(())
        })
    }

    /// Forgets `host`'s bundle and its setup key.
    pub fn remove_join(&self, host: HostSlot) -> Result<Option<JoinRecord>, ApiError> {
        self.locked(&self.dir, |_| {
            let record = self.join(host)?;
            self.pmxcfs.remove(&self.key_path(host)).map_err(storage)?;
            self.pmxcfs
                .remove(&self.record_path(host))
                .map_err(storage)?;
            Ok(record)
        })
    }
}

/// The colon-separated SHA-256 of the first certificate in `pem`.
pub fn ca_fingerprint(pem: &[u8]) -> Result<String, String> {
    crate::web::tls::pem_fingerprint(pem).map_err(|e| e.to_string())
}

/// Issues, lists, and revokes bundles on this host.
#[derive(Clone)]
pub struct Joins {
    pub store: JoinStore,
    pub tokens: Arc<Tokens>,
    pub settings: Settings,
    pub identities: Identities,
    pub runner: Arc<dyn CommandRunner>,
    pub clock: Clock,
    pub ca: PathBuf,
}

impl std::fmt::Debug for Joins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Joins")
    }
}

fn check_issue(request: &IssueJoin, taken: &[HostSlot]) -> Result<u32, ApiError> {
    if taken.contains(&request.host) {
        return Err(invalid(
            "host",
            format!("{} is already a host of this cluster", request.host),
        ));
    }
    if request.max_prod_vms == 0 {
        return Err(invalid("max_prod_vms", "must be at least 1"));
    }
    if request.max_staging_vms == 0 {
        return Err(invalid("max_staging_vms", "must be at least 1"));
    }
    if let Some(ip) = &request.idrac_ip
        && ip.parse::<std::net::Ipv4Addr>().is_err()
    {
        return Err(invalid("idrac_ip", "is not valid IPv4"));
    }
    let ttl = request.ttl_hours.unwrap_or(DEFAULT_TTL_HOURS);
    if ttl == 0 || ttl > MAX_TTL_HOURS {
        return Err(invalid(
            "ttl_hours",
            format!("must be between 1 and {MAX_TTL_HOURS}"),
        ));
    }
    Ok(ttl)
}

fn bundle_host_settings(request: &IssueJoin) -> SettingValues {
    let mut values = SettingValues::from([
        (
            "MAX_PROD_VM_COUNT_ON_THIS_HOST".to_string(),
            request.max_prod_vms.to_string(),
        ),
        (
            "MAX_STAGING_VM_COUNT_ON_THIS_HOST".to_string(),
            request.max_staging_vms.to_string(),
        ),
    ]);
    if let Some(ip) = &request.idrac_ip {
        values.insert("IDRAC_IP".into(), ip.clone());
    }
    values
}

impl Joins {
    async fn registry(&self, args: &[&str]) -> Result<(), ApiError> {
        let command = Command::new(PYTHON)
            .arg(REGISTRY)
            .args(args.iter().copied());
        self.runner
            .run(command)
            .await
            .map(|_| ())
            .map_err(|e| ApiError::new(ErrorCode::Internal, format!("the registry refused: {e}")))
    }

    async fn magic_dns_suffix(&self) -> Result<String, ApiError> {
        let status = bmac_adapters::tailscale::Tailscale::new(self.runner.as_ref())
            .status()
            .await
            .map_err(|e| ApiError::new(ErrorCode::Unavailable, format!("tailscale: {e}")))?;
        status
            .current_tailnet
            .map(|t| t.magic_dns_suffix)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ApiError::new(
                    ErrorCode::Unavailable,
                    "this host's tailnet has no MagicDNS name",
                )
            })
    }

    pub async fn issue(&self, request: IssueJoin, by: &str) -> Result<IssuedJoin, ApiError> {
        let hosts = self.settings.hosts()?;
        let taken: Vec<HostSlot> = hosts.iter().map(|h| h.host).collect();
        let ttl_hours = check_issue(&request, &taken)?;
        let cluster = self.settings.cluster()?.ok_or_else(|| {
            ApiError::new(ErrorCode::Unavailable, "this cluster has no settings yet")
        })?;
        let identity = self.store.cluster()?.ok_or_else(|| {
            ApiError::new(
                ErrorCode::Unavailable,
                "this cluster has no identity record yet; finish the first host's setup",
            )
        })?;
        let cluster_name = cluster
            .values
            .get("PROXMOX_CLUSTER_NAME")
            .cloned()
            .unwrap_or_default();
        let ca_pem = std::fs::read(&self.ca).map_err(|e| {
            ApiError::new(
                ErrorCode::Unavailable,
                format!("cannot read {}: {e}", self.ca.display()),
            )
        })?;
        let ca = ca_fingerprint(&ca_pem)
            .map_err(|e| ApiError::new(ErrorCode::Internal, format!("cluster CA: {e}")))?;
        let suffix = self.magic_dns_suffix().await?;
        let identities = self.identities.all()?;
        let mut bundle_hosts = Vec::new();
        for facts in &hosts {
            let Some(identity) = identities.iter().find(|i| i.host == facts.host) else {
                continue;
            };
            let Some(fingerprint) = identity.https_fingerprint.clone() else {
                continue;
            };
            let name = identity
                .tailnet_name
                .clone()
                .unwrap_or_else(|| format!("{}.{suffix}", facts.host));
            let private_ip =
                bmac_settings::host_private_ip(facts.host, &facts.values, &cluster.values)?;
            bundle_hosts.push(BundleHost {
                host: facts.host,
                dashboard_url: format!("https://{name}:{DASHBOARD_PORT}"),
                https_fingerprint: fingerprint,
                private_ip,
            });
        }
        if bundle_hosts.is_empty() {
            return Err(ApiError::new(
                ErrorCode::Unavailable,
                "no host has published its Dashboard fingerprint yet",
            ));
        }

        let host = request.host;
        self.registry(&["host-reserve", &host.to_string()]).await?;
        let key = SshHostKey::generate(&format!("bmac-setup-{host}"))
            .map_err(|e| ApiError::new(ErrorCode::Internal, e))?;
        let now = (self.clock)();
        let ttl_ms = i64::from(ttl_hours) * 3_600_000;
        if let Some(old) = self.store.join(host)? {
            self.tokens.revoke(&old.ticket_sha256)?;
        }
        let token = self.tokens.issue(TokenKind::Join, Some(host), ttl_ms, by)?;
        let record = JoinRecord {
            schema_version: RECORD_SCHEMA,
            record_type: "join_bundle".into(),
            revision: 0,
            host,
            ticket_sha256: crate::mesh::tokens::token_hash(&token)
                .expect("issued tokens are well formed"),
            issued_by: by.into(),
            issued_at_ms: now,
            expires_at_ms: now + ttl_ms,
            setup_ssh_public_key: key.public.clone(),
        };
        self.store.save_join(&record, &key.private)?;
        let bundle = JoinBundle {
            version: JOIN_BUNDLE_VERSION,
            cluster_uuid: identity.cluster_uuid,
            cluster_name: cluster_name.clone(),
            cluster_settings: cluster.values,
            slot: host,
            host_settings: bundle_host_settings(&request),
            join_token: token,
            expires_ms: record.expires_at_ms,
            hosts: bundle_hosts,
            proxmox_ca_fingerprint: ca,
            setup_ssh_public_key: key.public,
            used_tailscale_key_sha256: identity.used_tailscale_key_sha256,
        };
        let content = serde_json::to_string_pretty(&bundle).expect("bundles serialize");
        Ok(IssuedJoin {
            file_name: format!("{cluster_name}-{host}-join.json"),
            content,
        })
    }

    pub fn list(&self, can_manage: bool) -> Result<JoinList, ApiError> {
        let pending = self.tokens.pending()?;
        let mut joins = Vec::new();
        for record in self.store.joins()? {
            let used_at_ms = self
                .tokens
                .record(&record.ticket_sha256)?
                .and_then(|t| t.used_at_ms);
            let request = pending
                .iter()
                .rev()
                .find(|p| p.kind == TokenKind::Join && p.host == Some(record.host))
                .map(|p| JoinRequestView {
                    request_id: p.id.clone(),
                    address: p.address.clone(),
                    ssh_host_key_fingerprint: p
                        .ssh_host_key
                        .as_deref()
                        .and_then(crate::identity::ssh_host_key)
                        .map(|(_, fingerprint)| fingerprint),
                    https_fingerprint: p.https_fingerprint.clone(),
                    received_by: p.received_by,
                    created_at_ms: p.created_at_ms,
                });
            joins.push(JoinView {
                host: record.host,
                issued_by: record.issued_by,
                issued_at_ms: record.issued_at_ms,
                expires_at_ms: record.expires_at_ms,
                used_at_ms,
                request,
            });
        }
        Ok(JoinList { joins, can_manage })
    }

    /// Revokes `host`'s bundle: its token, its pending request, its setup
    /// key, and the slot's reservation.
    pub async fn revoke(&self, host: HostSlot) -> Result<(), ApiError> {
        let Some(record) = self.store.join(host)? else {
            return Err(ApiError::new(
                ErrorCode::NotFound,
                format!("{host} has no outstanding join bundle"),
            ));
        };
        self.tokens.revoke(&record.ticket_sha256)?;
        self.tokens.remove_pending_for(host)?;
        self.store.remove_join(host)?;
        self.registry(&[
            "host-release",
            &host.to_string(),
            "--expected-state",
            "joining",
            "--reason",
            "join bundle revoked",
        ])
        .await
    }
}

#[cfg(test)]
#[path = "joins_tests.rs"]
mod tests;
