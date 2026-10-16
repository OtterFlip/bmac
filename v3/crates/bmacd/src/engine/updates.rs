// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Dashboard's Updates page: bundles uploaded to this host in chunks,
//! and each host's version and rollback copy, asked over port 8444.

use base64::Engine as _;
use bmac_proto::api::{ApiError, ErrorCode, Role};
use bmac_proto::engine::EngineCall;
use bmac_proto::updates::{
    HostUpdateState, HostUpdates, STATE_PATH, UPLOAD_CHUNK, UpdatesView, UploadStarted,
};

use super::{Engine, Replies, body};
use crate::audit::AuditKind;
use crate::auth::sessions::SessionInfo;

fn unavailable() -> ApiError {
    ApiError::new(
        ErrorCode::Unavailable,
        "updates are not available on this host",
    )
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}

impl Engine {
    pub(super) async fn update_call(
        &self,
        session: &SessionInfo,
        client: std::net::IpAddr,
        call: EngineCall,
        replies: &Replies,
    ) {
        let Some(mesh) = self.mesh.clone() else {
            replies.err(unavailable()).await;
            return;
        };
        let Some(installer) = mesh.installer().cloned() else {
            replies.err(unavailable()).await;
            return;
        };
        if matches!(call, EngineCall::Updates {}) {
            replies
                .ok(body(self.updates_view(&mesh, &installer).await))
                .await;
            return;
        }
        if session.role != Role::Admin {
            replies
                .err(ApiError::new(
                    ErrorCode::Forbidden,
                    "managing updates needs the admin role",
                ))
                .await;
            return;
        }
        let store = installer.store().clone();
        let now = self.auth.now();
        let answered = tokio::task::spawn_blocking(
            move || -> (Result<serde_json::Value, ApiError>, Option<String>) {
                match call {
                    EngineCall::StartUpload { request } => {
                        match store.start_upload(request.size, now) {
                            Ok(upload_id) => (
                                Ok(body(UploadStarted {
                                    upload_id,
                                    chunk: UPLOAD_CHUNK,
                                })),
                                None,
                            ),
                            Err(e) => (Err(invalid(e)), None),
                        }
                    }
                    EngineCall::UploadChunk { upload_id, request } => {
                        let data = match base64::engine::general_purpose::STANDARD
                            .decode(request.data.as_bytes())
                        {
                            Ok(data) if data.len() as u64 <= UPLOAD_CHUNK => data,
                            Ok(_) => return (Err(invalid("the chunk is too large")), None),
                            Err(_) => return (Err(invalid("the chunk isn't base64")), None),
                        };
                        match store.append_upload(&upload_id, request.offset, &data) {
                            Ok(received) => (Ok(serde_json::json!({ "received": received })), None),
                            Err(e) => (Err(ApiError::new(ErrorCode::Conflict, e)), None),
                        }
                    }
                    EngineCall::FinishUpload { upload_id } => {
                        match store.finish_upload(&upload_id, now) {
                            Ok(summary) => {
                                let message = format!(
                                    "uploaded release bundle {} (BMAC {}, signed with the {} key)",
                                    summary.sha256, summary.version, summary.signed_by
                                );
                                (Ok(body(summary)), Some(message))
                            }
                            Err(e) => (Err(invalid(e)), None),
                        }
                    }
                    EngineCall::DeleteBundle { sha256 } => match store.delete(&sha256) {
                        Ok(()) => (
                            Ok(serde_json::json!({})),
                            Some(format!("deleted release bundle {sha256}")),
                        ),
                        Err(e) => (Err(ApiError::new(ErrorCode::Conflict, e)), None),
                    },
                    _ => (Err(unavailable()), None),
                }
            },
        )
        .await
        .unwrap_or_else(|e| (Err(ApiError::new(ErrorCode::Internal, e.to_string())), None));
        let (outcome, message) = answered;
        if let Some(message) = message {
            self.auth
                .audit()
                .record(
                    self.auth.now(),
                    AuditKind::SettingsChanged,
                    Some(&session.user),
                    Some(client),
                    message,
                )
                .await;
        }
        match outcome {
            Ok(value) => {
                replies.ok(value).await;
            }
            Err(error) => {
                replies.err(error).await;
            }
        }
    }

    async fn updates_view(
        &self,
        mesh: &std::sync::Arc<crate::mesh::Mesh>,
        installer: &crate::updates::install::Installer,
    ) -> UpdatesView {
        let store = installer.store().clone();
        let keys = store.keys().iter().map(|k| k.name.to_string()).collect();
        let bundles = tokio::task::spawn_blocking(move || store.staged())
            .await
            .unwrap_or_default();
        let members = mesh.member_hosts().await.unwrap_or_default();
        let client = mesh.peer_client();
        let mut asks = tokio::task::JoinSet::new();
        for host in members.iter().copied() {
            let client = client.clone();
            asks.spawn(async move {
                let state: Result<HostUpdateState, String> =
                    client.json(host, "GET", STATE_PATH, None).await;
                (host, state)
            });
        }
        let mut hosts = Vec::new();
        while let Some(Ok((host, state))) = asks.join_next().await {
            hosts.push(match state {
                Ok(state) => HostUpdates {
                    host,
                    version: Some(state.version),
                    installed: state.installed,
                    error: None,
                },
                Err(e) => HostUpdates {
                    host,
                    version: None,
                    installed: Default::default(),
                    error: Some(e),
                },
            });
        }
        hosts.sort_by_key(|h| h.host);
        UpdatesView {
            this_host: mesh.host(),
            hosts,
            bundles,
            trusted_keys: keys,
        }
    }
}
