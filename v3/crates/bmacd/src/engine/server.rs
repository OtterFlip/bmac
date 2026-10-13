// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The engine socket: binding it with the right owner and mode, checking
//! each peer's UID, and running each request as its own task.

use std::fs;
use std::future::Future;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::engine::{EngineOutcome, EngineReply, EngineRequest, MAX_REQUEST_FRAME};
use tokio::io::{BufReader, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use super::{Handler, Peer, Replies};
use crate::log;
use crate::peers::AllowedPeers;
use crate::wire::{self, WireError};

/// The socket's mode: the owner (root) and the front end's group.
pub const SOCKET_MODE: u32 = 0o660;
/// Requests one connection may have running at once.
pub const MAX_IN_FLIGHT: usize = 64;
/// Replies queued for one connection before handlers wait.
const REPLY_QUEUE: usize = 64;

/// The socket directory's mode: root, and the front end's group may enter.
pub const DIRECTORY_MODE: u32 = 0o750;

/// Gives the socket's directory (systemd's `RuntimeDirectory`, created
/// `root:root`) to `group`, so the front end can reach the socket and
/// nothing else can.
pub fn prepare_directory(directory: &Path, group: u32) -> io::Result<()> {
    std::os::unix::fs::chown(directory, None, Some(group))?;
    fs::set_permissions(directory, fs::Permissions::from_mode(DIRECTORY_MODE))
}

/// Binds the socket at `path`, readable and writable by root and `group`.
/// A stale socket from an earlier run is replaced; any other file there is
/// an error, never deleted.
pub fn bind(path: &Path, group: Option<u32>) -> io::Result<UnixListener> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    if let Some(gid) = group {
        std::os::unix::fs::chown(path, None, Some(gid))?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE))?;
    Ok(listener)
}

/// Accepts connections until `shutdown` resolves, then drops every open
/// connection.
pub async fn serve<H: Handler>(
    listener: UnixListener,
    allowed: AllowedPeers,
    handler: Arc<H>,
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
                    log::warning(&format!("engine socket: accept failed: {e}"));
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.shutdown().await;
}

async fn connection<H: Handler>(stream: UnixStream, allowed: Arc<AllowedPeers>, handler: Arc<H>) {
    let peer = match stream.peer_cred() {
        Ok(cred) => Peer {
            uid: cred.uid(),
            pid: cred.pid(),
        },
        Err(e) => {
            log::warning(&format!("engine socket: no peer credentials: {e}"));
            return;
        }
    };
    if !allowed.allows(peer.uid) {
        log::warning(&format!(
            "engine socket: refused a connection from UID {} (PID {})",
            peer.uid,
            peer.pid
                .map_or_else(|| "unknown".to_string(), |p| p.to_string())
        ));
        return;
    }
    let (read, write) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<EngineReply>(REPLY_QUEUE);
    let writer = tokio::spawn(async move {
        let mut write = BufWriter::new(write);
        while let Some(reply) = rx.recv().await {
            if wire::write_frame(&mut write, &reply).await.is_err() {
                break;
            }
        }
    });

    let limit = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    let mut tasks = JoinSet::new();
    let mut reader = BufReader::new(read);
    loop {
        // Not raced against anything: a partly read line would be lost.
        let frame = wire::read_frame::<EngineRequest, _>(&mut reader, MAX_REQUEST_FRAME).await;
        while tasks.try_join_next().is_some() {}
        match frame {
            Ok(Some(request)) => {
                let Ok(permit) = limit.clone().acquire_owned().await else {
                    break;
                };
                let replies = Replies::new(request.id, tx.clone());
                let handler = handler.clone();
                tasks.spawn(async move {
                    handler.handle(peer, request, replies).await;
                    drop(permit);
                });
            }
            Ok(None) => break,
            Err(e) => {
                // The stream can't be trusted past a bad frame, so say why
                // and close it.
                let message = match &e {
                    WireError::Json(_) => format!("the request is not one the engine accepts: {e}"),
                    _ => e.to_string(),
                };
                let error = ApiError::new(ErrorCode::Invalid, message);
                let _ = tx
                    .send(EngineReply {
                        id: 0,
                        outcome: EngineOutcome::Err { error },
                    })
                    .await;
                break;
            }
        }
    }
    // The front end closed its end or broke the protocol: nobody is left to
    // read the replies, so stop the work done for it.
    tasks.shutdown().await;
    drop(tx);
    let _ = writer.await;
}
