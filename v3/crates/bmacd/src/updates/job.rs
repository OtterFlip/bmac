// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `cluster.update` and `cluster.rollback`. The job runs on the host the
//! bundle was uploaded to and has every host, itself included, install
//! over port 8444, one at a time and itself last, checking each host's
//! health before the next. The first failure stops the update and offers
//! to roll back the hosts it updated. Installing on its own host restarts
//! `bmacd`, which stops this process; the new `bmacd` resumes the job from
//! its checkpoints.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use bmac_core::job::{FieldType, PhaseStatus, Severity};
use bmac_core::{HostSlot, JobId};
use bmac_exec::BoxFuture;
use bmac_proto::api::Role;
use bmac_proto::operations::{
    CancelRule, Category, Operation, OperationKind, ResumeRule, TargetRule,
};
use bmac_proto::updates::{
    BundleSummary, HostUpdateState, INSTALL_PATH, InstallMode, InstallRequest, InstallState,
    InstallStatus, STATE_PATH,
};
use bmac_release::{compare_versions, debian_version};
use serde_json::{Value, json};

use super::UpdateStore;
use crate::cluster::Membership;
use crate::native::disks::{choice, field, text};
use crate::native::{Checkpoints, Stop, Workflow};

pub const UPDATE_OPERATION: &str = "cluster.update";
pub const ROLLBACK_OPERATION: &str = "cluster.rollback";
const BUNDLE_PHASE: (&str, &str) = ("bundle", "Check the bundle");
const CHECK_PHASE: (&str, &str) = ("check", "Check the cluster");
const INSTALL_PHASE: (&str, &str) = ("install", "Update the hosts one at a time");
const ROLLBACK_PHASE: (&str, &str) = ("rollback", "Roll the hosts back one at a time");
/// The pmxcfs record of the record schema the cluster's data is in.
pub const RELEASE_RECORD: &str = "/etc/pve/priv/bmac/release.json";
const URL_CHOICE: &str = "url";

pub fn update_operation() -> Operation {
    Operation {
        id: UPDATE_OPERATION.parse().expect("a valid operation id"),
        title: "Update the cluster".into(),
        summary: "Install a release bundle on every host, one host at a time.".into(),
        description: "Installs a signed release bundle uploaded to this host, or fetched from \
                      a URL, on every host one at a time, checking each host's health before \
                      the next. The first failure stops the update and offers to roll back the \
                      hosts already updated."
            .into(),
        category: Category::Cluster,
        kind: OperationKind::Native,
        script: None,
        min_role: Role::Admin,
        mutating: true,
        destructive: false,
        supports_dry_run: false,
        params: Vec::new(),
        locks: vec!["update".into(), "cluster-membership".into()],
        target: TargetRule::Any {},
        cancel: CancelRule::Phases {
            not_during: vec![INSTALL_PHASE.0.into(), ROLLBACK_PHASE.0.into()],
        },
        resume: ResumeRule::Checkpoint,
        notes: Some("New in v3.".into()),
    }
}

pub fn rollback_operation() -> Operation {
    Operation {
        id: ROLLBACK_OPERATION.parse().expect("a valid operation id"),
        title: "Roll back a cluster update".into(),
        summary: "Return the hosts running a bundle to the build each ran before it.".into(),
        description: "For each host running the chosen bundle, installs the bundle it ran \
                      before, one host at a time, checking each host's health before the next."
            .into(),
        category: Category::Cluster,
        kind: OperationKind::Native,
        script: None,
        min_role: Role::Admin,
        mutating: true,
        destructive: false,
        supports_dry_run: false,
        params: Vec::new(),
        locks: vec!["update".into(), "cluster-membership".into()],
        target: TargetRule::Any {},
        cancel: CancelRule::Phases {
            not_during: vec![ROLLBACK_PHASE.0.into()],
        },
        resume: ResumeRule::Checkpoint,
        notes: Some("New in v3.".into()),
    }
}

/// What the job needs of the cluster.
pub trait UpdateCluster: Send + Sync {
    fn me(&self) -> HostSlot;
    fn membership(&self) -> Result<Membership, String>;
    /// A host's state, and the install `job` asked of it in `mode`.
    fn state(
        &self,
        host: HostSlot,
        job: JobId,
        mode: InstallMode,
    ) -> BoxFuture<'_, Result<HostUpdateState, String>>;
    fn install<'a>(
        &'a self,
        host: HostSlot,
        request: &'a InstallRequest,
    ) -> BoxFuture<'a, Result<InstallStatus, String>>;
    /// Downloads `url` to `to`.
    fn fetch_url<'a>(&'a self, url: &'a str, to: &'a Path) -> BoxFuture<'a, Result<(), String>>;
    /// The record schema the cluster's data is in.
    fn record_schema(&self) -> Result<u64, String>;
    fn set_record_schema(&self, schema: u64) -> Result<(), String>;
    /// Turns HA maintenance on or off for a host.
    fn maintenance(&self, host: HostSlot, on: bool) -> BoxFuture<'_, Result<(), String>>;
    /// The HA resources running on a host.
    fn ha_resources_on(&self, host: HostSlot) -> BoxFuture<'_, Result<Vec<String>, String>>;
}

