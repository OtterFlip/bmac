// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The engine: runs as root and answers the front end's typed calls on
//! `/run/bmac/engine.sock`.

pub mod events;
pub mod listen;
pub mod server;
pub mod setup;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use bmac_core::HostSlot;
use bmac_proto::api::{ApiError, ErrorCode, JobList, JobView, Role};
use bmac_proto::certificate::CertificateView;
use bmac_proto::engine::{
    Collection, EngineCall, EngineOutcome, EngineReply, EngineRequest, Hello,
};
use bmac_proto::settings::{SaveSettings, SettingsView};
use bmac_proto::stream::{StreamBody, StreamMessage};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use self::events::{EventHub, Next, Start, WatchError};
use crate::audit::AuditKind;
use crate::auth::Auth;
use crate::auth::sessions::SessionInfo;
use crate::certificate::Certificates;
use crate::identity::Identities;
use crate::jobs::{Caller, JobRecord, Jobs};
use crate::notify::Notifier;
use crate::operations::Registry;
use crate::runner::{RelayedAnswer, Runner};
use crate::settings::{self, Settings};
use crate::status::StatusSource;
use crate::store::PAGE_EVENTS;

/// The connected process, from `SO_PEERCRED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub uid: u32,
    pub pid: Option<i32>,
}

/// Where a handler sends the replies to one request.
#[derive(Debug, Clone)]
pub struct Replies {
    id: u64,
    tx: mpsc::Sender<EngineReply>,
}

impl Replies {
    pub fn new(id: u64, tx: mpsc::Sender<EngineReply>) -> Replies {
        Replies { id, tx }
    }

    /// Sends one outcome; `false` once the connection is gone.
    pub async fn send(&self, outcome: EngineOutcome) -> bool {
        self.tx
            .send(EngineReply {
                id: self.id,
                outcome,
            })
            .await
            .is_ok()
    }

    pub async fn ok(&self, body: Value) -> bool {
        self.send(EngineOutcome::Ok { body }).await
    }

    pub async fn err(&self, error: ApiError) -> bool {
        self.send(EngineOutcome::Err { error }).await
    }

    pub async fn event(&self, message: StreamMessage) -> bool {
        self.send(EngineOutcome::Event { message }).await
    }
}

/// Answers engine calls. A unary call sends one reply; a stream sends events
/// until the front end disconnects, when the server stops the task.
pub trait Handler: Send + Sync + 'static {
    fn handle(
        &self,
        peer: Peer,
        request: EngineRequest,
        replies: Replies,
    ) -> impl Future<Output = ()> + Send;
}

/// The engine's dispatcher. Every call except `hello` and `login` needs a
/// session token, checked here before any call-specific code runs.
#[derive(Debug, Clone)]
pub struct Engine {
    version: String,
    listen: watch::Receiver<Vec<SocketAddr>>,
    auth: Arc<Auth>,
    hub: Arc<EventHub>,
    registry: Arc<Registry>,
    jobs: Option<Arc<Jobs>>,
    runner: Option<Arc<Runner>>,
    status: Option<Arc<StatusSource>>,
    settings: Option<SettingsSource>,
    certificates: Option<Certificates>,
    notifier: Option<Arc<Notifier>>,
    mesh: Option<Arc<crate::mesh::Mesh>>,
    setup: Option<crate::native::setup::Paths>,
    joins: Option<Arc<crate::joins::Joins>>,
}

/// Where the Settings page's records live.
#[derive(Clone)]
struct SettingsSource {
    settings: Settings,
    identities: Identities,
}

impl std::fmt::Debug for SettingsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SettingsSource")
    }
}

impl Engine {
    /// An engine that offers no listen addresses.
    pub fn new(version: impl Into<String>, auth: Arc<Auth>) -> Engine {
        Engine::with_listen(version, auth, watch::channel(Vec::new()).1)
    }

    /// An engine whose `hello` reply names the current addresses in `listen`.
    pub fn with_listen(
        version: impl Into<String>,
        auth: Arc<Auth>,
        listen: watch::Receiver<Vec<SocketAddr>>,
    ) -> Engine {
        Engine {
            version: version.into(),
            listen,
            auth,
            hub: EventHub::new(),
            registry: Arc::new(
                Registry::builtin().expect("the embedded operation registry is checked by tests"),
            ),
            jobs: None,
            runner: None,
            status: None,
            settings: None,
            certificates: None,
            notifier: None,
            mesh: None,
            setup: None,
            joins: None,
        }
    }

