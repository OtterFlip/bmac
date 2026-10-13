// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Reading the record store as a whole (`MONITORING.md`, "Logs"): what this
//! host holds of each origin and where its holes are, searching it, and
//! keeping it within its retention budget.

pub mod retention;
pub mod search;
pub mod task;

use crate::db::DbError;
use crate::store::Store;
use bmac_proto::logs::OriginHolding;
use bmac_proto::peer::StreamHolding;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;

/// Every stream this host holds records of, evicted ones included.
pub async fn holdings(store: &Store) -> Result<Vec<StreamHolding>, DbError> {
    store.database().run(stream_holdings).await
}

fn stream_holdings(c: &Connection) -> Result<Vec<StreamHolding>, rusqlite::Error> {
    let mut statement = c.prepare_cached(
        "SELECT origin, stream, MIN(seq), MAX(seq), COUNT(*), MIN(time_ms), MAX(time_ms), SUM(size)
         FROM records GROUP BY origin, stream ORDER BY origin, stream",
    )?;
    let rows = statement.query_map([], |r| {
        Ok(StreamHolding {
            origin: r.get(0)?,
            stream: r.get(1)?,
            min_seq: r.get::<_, i64>(2)? as u64,
            max_seq: r.get::<_, i64>(3)? as u64,
            count: r.get::<_, i64>(4)? as u64,
            oldest_ms: r.get(5)?,
            newest_ms: r.get(6)?,
            bytes: r.get::<_, i64>(7)? as u64,
        })
    })?;
    rows.collect()
}

/// The sequences of a stream from `lo` to `hi` this host lacks, as
/// inclusive ranges, leaving out what retention already deleted.
pub async fn missing(
    store: &Store,
    origin: &str,
    stream: &str,
    lo: u64,
    hi: u64,
) -> Result<Vec<(u64, u64)>, DbError> {
    let (origin, stream) = (origin.to_string(), stream.to_string());
    store
        .database()
        .run(move |c| missing_ranges(c, &origin, &stream, lo, hi))
        .await
}

fn missing_ranges(
    c: &Connection,
    origin: &str,
    stream: &str,
    lo: u64,
    hi: u64,
) -> Result<Vec<(u64, u64)>, rusqlite::Error> {
    let floor: i64 = c
        .query_row(
            "SELECT floor FROM streams WHERE origin = ?1 AND stream = ?2",
            params![origin, stream],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let lo = lo.max(floor as u64 + 1).max(1);
    if lo > hi {
        return Ok(Vec::new());
    }
    let (first, last): (Option<i64>, Option<i64>) = c.query_row(
        "SELECT MIN(seq), MAX(seq) FROM records
         WHERE origin = ?1 AND stream = ?2 AND seq BETWEEN ?3 AND ?4",
        params![origin, stream, lo as i64, hi as i64],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (Some(first), Some(last)) = (first, last) else {
        return Ok(vec![(lo, hi)]);
    };
    let mut out = Vec::new();
    if first as u64 > lo {
        out.push((lo, first as u64 - 1));
    }
    let mut statement = c.prepare_cached(
        "SELECT before, seq FROM (
             SELECT seq, LAG(seq) OVER (ORDER BY seq) AS before FROM records
             WHERE origin = ?1 AND stream = ?2 AND seq BETWEEN ?3 AND ?4)
         WHERE before IS NOT NULL AND seq > before + 1",
    )?;
    let holes = statement.query_map(params![origin, stream, lo as i64, hi as i64], |r| {
        Ok((
            r.get::<_, i64>(0)? as u64 + 1,
            r.get::<_, i64>(1)? as u64 - 1,
        ))
    })?;
    for hole in holes {
        out.push(hole?);
    }
    if (last as u64) < hi {
        out.push((last as u64 + 1, hi));
    }
    Ok(out)
}

/// What this host holds of each origin, for Settings, then Log replication.
pub async fn origin_holdings(store: &Store) -> Result<Vec<OriginHolding>, DbError> {
    store
        .database()
        .run(|c| {
            let mut out: BTreeMap<String, OriginHolding> = BTreeMap::new();
            for held in stream_holdings(c)? {
                let origin = out
                    .entry(held.origin.clone())
                    .or_insert_with(|| OriginHolding {
                        origin: held.origin.clone(),
                        streams: 0,
                        records: 0,
                        bytes: 0,
                        oldest_ms: None,
                        newest_ms: None,
                        latest_stream: None,
                        last_seq: 0,
                        gaps: 0,
                    });
                origin.streams += 1;
                origin.records += held.count;
                origin.bytes += held.bytes;
                origin.gaps += (held.max_seq - held.min_seq + 1).saturating_sub(held.count);
                origin.oldest_ms = Some(origin.oldest_ms.map_or(held.oldest_ms, |t| t.min(held.oldest_ms)));
                origin.newest_ms = Some(origin.newest_ms.map_or(held.newest_ms, |t| t.max(held.newest_ms)));
            }
            let mut latest = c.prepare_cached(
                "SELECT s.stream, COALESCE((SELECT MAX(seq) FROM records r
                     WHERE r.origin = s.origin AND r.stream = s.stream), 0)
                 FROM streams s WHERE s.origin = ?1 ORDER BY s.started_ms DESC, s.rowid DESC LIMIT 1",
            )?;
            for origin in out.values_mut() {
                if let Some((stream, seq)) = latest
                    .query_row([&origin.origin], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                    .optional()?
                {
                    origin.latest_stream = Some(stream);
                    origin.last_seq = seq as u64;
                }
            }
            Ok(out.into_values().collect())
        })
        .await
}

#[cfg(test)]
mod tests;
