// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The UI's JSON API. Every request but `hello` and `login` carries the
//! bearer token; every change is a `POST` of `application/json` from this
//! origin, and is audited with the address it came from.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use bmac_backup::retention::ApplianceRetention;
use bmac_notify::postmark::{self, Email, Outcome};
use bmac_notify::templates::{Layout, Model, TestEmail, render};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::appliance_ui::*;
use bmac_proto::backups::display_fingerprint;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use uuid::Uuid;

use super::sessions::is_token_shaped;
use super::{ClientAddr, Ui};
use crate::admin;
use crate::grants::Grants;
use crate::journal::Journal;
use crate::peers::{self, Peers, Probe};
use crate::platform::{IoPolicy, Platform};
use crate::retention::{self, Change, Settings};
use crate::scrub::ScrubState;
use crate::watchdog::WatchState;

type Answer = Result<Value, ApiError>;

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}

fn internal(message: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorCode::Internal, message.to_string())
}

fn conflict(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Conflict, message)
}

fn json(value: impl Serialize) -> Answer {
    serde_json::to_value(value).map_err(internal)
}

fn error_response(error: ApiError) -> Response {
    let status =
        StatusCode::from_u16(error.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let retry = error.retry_after_seconds;
    let mut response = (status, axum::Json(error)).into_response();
    if let Some(seconds) = retry {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}

fn check_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    if host.is_none() || origin.to_str().ok() != host.map(|h| format!("https://{h}")).as_deref() {
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

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| invalid(format!("the request body doesn't fit: {e}")))
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && is_token_shaped(token)).then(|| token.to_string())
}

fn uuid(text: &str, what: &str) -> Result<Uuid, ApiError> {
    text.parse()
        .map_err(|_| invalid(format!("{text:?} isn't {what}")))
}

fn counts(p: &ApplianceRetention) -> RetentionCounts {
    RetentionCounts {
        daily: p.daily,
        weekly: p.weekly,
        monthly: p.monthly,
        month_days: p.month_days,
        yearly: p.yearly,
    }
}

fn io_name(policy: IoPolicy) -> String {
    serde_json::to_value(policy)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// One request, as the handlers see it.
struct Call<'a> {
    ui: &'a Ui,
    address: IpAddr,
    token: Option<String>,
    body: &'a [u8],
    query: Option<&'a str>,
}

impl Call<'_> {
    fn now(&self) -> i64 {
        (self.ui.now_ms)()
    }

    fn by(&self) -> String {
        format!("ui from {}", self.address)
    }

    fn audit(&self, event: &str, details: Value) {
        let _ = self.ui.audit.record(self.now(), event, &self.by(), details);
    }

    fn journal(&self) -> Result<Journal, ApiError> {
        self.ui
            .metadata
            .load()
            .map_err(internal)?
            .ok_or_else(|| ApiError::new(ErrorCode::Unavailable, "the pool has no journal"))
    }

    fn body<T: DeserializeOwned>(&self) -> Result<T, ApiError> {
        parse(self.body)
    }

    /// The administrator's password, checked off the async threads; a
    /// failure counts toward the login limit.
    async fn password_ok(&self, password: String) -> Result<bool, ApiError> {
        let root = self.ui.metadata.root().to_path_buf();
        let ok = tokio::task::spawn_blocking(move || admin::verify(&root, &password))
            .await
            .map_err(internal)?
            .map_err(internal)?;
        if !ok {
            self.ui.sessions.failed(self.now());
        }
        Ok(ok)
    }
}

