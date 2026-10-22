// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! BMAC's example production app: a tiny todo list served over HTTPS by one
//! process, with its data in one SQLite file. No web server or database
//! service is needed.
//!
//!   hello-todo --listen [::]:443 --cert CERT.pem --key KEY.pem --db todos.sqlite3
//!
//! The page is at `/`; the JSON API is under `/api/v1/` (BMAC's Cloudflare
//! setup never challenges paths under `/api/`):
//!
//!   GET    /api/v1/info          host name, todo count and limit
//!   GET    /api/v1/todos         every todo
//!   POST   /api/v1/todos         {"title": "..."}            -> 201
//!   GET    /api/v1/todos/{id}
//!   PATCH  /api/v1/todos/{id}    {"title": "...", "done": true}
//!   DELETE /api/v1/todos/{id}                                -> 204

mod store;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use store::{Store, StoreError, MAX_TODOS};

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const APP_CSS: &str = include_str!("web/app.css");
const MAX_BODY_BYTES: usize = 16 * 1024;

#[derive(Clone)]
struct App {
    store: Arc<Mutex<Store>>,
    hostname: Arc<str>,
}

struct Options {
    listen: SocketAddr,
    cert: String,
    key: String,
    db: String,
}

fn usage() -> ! {
    eprintln!("usage: hello-todo --listen ADDRESS:PORT --cert CERT.pem --key KEY.pem --db FILE");
    std::process::exit(2)
}

fn parse_options() -> Options {
    let mut listen = None;
    let (mut cert, mut key, mut db) = (None, None, None);
    let mut arguments = std::env::args().skip(1);
    while let Some(flag) = arguments.next() {
        let value = arguments.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--listen" => listen = Some(value.parse().unwrap_or_else(|_| usage())),
            "--cert" => cert = Some(value),
            "--key" => key = Some(value),
            "--db" => db = Some(value),
            _ => usage(),
        }
    }
    match (listen, cert, key, db) {
        (Some(listen), Some(cert), Some(key), Some(db)) => Options {
            listen,
            cert,
            key,
            db,
        },
        _ => usage(),
    }
}

/// An error answered as `{"error": "..."}`.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Invalid(message) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, message),
            StoreError::NotFound => ApiError(StatusCode::NOT_FOUND, "todo not found".into()),
            StoreError::Full => ApiError(
                StatusCode::CONFLICT,
                format!("the list is full: at most {MAX_TODOS} todos; delete one first"),
            ),
            StoreError::Database(message) => {
                eprintln!("database error: {message}");
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, "database error".into())
            }
        }
    }
}

/// Run a store operation off the async runtime.
async fn with_store<T, F>(app: &App, operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
{
    let store = app.store.clone();
    tokio::task::spawn_blocking(move || {
        let mut store = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        operation(&mut store)
    })
    .await
    .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()))?
    .map_err(ApiError::from)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewTodo {
    title: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TodoChange {
    title: Option<String>,
    done: Option<bool>,
}

fn json_body<T>(
    body: Result<Json<T>, axum::extract::rejection::JsonRejection>,
) -> Result<T, ApiError> {
    body.map(|Json(value)| value)
        .map_err(|rejection| ApiError(rejection.status(), rejection.body_text()))
}

async fn info(State(app): State<App>) -> Result<Json<serde_json::Value>, ApiError> {
    let count = with_store(&app, |store| store.count()).await?;
    Ok(Json(
        json!({ "hostname": &*app.hostname, "count": count, "max_todos": MAX_TODOS }),
    ))
}

async fn list(State(app): State<App>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(with_store(&app, |store| store.list()).await?))
}

async fn create(
    State(app): State<App>,
    body: Result<Json<NewTodo>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let NewTodo { title } = json_body(body)?;
    let todo = with_store(&app, move |store| store.create(&title)).await?;
    Ok((StatusCode::CREATED, Json(todo)))
}

async fn get_one(
    State(app): State<App>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(with_store(&app, move |store| store.get(id)).await?))
}

async fn update(
    State(app): State<App>,
    Path(id): Path<i64>,
    body: Result<Json<TodoChange>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let TodoChange { title, done } = json_body(body)?;
    Ok(Json(
        with_store(&app, move |store| store.update(id, title.as_deref(), done)).await?,
    ))
}

