// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Port 8444's update endpoints (`bmac_proto::updates`): a member fetches
//! a staged bundle, the host running an update job asks this host to
//! install, and watches how it goes.

use std::io::Read;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{Uri, header};
use axum::response::Response;
use bmac_core::JobId;
use bmac_core::job::JobState;
use bmac_proto::api::ErrorCode;
use bmac_proto::updates::{
    BUNDLE_PATH, HostUpdateState, InstallMode, InstallRequest, InstallState, InstallStatus,
};
use bmac_release::MAX_BUNDLE;
use bytes::Bytes;
use tokio::sync::mpsc;

use super::{Mesh, error, ok_json, parse};
use crate::updates::install::Begun;
use crate::updates::job::{ROLLBACK_OPERATION, UPDATE_OPERATION};
use crate::web::api::{ChannelBody, api_error};
use crate::web::https::ClientCertificate;

const READ_CHUNK: usize = 256 * 1024;

fn installer(
    mesh: &Mesh,
) -> Result<&crate::updates::install::Installer, bmac_proto::api::ApiError> {
    mesh.installer
        .get()
        .ok_or_else(|| error(ErrorCode::Unavailable, "this host doesn't take updates"))
}

pub(super) async fn serve_bundle(
    State(mesh): State<Arc<Mesh>>,
    axum::Extension(certificate): axum::Extension<ClientCertificate>,
    UrlPath(sha): UrlPath<String>,
) -> Response {
    if let Err(e) = mesh.member(&certificate).await {
        return api_error(e);
    }
    let installer = match installer(&mesh) {
        Ok(installer) => installer,
        Err(e) => return api_error(e),
    };
    let path = match installer.store().bundle_path(&sha) {
        Ok(path) => path,
        Err(e) => return api_error(error(ErrorCode::Invalid, e)),
    };
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => {
            return api_error(error(
                ErrorCode::NotFound,
                format!("bundle {sha} isn't on {}", mesh.host()),
            ));
        }
    };
    let (tx, rx) = mpsc::channel::<Bytes>(4);
    tokio::task::spawn_blocking(move || {
        let mut buffer = vec![0u8; READ_CHUNK];
        loop {
            match file.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if tx
                        .blocking_send(Bytes::copy_from_slice(&buffer[..n]))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
    let mut response = Response::new(Body::new(ChannelBody(rx)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    response
}

pub(super) async fn serve_install(
    State(mesh): State<Arc<Mesh>>,
    axum::Extension(certificate): axum::Extension<ClientCertificate>,
    body: Bytes,
) -> Response {
    let requester = match mesh.member(&certificate).await {
        Ok(host) => host,
        Err(e) => return api_error(e),
    };
    let request: InstallRequest = match parse(&body) {
        Ok(request) => request,
        Err(e) => return api_error(e),
    };
    let installer = match installer(&mesh) {
        Ok(installer) => installer.clone(),
        Err(e) => return api_error(e),
    };
    let Some(runner) = mesh.runner.get() else {
        return api_error(error(ErrorCode::Unavailable, "this host runs no jobs"));
    };
    let jobs = runner.jobs().clone();
    let id = request.job;
    let record = match tokio::task::spawn_blocking(move || jobs.get(id)).await {
        Ok(Ok(record)) => record,
        Ok(Err(e)) => return api_error(e),
        Err(e) => return api_error(error(ErrorCode::Internal, e.to_string())),
    };
    if ![UPDATE_OPERATION, ROLLBACK_OPERATION].contains(&record.operation.as_str())
        || record.state != JobState::Running
        || record.lease.as_ref().is_none_or(|l| l.holder != requester)
    {
        return api_error(error(
            ErrorCode::Forbidden,
            format!("job {id} is no update {requester} is running"),
        ));
    }
    let now = mesh.now();
    let begun = {
        let _guard = mesh.installing.lock().await;
        let installer = installer.clone();
        let request = request.clone();
        tokio::task::spawn_blocking(move || installer.begin(&request, now)).await
    };
    let status = match begun {
        Ok(Ok(Begun::Existing(status))) => return ok_json(status),
        Ok(Ok(Begun::New)) => InstallStatus {
            mode: request.mode,
            state: InstallState::Fetching,
            message: None,
            log_tail: None,
        },
        Ok(Err(e)) => return api_error(error(ErrorCode::Conflict, e)),
        Err(e) => return api_error(error(ErrorCode::Internal, e.to_string())),
    };
    let client = mesh.peer_client();
    tokio::spawn(async move {
        if let Err(e) = fetch_and_launch(&installer, &client, requester, &request, now).await {
            crate::log::warning(&format!("update job {}: {e}", request.job));
            installer.fail(request.job, request.mode, &e);
        }
    });
    ok_json(status)
}

async fn fetch_and_launch(
    installer: &crate::updates::install::Installer,
    client: &super::client::PeerClient,
    requester: bmac_core::HostSlot,
    request: &InstallRequest,
    now: i64,
) -> Result<(), String> {
    let needs = installer.needs(request)?;
    let store = installer.store().clone();
    if !store.has(&needs) {
        let scratch = store.scratch(&format!("{}-{needs}", request.job))?;
        let fetched = client
            .download(
                requester,
                &format!("{BUNDLE_PATH}{needs}"),
                &scratch,
                MAX_BUNDLE,
            )
            .await;
        if let Err(e) = fetched {
            let _ = std::fs::remove_file(&scratch);
            return Err(format!("fetching bundle {needs} from {requester}: {e}"));
        }
        let expect = needs.clone();
        tokio::task::spawn_blocking(move || store.stage(&scratch, Some(&expect), now))
            .await
            .map_err(|e| e.to_string())??;
    }
    installer.launch(request).await
}

/// `job` and `mode` from a state request's query.
fn state_query(uri: &Uri) -> Result<Option<(JobId, InstallMode)>, bmac_proto::api::ApiError> {
    let (mut job, mut mode) = (None, InstallMode::Update);
    for pair in uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
    {
        match pair.split_once('=') {
            Some(("job", value)) => {
                job = Some(
                    value
                        .parse::<JobId>()
                        .map_err(|_| error(ErrorCode::Invalid, "bad job"))?,
                );
            }
            Some(("mode", "update")) => mode = InstallMode::Update,
            Some(("mode", "rollback")) => mode = InstallMode::Rollback,
            _ => {
                return Err(error(
                    ErrorCode::Invalid,
                    format!("bad query parameter {pair:?}"),
                ));
            }
        }
    }
    Ok(job.map(|job| (job, mode)))
}

pub(super) async fn serve_state(
    State(mesh): State<Arc<Mesh>>,
    axum::Extension(certificate): axum::Extension<ClientCertificate>,
    uri: Uri,
) -> Response {
    if let Err(e) = mesh.member(&certificate).await {
        return api_error(e);
    }
    let query = match state_query(&uri) {
        Ok(query) => query,
        Err(e) => return api_error(e),
    };
    let installer = match installer(&mesh) {
        Ok(installer) => installer.clone(),
        Err(e) => return api_error(e),
    };
    let now = mesh.now();
    let reading = installer.clone();
    let read = tokio::task::spawn_blocking(move || {
        let install = match query {
            Some((job, mode)) => reading.status(job, mode, now)?,
            None => None,
        };
        Ok::<_, String>((reading.store().installed(), install))
    })
    .await;
    let (installed, install) = match read {
        Ok(Ok(read)) => read,
        Ok(Err(e)) => return api_error(error(ErrorCode::Internal, e)),
        Err(e) => return api_error(error(ErrorCode::Internal, e.to_string())),
    };
    ok_json(HostUpdateState {
        host: mesh.host(),
        version: crate::VERSION.to_string(),
        installed,
        install,
        services: installer.services().await,
        reachable_peers: mesh.reachable_peers(),
    })
}