pub(super) async fn handle(
    State(ui): State<Arc<Ui>>,
    Extension(client): Extension<ClientAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri
        .path()
        .strip_prefix(APPLIANCE_API)
        .unwrap_or_default()
        .to_string();
    let call = Call {
        ui: &ui,
        address: client.0.ip().to_canonical(),
        token: bearer(&headers),
        body: &body,
        query: uri.query(),
    };
    let answer = match (check_origin(&headers), method == Method::POST) {
        (Err(e), _) => Err(e),
        (Ok(()), true) => match check_json(&headers) {
            Ok(()) => route(&call, &method, &path).await,
            Err(e) => Err(e),
        },
        (Ok(()), false) => route(&call, &method, &path).await,
    };
    let mut response = match answer {
        Ok(value) => (StatusCode::OK, axum::Json(value)).into_response(),
        Err(error) => error_response(error),
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn route(call: &Call<'_>, method: &Method, path: &str) -> Answer {
    match (method.as_str(), path) {
        ("GET", "/hello") => return json(hello(call.ui).await),
        ("POST", "/login") => return login(call).await,
        ("POST", "/logout") => {
            if let Some(token) = &call.token
                && call.ui.sessions.end(token)
            {
                call.audit("logout", Value::Null);
            }
            return Ok(Value::Null);
        }
        _ => {}
    }
    let token = call
        .token
        .as_deref()
        .ok_or_else(|| ApiError::new(ErrorCode::Unauthenticated, "log in first"))?;
    call.ui.sessions.touch(token, call.now()).ok_or_else(|| {
        ApiError::new(
            ErrorCode::Unauthenticated,
            "your session has ended; log in again",
        )
    })?;
    if !(call.ui.unlocked)().await {
        return Err(ApiError::new(
            ErrorCode::Unavailable,
            "the pool is locked; unlock it at the appliance's console (bmac-backupd unlock)",
        ));
    }
    match (method.as_str(), path) {
        ("GET", "/status") => status(call).await,
        ("GET", "/pits") => pits(call),
        ("GET", "/throughput") => throughput(call),
        ("GET", "/retention") => show_retention(call).await,
        ("POST", "/retention") => set_retention(call).await,
        ("POST", "/retention/cancel") => cancel_retention(call).await,
        ("POST", "/holds") => set_hold(call).await,
        ("POST", "/prune") => prune(call).await,
        ("GET", "/grants") => grants(call),
        ("POST", "/grants") => issue_grant(call).await,
        ("POST", "/grants/revoke") => revoke_grant(call).await,
        ("POST", "/grants/recovery") => issue_recovery_grant(call).await,
        ("GET", "/peers") => show_peers(call),
        ("GET", "/recovery") => recovery_view(call),
        ("POST", "/peers/trust") => trust(call).await,
        ("POST", "/peers/untrust") => untrust(call).await,
        ("GET", "/scrub") => json(scrub_view(call)?),
        ("POST", "/scrub/start") => start_scrub(call).await,
        ("GET", "/settings") => json(platform_view(call)?),
        ("POST", "/settings") => save_platform(call).await,
        ("POST", "/settings/token") => set_token(call),
        ("POST", "/settings/test-email") => test_email(call).await,
        ("POST", "/password") => change_password(call).await,
        ("GET", "/audit") => audit(call),
        _ => Err(ApiError::new(ErrorCode::NotFound, "no such API path")),
    }
}

pub(super) async fn hello(ui: &Ui) -> ApplianceHello {
    let locked = !(ui.unlocked)().await;
    ApplianceHello {
        name: ui.identity.name.clone(),
        appliance_uuid: ui.identity.appliance_uuid.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        locked,
        no_password: !locked && admin::load(ui.metadata.root()).ok().flatten().is_none(),
        fingerprint: display_fingerprint(&ui.fingerprint),
    }
}

async fn login(call: &Call<'_>) -> Answer {
    let request: ApplianceLogin = call.body()?;
    if let Err(wait) = call.ui.sessions.admit(call.address, call.now()) {
        call.audit("login_rate_limited", Value::Null);
        let mut error = ApiError::new(ErrorCode::RateLimited, "too many login attempts.");
        error.retry_after_seconds = Some(wait);
        return Err(error);
    }
    if !(call.ui.unlocked)().await {
        return Err(ApiError::new(
            ErrorCode::Unavailable,
            "the pool is locked; unlock it at the appliance's console first",
        ));
    }
    if !call.password_ok(request.password).await? {
        call.audit("login_failed", Value::Null);
        return Err(ApiError::new(
            ErrorCode::Unauthenticated,
            "that isn't the administrator's password (or none is set; set one at the console with bmac-backupd admin-password)",
        ));
    }
    let (token, session) = call.ui.sessions.start(call.now()).map_err(internal)?;
    call.audit("login", Value::Null);
    json(ApplianceSession {
        token,
        idle_timeout_seconds: (super::sessions::IDLE_MS / 1000) as u32,
        expires_at_ms: session.expires_at_ms(),
    })
}

async fn change_password(call: &Call<'_>) -> Answer {
    let request: ChangePassword = call.body()?;
    if let Err(wait) = call.ui.sessions.admit(call.address, call.now()) {
        let mut error = ApiError::new(ErrorCode::RateLimited, "too many password attempts.");
        error.retry_after_seconds = Some(wait);
        return Err(error);
    }
    if !call.password_ok(request.current).await? {
        call.audit("admin_password_refused", Value::Null);
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "the current password isn't right",
        ));
    }
    admin::check_strength(&request.new).map_err(invalid)?;
    let (root, rounds, now) = (
        call.ui.metadata.root().to_path_buf(),
        call.ui.rounds,
        call.now(),
    );
    let new = request.new;
    tokio::task::spawn_blocking(move || admin::set_password(&root, &new, now, rounds))
        .await
        .map_err(internal)?
        .map_err(internal)?;
    if let Some(token) = &call.token {
        call.ui.sessions.end_others(token);
    }
    call.audit("admin_password", Value::Null);
    Ok(Value::Null)
}

