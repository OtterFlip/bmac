// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Job records in `/etc/pve/priv/bmac/jobs/` (`ARCHITECTURE.md`, "Jobs").
//!
//! Any host admits a job under the jobs lock: quorum, role, parameters,
//! idempotency key, target host, and resource conflicts, then a record in
//! state `admitted`. The target host claims it with a lease (a 32-hex nonce,
//! renewed only by the same nonce, never taken over), as v2's orchestration
//! leases work. A lease that expires leaves the job `interrupted`, still
//! holding its resources: nothing re-runs it automatically, and an operator
//! either resumes it (only operations that continue from a checkpoint) or
//! marks it failed after checking the live state. Unfinished jobs and the
//! newest finished ones are kept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bmac_core::job::JobState;
use bmac_core::{GuestName, HostSlot, JobId, OperationId};
use bmac_pmxcfs::{LockOptions, Pmxcfs, PmxcfsError, RegistryLock, read_record, write_record};
use bmac_proto::api::{ApiError, ErrorCode, JobSummary, LaunchJob, Role};
use bmac_proto::operations::{Operation, ResumeRule, TargetRule};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::auth::Clock;
use crate::cluster::{ClusterView, Membership};
use crate::operations::{Registry, locks_conflict, prepare};

pub const JOBS_DIR: &str = "/etc/pve/priv/bmac/jobs";
/// The jobs lock's name in `/etc/pve/priv/lock/`.
pub const LOCK_PREFIX: &str = "bmac-jobs-";
pub const RECORD_SCHEMA: u64 = 1;
const RECORD_TYPE: &str = "job";
pub const KEEP_FINISHED: usize = 500;
pub const LEASE_TTL_MS: i64 = 60_000;
/// How often the holder renews its lease.
pub const LEASE_RENEW_MS: i64 = 15_000;
/// An admitted job its target never claims fails after this; it never ran.
pub const CLAIM_TIMEOUT_MS: i64 = 5 * 60_000;
pub const MAX_IDEMPOTENCY_KEY: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobLease {
    pub nonce: String,
    pub holder: HostSlot,
    pub acquired_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelOrder {
    pub forced: bool,
    pub by: String,
    pub requested_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRecord {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub id: JobId,
    pub operation: OperationId,
    pub title: String,
    pub parameters: BTreeMap<String, String>,
    /// The script's arguments, fixed at admission.
    pub args: Vec<String>,
    pub locks: Vec<String>,
    pub dry_run: bool,
    pub user: String,
    pub role: Role,
    pub idempotency_key: String,
    pub admitted_by: HostSlot,
    pub target: HostSlot,
    pub state: JobState,
    pub message: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub admitted_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    /// The current or last lease; kept after an interruption so the holder
    /// can still report how the job ended.
    pub lease: Option<JobLease>,
    pub cancel: Option<CancelOrder>,
    /// Whether a soft cancel may interrupt the current phase.
    pub cancel_allowed: bool,
    pub phase: Option<String>,
    pub pending_request: Option<String>,
    /// How many times a host has claimed it.
    pub attempts: u32,
    /// The sequence number of its newest event, so a resumed run continues
    /// the numbering.
    #[serde(default)]
    pub last_seq: u64,
}

impl JobRecord {
    /// Whether it still holds its resources.
    pub fn holds_locks(&self) -> bool {
        !self.state.is_terminal()
    }

    pub fn summary(&self) -> JobSummary {
        JobSummary {
            id: self.id,
            operation: self.operation.clone(),
            title: self.title.clone(),
            state: self.state,
            host: self.target,
            user: self.user.clone(),
            dry_run: self.dry_run,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            pending_request: self.pending_request.clone(),
            message: self.message.clone(),
            cancel_requested: self.cancel.is_some() && !self.state.is_terminal(),
            last_seq: self.last_seq,
        }
    }

    fn finish(&mut self, state: JobState, message: String, now: i64) {
        self.state = state;
        self.message = Some(message);
        self.finished_at_ms = Some(now);
        self.updated_at_ms = now;
        self.pending_request = None;
    }
}

/// The logged-in user acting on a job.
#[derive(Debug, Clone, Copy)]
pub struct Caller<'a> {
    pub user: &'a str,
    pub role: Role,
}

/// The job, and whether an earlier launch with the same idempotency key
/// already admitted it.
#[derive(Debug, Clone)]
pub struct Admission {
    pub job: JobRecord,
    pub existing: bool,
}

/// A claimed job and the nonce that renews and finishes it.
#[derive(Debug, Clone)]
pub struct Claim {
    pub job: JobRecord,
    pub nonce: String,
}

/// What the running host reports about its job between events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub phase: Option<String>,
    pub cancel_allowed: bool,
    pub pending_request: Option<String>,
    pub last_seq: u64,
}