async fn delete(
    State(app): State<App>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    with_store(&app, move |store| store.delete(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

async fn not_found(request: Request) -> Response {
    if request.uri().path().starts_with("/api/") {
        ApiError(StatusCode::NOT_FOUND, "no such API route".into()).into_response()
    } else {
        (StatusCode::NOT_FOUND, "not found\n").into_response()
    }
}

/// Security headers on every response; API answers are never cached.
async fn headers(request: Request, next: Next) -> Response {
    let api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if api {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

fn router(app: App) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }),
        )
        .route(
            "/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route("/api/v1/info", get(info))
        .route("/api/v1/todos", get(list).post(create))
        .route(
            "/api/v1/todos/{id}",
            get(get_one).patch(update).delete(delete),
        )
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(headers))
        .with_state(app)
}

fn tls_acceptor(cert: &str, key: &str) -> Result<TlsAcceptor, String> {
    let certs = CertificateDer::pem_file_iter(cert)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|error| format!("cannot read certificate {cert}: {error}"))?;
    if certs.is_empty() {
        return Err(format!("{cert} contains no certificate"));
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|error| format!("cannot read key {key}: {error}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| format!("certificate and key don't match: {error}"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[tokio::main]
async fn main() {
    let options = parse_options();
    let store = Store::open(&options.db).unwrap_or_else(|error| {
        eprintln!("cannot open database {}: {error:?}", options.db);
        std::process::exit(1)
    });
    let acceptor = tls_acceptor(&options.cert, &options.key).unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(1)
    });
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let app = router(App {
        store: Arc::new(Mutex::new(store)),
        hostname: hostname.into(),
    });
    let listener = TcpListener::bind(options.listen)
        .await
        .unwrap_or_else(|error| {
            eprintln!("cannot listen on {}: {error}", options.listen);
            std::process::exit(1)
        });
    eprintln!("hello-todo listening on https://{}", options.listen);

    let terminate = async {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install the SIGTERM handler");
        signal.recv().await;
    };
    tokio::pin!(terminate);
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(connection) => connection,
                Err(error) => {
                    eprintln!("accept failed: {error}");
                    continue;
                }
            },
            _ = &mut terminate => break,
        };
        let acceptor = acceptor.clone();
        let service = TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            // Failed handshakes (scanners, wrong SNI) are not worth logging.
            let Ok(tls) = acceptor.accept(stream).await else {
                return;
            };
            let _ = auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(tls), service)
                .await;
        });
    }
    eprintln!("hello-todo stopping");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use tower_service::Service;

    fn test_app() -> Router {
        router(App {
            store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
            hostname: "testhost".into(),
        })
    }

    async fn call(
        app: &mut Router,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> (StatusCode, String, Response) {
        let mut request = axum::http::Request::builder().method(method).uri(uri);
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let request = request
            .body(Body::from(body.unwrap_or("").to_string()))
            .unwrap();
        let response = app.call(request).await.unwrap();
        let status = response.status();
        let (parts, body) = response.into_parts();
        let text = String::from_utf8(to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap();
        (status, text, Response::from_parts(parts, Body::empty()))
    }

    #[tokio::test]
    async fn api_round_trip() {
        let mut app = test_app();
        let (status, body, _) = call(
            &mut app,
            "POST",
            "/api/v1/todos",
            Some(r#"{"title":"Buy milk"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let todo: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = todo["id"].as_i64().unwrap();

        let (status, body, _) = call(
            &mut app,
            "PATCH",
            &format!("/api/v1/todos/{id}"),
            Some(r#"{"done":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["done"],
            true
        );

        let (status, body, response) = call(&mut app, "GET", "/api/v1/todos", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Vec<serde_json::Value>>(&body)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");

        let (status, body, _) = call(&mut app, "GET", "/api/v1/info", None).await;
        assert_eq!(status, StatusCode::OK);
        let info: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (info["hostname"].as_str(), info["count"].as_i64()),
            (Some("testhost"), Some(1))
        );

        let (status, _, _) = call(&mut app, "DELETE", &format!("/api/v1/todos/{id}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, body, _) = call(&mut app, "GET", &format!("/api/v1/todos/{id}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("todo not found"));
    }

    #[tokio::test]
    async fn bad_requests_get_json_errors() {
        let mut app = test_app();
        for (method, uri, body, expected) in [
            (
                "POST",
                "/api/v1/todos",
                r#"{"title":""}"#,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                "POST",
                "/api/v1/todos",
                r#"{"title":"x","extra":1}"#,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            ("POST", "/api/v1/todos", "not json", StatusCode::BAD_REQUEST),
            (
                "PATCH",
                "/api/v1/todos/1",
                r#"{"done":true}"#,
                StatusCode::NOT_FOUND,
            ),
            ("GET", "/api/v1/nope", "", StatusCode::NOT_FOUND),
        ] {
            let (status, text, _) = call(&mut app, method, uri, Some(body)).await;
            assert_eq!(status, expected, "{method} {uri} {body}: {text}");
            assert!(
                serde_json::from_str::<serde_json::Value>(&text).unwrap()["error"].is_string(),
                "{text}"
            );
        }
        let oversized = format!(r#"{{"title":"{}"}}"#, "x".repeat(MAX_BODY_BYTES));
        let (status, _, _) = call(&mut app, "POST", "/api/v1/todos", Some(&oversized)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn the_page_and_assets_are_served_with_a_strict_policy() {
        let mut app = test_app();
        for path in ["/", "/app.js", "/app.css"] {
            let (status, _, response) = call(&mut app, "GET", path, None).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert!(response.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .starts_with("default-src 'self'"));
        }
        let (status, _, _) = call(&mut app, "GET", "/missing", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
