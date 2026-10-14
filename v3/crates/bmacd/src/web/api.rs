// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The JSON API under `/api/v1/` (`ARCHITECTURE.md`, "API"). Each endpoint
//! checks the request's shape, then relays one typed call to the engine,
//! which checks the session and the role. The front end reads the token
//! only to pass it on.

use std::convert::Infallible;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use bmac_core::job::Answer;
use bmac_core::{HostSlot, JobId};
use bmac_proto::alerts::{SaveNotifySettings, SaveSilence, SendTestEmail, SetPostmarkToken};
use bmac_proto::api::{
    ApiError, CancelRequest, ErrorCode, LaunchJob, LoginRequest, LogoutRequest, WatchRequest,
};
use bmac_proto::certificate::SwitchCertificate;
use bmac_proto::engine::{Collection, EngineCall, EngineOutcome};
use bmac_proto::settings::SaveSettings;
use bmac_proto::stream::{SSE_KEEPALIVE, StreamBody, sse_frame};
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;

use super::app::AppState;
use super::client::EngineStream;
use super::https::ClientAddr;
use crate::auth::sessions::is_token_shaped;

/// SSE frames queued for one tab before the relay waits for it.
const SSE_QUEUE: usize = 64;
const MAX_LAST_EVENT_ID: usize = 64;

/// An API error body, never cached.
pub fn api_error(error: ApiError) -> Response {
    let status =
        StatusCode::from_u16(error.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let retry_after = error.retry_after_seconds;
    let mut response = (status, axum::Json(error)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(seconds) = retry_after {
        headers.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}

fn api_ok(body: serde_json::Value) -> Response {
    let mut response = (StatusCode::OK, axum::Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}

/// The host the browser asked for: `Host` in HTTP/1.1, `:authority` in
/// HTTP/2.
fn request_host<'a>(headers: &'a HeaderMap, uri: &'a Uri) -> Option<&'a str> {
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| uri.authority().map(|a| a.as_str()))
}

/// A browser's `Origin`, when sent, must be this site. Clients that send
/// none (not browsers) are allowed; the token is what authorizes them.
fn check_origin(headers: &HeaderMap, uri: &Uri) -> Result<(), ApiError> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let expected = request_host(headers, uri).map(|host| format!("https://{host}"));
    if origin.to_str().ok() != expected.as_deref() {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "requests from another site are refused",
        ));
    }
    Ok(())
}

fn check_json(headers: &HeaderMap) -> Result<(), ApiError> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
    if json {
        Ok(())
    } else {
        Err(invalid("the body must be application/json"))
    }
}

fn parse_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| invalid(format!("the request body doesn't fit: {e}")))
}

/// A body that may be left out, as for logout and cancel.
fn parse_optional<T: DeserializeOwned + Default>(body: &[u8]) -> Result<T, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        Ok(T::default())
    } else {
        parse_body(body)
    }
}

/// The bearer token, if it has the shape of one; anything else is treated
/// as no token and never reaches the engine.
fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && is_token_shaped(token)).then(|| token.to_string())
}

fn authorized(headers: &HeaderMap, uri: &Uri) -> Result<String, ApiError> {
    check_origin(headers, uri)?;
    bearer(headers).ok_or_else(|| ApiError::new(ErrorCode::Unauthenticated, "log in first"))
}

fn job_id(text: &str) -> Result<JobId, ApiError> {
    JobId::from_str(text).map_err(|_| invalid("not a job ID"))
}

async fn relay(
    state: &AppState,
    client: ClientAddr,
    token: Option<String>,
    call: EngineCall,
) -> Response {
    let address = client.0.ip().to_canonical();
    match state.engine.call(address, token, call).await {
        Ok(EngineOutcome::Ok { body }) => api_ok(body),
        Ok(EngineOutcome::Err { error }) => api_error(error),
        Ok(EngineOutcome::Event { .. }) => api_error(ApiError::new(
            ErrorCode::Internal,
            "the engine answered with an event",
        )),
        Err(e) => {
            crate::log::warning(&format!("engine call failed: {e}"));
            api_error(ApiError::new(
                ErrorCode::Unavailable,
                "BMAC's engine on this host isn't answering; try again shortly",
            ))
        }
    }
}

/// A `GET` that needs a session.
async fn get(
    state: &AppState,
    client: ClientAddr,
    headers: &HeaderMap,
    uri: &Uri,
    call: Result<EngineCall, ApiError>,
) -> Response {
    match authorized(headers, uri).and_then(|token| Ok((token, call?))) {
        Ok((token, call)) => relay(state, client, Some(token), call).await,
        Err(error) => api_error(error),
    }
}

