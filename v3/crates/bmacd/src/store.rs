// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The host's record log in `bmac.db`: job events, audit events, and
//! `bmacd`'s own log, each keyed `(origin host, stream, sequence)`.
//!
//! A new stream starts each time `bmacd` starts; the sequence counts up within
//! it, and every record carries a SHA-256 chained over the stream's earlier
//! records, so a copy fetched from any peer can be checked. Records are
//! written once and never rewritten, except that a finished job's events
//! move, unchanged, into compressed frames in a segment file
//! ([`Store::compact_job`]). Readers page through events and decompress one
//! frame at a time, so a large output is never loaded whole. Secrets are
//! scrubbed by the supervisor before anything reaches the store.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bmac_core::JobId;
use bmac_core::job::{EventBody, JobEvent};
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::db::{Database, DbError};

/// Raw bytes per compressed frame at most; one frame is read at a time.
pub const FRAME_BYTES: usize = 256 * 1024;
/// A finished job with fewer bytes of events stays inline.
pub const COMPACT_OVER: usize = 64 * 1024;
/// Bytes of events one page returns at most, beyond its first event.
pub const PAGE_BYTES: usize = 1024 * 1024;
pub const PAGE_EVENTS: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    JobEvent,
    Audit,
    Log,
    /// An entry from the journal collector.
    Journal,
}

impl RecordKind {
    pub fn parse(text: &str) -> Option<RecordKind> {
        [
            RecordKind::JobEvent,
            RecordKind::Audit,
            RecordKind::Log,
            RecordKind::Journal,
        ]
        .into_iter()
        .find(|k| k.as_str() == text)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RecordKind::JobEvent => "job_event",
            RecordKind::Audit => "audit",
            RecordKind::Log => "log",
            RecordKind::Journal => "journal",
        }
    }
}

/// A record to append; the store assigns its key and hash.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub time_ms: i64,
    pub kind: RecordKind,
    pub job: Option<(JobId, u64)>,
    /// One line of JSON.
    pub body: String,
}

impl Entry {
    pub fn job_event(event: &JobEvent) -> Entry {
        Entry {
            time_ms: event.at_ms,
            kind: RecordKind::JobEvent,
            job: Some((event.job, event.seq)),
            body: serde_json::to_string(&event.body).expect("job events serialize"),
        }
    }

    pub fn audit(
        time_ms: i64,
        kind: &str,
        user: Option<&str>,
        client: Option<&str>,
        message: &str,
    ) -> Entry {
        Entry {
            time_ms,
            kind: RecordKind::Audit,
            job: None,
            body: serde_json::json!({"kind": kind, "user": user, "client": client, "message": message})
                .to_string(),
        }
    }

    pub fn log(time_ms: i64, level: &str, message: &str) -> Entry {
        Entry {
            time_ms,
            kind: RecordKind::Log,
            job: None,
            body: serde_json::json!({"level": level, "message": message}).to_string(),
        }
    }

    /// A journal entry: `priority` is syslog's, 0 (emergency) to 7 (debug).
    pub fn journal(time_ms: i64, unit: &str, priority: u8, message: &str, snapshot: bool) -> Entry {
        let mut body = serde_json::json!({"unit": unit, "priority": priority, "message": message});
        if snapshot {
            body["snapshot"] = Value::Bool(true);
        }
        Entry {
            time_ms,
            kind: RecordKind::Journal,
            job: None,
            body: body.to_string(),
        }
    }
}

/// Eviction classes, evicted in this order (`MONITORING.md`, "Retention");
/// output counts as class 1 only once its job completed.
pub const CLASS_OUTPUT: i64 = 1;
pub const CLASS_JOB: i64 = 2;
pub const CLASS_DIAGNOSTIC: i64 = 3;
pub const CLASS_AUDIT: i64 = 4;

/// What search and retention need from a record without reading its body:
/// its event type, severity (0 info, 1 warning, 2 error), and class.
pub fn record_meta(kind: RecordKind, body: &str) -> (Option<String>, i64, i64) {
    let parsed: Value = serde_json::from_str(body).unwrap_or_default();
    let text = |key: &str| parsed.get(key).and_then(Value::as_str);
    let level = |level: Option<&str>| match level {
        Some("error") => 2,
        Some("warning") => 1,
        _ => 0,
    };
    match kind {
        RecordKind::JobEvent => {
            let etype = text("type").map(String::from);
            let (severity, class) = match etype.as_deref() {
                Some("failed" | "interrupted") => (2, CLASS_AUDIT),
                Some("warning" | "validation_error" | "cancelled") => (1, CLASS_JOB),
                Some("output") => (level(text("level")), CLASS_OUTPUT),
                _ => (0, CLASS_JOB),
            };
            (etype, severity, class)
        }
        RecordKind::Audit => (None, 0, CLASS_AUDIT),
        RecordKind::Log => (None, level(text("level")), CLASS_DIAGNOSTIC),
        RecordKind::Journal => {
            let severity = match parsed.get("priority").and_then(Value::as_u64) {
                Some(0..=3) => 2,
                Some(4) => 1,
                _ => 0,
            };
            (None, severity, CLASS_DIAGNOSTIC)
        }
    }
}

pub use bmac_proto::peer::Record;

