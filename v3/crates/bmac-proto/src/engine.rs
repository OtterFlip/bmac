// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The front end to engine socket, `/run/bmac/engine.sock`. The front end
//! (`bmac-web`, unprivileged) can send only these calls; the engine (root)
//! checks the session token on every one, so a compromised front end can do
//! no more than the logged-in users it relays for. Frames are one JSON
//! object per line.

use std::net::IpAddr;

use bmac_core::job::Answer;
use bmac_core::{HostSlot, JobId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::alerts::{SaveNotifySettings, SaveSilence, SendTestEmail, SetPostmarkToken};
use crate::api::{ApiError, CancelRequest, LaunchJob, LoginRequest, LogoutRequest, WatchRequest};
use crate::certificate::SwitchCertificate;
use crate::joins::IssueJoin;
use crate::logs::LogQuery;
use crate::settings::SaveSettings;
use crate::stream::StreamMessage;

pub const ENGINE_SOCKET: &str = "/run/bmac/engine.sock";
/// The largest request line the engine reads; the API's own body limit is
/// smaller.
pub const MAX_REQUEST_FRAME: usize = 1024 * 1024;
/// The largest reply line the front end reads: a job result (at most
/// 8 MiB) plus its framing.
pub const MAX_REPLY_FRAME: usize = 9 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineRequest {
    /// Chosen by the front end; the reply carries it back.
    pub id: u64,
    /// The browser's address, for rate limits and audit.
    pub client: IpAddr,
    /// The bearer token, absent only for `login`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub call: EngineCall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Collection {
    Hosts,
    Vms,
    Storage,
    Backups,
    Alerts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case", deny_unknown_fields)]
pub enum EngineCall {
    /// The front end's first call: the reply is a [`Hello`], so a front end
    /// left running across an upgrade notices the new engine.
    Hello {},
    Login {
        request: LoginRequest,
    },
    Logout {
        request: LogoutRequest,
    },
    Cluster {},
    List {
        collection: Collection,
    },
    Operations {},
    LaunchJob {
        request: LaunchJob,
    },
    Jobs {},
    Job {
        job: JobId,
        from_seq: u64,
    },
    Answer {
        job: JobId,
        request_id: String,
        answer: Answer,
    },
    Cancel {
        job: JobId,
        request: CancelRequest,
    },
    /// Restarts an interrupted job that can continue from its checkpoint.
    Resume {
        job: JobId,
    },
    /// Marks an interrupted job failed once someone has checked the live
    /// state, releasing its resources.
    Reconcile {
        job: JobId,
    },
    /// Opens the event stream; replies follow as `event` outcomes until the
    /// front end closes the connection or the session ends.
    Subscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_event_id: Option<String>,
    },
    Watch {
        request: WatchRequest,
    },
    Settings {},
    SaveClusterSettings {
        request: SaveSettings,
    },
    SaveHostFacts {
        host: HostSlot,
        request: SaveSettings,
    },
    Certificate {},
    RotateCertificate {},
    SwitchCertificate {
        request: SwitchCertificate,
    },
    DiscardCertificate {},
    Alerts {},
    AcknowledgeAlert {
        id: String,
    },
    SaveSilence {
        request: SaveSilence,
    },
    DeleteSilence {
        id: String,
    },
    SaveNotifySettings {
        request: SaveNotifySettings,
    },
    SetPostmarkToken {
        request: SetPostmarkToken,
    },
    SendTestEmail {
        request: SendTestEmail,
    },
    EmailTemplates {},
    CreateEmailTemplates {},
    SearchLogs {
        query: LogQuery,
    },
    LogReplication {},
    Setup {},
    Joins {},
    IssueJoin {
        request: IssueJoin,
    },
    RevokeJoin {
        host: HostSlot,
    },
    Appliances {},
    IssueEnroll {
        request: crate::backups::IssueEnroll,
    },
    ApproveAppliance {
        request_id: String,
        request: crate::backups::ApproveAppliance,
    },
    RejectAppliance {
        request_id: String,
    },
    RevokeAppliance {
        appliance_uuid: uuid::Uuid,
    },
    SetApplianceAddress {
        appliance_uuid: uuid::Uuid,
        request: crate::backups::SetApplianceAddress,
    },
    BackupPolicies {},
    /// The policy as the Dashboard edits it, checked by the engine.
    SaveBackupPolicy {
        request: serde_json::Value,
    },
    DeleteBackupPolicy {
        vm_uuid: uuid::Uuid,
        revision: u64,
    },
}

impl EngineCall {
    /// Whether the call may be made without a session.
    pub fn is_anonymous(&self) -> bool {
        matches!(self, EngineCall::Hello {} | EngineCall::Login { .. })
    }

    /// Whether the engine answers with a stream of `event` outcomes rather
    /// than one reply.
    pub fn is_stream(&self) -> bool {
        matches!(self, EngineCall::Subscribe { .. })
    }

    pub fn name(&self) -> &'static str {
        match self {
            EngineCall::Hello {} => "hello",
            EngineCall::Login { .. } => "login",
            EngineCall::Logout { .. } => "logout",
            EngineCall::Cluster {} => "cluster",
            EngineCall::List { .. } => "list",
            EngineCall::Operations {} => "operations",
            EngineCall::LaunchJob { .. } => "launch_job",
            EngineCall::Jobs {} => "jobs",
            EngineCall::Job { .. } => "job",
            EngineCall::Answer { .. } => "answer",
            EngineCall::Cancel { .. } => "cancel",
            EngineCall::Resume { .. } => "resume",
            EngineCall::Reconcile { .. } => "reconcile",
            EngineCall::Subscribe { .. } => "subscribe",
            EngineCall::Watch { .. } => "watch",
            EngineCall::Settings {} => "settings",
            EngineCall::SaveClusterSettings { .. } => "save_cluster_settings",
            EngineCall::SaveHostFacts { .. } => "save_host_facts",
            EngineCall::Certificate {} => "certificate",
            EngineCall::RotateCertificate {} => "rotate_certificate",
            EngineCall::SwitchCertificate { .. } => "switch_certificate",
            EngineCall::DiscardCertificate {} => "discard_certificate",
            EngineCall::Alerts {} => "alerts",
            EngineCall::AcknowledgeAlert { .. } => "acknowledge_alert",
            EngineCall::SaveSilence { .. } => "save_silence",
            EngineCall::DeleteSilence { .. } => "delete_silence",
            EngineCall::SaveNotifySettings { .. } => "save_notify_settings",
            EngineCall::SetPostmarkToken { .. } => "set_postmark_token",
            EngineCall::SendTestEmail { .. } => "send_test_email",
            EngineCall::EmailTemplates {} => "email_templates",
            EngineCall::CreateEmailTemplates {} => "create_email_templates",
            EngineCall::SearchLogs { .. } => "search_logs",
            EngineCall::LogReplication {} => "log_replication",
            EngineCall::Setup {} => "setup",
            EngineCall::Joins {} => "joins",
            EngineCall::IssueJoin { .. } => "issue_join",
            EngineCall::RevokeJoin { .. } => "revoke_join",
            EngineCall::Appliances {} => "appliances",
            EngineCall::IssueEnroll { .. } => "issue_enroll",
            EngineCall::ApproveAppliance { .. } => "approve_appliance",
            EngineCall::RejectAppliance { .. } => "reject_appliance",
            EngineCall::RevokeAppliance { .. } => "revoke_appliance",
            EngineCall::SetApplianceAddress { .. } => "set_appliance_address",
            EngineCall::BackupPolicies {} => "backup_policies",
            EngineCall::SaveBackupPolicy { .. } => "save_backup_policy",
            EngineCall::DeleteBackupPolicy { .. } => "delete_backup_policy",
        }
    }
}