/// A `POST` with a JSON body that needs a session.
async fn post_json(
    state: &AppState,
    client: ClientAddr,
    headers: &HeaderMap,
    uri: &Uri,
    make: impl FnOnce() -> Result<EngineCall, ApiError>,
) -> Response {
    let checked = authorized(headers, uri).and_then(|token| {
        check_json(headers)?;
        Ok((token, make()?))
    });
    match checked {
        Ok((token, call)) => relay(state, client, Some(token), call).await,
        Err(error) => api_error(error),
    }
}

pub async fn login(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let request = check_origin(&headers, &uri)
        .and_then(|()| check_json(&headers))
        .and_then(|()| parse_body::<LoginRequest>(&body));
    match request {
        Ok(request) => relay(&state, client, None, EngineCall::Login { request }).await,
        Err(error) => api_error(error),
    }
}

pub async fn logout(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::Logout {
            request: parse_optional::<LogoutRequest>(&body)?,
        })
    })
    .await
}

pub async fn cluster(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(&state, client, &headers, &uri, Ok(EngineCall::Cluster {})).await
}

pub async fn list(
    collection: Collection,
    state: AppState,
    client: ClientAddr,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(
        &state,
        client,
        &headers,
        &uri,
        Ok(EngineCall::List { collection }),
    )
    .await
}

pub async fn operations(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(
        &state,
        client,
        &headers,
        &uri,
        Ok(EngineCall::Operations {}),
    )
    .await
}

pub async fn settings(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(&state, client, &headers, &uri, Ok(EngineCall::Settings {})).await
}

pub async fn save_cluster_settings(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::SaveClusterSettings {
            request: parse_body::<SaveSettings>(&body)?,
        })
    })
    .await
}

pub async fn save_host_facts(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(host): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::SaveHostFacts {
            host: host
                .parse::<HostSlot>()
                .map_err(|_| invalid("not a host name (mox1 to mox32)"))?,
            request: parse_body::<SaveSettings>(&body)?,
        })
    })
    .await
}

pub async fn joins(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(&state, client, &headers, &uri, Ok(EngineCall::Joins {})).await
}

pub async fn issue_join(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::IssueJoin {
            request: parse_body::<bmac_proto::joins::IssueJoin>(&body)?,
        })
    })
    .await
}

pub async fn revoke_join(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(host): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        parse_optional::<NoParameters>(&body)?;
        Ok(EngineCall::RevokeJoin {
            host: host
                .parse::<HostSlot>()
                .map_err(|_| invalid("not a host name (mox1 to mox32)"))?,
        })
    })
    .await
}

pub async fn appliances(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(
        &state,
        client,
        &headers,
        &uri,
        Ok(EngineCall::Appliances {}),
    )
    .await
}

pub async fn issue_enroll(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::IssueEnroll {
            request: parse_optional::<bmac_proto::backups::IssueEnroll>(&body)?,
        })
    })
    .await
}

fn request_id(id: &str) -> Result<String, ApiError> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("not a request id"));
    }
    Ok(id.to_string())
}

pub async fn approve_appliance(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::ApproveAppliance {
            request_id: request_id(&id)?,
            request: parse_body::<bmac_proto::backups::ApproveAppliance>(&body)?,
        })
    })
    .await
}

pub async fn reject_appliance(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        parse_optional::<NoParameters>(&body)?;
        Ok(EngineCall::RejectAppliance {
            request_id: request_id(&id)?,
        })
    })
    .await
}

pub async fn revoke_appliance(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(appliance): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        parse_optional::<NoParameters>(&body)?;
        Ok(EngineCall::RevokeAppliance {
            appliance_uuid: appliance
                .parse()
                .map_err(|_| invalid("not an appliance ID"))?,
        })
    })
    .await
}

pub async fn backup_policies(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(
        &state,
        client,
        &headers,
        &uri,
        Ok(EngineCall::BackupPolicies {}),
    )
    .await
}

pub async fn save_backup_policy(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::SaveBackupPolicy {
            request: parse_body::<serde_json::Value>(&body)?,
        })
    })
    .await
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DeletePolicy {
    revision: u64,
}

pub async fn delete_backup_policy(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(vm): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::DeleteBackupPolicy {
            vm_uuid: vm.parse().map_err(|_| invalid("not a VM ID"))?,
            revision: parse_body::<DeletePolicy>(&body)?.revision,
        })
    })
    .await
}

pub async fn certificate(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(
        &state,
        client,
        &headers,
        &uri,
        Ok(EngineCall::Certificate {}),
    )
    .await
}

pub async fn rotate_certificate(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        parse_optional::<NoParameters>(&body)?;
        Ok(EngineCall::RotateCertificate {})
    })
    .await
}