    /// Turns on setup mode while `paths` say this host is being set up.
    /// Issues, lists, and revokes join bundles with `joins`.
    pub fn with_joins(mut self, joins: Arc<crate::joins::Joins>) -> Engine {
        self.joins = Some(joins);
        self
    }

    async fn join_call(
        &self,
        session: &SessionInfo,
        client: std::net::IpAddr,
        call: EngineCall,
        replies: &Replies,
    ) {
        let Some(joins) = self.joins.clone() else {
            replies
                .err(ApiError::new(
                    ErrorCode::Unavailable,
                    "join bundles are not available on this host",
                ))
                .await;
            return;
        };
        let admin = session.role == Role::Admin;
        if !matches!(call, EngineCall::Joins {}) && !admin {
            replies
                .err(ApiError::new(
                    ErrorCode::Forbidden,
                    "adding hosts needs the admin role",
                ))
                .await;
            return;
        }
        let (outcome, message) = match call {
            EngineCall::IssueJoin { request } => {
                let host = request.host;
                match joins.issue(request, &session.user).await {
                    Ok(issued) => (
                        Ok(body(issued)),
                        Some(format!("issued a join bundle for {host}")),
                    ),
                    Err(e) => (Err(e), None),
                }
            }
            EngineCall::RevokeJoin { host } => match joins.revoke(host).await {
                Ok(()) => (
                    Ok(serde_json::json!({})),
                    Some(format!("revoked the join bundle for {host}")),
                ),
                Err(e) => (Err(e), None),
            },
            _ => {
                let listed = tokio::task::spawn_blocking(move || joins.list(admin))
                    .await
                    .unwrap_or_else(|e| Err(ApiError::new(ErrorCode::Internal, e.to_string())));
                (listed.map(body), None)
            }
        };
        if let Some(message) = message {
            self.auth
                .audit()
                .record(
                    self.auth.now(),
                    AuditKind::JoinChanged,
                    Some(&session.user),
                    Some(client),
                    message,
                )
                .await;
        }
        match outcome {
            Ok(value) => {
                replies.ok(value).await;
            }
            Err(error) => {
                replies.err(error).await;
            }
        }
    }

    pub fn with_setup(mut self, paths: crate::native::setup::Paths) -> Engine {
        self.setup = Some(paths);
        self
    }

    fn setup_active(&self) -> bool {
        setup::is_active(self.setup.as_ref())
    }

    /// Refuses what setup mode doesn't offer.
    async fn setup_gate(&self, call: &EngineCall) -> Result<(), ApiError> {
        match setup::gate(self.setup_active(), call) {
            setup::Gate::Allow => Ok(()),
            setup::Gate::Deny(error) => Err(error),
            setup::Gate::SetupJob(job) => {
                let jobs = self.jobs.clone().ok_or_else(no_jobs)?;
                tokio::task::spawn_blocking(move || jobs.get(job))
                    .await
                    .unwrap_or_else(|e| Err(ApiError::new(ErrorCode::Internal, e.to_string())))
                    .and_then(|record| setup::check_job(&record))
            }
        }
    }

    /// Shows the peers' copies of the records in Log replication.
    pub fn with_mesh(mut self, mesh: Arc<crate::mesh::Mesh>) -> Engine {
        self.mesh = Some(mesh);
        self
    }

    /// The Logs page's search, for anyone signed in.
    async fn search_logs(&self, query: bmac_proto::logs::LogQuery, replies: &Replies) {
        use crate::records::search::{SearchError, search};
        match search(self.auth.store(), query).await {
            Ok(page) => replies.ok(body(page)).await,
            Err(SearchError::Cursor) => {
                replies
                    .err(ApiError::new(
                        ErrorCode::Invalid,
                        "the paging cursor is malformed",
                    ))
                    .await
            }
            Err(e) => {
                replies
                    .err(ApiError::new(ErrorCode::Internal, e.to_string()))
                    .await
            }
        };
    }

    /// Settings, then Log replication.
    async fn log_replication(&self, replies: &Replies) {
        use crate::records::{self, retention};
        let store = self.auth.store();
        let origins = match records::origin_holdings(store).await {
            Ok(origins) => origins,
            Err(e) => {
                replies
                    .err(ApiError::new(ErrorCode::Internal, e.to_string()))
                    .await;
                return;
            }
        };
        let (used_bytes, _, audit_evicted_ms) = retention::usage(store).await.unwrap_or_default();
        let policy = retention::Policy::default();
        let space = retention::space(store.segments().parent().unwrap_or(store.segments()));
        let peers = match &self.mesh {
            Some(mesh) => mesh.replication().await,
            None => Vec::new(),
        };
        replies
            .ok(body(bmac_proto::logs::ReplicationView {
                host: self.mesh.as_ref().map(|m| m.host()),
                origins,
                peers,
                retention: bmac_proto::logs::RetentionView {
                    keep_days: policy.keep_days,
                    budget_bytes: policy.budget(used_bytes, space),
                    used_bytes,
                    audit_evicted_ms,
                },
            }))
            .await;
    }

