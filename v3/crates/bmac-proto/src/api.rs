// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The JSON API under `/api/v1/` (`ARCHITECTURE.md`, "API") and the login
//! exchange (`SECURITY.md`, "Login"). The token lives only in the page's
//! memory; it never goes in a cookie, browser storage, or a URL.

use std::collections::BTreeMap;

use bmac_core::job::{Answer, JobEvent, JobState};
use bmac_core::{HostSlot, JobId, OperationId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

pub const API_PREFIX: &str = "/api/v1";

/// A BMAC role, from the Proxmox groups `bmac-viewer`, `bmac-operator`, and
/// `bmac-admin`; `root@pam` is an administrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum SecondFactor {
    Totp,
    Recovery,
    Webauthn,
}

/// `POST /api/v1/login`: a password first, then the answer to the second
/// factor challenge Proxmox returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum LoginRequest {
    Password {
        user: String,
        password: String,
    },
    SecondFactor {
        challenge: String,
        method: SecondFactor,
        response: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum LoginResponse {
    /// The user has a second factor; answer the challenge with one of these.
    SecondFactor {
        challenge: String,
        methods: Vec<SecondFactor>,
    },
    Session(Session),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct Session {
    /// 256 random bits, base64url. Sent as `Authorization: Bearer`.
    pub token: String,
    pub user: String,
    pub role: Role,
    pub idle_timeout_seconds: u64,
    pub expires_at_ms: i64,
}

/// `POST /api/v1/logout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(deny_unknown_fields)]
pub struct LogoutRequest {
    /// End every session of this user, on every host.
    #[serde(default)]
    pub everywhere: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No token, or it expired or was revoked: log in again.
    Unauthenticated,
    /// The user's role doesn't allow this.
    Forbidden,
    NotFound,
    /// The body or a parameter doesn't fit.
    Invalid,
    /// Another job holds a resource this one needs.
    Conflict,
    RateLimited,
    /// This host's partition has no quorum, so nothing can change.
    NoQuorum,
    /// The engine or a peer can't be reached right now.
    Unavailable,
    Internal,
}

impl ErrorCode {
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::Unauthenticated => 401,
            ErrorCode::Forbidden => 403,
            ErrorCode::NotFound => 404,
            ErrorCode::Invalid => 422,
            ErrorCode::Conflict => 409,
            ErrorCode::RateLimited => 429,
            ErrorCode::NoQuorum | ErrorCode::Unavailable => 503,
            ErrorCode::Internal => 500,
        }
    }
}

/// Every error body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error, TS)]
#[ts(optional_fields)]
#[error("{message}")]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    /// Per-field messages for an `invalid` body.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[ts(optional = nullable)]
    pub field_errors: BTreeMap<String, String>,
    /// Seconds to wait, for `rate_limited`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ApiError {
        ApiError {
            code,
            message: message.into(),
            field_errors: BTreeMap::new(),
            retry_after_seconds: None,
        }
    }
}

/// `POST /api/v1/jobs`. The web app makes the idempotency key when it opens
/// the launch dialog, so a repeated POST returns the job already admitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(deny_unknown_fields)]
pub struct LaunchJob {
    pub operation: OperationId,
    #[serde(default)]
    pub parameters: BTreeMap<String, Value>,
    pub idempotency_key: String,
    #[serde(default)]
    pub dry_run: bool,
}

/// A job as lists and the event stream show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct JobSummary {
    pub id: JobId,
    pub operation: OperationId,
    pub title: String,
    pub state: JobState,
    /// The host it runs on.
    pub host: HostSlot,
    pub user: String,
    #[serde(default)]
    pub dry_run: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// The request waiting for an answer, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_request: Option<String>,
    /// Why it is in its current state, such as why it was interrupted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// A cancel was requested and the job hasn't stopped yet.
    #[serde(default)]
    pub cancel_requested: bool,
    /// The last sequence number recorded.
    pub last_seq: u64,
}

/// `GET /api/v1/jobs`: every unfinished job and the most recent finished ones,
/// newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct JobList {
    pub jobs: Vec<JobSummary>,
}

