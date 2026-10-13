// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The front end's side of the engine socket. Unary calls share one
//! connection and are matched to replies by ID. Each stream gets its own
//! connection, so closing the stream ends the engine's work for it.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use bmac_proto::engine::{EngineCall, EngineOutcome, EngineReply, EngineRequest, MAX_REPLY_FRAME};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::log;
use crate::wire::{self, WireError};

/// How long a unary call may take before the front end gives up on it.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_QUEUE: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach the engine at {path}: {source}")]
    Connect {
        path: String,
        source: std::io::Error,
    },
    #[error("the engine closed the connection")]
    Disconnected,
    #[error("the engine did not answer within {0:?}")]
    Timeout(Duration),
    #[error("{0} is a stream; open it as one")]
    StreamCall(&'static str),
    #[error("{0} is not a stream")]
    NotStream(&'static str),
    #[error(transparent)]
    Wire(#[from] WireError),
}

#[derive(Default)]
struct Waiters {
    by_id: HashMap<u64, oneshot::Sender<EngineOutcome>>,
    /// Set by the reader as it stops, under the same lock, so no caller can
    /// register after the last reply was read.
    closed: bool,
}

type Pending = Arc<StdMutex<Waiters>>;

fn lock(pending: &Pending) -> std::sync::MutexGuard<'_, Waiters> {
    pending.lock().unwrap_or_else(|e| e.into_inner())
}

struct Connection {
    write: Mutex<tokio::net::unix::OwnedWriteHalf>,
    pending: Pending,
    reader: JoinHandle<()>,
}

impl Connection {
    fn alive(&self) -> bool {
        !self.reader.is_finished()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

pub struct EngineClient {
    path: PathBuf,
    shared: Mutex<Option<Arc<Connection>>>,
    next_id: AtomicU64,
}

impl EngineClient {
    pub fn new(path: impl Into<PathBuf>) -> EngineClient {
        EngineClient {
            path: path.into(),
            shared: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    fn request(&self, client: IpAddr, token: Option<String>, call: EngineCall) -> EngineRequest {
        EngineRequest {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            client,
            token,
            call,
        }
    }

    async fn connect(path: &Path) -> Result<UnixStream, ClientError> {
        UnixStream::connect(path)
            .await
            .map_err(|source| ClientError::Connect {
                path: path.display().to_string(),
                source,
            })
    }

    async fn shared(&self) -> Result<Arc<Connection>, ClientError> {
        let mut shared = self.shared.lock().await;
        if let Some(conn) = shared.as_ref().filter(|c| c.alive()) {
            return Ok(conn.clone());
        }
        let (read, write) = Self::connect(&self.path).await?.into_split();
        let pending: Pending = Arc::default();
        let reader = tokio::spawn(read_replies(read, pending.clone()));
        let conn = Arc::new(Connection {
            write: Mutex::new(write),
            pending,
            reader,
        });
        *shared = Some(conn.clone());
        Ok(conn)
    }

    /// Makes one unary call and returns the engine's reply.
    pub async fn call(
        &self,
        client: IpAddr,
        token: Option<String>,
        call: EngineCall,
    ) -> Result<EngineOutcome, ClientError> {
        if call.is_stream() {
            return Err(ClientError::StreamCall(call.name()));
        }
        let request = self.request(client, token, call);
        let frame = wire::encode(&request)?;
        let conn = self.shared().await?;
        let (tx, rx) = oneshot::channel();
        {
            let mut waiters = lock(&conn.pending);
            if waiters.closed {
                return Err(ClientError::Disconnected);
            }
            waiters.by_id.insert(request.id, tx);
        }
        let sent = conn.write.lock().await.write_all(&frame).await;
        if sent.is_err() {
            lock(&conn.pending).by_id.remove(&request.id);
            conn.reader.abort();
            return Err(ClientError::Disconnected);
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(_)) => Err(ClientError::Disconnected),
            Err(_) => {
                lock(&conn.pending).by_id.remove(&request.id);
                Err(ClientError::Timeout(CALL_TIMEOUT))
            }
        }
    }

    /// Opens a stream call on its own connection.
    pub async fn stream(
        &self,
        client: IpAddr,
        token: Option<String>,
        call: EngineCall,
    ) -> Result<EngineStream, ClientError> {
        if !call.is_stream() {
            return Err(ClientError::NotStream(call.name()));
        }
        let request = self.request(client, token, call);
        let mut stream = Self::connect(&self.path).await?;
        wire::write_frame(&mut stream, &request).await?;
        let (read, write) = stream.into_split();
        let (tx, rx) = mpsc::channel(STREAM_QUEUE);
        let task = tokio::spawn(async move {
            let _write = write;
            let mut reader = BufReader::new(read);
            loop {
                let reply = tokio::select! {
                    () = tx.closed() => break,
                    reply = wire::read_frame::<EngineReply, _>(&mut reader, MAX_REPLY_FRAME) => reply,
                };
                match reply {
                    Ok(Some(reply)) => {
                        if tx.send(reply.outcome).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        log::warning(&format!("engine stream: {e}"));
                        break;
                    }
                }
            }
        });
        Ok(EngineStream { rx, task })
    }
}

/// The outcomes of one stream call. Dropping it closes the connection.
pub struct EngineStream {
    rx: mpsc::Receiver<EngineOutcome>,
    task: JoinHandle<()>,
}

impl EngineStream {
    /// The next outcome; `None` once the engine has closed the stream.
    pub async fn next(&mut self) -> Option<EngineOutcome> {
        self.rx.recv().await
    }
}

impl Drop for EngineStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_replies(read: OwnedReadHalf, pending: Pending) {
    let mut reader = BufReader::new(read);
    loop {
        match wire::read_frame::<EngineReply, _>(&mut reader, MAX_REPLY_FRAME).await {
            Ok(Some(reply)) => {
                let waiter = lock(&pending).by_id.remove(&reply.id);
                match waiter {
                    Some(waiter) => {
                        let _ = waiter.send(reply.outcome);
                    }
                    None => log::warning(&format!(
                        "engine socket: a reply for unknown request {}",
                        reply.id
                    )),
                }
            }
            Ok(None) => break,
            Err(e) => {
                log::warning(&format!("engine socket: {e}"));
                break;
            }
        }
    }
    // Dropping the waiters tells every caller the connection is gone.
    let mut waiters = lock(&pending);
    waiters.closed = true;
    waiters.by_id.clear();
}