    /// Answers the alert and notification calls.
    pub fn with_notifier(mut self, notifier: Arc<Notifier>) -> Engine {
        self.notifier = Some(notifier);
        self
    }

    /// Runs an alert or notification call: reads for anyone, acknowledging
    /// and silences for operators, the rest for administrators; changes are
    /// audited.
    async fn notify_call(
        &self,
        session: &SessionInfo,
        client: std::net::IpAddr,
        call: EngineCall,
        replies: &Replies,
    ) {
        let Some(notifier) = self.notifier.clone() else {
            replies
                .err(ApiError::new(
                    ErrorCode::Unavailable,
                    "alerts are not available on this host",
                ))
                .await;
            return;
        };
        let operator = matches!(session.role, Role::Operator | Role::Admin);
        let admin = session.role == Role::Admin;
        let needs = match &call {
            EngineCall::Alerts {} | EngineCall::List { .. } => None,
            EngineCall::AcknowledgeAlert { .. }
            | EngineCall::SaveSilence { .. }
            | EngineCall::DeleteSilence { .. } => (!operator).then_some("the operator role"),
            _ => (!admin).then_some("the admin role"),
        };
        if let Some(role) = needs {
            replies
                .err(ApiError::new(
                    ErrorCode::Forbidden,
                    format!("this needs {role}"),
                ))
                .await;
            return;
        }
        let user = session.user.clone();
        let blocking = |f: NotifyWork| {
            let notifier = notifier.clone();
            async move {
                tokio::task::spawn_blocking(move || f(&notifier))
                    .await
                    .unwrap_or_else(|e| Err(ApiError::new(ErrorCode::Internal, e.to_string())))
            }
        };
        let outcome = match call {
            EngineCall::Alerts {} | EngineCall::List { .. } => {
                blocking(Box::new(move |n| {
                    Ok((body(n.view(operator, admin)?), None))
                }))
                .await
            }
            EngineCall::AcknowledgeAlert { id } => {
                blocking(Box::new(move |n| {
                    let alert = n.acknowledge(&id, &user)?;
                    let message = format!(
                        "acknowledged alert {} ({} on {})",
                        alert.id, alert.condition, alert.resource
                    );
                    Ok((body(alert), Some((AuditKind::AlertAcknowledged, message))))
                }))
                .await
            }
            EngineCall::SaveSilence { request } => {
                blocking(Box::new(move |n| {
                    let silence = n.save_silence(request, &user)?;
                    let message = format!(
                        "silence {} until {} for condition {}, resource {}, host {}: {}",
                        silence.id,
                        silence.until_ms,
                        silence.condition.as_deref().unwrap_or("any"),
                        silence.resource.as_deref().unwrap_or("any"),
                        silence
                            .host
                            .map(|h| h.to_string())
                            .unwrap_or_else(|| "any".into()),
                        silence.reason
                    );
                    Ok((body(silence), Some((AuditKind::SilenceChanged, message))))
                }))
                .await
            }
            EngineCall::DeleteSilence { id } => {
                blocking(Box::new(move |n| {
                    let silence = n.delete_silence(&id)?;
                    let message = format!("silence {} removed", silence.id);
                    Ok((body(silence), Some((AuditKind::SilenceChanged, message))))
                }))
                .await
            }
            EngineCall::SaveNotifySettings { request } => {
                blocking(Box::new(move |n| {
                    let revision = n.save_settings(request)?;
                    let message = format!("notification settings saved at revision {revision}");
                    Ok((
                        body(n.view(operator, admin)?),
                        Some((AuditKind::NotificationsChanged, message)),
                    ))
                }))
                .await
            }
            EngineCall::SetPostmarkToken { request } => {
                blocking(Box::new(move |n| {
                    n.set_token(&request.token)?;
                    let message = if request.token.trim().is_empty() {
                        "Postmark server token removed"
                    } else {
                        "Postmark server token replaced"
                    };
                    Ok((
                        body(n.view(operator, admin)?),
                        Some((AuditKind::NotificationsChanged, message.to_string())),
                    ))
                }))
                .await
            }
            EngineCall::SendTestEmail { request } => {
                match notifier.test_email(request.to, &user).await {
                    Ok(results) => {
                        let sent: Vec<&str> = results
                            .iter()
                            .filter(|r| r.message_id.is_some())
                            .map(|r| r.recipient.as_str())
                            .collect();
                        let message = format!(
                            "test email sent to {}",
                            if sent.is_empty() {
                                "nobody".to_string()
                            } else {
                                sent.join(", ")
                            }
                        );
                        Ok((
                            body(results),
                            Some((AuditKind::NotificationsChanged, message)),
                        ))
                    }
                    Err(error) => Err(error),
                }
            }
            EngineCall::EmailTemplates {} => {
                notifier.templates().await.map(|view| (body(view), None))
            }
            EngineCall::CreateEmailTemplates {} => notifier.create_templates().await.map(|view| {
                (
                    body(view),
                    Some((
                        AuditKind::NotificationsChanged,
                        "created the missing email templates".to_string(),
                    )),
                )
            }),
            other => Err(ApiError::new(
                ErrorCode::Internal,
                format!("{} is not a notification call", other.name()),
            )),
        };
        match outcome {
            Ok((value, audit)) => {
                if let Some((kind, message)) = audit {
                    self.auth
                        .audit()
                        .record(
                            self.auth.now(),
                            kind,
                            Some(&session.user),
                            Some(client),
                            message,
                        )
                        .await;
                }
                replies.ok(value).await;
            }
            Err(error) => {
                replies.err(error).await;
            }
        }
    }

