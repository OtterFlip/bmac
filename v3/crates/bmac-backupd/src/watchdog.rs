// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The appliance as the cluster's external watchdog (`MONITORING.md`):
//! every minute it reads each enrolled cluster's heartbeat from every host,
//! and checks the configured public URLs. When no host has answered, or a
//! URL has failed, for `down_after_minutes`, it sends email through its own
//! Postmark settings, and again when they recover. The state is kept in the
//! metadata dataset, so a restart sends nothing twice.

use std::collections::BTreeMap;
use std::time::Duration;

use bmac_notify::postmark::{self, Email, Outcome, Transport};
use bmac_notify::templates::{
    AnsweringHost, Layout, Model, UrlDown, UrlUp, WatchdogDown, WatchdogHost, WatchdogUp, render,
};
use serde::{Deserialize, Serialize};

use crate::audit::Audit;
use crate::fsx;
use crate::journal::Metadata;
use crate::platform::Platform;

pub const EVERY: Duration = Duration::from_secs(60);
const SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovered {
    pub since_ms: i64,
    pub at_ms: i64,
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterWatch {
    pub cluster_uuid: String,
    pub cluster_name: String,
    pub watching_since_ms: i64,
    pub last_seen_ms: BTreeMap<String, i64>,
    /// When the last host answered, once none has for the wait.
    pub down_since_ms: Option<i64>,
    pub down_sent: bool,
    pub up_pending: Option<Recovered>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UrlWatch {
    pub url: String,
    pub last_ok_ms: Option<i64>,
    /// The first failed check of the current run of failures.
    pub failing_since_ms: Option<i64>,
    pub last_error: Option<String>,
    /// It has failed for the whole wait.
    pub down: bool,
    pub down_sent: bool,
    pub up_pending: Option<Recovered>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchState {
    pub schema_version: u32,
    pub clusters: Vec<ClusterWatch>,
    pub urls: Vec<UrlWatch>,
    /// The last email failure that won't succeed by retrying.
    #[serde(default)]
    pub notify_error: Option<String>,
}

impl WatchState {
    fn path(metadata: &Metadata) -> std::path::PathBuf {
        metadata.root().join("watchdog.json")
    }

    pub fn load(metadata: &Metadata) -> std::io::Result<WatchState> {
        Ok(fsx::read_json(&Self::path(metadata))?.unwrap_or_default())
    }

    pub fn save(&self, metadata: &Metadata) -> std::io::Result<()> {
        fsx::write_json(&Self::path(metadata), self, 0o644)
    }
}

/// An email the state says is due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mail {
    ClusterDown {
        since_ms: i64,
        hosts: Vec<(String, Option<i64>)>,
    },
    ClusterUp(Recovered),
    UrlDown {
        since_ms: i64,
        error: String,
    },
    UrlUp(Recovered),
}

impl ClusterWatch {
    /// Records one round of heartbeats from `hosts`, of which `answered`
    /// answered; the email now due.
    pub fn observe(
        &mut self,
        hosts: &[String],
        answered: &[String],
        now_ms: i64,
        down_after_ms: i64,
    ) -> Option<Mail> {
        for host in answered {
            self.last_seen_ms.insert(host.clone(), now_ms);
        }
        if !answered.is_empty() {
            if let Some(since) = self.down_since_ms.take()
                && self.down_sent
            {
                self.up_pending = Some(Recovered {
                    since_ms: since,
                    at_ms: now_ms,
                    hosts: answered.to_vec(),
                });
            }
            self.down_sent = false;
        } else if self.down_since_ms.is_none() {
            let last = self
                .last_seen_ms
                .values()
                .copied()
                .max()
                .unwrap_or(self.watching_since_ms);
            if now_ms - last >= down_after_ms {
                self.down_since_ms = Some(last);
                self.up_pending = None;
            }
        }
        self.due(hosts)
    }

    fn due(&self, hosts: &[String]) -> Option<Mail> {
        match (self.down_since_ms, &self.up_pending) {
            (Some(since), _) if !self.down_sent => Some(Mail::ClusterDown {
                since_ms: since,
                hosts: hosts
                    .iter()
                    .map(|h| (h.clone(), self.last_seen_ms.get(h).copied()))
                    .collect(),
            }),
            (None, Some(up)) => Some(Mail::ClusterUp(up.clone())),
            _ => None,
        }
    }

    pub fn sent(&mut self, mail: &Mail) {
        match mail {
            Mail::ClusterDown { .. } => self.down_sent = true,
            Mail::ClusterUp(_) => self.up_pending = None,
            _ => {}
        }
    }
}

impl UrlWatch {
    pub fn observe(
        &mut self,
        result: &Result<(), String>,
        now_ms: i64,
        down_after_ms: i64,
    ) -> Option<Mail> {
        match result {
            Ok(()) => {
                self.last_ok_ms = Some(now_ms);
                self.last_error = None;
                if let Some(since) = self.failing_since_ms.take()
                    && self.down_sent
                {
                    self.up_pending = Some(Recovered {
                        since_ms: since,
                        at_ms: now_ms,
                        hosts: Vec::new(),
                    });
                }
                self.down_sent = false;
                self.down = false;
            }
            Err(e) => {
                self.last_error = Some(e.clone());
                let since = *self.failing_since_ms.get_or_insert(now_ms);
                if now_ms - since >= down_after_ms && !self.down {
                    self.down = true;
                    self.up_pending = None;
                }
            }
        }
        match (self.failing_since_ms, &self.up_pending) {
            (Some(since), _) if self.down && !self.down_sent => Some(Mail::UrlDown {
                since_ms: since,
                error: self.last_error.clone().unwrap_or_default(),
            }),
            (None, Some(up)) => Some(Mail::UrlUp(up.clone())),
            _ => None,
        }
    }

    pub fn sent(&mut self, mail: &Mail) {
        match mail {
            Mail::UrlDown { .. } => self.down_sent = true,
            Mail::UrlUp(_) => self.up_pending = None,
            _ => {}
        }
    }
}

fn utc(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

/// One cluster's heartbeats this round.
#[derive(Debug, Clone)]
pub struct Round {
    pub cluster_uuid: String,
    pub cluster_name: String,
    pub dashboard_url: String,
    pub hosts: Vec<String>,
    pub answered: Vec<String>,
}

/// Where email goes, when the settings allow it.
pub struct Sender<'a> {
    pub transport: &'a dyn Transport,
    pub from: String,
    pub to: Vec<String>,
}

/// Whether the mail went out, or what stops it for good.
enum Sent {
    Yes,
    Later,
    Never(String),
}

async fn send<M: Model>(sender: &Sender<'_>, layout: &Layout, model: &M) -> Sent {
    let mut worst = Sent::Yes;
    for to in &sender.to {
        let email = Email {
            from: sender.from.clone(),
            to: to.clone(),
            template: M::ALIAS.to_string(),
            model: render(layout, model),
            tag: "watchdog".into(),
            alert_id: None,
        };
        match postmark::send(sender.transport, &email).await {
            Outcome::Sent { .. } | Outcome::InactiveRecipient => {}
            Outcome::Retry(_) | Outcome::TemplateMissing => worst = Sent::Later,
            Outcome::BadToken => return Sent::Never("Postmark refused the server token".into()),
            Outcome::SenderRefused => {
                return Sent::Never(format!(
                    "Postmark refused the sender address {}",
                    sender.from
                ));
            }
            Outcome::Failed(e) => return Sent::Never(e),
        }
    }
    worst
}

async fn deliver(
    sender: Option<&Sender<'_>>,
    layout: &Layout,
    appliance_name: &str,
    subject: &str,
    mail: &Mail,
) -> Sent {
    let Some(sender) = sender else {
        return Sent::Later;
    };
    match mail {
        Mail::ClusterDown { since_ms, hosts } => {
            send(
                sender,
                layout,
                &WatchdogDown {
                    appliance_name: appliance_name.into(),
                    down_since: utc(*since_ms),
                    hosts: hosts
                        .iter()
                        .map(|(h, seen)| WatchdogHost {
                            host: h.clone(),
                            last_seen: seen.map(utc).unwrap_or_else(|| "never".into()),
                        })
                        .collect(),
                },
            )
            .await
        }
        Mail::ClusterUp(up) => {
            send(
                sender,
                layout,
                &WatchdogUp {
                    appliance_name: appliance_name.into(),
                    down_since: utc(up.since_ms),
                    up_at: utc(up.at_ms),
                    hosts: up
                        .hosts
                        .iter()
                        .map(|h| AnsweringHost { host: h.clone() })
                        .collect(),
                },
            )
            .await
        }
        Mail::UrlDown { since_ms, error } => {
            send(
                sender,
                layout,
                &UrlDown {
                    appliance_name: appliance_name.into(),
                    url: subject.into(),
                    failing_since: utc(*since_ms),
                    error: error.clone(),
                },
            )
            .await
        }
        Mail::UrlUp(up) => {
            send(
                sender,
                layout,
                &UrlUp {
                    appliance_name: appliance_name.into(),
                    url: subject.into(),
                    failing_since: utc(up.since_ms),
                    up_at: utc(up.at_ms),
                },
            )
            .await
        }
    }
}

/// One watchdog round: updates the state from the heartbeats and URL
/// checks, and sends what is due.
#[allow(clippy::too_many_arguments)]
pub async fn tick(
    metadata: &Metadata,
    audit: &Audit,
    appliance_name: &str,
    rounds: &[Round],
    urls: &[(String, Result<(), String>)],
    sender: Option<&Sender<'_>>,
    now_ms: i64,
) -> Result<WatchState, String> {
    let platform = Platform::load(metadata).map_err(|e| e.to_string())?;
    let down_after = i64::from(platform.watchdog.down_after_minutes.max(1)) * 60_000;
    let mut state = WatchState::load(metadata).map_err(|e| e.to_string())?;
    state.schema_version = SCHEMA;
    state
        .clusters
        .retain(|c| rounds.iter().any(|r| r.cluster_uuid == c.cluster_uuid));
    state
        .urls
        .retain(|u| urls.iter().any(|(url, _)| *url == u.url));
    let mut notify_error = None;
    let record = |event: &str, subject: &str, since_ms: Option<i64>| {
        eprintln!(
            "bmac-backupd: watchdog: {subject} {}",
            if event.ends_with("down") {
                format!(
                    "has given no good answer since {}",
                    since_ms.map(utc).unwrap_or_default()
                )
            } else {
                "answers again".into()
            }
        );
        let _ = audit.record(
            now_ms,
            event,
            "bmac-backupd",
            serde_json::json!({"subject": subject, "since_ms": since_ms}),
        );
    };
    for round in rounds {
        let position = state
            .clusters
            .iter()
            .position(|c| c.cluster_uuid == round.cluster_uuid);
        let watch = match position {
            Some(i) => &mut state.clusters[i],
            None => {
                state.clusters.push(ClusterWatch {
                    cluster_uuid: round.cluster_uuid.clone(),
                    watching_since_ms: now_ms,
                    ..Default::default()
                });
                state.clusters.last_mut().expect("just pushed")
            }
        };
        watch.cluster_name = round.cluster_name.clone();
        let was_down = watch.down_since_ms;
        let mail = watch.observe(&round.hosts, &round.answered, now_ms, down_after);
        let subject = format!("cluster {}", round.cluster_name);
        match (was_down, watch.down_since_ms) {
            (None, Some(since)) => record("watchdog_down", &subject, Some(since)),
            (Some(since), None) => record("watchdog_up", &subject, Some(since)),
            _ => {}
        }
        let Some(mail) = mail else {
            continue;
        };
        let layout = Layout::new(&round.cluster_name, &round.dashboard_url, &utc(now_ms));
        match deliver(sender, &layout, appliance_name, &round.cluster_name, &mail).await {
            Sent::Yes => watch.sent(&mail),
            Sent::Never(e) => {
                watch.sent(&mail);
                notify_error = Some(e);
            }
            Sent::Later => {}
        }
    }
    let (cluster_name, dashboard_url) = rounds
        .first()
        .map(|r| (r.cluster_name.clone(), r.dashboard_url.clone()))
        .unwrap_or_else(|| (appliance_name.to_string(), String::new()));
    for (url, result) in urls {
        let position = state.urls.iter().position(|u| u.url == *url);
        let watch = match position {
            Some(i) => &mut state.urls[i],
            None => {
                state.urls.push(UrlWatch {
                    url: url.clone(),
                    ..Default::default()
                });
                state.urls.last_mut().expect("just pushed")
            }
        };
        let (was_down, since) = (watch.down, watch.failing_since_ms);
        let mail = watch.observe(result, now_ms, down_after);
        match (was_down, watch.down) {
            (false, true) => record("url_down", url, watch.failing_since_ms),
            (true, false) => record("url_up", url, since),
            _ => {}
        }
        let Some(mail) = mail else {
            continue;
        };
        let layout = Layout::new(&cluster_name, &dashboard_url, &utc(now_ms));
        match deliver(sender, &layout, appliance_name, url, &mail).await {
            Sent::Yes => watch.sent(&mail),
            Sent::Never(e) => {
                watch.sent(&mail);
                notify_error = Some(e);
            }
            Sent::Later => {}
        }
    }
    if let Some(e) = &notify_error
        && state.notify_error.as_ref() != Some(e)
    {
        eprintln!("bmac-backupd: watchdog: email: {e}");
        let _ = audit.record(
            now_ms,
            "notify_failed",
            "bmac-backupd",
            serde_json::json!({"error": e}),
        );
    }
    if notify_error.is_some() || sender.is_some() {
        state.notify_error = notify_error;
    }
    state.save(metadata).map_err(|e| e.to_string())?;
    Ok(state)
}

#[cfg(test)]
#[path = "watchdog_tests.rs"]
mod tests;
