// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Writes the TypeScript form of the dashboard API and stream types, with
//! everything they reference, into a directory named `generated`, replacing
//! its contents. `dev/generate-typescript` runs this into `ui/src/generated`,
//! and `dev/check` fails when that copy is stale.
//!
//!   cargo run -p bmac-proto --example typescript -- <dir>/generated

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bmac_core::job;
use bmac_proto::{
    alerts, api, appliance_ui, backups, certificate, deployer, inventory, joins, logs, operations,
    settings, status, stream,
};
use ts_rs::{Config, TS};

const HEADER: &str = "\
// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Generated from the Rust types in v3/crates by dev/generate-typescript.
// Do not edit; change the Rust types and regenerate.
";

fn export(cfg: &Config) -> Result<(), ts_rs::ExportError> {
    macro_rules! export_all {
        ($($ty:ty),* $(,)?) => { $(<$ty>::export_all(cfg)?;)* };
    }
    export_all!(
        api::Role,
        api::SecondFactor,
        api::LoginRequest,
        api::LoginResponse,
        api::Session,
        api::LogoutRequest,
        api::ErrorCode,
        api::ApiError,
        api::LaunchJob,
        api::JobSummary,
        api::JobList,
        api::JobView,
        api::AnswerRequest,
        api::CancelRequest,
        api::WatchRequest,
        api::SetupView,
        operations::OperationList,
        settings::SettingsView,
        settings::SaveSettings,
        alerts::NotifyView,
        alerts::AcknowledgeAlert,
        alerts::SaveSilence,
        alerts::SaveNotifySettings,
        alerts::SetPostmarkToken,
        alerts::SendTestEmail,
        alerts::TestEmailResult,
        alerts::TemplatesView,
        logs::LogQuery,
        logs::LogPage,
        logs::ReplicationView,
        certificate::CertificateView,
        certificate::SwitchCertificate,
        joins::IssueJoin,
        joins::IssuedJoin,
        joins::JoinList,
        backups::IssueEnroll,
        backups::IssuedEnroll,
        backups::ApproveAppliance,
        backups::ApplianceList,
        backups::SetApplianceAddress,
        backups::SaveBackupSettings,
        backups::BackupsView,
        status::ClusterSnapshot,
        status::HostsSnapshot,
        status::VmsSnapshot,
        status::StorageSnapshot,
        status::HostObservation,
        status::ClusterObservation,
        status::GuestsObservation,
        status::StorageObservation,
        stream::StreamMessage,
        stream::StreamBody,
        deployer::DeployerInfo,
        deployer::Check,
        deployer::DeployerSettingsView,
        deployer::TaskSummary,
        deployer::TaskView,
        deployer::AnswerTask,
        deployer::TaskKind,
        deployer::SourceIso,
        deployer::SshEntry,
        inventory::Inventory,
        inventory::ClearRequest,
        inventory::ClearReport,
        job::RequestForm,
        job::EventBody,
        appliance_ui::ApplianceHello,
        appliance_ui::ApplianceLogin,
        appliance_ui::ApplianceSession,
        appliance_ui::ChangePassword,
        appliance_ui::ApplianceStatusView,
        appliance_ui::PitsView,
        appliance_ui::ThroughputView,
        appliance_ui::ApplianceRetentionView,
        appliance_ui::SetRetention,
        appliance_ui::RetentionChange,
        appliance_ui::SetHold,
        appliance_ui::PruneResult,
        appliance_ui::GrantView,
        appliance_ui::IssueGrant,
        appliance_ui::IssueRecoveryGrant,
        appliance_ui::IssuedGrant,
        appliance_ui::RevokeGrant,
        appliance_ui::PeersView,
        appliance_ui::TrustPeer,
        appliance_ui::TrustResult,
        appliance_ui::UntrustPeer,
        appliance_ui::PlatformView,
        appliance_ui::SavePlatform,
        appliance_ui::SetApplianceToken,
        appliance_ui::ApplianceTestEmail,
        appliance_ui::AuditView,
        appliance_ui::RecoveryView,
    );
    Ok(())
}

fn constants() -> String {
    format!(
        "{HEADER}\nexport const API_PREFIX = {:?};\nexport const KEEPALIVE_SECONDS = {};\n\
         export const DEPLOYER_TASK_EVENT = {:?};\nexport const DEPLOYER_TASK_UPDATED = {:?};\n\
         export const DEPLOYER_CLOSE_REQUESTED = {:?};\nexport const APPLIANCE_API = {:?};\n",
        api::API_PREFIX,
        stream::KEEPALIVE_SECONDS,
        deployer::events::TASK_EVENT,
        deployer::events::TASK_UPDATED,
        deployer::events::CLOSE_REQUESTED,
        appliance_ui::APPLIANCE_API,
    )
}

/// Collects the `/`-separated paths of the `.ts` files under `root/relative`.
fn collect(root: &Path, relative: &Path, names: &mut Vec<String>) -> Result<(), String> {
    for entry in fs::read_dir(root.join(relative)).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = relative.join(entry.file_name());
        let text = path
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 export {}", path.display()))?;
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            collect(root, &path, names)?;
        } else if text.ends_with(".ts") {
            names.push(text.replace(std::path::MAIN_SEPARATOR, "/"));
        } else {
            return Err(format!("unexpected export {text}"));
        }
    }
    Ok(())
}

fn run(out: &Path) -> Result<(), String> {
    if out.file_name().and_then(|name| name.to_str()) != Some("generated") {
        return Err(format!(
            "refusing to replace {}: the directory must be named `generated`",
            out.display()
        ));
    }
    let staging = tempfile::tempdir().map_err(|e| format!("temporary directory: {e}"))?;
    let cfg = Config::new()
        .with_large_int("number")
        .with_out_dir(staging.path());
    export(&cfg).map_err(|e| format!("export: {e}"))?;

    let mut names = Vec::new();
    collect(staging.path(), Path::new(""), &mut names)?;
    names.sort();

    if out.exists() {
        fs::remove_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    let mut index = format!("{HEADER}\nexport * from \"./constants\";\n");
    for name in &names {
        let body = fs::read_to_string(staging.path().join(name)).map_err(|e| e.to_string())?;
        let target = out.join(name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        fs::write(&target, format!("{HEADER}{body}")).map_err(|e| e.to_string())?;
        let module = name.trim_end_matches(".ts");
        index.push_str(&format!("export type * from \"./{module}\";\n"));
    }
    fs::write(out.join("constants.ts"), constants()).map_err(|e| e.to_string())?;
    fs::write(out.join("index.ts"), index).map_err(|e| e.to_string())?;
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let [out] = args.as_slice() else {
        eprintln!("usage: typescript <dir>/generated");
        return ExitCode::from(2);
    };
    match run(out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("typescript: {message}");
            ExitCode::FAILURE
        }
    }
}