async fn status(call: &Call<'_>) -> Answer {
    let ui = call.ui;
    let pool = ui.pool.status().await.ok().map(|s| PoolView {
        name: ui.binding.pool.clone(),
        guid: ui.binding.pool_guid.clone(),
        health: s.health,
        size_bytes: s.size_bytes,
        free_bytes: s.free_bytes,
    });
    let mut clusters = Vec::new();
    let mut transfers = Vec::new();
    let zones = retention::Zones::load(&ui.metadata).unwrap_or_default();
    for c in (ui.clusters)() {
        for t in &c.transfers {
            transfers.push(LiveTransfer {
                cluster_uuid: c.status.cluster_uuid.clone(),
                vm_uuid: t.vm_uuid.to_string(),
                pit_uuid: t.pit_uuid.to_string(),
                bytes: t.bytes,
            });
        }
        let time_zone = zone_name(&zones, &c.status.cluster_uuid, call.now());
        clusters.push(ClusterView {
            cluster_uuid: c.status.cluster_uuid,
            cluster_name: c.status.cluster_name,
            state: c.status.state,
            last_checkin_ms: c.status.last_checkin_ms,
            last_error: c.status.last_error,
            offers: c.status.offers as u32,
            time_zone,
            pulling: c.status.pulling,
            hosts: c.hosts,
        });
    }
    let journal = call.journal()?;
    let watch = WatchState::load(&ui.metadata).map_err(internal)?;
    let mut views: Vec<WatchView> = watch
        .clusters
        .iter()
        .map(|c| WatchView {
            kind: "cluster".into(),
            subject: c.cluster_name.clone(),
            down_since_ms: c.down_since_ms,
            last_error: None,
        })
        .collect();
    views.extend(watch.urls.iter().map(|u| WatchView {
        kind: "url".into(),
        subject: u.url.clone(),
        down_since_ms: u.failing_since_ms,
        last_error: u.last_error.clone(),
    }));
    let settings = Settings::load(&ui.metadata).map_err(internal)?;
    json(ApplianceStatusView {
        hello: hello(ui).await,
        pool,
        clusters,
        transfers,
        pits: journal.pits.len() as u32,
        scrub: scrub_view(call)?,
        watch: views,
        notify_error: watch.notify_error,
        pending_retention: settings.pending.map(|p| PendingRetention {
            policy: counts(&p.policy),
            effective_at_ms: p.effective_at_ms,
            by: p.by,
        }),
    })
}

fn cluster_names(call: &Call<'_>) -> std::collections::BTreeMap<String, String> {
    crate::state::enrollments(&call.ui.paths)
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.cluster_uuid, e.cluster_name))
        .collect()
}

/// A cluster's time zone as an IANA name.
fn zone_name(zones: &retention::Zones, cluster: &str, now_ms: i64) -> String {
    zones
        .zone(cluster, now_ms)
        .iana_name()
        .unwrap_or("UTC")
        .to_string()
}

fn pits(call: &Call<'_>) -> Answer {
    let journal = call.journal()?;
    let now = call.now();
    let names = cluster_names(call);
    let zones = retention::Zones::load(&call.ui.metadata).map_err(internal)?;
    let settings = Settings::load(&call.ui.metadata).map_err(internal)?;
    let held: BTreeSet<Uuid> = settings.holds.iter().map(|h| h.pit_uuid).collect();
    let suspect: BTreeSet<Uuid> = ScrubState::load(&call.ui.metadata)
        .map_err(internal)?
        .suspects
        .iter()
        .map(|s| s.pit_uuid)
        .collect();
    let granted: BTreeSet<Uuid> = Grants::load(&call.ui.metadata)
        .map_err(internal)?
        .grants
        .iter()
        .filter(|g| g.live(now))
        .flat_map(|g| g.pits.iter().copied())
        .collect();
    let mut list: Vec<PitView> = journal
        .pits
        .iter()
        .map(|p| PitView {
            cluster_name: names
                .get(&p.cluster_uuid)
                .cloned()
                .unwrap_or_else(|| p.cluster_uuid.clone()),
            cluster_uuid: p.cluster_uuid.clone(),
            vm_uuid: p.vm_uuid.to_string(),
            branch_uuid: p.branch_uuid.to_string(),
            pit_uuid: p.pit_uuid.to_string(),
            parent: p.parent.map(|u| u.to_string()),
            name: p.name.clone(),
            taken_s: p.taken_s,
            published_at_ms: p.published_at_ms,
            bytes: p.bytes,
            volumes: p.volumes.len() as u32,
            received_from: p.received_from.map(|u| u.to_string()),
            held: held.contains(&p.pit_uuid),
            suspect: suspect.contains(&p.pit_uuid),
            granted: granted.contains(&p.pit_uuid),
            time_zone: zone_name(&zones, &p.cluster_uuid, now),
        })
        .collect();
    list.sort_by(|a, b| {
        (&a.cluster_uuid, &a.vm_uuid, a.taken_s).cmp(&(&b.cluster_uuid, &b.vm_uuid, b.taken_s))
    });
    json(PitsView {
        pits: list,
        receiving: journal
            .receiving
            .iter()
            .map(|r| ReceivingView {
                cluster_uuid: r.cluster_uuid.clone(),
                vm_uuid: r.vm_uuid.to_string(),
                pit_uuid: r.pit_uuid.to_string(),
                started_at_ms: r.started_at_ms,
                bytes: r.volumes.iter().map(|v| v.bytes).sum(),
            })
            .collect(),
        quarantined: journal
            .quarantined
            .iter()
            .map(|q| QuarantinedView {
                cluster_uuid: q.cluster_uuid.clone(),
                vm_uuid: q.vm_uuid.to_string(),
                pit_uuid: q.pit_uuid.to_string(),
                reason: q.reason.clone(),
                at_ms: q.at_ms,
            })
            .collect(),
    })
}

