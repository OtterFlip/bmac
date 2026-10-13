// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Logs page's search: every stored record, this host's and its copies
//! of the others', newest first, a page at a time.

use crate::db::DbError;
use crate::store::{Key, RecordKind, Store, StoreError, read_frame};
use bmac_proto::logs::{LogKind, LogPage, LogQuery, LogRecord, LogSeverity, MAX_LOG_PAGE};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params_from_iter};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

/// The most rows one page reads before answering with what it has: a text
/// search over compacted output can't be filtered in SQL.
const SCAN_ROWS: usize = 20_000;
const BATCH: usize = 500;
const SUMMARY_CHARS: usize = 240;

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("the paging cursor is malformed")]
    Cursor,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<DbError> for SearchError {
    fn from(e: DbError) -> Self {
        SearchError::Store(e.into())
    }
}

impl From<rusqlite::Error> for SearchError {
    fn from(e: rusqlite::Error) -> Self {
        SearchError::Store(DbError::from(e).into())
    }
}

/// Where a page ended: the key of its last record, in the page's order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    time_ms: i64,
    origin: String,
    stream: String,
    seq: i64,
}

impl Cursor {
    fn encode(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.time_ms, self.seq, self.origin, self.stream
        )
    }

    fn decode(text: &str) -> Option<Cursor> {
        let mut parts = text.splitn(4, ':');
        Some(Cursor {
            time_ms: parts.next()?.parse().ok()?,
            seq: parts.next()?.parse().ok()?,
            origin: parts.next()?.to_string(),
            stream: parts.next()?.to_string(),
        })
    }
}

pub async fn search(store: &Store, query: LogQuery) -> Result<LogPage, SearchError> {
    let segments = store.segments().to_path_buf();
    store
        .database()
        .run(move |c| Ok(run(c, &segments, &query)))
        .await?
}

fn kind_name(kind: LogKind) -> &'static str {
    match kind {
        LogKind::JobEvent => RecordKind::JobEvent.as_str(),
        LogKind::Audit => RecordKind::Audit.as_str(),
        LogKind::Log => RecordKind::Log.as_str(),
        LogKind::Journal => RecordKind::Journal.as_str(),
    }
}

fn severity_level(severity: LogSeverity) -> i64 {
    match severity {
        LogSeverity::Info => 0,
        LogSeverity::Warning => 1,
        LogSeverity::Error => 2,
    }
}

fn run(c: &Connection, segments: &Path, query: &LogQuery) -> Result<LogPage, SearchError> {
    let limit = query.limit.unwrap_or(100).clamp(1, MAX_LOG_PAGE) as usize;
    let mut filters = Vec::new();
    let mut args: Vec<SqlValue> = Vec::new();
    if let Some(host) = &query.host {
        filters.push("origin = ?".to_string());
        args.push(host.to_string().into());
    }
    if let Some(job) = &query.job {
        filters.push("job = ?".to_string());
        args.push(job.to_string().into());
    }
    if !query.kinds.is_empty() {
        filters.push(format!(
            "kind IN ({})",
            vec!["?"; query.kinds.len()].join(", ")
        ));
        args.extend(
            query
                .kinds
                .iter()
                .map(|k| SqlValue::from(kind_name(*k).to_string())),
        );
    }
    if let Some(severity) = query.min_severity {
        filters.push("severity >= ?".to_string());
        args.push(severity_level(severity).into());
    }
    if let Some(unit) = &query.unit {
        filters.push(
            "kind = 'journal' AND evicted = 0 AND json_valid(body) AND json_extract(body, '$.unit') = ?"
                .to_string(),
        );
        args.push(unit.clone().into());
    }
    if let Some(from) = query.from_ms {
        filters.push("time_ms >= ?".to_string());
        args.push(from.into());
    }
    if let Some(to) = query.to_ms {
        filters.push("time_ms <= ?".to_string());
        args.push(to.into());
    }
    let needle = query
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase);
    if let Some(needle) = &needle {
        filters.push("((body IS NOT NULL AND instr(lower(body), ?) > 0) OR (body IS NULL AND segment IS NOT NULL))".to_string());
        args.push(needle.clone().into());
    }
    let mut cursor = match &query.before {
        Some(text) => Some(Cursor::decode(text).ok_or(SearchError::Cursor)?),
        None => None,
    };

    let mut records = Vec::new();
    let mut frames: HashMap<i64, HashMap<Key, String>> = HashMap::new();
    let mut scanned = 0;
    let mut exhausted = false;
    while records.len() < limit && scanned < SCAN_ROWS {
        let mut sql = String::from(
            "SELECT origin, stream, seq, time_ms, kind, job, body, segment, severity, evicted
             FROM records",
        );
        let mut where_ = filters.clone();
        let mut batch_args = args.clone();
        if let Some(cursor) = &cursor {
            where_.push("(time_ms, origin, stream, seq) < (?, ?, ?, ?)".to_string());
            batch_args.extend([
                cursor.time_ms.into(),
                cursor.origin.clone().into(),
                cursor.stream.clone().into(),
                cursor.seq.into(),
            ]);
        }
        if !where_.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_.join(" AND "));
        }
        sql.push_str(&format!(
            " ORDER BY time_ms DESC, origin DESC, stream DESC, seq DESC LIMIT {BATCH}"
        ));
        let mut statement = c.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(batch_args))?;
        let mut read = 0;
        while let Some(row) = rows.next()? {
            read += 1;
            scanned += 1;
            let key = Cursor {
                time_ms: row.get(3)?,
                origin: row.get(0)?,
                stream: row.get(1)?,
                seq: row.get(2)?,
            };
            let body: Option<String> = row.get(6)?;
            let body = match body {
                Some(body) => Some(body),
                None => match row.get::<_, Option<i64>>(7)? {
                    Some(segment) => {
                        if !frames.contains_key(&segment) {
                            if frames.len() > 8 {
                                frames.clear();
                            }
                            frames.insert(segment, read_frame(c, segments, segment)?);
                        }
                        frames[&segment]
                            .get(&(key.origin.clone(), key.stream.clone(), key.seq))
                            .cloned()
                    }
                    None => None,
                },
            };
            let keep = match (&needle, &body) {
                (Some(needle), Some(body)) => body.to_lowercase().contains(needle.as_str()),
                (Some(_), None) => false,
                (None, _) => true,
            };
            if keep {
                let kind: String = row.get(4)?;
                let job: Option<String> = row.get(5)?;
                records.push(record(
                    &key,
                    &kind,
                    job.and_then(|j| j.parse().ok()),
                    row.get(8)?,
                    row.get(9)?,
                    body.as_deref().unwrap_or_default(),
                ));
            }
            cursor = Some(key);
            if records.len() >= limit || scanned >= SCAN_ROWS {
                break;
            }
        }
        if read < BATCH && records.len() < limit && scanned < SCAN_ROWS {
            exhausted = true;
            break;
        }
    }
    let next = if exhausted {
        None
    } else {
        cursor.map(|c| c.encode())
    };
    Ok(LogPage { records, next })
}

