// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Messages on `GET /api/v1/events` (`ARCHITECTURE.md`, "Live updates").
//! Snapshots come from the plain `GET` endpoints; the stream carries the
//! changes after them.

use bmac_core::HostSlot;
use bmac_core::job::JobEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::api::JobSummary;

pub const KEEPALIVE_SECONDS: u64 = 15;

/// One Server-Sent Event. `id` goes in the SSE `id:` line, so a reconnect
/// sends it back as `Last-Event-ID`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct StreamMessage {
    pub id: String,
    #[serde(flatten)]
    pub body: StreamBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamBody {
    /// Sent first: the stream's name for `POST /api/v1/events/watch`.
    Hello {
        stream: String,
    },
    /// The gap since `Last-Event-ID` is too old to replay, or this client
    /// fell behind: fetch fresh snapshots.
    Resnapshot {
        reason: String,
    },
    /// A host's stream came up or dropped.
    Host {
        host: HostSlot,
        reachable: bool,
        last_seen_ms: i64,
    },
    Quorum {
        quorate: bool,
    },
    /// A collector observation changed. The shapes are defined with the
    /// collectors (M3), so they travel as JSON here.
    Observation {
        host: HostSlot,
        collector: String,
        data: Value,
    },
    /// A job was admitted or changed state, or its pending request changed.
    Job {
        job: JobSummary,
    },
    /// An event of a watched job.
    JobEvent {
        event: JobEvent,
    },
    /// An alert opened, changed, or closed (M5).
    Alert {
        alert: Value,
    },
    /// The last message: the session ended, so log in again.
    SessionEnded {
        message: String,
    },
}

/// Formats a message as one SSE frame.
pub fn sse_frame(message: &StreamMessage) -> String {
    let data = serde_json::to_string(message).expect("stream messages serialize");
    format!("id: {}\ndata: {data}\n\n", message.id)
}

/// The keep-alive comment.
pub const SSE_KEEPALIVE: &str = ": keep-alive\n\n";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_carry_the_id_twice() {
        let message = StreamMessage {
            id: "42".into(),
            body: StreamBody::Quorum { quorate: false },
        };
        assert_eq!(
            sse_frame(&message),
            "id: 42\ndata: {\"id\":\"42\",\"type\":\"quorum\",\"quorate\":false}\n\n"
        );
        let back: StreamMessage =
            serde_json::from_value(json!({"id": "7", "type": "resnapshot", "reason": "gap"}))
                .unwrap();
        assert!(matches!(back.body, StreamBody::Resnapshot { .. }));
    }
}
