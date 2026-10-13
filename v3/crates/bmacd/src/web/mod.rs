// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The HTTPS front end, as the unprivileged user `bmac-web`. It relays the
//! browser's calls to the engine and can send nothing else.

pub mod api;
pub mod app;
pub mod assets;
pub mod client;
pub mod https;
pub mod tls;

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use bmac_proto::engine::{EngineCall, EngineOutcome, Hello};

use self::client::{ClientError, EngineClient};
use self::https::{HttpsServer, Listeners};
use crate::log;

/// How often the front end asks the engine for its listen addresses.
pub const REFRESH: Duration = Duration::from_secs(30);

/// The credentials the front end's unit passes with `LoadCredential`.
pub const KEY_CREDENTIAL: &str = "https.key";
pub const CERT_CREDENTIAL: &str = "https.pem";

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Engine(#[from] ClientError),
    #[error("the engine answered hello with an error: {0}")]
    Refused(String),
    #[error("the engine's hello reply could not be read: {0}")]
    BadHello(String),
    #[error("the engine is version {engine} but this front end is {front_end}; restart bmacd-web")]
    VersionMismatch { engine: String, front_end: String },
}

/// Asks the engine for its version and checks it matches this binary.
pub async fn hello(client: &EngineClient, version: &str) -> Result<Hello, StartError> {
    let local = IpAddr::V4(Ipv4Addr::LOCALHOST);
    match client.call(local, None, EngineCall::Hello {}).await? {
        EngineOutcome::Ok { body } => {
            let hello: Hello =
                serde_json::from_value(body).map_err(|e| StartError::BadHello(e.to_string()))?;
            if hello.version != version {
                return Err(StartError::VersionMismatch {
                    engine: hello.version,
                    front_end: version.to_string(),
                });
            }
            Ok(hello)
        }
        EngineOutcome::Err { error } => Err(StartError::Refused(error.message)),
        EngineOutcome::Event { .. } => Err(StartError::BadHello(
            "an event instead of a reply".to_string(),
        )),
    }
}

/// Applies the engine's listen addresses, logging what changed.
pub fn apply_listen(listeners: &mut Listeners, hello: &Hello) {
    let before = listeners.bound();
    for (address, error) in listeners.reconcile(&hello.listen) {
        log::warning(&format!("https: cannot listen on {address}: {error}"));
    }
    let after = listeners.bound();
    if before != after {
        let list = after
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        log::info(&format!(
            "https: listening on {}",
            if list.is_empty() {
                "nothing yet"
            } else {
                &list
            }
        ));
    }
}

/// Serves HTTPS on the engine's addresses until `stop` resolves, asking the
/// engine again every [`REFRESH`]. Returns an error when the engine turns
/// out to be a different version, so systemd restarts this process.
pub async fn run(
    client: &EngineClient,
    version: &str,
    server: Arc<HttpsServer>,
    first: Hello,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), StartError> {
    let mut listeners = Listeners::new(server);
    apply_listen(&mut listeners, &first);
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => return Ok(()),
            () = tokio::time::sleep(REFRESH) => {}
        }
        match hello(client, version).await {
            Ok(hello) => apply_listen(&mut listeners, &hello),
            Err(e @ StartError::VersionMismatch { .. }) => return Err(e),
            // The engine restarting is brief, and BindsTo stops this unit
            // if it stays down; keep serving meanwhile.
            Err(e) => log::warning(&format!("https: cannot refresh listen addresses: {e}")),
        }
    }
}

/// Retries [`hello`] while the engine socket isn't there yet.
pub async fn wait_for_engine(
    client: &EngineClient,
    version: &str,
    attempts: u32,
) -> Result<Hello, StartError> {
    let mut last = None;
    for attempt in 0..attempts.max(1) {
        match hello(client, version).await {
            Err(StartError::Engine(
                e @ (ClientError::Connect { .. } | ClientError::Disconnected),
            )) => {
                last = Some(e);
                if attempt + 1 < attempts {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            other => return other,
        }
    }
    Err(StartError::Engine(
        last.unwrap_or(ClientError::Disconnected),
    ))
}
