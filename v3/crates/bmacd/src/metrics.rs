// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The metric history behind the Dashboard's charts and the pool-full
//! projection (`MONITORING.md`, "Collectors"): selected numbers from every
//! host's newest observations, sampled each minute, kept at one-minute
//! resolution for 7 days and hourly for 90. Every host keeps every host's
//! history, so any host can draw any chart.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_proto::status::{GUESTS_COLLECTOR, HOST_COLLECTOR, STORAGE_COLLECTOR};
use rusqlite::params;
use serde_json::Value;

use crate::auth::Clock;
use crate::db::{Database, DbError};
use crate::engine::events::EventHub;

pub const MINUTE_MS: i64 = 60_000;
pub const HOUR_MS: i64 = 60 * MINUTE_MS;
pub const MINUTES_KEPT_MS: i64 = 7 * 24 * HOUR_MS;
pub const HOURS_KEPT_MS: i64 = 90 * 24 * HOUR_MS;
/// An observation older than this is not sampled: its host has gone quiet.
/// Unchanged observations are republished every two minutes.
const STALE_MS: i64 = 3 * MINUTE_MS;

/// Days until each pool is full, by host and pool, from its use over the
/// last 7 days.
pub type Projections = Arc<Mutex<BTreeMap<(HostSlot, String), f64>>>;

/// How many days until a used fraction reaches 1 at the rate of the
/// least-squares line through `points`, or `None` when it isn't growing or
/// there are fewer than a day's worth of points.
pub fn days_until_full(points: &[Point]) -> Option<f64> {
    let first = points.first()?.time_ms;
    let last = points.last()?;
    if points.len() < 2 || last.time_ms - first < 24 * HOUR_MS {
        return None;
    }
    let n = points.len() as f64;
    let xs: Vec<f64> = points
        .iter()
        .map(|p| (p.time_ms - first) as f64 / (24 * HOUR_MS) as f64)
        .collect();
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = points.iter().map(|p| p.value).sum::<f64>() / n;
    let (mut covariance, mut variance) = (0.0, 0.0);
    for (x, p) in xs.iter().zip(points) {
        covariance += (x - mean_x) * (p.value - mean_y);
        variance += (x - mean_x) * (x - mean_x);
    }
    let per_day = covariance / variance;
    if !per_day.is_finite() || per_day <= 0.0 {
        return None;
    }
    let now = mean_y + per_day * (xs[xs.len() - 1] - mean_x);
    Some(((1.0 - now) / per_day).max(0.0))
}

/// The numbers one observation contributes, by metric name.
pub fn samples(collector: &str, data: &Value, now_ms: i64) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    let number = |v: &Value, key: &str| v.get(key).and_then(Value::as_f64);
    match collector {
        HOST_COLLECTOR => {
            if let Some(cpu) = number(data, "cpu_fraction") {
                out.push(("cpu".to_string(), cpu));
            }
            if let Some(load) = number(data, "load1") {
                out.push(("load1".to_string(), load));
            }
            if let (Some(used), Some(total)) =
                (number(data, "memory_used"), number(data, "memory_total"))
                && total > 0.0
            {
                out.push(("memory".to_string(), used / total));
            }
        }
        STORAGE_COLLECTOR => {
            for pool in data["pools"].as_array().into_iter().flatten() {
                if let (Some(name), Some(free)) =
                    (pool["name"].as_str(), number(pool, "free_fraction"))
                {
                    out.push((format!("pool.{name}.used"), 1.0 - free));
                }
            }
        }
        GUESTS_COLLECTOR => {
            for job in data["replication"].as_array().into_iter().flatten() {
                if let (Some(id), Some(last)) = (job["id"].as_str(), job["last_sync_ms"].as_i64()) {
                    let lag = (now_ms - last).max(0) as f64 / 1000.0;
                    out.push((format!("replication.{id}.lag_seconds"), lag));
                }
            }
        }
        _ => {}
    }
    out
}

/// One point of a series: the bucket's start and its average.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub time_ms: i64,
    pub value: f64,
}

#[derive(Clone, Debug)]
pub struct Metrics {
    db: Database,
}

impl Metrics {
    pub fn new(db: Database) -> Metrics {
        Metrics { db }
    }