/// How many audit entries the throughput page reads back.
const THROUGHPUT_SCAN: usize = 2000;
const THROUGHPUT_KEEP: usize = 200;

fn throughput(call: &Call<'_>) -> Answer {
    let entries = call.ui.audit.recent(THROUGHPUT_SCAN).map_err(internal)?;
    let text = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let mut recent: Vec<TransferRecord> = entries
        .iter()
        .filter(|e| e.event == "publish")
        .take(THROUGHPUT_KEEP)
        .map(|e| TransferRecord {
            at_ms: e.at_ms,
            cluster_uuid: text(&e.details, "cluster_uuid"),
            vm_uuid: text(&e.details, "vm_uuid"),
            pit_uuid: text(&e.details, "pit_uuid"),
            bytes: e.details["bytes"].as_u64().unwrap_or(0),
            duration_ms: e.details["duration_ms"].as_u64().unwrap_or(0),
            from_peer: e.details["from_peer"].as_str().map(str::to_string),
        })
        .collect();
    recent.reverse();
    let live = (call.ui.clusters)()
        .into_iter()
        .flat_map(|c| {
            let cluster = c.status.cluster_uuid;
            c.transfers.into_iter().map(move |t| LiveTransfer {
                cluster_uuid: cluster.clone(),
                vm_uuid: t.vm_uuid.to_string(),
                pit_uuid: t.pit_uuid.to_string(),
                bytes: t.bytes,
            })
        })
        .collect();
    json(ThroughputView { recent, live })
}

async fn show_retention(call: &Call<'_>) -> Answer {
    let journal = call.journal()?;
    let settings = Settings::load(&call.ui.metadata).map_err(internal)?;
    let pins = Grants::load(&call.ui.metadata)
        .map_err(internal)?
        .pins(call.now());
    let zones = retention::Zones::load(&call.ui.metadata).map_err(internal)?;
    let mut plan = retention::plan(&journal, &settings, &zones, &pins, call.now());
    retention::estimate(call.ui.pool.as_ref(), &mut plan).await;
    json(ApplianceRetentionView {
        policy: counts(&settings.policy),
        floor: counts(&settings.floor),
        delay_hours: settings.delay_hours,
        pending: settings.pending.as_ref().map(|p| PendingRetention {
            policy: counts(&p.policy),
            effective_at_ms: p.effective_at_ms,
            by: p.by.clone(),
        }),
        holds: settings
            .holds
            .iter()
            .map(|h| HoldView {
                cluster_uuid: h.cluster_uuid.clone(),
                pit_uuid: h.pit_uuid.to_string(),
                by: h.by.clone(),
                at_ms: h.at_ms,
            })
            .collect(),
        prune: plan.prune.iter().map(|d| d.pit_uuid.to_string()).collect(),
        reclaim_bytes: plan.reclaim_bytes,
        coverage: plan
            .coverage
            .iter()
            .map(|c| CoverageView {
                cluster_uuid: c.cluster_uuid.clone(),
                vm_uuid: c.vm_uuid.to_string(),
                pits: c.pits as u32,
                oldest_s: c.oldest_s,
                newest_s: c.newest_s,
                time_zone: zone_name(&zones, &c.cluster_uuid, call.now()),
            })
            .collect(),
    })
}

/// Changes the retention settings under the journal lock, and audits it.
async fn change_retention<T>(
    call: &Call<'_>,
    event: Value,
    change: impl FnOnce(&mut Settings, &Journal) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let metadata = &call.ui.metadata;
    let _guard = metadata.lock().await.map_err(internal)?;
    let journal = call.journal()?;
    let mut settings = Settings::load(metadata).map_err(internal)?;
    let out = change(&mut settings, &journal)?;
    settings.save(metadata).map_err(internal)?;
    let mut details = event;
    details["retention"] = serde_json::to_value(settings.policy).unwrap_or_default();
    details["floor"] = serde_json::to_value(settings.floor).unwrap_or_default();
    call.audit("policy", details);
    Ok(out)
}

