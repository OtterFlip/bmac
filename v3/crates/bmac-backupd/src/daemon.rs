// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `bmac-backupd run`: an [`Agent`] per enrolled cluster, started as the
//! enrollments appear, all writing the one bound pool.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_exec::RealRunner;
use bmac_proto::appliance::SOURCE_PORT;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::audit::Audit;
use crate::checkin::{Agent, ClusterStatus};
use crate::client::ClusterClient;
use crate::export::Exporter;
use crate::grants::Grants;
use crate::journal::{Layout, Metadata};
use crate::platform::Platform;
use crate::pool::{Pool, RealPool, pool_guid};
use crate::roots::Roots;
use crate::scrub::{self, IoGate};
use crate::setup::metadata_mounted;
use crate::state::{self, Identity, Paths, PoolBinding};
use crate::{admin, peers, retention, ui, urlcheck, watchdog};

static RUNNER: RealRunner = RealRunner;
const RESCAN: Duration = Duration::from_secs(60);
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
const SCRUB_CHECK: Duration = Duration::from_secs(300);
/// Exports, to restore jobs and trusted peers (`BACKUP.md`, "Protocol").
pub const EXPORT_PORT: u16 = SOURCE_PORT;

/// Each enrolled cluster's agent and its tasks, by cluster UUID.
type Agents = BTreeMap<String, (Arc<Agent>, Vec<JoinHandle<()>>)>;

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