    /// Adds samples to their minute and hour buckets.
    pub async fn record(
        &self,
        host: HostSlot,
        time_ms: i64,
        samples: Vec<(String, f64)>,
    ) -> Result<(), DbError> {
        let host = host.to_string();
        self.db
            .run(move |c| {
                let tx = c.unchecked_transaction()?;
                {
                    let mut add = tx.prepare_cached(
                        "INSERT INTO metrics (host, metric, resolution_ms, time_ms, sum, count)
                         VALUES (?1, ?2, ?3, ?4, ?5, 1)
                         ON CONFLICT (host, metric, resolution_ms, time_ms)
                         DO UPDATE SET sum = sum + excluded.sum, count = count + 1",
                    )?;
                    for (metric, value) in &samples {
                        for resolution in [MINUTE_MS, HOUR_MS] {
                            let bucket = time_ms - time_ms.rem_euclid(resolution);
                            add.execute(params![host, metric, resolution, bucket, value])?;
                        }
                    }
                }
                tx.commit()
            })
            .await
    }

    /// Drops minute buckets older than 7 days and hour buckets older than 90.
    pub async fn prune(&self, now_ms: i64) -> Result<usize, DbError> {
        self.db
            .run(move |c| {
                c.execute(
                    "DELETE FROM metrics WHERE (resolution_ms = ?1 AND time_ms < ?2)
                     OR (resolution_ms = ?3 AND time_ms < ?4)",
                    params![
                        MINUTE_MS,
                        now_ms - MINUTES_KEPT_MS,
                        HOUR_MS,
                        now_ms - HOURS_KEPT_MS
                    ],
                )
            })
            .await
    }

    /// A metric's points since `since_ms`, oldest first, at one-minute
    /// resolution when that reaches back far enough, else hourly.
    pub async fn series(
        &self,
        host: HostSlot,
        metric: &str,
        since_ms: i64,
        now_ms: i64,
    ) -> Result<Vec<Point>, DbError> {
        let resolution = if now_ms - since_ms <= MINUTES_KEPT_MS {
            MINUTE_MS
        } else {
            HOUR_MS
        };
        let (host, metric) = (host.to_string(), metric.to_string());
        self.db
            .run(move |c| {
                let mut query = c.prepare_cached(
                    "SELECT time_ms, sum / count FROM metrics
                     WHERE host = ?1 AND metric = ?2 AND resolution_ms = ?3 AND time_ms >= ?4
                     ORDER BY time_ms",
                )?;
                let rows = query.query_map(params![host, metric, resolution, since_ms], |row| {
                    Ok(Point {
                        time_ms: row.get(0)?,
                        value: row.get(1)?,
                    })
                })?;
                rows.collect()
            })
            .await
    }

    /// Every pool's days until full, from the pools seen in `hub`.
    async fn project(&self, hub: &EventHub, now_ms: i64) -> BTreeMap<(HostSlot, String), f64> {
        let mut out = BTreeMap::new();
        for host in HostSlot::all() {
            let Some(storage) = hub.observation(host, STORAGE_COLLECTOR) else {
                continue;
            };
            for pool in storage["pools"].as_array().into_iter().flatten() {
                let Some(name) = pool["name"].as_str() else {
                    continue;
                };
                let metric = format!("pool.{name}.used");
                if let Ok(points) = self
                    .series(host, &metric, now_ms - MINUTES_KEPT_MS, now_ms)
                    .await
                    && let Some(days) = days_until_full(&points)
                {
                    out.insert((host, name.to_string()), days);
                }
            }
        }
        out
    }