async fn set_retention(call: &Call<'_>) -> Answer {
    let request: SetRetention = call.body()?;
    let policy = ApplianceRetention {
        daily: request.policy.daily,
        weekly: request.policy.weekly,
        monthly: request.policy.monthly,
        month_days: request.policy.month_days,
        yearly: request.policy.yearly,
    };
    let (by, now) = (call.by(), call.now());
    let change = change_retention(
        call,
        serde_json::json!({"requested": policy}),
        |settings, _| settings.change(policy, &by, now, false).map_err(invalid),
    )
    .await?;
    json(match change {
        Change::Applied => RetentionChange {
            applied: true,
            effective_at_ms: None,
        },
        Change::Delayed { effective_at_ms } => RetentionChange {
            applied: false,
            effective_at_ms: Some(effective_at_ms),
        },
    })
}

async fn cancel_retention(call: &Call<'_>) -> Answer {
    change_retention(
        call,
        serde_json::json!({"cancel_pending": true}),
        |settings, _| {
            settings
                .pending
                .take()
                .map(|_| ())
                .ok_or_else(|| conflict("no change is waiting"))
        },
    )
    .await?;
    Ok(Value::Null)
}

async fn set_hold(call: &Call<'_>) -> Answer {
    let request: SetHold = call.body()?;
    let pit = uuid(&request.pit_uuid, "a PIT UUID")?;
    let (by, now) = (call.by(), call.now());
    change_retention(
        call,
        serde_json::json!({"pit_uuid": pit, "hold": request.hold}),
        |settings, journal| {
            let cluster = journal
                .pits
                .iter()
                .find(|p| p.pit_uuid == pit)
                .map(|p| p.cluster_uuid.clone())
                .ok_or_else(|| {
                    ApiError::new(ErrorCode::NotFound, "that PIT isn't published here")
                })?;
            if request.hold {
                settings.hold(&cluster, pit, &by, now);
            } else {
                settings.release(&cluster, &pit);
            }
            Ok(())
        },
    )
    .await?;
    Ok(Value::Null)
}

async fn prune(call: &Call<'_>) -> Answer {
    let pins = Grants::load(&call.ui.metadata)
        .map_err(internal)?
        .pins(call.now());
    let report = retention::prune(
        call.ui.pool.as_ref(),
        &call.ui.metadata,
        &call.ui.audit,
        &pins,
        call.now(),
    )
    .await
    .map_err(internal)?;
    json(PruneResult {
        pruned: report.pruned.len() as u32,
        reclaim_bytes: report.reclaim_bytes,
        skipped: report
            .skipped
            .iter()
            .map(|(pit, why)| format!("{pit}: {why}"))
            .collect(),
    })
}

fn grant_view(g: &crate::grants::Grant, now: i64) -> GrantView {
    GrantView {
        grant_id: g.grant_id.clone(),
        cluster_uuid: g.cluster_uuid.clone(),
        pits: g.pits.iter().map(Uuid::to_string).collect(),
        created_at_ms: g.created_at_ms,
        expires_at_ms: g.expires_at_ms,
        by: g.by.clone(),
        state: if g.revoked {
            "revoked"
        } else if g.live(now) {
            "live"
        } else {
            "ended"
        }
        .into(),
        last_used_ms: g.last_used_ms,
        kind: match g.kind {
            crate::grants::GrantKind::Restore => "restore",
            crate::grants::GrantKind::Recovery => "recovery",
        }
        .into(),
        host_fingerprint: g.host_fingerprint.as_deref().map(display_fingerprint),
    }
}

fn grants(call: &Call<'_>) -> Answer {
    let now = call.now();
    let mut list: Vec<GrantView> = Grants::load(&call.ui.metadata)
        .map_err(internal)?
        .grants
        .iter()
        .map(|g| grant_view(g, now))
        .collect();
    list.sort_by_key(|g| std::cmp::Reverse(g.created_at_ms));
    json(list)
}

async fn issue_grant(call: &Call<'_>) -> Answer {
    let request: IssueGrant = call.body()?;
    let pits = request
        .pits
        .iter()
        .map(|p| uuid(p, "a PIT UUID"))
        .collect::<Result<Vec<_>, _>>()?;
    let metadata = &call.ui.metadata;
    let _guard = metadata.lock().await.map_err(internal)?;
    let journal = call.journal()?;
    let mut grants = Grants::load(metadata).map_err(internal)?;
    let (grant, token) = grants
        .issue(
            &journal,
            &request.cluster_uuid,
            &pits,
            Some(request.hours),
            &call.by(),
            call.now(),
        )
        .map_err(invalid)?;
    grants.save(metadata).map_err(internal)?;
    call.audit(
        "grant",
        serde_json::json!({"grant_id": grant.grant_id, "cluster_uuid": grant.cluster_uuid, "pits": grant.pits, "expires_at_ms": grant.expires_at_ms}),
    );
    json(IssuedGrant {
        grant: grant_view(&grant, call.now()),
        token,
    })
}

