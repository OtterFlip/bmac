// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Deployer's workflows as tasks (`DEPLOYMENT.md`, "BMAC Deployer"):
//! each one asks for what it needs through request forms, so the window
//! only starts them by kind and answers their questions. [`discovery`]
//! reads a machine's hardware; [`host`] builds a host's ISO, gives the
//! boot steps, waits for the host, and hands it to its Dashboard; [`ssh`]
//! sets up workstation SSH to hosts and guests afterwards; [`hello`]
//! deploys the hello app to a production guest.

pub mod discovery;
pub mod hello;
pub mod host;
pub mod ssh;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bmac_core::job::{Field, FieldOption, FieldType};
use bmac_exec::CommandRunner;
use bmac_proto::deployer::{BuiltIso, LinkWait, SourceIso, TaskKind, TaskSummary};
use serde_json::Value;

use crate::download::{Fetch, Pinned};
use crate::preflight::Workstation;
use crate::tasks::{Clock, Stop, TaskContext, Tasks, Values};

/// The workstation programs the workflows run, by absolute path.
#[derive(Debug, Clone, Default)]
pub struct Tools {
    pub ssh: Option<PathBuf>,
    pub ssh_keyscan: Option<PathBuf>,
    pub xorriso: Option<PathBuf>,
    pub tailscale: Option<PathBuf>,
    pub dpkg_deb: Option<PathBuf>,
}

impl Tools {
    pub fn find(ws: &Workstation) -> Tools {
        Tools {
            ssh: ws.which("ssh"),
            ssh_keyscan: ws.which("ssh-keyscan"),
            xorriso: ws.which("xorriso"),
            tailscale: ws.which("tailscale"),
            dpkg_deb: ws.which("dpkg-deb"),
        }
    }
}

/// Where a built ISO is listed, so Settings can show and delete it.
pub trait IsoBook: Send + Sync {
    fn record(&self, iso: BuiltIso);
    fn delete(&self, path: &str) -> Result<(), String>;
}

/// What the workflows work with.
pub struct Env {
    pub runner: Arc<dyn CommandRunner>,
    pub fetch: Arc<dyn Fetch>,
    pub tools: Tools,
    /// Verified downloads: source ISOs, Tailscale, the assistant.
    pub cache_dir: PathBuf,
    /// Private working state: discovery sessions, SSH backups.
    pub state_dir: PathBuf,
    /// Where built ISOs and inventory files go.
    pub iso_dir: PathBuf,
    /// `~/.ssh`.
    pub ssh_dir: PathBuf,
    /// The static `bmac-inventory` the discovery ISO carries.
    pub inventory_binary: Option<PathBuf>,
    /// The `bmacd` packages a host ISO embeds.
    pub packages_dir: Option<PathBuf>,
    pub link_wait: LinkWait,
    pub isos: Arc<dyn IsoBook>,
    pub clock: Clock,
    /// How often the tailnet and the Dashboard are looked at.
    pub poll: Duration,
    /// The Alpine ISO and Tailscale build discovery ISOs are made from.
    pub discovery_sources: (Pinned, Pinned),
    /// The port a new host's Dashboard answers on.
    pub dashboard_port: u16,
    /// The workstation's time zone (IANA), a new cluster's unless its
    /// settings file names one.
    pub time_zone: String,
}

/// The workstation's time zone as an IANA name, or UTC.
pub fn workstation_zone() -> String {
    jiff::tz::TimeZone::system()
        .iana_name()
        .unwrap_or("UTC")
        .to_string()
}

/// The sources a real Deployer builds discovery ISOs from: the operator's
/// chosen ISO or the pinned Alpine release, and the pinned Tailscale build.
pub fn discovery_sources(chosen: Option<&SourceIso>) -> (Pinned, Pinned) {
    let iso = match chosen {
        Some(iso) => Pinned {
            url: iso.url.clone(),
            sha256: iso.sha256.clone(),
        },
        None => crate::discovery::alpine_iso(),
    };
    (iso, crate::discovery::tailscale_tgz())
}

impl Env {
    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    pub(crate) fn tool<'a>(&self, path: &'a Option<PathBuf>, name: &str) -> Result<&'a Path, Stop> {
        path.as_deref().ok_or_else(|| {
            Stop::failed(format!(
                "{name} isn't installed on this workstation. Install it (the Start page shows how) \
                 and run this again."
            ))
        })
    }
}