    /// Samples every host's fresh observations once a minute; prunes and
    /// projects pool growth into `projections` once an hour.
    pub fn spawn(
        self,
        hub: Arc<EventHub>,
        clock: Clock,
        projections: Projections,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(MINUTE_MS as u64));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_prune = i64::MIN;
            loop {
                tick.tick().await;
                let now = clock();
                for host in HostSlot::all() {
                    let mut taken = Vec::new();
                    for (collector, data) in hub.observations_of(host) {
                        let at = data["at_ms"].as_i64().unwrap_or(i64::MIN);
                        if now - at <= STALE_MS {
                            taken.extend(samples(&collector, &data, now));
                        }
                    }
                    if !taken.is_empty()
                        && let Err(e) = self.record(host, now, taken).await
                    {
                        crate::log::warning(&format!("metric history: {e}"));
                    }
                }
                if now - last_prune >= HOUR_MS {
                    last_prune = now;
                    if let Err(e) = self.prune(now).await {
                        crate::log::warning(&format!("metric history: {e}"));
                    }
                    let projected = self.project(&hub, now).await;
                    *projections.lock().unwrap_or_else(|p| p.into_inner()) = projected;
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn samples_come_from_host_pool_and_replication_numbers() {
        let host = json!({"cpu_fraction": 0.25, "load1": 1.5, "memory_used": 2, "memory_total": 8});
        assert_eq!(
            samples(HOST_COLLECTOR, &host, 0),
            [
                ("cpu".into(), 0.25),
                ("load1".into(), 1.5),
                ("memory".into(), 0.25)
            ]
        );
        let storage =
            json!({"pools": [{"name": "rpool", "free_fraction": 0.75}, {"name": "tank"}]});
        assert_eq!(
            samples(STORAGE_COLLECTOR, &storage, 0),
            [("pool.rpool.used".into(), 0.25)]
        );
        let guests = json!({"replication": [{"id": "101-0", "last_sync_ms": 1_000}]});
        assert_eq!(
            samples(GUESTS_COLLECTOR, &guests, 31_000),
            [("replication.101-0.lag_seconds".into(), 30.0)]
        );
        assert!(samples("units", &json!({}), 0).is_empty());
    }

    #[test]
    fn a_growing_pool_is_projected_to_fill() {
        let day = 24 * HOUR_MS;
        let points: Vec<Point> = (0..=4)
            .map(|d| Point {
                time_ms: d * day,
                value: 0.5 + 0.05 * d as f64,
            })
            .collect();
        let days = days_until_full(&points).unwrap();
        assert!((days - 6.0).abs() < 1e-6, "{days}");
        let flat: Vec<Point> = (0..=4)
            .map(|d| Point {
                time_ms: d * day,
                value: 0.5,
            })
            .collect();
        assert_eq!(days_until_full(&flat), None);
        assert_eq!(days_until_full(&points[..1]), None);
        let short = [
            Point {
                time_ms: 0,
                value: 0.1,
            },
            Point {
                time_ms: HOUR_MS,
                value: 0.2,
            },
        ];
        assert_eq!(
            days_until_full(&short),
            None,
            "an hour is too little to project from"
        );
    }

    #[tokio::test]
    async fn buckets_average_and_age_out_by_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let metrics = Metrics::new(Database::open(&dir.path().join("bmac.db")).unwrap());
        let host = HostSlot::new(1).unwrap();
        let start = 1_000 * HOUR_MS;
        metrics
            .record(host, start, vec![("cpu".into(), 0.2)])
            .await
            .unwrap();
        metrics
            .record(host, start + 10_000, vec![("cpu".into(), 0.4)])
            .await
            .unwrap();
        metrics
            .record(host, start + MINUTE_MS, vec![("cpu".into(), 0.6)])
            .await
            .unwrap();

        let minutes = metrics
            .series(host, "cpu", start, start + HOUR_MS)
            .await
            .unwrap();
        assert_eq!(minutes.len(), 2);
        assert!(
            (minutes[0].value - 0.3).abs() < 1e-9,
            "one minute averages its samples"
        );
        assert_eq!(minutes[1].time_ms, start + MINUTE_MS);

        let long_ago = start + 30 * 24 * HOUR_MS;
        let hours = metrics.series(host, "cpu", start, long_ago).await.unwrap();
        assert_eq!(hours.len(), 1);
        assert!(
            (hours[0].value - 0.4).abs() < 1e-9,
            "the hour averages all three"
        );

        metrics.prune(long_ago).await.unwrap();
        assert!(
            metrics
                .series(host, "cpu", start, start + HOUR_MS)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            metrics
                .series(host, "cpu", start, long_ago)
                .await
                .unwrap()
                .len(),
            1
        );
        metrics.prune(start + 91 * 24 * HOUR_MS).await.unwrap();
        assert!(
            metrics
                .series(host, "cpu", start, long_ago)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