pub async fn switch_certificate(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::SwitchCertificate {
            request: parse_body::<SwitchCertificate>(&body)?,
        })
    })
    .await
}

pub async fn discard_certificate(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        parse_optional::<NoParameters>(&body)?;
        Ok(EngineCall::DiscardCertificate {})
    })
    .await
}

fn record_id(id: &str) -> Result<String, ApiError> {
    if !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(id.to_string())
    } else {
        Err(invalid("not an alert or silence ID"))
    }
}

pub async fn alerts(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(&state, client, &headers, &uri, Ok(EngineCall::Alerts {})).await
}

pub async fn acknowledge_alert(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        parse_optional::<NoParameters>(&body)?;
        Ok(EngineCall::AcknowledgeAlert {
            id: record_id(&id)?,
        })
    })
    .await
}

pub async fn save_silence(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::SaveSilence {
            request: parse_body::<SaveSilence>(&body)?,
        })
    })
    .await
}

pub async fn delete_silence(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        parse_optional::<NoParameters>(&body)?;
        Ok(EngineCall::DeleteSilence {
            id: record_id(&id)?,
        })
    })
    .await
}

pub async fn save_notify_settings(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::SaveNotifySettings {
            request: parse_body::<SaveNotifySettings>(&body)?,
        })
    })
    .await
}

pub async fn set_postmark_token(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::SetPostmarkToken {
            request: parse_body::<SetPostmarkToken>(&body)?,
        })
    })
    .await
}

pub async fn send_test_email(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::SendTestEmail {
            request: parse_body::<SendTestEmail>(&body)?,
        })
    })
    .await
}

pub async fn email_templates(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(
        &state,
        client,
        &headers,
        &uri,
        Ok(EngineCall::EmailTemplates {}),
    )
    .await
}

pub async fn create_email_templates(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        parse_optional::<NoParameters>(&body)?;
        Ok(EngineCall::CreateEmailTemplates {})
    })
    .await
}

pub async fn search_logs(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::SearchLogs {
            query: parse_body::<bmac_proto::logs::LogQuery>(&body)?,
        })
    })
    .await
}

pub async fn setup(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(&state, client, &headers, &uri, Ok(EngineCall::Setup {})).await
}

pub async fn log_replication(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(
        &state,
        client,
        &headers,
        &uri,
        Ok(EngineCall::LogReplication {}),
    )
    .await
}

pub async fn launch_job(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::LaunchJob {
            request: parse_body::<LaunchJob>(&body)?,
        })
    })
    .await
}

/// `from_seq` from the query string; 0 when absent.
fn from_seq(uri: &Uri) -> Result<u64, ApiError> {
    let Some(query) = uri.query() else {
        return Ok(0);
    };
    let mut seq = 0;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("from_seq", value)) => {
                seq = value
                    .parse()
                    .map_err(|_| invalid("from_seq must be a whole number"))?;
            }
            _ => return Err(invalid(format!("unknown query parameter {pair:?}"))),
        }
    }
    Ok(seq)
}

pub async fn job(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let call = job_id(&id).and_then(|job| {
        Ok(EngineCall::Job {
            job,
            from_seq: from_seq(&uri)?,
        })
    });
    get(&state, client, &headers, &uri, call).await
}

pub async fn answer(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path((id, request_id)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        if request_id.is_empty() || request_id.len() > 128 {
            return Err(invalid("not a request ID"));
        }
        Ok(EngineCall::Answer {
            job: job_id(&id)?,
            request_id,
            answer: parse_body::<Answer>(&body)?,
        })
    })
    .await
}

pub async fn cancel(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::Cancel {
            job: job_id(&id)?,
            request: parse_optional::<CancelRequest>(&body)?,
        })
    })
    .await
}

/// The body of a `POST` that takes no parameters: empty or `{}`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NoParameters {}

pub async fn jobs(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    get(&state, client, &headers, &uri, Ok(EngineCall::Jobs {})).await
}

pub async fn resume(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        parse_optional::<NoParameters>(&body)?;
        Ok(EngineCall::Resume { job: job_id(&id)? })
    })
    .await
}

pub async fn reconcile(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        parse_optional::<NoParameters>(&body)?;
        Ok(EngineCall::Reconcile { job: job_id(&id)? })
    })
    .await
}

pub async fn watch(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    post_json(&state, client, &headers, &uri, || {
        Ok(EngineCall::Watch {
            request: parse_body::<WatchRequest>(&body)?,
        })
    })
    .await
}

pub async fn not_found(_body: Bytes) -> Response {
    api_error(ApiError::new(ErrorCode::NotFound, "no such API endpoint"))
}