/// Starts a workflow as a task.
pub fn start(tasks: &Tasks, kind: TaskKind, env: Arc<Env>) -> TaskSummary {
    match kind {
        TaskKind::Discovery => tasks.start(
            "discovery",
            "Hardware discovery",
            Default::default(),
            move |ctx| async move { discovery::run(&ctx, &env).await },
        ),
        TaskKind::Host => tasks.start(
            "host",
            "Install a Proxmox host",
            Default::default(),
            move |ctx| async move { host::run(&ctx, &env).await },
        ),
        TaskKind::HostSsh => tasks.start(
            "host_ssh",
            "Set up SSH to a host",
            Default::default(),
            move |ctx| async move { ssh::host(&ctx, &env).await },
        ),
        TaskKind::GuestSsh => tasks.start(
            "guest_ssh",
            "Set up SSH to a guest",
            Default::default(),
            move |ctx| async move { ssh::guest(&ctx, &env).await },
        ),
        TaskKind::Hello => tasks.start(
            "hello",
            "Deploy the hello app",
            Default::default(),
            move |ctx| async move { hello::run(&ctx, &env).await },
        ),
    }
}

/// A field with nothing but its id, type, and label set.
pub(crate) fn field(id: &str, kind: FieldType, label: &str) -> Field {
    Field {
        id: id.into(),
        kind,
        label: label.into(),
        help: None,
        required: false,
        default: None,
        placeholder: None,
        suffix: None,
        min: None,
        max: None,
        min_selected: None,
        max_selected: None,
        pattern: None,
        sensitive: false,
        disabled: false,
        options: Vec::new(),
        filters: Vec::new(),
    }
}

pub(crate) trait FieldExt {
    fn required(self) -> Self;
    fn help(self, text: &str) -> Self;
    fn default_value(self, value: Value) -> Self;
    fn choices(self, options: Vec<FieldOption>) -> Self;
    fn range(self, min: f64, max: f64) -> Self;
}

impl FieldExt for Field {
    fn required(mut self) -> Field {
        self.required = true;
        self
    }
    fn help(mut self, text: &str) -> Field {
        self.help = Some(text.into());
        self
    }
    fn default_value(mut self, value: Value) -> Field {
        self.default = (!value.is_null()).then_some(value);
        self
    }
    fn choices(mut self, options: Vec<FieldOption>) -> Field {
        self.options = options;
        self
    }
    fn range(mut self, min: f64, max: f64) -> Field {
        self.min = Some(min);
        self.max = Some(max);
        self
    }
}

pub(crate) fn option(value: &str, label: &str, help: Option<&str>) -> FieldOption {
    FieldOption {
        value: value.into(),
        label: label.into(),
        help: help.map(str::to_string),
    }
}

/// A value as trimmed text; `None` when absent or blank.
pub(crate) fn text(values: &Values, id: &str) -> Option<String> {
    let text = match values.get(id)? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

pub(crate) fn list(values: &Values, id: &str) -> Vec<String> {
    match values.get(id) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
        _ => Vec::new(),
    }
}

/// A download's progress, reported at most once per percent (or per
/// 64 MiB when the size is unknown).
pub(crate) fn download_progress(
    ctx: &TaskContext,
    what: String,
) -> impl Fn(u64, Option<u64>) + Send + Sync + use<> {
    let ctx = ctx.clone();
    let last = AtomicU64::new(u64::MAX);
    move |done, total| {
        let bucket = match total {
            Some(total) if total > 0 => done * 100 / total,
            _ => done >> 26,
        };
        if last.swap(bucket, Ordering::Relaxed) != bucket {
            ctx.progress(
                what.clone(),
                Some(done as f64),
                total.map(|t| t as f64),
                Some("bytes"),
            );
        }
    }
}

/// A note from library code, as task output.
pub(crate) fn note(ctx: &TaskContext) -> impl Fn(String) + Send + Sync + use<> {
    let ctx = ctx.clone();
    move |text| ctx.output(text)
}

/// A wait that ended with an error string becomes a cancel when the
/// operator cancelled.
pub(crate) fn stopped(ctx: &TaskContext, error: String) -> Stop {
    if ctx.token().is_cancelled() {
        Stop::Cancelled
    } else {
        Stop::failed(error)
    }
}

#[cfg(test)]
mod tests;