    /// Answers the Settings page's calls from these records.
    pub fn with_settings(mut self, settings: Settings, identities: Identities) -> Engine {
        self.settings = Some(SettingsSource {
            settings,
            identities,
        });
        self
    }

    async fn settings_read(&self, session: &SessionInfo, replies: &Replies) {
        let Some(source) = self.settings.clone() else {
            replies.err(no_settings()).await;
            return;
        };
        let can_edit = session.role == Role::Admin;
        let outcome = tokio::task::spawn_blocking(move || -> Result<SettingsView, ApiError> {
            Ok(SettingsView {
                cluster: source.settings.cluster()?.map(|c| c.view()),
                hosts: source.settings.hosts()?.iter().map(|f| f.view()).collect(),
                cluster_keys: settings::CLUSTER_KEYS.iter().map(|k| k.view()).collect(),
                host_keys: settings::HOST_KEYS.iter().map(|k| k.view()).collect(),
                identities: source.identities.all()?,
                can_edit,
            })
        })
        .await
        .unwrap_or_else(|e| Err(ApiError::new(ErrorCode::Internal, e.to_string())));
        match outcome {
            Ok(view) => replies.ok(body(view)).await,
            Err(error) => replies.err(error).await,
        };
    }

    /// Saves the cluster settings, or `host`'s facts, for an administrator,
    /// and audits the change.
    async fn settings_save(
        &self,
        session: &SessionInfo,
        client: std::net::IpAddr,
        host: Option<HostSlot>,
        request: SaveSettings,
        replies: &Replies,
    ) {
        let Some(source) = self.settings.clone() else {
            replies.err(no_settings()).await;
            return;
        };
        if session.role != Role::Admin {
            replies
                .err(ApiError::new(
                    ErrorCode::Forbidden,
                    "changing settings needs the admin role",
                ))
                .await;
            return;
        }
        let (user, now) = (session.user.clone(), self.auth.now());
        let SaveSettings {
            values,
            expected_revision,
        } = request;
        let outcome = tokio::task::spawn_blocking(move || -> Result<(Value, String), ApiError> {
            Ok(match host {
                None => {
                    let saved =
                        source
                            .settings
                            .save_cluster(values, expected_revision, &user, now)?;
                    let message = format!("cluster settings saved at revision {}", saved.revision);
                    (body(saved.view()), message)
                }
                Some(host) => {
                    let saved =
                        source
                            .settings
                            .save_host(host, values, expected_revision, &user, now)?;
                    let message = format!("{host}'s facts saved at revision {}", saved.revision);
                    (body(saved.view()), message)
                }
            })
        })
        .await
        .unwrap_or_else(|e| Err(ApiError::new(ErrorCode::Internal, e.to_string())));
        match outcome {
            Ok((saved, message)) => {
                self.auth
                    .audit()
                    .record(
                        now,
                        AuditKind::SettingsChanged,
                        Some(&session.user),
                        Some(client),
                        message,
                    )
                    .await;
                replies.ok(saved).await;
            }
            Err(error) => {
                replies.err(error).await;
            }
        }
    }

    /// Answers the certificate rotation calls for this host's certificate.
    pub fn with_certificates(mut self, certificates: Certificates) -> Engine {
        self.certificates = Some(certificates);
        self
    }