pub struct Context<'a> {
    pub flow: &'a Workflow,
    pub cluster: &'a dyn UpdateCluster,
    pub store: &'a UpdateStore,
    pub job: JobId,
    pub now_ms: &'a (dyn Fn() -> i64 + Sync),
    pub poll: Duration,
    /// How long one host's package install may take.
    pub install_within: Duration,
    /// How long a host has to come back healthy after it.
    pub health_within: Duration,
    /// How long HA has to move a host's resources off it.
    pub ha_within: Duration,
}

fn failed(message: impl Into<String>) -> Stop {
    Stop::failed(message)
}

fn stop_message(stop: Stop) -> String {
    match stop {
        Stop::Cancelled => "cancelled".into(),
        Stop::Failed { message, .. } => message,
    }
}

fn updated(host: HostSlot) -> String {
    format!("updated:{host}")
}

fn rolled_back(host: HostSlot) -> String {
    format!("rolled-back:{host}")
}

/// Whether `version` is older than `than`, as their packages sort.
fn older(version: &str, than: &str) -> bool {
    compare_versions(&debian_version(version), &debian_version(than)) == std::cmp::Ordering::Less
}

/// The hosts in the order they are updated: the others by slot, then this one.
fn order(hosts: impl IntoIterator<Item = HostSlot>, me: HostSlot) -> Vec<HostSlot> {
    let mut hosts: Vec<HostSlot> = hosts.into_iter().filter(|h| *h != me).collect();
    hosts.sort();
    hosts.dedup();
    hosts.push(me);
    hosts
}

fn bundle_label(summary: &BundleSummary) -> String {
    let built = jiff::Timestamp::from_second(summary.built_at_s)
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default();
    format!(
        "{} (built {built}, signed with the {} key)",
        summary.version, summary.signed_by
    )
}

