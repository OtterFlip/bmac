// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Host-to-host streams on port 8444 (`ARCHITECTURE.md`, "Cluster-wide
//! view"; `SECURITY.md`, "Joins and enrollment"). Each host pulls a stream
//! from every peer: `GET /v1/stream?stream=<id>&after=<seq>` returns the
//! peer's records after that point, read from its store, then its new ones
//! live, as NDJSON. The receiver stores each record before asking for more,
//! so after a drop it resumes from what it actually stored.

use bmac_core::job::{Answer, JobEvent};
use bmac_core::{HostSlot, JobId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{JobSummary, Role};

pub const PEER_PROTOCOL: &str = "bmac-peer";
pub const PEER_PORT: u16 = 8444;
pub const PEER_VERSION_MIN: u32 = 1;
pub const PEER_VERSION_MAX: u32 = 1;

pub const JOIN_PATH: &str = "/v1/join";
pub const ENROLL_PATH: &str = "/v1/enroll";
pub const STREAM_PATH: &str = "/v1/stream";
pub const ANSWER_PATH: &str = "/v1/answer";
/// `GET`: what the peer holds of every origin's streams.
pub const HOLDINGS_PATH: &str = "/v1/holdings";
/// `GET ?origin=&stream=&after=&limit=`: one page of any origin's records.
pub const RECORDS_PATH: &str = "/v1/records";

/// A record from a host's record log, with its key and chained hash
/// (`MONITORING.md`, "Logs").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub origin: String,
    pub stream: String,
    pub seq: u64,
    pub time_ms: i64,
    /// `job_event`, `audit`, or `log`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_seq: Option<u64>,
    /// One line of JSON.
    pub body: String,
    /// SHA-256 over the previous record's hash and this record, lowercase
    /// hex.
    pub hash: String,
    /// The previous record's hash, so a copy can be checked by a host that
    /// doesn't hold the previous record (all zeros before the first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    /// Evicted for space on the sender: the body is empty and the hash is
    /// taken as given, so the record is never fetched again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub evicted: bool,
}

/// What a host holds of one stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamHolding {
    pub origin: String,
    pub stream: String,
    pub min_seq: u64,
    pub max_seq: u64,
    /// Records held, evicted ones included; fewer than
    /// `max_seq - min_seq + 1` means gaps.
    pub count: u64,
    pub oldest_ms: i64,
    pub newest_ms: i64,
    /// Body bytes held, before compression.
    pub bytes: u64,
}

/// The answer of `GET /v1/holdings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Holdings {
    pub host: HostSlot,
    pub streams: Vec<StreamHolding>,
}

impl Record {
    /// The job event it carries, if it is one.
    pub fn job_event(&self) -> Option<JobEvent> {
        if self.kind != "job_event" {
            return None;
        }
        Some(JobEvent {
            job: self.job?,
            seq: self.job_seq?,
            at_ms: self.time_ms,
            body: serde_json::from_str(&self.body).ok()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerFrame {
    /// Sent first: who is sending, and the stream its records continue. A
    /// stream other than the one asked for means the sender restarted.
    Hello {
        host: HostSlot,
        stream: String,
    },
    Record {
        record: Record,
    },
    /// A job was admitted on the sender, or changed there.
    Job {
        job: JobSummary,
    },
    Observation {
        collector: String,
        data: Value,
    },
    Keepalive,
}

/// `POST /v1/answer`: an answer given on the sender for a job running on
/// the receiver. The receiver checks the role again against its own view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerRequest {
    pub job: JobId,
    pub request_id: String,
    pub user: String,
    pub role: Role,
    pub answer: Answer,
}

/// `POST /v1/join`, from a new host with no node certificate yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinRequest {
    pub token: String,
    /// The slot named in its bundle.
    pub host: HostSlot,
    /// The host's SSH host key and HTTPS certificate fingerprint, shown to
    /// the administrator who approves the join.
    pub ssh_host_key: String,
    pub https_fingerprint: String,
}

/// `POST /v1/enroll`, from a new appliance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollRequest {
    pub token: String,
    pub name: String,
    /// The appliance's client certificate, PEM.
    pub certificate: String,
}

/// The only answer a valid join or enrollment token gets: a pending request
/// an administrator approves in the Dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub request_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip() {
        for body in [
            json!({"type": "hello", "host": "mox2", "stream": "18f-ab12"}),
            json!({"type": "record", "record": {"origin": "mox2", "stream": "18f-ab12", "seq": 7,
                   "time_ms": 5, "kind": "job_event", "job": "6f9619ff-8b86-4011-b42d-00c04fc964ff",
                   "job_seq": 3, "body": "{\"type\":\"warning\",\"message\":\"hm\"}", "hash": "00"}}),
            json!({"type": "record", "record": {"origin": "mox3", "stream": "s", "seq": 2,
                   "time_ms": 5, "kind": "audit", "body": "", "hash": "11", "prev": "00", "evicted": true}}),
            json!({"type": "keepalive"}),
        ] {
            let frame: PeerFrame = serde_json::from_value(body.clone()).unwrap();
            assert_eq!(serde_json::to_value(&frame).unwrap(), body);
        }
        let record: PeerFrame = serde_json::from_value(json!({"type": "record", "record": {
            "origin": "mox2", "stream": "s", "seq": 7, "time_ms": 5, "kind": "job_event",
            "job": "6f9619ff-8b86-4011-b42d-00c04fc964ff", "job_seq": 3,
            "body": "{\"type\":\"warning\",\"message\":\"hm\"}", "hash": "00"}}))
        .unwrap();
        let PeerFrame::Record { record } = record else {
            panic!()
        };
        assert_eq!(record.job_event().unwrap().seq, 3);
        let answer = json!({"job": "6f9619ff-8b86-4011-b42d-00c04fc964ff", "request_id": "r1",
                            "user": "alice@pve", "role": "operator", "answer": {"kind": "confirm", "confirmed": true}});
        let parsed: AnswerRequest = serde_json::from_value(answer.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), answer);
    }

    #[test]
    fn join_requests_refuse_extra_keys() {
        let join = json!({"token": "t", "host": "mox4", "ssh_host_key": "ssh-ed25519 AAAA", "https_fingerprint": "AB:CD"});
        assert!(serde_json::from_value::<JoinRequest>(join.clone()).is_ok());
        let mut extra = join;
        extra["approve"] = json!(true);
        assert!(serde_json::from_value::<JoinRequest>(extra).is_err());
    }
}
