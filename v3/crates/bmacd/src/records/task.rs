// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Keeps the record store within its budget every hour, and publishes the
//! `logs` observation every minute: the store's size, the last audit
//! eviction, and the online peers whose copies of this host's records lag.

use std::sync::Arc;
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_proto::status::{LOGS_COLLECTOR, LogObservation};

use super::retention::{self, Policy};
use crate::auth::Clock;
use crate::engine::events::EventHub;
use crate::log;
use crate::mesh::Mesh;
use crate::status::Publisher;
use crate::store::Store;

pub const OBSERVE_EVERY: Duration = Duration::from_secs(60);
pub const RETAIN_EVERY: Duration = Duration::from_secs(60 * 60);

pub struct RecordsTask {
    pub host: HostSlot,
    pub store: Store,
    pub mesh: Option<Arc<Mesh>>,
    pub hub: Arc<EventHub>,
    pub clock: Clock,
    pub policy: Policy,
}

impl RecordsTask {
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut publisher = Publisher::new(self.hub.clone(), self.host, self.clock.clone());
            let mut retained: Option<tokio::time::Instant> = None;
            let mut budget = self.policy.budget_bytes;
            let mut tick = tokio::time::interval(OBSERVE_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let now = (self.clock)();
                if retained.is_none_or(|t| t.elapsed() >= RETAIN_EVERY) {
                    retained = Some(tokio::time::Instant::now());
                    let space = retention::space(
                        self.store
                            .segments()
                            .parent()
                            .unwrap_or(self.store.segments()),
                    );
                    match retention::enforce(&self.store, self.policy, now, space).await {
                        Ok(report) => {
                            budget = report.budget_bytes;
                            if report.aged > 0 || report.evicted.iter().any(|n| *n > 0) {
                                log::info(&format!(
                                    "log retention: deleted {} record(s) for age, evicted {:?} by class for space",
                                    report.aged, report.evicted
                                ));
                            }
                        }
                        Err(e) => log::warning(&format!("log retention: {e}")),
                    }
                }
                publisher.publish(LOGS_COLLECTOR, self.observe(now, budget).await);
            }
        })
    }

    async fn observe(&self, now: i64, budget_bytes: u64) -> LogObservation {
        let (used_bytes, oldest_ms, audit_evicted_ms) =
            retention::usage(&self.store).await.unwrap_or_default();
        let (lagging, peers_checked) = match &self.mesh {
            Some(mesh) => mesh.lagging().await,
            None => (Vec::new(), 0),
        };
        LogObservation {
            at_ms: now,
            used_bytes,
            budget_bytes,
            oldest_ms,
            audit_evicted_ms,
            lagging,
            peers_checked,
        }
    }
}