#[derive(Clone)]
pub struct JobsConfig {
    pub pmxcfs: Pmxcfs,
    pub dir: PathBuf,
    /// This host.
    pub host: HostSlot,
    pub lease_ttl_ms: i64,
    pub claim_timeout_ms: i64,
    pub keep_finished: usize,
    pub lock_timeout: Duration,
    pub clock: Clock,
}

impl JobsConfig {
    pub fn system(host: HostSlot, clock: Clock) -> JobsConfig {
        JobsConfig {
            pmxcfs: Pmxcfs::system(),
            dir: PathBuf::from(JOBS_DIR),
            host,
            lease_ttl_ms: LEASE_TTL_MS,
            claim_timeout_ms: CLAIM_TIMEOUT_MS,
            keep_finished: KEEP_FINISHED,
            lock_timeout: Duration::from_secs(15),
            clock,
        }
    }
}

fn error(code: ErrorCode, message: impl Into<String>) -> ApiError {
    ApiError::new(code, message)
}

fn storage(e: PmxcfsError) -> ApiError {
    match e {
        PmxcfsError::LockTimeout { .. } => error(
            ErrorCode::Unavailable,
            "the job records are busy; try again",
        ),
        other => error(ErrorCode::Internal, format!("job records: {other}")),
    }
}

fn valid_idempotency_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_IDEMPOTENCY_KEY
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// All records, held under the lock.
struct Locked<'a> {
    lock: &'a RegistryLock,
    jobs: &'a Jobs,
    records: Vec<JobRecord>,
}

impl Locked<'_> {
    fn find(&self, id: JobId) -> Result<JobRecord, ApiError> {
        self.records
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| error(ErrorCode::NotFound, format!("no job {id}")))
    }

    fn save(&mut self, record: &mut JobRecord) -> Result<(), ApiError> {
        let expected = (record.revision > 0).then_some(record.revision);
        let value = serde_json::to_value(&*record).expect("job records serialize");
        let path = self.jobs.path(record.id);
        let written = write_record(self.lock, &path, value, expected).map_err(storage)?;
        record.revision = written.revision;
        match self.records.iter_mut().find(|r| r.id == record.id) {
            Some(slot) => *slot = record.clone(),
            None => self.records.push(record.clone()),
        }
        Ok(())
    }

    /// Removes the oldest finished records beyond the limit.
    fn prune(&mut self) -> Result<(), ApiError> {
        let mut finished: Vec<(i64, JobId)> = self
            .records
            .iter()
            .filter(|r| r.state.is_terminal())
            .map(|r| (r.finished_at_ms.unwrap_or(r.updated_at_ms), r.id))
            .collect();
        if finished.len() <= self.jobs.config.keep_finished {
            return Ok(());
        }
        finished.sort();
        let excess = finished.len() - self.jobs.config.keep_finished;
        for (_, id) in finished.into_iter().take(excess) {
            self.jobs
                .config
                .pmxcfs
                .remove(&self.jobs.path(id))
                .map_err(storage)?;
            self.records.retain(|r| r.id != id);
        }
        Ok(())
    }
}

impl std::fmt::Debug for Jobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jobs")
            .field("dir", &self.config.dir)
            .field("host", &self.config.host)
            .finish_non_exhaustive()
    }
}

pub struct Jobs {
    config: JobsConfig,
    registry: Arc<Registry>,
    cluster: Arc<dyn ClusterView>,
    random: SystemRandom,
}

