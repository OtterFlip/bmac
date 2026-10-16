// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Cluster updates (`DEPLOYMENT.md`, "Upgrades"). The Dashboard uploads a
//! release bundle to the host it talks to, in chunks
//! (`POST /api/v1/updates/uploads`, then
//! `POST /api/v1/updates/uploads/<id>` per chunk and `.../finish`), and
//! `GET /api/v1/updates` shows each host's version, what it can roll back
//! to, and the bundles staged here. The `cluster.update` job then has each
//! host fetch the bundle over port 8444 (`GET /v1/updates/bundles/<sha256>`),
//! check it, and install it (`POST /v1/updates/install`), one host at a
//! time, watching `GET /v1/updates/state`.

use std::collections::BTreeMap;

use bmac_core::{HostSlot, JobId};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// `GET`: the bundle file, streamed; the path ends with its SHA-256.
pub const BUNDLE_PATH: &str = "/v1/updates/bundles/";
/// `POST` an [`InstallRequest`].
pub const INSTALL_PATH: &str = "/v1/updates/install";
/// `GET ?job=<id>`: a [`HostUpdateState`].
pub const STATE_PATH: &str = "/v1/updates/state";

/// Raw bytes per upload chunk; base64 keeps the request under the front
/// end's 1 MiB body limit.
pub const UPLOAD_CHUNK: u64 = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct PackageSummary {
    pub name: String,
    pub version: String,
}

/// A checked bundle kept on a host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct BundleSummary {
    pub sha256: String,
    /// The BMAC version it installs.
    pub version: String,
    pub built_at_s: i64,
    /// `release` or `development`.
    pub signed_by: String,
    pub size: u64,
    pub record_schema: u64,
    pub reads_record_schema_from: u64,
    pub packages: Vec<PackageSummary>,
    pub staged_at_ms: i64,
}

/// A bundle a host installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct InstalledBundle {
    pub sha256: String,
    pub version: String,
}

/// What a host runs and can roll back to. A host installed from its ISO
/// has neither until its first update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct Installed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<InstalledBundle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<InstalledBundle>,
}

/// One host in [`UpdatesView`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct HostUpdates {
    pub host: HostSlot,
    /// The running `bmacd`'s version; `None` when the host didn't answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub installed: Installed,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `GET /api/v1/updates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct UpdatesView {
    /// The host the bundles are staged on, which runs the update job.
    pub this_host: HostSlot,
    pub hosts: Vec<HostUpdates>,
    pub bundles: Vec<BundleSummary>,
    /// The keys this build trusts: `release`, and `development` in lab builds.
    pub trusted_keys: Vec<String>,
}

/// The body of `POST /api/v1/updates/uploads`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct StartUpload {
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UploadStarted {
    pub upload_id: String,
    pub chunk: u64,
}

/// The body of `POST /api/v1/updates/uploads/<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct UploadChunk {
    pub offset: u64,
    /// The chunk's bytes, base64.
    pub data: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMode {
    /// Install the bundle.
    Update,
    /// Go back to the bundle installed before `sha256`, which the host
    /// must be running.
    Rollback,
}

/// The body of `POST /v1/updates/install`. Only the host running `job`, a
/// `cluster.update` or `cluster.rollback`, may ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    pub job: JobId,
    pub mode: InstallMode,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    Fetching,
    Installing,
    Installed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallStatus {
    pub mode: InstallMode,
    pub state: InstallState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The end of the package manager's output, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<String>,
}

/// The answer of `GET /v1/updates/state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostUpdateState {
    pub host: HostSlot,
    pub version: String,
    pub installed: Installed,
    /// The install `job` asked for, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<InstallStatus>,
    /// Whether each of `bmacd`'s companion units is active.
    pub services: BTreeMap<String, bool>,
    /// The members whose streams this host receives.
    pub reachable_peers: Vec<HostSlot>,
}