    /// Runs a certificate call: reads for anyone, changes for an
    /// administrator, audited when they changed something.
    async fn certificate_call(
        &self,
        session: &SessionInfo,
        client: std::net::IpAddr,
        change: Option<CertificateChange>,
        replies: &Replies,
    ) {
        let Some(certificates) = self.certificates.clone() else {
            replies
                .err(ApiError::new(
                    ErrorCode::Unavailable,
                    "certificate rotation is not available on this host",
                ))
                .await;
            return;
        };
        let admin = session.role == Role::Admin;
        if change.is_some() && !admin {
            replies
                .err(ApiError::new(
                    ErrorCode::Forbidden,
                    "rotating the certificate needs the admin role",
                ))
                .await;
            return;
        }
        let now = self.auth.now();
        let outcome = tokio::task::spawn_blocking(
            move || -> Result<(CertificateView, Option<String>), ApiError> {
                let message = match change {
                    None => None,
                    Some(CertificateChange::Rotate) => {
                        let made = certificates.rotate(now)?;
                        Some(format!(
                            "new certificate made, not in use yet: {}",
                            made.fingerprint
                        ))
                    }
                    Some(CertificateChange::Discard) => certificates
                        .discard()?
                        .then(|| "new certificate discarded".to_string()),
                    Some(CertificateChange::Switch(fingerprint)) => {
                        let switched = certificates.switch(&fingerprint)?;
                        Some(format!(
                            "switched to the new certificate {}",
                            switched.fingerprint
                        ))
                    }
                };
                Ok((certificates.view(admin), message))
            },
        )
        .await
        .unwrap_or_else(|e| Err(ApiError::new(ErrorCode::Internal, e.to_string())));
        match outcome {
            Ok((view, message)) => {
                if let Some(message) = message {
                    self.auth
                        .audit()
                        .record(
                            now,
                            AuditKind::CertificateChanged,
                            Some(&session.user),
                            Some(client),
                            message,
                        )
                        .await;
                }
                replies.ok(body(view)).await;
            }
            Err(error) => {
                replies.err(error).await;
            }
        }
    }

    /// Answers the cluster status calls from `status`.
    pub fn with_status(mut self, status: Arc<StatusSource>) -> Engine {
        self.status = Some(status);
        self
    }