pub async fn method_not_allowed() -> Response {
    let mut response = api_error(ApiError::new(
        ErrorCode::Invalid,
        "this endpoint doesn't take that method",
    ));
    *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    response
}

/// A streaming body fed by the relay task; dropping it (the tab left) ends
/// the task, which closes the engine stream.
pub(crate) struct ChannelBody(pub(crate) mpsc::Receiver<Bytes>);

impl http_body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|frame| frame.map(|bytes| Ok(http_body::Frame::data(bytes))))
    }
}

async fn relay_stream(
    mut stream: EngineStream,
    first: Bytes,
    tx: mpsc::Sender<Bytes>,
    keepalive: Duration,
) {
    if tx.send(first).await.is_err() {
        return;
    }
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + keepalive, keepalive);
    loop {
        let frame = tokio::select! {
            _ = tx.closed() => return,
            _ = ticker.tick() => Bytes::from_static(SSE_KEEPALIVE.as_bytes()),
            outcome = stream.next() => match outcome {
                Some(EngineOutcome::Event { message }) => {
                    let last = matches!(message.body, StreamBody::SessionEnded { .. });
                    let frame = Bytes::from(sse_frame(&message));
                    if last {
                        let _ = tx.send(frame).await;
                        return;
                    }
                    frame
                }
                _ => return,
            },
        };
        if tx.send(frame).await.is_err() {
            return;
        }
    }
}

/// `GET /api/v1/events`: Server-Sent Events, read by the web app with
/// `fetch` so the token goes in a header.
pub async fn events(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let token = match authorized(&headers, &uri) {
        Ok(token) => token,
        Err(error) => return api_error(error),
    };
    let last_event_id = match headers.get("last-event-id").map(|v| v.to_str()) {
        None => None,
        Some(Ok(id))
            if id.len() <= MAX_LAST_EVENT_ID && id.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            Some(id.to_string())
        }
        Some(_) => return api_error(invalid("Last-Event-ID is not one of ours")),
    };
    let address = client.0.ip().to_canonical();
    let call = EngineCall::Subscribe { last_event_id };
    let mut stream = match state.engine.stream(address, Some(token), call).await {
        Ok(stream) => stream,
        Err(e) => {
            crate::log::warning(&format!("engine stream failed: {e}"));
            return api_error(ApiError::new(
                ErrorCode::Unavailable,
                "BMAC's engine on this host isn't answering; try again shortly",
            ));
        }
    };
    let first = match stream.next().await {
        Some(EngineOutcome::Event { message }) => Bytes::from(sse_frame(&message)),
        Some(EngineOutcome::Err { error }) => return api_error(error),
        _ => {
            return api_error(ApiError::new(
                ErrorCode::Unavailable,
                "BMAC's engine on this host closed the stream",
            ));
        }
    };
    let (tx, rx) = mpsc::channel(SSE_QUEUE);
    tokio::spawn(relay_stream(stream, first, tx, state.keepalive));
    let mut response = Response::new(Body::new(ChannelBody(rx)));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_seq_is_the_only_query_parameter() {
        let uri = |q: &str| Uri::try_from(format!("/api/v1/jobs/x{q}")).unwrap();
        assert_eq!(from_seq(&uri("")).unwrap(), 0);
        assert_eq!(from_seq(&uri("?from_seq=42")).unwrap(), 42);
        assert!(from_seq(&uri("?from_seq=-1")).is_err());
        assert!(from_seq(&uri("?path=/etc")).is_err());
    }

    #[test]
    fn origins_must_match_the_host() {
        let uri = Uri::from_static("/api/v1/cluster");
        let mut headers = HeaderMap::new();
        assert!(check_origin(&headers, &uri).is_ok());
        headers.insert(
            header::HOST,
            HeaderValue::from_static("mox1.tail1234.ts.net:8443"),
        );
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://mox1.tail1234.ts.net:8443"),
        );
        assert!(check_origin(&headers, &uri).is_ok());
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://mox1.tail1234.ts.net:8443"),
        );
        assert!(check_origin(&headers, &uri).is_err());
        headers.insert(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(check_origin(&headers, &uri).is_err());
        let h2 = Uri::from_static("https://10.10.0.1:8443/api/v1/cluster");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://10.10.0.1:8443"),
        );
        assert!(check_origin(&headers, &h2).is_ok());
    }

    #[test]
    fn only_well_formed_bearer_tokens_are_passed_on() {
        let token = "A".repeat(43);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        assert_eq!(bearer(&headers), Some(token.clone()));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {token}")).unwrap(),
        );
        assert_eq!(bearer(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer ../../etc/passwd"),
        );
        assert_eq!(bearer(&headers), None);
    }
}
