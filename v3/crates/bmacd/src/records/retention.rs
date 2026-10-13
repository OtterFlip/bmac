// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Keeping the record store within its budget (`MONITORING.md`,
//! "Retention"): records older than the age limit are deleted, a stream
//! prefix at a time, and the stream's floor remembers it so they are never
//! copied back; over the size budget, the oldest records of the lowest class
//! are evicted, keeping their key and hash so peers don't send them again.
//!
//! Sizes are body bytes before compression, the same on every host.

use crate::db::DbError;
use crate::store::{CLASS_AUDIT, CLASS_DIAGNOSTIC, CLASS_JOB, CLASS_OUTPUT, Store};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const EVICT_BATCH: i64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub keep_days: u32,
    pub budget_bytes: u64,
    /// Free space on the store's filesystem never drops below this share.
    pub min_free_percent: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            keep_days: 30,
            budget_bytes: 8 << 30,
            min_free_percent: 15,
        }
    }
}

impl Policy {
    pub fn cutoff(&self, now_ms: i64) -> i64 {
        now_ms - i64::from(self.keep_days) * DAY_MS
    }

    /// The budget, less what keeps the filesystem's free space above its
    /// floor; `space` is the filesystem's total and available bytes.
    pub fn budget(&self, used: u64, space: Option<(u64, u64)>) -> u64 {
        let Some((total, available)) = space else {
            return self.budget_bytes;
        };
        let floor = total / 100 * self.min_free_percent;
        if available >= floor {
            return self.budget_bytes;
        }
        self.budget_bytes
            .min(used.saturating_sub(floor - available))
    }
}

/// The total and available bytes of the filesystem holding `path`.
pub fn space(path: &Path) -> Option<(u64, u64)> {
    let stat = rustix::fs::statvfs(path).ok()?;
    let unit = stat.f_frsize.max(1);
    Some((stat.f_blocks * unit, stat.f_bavail * unit))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub used_bytes: u64,
    pub budget_bytes: u64,
    pub oldest_ms: Option<i64>,
    /// Records deleted for age.
    pub aged: u64,
    /// Records evicted for space, by class (output of successful jobs, other
    /// job events, diagnostics, audit and failures).
    pub evicted: [u64; 4],
    /// When audit events or failures were last evicted for space, if ever.
    pub audit_evicted_ms: Option<i64>,
}

/// Applies `policy` at `now_ms`, and stops storing copies retention would
/// delete.
pub async fn enforce(
    store: &Store,
    policy: Policy,
    now_ms: i64,
    space: Option<(u64, u64)>,
) -> Result<Report, DbError> {
    store.keep_from(policy.cutoff(now_ms));
    let segments = store.segments().to_path_buf();
    store
        .database()
        .run(move |c| apply(c, &segments, policy, now_ms, space))
        .await
}

/// The store's size and the last audit eviction, without changing anything.
pub async fn usage(store: &Store) -> Result<(u64, Option<i64>, Option<i64>), DbError> {
    store
        .database()
        .run(|c| {
            let (used, oldest) = store_used(c)?;
            Ok((used, oldest, audit_evicted(c)?))
        })
        .await
}

fn store_used(c: &Connection) -> Result<(u64, Option<i64>), rusqlite::Error> {
    c.query_row(
        "SELECT COALESCE(SUM(size), 0), MIN(time_ms) FROM records WHERE evicted = 0",
        [],
        |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
    )
}

fn audit_evicted(c: &Connection) -> Result<Option<i64>, rusqlite::Error> {
    c.query_row(
        "SELECT time_ms FROM retention_events WHERE name = 'audit_evicted'",
        [],
        |r| r.get(0),
    )
    .optional()
}

