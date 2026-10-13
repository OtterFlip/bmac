// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Join and enrollment tokens (`SECURITY.md`, "Joins and enrollment").
//!
//! A token is 256 random bits given to the operator once; pmxcfs keeps only
//! its SHA-256, as the record's file name, with its kind, expiry, and
//! whether it was used. Redeeming one marks it used under a lock and records
//! a pending request, which is all a valid token ever gets: nothing is
//! trusted until an administrator approves it. Every failure gets the same
//! answer, and each source address is rate-limited.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_pmxcfs::{LockOptions, Pmxcfs, PmxcfsError, RegistryLock, read_record, write_record};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::peer::{EnrollRequest, JoinRequest, Pending};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::Clock;
use crate::auth::limits::{Limiter, Window};
use crate::log;

pub const ENROLLMENT_DIR: &str = "/etc/pve/priv/bmac/enrollment";
const LOCK_PREFIX: &str = "bmac-enroll-";
pub const RECORD_SCHEMA: u64 = 1;
/// Redemption attempts per source address.
pub const ATTEMPTS: Window = Window {
    limit: 10,
    period_ms: 10 * 60_000,
};
const MAX_SSH_KEY: usize = 1024;
const MAX_CERTIFICATE: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Join,
    Enroll,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRecord {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub kind: TokenKind,
    /// For a join, the slot the new host must claim.
    pub host: Option<HostSlot>,
    pub issued_by: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub used_at_ms: Option<i64>,
    pub used_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRecord {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub id: String,
    pub kind: TokenKind,
    pub host: Option<HostSlot>,
    pub name: Option<String>,
    pub ssh_host_key: Option<String>,
    pub https_fingerprint: Option<String>,
    pub certificate: Option<String>,
    /// Where the request came from.
    pub address: String,
    pub received_by: HostSlot,
    pub created_at_ms: i64,
}

fn refused() -> ApiError {
    ApiError::new(
        ErrorCode::Unauthenticated,
        "the token is not valid; ask an administrator for a new one",
    )
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}

fn storage(e: PmxcfsError) -> ApiError {
    match e {
        PmxcfsError::LockTimeout { .. } => {
            ApiError::new(ErrorCode::Unavailable, "the cluster is busy; try again")
        }
        other => ApiError::new(ErrorCode::Internal, format!("enrollment records: {other}")),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-256 a well-formed token is stored under.
pub fn token_hash(token: &str) -> Option<String> {
    (token.len() == 64
        && token
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    .then(|| hex(&Sha256::digest(token.as_bytes())))
}

fn is_fingerprint(text: &str) -> bool {
    let parts: Vec<&str> = text.split(':').collect();
    parts.len() == 32
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn is_appliance_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub struct Tokens {
    pmxcfs: Pmxcfs,
    dir: PathBuf,
    host: HostSlot,
    clock: Clock,
    lock_timeout: Duration,
    limiter: Mutex<Limiter>,
    random: SystemRandom,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl Tokens {
    pub fn new(pmxcfs: Pmxcfs, dir: impl Into<PathBuf>, host: HostSlot, clock: Clock) -> Tokens {
        Tokens {
            pmxcfs,
            dir: dir.into(),
            host,
            clock,
            lock_timeout: Duration::from_secs(15),
            limiter: Mutex::new(Limiter::new(ATTEMPTS)),
            random: SystemRandom::new(),
        }
    }

    pub fn system(host: HostSlot, clock: Clock) -> Tokens {
        Tokens::new(Pmxcfs::system(), ENROLLMENT_DIR, host, clock)
    }

    fn token_path(&self, hash: &str) -> PathBuf {
        self.dir.join("tokens").join(format!("{hash}.json"))
    }

    fn pending_path(&self, id: &str) -> PathBuf {
        self.dir.join("pending").join(format!("{id}.json"))
    }

    fn locked<T>(
        &self,
        f: impl FnOnce(&RegistryLock) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        for sub in ["tokens", "pending"] {
            self.pmxcfs
                .ensure_directory(&self.dir.join(sub), 0o700)
                .map_err(storage)?;
        }
        let options = LockOptions {
            timeout: self.lock_timeout,
            prefix: LOCK_PREFIX.to_string(),
            short_lived: true,
            ..LockOptions::default()
        };
        let lock = RegistryLock::acquire(&self.pmxcfs, &self.dir, &options).map_err(storage)?;
        let result = f(&lock);
        let released = lock.release().map_err(storage);
        let value = result?;
        released?;
        Ok(value)
    }

    fn random_hex(&self, bytes: usize) -> Result<String, ApiError> {
        let mut buf = vec![0u8; bytes];
        self.random
            .fill(&mut buf)
            .map_err(|_| ApiError::new(ErrorCode::Internal, "no randomness"))?;
        Ok(hex(&buf))
    }

    /// Issues a token and returns it; only its hash is stored.
    pub fn issue(
        &self,
        kind: TokenKind,
        host: Option<HostSlot>,
        ttl_ms: i64,
        by: &str,
    ) -> Result<String, ApiError> {
        let token = self.random_hex(32)?;
        let hash = token_hash(&token).expect("generated tokens are well formed");
        let now = (self.clock)();
        let record = TokenRecord {
            schema_version: RECORD_SCHEMA,
            record_type: "enrollment_token".into(),
            revision: 0,
            kind,
            host,
            issued_by: by.into(),
            issued_at_ms: now,
            expires_at_ms: now + ttl_ms,
            used_at_ms: None,
            used_from: None,
        };
        self.locked(|lock| {
            let value = serde_json::to_value(&record).expect("token records serialize");
            write_record(lock, &self.token_path(&hash), value, None).map_err(storage)?;
            Ok(())
        })?;
        Ok(token)
    }

    pub fn redeem_join(&self, request: &JoinRequest, from: IpAddr) -> Result<Pending, ApiError> {
        let key = request.ssh_host_key.trim();
        if key.len() > MAX_SSH_KEY || !key.starts_with("ssh-ed25519 ") {
            return Err(invalid("ssh_host_key must be an ssh-ed25519 public key"));
        }
        if !is_fingerprint(&request.https_fingerprint) {
            return Err(invalid("https_fingerprint must be a SHA-256 fingerprint"));
        }
        self.redeem(&request.token, TokenKind::Join, from, |pending, token| {
            if token.host.is_some_and(|h| h != request.host) {
                return Err("it was issued for another host slot");
            }
            pending.host = Some(request.host);
            pending.ssh_host_key = Some(key.to_string());
            pending.https_fingerprint = Some(request.https_fingerprint.to_ascii_uppercase());
            Ok(())
        })
    }

    pub fn redeem_enroll(
        &self,
        request: &EnrollRequest,
        from: IpAddr,
    ) -> Result<Pending, ApiError> {
        if !is_appliance_name(&request.name) {
            return Err(invalid("name must be a lowercase DNS label"));
        }
        if request.certificate.len() > MAX_CERTIFICATE
            || CertificateDer::from_pem_slice(request.certificate.as_bytes()).is_err()
        {
            return Err(invalid("certificate must be one PEM certificate"));
        }
        self.redeem(&request.token, TokenKind::Enroll, from, |pending, _| {
            pending.name = Some(request.name.clone());
            pending.certificate = Some(request.certificate.clone());
            Ok(())
        })
    }

    fn redeem(
        &self,
        token: &str,
        kind: TokenKind,
        from: IpAddr,
        fill: impl FnOnce(&mut PendingRecord, &TokenRecord) -> Result<(), &'static str>,
    ) -> Result<Pending, ApiError> {
        let now = (self.clock)();
        let address = from.to_string();
        {
            let mut limiter = self.limiter.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(limited) = limiter.check(&address, now) {
                if limited.first {
                    log::warning(&format!(
                        "enrollment: too many token attempts from {address}"
                    ));
                }
                let mut error = ApiError::new(
                    ErrorCode::RateLimited,
                    "too many attempts; wait and try again",
                );
                error.retry_after_seconds = Some(limited.retry_after_seconds);
                return Err(error);
            }
            limiter.record(&address, now);
        }
        let Some(hash) = token_hash(token) else {
            log::warning(&format!("enrollment: a malformed token from {address}"));
            return Err(refused());
        };
        let id = self.random_hex(16)?;
        self.locked(|lock| {
            let path = self.token_path(&hash);
            let Some(value) = read_record(&self.pmxcfs, &path, "enrollment token", RECORD_SCHEMA)
                .map_err(storage)?
            else {
                log::warning(&format!("enrollment: an unknown token from {address}"));
                return Err(refused());
            };
            let mut record: TokenRecord = serde_json::from_value(value).map_err(|e| {
                ApiError::new(ErrorCode::Internal, format!("enrollment token: {e}"))
            })?;
            let why = if record.kind != kind {
                Some("it is for another kind of request")
            } else if let Some(used) = record.used_at_ms {
                log::warning(&format!(
                    "enrollment: a token used at {used} from {} was presented again from {address}",
                    record.used_from.as_deref().unwrap_or("?")
                ));
                Some("it was already used")
            } else if now >= record.expires_at_ms {
                Some("it expired")
            } else {
                None
            };
            let mut pending = PendingRecord {
                schema_version: RECORD_SCHEMA,
                record_type: "pending_request".into(),
                revision: 0,
                id: id.clone(),
                kind,
                host: None,
                name: None,
                ssh_host_key: None,
                https_fingerprint: None,
                certificate: None,
                address: address.clone(),
                received_by: self.host,
                created_at_ms: now,
            };
            if let Some(why) = why.or_else(|| fill(&mut pending, &record).err()) {
                log::warning(&format!(
                    "enrollment: refused a token from {address}: {why}"
                ));
                return Err(refused());
            }
            let expected = record.revision;
            record.used_at_ms = Some(now);
            record.used_from = Some(address.clone());
            let value = serde_json::to_value(&record).expect("token records serialize");
            write_record(lock, &path, value, Some(expected)).map_err(storage)?;
            let value = serde_json::to_value(&pending).expect("pending records serialize");
            write_record(lock, &self.pending_path(&id), value, None).map_err(storage)?;
            log::info(&format!(
                "enrollment: a {} request from {address} is waiting for approval ({id})",
                match kind {
                    TokenKind::Join => "join",
                    TokenKind::Enroll => "enrollment",
                }
            ));
            Ok(Pending {
                request_id: id.clone(),
            })
        })
    }

    pub fn record(&self, hash: &str) -> Result<Option<TokenRecord>, ApiError> {
        let Some(value) = read_record(
            &self.pmxcfs,
            &self.token_path(hash),
            "enrollment token",
            RECORD_SCHEMA,
        )
        .map_err(storage)?
        else {
            return Ok(None);
        };
        serde_json::from_value(value)
            .map(Some)
            .map_err(|e| ApiError::new(ErrorCode::Internal, format!("enrollment token: {e}")))
    }

    /// Forgets a token, used or not. Missing is not an error.
    pub fn revoke(&self, hash: &str) -> Result<(), ApiError> {
        self.locked(|_| self.pmxcfs.remove(&self.token_path(hash)).map_err(storage))
    }

    /// Forgets the pending request `id`.
    pub fn remove_pending(&self, id: &str) -> Result<(), ApiError> {
        if !id.bytes().all(|b| b.is_ascii_hexdigit()) || id.is_empty() {
            return Err(invalid("not a request id"));
        }
        self.locked(|_| self.pmxcfs.remove(&self.pending_path(id)).map_err(storage))
    }

    /// Forgets every pending join request for `host`.
    pub fn remove_pending_for(&self, host: HostSlot) -> Result<(), ApiError> {
        for pending in self.pending()? {
            if pending.kind == TokenKind::Join && pending.host == Some(host) {
                self.remove_pending(&pending.id)?;
            }
        }
        Ok(())
    }

    /// The requests waiting for an administrator, oldest first.
    pub fn pending(&self) -> Result<Vec<PendingRecord>, ApiError> {
        let dir = self.dir.join("pending");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if let Some(value) = read_record(&self.pmxcfs, &path, "pending request", RECORD_SCHEMA)
                .map_err(storage)?
                && let Ok(record) = serde_json::from_value::<PendingRecord>(value)
            {
                out.push(record);
            }
        }
        out.sort_by_key(|r| r.created_at_ms);
        Ok(out)
    }
}
