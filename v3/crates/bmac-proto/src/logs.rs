// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Logs page and Settings, then Log replication (`MONITORING.md`,
//! "Logs"): searching every stored record, from this host and its copies of
//! the others, and how far copying has got.

use bmac_core::{HostSlot, JobId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

/// The most records one search page returns.
pub const MAX_LOG_PAGE: u32 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum LogKind {
    JobEvent,
    Audit,
    /// `bmacd`'s own log.
    Log,
    /// The journal collector's entries.
    Journal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum LogSeverity {
    Info,
    Warning,
    Error,
}

/// The body of `POST /api/v1/logs/search`. Every filter is optional; the
/// newest records come first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields)]
pub struct LogQuery {
    /// The host the records came from.
    #[serde(default)]
    pub host: Option<HostSlot>,
    #[serde(default)]
    pub job: Option<JobId>,
    /// Empty means every kind.
    #[serde(default)]
    pub kinds: Vec<LogKind>,
    #[serde(default)]
    pub min_severity: Option<LogSeverity>,
    /// A systemd unit, for journal records.
    #[serde(default)]
    pub unit: Option<String>,
    /// Text the record contains, case-insensitively.
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub from_ms: Option<i64>,
    #[serde(default)]
    pub to_ms: Option<i64>,
    /// The `next` of the previous page.
    #[serde(default)]
    pub before: Option<String>,
    /// At most [`MAX_LOG_PAGE`]; 100 when absent.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct LogRecord {
    pub origin: String,
    pub stream: String,
    pub seq: u64,
    pub time_ms: i64,
    pub kind: LogKind,
    pub severity: LogSeverity,
    pub job: Option<JobId>,
    pub unit: Option<String>,
    /// One line for the list.
    pub summary: String,
    #[ts(type = "unknown")]
    pub body: Value,
    /// Evicted for space: only the key and time are left.
    pub evicted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct LogPage {
    pub records: Vec<LogRecord>,
    /// Pass as `before` for the next, older page; absent at the end.
    pub next: Option<String>,
}

/// What this host holds of one origin's records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct OriginHolding {
    pub origin: String,
    pub streams: u32,
    pub records: u64,
    pub bytes: u64,
    pub oldest_ms: Option<i64>,
    pub newest_ms: Option<i64>,
    /// The origin's newest stream this host knows, and its last sequence.
    pub latest_stream: Option<String>,
    pub last_seq: u64,
    /// Sequences missing inside the streams held.
    pub gaps: u64,
}

/// How one peer's copy of an origin compares with this host's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PeerOrigin {
    pub origin: String,
    /// The peer's last sequence of the origin's newest stream this host
    /// knows; 0 if it holds none of it.
    pub last_seq: u64,
    /// Records this host holds of that stream beyond the peer's; negative
    /// when the peer is ahead.
    pub behind_records: i64,
    /// How old the oldest of those records is, if any.
    pub behind_ms: Option<i64>,
    pub gaps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PeerReplication {
    pub peer: HostSlot,
    /// When this host last read the peer's holdings.
    pub checked_ms: Option<i64>,
    pub error: Option<String>,
    pub origins: Vec<PeerOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RetentionView {
    pub keep_days: u32,
    pub budget_bytes: u64,
    pub used_bytes: u64,
    /// When audit events were last evicted for space, if ever.
    pub audit_evicted_ms: Option<i64>,
}

/// The answer of `GET /api/v1/logs/replication`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ReplicationView {
    /// The host answering: the view is its store and what it knows of its
    /// peers.
    pub host: Option<HostSlot>,
    pub origins: Vec<OriginHolding>,
    pub peers: Vec<PeerReplication>,
    pub retention: RetentionView,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn queries_refuse_unknown_filters_and_default_the_rest() {
        let query: LogQuery =
            serde_json::from_value(json!({"kinds": ["journal"], "min_severity": "warning"}))
                .unwrap();
        assert_eq!(query.kinds, vec![LogKind::Journal]);
        assert_eq!(query.limit, None);
        assert!(serde_json::from_value::<LogQuery>(json!({"grep": "x"})).is_err());
    }
}