fn record(
    key: &Cursor,
    kind: &str,
    job: Option<bmac_core::JobId>,
    severity: i64,
    evicted: bool,
    body: &str,
) -> LogRecord {
    let kind = match RecordKind::parse(kind) {
        Some(RecordKind::Audit) => LogKind::Audit,
        Some(RecordKind::Log) => LogKind::Log,
        Some(RecordKind::Journal) => LogKind::Journal,
        _ => LogKind::JobEvent,
    };
    let body: Value = if evicted {
        Value::Null
    } else {
        serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_string()))
    };
    let text = |key: &str| body.get(key).and_then(Value::as_str);
    let unit = match kind {
        LogKind::Journal => text("unit").map(String::from),
        _ => None,
    };
    let summary = if evicted {
        "(evicted for space)".to_string()
    } else {
        match kind {
            LogKind::JobEvent => {
                let etype = text("type").unwrap_or("event");
                match text("text")
                    .or(text("message"))
                    .or(text("name"))
                    .or(text("title"))
                {
                    Some(detail) => format!("{etype}: {detail}"),
                    None => etype.to_string(),
                }
            }
            LogKind::Audit => {
                let who = text("user").map(|u| format!(" by {u}")).unwrap_or_default();
                format!(
                    "{}{who}: {}",
                    text("kind").unwrap_or("audit"),
                    text("message").unwrap_or_default()
                )
            }
            LogKind::Log => text("message").unwrap_or_default().to_string(),
            LogKind::Journal => format!(
                "{}: {}",
                text("unit").unwrap_or("journal"),
                text("message").unwrap_or_default()
            ),
        }
    };
    let summary = match summary.char_indices().nth(SUMMARY_CHARS) {
        Some((at, _)) => format!("{}…", &summary[..at]),
        None => summary,
    };
    LogRecord {
        origin: key.origin.clone(),
        stream: key.stream.clone(),
        seq: key.seq as u64,
        time_ms: key.time_ms,
        kind,
        severity: match severity {
            2 => LogSeverity::Error,
            1 => LogSeverity::Warning,
            _ => LogSeverity::Info,
        },
        job,
        unit,
        summary,
        body,
        evicted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_with_colons_in_the_stream() {
        let cursor = Cursor {
            time_ms: 5,
            origin: "mox1".into(),
            stream: "a:b".into(),
            seq: 9,
        };
        assert_eq!(Cursor::decode(&cursor.encode()), Some(cursor));
        assert_eq!(Cursor::decode("x:1:a:b"), None);
    }
}