/// `GET /api/v1/jobs/{id}?from_seq=N`: the record and its events from `N`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct JobView {
    pub job: JobSummary,
    /// The launch parameters as text.
    pub parameters: BTreeMap<String, String>,
    /// The resources it holds until it finishes.
    pub locks: Vec<String>,
    pub events: Vec<JobEvent>,
    /// Whether events after these exist; fetch again from the last one.
    pub more: bool,
}

/// `GET /api/v1/setup`: whether this host is still being set up
/// (`DEPLOYMENT.md`, "Host setup job"). While it is, the API offers only
/// the setup job, and the Dashboard shows only its page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct SetupView {
    pub active: bool,
    /// The operation that sets the host up; launch it to start or redo it.
    pub operation: OperationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostSlot>,
    /// It creates a new cluster rather than joining one.
    #[serde(default)]
    pub new_cluster: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_name: Option<String>,
    /// The newest setup job, if one was launched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobSummary>,
    /// On a joining host, how its join request stands. Its setup job runs
    /// from the existing host that approves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<JoinProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum JoinState {
    /// Presenting the join token to the hosts in the bundle.
    Requesting,
    /// An existing host took the token; an administrator approves there.
    Waiting,
    /// Every host refused the token.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct JoinProgress {
    pub state: JoinState,
    /// The host that took the request, where it is approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostSlot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Why the last attempt failed, or who is setting the host up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub updated_at_ms: i64,
}

/// `POST /api/v1/jobs/{id}/requests/{rid}`.
pub type AnswerRequest = Answer;

/// `POST /api/v1/jobs/{id}/cancel`. A soft cancel answers a pending request
/// with a cancel or interrupts the script; a forced one terminates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    #[serde(default)]
    pub force: bool,
}

/// `POST /api/v1/events/watch`: the jobs whose events this tab's stream
/// carries. Replaces the previous set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(deny_unknown_fields)]
pub struct WatchRequest {
    pub stream: String,
    pub jobs: Vec<JobId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn login_steps_are_tagged() {
        let password: LoginRequest = serde_json::from_value(
            json!({"step": "password", "user": "alice@pve", "password": "pw"}),
        )
        .unwrap();
        assert!(matches!(password, LoginRequest::Password { .. }));
        let tfa: LoginRequest = serde_json::from_value(
            json!({"step": "second_factor", "challenge": "c1", "method": "totp", "response": "123456"}),
        )
        .unwrap();
        assert!(matches!(
            tfa,
            LoginRequest::SecondFactor {
                method: SecondFactor::Totp,
                ..
            }
        ));
        assert!(
            serde_json::from_value::<LoginRequest>(
                json!({"step": "password", "user": "a", "password": "p", "remember": true})
            )
            .is_err(),
            "no remember-me: the client logs in every time"
        );
        let session = LoginResponse::Session(Session {
            token: "t".into(),
            user: "alice@pve".into(),
            role: Role::Operator,
            idle_timeout_seconds: 900,
            expires_at_ms: 1,
        });
        assert_eq!(serde_json::to_value(&session).unwrap()["step"], "session");
    }

    #[test]
    fn roles_are_ordered_by_privilege() {
        assert!(Role::Viewer < Role::Operator && Role::Operator < Role::Admin);
    }

    #[test]
    fn errors_carry_a_status() {
        let error = ApiError::new(ErrorCode::NoQuorum, "no quorum");
        assert_eq!(error.code.http_status(), 503);
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({"code": "no_quorum", "message": "no quorum"})
        );
    }

    #[test]
    fn launches_refuse_unknown_keys() {
        let launch = json!({"operation": "add_prod_vm", "parameters": {"cores": 4}, "idempotency_key": "k1"});
        let parsed: LaunchJob = serde_json::from_value(launch.clone()).unwrap();
        assert!(!parsed.dry_run);
        let mut extra = launch;
        extra["argv"] = json!(["--force"]);
        assert!(serde_json::from_value::<LaunchJob>(extra).is_err());
    }
}