impl Context<'_> {
    fn now(&self) -> i64 {
        (self.now_ms)()
    }

    async fn sleep(&self) -> Result<(), Stop> {
        self.flow.check()?;
        tokio::time::sleep(self.poll).await;
        Ok(())
    }

    /// Every member's state; all must answer.
    async fn states(&self, mode: InstallMode) -> Result<Vec<HostUpdateState>, Stop> {
        let membership = self.cluster.membership().map_err(failed)?;
        if !membership.quorate {
            return Err(failed("the cluster isn't quorate"));
        }
        let offline: Vec<String> = membership
            .members
            .keys()
            .filter(|h| !membership.online.contains(h))
            .map(HostSlot::to_string)
            .collect();
        if !offline.is_empty() {
            return Err(failed(format!(
                "every host must be online; {} isn't",
                offline.join(", ")
            )));
        }
        let mut states = Vec::new();
        let mut unreachable = Vec::new();
        for host in membership.members.keys() {
            match self.cluster.state(*host, self.job, mode).await {
                Ok(state) => states.push(state),
                Err(e) => unreachable.push(format!("{host}: {e}")),
            }
        }
        if !unreachable.is_empty() {
            return Err(failed(format!(
                "every host must answer; these didn't: {}",
                unreachable.join("; ")
            )));
        }
        Ok(states)
    }

    /// Asks the user which bundle to install, fetching one from a URL if
    /// they choose, and returns its SHA-256.
    async fn choose_bundle(&self) -> Result<String, Stop> {
        let staged = self.store.staged();
        let mut bundle = field("bundle", FieldType::Select, "Bundle");
        bundle.options = staged
            .iter()
            .map(|s| choice(&s.sha256, &bundle_label(s), Some(s.sha256.clone())))
            .chain([choice(URL_CHOICE, "Fetch a bundle from a URL", None)])
            .collect();
        bundle.default = staged.first().map(|s| json!(s.sha256));
        let mut url = field("url", FieldType::String, "Bundle URL");
        url.required = false;
        url.help = Some("Only when fetching from a URL: http:// or https://".into());
        let values = self
            .flow
            .input_group(
                "Choose the release bundle",
                Some("Bundles uploaded to this host in Settings, Updates, or one fetched from a URL."),
                vec![bundle, url],
                |values| {
                    let chosen = text(values, "bundle");
                    let url = text(values, "url");
                    if chosen == URL_CHOICE
                        && !(url.starts_with("https://") || url.starts_with("http://"))
                    {
                        return Err(crate::native::disks::refuse(
                            "url",
                            "an http:// or https:// URL is required",
                        ));
                    }
                    Ok(())
                },
            )
            .await?;
        let chosen = text(&values, "bundle");
        if chosen != URL_CHOICE {
            return Ok(chosen);
        }
        let url = text(&values, "url");
        self.flow.info(format!("Fetching the bundle from {url}."));
        let scratch = self
            .store
            .scratch(&format!("url-{}", self.job))
            .map_err(failed)?;
        if let Err(e) = self.cluster.fetch_url(&url, &scratch).await {
            let _ = std::fs::remove_file(&scratch);
            return Err(failed(format!("cannot fetch {url}: {e}")));
        }
        let summary = self
            .store
            .stage(&scratch, None, self.now())
            .map_err(|e| failed(format!("the bundle from {url} is refused: {e}")))?;
        Ok(summary.sha256)
    }

    /// Waits for `host`'s install to finish.
    async fn await_install(
        &self,
        host: HostSlot,
        mode: InstallMode,
        first: InstallStatus,
    ) -> Result<(), String> {
        let deadline = Instant::now() + self.install_within;
        let mut status = Some(first);
        let mut last_error = String::new();
        loop {
            match status.as_ref().map(|s| s.state) {
                Some(InstallState::Installed) => return Ok(()),
                Some(InstallState::Failed) => {
                    let status = status.unwrap();
                    let mut message = status
                        .message
                        .unwrap_or_else(|| "the install failed".into());
                    if let Some(log) = status.log_tail.filter(|l| !l.trim().is_empty()) {
                        message = format!("{message}:\n{log}");
                    }
                    return Err(message);
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(if last_error.is_empty() {
                    format!("the install didn't finish within {:?}", self.install_within)
                } else {
                    format!("{host} stopped answering: {last_error}")
                });
            }
            self.sleep().await.map_err(|_| "cancelled".to_string())?;
            match self.cluster.state(host, self.job, mode).await {
                Ok(state) => status = state.install,
                Err(e) => last_error = e,
            }
        }
    }

    /// Waits for `host` to run `version` with its services up and every
    /// other member reachable from it.
    async fn await_health(
        &self,
        host: HostSlot,
        mode: InstallMode,
        version: &str,
        members: &BTreeSet<HostSlot>,
    ) -> Result<(), String> {
        let deadline = Instant::now() + self.health_within;
        loop {
            let problem = match self.cluster.state(host, self.job, mode).await {
                Err(e) => format!("it doesn't answer: {e}"),
                Ok(state) => {
                    let down: Vec<&str> = state
                        .services
                        .iter()
                        .filter(|(_, up)| !**up)
                        .map(|(name, _)| name.as_str())
                        .collect();
                    let reachable: BTreeSet<HostSlot> =
                        state.reachable_peers.iter().copied().collect();
                    let missing: Vec<String> = members
                        .iter()
                        .filter(|m| **m != host && !reachable.contains(m))
                        .map(HostSlot::to_string)
                        .collect();
                    if state.version != version {
                        format!("it runs {}, not {version}", state.version)
                    } else if !down.is_empty() {
                        format!("{} isn't running", down.join(" and "))
                    } else if !missing.is_empty() {
                        format!("it doesn't reach {}", missing.join(", "))
                    } else {
                        return Ok(());
                    }
                }
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "{host} isn't healthy after {}s: {problem}",
                    self.health_within.as_secs()
                ));
            }
            self.sleep().await.map_err(|_| "cancelled".to_string())?;
        }
    }

    async fn drain(&self, host: HostSlot) -> Result<(), String> {
        self.cluster.maintenance(host, true).await?;
        self.flow
            .info(format!("Moving {host}'s HA resources to other hosts."));
        let deadline = Instant::now() + self.ha_within;
        loop {
            let left = self.cluster.ha_resources_on(host).await?;
            if left.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "HA didn't move {} off {host} within {}s",
                    left.join(", "),
                    self.ha_within.as_secs()
                ));
            }
            self.sleep().await.map_err(|_| "cancelled".to_string())?;
        }
    }

    /// Installs on one host and checks it: `sha256` is the bundle an
    /// update installs, or the one a rollback leaves; `version` what the
    /// host runs afterwards.
    async fn one_host(
        &self,
        host: HostSlot,
        mode: InstallMode,
        sha256: &str,
        version: &str,
        members: &BTreeSet<HostSlot>,
        migrate: bool,
    ) -> Result<(), String> {
        let me = host == self.cluster.me();
        if migrate {
            self.drain(host).await?;
        }
        if me {
            self.flow.expect_service_restart().map_err(stop_message)?;
            self.flow.info(format!(
                "Installing on {host}, which runs this job: bmacd restarts, and the job carries on once it is back."
            ));
        } else {
            self.flow.info(format!("Installing on {host}."));
        }
        let request = InstallRequest {
            job: self.job,
            mode,
            sha256: sha256.to_string(),
        };
        let outcome = async {
            let first = self.cluster.install(host, &request).await?;
            self.await_install(host, mode, first).await?;
            self.flow
                .info(format!("{host} installed {version}; checking its health."));
            self.await_health(host, mode, version, members).await
        }
        .await;
        if me {
            let _ = self.flow.clear_service_restart();
        }
        if migrate && let Err(e) = self.cluster.maintenance(host, false).await {
            self.flow.warning(format!(
                "cannot take {host} out of HA maintenance: {e}; run `ha-manager crm-command node-maintenance disable {host}`"
            ));
        }
        outcome
    }
}