fn apply(
    c: &Connection,
    segments: &Path,
    policy: Policy,
    now_ms: i64,
    space: Option<(u64, u64)>,
) -> Result<Report, rusqlite::Error> {
    let tx = c.unchecked_transaction()?;
    tx.execute(
        "UPDATE streams SET floor = MAX(floor, (SELECT MAX(seq) FROM records r
             WHERE r.origin = streams.origin AND r.stream = streams.stream AND r.time_ms < ?1))
         WHERE EXISTS (SELECT 1 FROM records r
             WHERE r.origin = streams.origin AND r.stream = streams.stream AND r.time_ms < ?1)",
        [policy.cutoff(now_ms)],
    )?;
    let aged = tx.execute(
        "DELETE FROM records WHERE seq <= (SELECT s.floor FROM streams s
             WHERE s.origin = records.origin AND s.stream = records.stream)",
        [],
    )? as u64;

    let (mut used, _) = store_used(&tx)?;
    let budget = policy.budget(used, space);
    let mut evicted = [0u64; 4];
    let steps = [
        format!(
            "class = {CLASS_OUTPUT} AND job IN (SELECT job FROM records
                 WHERE kind = 'job_event' AND etype = 'completed')"
        ),
        format!("class IN ({CLASS_OUTPUT}, {CLASS_JOB})"),
        format!("class = {CLASS_DIAGNOSTIC}"),
        format!("class = {CLASS_AUDIT}"),
    ];
    for (step, condition) in steps.iter().enumerate() {
        while used > budget {
            let mut select = tx.prepare_cached(&format!(
                "SELECT rowid, size FROM records WHERE evicted = 0 AND size > 0 AND {condition}
                 ORDER BY time_ms LIMIT {EVICT_BATCH}"
            ))?;
            let batch = select
                .query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            if batch.is_empty() {
                break;
            }
            let mut evict = tx.prepare_cached(
                "UPDATE records SET evicted = 1, body = '', segment = NULL, size = 0 WHERE rowid = ?1",
            )?;
            for (rowid, size) in batch {
                if used <= budget {
                    break;
                }
                evict.execute([rowid])?;
                used = used.saturating_sub(size);
                evicted[step] += 1;
            }
        }
    }
    if evicted[3] > 0 {
        tx.execute(
            "INSERT INTO retention_events (name, time_ms) VALUES ('audit_evicted', ?1)
             ON CONFLICT (name) DO UPDATE SET time_ms = excluded.time_ms",
            [now_ms],
        )?;
    }
    let files = {
        let mut unused = tx.prepare(
            "SELECT DISTINCT file FROM segments WHERE id NOT IN
                 (SELECT segment FROM records WHERE segment IS NOT NULL)",
        )?;
        unused
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    tx.execute(
        "DELETE FROM segments WHERE id NOT IN
             (SELECT segment FROM records WHERE segment IS NOT NULL)",
        [],
    )?;
    let still_used = {
        let mut held = tx.prepare("SELECT 1 FROM segments WHERE file = ?1 LIMIT 1")?;
        files
            .into_iter()
            .map(|file| Ok((held.exists(params![file])?, file)))
            .collect::<Result<Vec<_>, rusqlite::Error>>()?
    };
    tx.commit()?;
    for (held, file) in still_used {
        if !held && !file.contains('/') {
            let _ = std::fs::remove_file(segments.join(&file));
        }
    }
    let (used_bytes, oldest_ms) = store_used(c)?;
    Ok(Report {
        used_bytes,
        budget_bytes: budget,
        oldest_ms,
        aged,
        evicted,
        audit_evicted_ms: audit_evicted(c)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_filesystems_shrink_the_budget_to_keep_free_space() {
        let policy = Policy::default();
        assert_eq!(policy.budget(100, None), 8 << 30);
        assert_eq!(policy.budget(100, Some((1000, 500))), 8 << 30);
        assert_eq!(
            policy.budget(100, Some((1000, 140))),
            90,
            "10 bytes short of 15%"
        );
        assert_eq!(policy.budget(5, Some((1000, 0))), 0);
    }
}