    async fn status_read<T: serde::Serialize + Send + 'static>(
        &self,
        replies: &Replies,
        read: impl FnOnce(&StatusSource) -> T + Send + 'static,
    ) {
        let Some(status) = self.status.clone() else {
            replies
                .err(ApiError::new(
                    ErrorCode::Unavailable,
                    "cluster status is not available on this host",
                ))
                .await;
            return;
        };
        match tokio::task::spawn_blocking(move || read(&status)).await {
            Ok(value) => replies.ok(body(value)).await,
            Err(e) => {
                replies
                    .err(ApiError::new(ErrorCode::Internal, e.to_string()))
                    .await
            }
        };
    }

    pub fn auth(&self) -> &Arc<Auth> {
        &self.auth
    }

    pub fn hub(&self) -> &Arc<EventHub> {
        &self.hub
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Lets this engine admit and manage jobs, with the jobs' registry.
    pub fn with_jobs(mut self, jobs: Arc<Jobs>) -> Engine {
        self.registry = jobs.registry().clone();
        self.jobs = Some(jobs);
        self
    }

    pub fn jobs(&self) -> Option<&Arc<Jobs>> {
        self.jobs.as_ref()
    }

    /// Runs the jobs targeted at this host, and takes answers for them.
    pub fn with_runner(mut self, runner: Arc<Runner>) -> Engine {
        self = self.with_jobs(runner.jobs().clone());
        self.runner = Some(runner);
        self
    }

    fn kick_runner(&self) {
        if let Some(runner) = &self.runner {
            runner.kick();
        }
    }

    /// Runs a blocking job-record call, publishes the changed job, and
    /// audits it.
    async fn job_change(
        &self,
        session: &SessionInfo,
        client: std::net::IpAddr,
        kind: AuditKind,
        replies: &Replies,
        change: impl FnOnce(&Jobs, Caller) -> Result<(JobRecord, bool), ApiError> + Send + 'static,
    ) {
        let Some(jobs) = self.jobs.clone() else {
            replies.err(no_jobs()).await;
            return;
        };
        let (user, role) = (session.user.clone(), session.role);
        let outcome =
            tokio::task::spawn_blocking(move || change(&jobs, Caller { user: &user, role }))
                .await
                .unwrap_or_else(|e| Err(ApiError::new(ErrorCode::Internal, e.to_string())));
        match outcome {
            Ok((record, changed)) => {
                let summary = record.summary();
                if changed {
                    self.hub.publish(StreamBody::Job {
                        job: summary.clone(),
                    });
                    let message = format!("{} {} ({})", kind.as_str(), record.id, record.operation);
                    self.auth
                        .audit()
                        .record(
                            self.auth.now(),
                            kind,
                            Some(&session.user),
                            Some(client),
                            message,
                        )
                        .await;
                }
                replies.ok(body(summary)).await;
            }
            Err(error) => {
                replies.err(error).await;
            }
        }
    }

    /// Sweeps the job records every `every`: running jobs whose lease
    /// expired become interrupted, and admitted jobs nobody claimed fail.
    pub fn spawn_job_upkeep(
        &self,
        every: std::time::Duration,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let jobs = self.jobs.clone()?;
        let hub = self.hub.clone();
        Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let jobs = jobs.clone();
                match tokio::task::spawn_blocking(move || jobs.sweep()).await {
                    Ok(Ok(changed)) => {
                        for record in changed {
                            hub.publish(StreamBody::Job {
                                job: record.summary(),
                            });
                        }
                    }
                    Ok(Err(error)) => crate::log::warning(&format!("job sweep: {}", error.message)),
                    Err(error) => crate::log::error(&format!("job sweep: {error}")),
                }
            }
        }))
    }

    /// Runs a blocking read of the job records.
    async fn job_read<T: serde::Serialize + Send + 'static>(
        &self,
        replies: &Replies,
        read: impl FnOnce(&Jobs) -> Result<T, ApiError> + Send + 'static,
    ) {
        let Some(jobs) = self.jobs.clone() else {
            replies.err(no_jobs()).await;
            return;
        };
        let outcome = tokio::task::spawn_blocking(move || read(&jobs))
            .await
            .unwrap_or_else(|e| Err(ApiError::new(ErrorCode::Internal, e.to_string())));
        match outcome {
            Ok(value) => replies.ok(body(value)).await,
            Err(error) => replies.err(error).await,
        };
    }

    /// Runs a blocking read that may not need the job records.
    async fn job_read_with<T: serde::Serialize + Send + 'static>(
        &self,
        replies: &Replies,
        read: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
    ) {
        let outcome = tokio::task::spawn_blocking(read)
            .await
            .unwrap_or_else(|e| Err(ApiError::new(ErrorCode::Internal, e.to_string())));
        match outcome {
            Ok(value) => replies.ok(body(value)).await,
            Err(error) => replies.err(error).await,
        };
    }

    /// Streams live updates until the tab leaves or the session ends.
    async fn subscribe(
        &self,
        session: &SessionInfo,
        last_event_id: Option<String>,
        replies: &Replies,
    ) {
        let Some(mut ended) = self.auth.watch_end(session) else {
            replies
                .err(ApiError::new(
                    ErrorCode::Unauthenticated,
                    "the session has ended; log in again",
                ))
                .await;
            return;
        };
        let mut subscription = self
            .hub
            .subscribe(&session.token_hash, last_event_id.as_deref());
        if !replies.event(subscription.hello.clone()).await {
            return;
        }
        let mut last_id = subscription.hello.id.clone();
        let first: Vec<StreamMessage> = match &subscription.start {
            Start::Resnapshot(reason) => vec![StreamMessage {
                id: last_id.clone(),
                body: StreamBody::Resnapshot {
                    reason: reason.clone(),
                },
            }],
            Start::Replay(_) => subscription
                .replay()
                .iter()
                .map(|m| (**m).clone())
                .collect(),
        };
        for message in first {
            last_id = message.id.clone();
            if !replies.event(message).await {
                return;
            }
        }
        loop {
            let next = tokio::select! {
                reason = ended.wait_for(Option::is_some) => {
                    let message = reason.ok().and_then(|r| r.as_ref().map(|r| r.message()));
                    Err(message.unwrap_or_else(|| "the session has ended; log in again".into()))
                }
                next = subscription.next() => match next {
                    Next::Message(message) => Ok((*message).clone()),
                    Next::Resnapshot(message) => Ok(message),
                    Next::Closed => return,
                },
            };
            let message = match next {
                Ok(message) => message,
                Err(message) => {
                    let body = StreamBody::SessionEnded { message };
                    replies.event(StreamMessage { id: last_id, body }).await;
                    return;
                }
            };
            last_id = message.id.clone();
            if !replies.event(message).await {
                return;
            }
        }
    }
}

fn no_settings() -> ApiError {
    ApiError::new(
        ErrorCode::Unavailable,
        "settings are not available on this host",
    )
}

