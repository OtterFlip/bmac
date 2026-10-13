// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Setup mode (`DEPLOYMENT.md`, "Host setup job"): from the first boot
//! until the setup job finishes, the engine answers only what the setup
//! page needs, and the setup job is offered nowhere else.

use bmac_core::{JobId, OperationId};
use bmac_proto::api::{ApiError, ErrorCode, JoinProgress, JoinState, SetupView};
use bmac_proto::bootstrap::BootstrapOrigin;
use bmac_proto::engine::EngineCall;
use bmac_proto::operations::OperationList;

use crate::jobs::{JobRecord, Jobs};
use crate::native::setup::{self, Paths};

/// What the engine does with a call, given whether setup mode is on.
#[derive(Debug, PartialEq)]
pub enum Gate {
    Allow,
    Deny(ApiError),
    /// Allowed only if the job is a setup job.
    SetupJob(JobId),
}

fn operation() -> OperationId {
    setup::OPERATION
        .parse()
        .expect("the setup operation id is valid")
}

fn still_setting_up() -> ApiError {
    ApiError::new(
        ErrorCode::Unavailable,
        "this host is still being set up; only its setup job is available until it finishes",
    )
}

/// An engine without setup paths is never in setup mode.
pub fn is_active(paths: Option<&Paths>) -> bool {
    paths.is_some_and(setup::in_setup_mode)
}

pub fn gate(active: bool, call: &EngineCall) -> Gate {
    let launches_setup = matches!(call, EngineCall::LaunchJob { request } if request.operation.as_str() == setup::OPERATION);
    if !active {
        return if launches_setup {
            Gate::Deny(ApiError::new(
                ErrorCode::Conflict,
                "this host's setup is finished",
            ))
        } else {
            Gate::Allow
        };
    }
    match call {
        EngineCall::Hello {}
        | EngineCall::Login { .. }
        | EngineCall::Logout { .. }
        | EngineCall::Setup {}
        | EngineCall::Operations {}
        | EngineCall::Jobs {}
        | EngineCall::Subscribe { .. }
        | EngineCall::Watch { .. }
        | EngineCall::Certificate {} => Gate::Allow,
        EngineCall::LaunchJob { .. } if launches_setup => Gate::Allow,
        EngineCall::Job { job, .. }
        | EngineCall::Answer { job, .. }
        | EngineCall::Cancel { job, .. }
        | EngineCall::Resume { job }
        | EngineCall::Reconcile { job } => Gate::SetupJob(*job),
        _ => Gate::Deny(still_setting_up()),
    }
}

/// Whether `record` passes a [`Gate::SetupJob`].
pub fn check_job(record: &JobRecord) -> Result<(), ApiError> {
    if record.operation.as_str() == setup::OPERATION {
        Ok(())
    } else {
        Err(still_setting_up())
    }
}

/// The operations offered: only setup while it lasts, never setup after.
pub fn operations(active: bool, mut list: OperationList) -> OperationList {
    list.operations
        .retain(|view| (view.operation.id.as_str() == setup::OPERATION) == active);
    list
}

/// The jobs shown: only setup jobs while setup lasts.
pub fn jobs(active: bool, records: Vec<JobRecord>) -> Vec<JobRecord> {
    if !active {
        return records;
    }
    records
        .into_iter()
        .filter(|r| r.operation.as_str() == setup::OPERATION)
        .collect()
}

/// The setup page's view. The bootstrap file is read only for its names;
/// a broken one still shows as setup mode, and the job reports why.
pub fn view(paths: Option<&Paths>, jobs: Option<&Jobs>) -> Result<SetupView, ApiError> {
    let active = is_active(paths);
    let bootstrap = paths
        .filter(|_| active)
        .and_then(|p| setup::read_bootstrap(&p.bootstrap).ok());
    let job = match jobs {
        Some(jobs) => jobs
            .list()?
            .into_iter()
            .filter(|r| r.operation.as_str() == setup::OPERATION)
            .max_by_key(|r| r.created_at_ms)
            .map(|r| r.summary()),
        None => None,
    };
    let (new_cluster, cluster_name) = match bootstrap.as_ref().map(|b| &b.origin) {
        Some(BootstrapOrigin::NewCluster(seed)) => (
            true,
            seed.cluster_settings.get("PROXMOX_CLUSTER_NAME").cloned(),
        ),
        Some(BootstrapOrigin::Join(bundle)) => (false, Some(bundle.cluster_name.clone())),
        None => (false, None),
    };
    let join = match (paths, bootstrap.as_ref().map(|b| &b.origin)) {
        (Some(paths), Some(BootstrapOrigin::Join(_))) => Some(
            crate::joiner::read_state(&paths.join_state).unwrap_or(JoinProgress {
                state: JoinState::Requesting,
                host: None,
                dashboard_url: None,
                request_id: None,
                message: None,
                updated_at_ms: 0,
            }),
        ),
        _ => None,
    };
    Ok(SetupView {
        active,
        operation: operation(),
        host: bootstrap.as_ref().map(|b| b.host),
        new_cluster,
        cluster_name,
        job,
        join,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(value: serde_json::Value) -> EngineCall {
        serde_json::from_value(value).unwrap()
    }

    fn launch(operation: &str) -> EngineCall {
        call(json!({"name": "launch_job", "request": {
            "operation": operation, "parameters": {}, "idempotency_key": "k"}}))
    }

    #[test]
    fn setup_mode_offers_only_the_setup_job() {
        assert_eq!(gate(true, &launch("hosts.setup")), Gate::Allow);
        assert!(matches!(gate(true, &launch("prod.add")), Gate::Deny(_)));
        for name in [
            "settings",
            "cluster",
            "alerts",
            "log_replication",
            "rotate_certificate",
        ] {
            assert!(
                matches!(gate(true, &call(json!({"name": name}))), Gate::Deny(_)),
                "{name}"
            );
        }
        for name in ["setup", "operations", "jobs", "certificate"] {
            assert_eq!(
                gate(true, &call(json!({"name": name}))),
                Gate::Allow,
                "{name}"
            );
        }
        let job = "6f9619ff-8b86-4011-b42d-00c04fc964ff";
        let resume = call(json!({"name": "resume", "job": job}));
        assert_eq!(gate(true, &resume), Gate::SetupJob(job.parse().unwrap()));
    }

    #[test]
    fn after_setup_everything_but_setup_is_offered() {
        assert!(
            matches!(gate(false, &launch("hosts.setup")), Gate::Deny(e) if e.code == ErrorCode::Conflict)
        );
        assert_eq!(gate(false, &launch("prod.add")), Gate::Allow);
        assert_eq!(gate(false, &call(json!({"name": "settings"}))), Gate::Allow);
    }
}