/// The record schema check: the bundle must read the cluster's data, and
/// data newer than the bundle refuses it.
pub fn schema_refusal(bundle: &BundleSummary, cluster: u64) -> Option<String> {
    if cluster > bundle.record_schema {
        return Some(format!(
            "the cluster's records are in schema {cluster}, newer than this bundle's {}; a bundle that old can't run this cluster",
            bundle.record_schema
        ));
    }
    if bundle.reads_record_schema_from > cluster {
        return Some(format!(
            "this bundle reads records from schema {} on, and the cluster's are in schema {cluster}; update to an intermediate release first",
            bundle.reads_record_schema_from
        ));
    }
    None
}

/// `cluster.update`.
pub async fn update(cx: &Context<'_>) -> Result<Value, Stop> {
    let flow = cx.flow;
    let mut done: Checkpoints = flow.checkpoints();
    let me = cx.cluster.me();

    flow.phase(BUNDLE_PHASE.0, BUNDLE_PHASE.1, PhaseStatus::Running);
    let sha = match done.get("bundle").and_then(|v| v["sha256"].as_str()) {
        Some(sha) => sha.to_string(),
        None => {
            let sha = cx.choose_bundle().await?;
            done.record("bundle", json!({ "sha256": sha }))?;
            sha
        }
    };
    let bundle = cx
        .store
        .open(&sha)
        .map_err(|e| failed(format!("bundle {sha}: {e}")))?;
    let summary = super::summary(&bundle, 0);
    let version = summary.version.clone();
    flow.info(format!(
        "Bundle {sha} checks out: BMAC {}.",
        bundle_label(&summary)
    ));
    flow.phase(BUNDLE_PHASE.0, BUNDLE_PHASE.1, PhaseStatus::Complete);

    if let Some(stopped) = done.get("stopped").cloned() {
        return offer_rollback(cx, &mut done, &sha, &stopped).await;
    }

    flow.phase(CHECK_PHASE.0, CHECK_PHASE.1, PhaseStatus::Running);
    let (hosts, members, migrate) = match done.get("plan").cloned() {
        Some(plan) => {
            let hosts: Vec<HostSlot> = serde_json::from_value(plan["hosts"].clone())
                .map_err(|e| failed(format!("the recorded plan: {e}")))?;
            let members: BTreeSet<HostSlot> = serde_json::from_value(plan["members"].clone())
                .map_err(|e| failed(format!("the recorded plan: {e}")))?;
            (hosts, members, plan["migrate"] == true)
        }
        None => {
            let states = cx.states(InstallMode::Update).await?;
            let members: BTreeSet<HostSlot> = states.iter().map(|s| s.host).collect();
            let newer: Vec<String> = states
                .iter()
                .filter(|s| older(&version, &s.version))
                .map(|s| format!("{} runs {}", s.host, s.version))
                .collect();
            if !newer.is_empty() {
                return Err(failed(format!(
                    "this bundle is older than what runs now ({}); roll back instead",
                    newer.join(", ")
                )));
            }
            let schema = cx.cluster.record_schema().map_err(failed)?;
            if let Some(refusal) = schema_refusal(&summary, schema) {
                return Err(failed(refusal));
            }
            let current = |s: &HostUpdateState| {
                s.version == version
                    && s.installed
                        .current
                        .as_ref()
                        .is_some_and(|c| c.sha256 == sha)
            };
            let skip: Vec<HostSlot> = states
                .iter()
                .filter(|s| current(s))
                .map(|s| s.host)
                .collect();
            for host in &skip {
                flow.info(format!("{host} already runs this bundle."));
            }
            let hosts: Vec<HostSlot> = order(members.iter().copied(), me)
                .into_iter()
                .filter(|h| !skip.contains(h))
                .collect();
            if hosts.is_empty() {
                flow.phase(CHECK_PHASE.0, CHECK_PHASE.1, PhaseStatus::Complete);
                cx.cluster
                    .set_record_schema(summary.record_schema.max(schema))
                    .map_err(failed)?;
                return Ok(json!({ "version": version, "updated": Vec::<String>::new() }));
            }
            let no_copy: Vec<String> = states
                .iter()
                .filter(|s| hosts.contains(&s.host) && s.installed.current.is_none())
                .map(|s| s.host.to_string())
                .collect();
            if !no_copy.is_empty() {
                flow.warning(format!(
                    "{} still run the build they were installed with, which no bundle on them holds: a rollback can't return them to it.",
                    no_copy.join(", ")
                ));
            }
            let mut migrate = field(
                "migrate",
                FieldType::Boolean,
                "Move HA resources off each host while it updates",
            );
            migrate.required = false;
            migrate.default = Some(json!(false));
            migrate.help =
                Some("Updating restarts BMAC, not guests; guests keep running either way.".into());
            let options = flow
                .input_group("Update options", None, vec![migrate], |_| Ok(()))
                .await?;
            let migrate = options.get("migrate") == Some(&json!(true));
            let mut details: Vec<String> = states
                .iter()
                .map(|s| format!("{}: {} now", s.host, s.version))
                .collect();
            details.push(format!(
                "Order: {}",
                hosts
                    .iter()
                    .map(HostSlot::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            if !flow
                .confirm(
                    &format!("Update {} hosts to {version}?", hosts.len()),
                    "Each host installs the bundle and must come back healthy before the next starts. The first failure stops the update.",
                    Severity::Warning,
                    None,
                    details,
                )
                .await?
            {
                return Err(Stop::Cancelled);
            }
            done.record(
                "plan",
                json!({ "hosts": hosts, "members": members, "migrate": migrate }),
            )?;
            (hosts, members, migrate)
        }
    };
    flow.phase(CHECK_PHASE.0, CHECK_PHASE.1, PhaseStatus::Complete);

    flow.phase_with(
        INSTALL_PHASE.0,
        INSTALL_PHASE.1,
        PhaseStatus::Running,
        false,
    );
    for host in &hosts {
        if done.is_done(&updated(*host)) {
            continue;
        }
        match cx
            .one_host(
                *host,
                InstallMode::Update,
                &sha,
                &version,
                &members,
                migrate,
            )
            .await
        {
            Ok(()) => {
                flow.info(format!("{host} is updated and healthy."));
                done.record(&updated(*host), json!({ "at_ms": cx.now() }))?;
            }
            Err(message) => {
                flow.phase_with(INSTALL_PHASE.0, INSTALL_PHASE.1, PhaseStatus::Failed, false);
                let stopped = json!({ "host": host, "message": message });
                done.record("stopped", stopped.clone())?;
                return offer_rollback(cx, &mut done, &sha, &stopped).await;
            }
        }
    }
    flow.phase_with(
        INSTALL_PHASE.0,
        INSTALL_PHASE.1,
        PhaseStatus::Complete,
        false,
    );
    let schema = cx.cluster.record_schema().map_err(failed)?;
    cx.cluster
        .set_record_schema(summary.record_schema.max(schema))
        .map_err(failed)?;
    flow.result(json!({ "version": version, "updated": hosts }));
    Ok(json!({ "version": version, "updated": hosts }))
}

/// After a failed host: offers to roll back every host the bundle was
/// installed on, and fails the job either way.
async fn offer_rollback(
    cx: &Context<'_>,
    done: &mut Checkpoints,
    sha: &str,
    stopped: &Value,
) -> Result<Value, Stop> {
    let flow = cx.flow;
    let host = stopped["host"].as_str().unwrap_or("a host").to_string();
    let message = stopped["message"].as_str().unwrap_or_default().to_string();
    let why = format!("The update stopped at {host}: {message}");
    if !done.is_done("rollback-asked") {
        flow.warning(why.clone());
        let states = cx.states(InstallMode::Rollback).await?;
        let on_bundle: Vec<HostSlot> = states
            .iter()
            .filter(|s| {
                s.installed
                    .current
                    .as_ref()
                    .is_some_and(|c| c.sha256 == sha)
            })
            .map(|s| s.host)
            .collect();
        if on_bundle.is_empty() {
            return Err(failed(format!("{why}. No host runs the new bundle.")));
        }
        let me = cx.cluster.me();
        let hosts: Vec<HostSlot> = order(on_bundle.iter().copied(), me)
            .into_iter()
            .filter(|h| on_bundle.contains(h))
            .collect();
        let details: Vec<String> = hosts
            .iter()
            .map(|h| {
                let previous = states
                    .iter()
                    .find(|s| s.host == *h)
                    .and_then(|s| s.installed.previous.as_ref())
                    .map_or("nothing kept".to_string(), |p| p.version.clone());
                format!("{h}: back to {previous}")
            })
            .collect();
        let roll = flow
            .confirm(
                "Roll back the updated hosts?",
                &format!(
                    "{why}. Roll {} back to the build each ran before? Fix what stopped the update first, or the rollback stops on it too.",
                    hosts.iter().map(HostSlot::to_string).collect::<Vec<_>>().join(", ")
                ),
                Severity::Destructive,
                None,
                details,
            )
            .await?;
        done.record("rollback-asked", json!({ "roll": roll, "hosts": hosts }))?;
    }
    let asked = done.get("rollback-asked").cloned().unwrap_or_default();
    if asked["roll"] != true {
        return Err(failed(format!(
            "{why}. Nothing was rolled back; run Roll back a cluster update to return the updated hosts to their previous build."
        )));
    }
    let hosts: Vec<HostSlot> = serde_json::from_value(asked["hosts"].clone())
        .map_err(|e| failed(format!("the recorded rollback: {e}")))?;
    let rolled = roll_back(cx, done, sha, &hosts).await?;
    Err(failed(format!(
        "{why}. Rolled back {}.",
        rolled
            .iter()
            .map(HostSlot::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Rolls each of `hosts` back from bundle `sha` to the bundle it ran before.
async fn roll_back(
    cx: &Context<'_>,
    done: &mut Checkpoints,
    sha: &str,
    hosts: &[HostSlot],
) -> Result<Vec<HostSlot>, Stop> {
    let flow = cx.flow;
    flow.phase_with(
        ROLLBACK_PHASE.0,
        ROLLBACK_PHASE.1,
        PhaseStatus::Running,
        false,
    );
    let members: BTreeSet<HostSlot> = cx
        .cluster
        .membership()
        .map_err(failed)?
        .members
        .into_keys()
        .collect();
    let mut rolled = Vec::new();
    for host in hosts {
        if let Some(record) = done.get(&rolled_back(*host)) {
            if record["skipped"] != true {
                rolled.push(*host);
            }
            continue;
        }
        let state = cx
            .cluster
            .state(*host, cx.job, InstallMode::Rollback)
            .await
            .map_err(|e| failed(format!("{host}: {e}")))?;
        let runs_bundle = state
            .installed
            .current
            .as_ref()
            .is_some_and(|c| c.sha256 == sha);
        let Some(previous) = state.installed.previous.clone().filter(|_| runs_bundle) else {
            let finished = state
                .install
                .as_ref()
                .is_some_and(|i| i.state == InstallState::Installed);
            if !finished {
                flow.warning(format!("{host} no longer runs bundle {sha}; leaving it."));
                done.record(&rolled_back(*host), json!({ "skipped": true }))?;
                continue;
            }
            // Its rollback finished while this job was restarting.
            let version = state.version.clone();
            cx.await_health(*host, InstallMode::Rollback, &version, &members)
                .await
                .map_err(|e| failed(format!("rolling back {host}: {e}")))?;
            done.record(&rolled_back(*host), json!({ "version": version }))?;
            rolled.push(*host);
            continue;
        };
        cx.one_host(
            *host,
            InstallMode::Rollback,
            sha,
            &previous.version,
            &members,
            false,
        )
        .await
        .map_err(|e| {
            flow.phase_with(
                ROLLBACK_PHASE.0,
                ROLLBACK_PHASE.1,
                PhaseStatus::Failed,
                false,
            );
            failed(format!("rolling back {host}: {e}"))
        })?;
        flow.info(format!(
            "{host} is back on {} and healthy.",
            previous.version
        ));
        done.record(&rolled_back(*host), json!({ "version": previous.version }))?;
        rolled.push(*host);
    }
    flow.phase_with(
        ROLLBACK_PHASE.0,
        ROLLBACK_PHASE.1,
        PhaseStatus::Complete,
        false,
    );
    Ok(rolled)
}

/// `cluster.rollback`.
pub async fn rollback(cx: &Context<'_>) -> Result<Value, Stop> {
    let flow = cx.flow;
    let mut done = flow.checkpoints();
    let me = cx.cluster.me();
    flow.phase(CHECK_PHASE.0, CHECK_PHASE.1, PhaseStatus::Running);
    let (sha, hosts) = match done.get("plan").cloned() {
        Some(plan) => (
            plan["sha256"].as_str().unwrap_or_default().to_string(),
            serde_json::from_value::<Vec<HostSlot>>(plan["hosts"].clone())
                .map_err(|e| failed(format!("the recorded plan: {e}")))?,
        ),
        None => {
            let states = cx.states(InstallMode::Rollback).await?;
            let mut from: Vec<(String, String, Vec<HostSlot>)> = Vec::new();
            for state in &states {
                let (Some(current), Some(_)) =
                    (&state.installed.current, &state.installed.previous)
                else {
                    continue;
                };
                match from.iter_mut().find(|(sha, _, _)| *sha == current.sha256) {
                    Some((_, _, hosts)) => hosts.push(state.host),
                    None => from.push((
                        current.sha256.clone(),
                        current.version.clone(),
                        vec![state.host],
                    )),
                }
            }
            if from.is_empty() {
                return Err(failed("no host keeps an earlier bundle to roll back to"));
            }
            let mut bundle = field("bundle", FieldType::Select, "Roll back from");
            bundle.options = from
                .iter()
                .map(|(sha, version, hosts)| {
                    let names: Vec<String> = hosts.iter().map(HostSlot::to_string).collect();
                    choice(
                        sha,
                        &format!("{version} (on {})", names.join(", ")),
                        Some(sha.clone()),
                    )
                })
                .collect();
            bundle.default = Some(json!(from[0].0));
            let values = flow
                .input_group(
                    "Choose the update to roll back",
                    Some("Each host running it goes back to the bundle it ran before."),
                    vec![bundle],
                    |_| Ok(()),
                )
                .await?;
            let chosen = text(&values, "bundle");
            let Some((sha, version, hosts)) = from.into_iter().find(|(s, _, _)| *s == chosen)
            else {
                return Err(failed("no host runs that bundle"));
            };
            let hosts = order(hosts.clone(), me)
                .into_iter()
                .filter(|h| hosts.contains(h))
                .collect::<Vec<_>>();
            let details: Vec<String> = hosts
                .iter()
                .map(|h| {
                    let previous = states
                        .iter()
                        .find(|s| s.host == *h)
                        .and_then(|s| s.installed.previous.as_ref())
                        .map_or(String::new(), |p| p.version.clone());
                    format!("{h}: {version} back to {previous}")
                })
                .collect();
            if !flow
                .confirm(
                    &format!("Roll {} hosts back from {version}?", hosts.len()),
                    "Each host installs the bundle it ran before and must come back healthy before the next starts.",
                    Severity::Warning,
                    None,
                    details,
                )
                .await?
            {
                return Err(Stop::Cancelled);
            }
            done.record("plan", json!({ "sha256": sha, "hosts": hosts }))?;
            (sha, hosts)
        }
    };
    flow.phase(CHECK_PHASE.0, CHECK_PHASE.1, PhaseStatus::Complete);
    let rolled = roll_back(cx, &mut done, &sha, &hosts).await?;
    let result = json!({ "from": sha, "rolled_back": rolled });
    flow.result(result.clone());
    Ok(result)
}

// The real cluster.

pub struct RealCluster {
    pub node: HostSlot,
    pub peers: crate::mesh::client::PeerClient,
    pub runner: bmac_exec::RealRunner,
    pub pmxcfs: bmac_pmxcfs::Pmxcfs,
}

impl RealCluster {
    async fn run(&self, program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
        use bmac_exec::CommandRunner;
        let command = bmac_exec::Command::new(program)
            .args(args.iter().map(|a| a.to_string()))
            .timeout(timeout);
        self.runner
            .run(command)
            .await
            .map(|o| o.stdout_text())
            .map_err(|e| format!("{program}: {e}"))
    }
}

const HA_MANAGER: &str = "/usr/sbin/ha-manager";
const PVESH: &str = "/usr/bin/pvesh";
const CURL: &str = "/usr/bin/curl";

/// The HA resources `pvesh get /cluster/ha/status/current` places on `host`.
pub fn ha_resources_on(status: &Value, host: HostSlot) -> Vec<String> {
    let name = host.to_string();
    status
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["type"] == "service" && e["node"] == name.as_str())
        .filter(|e| e["state"] != "stopped" && e["state"] != "ignored")
        .filter_map(|e| e["sid"].as_str().map(str::to_string))
        .collect()
}

impl UpdateCluster for RealCluster {
    fn me(&self) -> HostSlot {
        self.node
    }

    fn membership(&self) -> Result<Membership, String> {
        use crate::cluster::ClusterView;
        crate::cluster::PveCluster::system().membership()
    }

    fn state(
        &self,
        host: HostSlot,
        job: JobId,
        mode: InstallMode,
    ) -> BoxFuture<'_, Result<HostUpdateState, String>> {
        let mode = match mode {
            InstallMode::Update => "update",
            InstallMode::Rollback => "rollback",
        };
        let path = format!("{STATE_PATH}?job={job}&mode={mode}");
        Box::pin(async move { self.peers.json(host, "GET", &path, None).await })
    }

    fn install<'a>(
        &'a self,
        host: HostSlot,
        request: &'a InstallRequest,
    ) -> BoxFuture<'a, Result<InstallStatus, String>> {
        Box::pin(async move {
            self.peers
                .json(host, "POST", INSTALL_PATH, Some(request))
                .await
        })
    }

    fn fetch_url<'a>(&'a self, url: &'a str, to: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let max = bmac_release::MAX_BUNDLE.to_string();
            let out = to.display().to_string();
            self.run(
                CURL,
                &[
                    "--fail",
                    "--silent",
                    "--show-error",
                    "--location",
                    "--proto",
                    "=https,http",
                    "--max-filesize",
                    &max,
                    "--output",
                    &out,
                    "--",
                    url,
                ],
                Duration::from_secs(3600),
            )
            .await
            .map(drop)
        })
    }

    fn record_schema(&self) -> Result<u64, String> {
        let path = Path::new(RELEASE_RECORD);
        if !self.pmxcfs.exists(path) {
            return Ok(bmac_pmxcfs::OLDEST_READ_SCHEMA);
        }
        let value = self
            .pmxcfs
            .read_json(path, "the release record")
            .map_err(|e| e.to_string())?;
        value["record_schema"]
            .as_u64()
            .ok_or_else(|| format!("{RELEASE_RECORD} has no record_schema"))
    }

    fn set_record_schema(&self, schema: u64) -> Result<(), String> {
        self.pmxcfs
            .write_json(
                Path::new(RELEASE_RECORD),
                &json!({ "record_schema": schema }),
            )
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn maintenance(&self, host: HostSlot, on: bool) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let name = host.to_string();
            let action = if on { "enable" } else { "disable" };
            self.run(
                HA_MANAGER,
                &["crm-command", "node-maintenance", action, &name],
                Duration::from_secs(60),
            )
            .await
            .map(drop)
        })
    }

    fn ha_resources_on(&self, host: HostSlot) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(async move {
            let text = self
                .run(
                    PVESH,
                    &[
                        "get",
                        "/cluster/ha/status/current",
                        "--output-format",
                        "json",
                    ],
                    Duration::from_secs(60),
                )
                .await?;
            let status: Value =
                serde_json::from_str(&text).map_err(|e| format!("HA status: {e}"))?;
            Ok(ha_resources_on(&status, host))
        })
    }
}