/// The entry a copied record holds, if it is well formed.
fn record_entry(record: &Record) -> Option<Entry> {
    Some(Entry {
        time_ms: record.time_ms,
        kind: RecordKind::parse(&record.kind)?,
        job: match (record.job, record.job_seq) {
            (Some(job), Some(seq)) => Some((job, seq)),
            (None, None) => None,
            _ => return None,
        },
        body: record.body.clone(),
    })
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Why copies were refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CopyError {
    #[error("record {origin}/{stream}/{seq} follows one this host doesn't hold")]
    Gap {
        origin: String,
        stream: String,
        seq: u64,
    },
    #[error("record {origin}/{stream}/{seq} doesn't match its stream's hash chain")]
    Chain {
        origin: String,
        stream: String,
        seq: u64,
    },
    #[error("record {origin}/{stream}/{seq} is malformed")]
    Malformed {
        origin: String,
        stream: String,
        seq: u64,
    },
    #[error("record {origin}/{stream}/{seq} is this host's own")]
    Own {
        origin: String,
        stream: String,
        seq: u64,
    },
}

/// One page of a job's events, oldest first.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EventPage {
    pub events: Vec<JobEvent>,
    /// Whether events after this page exist.
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub time_ms: i64,
    pub host: String,
    pub kind: String,
    pub user: Option<String>,
    pub client: Option<String>,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Copy(#[from] CopyError),
    #[error("segment file {path}: {source}")]
    Segment {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy)]
struct Chain {
    seq: u64,
    hash: [u8; 32],
}

struct Inner {
    db: Database,
    origin: String,
    stream: String,
    segments: PathBuf,
    chain: Mutex<Chain>,
    appended: tokio::sync::broadcast::Sender<Arc<Record>>,
    /// Copies older than this are not stored: retention would delete them.
    keep_from_ms: std::sync::atomic::AtomicI64,
    #[cfg(test)]
    fail_after_segment_write: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("origin", &self.inner.origin)
            .field("stream", &self.inner.stream)
            .finish_non_exhaustive()
    }
}

/// The chained hash of one record.
fn chain_hash(prev: &[u8; 32], origin: &str, stream: &str, seq: u64, entry: &Entry) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(prev);
    for part in [origin, stream, entry.kind.as_str()] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
    }
    h.update(seq.to_be_bytes());
    h.update(entry.time_ms.to_be_bytes());
    match entry.job {
        Some((job, job_seq)) => {
            h.update([1]);
            h.update(job.to_string().as_bytes());
            h.update(job_seq.to_be_bytes());
        }
        None => h.update([0]),
    }
    h.update((entry.body.len() as u64).to_be_bytes());
    h.update(entry.body.as_bytes());
    h.finalize().into()
}

fn new_stream(started_ms: i64) -> String {
    let mut bytes = [0u8; 6];
    let _ = SystemRandom::new().fill(&mut bytes);
    let random: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{started_ms:x}-{random}")
}

fn segment_error(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Segment {
        path: path.to_path_buf(),
        source,
    }
}

impl Store {
    /// Starts this incarnation's stream.
    pub async fn open(
        db: Database,
        origin: impl Into<String>,
        now_ms: i64,
    ) -> Result<Store, DbError> {
        let segments = db.segments_dir();
        let origin = origin.into();
        let stream = new_stream(now_ms);
        let (o, s) = (origin.clone(), stream.clone());
        db.run(move |c| {
            c.execute(
                "INSERT INTO streams (origin, stream, started_ms) VALUES (?1, ?2, ?3)",
                params![o, s, now_ms],
            )
        })
        .await?;
        Ok(Store {
            inner: Arc::new(Inner {
                db,
                origin,
                stream,
                segments,
                chain: Mutex::new(Chain {
                    seq: 0,
                    hash: [0; 32],
                }),
                appended: tokio::sync::broadcast::channel(4096).0,
                keep_from_ms: std::sync::atomic::AtomicI64::new(i64::MIN),
                #[cfg(test)]
                fail_after_segment_write: std::sync::atomic::AtomicBool::new(false),
            }),
        })
    }

    pub fn origin(&self) -> &str {
        &self.inner.origin
    }

    pub fn stream(&self) -> &str {
        &self.inner.stream
    }

    pub(crate) fn database(&self) -> &Database {
        &self.inner.db
    }

    pub(crate) fn segments(&self) -> &Path {
        &self.inner.segments
    }