/// The body of the reply to [`EngineCall::Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: String,
    /// Where the front end listens: this host's Tailscale and private
    /// addresses on the Dashboard port, never a public or wildcard address.
    #[serde(default)]
    pub listen: Vec<std::net::SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineReply {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: EngineOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum EngineOutcome {
    /// The API response body for the call.
    Ok {
        body: Value,
    },
    Err {
        error: ApiError,
    },
    Event {
        message: StreamMessage,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_call_round_trips() {
        let job = "6f9619ff-8b86-4011-b42d-00c04fc964ff";
        let calls = [
            json!({"name": "hello"}),
            json!({"name": "login", "request": {"step": "password", "user": "alice@pve", "password": "pw"}}),
            json!({"name": "logout", "request": {"everywhere": true}}),
            json!({"name": "cluster"}),
            json!({"name": "list", "collection": "vms"}),
            json!({"name": "operations"}),
            json!({"name": "launch_job", "request": {"operation": "prod.add", "parameters": {}, "idempotency_key": "k", "dry_run": true}}),
            json!({"name": "jobs"}),
            json!({"name": "job", "job": job, "from_seq": 5}),
            json!({"name": "answer", "job": job, "request_id": "r1", "answer": {"kind": "acknowledge"}}),
            json!({"name": "cancel", "job": job, "request": {"force": false}}),
            json!({"name": "resume", "job": job}),
            json!({"name": "reconcile", "job": job}),
            json!({"name": "subscribe", "last_event_id": "41"}),
            json!({"name": "watch", "request": {"stream": "s1", "jobs": [job]}}),
            json!({"name": "settings"}),
            json!({"name": "save_cluster_settings", "request": {"values": {"MAX_MOX_HOSTS": "3"}, "expected_revision": 2}}),
            json!({"name": "save_host_facts", "host": "mox2", "request": {"values": {}, "expected_revision": null}}),
            json!({"name": "certificate"}),
            json!({"name": "rotate_certificate"}),
            json!({"name": "switch_certificate", "request": {"fingerprint": "AB:CD"}}),
            json!({"name": "discard_certificate"}),
            json!({"name": "alerts"}),
            json!({"name": "acknowledge_alert", "id": "a1"}),
            json!({"name": "save_silence", "request": {"condition": "zfs.", "resource": null, "host": "mox2", "until_ms": 5, "reason": "disk swap"}}),
            json!({"name": "delete_silence", "id": "s1"}),
            json!({"name": "save_notify_settings", "request": {"settings": {"sender": "bmac@example.com", "recipients": ["ops@example.com"], "routes": [], "throttle_minutes": 60, "digest_hour": 7, "dashboard_url": null, "disabled_conditions": []}, "expected_revision": null}}),
            json!({"name": "set_postmark_token", "request": {"token": "t"}}),
            json!({"name": "search_logs", "query": {"host": "mox2", "job": null, "kinds": ["journal"], "min_severity": "warning", "unit": null, "text": "zfs", "from_ms": 1, "to_ms": null, "before": null, "limit": 50}}),
            json!({"name": "log_replication"}),
            json!({"name": "send_test_email", "request": {"to": null}}),
            json!({"name": "email_templates"}),
            json!({"name": "create_email_templates"}),
            json!({"name": "setup"}),
            json!({"name": "joins"}),
            json!({"name": "issue_join", "request": {"host": "mox2", "max_prod_vms": 2, "max_staging_vms": 1}}),
            json!({"name": "revoke_join", "host": "mox2"}),
        ];
        for call in calls {
            let request = json!({"id": 1, "client": "100.64.0.9", "token": "t", "call": call});
            let parsed: EngineRequest = serde_json::from_value(request.clone()).unwrap();
            assert_eq!(serde_json::to_value(&parsed).unwrap(), request);
            assert_eq!(parsed.call.name(), request["call"]["name"]);
        }
    }

    #[test]
    fn only_listed_calls_exist() {
        let shell =
            json!({"id": 1, "client": "127.0.0.1", "call": {"name": "run", "argv": ["/bin/sh"]}});
        assert!(serde_json::from_value::<EngineRequest>(shell).is_err());
        let extra = json!({"id": 1, "client": "127.0.0.1", "call": {"name": "cluster", "path": "/etc/pve"}});
        assert!(serde_json::from_value::<EngineRequest>(extra).is_err());
        assert!(
            EngineCall::Login {
                request: LoginRequest::Password {
                    user: "u".into(),
                    password: "p".into()
                }
            }
            .is_anonymous()
        );
        assert!(!EngineCall::Cluster {}.is_anonymous());
    }
}
