// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The socket between `bmac-source` and the engine: its own directory and
//! group, so the front end can't reach it and `bmac-source` can't reach the
//! engine socket, and a fixed set of calls, each naming the client
//! certificate's fingerprint so the engine decides every request itself.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bmac_exec::BoxFuture;
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::appliance::{
    CheckinRequest, ClaimRequest, IndexRequest, LocateRequest, SendRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{BufReader, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;

use super::source::TransferDone;
use crate::log;
use crate::peers::AllowedPeers;
use crate::wire;

pub const SOURCE_USER: &str = "bmac-source";
pub const SOURCE_DIRECTORY: &str = "/run/bmac-source";
pub const SOURCE_SOCKET: &str = "/run/bmac-source/engine.sock";
/// An appliance's index is the largest call.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceCall {
    /// The fingerprints to let through the TLS handshake.
    Fingerprints,
    /// The private and Tailscale addresses to listen on.
    Listen,
    Checkin {
        fingerprint: String,
        request: Box<CheckinRequest>,
    },
    Claim {
        fingerprint: String,
        request: ClaimRequest,
    },
    Index {
        fingerprint: String,
        request: IndexRequest,
    },
    Catalog {
        fingerprint: String,
    },
    Locate {
        fingerprint: String,
        request: LocateRequest,
    },
    AuthorizeSend {
        fingerprint: String,
        request: SendRequest,
    },
    TransferDone {
        fingerprint: String,
        report: TransferDone,
    },
    Heartbeat {
        fingerprint: String,
    },
    LuksHeaders {
        fingerprint: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SourceReply {
    Ok { body: Value },
    Err { error: ApiError },
}

pub trait SourceHandler: Send + Sync + 'static {
    fn handle(&self, call: SourceCall) -> BoxFuture<'_, Result<Value, ApiError>>;
}

/// Makes the socket's directory, entered only by root and `group`.
pub fn prepare_directory(directory: &Path, group: u32) -> io::Result<()> {
    std::fs::create_dir_all(directory)?;
    crate::engine::server::prepare_directory(directory, group)
}

/// Answers calls until `shutdown` resolves, one at a time per connection.
pub async fn serve(
    listener: UnixListener,
    allowed: AllowedPeers,
    handler: Arc<dyn SourceHandler>,
    shutdown: impl Future<Output = ()>,
) {
    let allowed = Arc::new(allowed);
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(connection(stream, allowed.clone(), handler.clone()));
                }
                Err(e) => {
                    log::warning(&format!("source socket: accept failed: {e}"));
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.shutdown().await;
}

async fn connection(
    stream: UnixStream,
    allowed: Arc<AllowedPeers>,
    handler: Arc<dyn SourceHandler>,
) {
    let uid = match stream.peer_cred() {
        Ok(cred) => cred.uid(),
        Err(e) => {
            log::warning(&format!("source socket: no peer credentials: {e}"));
            return;
        }
    };
    if !allowed.allows(uid) {
        log::warning(&format!(
            "source socket: refused a connection from UID {uid}"
        ));
        return;
    }
    let (read, write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut writer = BufWriter::new(write);
    loop {
        let reply = match wire::read_frame::<SourceCall, _>(&mut reader, MAX_FRAME).await {
            Ok(None) => return,
            Ok(Some(call)) => match handler.handle(call).await {
                Ok(body) => SourceReply::Ok { body },
                Err(error) => SourceReply::Err { error },
            },
            Err(e) => SourceReply::Err {
                error: ApiError::new(ErrorCode::Invalid, format!("not a source call: {e}")),
            },
        };
        let failed = matches!(&reply, SourceReply::Err { error } if error.code == ErrorCode::Invalid
            && error.message.starts_with("not a source call"));
        if wire::write_frame(&mut writer, &reply).await.is_err() || failed {
            return;
        }
    }
}

/// `bmac-source`'s side: one connection per call.
#[derive(Debug, Clone)]
pub struct SourceClient {
    path: PathBuf,
}

impl SourceClient {
    pub fn new(path: impl Into<PathBuf>) -> SourceClient {
        SourceClient { path: path.into() }
    }

    pub async fn call(&self, call: &SourceCall) -> Result<Value, ApiError> {
        let unavailable =
            |e: String| ApiError::new(ErrorCode::Unavailable, format!("the engine: {e}"));
        let stream = UnixStream::connect(&self.path)
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        let (read, mut write) = stream.into_split();
        wire::write_frame(&mut write, call)
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        let mut reader = BufReader::new(read);
        match wire::read_frame::<SourceReply, _>(&mut reader, MAX_FRAME).await {
            Ok(Some(SourceReply::Ok { body })) => Ok(body),
            Ok(Some(SourceReply::Err { error })) => Err(error),
            Ok(None) => Err(unavailable("closed the connection".into())),
            Err(e) => Err(unavailable(e.to_string())),
        }
    }

    pub async fn typed<T: serde::de::DeserializeOwned>(
        &self,
        call: &SourceCall,
    ) -> Result<T, ApiError> {
        let body = self.call(call).await?;
        serde_json::from_value(body)
            .map_err(|e| ApiError::new(ErrorCode::Internal, format!("the engine's answer: {e}")))
    }
}