/// `bmacd native cluster.update` and `bmacd native cluster.rollback`.
pub async fn run(flow: &Workflow, operation: &str) -> ExitCode {
    let spec = if operation == ROLLBACK_OPERATION {
        rollback_operation()
    } else {
        update_operation()
    };
    flow.begin(operation, &spec.title, &spec.description);
    let outcome = async {
        let node: HostSlot = crate::engine::listen::node_name()
            .parse()
            .map_err(|_| failed("this host's name is not a BMAC host name"))?;
        let job: JobId = std::env::var("BMAC_UI_RUN_ID")
            .ok()
            .and_then(|id| id.parse().ok())
            .ok_or_else(|| failed("BMAC_UI_RUN_ID names no job"))?;
        let cluster = RealCluster {
            node,
            peers: crate::mesh::client::PeerClient::system(node).map_err(failed)?,
            runner: bmac_exec::RealRunner,
            pmxcfs: bmac_pmxcfs::Pmxcfs::system(),
        };
        let store = UpdateStore::system();
        let now = || jiff::Timestamp::now().as_millisecond();
        let cx = Context {
            flow,
            cluster: &cluster,
            store: &store,
            job,
            now_ms: &now,
            poll: Duration::from_secs(3),
            install_within: Duration::from_secs(20 * 60),
            health_within: Duration::from_secs(180),
            ha_within: Duration::from_secs(15 * 60),
        };
        if operation == ROLLBACK_OPERATION {
            rollback(&cx).await.map(drop)
        } else {
            update(&cx).await.map(drop)
        }
    }
    .await;
    flow.finish(&outcome)
}

#[cfg(test)]
#[path = "job_tests.rs"]
mod tests;