    /// The oldest time a copy may have and still be stored.
    pub(crate) fn kept_from(&self) -> i64 {
        self.inner
            .keep_from_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Copies older than `ms` are no longer stored.
    pub(crate) fn keep_from(&self, ms: i64) {
        self.inner
            .keep_from_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Appends records in one transaction, in order.
    pub async fn append(&self, entries: Vec<Entry>) -> Result<(), DbError> {
        if entries.is_empty() {
            return Ok(());
        }
        let inner = self.inner.clone();
        self.inner
            .db
            .run(move |c| {
                let mut chain = inner.chain.lock().unwrap_or_else(|p| p.into_inner());
                let mut next = *chain;
                let tx = c.unchecked_transaction()?;
                {
                    let mut insert = tx.prepare_cached(INSERT_RECORD)?;
                    for entry in &entries {
                        let prev = next.hash;
                        next.seq += 1;
                        next.hash =
                            chain_hash(&prev, &inner.origin, &inner.stream, next.seq, entry);
                        insert_row(
                            &mut insert,
                            &inner.origin,
                            &inner.stream,
                            next.seq,
                            entry,
                            &next.hash,
                            &prev,
                            false,
                        )?;
                    }
                }
                tx.commit()?;
                let mut seq = chain.seq;
                let mut prev = chain.hash;
                for entry in entries {
                    seq += 1;
                    let before = prev;
                    prev = chain_hash(&prev, &inner.origin, &inner.stream, seq, &entry);
                    let _ = inner.appended.send(Arc::new(Record {
                        origin: inner.origin.clone(),
                        stream: inner.stream.clone(),
                        seq,
                        time_ms: entry.time_ms,
                        kind: entry.kind.as_str().to_string(),
                        job: entry.job.map(|(j, _)| j),
                        job_seq: entry.job.map(|(_, s)| s),
                        body: entry.body,
                        hash: hex(&prev),
                        prev: Some(hex(&before)),
                        evicted: false,
                    }));
                }
                *chain = next;
                Ok(())
            })
            .await
    }

    /// This host's records as they are committed, in order. A receiver that
    /// lags reads the gap from [`Store::records_after`].
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<Record>> {
        self.inner.appended.subscribe()
    }

    /// The newest stream of `origin` this host knows.
    pub async fn latest_stream(&self, origin: &str) -> Result<Option<String>, DbError> {
        latest_stream(&self.inner.db, origin).await
    }

    /// This host's streams started after `stream`, oldest first; `None` if
    /// `stream` isn't one of its own.
    pub async fn later_streams(&self, stream: &str) -> Result<Option<Vec<String>>, DbError> {
        let (origin, stream) = (self.inner.origin.clone(), stream.to_string());
        self.inner
            .db
            .run(move |c| {
                let Some((started, rowid)) = c
                    .query_row(
                        "SELECT started_ms, rowid FROM streams WHERE origin = ?1 AND stream = ?2",
                        params![origin, stream],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                    )
                    .optional()?
                else {
                    return Ok(None);
                };
                let mut query = c.prepare(
                    "SELECT stream FROM streams WHERE origin = ?1 AND (started_ms, rowid) > (?2, ?3)
                     ORDER BY started_ms, rowid",
                )?;
                let streams = query
                    .query_map(params![origin, started, rowid], |r| r.get(0))?
                    .collect::<Result<Vec<String>, _>>()?;
                Ok(Some(streams))
            })
            .await
    }

    /// The highest sequence held for a stream, or 0.
    pub async fn last_seq(&self, origin: &str, stream: &str) -> Result<u64, DbError> {
        let (origin, stream) = (origin.to_string(), stream.to_string());
        self.inner
            .db
            .run(move |c| {
                c.query_row(
                    "SELECT COALESCE(MAX(seq), 0) FROM records WHERE origin = ?1 AND stream = ?2",
                    params![origin, stream],
                    |r| r.get::<_, i64>(0),
                )
            })
            .await
            .map(|n| n as u64)
    }

    /// Up to `limit` records of a stream after `after`, oldest first.
    pub async fn records_after(
        &self,
        origin: &str,
        stream: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<Record>, StoreError> {
        let (origin, stream) = (origin.to_string(), stream.to_string());
        let segments = self.inner.segments.clone();
        self.inner
            .db
            .run(move |c| Ok(read_records(c, &segments, &origin, &stream, after, limit)))
            .await?
    }

    /// Stores copies of other hosts' records, in order, skipping those
    /// already held. Each must follow one this host holds, and its hash must
    /// continue that record's. Returns the newly stored ones.
    pub async fn insert_copies(&self, records: Vec<Record>) -> Result<Vec<Record>, StoreError> {
        let inner = self.inner.clone();
        self.inner
            .db
            .run(move |c| Ok(insert_copies(c, &inner, records)))
            .await?
    }

    /// The highest event sequence stored for `job`, or 0.
    pub async fn last_job_seq(&self, job: JobId) -> Result<u64, DbError> {
        self.inner
            .db
            .run(move |c| {
                c.query_row(
                    "SELECT COALESCE(MAX(job_seq), 0) FROM records WHERE job = ?1 AND kind = 'job_event'",
                    [job.to_string()],
                    |r| r.get::<_, i64>(0),
                )
            })
            .await
            .map(|n| n as u64)
    }

    /// `job`'s events from sequence `from_seq`, at most `limit` of them and
    /// about [`PAGE_BYTES`].
    pub async fn job_events(
        &self,
        job: JobId,
        from_seq: u64,
        limit: usize,
    ) -> Result<EventPage, StoreError> {
        let segments = self.inner.segments.clone();
        let limit = limit.clamp(1, PAGE_EVENTS);
        self.inner
            .db
            .run(move |c| Ok(read_page(c, &segments, job, from_seq, limit)))
            .await?
    }

    /// The newest `limit` audit events, newest first.
    pub async fn audit_recent(&self, limit: u32) -> Result<Vec<AuditRecord>, DbError> {
        self.inner
            .db
            .run(move |c| {
                let mut statement = c.prepare(
                    "SELECT time_ms, origin, body FROM records WHERE kind = 'audit' AND evicted = 0
                     ORDER BY time_ms DESC, seq DESC LIMIT ?1",
                )?;
                let rows = statement.query_map([limit], |row| {
                    let body: String = row.get(2)?;
                    let body: Value = serde_json::from_str(&body).unwrap_or_default();
                    let text = |key: &str| body.get(key).and_then(Value::as_str).map(String::from);
                    Ok(AuditRecord {
                        time_ms: row.get(0)?,
                        host: row.get(1)?,
                        kind: text("kind").unwrap_or_default(),
                        user: text("user"),
                        client: text("client"),
                        message: text("message").unwrap_or_default(),
                    })
                })?;
                rows.collect()
            })
            .await
    }

    /// Moves a finished job's events into compressed frames when they hold
    /// more than [`COMPACT_OVER`] bytes. Each frame is written and synced
    /// before the rows point at it, so a crash leaves at worst unreferenced
    /// bytes at the end of the segment file. Returns how many events moved.
    pub async fn compact_job(&self, job: JobId) -> Result<usize, StoreError> {
        let inner = self.inner.clone();
        self.inner
            .db
            .run(move |c| Ok(compact(c, &inner, job)))
            .await?
    }
}

pub(crate) type Key = (String, String, i64);

fn compact(c: &Connection, inner: &Inner, job: JobId) -> Result<usize, StoreError> {
    let id = job.to_string();
    let plan: Vec<(Key, usize)> = {
        let mut statement = c
            .prepare(
                "SELECT origin, stream, seq, length(CAST(body AS BLOB)) FROM records
                 WHERE job = ?1 AND kind = 'job_event' AND body IS NOT NULL
                 ORDER BY job_seq, origin, stream, seq",
            )
            .map_err(DbError::from)?;
        let rows = statement
            .query_map([&id], |r| {
                Ok((
                    (r.get(0)?, r.get(1)?, r.get(2)?),
                    r.get::<_, i64>(3)? as usize,
                ))
            })
            .map_err(DbError::from)?;
        rows.collect::<Result<_, _>>().map_err(DbError::from)?
    };
    if plan.iter().map(|(_, n)| n).sum::<usize>() <= COMPACT_OVER {
        return Ok(0);
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&inner.segments)
        .map_err(segment_error(&inner.segments))?;
    let name = format!("{id}.seg");
    let path = inner.segments.join(&name);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .map_err(segment_error(&path))?;
    let mut moved = 0;
    let mut frames: Vec<Vec<Key>> = vec![Vec::new()];
    let mut size = 0;
    for (key, len) in plan {
        if size + len > FRAME_BYTES && !frames.last().unwrap().is_empty() {
            frames.push(Vec::new());
            size = 0;
        }
        size += len;
        frames.last_mut().unwrap().push(key);
    }
    for keys in frames {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        let mut raw = 0;
        {
            let mut select = c
                .prepare_cached(
                    "SELECT body FROM records WHERE origin = ?1 AND stream = ?2 AND seq = ?3",
                )
                .map_err(DbError::from)?;
            for (origin, stream, seq) in &keys {
                let body: String = select
                    .query_row(params![origin, stream, seq], |r| r.get(0))
                    .map_err(DbError::from)?;
                raw += body.len();
                writeln!(encoder, "{origin}\t{stream}\t{seq}\t{body}")
                    .map_err(segment_error(&path))?;
            }
        }
        let frame = encoder.finish().map_err(segment_error(&path))?;
        let offset = file.metadata().map_err(segment_error(&path))?.len();
        file.write_all(&frame).map_err(segment_error(&path))?;
        file.sync_data().map_err(segment_error(&path))?;
        #[cfg(test)]
        if inner
            .fail_after_segment_write
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Segment {
                path,
                source: std::io::Error::other("injected failure"),
            });
        }
        let tx = c.unchecked_transaction().map_err(DbError::from)?;
        tx.execute(
            "INSERT INTO segments (file, offset, length, raw_bytes) VALUES (?1, ?2, ?3, ?4)",
            params![name, offset as i64, frame.len() as i64, raw as i64],
        )
        .map_err(DbError::from)?;
        let segment = tx.last_insert_rowid();
        {
            let mut update = tx
                .prepare_cached(
                    "UPDATE records SET body = NULL, segment = ?4
                     WHERE origin = ?1 AND stream = ?2 AND seq = ?3",
                )
                .map_err(DbError::from)?;
            for (origin, stream, seq) in &keys {
                update
                    .execute(params![origin, stream, seq, segment])
                    .map_err(DbError::from)?;
            }
        }
        tx.commit().map_err(DbError::from)?;
        moved += keys.len();
    }
    Ok(moved)
}

/// Reads one frame and returns its bodies by key.
pub(crate) fn read_frame(
    c: &Connection,
    segments: &Path,
    segment: i64,
) -> Result<HashMap<Key, String>, StoreError> {
    let (file, offset, length, raw): (String, i64, i64, i64) = c
        .query_row(
            "SELECT file, offset, length, raw_bytes FROM segments WHERE id = ?1",
            [segment],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(DbError::from)?;
    let path = segments.join(&file);
    let mut f = File::open(&path).map_err(segment_error(&path))?;
    f.seek(SeekFrom::Start(offset as u64))
        .map_err(segment_error(&path))?;
    let mut compressed = Vec::with_capacity(length as usize);
    f.take(length as u64)
        .read_to_end(&mut compressed)
        .map_err(segment_error(&path))?;
    let mut text = String::new();
    DeflateDecoder::new(compressed.as_slice())
        .take(raw as u64 + 64 * 1024 * 1024)
        .read_to_string(&mut text)
        .map_err(segment_error(&path))?;
    let mut bodies = HashMap::new();
    for line in text.lines() {
        let mut parts = line.splitn(4, '\t');
        if let (Some(origin), Some(stream), Some(seq), Some(body)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
            && let Ok(seq) = seq.parse()
        {
            bodies.insert(
                (origin.to_string(), stream.to_string(), seq),
                body.to_string(),
            );
        }
    }
    Ok(bodies)
}

fn read_page(
    c: &Connection,
    segments: &Path,
    job: JobId,
    from_seq: u64,
    limit: usize,
) -> Result<EventPage, StoreError> {
    let mut statement = c
        .prepare(
            "SELECT job_seq, time_ms, body, segment, origin, stream, seq FROM records
             WHERE job = ?1 AND kind = 'job_event' AND job_seq >= ?2
             ORDER BY job_seq, origin, stream, seq",
        )
        .map_err(DbError::from)?;
    let mut rows = statement
        .query(params![job.to_string(), from_seq as i64])
        .map_err(DbError::from)?;
    let mut page = EventPage::default();
    let mut bytes = 0;
    let mut frame: Option<(i64, HashMap<Key, String>)> = None;
    let mut last: Option<u64> = None;
    while let Some(row) = rows.next().map_err(DbError::from)? {
        let job_seq = row.get::<_, i64>(0).map_err(DbError::from)? as u64;
        if last == Some(job_seq) {
            continue;
        }
        if page.events.len() >= limit || (bytes >= PAGE_BYTES && !page.events.is_empty()) {
            page.more = true;
            break;
        }
        let body: Option<String> = row.get(2).map_err(DbError::from)?;
        let body = match body {
            Some(body) => body,
            None => {
                let segment: Option<i64> = row.get(3).map_err(DbError::from)?;
                let Some(segment) = segment else { continue };
                if frame.as_ref().is_none_or(|(id, _)| *id != segment) {
                    frame = Some((segment, read_frame(c, segments, segment)?));
                }
                let key: Key = (
                    row.get(4).map_err(DbError::from)?,
                    row.get(5).map_err(DbError::from)?,
                    row.get(6).map_err(DbError::from)?,
                );
                match frame.as_ref().and_then(|(_, f)| f.get(&key)) {
                    Some(body) => body.clone(),
                    None => continue,
                }
            }
        };
        let Ok(parsed) = serde_json::from_str::<EventBody>(&body) else {
            continue;
        };
        bytes += body.len();
        last = Some(job_seq);
        page.events.push(JobEvent {
            job,
            seq: job_seq,
            at_ms: row.get(1).map_err(DbError::from)?,
            body: parsed,
        });
    }
    Ok(page)
}

fn read_records(
    c: &Connection,
    segments: &Path,
    origin: &str,
    stream: &str,
    after: u64,
    limit: usize,
) -> Result<Vec<Record>, StoreError> {
    let mut statement = c
        .prepare(
            "SELECT seq, time_ms, kind, job, job_seq, body, segment, hash, prev, evicted FROM records
             WHERE origin = ?1 AND stream = ?2 AND seq > ?3 ORDER BY seq LIMIT ?4",
        )
        .map_err(DbError::from)?;
    let mut rows = statement
        .query(params![origin, stream, after as i64, limit as i64])
        .map_err(DbError::from)?;
    let mut out = Vec::new();
    let mut frame: Option<(i64, HashMap<Key, String>)> = None;
    while let Some(row) = rows.next().map_err(DbError::from)? {
        let seq: i64 = row.get(0).map_err(DbError::from)?;
        let body: Option<String> = row.get(5).map_err(DbError::from)?;
        let body = match body {
            Some(body) => body,
            None => {
                let Some(segment) = row.get::<_, Option<i64>>(6).map_err(DbError::from)? else {
                    continue;
                };
                if frame.as_ref().is_none_or(|(id, _)| *id != segment) {
                    frame = Some((segment, read_frame(c, segments, segment)?));
                }
                let key = (origin.to_string(), stream.to_string(), seq);
                match frame.as_ref().and_then(|(_, f)| f.get(&key)) {
                    Some(body) => body.clone(),
                    None => continue,
                }
            }
        };
        let job: Option<String> = row.get(3).map_err(DbError::from)?;
        let hash: Vec<u8> = row.get(7).map_err(DbError::from)?;
        let prev: Option<Vec<u8>> = row.get(8).map_err(DbError::from)?;
        let evicted: bool = row.get(9).map_err(DbError::from)?;
        out.push(Record {
            origin: origin.to_string(),
            stream: stream.to_string(),
            seq: seq as u64,
            time_ms: row.get(1).map_err(DbError::from)?,
            kind: row.get(2).map_err(DbError::from)?,
            job: job.and_then(|j| j.parse().ok()),
            job_seq: row
                .get::<_, Option<i64>>(4)
                .map_err(DbError::from)?
                .map(|s| s as u64),
            body,
            hash: hex(&hash),
            prev: prev.map(|p| hex(&p)),
            evicted,
        });
    }
    Ok(out)
}

const INSERT_RECORD: &str = "INSERT INTO records
    (origin, stream, seq, time_ms, kind, job, job_seq, body, hash, prev, evicted, size, etype, severity, class)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)";

#[allow(clippy::too_many_arguments)]
fn insert_row(
    insert: &mut rusqlite::CachedStatement<'_>,
    origin: &str,
    stream: &str,
    seq: u64,
    entry: &Entry,
    hash: &[u8; 32],
    prev: &[u8; 32],
    evicted: bool,
) -> Result<(), rusqlite::Error> {
    let (etype, severity, class) = record_meta(entry.kind, &entry.body);
    insert.execute(params![
        origin,
        stream,
        seq as i64,
        entry.time_ms,
        entry.kind.as_str(),
        entry.job.map(|(j, _)| j.to_string()),
        entry.job.map(|(_, s)| s as i64),
        entry.body,
        hash.as_slice(),
        prev.as_slice(),
        evicted,
        entry.body.len() as i64,
        etype,
        severity,
        class,
    ])?;
    Ok(())
}

pub(crate) fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn insert_copies(
    c: &Connection,
    inner: &Inner,
    records: Vec<Record>,
) -> Result<Vec<Record>, StoreError> {
    let keep_from = inner
        .keep_from_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    let tx = c.unchecked_transaction().map_err(DbError::from)?;
    let mut inserted = Vec::new();
    {
        let mut held = tx
            .prepare_cached(
                "SELECT hash FROM records WHERE origin = ?1 AND stream = ?2 AND seq = ?3",
            )
            .map_err(DbError::from)?;
        let mut floor = tx
            .prepare_cached("SELECT floor FROM streams WHERE origin = ?1 AND stream = ?2")
            .map_err(DbError::from)?;
        let mut stream_known = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO streams (origin, stream, started_ms) VALUES (?1, ?2, ?3)",
            )
            .map_err(DbError::from)?;
        let mut insert = tx.prepare_cached(INSERT_RECORD).map_err(DbError::from)?;
        for record in records {
            let (origin, stream, seq) = (record.origin.clone(), record.stream.clone(), record.seq);
            let refuse = |make: fn(String, String, u64) -> CopyError| {
                StoreError::from(make(origin.clone(), stream.clone(), seq))
            };
            if origin == inner.origin && stream == inner.stream {
                return Err(refuse(|origin, stream, seq| CopyError::Own {
                    origin,
                    stream,
                    seq,
                }));
            }
            let malformed = |origin, stream, seq| CopyError::Malformed {
                origin,
                stream,
                seq,
            };
            let given = match &record.prev {
                Some(text) => Some(unhex(text).ok_or_else(|| refuse(malformed))?),
                None => None,
            };
            let Some(entry) = record_entry(&record)
                .filter(|_| seq > 0)
                .filter(|_| !record.evicted || record.body.is_empty())
            else {
                return Err(refuse(malformed));
            };
            let hash = unhex(&record.hash).ok_or_else(|| refuse(malformed))?;
            let floor: i64 = floor
                .query_row(params![origin, stream], |r| r.get(0))
                .optional()
                .map_err(DbError::from)?
                .unwrap_or(0);
            if (seq as i64) <= floor || record.time_ms < keep_from {
                continue;
            }
            let chain = |origin, stream, seq| CopyError::Chain {
                origin,
                stream,
                seq,
            };
            let lookup = |held: &mut rusqlite::CachedStatement<'_>, seq: u64| {
                held.query_row(params![origin, stream, seq as i64], |r| {
                    r.get::<_, Vec<u8>>(0)
                })
                .optional()
                .map(|h| h.and_then(|h| <[u8; 32]>::try_from(h).ok()))
                .map_err(DbError::from)
            };
            let before = if seq == 1 {
                Some([0; 32])
            } else {
                lookup(&mut held, seq - 1)?
            };
            let prev = match (before, given) {
                (Some(held), Some(given)) if held != given => return Err(refuse(chain)),
                (Some(held), _) => held,
                (None, Some(given)) => given,
                (None, None) => {
                    return Err(refuse(|origin, stream, seq| CopyError::Gap {
                        origin,
                        stream,
                        seq,
                    }));
                }
            };
            if !record.evicted && chain_hash(&prev, &origin, &stream, seq, &entry) != hash {
                return Err(refuse(chain));
            }
            if let Some(existing) = lookup(&mut held, seq)? {
                if existing != hash {
                    return Err(refuse(chain));
                }
                continue;
            }
            stream_known
                .execute(params![
                    origin,
                    stream,
                    stream_started(&stream).unwrap_or(record.time_ms)
                ])
                .map_err(DbError::from)?;
            insert_row(
                &mut insert,
                &origin,
                &stream,
                seq,
                &entry,
                &hash,
                &prev,
                record.evicted,
            )
            .map_err(DbError::from)?;
            inserted.push(record);
        }
    }
    tx.commit().map_err(DbError::from)?;
    Ok(inserted)
}