async fn issue_recovery_grant(call: &Call<'_>) -> Answer {
    let request: IssueRecoveryGrant = call.body()?;
    let metadata = &call.ui.metadata;
    let _guard = metadata.lock().await.map_err(internal)?;
    let journal = call.journal()?;
    let mut grants = Grants::load(metadata).map_err(internal)?;
    let (grant, token) = grants
        .issue_recovery(
            &journal,
            &request.cluster_uuid,
            &request.host_fingerprint,
            Some(request.hours),
            &call.by(),
            call.now(),
        )
        .map_err(invalid)?;
    grants.save(metadata).map_err(internal)?;
    call.audit(
        "recovery_grant",
        serde_json::json!({"grant_id": grant.grant_id, "cluster_uuid": grant.cluster_uuid, "host_fingerprint": grant.host_fingerprint, "expires_at_ms": grant.expires_at_ms}),
    );
    json(IssuedGrant {
        grant: grant_view(&grant, call.now()),
        token,
    })
}

async fn revoke_grant(call: &Call<'_>) -> Answer {
    let request: RevokeGrant = call.body()?;
    let metadata = &call.ui.metadata;
    let _guard = metadata.lock().await.map_err(internal)?;
    let mut grants = Grants::load(metadata).map_err(internal)?;
    if !grants.revoke(&request.grant_id) {
        return Err(ApiError::new(
            ErrorCode::NotFound,
            "that isn't a live grant",
        ));
    }
    grants.save(metadata).map_err(internal)?;
    call.audit(
        "revoke_grant",
        serde_json::json!({"grant_id": request.grant_id}),
    );
    Ok(Value::Null)
}

fn recovery_view(call: &Call<'_>) -> Answer {
    use bmac_proto::appliance_ui::{
        ClusterRecoveryView, HeaderKept, ManifestKept, RecoveryView, ReportedRestoreTest,
    };
    let metadata = &call.ui.metadata;
    let clusters = (call.ui.clusters)()
        .into_iter()
        .map(|c| {
            let cluster = c.status.cluster_uuid;
            ClusterRecoveryView {
                manifests: crate::recovery::manifests_kept(metadata, &cluster)
                    .into_iter()
                    .map(|(generated_at_ms, revision)| ManifestKept {
                        generated_at_ms,
                        revision,
                    })
                    .collect(),
                headers: crate::recovery::headers_kept(metadata, &cluster)
                    .into_iter()
                    .flat_map(|(host, files)| {
                        files
                            .into_iter()
                            .map(move |(file, newest_at_ms, versions)| HeaderKept {
                                host: host.clone(),
                                file,
                                newest_at_ms,
                                versions: versions as u32,
                            })
                    })
                    .collect(),
                restore_tests: crate::recovery::tests_kept(metadata, &cluster)
                    .into_iter()
                    .map(|t| ReportedRestoreTest {
                        restore_uuid: t.restore_uuid.to_string(),
                        vm_uuid: t.vm_uuid.to_string(),
                        pit_uuid: t.pit_uuid.to_string(),
                        host: t.host,
                        started_at_ms: t.started_at_ms,
                        finished_at_ms: t.finished_at_ms,
                        booted: t.booted,
                        agent: t.agent,
                        agent_after_s: t.agent_after_s,
                        error: t.error,
                    })
                    .collect(),
                cluster_uuid: cluster,
                cluster_name: c.status.cluster_name,
            }
        })
        .collect();
    json(RecoveryView { clusters })
}

fn show_peers(call: &Call<'_>) -> Answer {
    let peers = Peers::load(&call.ui.metadata).map_err(internal)?;
    json(PeersView {
        fingerprint: display_fingerprint(&call.ui.fingerprint),
        peers: peers
            .peers
            .iter()
            .map(|e| PeerView {
                appliance_uuid: e.peer.appliance_uuid.clone(),
                fingerprint: display_fingerprint(&e.peer.fingerprint),
                address: e.peer.address.to_string(),
                added_at_ms: e.added_at_ms,
                by: e.by.clone(),
                last_served_ms: e.last_served_ms,
                trusts_us: e.trusts_us,
            })
            .collect(),
        refusals: peers
            .refusals
            .iter()
            .rev()
            .take(50)
            .map(|r| RefusalView {
                fingerprint: display_fingerprint(&r.fingerprint),
                claimed: r.claimed.clone(),
                from: r.from.clone(),
                at_ms: r.at_ms,
            })
            .collect(),
        reminder: peers::TRUST_REMINDER.into(),
    })
}