/// A reply and, for a change, what to audit.
type NotifyReply = Result<(Value, Option<(AuditKind, String)>), ApiError>;
type NotifyWork = Box<dyn FnOnce(&Notifier) -> NotifyReply + Send>;

enum CertificateChange {
    Rotate,
    Discard,
    Switch(String),
}

fn no_jobs() -> ApiError {
    ApiError::new(
        ErrorCode::Unavailable,
        "jobs are not available on this host",
    )
}

fn body(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_default()
}

impl Handler for Engine {
    async fn handle(&self, _peer: Peer, request: EngineRequest, replies: Replies) {
        let client = request.client;
        let session = match &request.call {
            EngineCall::Hello {} => {
                let hello = Hello {
                    version: self.version.clone(),
                    listen: self.listen.borrow().clone(),
                };
                replies.ok(body(hello)).await;
                return;
            }
            EngineCall::Login { request } => {
                match self.auth.login(request.clone(), client).await {
                    Ok(response) => replies.ok(body(response)).await,
                    Err(error) => replies.err(error).await,
                };
                return;
            }
            call => {
                let activity = !call.is_stream();
                match self
                    .auth
                    .authenticate(request.token.as_deref(), activity)
                    .await
                {
                    Ok(session) => session,
                    Err(error) => {
                        replies.err(error).await;
                        return;
                    }
                }
            }
        };
        if let Err(error) = self.setup_gate(&request.call).await {
            replies.err(error).await;
            return;
        }
        match request.call {
            EngineCall::Setup {} => {
                let paths = self.setup.clone();
                let jobs = self.jobs.clone();
                self.job_read_with(&replies, move || {
                    setup::view(paths.as_ref(), jobs.as_deref())
                })
                .await;
            }
            EngineCall::Logout { request } => {
                match self.auth.logout(&session, client, request.everywhere).await {
                    Ok(()) => replies.ok(serde_json::json!({})).await,
                    Err(error) => replies.err(error).await,
                };
            }
            EngineCall::Operations {} => {
                let list = self.registry.list_for(session.role);
                replies
                    .ok(body(setup::operations(self.setup_active(), list)))
                    .await;
            }
            EngineCall::Settings {} => self.settings_read(&session, &replies).await,
            EngineCall::SaveClusterSettings { request } => {
                self.settings_save(&session, client, None, request, &replies)
                    .await
            }
            EngineCall::SaveHostFacts { host, request } => {
                self.settings_save(&session, client, Some(host), request, &replies)
                    .await
            }
            EngineCall::Certificate {} => {
                self.certificate_call(&session, client, None, &replies)
                    .await
            }
            EngineCall::RotateCertificate {} => {
                self.certificate_call(&session, client, Some(CertificateChange::Rotate), &replies)
                    .await
            }
            EngineCall::SwitchCertificate { request } => {
                let change = CertificateChange::Switch(request.fingerprint);
                self.certificate_call(&session, client, Some(change), &replies)
                    .await
            }
            EngineCall::DiscardCertificate {} => {
                self.certificate_call(&session, client, Some(CertificateChange::Discard), &replies)
                    .await
            }
            EngineCall::Cluster {} => self.status_read(&replies, StatusSource::cluster).await,
            EngineCall::List {
                collection: Collection::Hosts,
            } => self.status_read(&replies, StatusSource::hosts).await,
            EngineCall::List {
                collection: Collection::Vms,
            } => self.status_read(&replies, StatusSource::vms).await,
            EngineCall::List {
                collection: Collection::Storage,
            } => self.status_read(&replies, StatusSource::storage).await,
            EngineCall::List {
                collection: Collection::Backups,
            } => {
                replies
                    .err(ApiError::new(
                        ErrorCode::Unavailable,
                        "Backups arrive with the backup appliance; not yet",
                    ))
                    .await;
            }
            call @ (EngineCall::List {
                collection: Collection::Alerts,
            }
            | EngineCall::Alerts {}
            | EngineCall::AcknowledgeAlert { .. }
            | EngineCall::SaveSilence { .. }
            | EngineCall::DeleteSilence { .. }
            | EngineCall::SaveNotifySettings { .. }
            | EngineCall::SetPostmarkToken { .. }
            | EngineCall::SendTestEmail { .. }
            | EngineCall::EmailTemplates {}
            | EngineCall::CreateEmailTemplates {}) => {
                self.notify_call(&session, client, call, &replies).await;
            }
            call @ (EngineCall::Joins {}
            | EngineCall::IssueJoin { .. }
            | EngineCall::RevokeJoin { .. }) => {
                self.join_call(&session, client, call, &replies).await;
            }
            EngineCall::SearchLogs { query } => self.search_logs(query, &replies).await,
            EngineCall::LogReplication {} => self.log_replication(&replies).await,
            EngineCall::LaunchJob { request } => {
                self.job_change(
                    &session,
                    client,
                    AuditKind::JobLaunched,
                    &replies,
                    move |jobs, caller| jobs.admit(caller, &request).map(|a| (a.job, !a.existing)),
                )
                .await;
                self.kick_runner();
            }
            EngineCall::Jobs {} => {
                let active = self.setup_active();
                self.job_read(&replies, move |jobs| {
                    Ok(JobList {
                        jobs: setup::jobs(active, jobs.list()?)
                            .iter()
                            .map(JobRecord::summary)
                            .collect(),
                    })
                })
                .await;
            }
            EngineCall::Job { job, from_seq } => {
                let Some(jobs) = self.jobs.clone() else {
                    replies.err(no_jobs()).await;
                    return;
                };
                let record = tokio::task::spawn_blocking(move || jobs.get(job))
                    .await
                    .unwrap_or_else(|e| Err(ApiError::new(ErrorCode::Internal, e.to_string())));
                let record = match record {
                    Ok(record) => record,
                    Err(error) => {
                        replies.err(error).await;
                        return;
                    }
                };
                match self
                    .auth
                    .store()
                    .job_events(job, from_seq, PAGE_EVENTS)
                    .await
                {
                    Ok(page) => {
                        replies
                            .ok(body(JobView {
                                job: record.summary(),
                                parameters: record.parameters,
                                locks: record.locks,
                                events: page.events,
                                more: page.more,
                            }))
                            .await;
                    }
                    Err(error) => {
                        replies
                            .err(ApiError::new(ErrorCode::Internal, error.to_string()))
                            .await;
                    }
                }
            }
            EngineCall::Cancel { job, request } => {
                self.job_change(
                    &session,
                    client,
                    AuditKind::JobCancelRequested,
                    &replies,
                    move |jobs, caller| jobs.cancel(caller, job, request.force).map(|r| (r, true)),
                )
                .await;
                if let Some(runner) = &self.runner {
                    runner.nudge(job);
                }
            }
            EngineCall::Resume { job } => {
                self.job_change(
                    &session,
                    client,
                    AuditKind::JobResumed,
                    &replies,
                    move |jobs, caller| jobs.resume(caller, job).map(|r| (r, true)),
                )
                .await;
                self.kick_runner();
            }
            EngineCall::Reconcile { job } => {
                self.job_change(
                    &session,
                    client,
                    AuditKind::JobReconciled,
                    &replies,
                    move |jobs, caller| jobs.reconcile(caller, job).map(|r| (r, true)),
                )
                .await;
            }
            EngineCall::Answer {
                job,
                request_id,
                answer,
            } => {
                let Some(runner) = self.runner.clone() else {
                    replies.err(no_jobs()).await;
                    return;
                };
                let relayed = RelayedAnswer {
                    user: session.user.clone(),
                    role: session.role,
                    job,
                    request_id,
                    answer,
                };
                match runner.answer(relayed).await {
                    Ok(record) => {
                        let message = format!(
                            "{} {} ({}) on {}",
                            AuditKind::JobAnswered.as_str(),
                            record.id,
                            record.operation,
                            record.target
                        );
                        self.auth
                            .audit()
                            .record(
                                self.auth.now(),
                                AuditKind::JobAnswered,
                                Some(&session.user),
                                Some(client),
                                message,
                            )
                            .await;
                        replies.ok(body(record.summary())).await;
                    }
                    Err(error) => {
                        replies.err(error).await;
                    }
                }
            }
            EngineCall::Subscribe { last_event_id } => {
                self.subscribe(&session, last_event_id, &replies).await;
            }
            EngineCall::Watch { request } => {
                match self
                    .hub
                    .watch(&session.token_hash, &request.stream, request.jobs)
                {
                    Ok(()) => replies.ok(serde_json::json!({})).await,
                    Err(e @ WatchError::UnknownStream) => {
                        replies
                            .err(ApiError::new(ErrorCode::NotFound, e.to_string()))
                            .await
                    }
                    Err(e @ WatchError::TooMany) => {
                        replies
                            .err(ApiError::new(ErrorCode::Invalid, e.to_string()))
                            .await
                    }
                };
            }
            call => {
                replies
                    .err(ApiError::new(
                        ErrorCode::Unavailable,
                        format!("{} is not available yet", call.name()),
                    ))
                    .await;
            }
        }
    }
}