/// When a stream started, from its name ([`new_stream`]).
fn stream_started(stream: &str) -> Option<i64> {
    i64::from_str_radix(stream.split_once('-')?.0, 16).ok()
}

/// The newest stream of `origin` this host knows.
async fn latest_stream(db: &Database, origin: &str) -> Result<Option<String>, DbError> {
    let origin = origin.to_string();
    db.run(move |c| {
        c.query_row(
            "SELECT stream FROM streams WHERE origin = ?1 ORDER BY started_ms DESC, rowid DESC LIMIT 1",
            [origin],
            |r| r.get(0),
        )
        .optional()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_core::job::LogLevel;

    fn output(job: JobId, seq: u64, text: String) -> JobEvent {
        JobEvent {
            job,
            seq,
            at_ms: 1_000 + seq as i64,
            body: EventBody::Output {
                stream: None,
                level: LogLevel::Info,
                text,
                source: None,
            },
        }
    }

    async fn store(dir: &Path) -> Store {
        let db = Database::open(&dir.join("bmac.db")).unwrap();
        Store::open(db, "mox1", 1_000).await.unwrap()
    }

    fn text_of(event: &JobEvent) -> &str {
        match &event.body {
            EventBody::Output { text, .. } => text,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn records_are_keyed_and_chained_within_a_stream() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path()).await;
        let job = JobId::random();
        store
            .append(vec![
                Entry::job_event(&output(job, 1, "one".into())),
                Entry::audit(
                    5,
                    "login",
                    Some("root@pam"),
                    Some("100.64.0.1"),
                    "logged in",
                ),
                Entry::log(6, "warning", "a note"),
            ])
            .await
            .unwrap();
        store
            .append(vec![Entry::job_event(&output(job, 2, "two".into()))])
            .await
            .unwrap();
        let c = Connection::open(dir.path().join("bmac.db")).unwrap();
        let mut statement = c
            .prepare("SELECT seq, kind, hash FROM records WHERE stream = ?1 ORDER BY seq")
            .unwrap();
        let rows: Vec<(i64, String, Vec<u8>)> = statement
            .query_map([store.stream()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| (r.0, r.1.as_str())).collect::<Vec<_>>(),
            [(1, "job_event"), (2, "audit"), (3, "log"), (4, "job_event")]
        );
        let first = chain_hash(
            &[0; 32],
            "mox1",
            store.stream(),
            1,
            &Entry::job_event(&output(job, 1, "one".into())),
        );
        assert_eq!(rows[0].2, first);
        assert_ne!(rows[1].2, rows[0].2);

        let restarted = Store::open(store.inner.db.clone(), "mox1", 2_000)
            .await
            .unwrap();
        assert_ne!(
            restarted.stream(),
            store.stream(),
            "a new incarnation, a new stream"
        );
        let audit = restarted.audit_recent(10).await.unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].user.as_deref(), Some("root@pam"));
        assert_eq!(audit[0].host, "mox1");
    }

    #[tokio::test]
    async fn a_large_output_is_paged_and_compacted_without_loading_it_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path()).await;
        let job = JobId::random();
        let line = "x".repeat(1000);
        for batch in 0..20u64 {
            let events: Vec<Entry> = (1..=500)
                .map(|i| {
                    let seq = batch * 500 + i;
                    Entry::job_event(&output(job, seq, format!("{seq} {line}")))
                })
                .collect();
            store.append(events).await.unwrap();
        }
        assert_eq!(store.last_job_seq(job).await.unwrap(), 10_000);

        let first = store.job_events(job, 1, PAGE_EVENTS).await.unwrap();
        assert!(first.more);
        let bytes: usize = first.events.iter().map(|e| text_of(e).len()).sum();
        assert!(bytes <= PAGE_BYTES + 2048, "a page is bounded: {bytes}");
        assert_eq!(first.events[0].seq, 1);

        let moved = store.compact_job(job).await.unwrap();
        assert_eq!(moved, 10_000);
        let c = Connection::open(dir.path().join("bmac.db")).unwrap();
        let (frames, largest): (i64, i64) = c
            .query_row("SELECT COUNT(*), MAX(raw_bytes) FROM segments", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!(frames > 30, "{frames} frames");
        assert!(
            largest as usize <= FRAME_BYTES,
            "a frame is bounded: {largest}"
        );
        let inline: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM records WHERE body IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inline, 0);
        let file =
            std::fs::metadata(dir.path().join("segments").join(format!("{job}.seg"))).unwrap();
        assert!(file.len() < 1_000_000, "compressed: {}", file.len());

        let mut seq = 1;
        let mut count = 0;
        loop {
            let page = store.job_events(job, seq, 777).await.unwrap();
            for event in &page.events {
                assert_eq!(event.seq, seq);
                assert!(text_of(event).starts_with(&format!("{seq} ")));
                seq += 1;
                count += 1;
            }
            if !page.more {
                break;
            }
        }
        assert_eq!(count, 10_000);
        assert_eq!(
            store.compact_job(job).await.unwrap(),
            0,
            "already compacted"
        );
    }

    #[tokio::test]
    async fn a_small_job_stays_inline() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path()).await;
        let job = JobId::random();
        store
            .append(vec![Entry::job_event(&output(job, 1, "short".into()))])
            .await
            .unwrap();
        assert_eq!(store.compact_job(job).await.unwrap(), 0);
        assert!(!dir.path().join("segments").exists());
    }

    #[tokio::test]
    async fn a_failure_between_the_segment_write_and_the_database_update_loses_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path()).await;
        let job = JobId::random();
        let events: Vec<Entry> = (1..=200)
            .map(|seq| Entry::job_event(&output(job, seq, format!("{seq} {}", "y".repeat(2000)))))
            .collect();
        store.append(events).await.unwrap();
        store
            .inner
            .fail_after_segment_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(store.compact_job(job).await.is_err());
        let page = store.job_events(job, 1, 1000).await.unwrap();
        assert_eq!(page.events.len(), 200, "the events are still inline");
        store
            .inner
            .fail_after_segment_write
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(store.compact_job(job).await.unwrap(), 200);
        let page = store.job_events(job, 1, 1000).await.unwrap();
        assert_eq!(page.events.len(), 200);
        assert!(text_of(&page.events[199]).starts_with("200 "));
    }

    #[tokio::test]
    async fn copies_continue_the_chain_and_are_stored_once() {
        let dir = tempfile::tempdir().unwrap();
        let mox1 = store(dir.path()).await;
        let other = tempfile::tempdir().unwrap();
        let mox2 = Store::open(
            Database::open(&other.path().join("bmac.db")).unwrap(),
            "mox2",
            1_000,
        )
        .await
        .unwrap();
        let mut live = mox1.subscribe();
        let job = JobId::random();
        mox1.append(
            (1..=5)
                .map(|s| Entry::job_event(&output(job, s, format!("line {s}"))))
                .collect(),
        )
        .await
        .unwrap();
        let sent: Vec<Record> = (0..5)
            .map(|_| (*live.try_recv().unwrap()).clone())
            .collect();
        let read = mox1
            .records_after("mox1", mox1.stream(), 0, 100)
            .await
            .unwrap();
        assert_eq!(read, sent, "the live records match what is read back");

        assert_eq!(
            mox2.insert_copies(read[..3].to_vec()).await.unwrap().len(),
            3
        );
        assert_eq!(mox2.last_seq("mox1", mox1.stream()).await.unwrap(), 3);
        let again = mox2.insert_copies(read.clone()).await.unwrap();
        assert_eq!(
            again.iter().map(|r| r.seq).collect::<Vec<_>>(),
            [4, 5],
            "held ones are skipped"
        );
        let page = mox2.job_events(job, 1, 100).await.unwrap();
        assert_eq!(page.events.len(), 5, "copied job events are readable");

        let mut forged = read[4].clone();
        forged.seq = 6;
        assert!(matches!(
            mox2.insert_copies(vec![forged.clone()]).await,
            Err(StoreError::Copy(CopyError::Chain { .. }))
        ));
        forged.seq = 8;
        assert!(matches!(
            mox2.insert_copies(vec![forged.clone()]).await,
            Err(StoreError::Copy(CopyError::Chain { .. }))
        ));
        forged.prev = None;
        assert!(matches!(
            mox2.insert_copies(vec![forged]).await,
            Err(StoreError::Copy(CopyError::Gap { .. }))
        ));

        let dir3 = tempfile::tempdir().unwrap();
        let mox3 = Store::open(
            Database::open(&dir3.path().join("bmac.db")).unwrap(),
            "mox3",
            1_000,
        )
        .await
        .unwrap();
        assert_eq!(
            mox3.insert_copies(vec![read[3].clone()])
                .await
                .unwrap()
                .len(),
            1,
            "a copy carrying the hash before it is stored without its predecessor"
        );
        let mut wrong = read[2].clone();
        wrong.prev = read[0].prev.clone();
        assert!(matches!(
            mox3.insert_copies(vec![wrong]).await,
            Err(StoreError::Copy(CopyError::Chain { .. }))
        ));
        assert_eq!(
            mox3.insert_copies(read.clone()).await.unwrap().len(),
            4,
            "the hole and the rest are filled later"
        );
        let mut altered = read[2].clone();
        altered.body = altered.body.replace("line 3", "line X");
        assert!(matches!(
            mox2.insert_copies(vec![altered]).await,
            Err(StoreError::Copy(CopyError::Chain { .. }))
        ));
        let own = mox2
            .records_after("mox1", mox1.stream(), 0, 1)
            .await
            .unwrap();
        assert!(matches!(
            mox1.insert_copies(own).await,
            Err(StoreError::Copy(CopyError::Own { .. }))
        ));

        mox1.append(
            (6..=200)
                .map(|s| Entry::job_event(&output(job, s, "w".repeat(1000))))
                .collect(),
        )
        .await
        .unwrap();
        mox1.compact_job(job).await.unwrap();
        let compacted = mox1
            .records_after("mox1", mox1.stream(), 5, 1000)
            .await
            .unwrap();
        assert_eq!(compacted.len(), 195);
        assert_eq!(
            mox2.insert_copies(compacted).await.unwrap().len(),
            195,
            "compacted records copy intact"
        );
    }

    const CHILD: &str = "BMAC_STORE_CRASH_CHILD";

    /// Run in a child process by the next test: writes until it aborts.
    #[tokio::test]
    async fn crash_child_writer() {
        let Ok(dir) = std::env::var(CHILD) else {
            return;
        };
        let store = store(Path::new(&dir)).await;
        let job: JobId = std::env::var("BMAC_STORE_CRASH_JOB")
            .unwrap()
            .parse()
            .unwrap();
        let mut seq = 0;
        let mut batch = 0;
        loop {
            batch += 1;
            let events: Vec<Entry> = (0..100)
                .map(|_| {
                    seq += 1;
                    Entry::job_event(&output(job, seq, "z".repeat(500)))
                })
                .collect();
            if batch == 31 {
                let inner = store.inner.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    drop(inner);
                    std::process::abort();
                });
            }
            store.append(events).await.unwrap();
        }
    }

    #[test]
    fn a_crash_during_writes_leaves_a_readable_database() {
        let dir = tempfile::tempdir().unwrap();
        let job = JobId::random();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "store::tests::crash_child_writer",
                "--exact",
                "--test-threads=1",
            ])
            .env(CHILD, dir.path())
            .env("BMAC_STORE_CRASH_JOB", job.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "the child aborts");

        let c = Connection::open(dir.path().join("bmac.db")).unwrap();
        let check: String = c
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        let (count, max): (i64, i64) = c
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(job_seq), 0) FROM records WHERE job = ?1",
                [job.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(count >= 3000, "the committed batches survive: {count}");
        assert_eq!(count, max, "whole batches only, with no gaps");
        assert_eq!(count % 100, 0);
        drop(c);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let store = store(dir.path()).await;
            let page = store.job_events(job, 1, 10).await.unwrap();
            assert_eq!(page.events.len(), 10);
            store
                .append(vec![Entry::log(9, "info", "after the crash")])
                .await
                .unwrap();
        });
    }
}