async fn trust(call: &Call<'_>) -> Answer {
    let request: TrustPeer = call.body()?;
    let appliance = uuid(&request.appliance_uuid, "an appliance UUID")?;
    let address = request
        .address
        .parse::<bmac_core::ApplianceAddress>()
        .map_err(|e| invalid(format!("{}: {e}", request.address)))?;
    let wanted = bmac_proto::appliance::normalized_fingerprint(&request.fingerprint)
        .ok_or_else(|| invalid("a fingerprint is 64 hexadecimal digits"))?;
    match (call.ui.probe)(wanted.clone(), address.clone()).await {
        Probe::Matches => {}
        Probe::Mismatch { presented } => {
            call.audit(
                "trust_mismatch",
                serde_json::json!({"appliance_uuid": appliance, "address": address.to_string(), "typed": wanted, "presented": presented}),
            );
            return json(TrustResult {
                trusted: false,
                presented: Some(display_fingerprint(&presented)),
                message: format!(
                    "WARNING: {address} presented a certificate with another fingerprint. Nothing was saved."
                ),
            });
        }
        Probe::Unreachable(e) => {
            return json(TrustResult {
                trusted: false,
                presented: None,
                message: format!("{address} can't be reached: {e}. Nothing was saved."),
            });
        }
    }
    let metadata = &call.ui.metadata;
    let _guard = metadata.lock().await.map_err(internal)?;
    let mut peers = Peers::load(metadata).map_err(internal)?;
    peers
        .trust(
            appliance,
            &wanted,
            address.clone(),
            &call.ui.fingerprint,
            &call.by(),
            call.now(),
        )
        .map_err(conflict)?;
    peers.save(metadata).map_err(internal)?;
    call.audit(
        "trust",
        serde_json::json!({"appliance_uuid": appliance, "fingerprint": wanted, "address": address.to_string()}),
    );
    json(TrustResult {
        trusted: true,
        presented: None,
        message: format!(
            "{address} presented that certificate; appliance {appliance} is now a trusted peer."
        ),
    })
}

async fn untrust(call: &Call<'_>) -> Answer {
    let request: UntrustPeer = call.body()?;
    if let Err(wait) = call.ui.sessions.admit(call.address, call.now()) {
        let mut error = ApiError::new(ErrorCode::RateLimited, "too many password attempts.");
        error.retry_after_seconds = Some(wait);
        return Err(error);
    }
    if !call.password_ok(request.password).await? {
        call.audit(
            "untrust_refused",
            serde_json::json!({"appliance_uuid": request.appliance_uuid}),
        );
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "that isn't the administrator's password",
        ));
    }
    let metadata = &call.ui.metadata;
    let _guard = metadata.lock().await.map_err(internal)?;
    let mut peers = Peers::load(metadata).map_err(internal)?;
    let entry = peers
        .untrust(&request.appliance_uuid)
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "that isn't a trusted peer"))?;
    peers.save(metadata).map_err(internal)?;
    call.audit(
        "untrust",
        serde_json::json!({"appliance_uuid": request.appliance_uuid, "fingerprint": entry.peer.fingerprint}),
    );
    Ok(Value::Null)
}

const DAY_MS: i64 = 24 * 3600 * 1000;

fn scrub_view(call: &Call<'_>) -> Result<Option<ScrubView>, ApiError> {
    let state = ScrubState::load(&call.ui.metadata).map_err(internal)?;
    let platform = Platform::load(&call.ui.metadata).map_err(internal)?;
    let next_due_ms = (platform.scrub_every_days > 0).then(|| {
        state.last_started_ms.map_or(call.now(), |t| {
            t + i64::from(platform.scrub_every_days) * DAY_MS
        })
    });
    let suspects: Vec<String> = state
        .suspects
        .iter()
        .map(|s| s.pit_uuid.to_string())
        .collect();
    Ok(match state.report {
        Some(r) => Some(ScrubView {
            state: r.state,
            started_at_ms: r.started_at_ms,
            finished_at_ms: r.finished_at_ms,
            done_permille: r.done_permille,
            repaired_bytes: r.repaired_bytes,
            errors: r.errors,
            affected: r.affected,
            suspect_pits: suspects,
            next_due_ms,
        }),
        None if state.requested || next_due_ms.is_some() => Some(ScrubView {
            state: if state.requested { "requested" } else { "none" }.into(),
            suspect_pits: suspects,
            next_due_ms,
            ..Default::default()
        }),
        None => None,
    })
}

async fn start_scrub(call: &Call<'_>) -> Answer {
    let metadata = &call.ui.metadata;
    {
        let _guard = metadata.lock().await.map_err(internal)?;
        let mut state = ScrubState::load(metadata).map_err(internal)?;
        if matches!(
            state.report.as_ref().map(|r| r.state.as_str()),
            Some("running" | "paused")
        ) {
            return Err(conflict("a scrub is already running"));
        }
        state.requested = true;
        state.save(metadata).map_err(internal)?;
    }
    call.audit("scrub_requested", Value::Null);
    call.ui.scrub_now.notify_one();
    Ok(Value::Null)
}