pub async fn run(
    paths: Paths,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    let identity: Identity = state::load(&paths.identity())
        .map_err(|e| e.to_string())?
        .ok_or("this appliance has no identity; run bmac-backupd init first")?;
    let binding: PoolBinding = state::load(&paths.pool())
        .map_err(|e| e.to_string())?
        .ok_or("this appliance has no pool; run bmac-backupd init first")?;
    let cert = std::fs::read_to_string(paths.cert())
        .map_err(|e| format!("{}: {e}", paths.cert().display()))?;
    let key = std::fs::read_to_string(paths.key())
        .map_err(|e| format!("{}: {e}", paths.key().display()))?;
    let layout = Layout {
        pool: binding.pool.clone(),
    };
    let pool: Arc<dyn Pool> = Arc::new(RealPool {
        runner: &RUNNER,
        pool: binding.pool.clone(),
    });
    let unlocked = {
        let (layout, binding) = (layout.clone(), binding.clone());
        Arc::new(move || {
            let (layout, binding) = (layout.clone(), binding.clone());
            Box::pin(async move {
                matches!(pool_guid(&RUNNER, &binding.pool).await, Ok(Some(g)) if g == binding.pool_guid)
                    && metadata_mounted(&RUNNER, &layout).await
            }) as bmac_exec::BoxFuture<'static, bool>
        })
    };
    if !unlocked().await {
        eprintln!(
            "bmac-backupd: pool {} (GUID {}) isn't imported; checking in as locked",
            binding.pool, binding.pool_guid
        );
    }
    let audit = Arc::new(Audit::new(&paths.metadata()));
    let metadata = Metadata::new(paths.metadata());
    let io = Arc::new(IoGate::default());
    let exporter = Exporter::new(
        paths.clone(),
        identity.clone(),
        metadata.clone(),
        audit.clone(),
        pool.clone(),
        unlocked.clone(),
        Arc::new(now_ms),
        &cert,
        &key,
    )?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv6Addr::UNSPECIFIED, EXPORT_PORT))
        .await
        .map_err(|e| format!("exports: port {EXPORT_PORT}: {e}"))?;
    let tunnels = exporter.tunnels.clone();
    let exports = tokio::spawn(Arc::new(exporter).serve(listener));
    let agents: Arc<Mutex<Agents>> = Arc::default();
    let scrub_now = Arc::new(Notify::new());
    let ui_state = {
        let agents = agents.clone();
        let clusters: ui::Clusters = Arc::new(move || {
            agents
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .values()
                .map(|(agent, _)| ui::ClusterSnapshot {
                    status: agent
                        .status
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .clone(),
                    hosts: agent
                        .enrollment
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .hosts
                        .iter()
                        .map(|h| h.host.to_string())
                        .collect(),
                    transfers: agent
                        .progress
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .clone(),
                })
                .collect()
        });
        let (probe_cert, probe_key) = (cert.clone(), key.clone());
        let probe: ui::Prober = Arc::new(move |wanted, address| {
            let (cert, key) = (probe_cert.clone(), probe_key.clone());
            Box::pin(
                async move { peers::probe(&wanted, &address, (&cert, &key), EXPORT_PORT).await },
            )
        });
        let mailer: ui::Mailer = Arc::new(|token| {
            bmac_notify::postmark::Https::new(token)
                .map(|t| Arc::new(t) as Arc<dyn bmac_notify::postmark::Transport>)
                .map_err(|e| e.to_string())
        });
        Arc::new(ui::Ui {
            paths: paths.clone(),
            identity: identity.clone(),
            fingerprint: state::pem_fingerprint(&cert)?,
            metadata: metadata.clone(),
            audit: audit.clone(),
            pool: pool.clone(),
            binding: binding.clone(),
            unlocked: unlocked.clone(),
            clusters,
            probe,
            mailer,
            now_ms: Arc::new(now_ms),
            rounds: admin::ROUNDS,
            scrub_now: scrub_now.clone(),
            sessions: Default::default(),
            assets: ui::assets::Assets::embedded(),
        })
    };
    let ui_listener = tokio::net::TcpListener::bind((std::net::Ipv6Addr::UNSPECIFIED, ui::UI_PORT))
        .await
        .map_err(|e| format!("ui: port {}: {e}", ui::UI_PORT))?;
    let ui_task = tokio::spawn(ui::serve(
        ui_listener,
        ui::tls_config(&cert, &key)?,
        ui_state,
    ));
    let rescan = async {
        loop {
            for enrollment in state::enrollments(&paths).unwrap_or_default() {
                {
                    // A recovery binds the namespace to a new cluster's CA:
                    // the old agent goes, and one for the new CA starts.
                    let mut running = agents.lock().unwrap_or_else(|p| p.into_inner());
                    let stale = running.get(&enrollment.cluster_uuid).map(|(agent, _)| {
                        agent
                            .enrollment
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .proxmox_ca_fingerprint
                            != enrollment.proxmox_ca_fingerprint
                    });
                    match stale {
                        Some(false) => continue,
                        Some(true) => {
                            if let Some((_, tasks)) = running.remove(&enrollment.cluster_uuid) {
                                for task in tasks {
                                    task.abort();
                                }
                            }
                        }
                        None => {}
                    }
                }
                let client = match ClusterClient::new(
                    &enrollment.proxmox_ca_fingerprint,
                    Some((&cert, &key)),
                    SOURCE_PORT,
                ) {
                    Ok(c) => c.via(tunnels.clone(), &enrollment.cluster_uuid),
                    Err(e) => {
                        eprintln!("bmac-backupd: cluster {}: {e}", enrollment.cluster_name);
                        continue;
                    }
                };
                eprintln!(
                    "bmac-backupd: checking in with cluster {} ({})",
                    enrollment.cluster_name, enrollment.cluster_uuid
                );
                let status = ClusterStatus {
                    cluster_uuid: enrollment.cluster_uuid.clone(),
                    cluster_name: enrollment.cluster_name.clone(),
                    ..Default::default()
                };
                let agent = Arc::new(Agent {
                    paths: paths.clone(),
                    identity: identity.clone(),
                    client,
                    enrollment: Mutex::new(enrollment.clone()),
                    layout: layout.clone(),
                    metadata: metadata.clone(),
                    audit: audit.clone(),
                    pool: pool.clone(),
                    unlocked: unlocked.clone(),
                    progress: Arc::default(),
                    failed: Mutex::default(),
                    status: Mutex::new(status),
                    offers: Mutex::default(),
                    offered: Notify::new(),
                    wake: Notify::new(),
                    now_ms: Arc::new(now_ms),
                    tls_identity: (cert.clone(), key.clone()),
                    repairing: Default::default(),
                    io: io.clone(),
                    recovery_pulled: Mutex::default(),
                });
                let tasks = vec![
                    tokio::spawn(agent.clone().run_checkins()),
                    tokio::spawn(agent.clone().run_pulls()),
                ];
                agents
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(enrollment.cluster_uuid.clone(), (agent, tasks));
            }
            tokio::time::sleep(RESCAN).await;
        }
    };
    let prunes = async {
        loop {
            tokio::time::sleep(PRUNE_EVERY).await;
            if !unlocked().await {
                continue;
            }
            let mut pins = Grants::load(&metadata).unwrap_or_default().pins(now_ms());
            for (pit, reasons) in Roots::load(&metadata).unwrap_or_default().pins(now_ms()) {
                pins.entry(pit).or_default().extend(reasons);
            }
            match retention::prune(pool.as_ref(), &metadata, &audit, &pins, now_ms()).await {
                Ok(r) if r.pruned.is_empty() && r.skipped.is_empty() => {}
                Ok(r) => eprintln!(
                    "bmac-backupd: pruned {} PITs (about {} bytes); {} stay after a failure",
                    r.pruned.len(),
                    r.reclaim_bytes,
                    r.skipped.len()
                ),
                Err(e) => eprintln!("bmac-backupd: retention: {e}"),
            }
        }
    };
    let scrubs = async {
        loop {
            if unlocked().await
                && let Err(e) =
                    scrub::tick(&RUNNER, &binding.pool, &metadata, &audit, &io, now_ms()).await
            {
                eprintln!("bmac-backupd: scrub: {e}");
            }
            tokio::select! {
                () = tokio::time::sleep(SCRUB_CHECK) => {}
                () = scrub_now.notified() => {}
            }
        }
    };
    let watchdog = async {
        let tls = bmac_notify::postmark::system_tls();
        loop {
            tokio::time::sleep(watchdog::EVERY).await;
            if !unlocked().await {
                continue;
            }
            let platform = Platform::load(&metadata).unwrap_or_default();
            let current: Vec<Arc<Agent>> = agents
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .values()
                .map(|(a, _)| a.clone())
                .collect();
            let mut rounds = tokio::task::JoinSet::new();
            for agent in current {
                rounds.spawn(async move { agent.round().await });
            }
            let mut checks = tokio::task::JoinSet::new();
            for url in platform.watchdog.urls.clone() {
                let tls = tls.clone();
                checks.spawn(async move {
                    let result = match tls {
                        Ok(tls) => urlcheck::check(&url, tls).await,
                        Err(e) => Err(e),
                    };
                    (url, result)
                });
            }
            let rounds: Vec<watchdog::Round> = rounds.join_all().await;
            let urls = checks.join_all().await;
            let transport = platform
                .can_notify(&metadata)
                .then(|| Platform::token(&metadata))
                .flatten()
                .and_then(|t| bmac_notify::postmark::Https::new(&t).ok());
            let sender = transport.as_ref().map(|t| watchdog::Sender {
                transport: t,
                from: platform.notify.from.clone(),
                to: platform.notify.to.clone(),
            });
            if let Err(e) = watchdog::tick(
                &metadata,
                &audit,
                &identity.name,
                &rounds,
                &urls,
                sender.as_ref(),
                now_ms(),
            )
            .await
            {
                eprintln!("bmac-backupd: watchdog: {e}");
            }
        }
    };
    tokio::select! {
        () = rescan => {}
        () = watchdog => {}
        () = prunes => {}
        () = scrubs => {}
        () = shutdown => {}
    }
    exports.abort();
    ui_task.abort();
    Ok(())
}
