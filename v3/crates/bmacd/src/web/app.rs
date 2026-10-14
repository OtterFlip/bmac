// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The front end's HTTP routes and the response headers every reply gets
//! (`SECURITY.md`, "Requests"). The front end relays API calls to the
//! engine; it reads tokens only to pass them on and never stores them.

use std::sync::Arc;

use std::time::Duration;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use axum::{Extension, Router};
use bmac_proto::api::API_PREFIX;
use bmac_proto::engine::Collection;
use bmac_proto::stream::KEEPALIVE_SECONDS;
use ring::rand::{SecureRandom, SystemRandom};

use super::api;
pub use super::api::api_error;
use super::assets::{self, Assets};
use super::client::EngineClient;
use super::https::ClientAddr;

#[derive(Clone)]
pub struct AppState {
    pub assets: Arc<Assets>,
    pub engine: Arc<EngineClient>,
    /// How often an idle event stream gets a keep-alive comment.
    pub keepalive: Duration,
}

/// The largest request body the front end reads.
pub const MAX_BODY: usize = 1024 * 1024;

pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
    img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'";

/// Stands in for the page's style nonce in the built `index.html`.
pub const STYLE_NONCE_PLACEHOLDER: &str = "__BMAC_STYLE_NONCE__";

/// The policy for the app page. xterm and Radix insert `<style>` elements at
/// run time; the page tags them with this nonce, which is new on every load.
/// Scripts stay `'self'` only.
pub fn page_security_policy(nonce: &str) -> String {
    CONTENT_SECURITY_POLICY.replacen(
        "style-src 'self'",
        &format!("style-src 'self' 'nonce-{nonce}'"),
        1,
    )
}

fn style_nonce() -> Option<String> {
    let mut bytes = [0u8; 16];
    SystemRandom::new().fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Headers on every response. The app page's policy adds its style nonce. No cookies, no CORS, and no HSTS (the
/// certificate is self-signed).
pub const SECURITY_HEADERS: [(&str, &str); 7] = [
    ("content-security-policy", CONTENT_SECURITY_POLICY),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("x-frame-options", "DENY"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
    (
        "permissions-policy",
        "camera=(), microphone=(), geolocation=(), usb=(), payment=()",
    ),
];

async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        let name = HeaderName::from_static(name);
        if name == header::CONTENT_SECURITY_POLICY && headers.contains_key(&name) {
            continue;
        }
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers.remove(header::SET_COOKIE);
    response
}

async fn static_file(State(state): State<AppState>, method: Method, uri: Uri) -> Response {
    let assets = &state.assets;
    if method != Method::GET && method != Method::HEAD {
        let mut headers = HeaderMap::new();
        headers.insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return (StatusCode::METHOD_NOT_ALLOWED, headers).into_response();
    }
    let Some((path, bytes)) = assets.resolve(uri.path()) else {
        return (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "no-cache")],
            "not found\n",
        )
            .into_response();
    };
    // Vite names built assets by content hash, so they never change.
    let cache = if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let headers = [
        (header::CONTENT_TYPE, assets::content_type(path)),
        (header::CACHE_CONTROL, cache),
    ];
    if path == "/index.html" {
        let Some(nonce) = style_nonce() else {
            return (StatusCode::INTERNAL_SERVER_ERROR, "no random source\n").into_response();
        };
        let page = String::from_utf8_lossy(bytes).replace(STYLE_NONCE_PLACEHOLDER, &nonce);
        let policy = HeaderValue::from_str(&page_security_policy(&nonce)).expect("ASCII policy");
        let mut response = if method == Method::HEAD {
            let mut response = (headers, ()).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(page.len()));
            response
        } else {
            (headers, page).into_response()
        };
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, policy);
        return response;
    }
    if method == Method::HEAD {
        let mut response = (headers, ()).into_response();
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
        return response;
    }
    (headers, bytes).into_response()
}

pub fn router(assets: Arc<Assets>, engine: Arc<EngineClient>) -> Router {
    router_with(AppState {
        assets,
        engine,
        keepalive: Duration::from_secs(KEEPALIVE_SECONDS),
    })
}

fn collection(collection: Collection) -> axum::routing::MethodRouter<AppState> {
    get(
        move |State(state): State<AppState>,
              Extension(client): Extension<ClientAddr>,
              headers: HeaderMap,
              uri: Uri| { api::list(collection, state, client, headers, uri) },
    )
}

pub fn router_with(state: AppState) -> Router {
    let api_routes = Router::new()
        .route("/login", post(api::login))
        .route("/logout", post(api::logout))
        .route("/cluster", get(api::cluster))
        .route("/hosts", collection(Collection::Hosts))
        .route("/vms", collection(Collection::Vms))
        .route("/storage", collection(Collection::Storage))
        .route("/backups", collection(Collection::Backups))
        .route("/alerts", get(api::alerts))
        .route("/alerts/{id}/acknowledge", post(api::acknowledge_alert))
        .route("/silences", post(api::save_silence))
        .route("/silences/{id}/delete", post(api::delete_silence))
        .route("/notify/settings", put(api::save_notify_settings))
        .route("/notify/token", put(api::set_postmark_token))
        .route("/notify/test", post(api::send_test_email))
        .route(
            "/notify/templates",
            get(api::email_templates).post(api::create_email_templates),
        )
        .route("/logs/search", post(api::search_logs))
        .route("/logs/replication", get(api::log_replication))
        .route("/operations", get(api::operations))
        .route("/setup", get(api::setup))
        .route("/settings", get(api::settings))
        .route("/settings/cluster", put(api::save_cluster_settings))
        .route("/settings/hosts/{host}", put(api::save_host_facts))
        .route("/certificate", get(api::certificate))
        .route("/joins", get(api::joins).post(api::issue_join))
        .route("/joins/{host}/revoke", post(api::revoke_join))
        .route("/appliances", get(api::appliances).post(api::issue_enroll))
        .route(
            "/appliances/requests/{id}/approve",
            post(api::approve_appliance),
        )
        .route(
            "/appliances/requests/{id}/reject",
            post(api::reject_appliance),
        )
        .route(
            "/appliances/{appliance}/revoke",
            post(api::revoke_appliance),
        )
        .route(
            "/appliances/{appliance}/address",
            post(api::set_appliance_address),
        )
        .route(
            "/backups/policies",
            get(api::backup_policies).post(api::save_backup_policy),
        )
        .route(
            "/backups/policies/{vm}/delete",
            post(api::delete_backup_policy),
        )
        .route("/certificate/rotate", post(api::rotate_certificate))
        .route("/certificate/switch", post(api::switch_certificate))
        .route("/certificate/discard", post(api::discard_certificate))
        .route("/jobs", get(api::jobs).post(api::launch_job))
        .route("/jobs/{id}", get(api::job))
        .route("/jobs/{id}/requests/{request}", post(api::answer))
        .route("/jobs/{id}/cancel", post(api::cancel))
        .route("/jobs/{id}/resume", post(api::resume))
        .route("/jobs/{id}/reconcile", post(api::reconcile))
        .route("/events", get(api::events))
        .route("/events/watch", post(api::watch))
        .route("/", any(api::not_found))
        .fallback(api::not_found)
        .method_not_allowed_fallback(api::method_not_allowed);
    Router::new()
        .nest(API_PREFIX, api_routes)
        .fallback(static_file)
        .with_state(state)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn(security_headers))
}