impl Jobs {
    pub fn new(config: JobsConfig, registry: Arc<Registry>, cluster: Arc<dyn ClusterView>) -> Jobs {
        Jobs {
            config,
            registry,
            cluster,
            random: SystemRandom::new(),
        }
    }

    pub fn host(&self) -> HostSlot {
        self.config.host
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    fn now(&self) -> i64 {
        (self.config.clock)()
    }

    fn path(&self, id: JobId) -> PathBuf {
        self.config.dir.join(format!("{id}.json"))
    }

    fn read_all(&self) -> Result<Vec<JobRecord>, ApiError> {
        let entries = match std::fs::read_dir(&self.config.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(error(
                    ErrorCode::Internal,
                    format!("cannot list {}: {e}", self.config.dir.display()),
                ));
            }
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| error(ErrorCode::Internal, e.to_string()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let record = self.read_one(&entry.path())?;
            if record.id.to_string() != stem {
                return Err(error(
                    ErrorCode::Internal,
                    format!("{name} holds job {}", record.id),
                ));
            }
            records.push(record);
        }
        Ok(records)
    }

    fn read_one(&self, path: &Path) -> Result<JobRecord, ApiError> {
        let value = read_record(&self.config.pmxcfs, path, "job record", RECORD_SCHEMA)
            .map_err(storage)?
            .ok_or_else(|| error(ErrorCode::NotFound, "no such job"))?;
        let record: JobRecord = serde_json::from_value(value).map_err(|e| {
            error(
                ErrorCode::Internal,
                format!("job record {}: {e}", path.display()),
            )
        })?;
        if record.record_type != RECORD_TYPE {
            return Err(error(
                ErrorCode::Internal,
                format!("{} is not a job record", path.display()),
            ));
        }
        Ok(record)
    }

    fn locked<T>(&self, f: impl FnOnce(&mut Locked) -> Result<T, ApiError>) -> Result<T, ApiError> {
        let options = LockOptions {
            timeout: self.config.lock_timeout,
            prefix: LOCK_PREFIX.to_string(),
            short_lived: true,
            ..LockOptions::default()
        };
        let lock = RegistryLock::acquire(&self.config.pmxcfs, &self.config.dir, &options)
            .map_err(storage)?;
        let result = (|| {
            let records = self.read_all()?;
            let mut locked = Locked {
                lock: &lock,
                jobs: self,
                records,
            };
            f(&mut locked)
        })();
        let released = lock.release().map_err(storage);
        let value = result?;
        released?;
        Ok(value)
    }

    fn operation(&self, id: &OperationId) -> Result<&Operation, ApiError> {
        self.registry
            .get(id.as_str())
            .ok_or_else(|| error(ErrorCode::NotFound, format!("no operation {id}")))
    }

    fn membership(&self) -> Result<Membership, ApiError> {
        self.cluster
            .membership()
            .map_err(|e| error(ErrorCode::Unavailable, e))
    }

    fn quorate(&self) -> Result<Membership, ApiError> {
        let membership = self.membership()?;
        if !membership.quorate {
            return Err(error(
                ErrorCode::NoQuorum,
                "this host's part of the cluster has no quorum, so nothing can change",
            ));
        }
        Ok(membership)
    }

    /// Every job, newest first.
    pub fn list(&self) -> Result<Vec<JobRecord>, ApiError> {
        let mut records = self.read_all()?;
        records.sort_by_key(|r| std::cmp::Reverse((r.created_at_ms, r.id)));
        Ok(records)
    }

    pub fn get(&self, id: JobId) -> Result<JobRecord, ApiError> {
        let path = self.path(id);
        if !self.config.pmxcfs.exists(&path) {
            return Err(error(ErrorCode::NotFound, format!("no job {id}")));
        }
        self.read_one(&path)
    }

    pub fn admit(&self, caller: Caller, launch: &LaunchJob) -> Result<Admission, ApiError> {
        if !valid_idempotency_key(&launch.idempotency_key) {
            return Err(error(
                ErrorCode::Invalid,
                "the idempotency key must be 1 to 128 letters, digits, or - _ . :",
            ));
        }
        let operation = self.operation(&launch.operation)?;
        if caller.role < operation.min_role {
            return Err(error(
                ErrorCode::Forbidden,
                format!(
                    "{} needs the {:?} role",
                    operation.title, operation.min_role
                ),
            ));
        }
        let prepared = prepare(operation, &launch.parameters, launch.dry_run)?;
        self.locked(|locked| {
            if let Some(existing) = locked
                .records
                .iter()
                .find(|r| r.user == caller.user && r.idempotency_key == launch.idempotency_key)
            {
                let same = existing.operation == launch.operation
                    && existing.parameters == prepared.summary
                    && existing.dry_run == prepared.dry_run;
                if !same {
                    return Err(error(
                        ErrorCode::Conflict,
                        "this idempotency key was already used for a different launch",
                    ));
                }
                return Ok(Admission {
                    job: existing.clone(),
                    existing: true,
                });
            }
            let membership = self.quorate()?;
            let target = self.resolve_target(operation, &prepared.summary, &membership)?;
            check_conflicts(&locked.records, &prepared.locks)?;
            let now = self.now();
            let mut record = JobRecord {
                schema_version: RECORD_SCHEMA,
                record_type: RECORD_TYPE.into(),
                revision: 0,
                id: JobId::random(),
                operation: launch.operation.clone(),
                title: operation.title.clone(),
                parameters: prepared.summary.clone(),
                args: prepared.args.clone(),
                locks: prepared.locks.clone(),
                dry_run: prepared.dry_run,
                user: caller.user.into(),
                role: caller.role,
                idempotency_key: launch.idempotency_key.clone(),
                admitted_by: self.config.host,
                target,
                state: JobState::Admitted,
                message: None,
                created_at_ms: now,
                updated_at_ms: now,
                admitted_at_ms: now,
                finished_at_ms: None,
                lease: None,
                cancel: None,
                cancel_allowed: true,
                phase: None,
                pending_request: None,
                attempts: 0,
                last_seq: 0,
            };
            locked.save(&mut record)?;
            locked.prune()?;
            Ok(Admission {
                job: record,
                existing: false,
            })
        })
    }

    fn resolve_target(
        &self,
        operation: &Operation,
        parameters: &BTreeMap<String, String>,
        membership: &Membership,
    ) -> Result<HostSlot, ApiError> {
        let param = |id: &str| -> Result<&str, ApiError> {
            parameters
                .get(id)
                .map(String::as_str)
                .ok_or_else(|| error(ErrorCode::Invalid, format!("{id} is required")))
        };
        let guest = |id: &str| -> Result<GuestName, ApiError> {
            param(id)?
                .parse()
                .map_err(|_| error(ErrorCode::Invalid, format!("{id} is not a guest")))
        };
        let online = |host: HostSlot| -> Result<HostSlot, ApiError> {
            if membership.online.contains(&host) {
                Ok(host)
            } else {
                Err(error(
                    ErrorCode::Unavailable,
                    format!("{host} is offline, and {} runs there", operation.title),
                ))
            }
        };
        let cluster = |e: String| error(ErrorCode::Unavailable, e);
        let me = self.config.host;
        match &operation.target {
            TargetRule::Any {} | TargetRule::Placement {} => Ok(me),
            TargetRule::Duty {} => membership
                .duty()
                .ok_or_else(|| error(ErrorCode::Unavailable, "no host is online")),
            TargetRule::Host { param: id } => {
                let host: HostSlot = param(id)?
                    .parse()
                    .map_err(|_| error(ErrorCode::Invalid, format!("{id} is not a host")))?;
                online(host)
            }
            TargetRule::AnyExcept { param: id } => {
                let excluded = param(id)?;
                if me.to_string() != excluded {
                    return Ok(me);
                }
                membership
                    .online
                    .iter()
                    .copied()
                    .find(|h| h.to_string() != excluded)
                    .ok_or_else(|| {
                        error(
                            ErrorCode::Unavailable,
                            format!("no host other than {excluded} is online"),
                        )
                    })
            }
            TargetRule::Owner { param: id } => {
                let name = guest(id)?;
                match self.cluster.guest_owner(name).map_err(cluster)? {
                    Some(owner) => online(owner),
                    None => Err(error(ErrorCode::NotFound, format!("no host has {name}"))),
                }
            }
            TargetRule::Standby { param: id } => {
                let name = guest(id)?;
                let owner = self.cluster.guest_owner(name).map_err(cluster)?;
                self.cluster
                    .placement(name)
                    .map_err(cluster)?
                    .into_iter()
                    .filter(|h| Some(*h) != owner && membership.online.contains(h))
                    .min()
                    .ok_or_else(|| {
                        error(
                            ErrorCode::Unavailable,
                            format!("none of {name}'s standby hosts is online"),
                        )
                    })
            }
        }
    }

    /// The target host takes an admitted job.
    pub fn claim(&self, id: JobId) -> Result<Claim, ApiError> {
        let mut bytes = [0u8; 16];
        self.random
            .fill(&mut bytes)
            .map_err(|_| error(ErrorCode::Internal, "no randomness"))?;
        let nonce: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.locked(|locked| {
            let mut record = locked.find(id)?;
            if record.state != JobState::Admitted {
                return Err(error(
                    ErrorCode::Conflict,
                    format!("job {id} is {:?}, not waiting to start", record.state),
                ));
            }
            if record.target != self.config.host {
                return Err(error(
                    ErrorCode::Conflict,
                    format!("job {id} runs on {}", record.target),
                ));
            }
            let now = self.now();
            record.state = JobState::Running;
            record.lease = Some(JobLease {
                nonce: nonce.clone(),
                holder: self.config.host,
                acquired_at_ms: now,
                expires_at_ms: now + self.config.lease_ttl_ms,
            });
            record.attempts += 1;
            record.cancel_allowed = true;
            record.phase = None;
            record.pending_request = None;
            record.message = None;
            record.updated_at_ms = now;
            locked.save(&mut record)?;
            Ok(Claim {
                job: record,
                nonce: nonce.clone(),
            })
        })
    }

    fn check_holder(&self, record: &JobRecord, nonce: &str) -> Result<(), ApiError> {
        match &record.lease {
            Some(lease) if lease.nonce == nonce && lease.holder == self.config.host => Ok(()),
            _ => Err(error(
                ErrorCode::Conflict,
                format!("this host no longer holds job {}", record.id),
            )),
        }
    }

    /// Extends the lease, with what the job is doing now. The returned record
    /// carries any cancel order.
    pub fn renew(
        &self,
        id: JobId,
        nonce: &str,
        progress: &Progress,
    ) -> Result<JobRecord, ApiError> {
        self.locked(|locked| {
            let mut record = locked.find(id)?;
            self.check_holder(&record, nonce)?;
            let now = self.now();
            let lease = record.lease.as_mut().expect("checked");
            if record.state != JobState::Running || now >= lease.expires_at_ms {
                return Err(error(
                    ErrorCode::Conflict,
                    format!("the lease on job {id} has expired"),
                ));
            }
            lease.expires_at_ms = now + self.config.lease_ttl_ms;
            record.phase = progress.phase.clone();
            record.cancel_allowed = progress.cancel_allowed;
            record.pending_request = progress.pending_request.clone();
            record.last_seq = record.last_seq.max(progress.last_seq);
            record.updated_at_ms = now;
            locked.save(&mut record)?;
            Ok(record)
        })
    }

    /// Records how the job ended. The holder may report it even after its
    /// lease expired and the job was marked interrupted.
    pub fn finish(
        &self,
        id: JobId,
        nonce: &str,
        state: JobState,
        message: impl Into<String>,
        last_seq: u64,
    ) -> Result<JobRecord, ApiError> {
        if !state.is_terminal() {
            return Err(error(
                ErrorCode::Invalid,
                "a job finishes completed, failed, or cancelled",
            ));
        }
        let message = message.into();
        self.locked(|locked| {
            let mut record = locked.find(id)?;
            self.check_holder(&record, nonce)?;
            if !matches!(record.state, JobState::Running | JobState::Interrupted) {
                return Err(error(
                    ErrorCode::Conflict,
                    format!("job {id} is already {:?}", record.state),
                ));
            }
            record.finish(state, message, self.now());
            record.last_seq = record.last_seq.max(last_seq);
            locked.save(&mut record)?;
            locked.prune()?;
            Ok(record)
        })
    }

    pub fn cancel(&self, caller: Caller, id: JobId, force: bool) -> Result<JobRecord, ApiError> {
        self.locked(|locked| {
            let mut record = locked.find(id)?;
            let operation = self.operation(&record.operation)?;
            if caller.role < operation.min_role {
                return Err(error(
                    ErrorCode::Forbidden,
                    format!("cancelling {} needs the {:?} role", operation.title, operation.min_role),
                ));
            }
            if force && caller.role < Role::Admin {
                return Err(error(
                    ErrorCode::Forbidden,
                    "a forced cancel needs the Admin role",
                ));
            }
            let now = self.now();
            match record.state {
                JobState::Admitted => {
                    record.finish(
                        JobState::Cancelled,
                        format!("cancelled by {} before it started", caller.user),
                        now,
                    );
                    record.cancel = Some(CancelOrder {
                        forced: force,
                        by: caller.user.into(),
                        requested_at_ms: now,
                    });
                }
                JobState::Running => {
                    if !force && !record.cancel_allowed && record.pending_request.is_none() {
                        let phase = record.phase.as_deref().unwrap_or("this phase");
                        return Err(error(
                            ErrorCode::Conflict,
                            format!("the job marked \"{phase}\" as unsafe to interrupt; wait for it to finish"),
                        ));
                    }
                    let previous = record.cancel.take();
                    record.cancel = Some(CancelOrder {
                        forced: force || previous.as_ref().is_some_and(|c| c.forced),
                        by: caller.user.into(),
                        requested_at_ms: previous.map_or(now, |c| c.requested_at_ms),
                    });
                    record.updated_at_ms = now;
                }
                JobState::Interrupted => {
                    return Err(error(
                        ErrorCode::Conflict,
                        "the job was interrupted; check the live state, then resume it or mark it failed",
                    ));
                }
                _ => {
                    return Err(error(
                        ErrorCode::Conflict,
                        format!("job {id} has already finished"),
                    ));
                }
            }
            locked.save(&mut record)?;
            locked.prune()?;
            Ok(record)
        })
    }

    fn interrupted(
        &self,
        locked: &Locked,
        caller: Caller,
        id: JobId,
    ) -> Result<JobRecord, ApiError> {
        let record = locked.find(id)?;
        let operation = self.operation(&record.operation)?;
        if caller.role < operation.min_role {
            return Err(error(
                ErrorCode::Forbidden,
                format!(
                    "{} needs the {:?} role",
                    operation.title, operation.min_role
                ),
            ));
        }
        if record.state != JobState::Interrupted {
            return Err(error(
                ErrorCode::Conflict,
                format!("job {id} is {:?}, not interrupted", record.state),
            ));
        }
        Ok(record)
    }

    /// Restarts an interrupted job from its checkpoint, on the host its
    /// target rule now picks. It kept its resources while interrupted.
    pub fn resume(&self, caller: Caller, id: JobId) -> Result<JobRecord, ApiError> {
        self.locked(|locked| {
            let mut record = self.interrupted(locked, caller, id)?;
            let operation = self.operation(&record.operation)?;
            if operation.resume != ResumeRule::Checkpoint {
                return Err(error(
                    ErrorCode::Invalid,
                    format!(
                        "{} can't continue after an interruption; check the live state, then mark it failed",
                        operation.title
                    ),
                ));
            }
            let membership = self.quorate()?;
            record.target = self.resolve_target(operation, &record.parameters, &membership)?;
            let now = self.now();
            record.state = JobState::Admitted;
            record.message = Some(format!("resumed by {}", caller.user));
            record.admitted_at_ms = now;
            record.updated_at_ms = now;
            locked.save(&mut record)?;
            Ok(record)
        })
    }

    /// For a starting `bmacd`: its jobs' processes stopped with the old one,
    /// so the jobs this host was running are interrupted now rather than
    /// when their leases run out.
    pub fn interrupt_own(&self) -> Result<Vec<JobRecord>, ApiError> {
        let host = self.config.host;
        let own = |r: &JobRecord| {
            r.state == JobState::Running && r.lease.as_ref().is_some_and(|l| l.holder == host)
        };
        if !self.read_all()?.iter().any(own) {
            return Ok(Vec::new());
        }
        self.locked(|locked| {
            let now = self.now();
            let mut changed = Vec::new();
            for mut record in locked.records.clone().into_iter().filter(own) {
                record.state = JobState::Interrupted;
                record.message = Some(format!(
                    "{host} restarted while it ran; inspect the live state before resuming or marking it failed"
                ));
                record.updated_at_ms = now;
                locked.save(&mut record)?;
                changed.push(record);
            }
            Ok(changed)
        })
    }

    /// Puts an interrupted checkpoint job this host was running back in
    /// line here, after the restart it asked for.
    pub fn resume_after_restart(&self, id: JobId) -> Result<JobRecord, ApiError> {
        let host = self.config.host;
        self.locked(|locked| {
            let mut record = locked.find(id)?;
            let operation = self.operation(&record.operation)?;
            let held = record.lease.as_ref().is_some_and(|l| l.holder == host);
            if record.state != JobState::Interrupted
                || !held
                || operation.resume != ResumeRule::Checkpoint
            {
                return Err(error(
                    ErrorCode::Conflict,
                    format!("job {id} isn't an interrupted checkpoint job of {host}"),
                ));
            }
            self.quorate()?;
            let now = self.now();
            record.state = JobState::Admitted;
            record.target = host;
            record.message = Some(format!("resumed after {host} restarted"));
            record.admitted_at_ms = now;
            record.updated_at_ms = now;
            locked.save(&mut record)?;
            Ok(record)
        })
    }

    /// Marks an interrupted job failed once someone has checked the live
    /// state, releasing its resources.
    pub fn reconcile(&self, caller: Caller, id: JobId) -> Result<JobRecord, ApiError> {
        self.locked(|locked| {
            let mut record = self.interrupted(locked, caller, id)?;
            record.finish(
                JobState::Failed,
                format!(
                    "marked failed by {} after checking the live state",
                    caller.user
                ),
                self.now(),
            );
            locked.save(&mut record)?;
            locked.prune()?;
            Ok(record)
        })
    }

    /// Interrupts running jobs whose lease expired and fails admitted ones
    /// their target never claimed. Every host runs this; the changed records
    /// are returned.
    pub fn sweep(&self) -> Result<Vec<JobRecord>, ApiError> {
        let now = self.now();
        let stale = |r: &JobRecord| match r.state {
            JobState::Running => r.lease.as_ref().is_some_and(|l| now >= l.expires_at_ms),
            JobState::Admitted => now >= r.admitted_at_ms + self.config.claim_timeout_ms,
            _ => false,
        };
        if !self.read_all()?.iter().any(stale) {
            return Ok(Vec::new());
        }
        self.locked(|locked| {
            let now = self.now();
            let mut changed = Vec::new();
            for mut record in locked.records.clone() {
                if !stale(&record) {
                    continue;
                }
                if record.state == JobState::Running {
                    let holder = record.lease.as_ref().map(|l| l.holder);
                    record.state = JobState::Interrupted;
                    record.message = Some(format!(
                        "{} stopped renewing its lease; inspect the live state before resuming or marking it failed",
                        holder.map_or("its host".to_string(), |h| h.to_string())
                    ));
                    record.updated_at_ms = now;
                } else {
                    record.finish(
                        JobState::Failed,
                        format!("{} never started it", record.target),
                        now,
                    );
                }
                locked.save(&mut record)?;
                changed.push(record);
            }
            locked.prune()?;
            Ok(changed)
        })
    }
}

/// Refuses locks that overlap an unfinished job's.
fn check_conflicts(records: &[JobRecord], wanted: &[String]) -> Result<(), ApiError> {
    for record in records.iter().filter(|r| r.holds_locks()) {
        for held in &record.locks {
            if let Some(lock) = wanted.iter().find(|w| locks_conflict(w, held)) {
                let hint = if record.state == JobState::Interrupted {
                    " until someone resumes it or marks it failed"
                } else {
                    ""
                };
                return Err(error(
                    ErrorCode::Conflict,
                    format!(
                        "{lock} is in use by \"{}\" ({}), started by {}{hint}",
                        record.title, record.id, record.user
                    ),
                ));
            }
        }
    }
    Ok(())
}