fn platform_view(call: &Call<'_>) -> Result<PlatformView, ApiError> {
    let p = Platform::load(&call.ui.metadata).map_err(internal)?;
    Ok(PlatformView {
        scrub_every_days: p.scrub_every_days,
        io_policy: io_name(p.io_policy),
        down_after_minutes: p.watchdog.down_after_minutes,
        urls: p.watchdog.urls,
        from: p.notify.from,
        to: p.notify.to,
        token_set: Platform::token(&call.ui.metadata).is_some(),
    })
}

async fn save_platform(call: &Call<'_>) -> Answer {
    let request: SavePlatform = call.body()?;
    let io_policy: IoPolicy = serde_json::from_value(Value::from(request.io_policy.clone()))
        .map_err(|_| invalid(format!("{:?} isn't an I/O policy", request.io_policy)))?;
    let metadata = &call.ui.metadata;
    let _guard = metadata.lock().await.map_err(internal)?;
    let mut p = Platform::load(metadata).map_err(internal)?;
    p.scrub_every_days = request.scrub_every_days;
    p.io_policy = io_policy;
    p.watchdog.down_after_minutes = request.down_after_minutes;
    p.watchdog.urls = request.urls.iter().map(|u| u.trim().to_string()).collect();
    p.notify.from = request.from.trim().to_string();
    p.notify.to = request.to.iter().map(|t| t.trim().to_string()).collect();
    p.save(metadata).map_err(invalid)?;
    call.audit("settings", serde_json::to_value(&p).unwrap_or_default());
    json(platform_view(call)?)
}

fn set_token(call: &Call<'_>) -> Answer {
    let request: SetApplianceToken = call.body()?;
    let metadata = &call.ui.metadata;
    match &request.token {
        Some(token) => Platform::set_token(metadata, token).map_err(invalid)?,
        None => Platform::clear_token(metadata).map_err(internal)?,
    }
    call.audit(
        "postmark_token",
        serde_json::json!({"set": request.token.is_some()}),
    );
    Ok(Value::Null)
}

async fn test_email(call: &Call<'_>) -> Answer {
    let metadata = &call.ui.metadata;
    let p = Platform::load(metadata).map_err(internal)?;
    let token =
        Platform::token(metadata).ok_or_else(|| conflict("set the Postmark server token first"))?;
    if p.notify.from.is_empty() || p.notify.to.is_empty() {
        return Err(conflict("set the sender and at least one recipient first"));
    }
    let transport = (call.ui.mailer)(&token).map_err(internal)?;
    let sent_at = jiff::Timestamp::from_millisecond(call.now())
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default();
    let layout = Layout::new(&call.ui.identity.name, "", &sent_at);
    let model = TestEmail {
        requested_by: call.by(),
        settings_url: String::new(),
    };
    let mut failures = Vec::new();
    for to in &p.notify.to {
        let email = Email {
            from: p.notify.from.clone(),
            to: to.clone(),
            template: TestEmail::ALIAS.to_string(),
            model: render(&layout, &model),
            tag: "test".into(),
            alert_id: None,
        };
        match postmark::send(transport.as_ref(), &email).await {
            Outcome::Sent { .. } => {}
            Outcome::Retry(e) => failures.push(format!("{to}: try again later ({e})")),
            Outcome::BadToken => failures.push("Postmark refused the server token".into()),
            Outcome::TemplateMissing => failures.push(format!(
                "Postmark has no template {}; push the templates first",
                TestEmail::ALIAS
            )),
            Outcome::SenderRefused => failures.push(format!(
                "Postmark refused the sender address {}",
                p.notify.from
            )),
            Outcome::InactiveRecipient => {
                failures.push(format!("{to} is marked inactive in Postmark"))
            }
            Outcome::Failed(e) => failures.push(format!("{to}: {e}")),
        }
    }
    failures.dedup();
    call.audit(
        "test_email",
        serde_json::json!({"to": p.notify.to, "failures": failures}),
    );
    json(ApplianceTestEmail {
        sent: failures.is_empty(),
        message: if failures.is_empty() {
            format!("sent to {}", p.notify.to.join(", "))
        } else {
            failures.join("; ")
        },
    })
}

fn audit(call: &Call<'_>) -> Answer {
    let limit = call
        .query
        .and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("limit="))
                .and_then(|n| n.parse::<usize>().ok())
        })
        .unwrap_or(200)
        .clamp(1, 5000);
    let entries = call.ui.audit.recent(limit).map_err(internal)?;
    json(AuditView {
        entries: entries
            .into_iter()
            .map(|e| AuditEntryView {
                at_ms: e.at_ms,
                event: e.event,
                by: e.by,
                details: e.details,
            })
            .collect(),
    })
}
